#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adapter;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use serde_json::{Value, json};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use adapter::{Session, log_error, post};

// Same key and value names as the C# adapter, so a switch keeps the chosen paths.
const REG_KEY: &str = r"Software\EMLst";
const REG_GAME_DIR: &str = "SelectedFolderPath";
const REG_CONFIG: &str = "SelectedConfigPath";
const DEFAULT_GAME_DIR: &str = r"C:\Program Files\sixteentons entertainment\Emergency4";
const DEFAULT_SYNC_FILE: &str = "input.txt";

struct Config {
    api: String,
    mod_id: Value,
    raw: Value,
}

fn load_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("Config {}: {e}", path.display()))?;
    let raw: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("Config {} ist kein gültiges JSON: {e}", path.display()))?;
    let api = raw["server_path"]
        .as_str()
        .ok_or_else(|| format!("Config {}: server_path fehlt", path.display()))?
        .to_string();
    Ok(Config { api, mod_id: raw["mod_id"].clone(), raw })
}

fn read_setting(name: &str, default: &str) -> String {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(REG_KEY)
        .and_then(|k| k.get_value::<String, _>(name))
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn write_setting(name: &str, value: &str) {
    let result = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(REG_KEY)
        .and_then(|(k, _)| k.set_value(name, &value.to_string()));
    if let Err(e) = result {
        log_error(&format!("Einstellung {name} nicht gespeichert: {e}"));
    }
}

/// The mod id as the game and the sync payload expect it: a JSON string without quotes.
fn plain(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

struct AdapterApp {
    pin: String,
    token: String,
    game_dir: String,
    config_path: String,
    config: Option<Config>,
    session: Option<Session>,
    http: ureq::Agent,
    status: Arc<Mutex<String>>,
}

impl AdapterApp {
    fn new() -> Self {
        let mut app = AdapterApp {
            pin: String::new(),
            token: String::new(),
            game_dir: read_setting(REG_GAME_DIR, DEFAULT_GAME_DIR),
            config_path: read_setting(REG_CONFIG, "config.cfg"),
            config: None,
            session: None,
            http: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(10)))
                // AUBLst explains a 4xx in the body; `adapter::reply` reads it.
                .http_status_as_error(false)
                .build()
                .into(),
            status: Arc::new(Mutex::new(String::new())),
        };
        app.reload_config();
        app
    }

    fn set_status(&self, text: impl Into<String>) {
        *self.status.lock().unwrap() = text.into();
    }

    fn fail(&self, text: String) {
        adapter::fail(&self.status, text);
    }

    fn reload_config(&mut self) {
        match load_config(&adapter::beside_exe(&self.config_path)) {
            Ok(c) => {
                self.set_status("Config geladen, bereit für einen Token.");
                self.config = Some(c);
            }
            Err(e) => {
                self.config = None;
                self.fail(e);
            }
        }
    }

    // ponytail: runs on the UI thread, the window freezes for up to the 10 s timeout
    // when the server hangs. Move it to a thread if that turns out to happen.
    fn request_token(&mut self) {
        let Some(config) = &self.config else { return };
        let mut body = config.raw["request_data"].clone();
        if !body.is_object() {
            return self.fail("Config: request_data fehlt".into());
        }
        if !self.pin.is_empty() {
            body["pin"] = Value::String(self.pin.clone());
        }
        let url = format!("{}?action=session_create", config.api);
        match post(&self.http, &url, &body) {
            Ok(res) => match res["session_token"].as_str() {
                Some(token) => {
                    self.token = token.to_string();
                    self.start();
                }
                None => self.fail(format!("Kein Token erhalten: {res}")),
            },
            Err(e) => self.fail(format!("Token anfordern fehlgeschlagen: {e}")),
        }
    }

    fn start(&mut self) {
        let Some(config) = &self.config else { return };
        let game_dir = PathBuf::from(&self.game_dir);
        let sync_name = config.raw["sync_path"].as_str().unwrap_or(DEFAULT_SYNC_FILE);
        let session = Session {
            api: config.api.clone(),
            sync_file: game_dir.join(sync_name),
            game_dir,
            token: self.token.clone(),
            mod_id: plain(&config.mod_id),
            http: self.http.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            sync: Arc::new(Mutex::new(false)),
            status: self.status.clone(),
        };

        if let Some(initial) = config.raw.get("initial_data").filter(|v| v.is_object()) {
            let mut initial = initial.clone();
            initial["session_token"] = json!(self.token);
            initial["mod_id"] = config.mod_id.clone();
            if let Err(e) = post(&self.http, &format!("{}?action=sync", config.api), &initial) {
                self.fail(format!("Startdaten senden fehlgeschlagen: {e}"));
            }
        }
        if let Err(e) = session.write_sync(&[], false) {
            self.fail(format!("Sync-Datei {}: {e}", session.sync_file.display()));
        }

        let log_session = session.clone();
        std::thread::spawn(move || adapter::run_log_monitor(log_session));
        let command_session = session.clone();
        std::thread::spawn(move || adapter::run_command_poller(command_session));
        self.set_status("Verbunden, warte auf das Spiel.");
        self.session = Some(session);
    }

    fn dispatch_url(&self) -> Option<String> {
        let config = self.config.as_ref()?;
        Some(format!("{}frontend?session_token={}", config.api.replace("backend/api.php", ""), self.token))
    }

    fn open_dispatch(&self, ctx: &egui::Context) {
        let Some(url) = self.dispatch_url() else { return };
        ctx.copy_text(url.clone());
        if let Err(e) = std::process::Command::new("explorer").arg(&url).spawn() {
            self.fail(format!("Browser öffnen fehlgeschlagen: {e}"));
        } else {
            self.set_status("Link geöffnet und in die Zwischenablage kopiert.");
        }
    }

    fn run_game(&self) {
        let Some(config) = &self.config else { return };
        let exe = Path::new(&self.game_dir).join("Em4.exe");
        let result = std::process::Command::new(&exe)
            .args(["-game", "-mod", &plain(&config.mod_id)])
            .current_dir(&self.game_dir)
            .spawn();
        if let Err(e) = result {
            self.fail(format!("{} lässt sich nicht starten: {e}", exe.display()));
        }
    }

    fn path_row(ui: &mut egui::Ui, path: &str, button: &str, enabled: bool) -> bool {
        let mut clicked = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                clicked = ui.add_enabled(enabled, egui::Button::new(button)).clicked();
                ui.add(egui::Label::new(path).truncate()).on_hover_text(path);
            });
        });
        clicked
    }
}

impl eframe::App for AdapterApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Worker threads only update the status text; repaint now and then to show it.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        let ctx = ui.ctx().clone();
        let started = self.session.is_some();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!started && self.config.is_some(), egui::Button::new("Token anfordern"))
                        .clicked()
                    {
                        self.request_token();
                    }
                    ui.add_enabled(
                        !started,
                        egui::TextEdit::singleline(&mut self.pin)
                            .hint_text("PIN (optional)")
                            .char_limit(10)
                            .desired_width(f32::INFINITY),
                    );
                });
            });
            ui.add(
                egui::TextEdit::singleline(&mut self.token.as_str())
                    .hint_text("Token")
                    .desired_width(f32::INFINITY),
            );

            if Self::path_row(ui, &self.game_dir.clone(), "Spielordner wählen", !started) {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&self.game_dir).pick_folder() {
                    self.game_dir = dir.display().to_string();
                    write_setting(REG_GAME_DIR, &self.game_dir);
                }
            }
            if Self::path_row(ui, &self.config_path.clone(), "Config wählen", !started) {
                if let Some(file) = rfd::FileDialog::new().add_filter("Config", &["cfg"]).pick_file() {
                    self.config_path = file.display().to_string();
                    write_setting(REG_CONFIG, &self.config_path);
                    self.reload_config();
                }
            }

            ui.horizontal(|ui| {
                if ui.add_enabled(started, egui::Button::new("Leitstelle öffnen")).clicked() {
                    self.open_dispatch(&ctx);
                }
                if ui.add_enabled(started, egui::Button::new("Spiel starten")).clicked() {
                    self.run_game();
                }
            });
            ui.separator();
            ui.add(egui::Label::new(self.status.lock().unwrap().as_str()).wrap());
        });
    }
}

impl Drop for AdapterApp {
    fn drop(&mut self) {
        if let Some(s) = &self.session {
            s.stop.store(true, Ordering::Relaxed);
            // Leaves the file ending in the "+" marker so the game doesn't wait on a half batch.
            let _ = s.write_sync(&[], false);
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AUBLst Adapter")
            .with_inner_size([440.0, 250.0]),
        ..Default::default()
    };
    eframe::run_native("AUBLst Adapter", options, Box::new(|_| Ok(Box::new(AdapterApp::new()))))
}
