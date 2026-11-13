#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adapter;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, Margin, RichText, Stroke, Ui, Vec2, vec2};
use serde_json::{Value, json};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use adapter::{Level, Session, Shared, log_error, post};

// Same key and value names as the C# adapter, so a switch keeps the chosen paths.
const REG_KEY: &str = r"Software\EMLst";
const REG_GAME_DIR: &str = "SelectedFolderPath";
const REG_CONFIG: &str = "SelectedConfigPath";
const REG_STATUS_OPEN: &str = "StatusOpen";
const REG_LOG_OPEN: &str = "LogOpen";
const DEFAULT_GAME_DIR: &str = r"C:\Program Files\sixteentons entertainment\Emergency4";
const DEFAULT_SYNC_FILE: &str = "input.txt";

const GREEN: Color32 = Color32::from_rgb(95, 191, 106);
const YELLOW: Color32 = Color32::from_rgb(217, 180, 74);
const RED: Color32 = Color32::from_rgb(224, 96, 96);
const GREY: Color32 = Color32::from_rgb(90, 90, 90);
const BLUE: Color32 = Color32::from_rgb(0, 92, 128);
const DARK_RED: Color32 = Color32::from_rgb(107, 38, 38);
const BIG_BUTTON: Vec2 = vec2(150.0, 36.0);

/// Codepoints in assets/Phosphor.ttf, from phosphoricons.com.
mod icon {
    pub const CONNECT: &str = "\u{EB5A}";
    pub const DISCONNECT: &str = "\u{EB56}";
    pub const COPY: &str = "\u{E1CA}";
    pub const LINK: &str = "\u{E2E2}";
    pub const OPEN: &str = "\u{E5DE}";
    pub const FOLDER: &str = "\u{E256}";
    pub const FILE: &str = "\u{E23A}";
    pub const PLAY: &str = "\u{E3D0}";
}

fn with_icon(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}

struct Config {
    api: String,
    mod_id: Value,
    raw: Value,
}

fn load_config(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("Lässt sich nicht lesen: {e}"))?;
    let raw: Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("Kein gültiges JSON: {e}"))?;
    let api = raw["server_path"].as_str().ok_or("In der Config fehlt server_path")?.to_string();
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

/// What a folder or config box shows.
enum Pick {
    Ok,
    Missing,
    Broken(String),
}

struct AdapterApp {
    pin: String,
    token: String,
    game_dir: String,
    game_dir_state: Pick,
    config_path: String,
    config: Option<Config>,
    config_state: Pick,
    session: Option<Session>,
    http: ureq::Agent,
    shared: Arc<Mutex<Shared>>,
    // One guard for every session: threads of a session just disconnected may still
    // be finishing a write to the sync file.
    sync: Arc<Mutex<bool>>,
    game_running: bool,
    game_checked: Option<Instant>,
    status_open: bool,
    log_open: bool,
    show_game_log: bool,
    follow_log: bool,
    copied: Option<(&'static str, Instant)>,
}

impl AdapterApp {
    fn new() -> Self {
        let mut app = AdapterApp {
            pin: String::new(),
            token: String::new(),
            game_dir: read_setting(REG_GAME_DIR, DEFAULT_GAME_DIR),
            game_dir_state: Pick::Missing,
            config_path: read_setting(REG_CONFIG, "config.cfg"),
            config: None,
            config_state: Pick::Missing,
            session: None,
            http: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(10)))
                // AUBLst explains a 4xx in the body; `adapter::reply` reads it.
                .http_status_as_error(false)
                .build()
                .into(),
            shared: Arc::new(Mutex::new(Shared::default())),
            sync: Arc::new(Mutex::new(false)),
            game_running: false,
            game_checked: None,
            status_open: read_setting(REG_STATUS_OPEN, "0") == "1",
            log_open: read_setting(REG_LOG_OPEN, "0") == "1",
            show_game_log: false,
            follow_log: true,
            copied: None,
        };
        app.check_game_dir();
        app.reload_config();
        app
    }

    fn fail(&self, text: String) {
        adapter::fail(&self.shared, text);
    }

    fn note(&self, level: Level, text: &str) {
        self.shared.lock().unwrap().push(level, text.into());
    }

    fn check_game_dir(&mut self) {
        let dir = Path::new(&self.game_dir);
        self.game_dir_state = if !dir.is_dir() {
            Pick::Missing
        } else if !dir.join("Em4.exe").is_file() {
            Pick::Broken("Hier liegt keine Em4.exe".into())
        } else {
            Pick::Ok
        };
    }

    fn reload_config(&mut self) {
        let path = adapter::beside_exe(&self.config_path);
        self.config = None;
        if !path.is_file() {
            self.config_state = Pick::Missing;
            return;
        }
        match load_config(&path) {
            Ok(c) => {
                self.config = Some(c);
                self.config_state = Pick::Ok;
            }
            Err(e) => {
                log_error(&format!("Config {}: {e}", path.display()));
                self.config_state = Pick::Broken(e);
            }
        }
    }

    fn ready(&self) -> bool {
        matches!(self.game_dir_state, Pick::Ok) && self.config.is_some()
    }

    // shortcut: runs on the UI thread, the window freezes for up to the 10 s timeout
    // when the server hangs. Move it to a thread if that turns out to happen.
    fn request_token(&mut self) {
        let Some(config) = &self.config else { return };
        let mut body = config.raw["request_data"].clone();
        if !body.is_object() {
            return self.fail("In der Config fehlt request_data".into());
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
                None => self.fail(format!("Kein Code erhalten: {res}")),
            },
            Err(e) => self.fail(format!("Code anfordern fehlgeschlagen: {e}")),
        }
    }

    fn start(&mut self) {
        let Some(config) = &self.config else { return };
        self.shared.lock().unwrap().reset();
        *self.sync.lock().unwrap() = false;
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
            sync: self.sync.clone(),
            shared: self.shared.clone(),
        };
        self.note(Level::Good, "Code erhalten");

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
        self.session = Some(session);
    }

    fn disconnect(&mut self) {
        if let Some(s) = self.session.take() {
            s.stop.store(true, Ordering::Relaxed);
            // A command poll still running must not write into a round that is gone.
            *self.sync.lock().unwrap() = false;
            self.note(Level::Info, "Getrennt");
        }
        self.token.clear();
    }

    fn dispatch_url(&self) -> Option<String> {
        let config = self.config.as_ref().filter(|_| !self.token.is_empty())?;
        Some(format!("{}frontend?session_token={}", config.api.replace("backend/api.php", ""), self.token))
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

    /// Shows "Kopiert" on the button for a moment instead of a separate notice.
    fn copy_button(&mut self, ui: &mut Ui, key: &'static str, label: &str, text: String, size: Vec2, enabled: bool) {
        let shown = match self.copied {
            Some((k, at)) if k == key && at.elapsed() < Duration::from_millis(1500) => with_icon(icon::COPY, "Kopiert"),
            _ => label.to_string(),
        };
        if ui.add_enabled(enabled, egui::Button::new(shown).min_size(size)).clicked() {
            ui.ctx().copy_text(text);
            self.copied = Some((key, Instant::now()));
            ui.ctx().request_repaint_after(Duration::from_millis(1600));
        }
    }

    fn connection(&mut self, ui: &mut Ui) {
        let connected = self.session.is_some();
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if connected {
                    let label = RichText::new(with_icon(icon::DISCONNECT, "Trennen")).size(15.0);
                    let button = egui::Button::new(label).fill(DARK_RED).min_size(BIG_BUTTON);
                    if ui.add(button).clicked() {
                        self.disconnect();
                    }
                } else {
                    let label = RichText::new(with_icon(icon::CONNECT, "Code anfordern")).size(15.0);
                    let button = egui::Button::new(label).fill(BLUE).min_size(BIG_BUTTON);
                    let hover = "Für einen Code braucht es Spielordner und Config.";
                    if ui.add_enabled(self.ready(), button).on_disabled_hover_text(hover).clicked() {
                        self.request_token();
                    }
                }
                ui.add_enabled(
                    !connected,
                    egui::TextEdit::singleline(&mut self.pin)
                        .hint_text(if connected { "Keine PIN" } else { "PIN (optional)" })
                        .font(egui::FontId::monospace(18.0))
                        .char_limit(10)
                        .margin(Margin::symmetric(10, 8))
                        .desired_width(f32::INFINITY),
                );
            });
        });
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let token = self.token.clone();
                self.copy_button(ui, "code", &with_icon(icon::COPY, "Kopieren"), token, BIG_BUTTON, connected);
                ui.add(
                    egui::TextEdit::singleline(&mut self.token.as_str())
                        .hint_text("Noch kein Code")
                        .background_color(ui.visuals().extreme_bg_color)
                        .font(egui::FontId::monospace(18.0))
                        .text_color(GREEN)
                        .margin(Margin::symmetric(10, 8))
                        .desired_width(f32::INFINITY),
                );
            });
        });
        if let Some(url) = self.dispatch_url().filter(|_| connected) {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(with_icon(icon::OPEN, "Öffnen")).clicked()
                        && let Err(e) = std::process::Command::new("explorer").arg(&url).spawn()
                    {
                        self.fail(format!("Browser öffnen fehlgeschlagen: {e}"));
                    }
                    self.copy_button(ui, "link", &with_icon(icon::LINK, "Link kopieren"), url.clone(), Vec2::ZERO, true);
                    let color = ui.visuals().hyperlink_color;
                    ui.add(egui::Label::new(RichText::new(&url).color(color)).truncate()).on_hover_text(&url);
                });
            });
        }
    }

    /// A folder or config box: yellow while nothing is chosen, red when the choice is unusable.
    fn pick_box(ui: &mut Ui, title: &str, icon: &str, path: &str, state: &Pick, enabled: bool) -> bool {
        let (stroke, fill, value, color) = match state {
            Pick::Ok => (Color32::from_rgb(51, 51, 51), Color32::from_rgb(34, 34, 34), path.to_string(), Color32::from_rgb(240, 240, 240)),
            Pick::Missing => (YELLOW, Color32::from_rgb(42, 37, 22), "Nicht gewählt".to_string(), YELLOW),
            Pick::Broken(e) => (RED, Color32::from_rgb(42, 25, 25), e.clone(), RED),
        };
        let mut clicked = false;
        egui::Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0, stroke))
            .corner_radius(4)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mut button = egui::Button::new(with_icon(icon, "Wählen"));
                        if !matches!(state, Pick::Ok) {
                            button = button.fill(Color32::from_rgb(138, 109, 30));
                        }
                        clicked = ui.add_enabled(enabled, button).on_disabled_hover_text("Erst trennen").clicked();
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(RichText::new(title).small().weak());
                            ui.add(egui::Label::new(RichText::new(value).color(color)).truncate()).on_hover_text(path);
                        });
                    });
                });
            });
        clicked
    }

    fn paths(&mut self, ui: &mut Ui) {
        let connected = self.session.is_some();
        if Self::pick_box(ui, "Spielordner", icon::FOLDER, &self.game_dir, &self.game_dir_state, !connected)
            && let Some(dir) = rfd::FileDialog::new().set_directory(&self.game_dir).pick_folder()
        {
            self.game_dir = dir.display().to_string();
            write_setting(REG_GAME_DIR, &self.game_dir);
            self.check_game_dir();
        }
        let shown = adapter::beside_exe(&self.config_path).display().to_string();
        if Self::pick_box(ui, "Config", icon::FILE, &shown, &self.config_state, !connected)
            && let Some(file) = rfd::FileDialog::new().add_filter("Config", &["cfg"]).pick_file()
        {
            self.config_path = file.display().to_string();
            write_setting(REG_CONFIG, &self.config_path);
            self.reload_config();
        }
        ui.horizontal(|ui| {
            let note = if connected {
                "Zum Ändern erst trennen."
            } else if !self.ready() {
                "Für einen Code braucht es Spielordner und Config."
            } else {
                ""
            };
            ui.label(RichText::new(note).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can_start = matches!(self.game_dir_state, Pick::Ok) && self.config.is_some() && !self.game_running;
                let hover = if self.game_running { "Em4.exe läuft schon" } else { "Erst Spielordner und Config wählen" };
                let button = egui::Button::new(with_icon(icon::PLAY, "Spiel starten"));
                if ui.add_enabled(can_start, button).on_disabled_hover_text(hover).clicked() {
                    self.run_game();
                }
            });
        });
    }

    fn status_rows(&self, sh: &Shared) -> [(Color32, &'static str, String); 4] {
        let game = if self.game_running { (GREEN, "Läuft") } else { (GREY, "Nicht gestartet") };
        let game = (game.0, "Spiel", game.1.to_string());
        if self.session.is_none() {
            return [
                (GREY, "Server", "Nicht verbunden".into()),
                game,
                (GREY, "Mod", "Keine Runde".into()),
                (GREY, "Befehle", "Keine".into()),
            ];
        }
        let server = if let Some(t) = &sh.server_down_since {
            (RED, format!("Antwortet seit {t} nicht"))
        } else if sh.rejecting {
            (RED, "Lehnt die Spieldaten ab".into())
        } else if let Some(t) = &sh.last_sent {
            (GREEN, format!("Daten zuletzt {t} gesendet"))
        } else {
            (GREEN, "Verbunden, noch keine Spieldaten".into())
        };
        let round = match &sh.round_since {
            Some(t) => (GREEN, format!("Seit {t} in der Runde")),
            None => (YELLOW, "Wartet auf den Rundenstart".into()),
        };
        let commands = if sh.unacked > 0 {
            (YELLOW, format!("{} {} auf Bestätigung", sh.unacked, if sh.unacked == 1 { "wartet" } else { "warten" }))
        } else if sh.round_since.is_none() {
            (YELLOW, "Bleiben auf dem Server".into())
        } else if sh.delivered > 0 {
            (GREEN, format!("{} ans Spiel übergeben", sh.delivered))
        } else {
            (GREEN, "Noch keine".into())
        };
        [(server.0, "Server", server.1), game, (round.0, "Mod", round.1), (commands.0, "Befehle", commands.1)]
    }

    fn status(&mut self, ui: &mut Ui) {
        let rows = self.status_rows(&self.shared.lock().unwrap());
        let (color, summary) = if self.session.is_none() {
            (GREY, "Nicht verbunden".to_string())
        } else if let Some((_, k, v)) = rows.iter().find(|r| r.0 == RED) {
            (RED, format!("{k}: {v}"))
        } else {
            let open: Vec<String> = rows.iter().filter(|r| r.0 != GREEN).map(|(_, k, v)| format!("{k}: {v}")).collect();
            if open.is_empty() { (GREEN, "Alles läuft".to_string()) } else { (YELLOW, open.join(", ")) }
        };
        let toggled = fold_header(ui, self.status_open, |ui| {
            dot(ui, color);
            ui.add(egui::Label::new(summary).truncate().selectable(false));
        });
        if toggled {
            self.status_open = !self.status_open;
            write_setting(REG_STATUS_OPEN, if self.status_open { "1" } else { "0" });
        }
        if self.status_open {
            ui.indent("status", |ui| {
                egui::Grid::new("status_rows").num_columns(2).spacing(vec2(12.0, 4.0)).show(ui, |ui| {
                    for (color, key, value) in &rows {
                        ui.label(RichText::new(*key).weak());
                        ui.horizontal(|ui| {
                            dot(ui, *color);
                            ui.label(value);
                        });
                        ui.end_row();
                    }
                });
            });
        }
    }

    fn log(&mut self, ui: &mut Ui) {
        let text_color = ui.visuals().text_color();
        let weak = ui.visuals().weak_text_color();
        let level_color = |level: Level| match level {
            Level::Info => text_color,
            Level::Good => GREEN,
            Level::Warn => YELLOW,
            Level::Bad => RED,
        };
        let (last, lines) = {
            let sh = self.shared.lock().unwrap();
            let last = sh.log.back().map(|e| (format!("{} {}", e.time, e.text), level_color(e.level)));
            let lines: Vec<(String, Color32)> = if !self.log_open {
                Vec::new()
            } else if self.show_game_log {
                sh.game_log.iter().map(|l| (l.clone(), weak)).collect()
            } else {
                sh.log.iter().map(|e| (format!("{} {}", e.time, e.text), level_color(e.level))).collect()
            };
            (last, lines)
        };

        let toggled = fold_header(ui, self.log_open, |ui| {
            let (text, color) = last.unwrap_or_else(|| ("Noch keine Einträge".into(), weak));
            ui.add(egui::Label::new(RichText::new(text).color(color)).truncate().selectable(false));
        });
        if toggled {
            self.log_open = !self.log_open;
            write_setting(REG_LOG_OPEN, if self.log_open { "1" } else { "0" });
        }
        if !self.log_open {
            return;
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.show_game_log, false, "Adapter");
            ui.selectable_value(&mut self.show_game_log, true, "Spiel-Log");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let all = lines.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\r\n");
                self.copy_button(ui, "log", &with_icon(icon::COPY, "Kopieren"), all, Vec2::ZERO, !lines.is_empty());
                ui.checkbox(&mut self.follow_log, "Mitscrollen");
            });
        });
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(3)
            .inner_margin(Margin::same(6))
            .show(ui, |ui| {
                egui::ScrollArea::both()
                    .auto_shrink(false)
                    .stick_to_bottom(self.follow_log)
                    .show_rows(ui, row_height, lines.len(), |ui, range| {
                        for (text, color) in &lines[range] {
                            ui.add(egui::Label::new(RichText::new(text).monospace().color(*color)).extend());
                        }
                    });
            });
    }
}

/// A quiet header line that opens and closes the section below it. True when clicked.
fn fold_header(ui: &mut Ui, open: bool, add: impl FnOnce(&mut Ui)) -> bool {
    let row = ui.horizontal(|ui| {
        let (_, response) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
        egui::collapsing_header::paint_default_icon(ui, if open { 1.0 } else { 0.0 }, &response);
        add(ui);
    });
    row.response.interact(egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

impl eframe::App for AdapterApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Worker threads only update shared state; repaint now and then to show it.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        if self.game_checked.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.game_running = adapter::game_running();
            self.game_checked = Some(Instant::now());
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 10.0);
            self.connection(ui);
            self.paths(ui);
            ui.separator();
            self.status(ui);
            ui.separator();
            self.log(ui);
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
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("AUBLst Adapter")
        .with_inner_size([500.0, 640.0])
        .with_min_inner_size([440.0, 360.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/aublst.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "AUBLst Adapter",
        options,
        Box::new(|cc| {
            // The boxes and log colours are made for the dark look.
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("phosphor".into(), egui::FontData::from_static(include_bytes!("../assets/Phosphor.ttf")).into());
            // As a fallback after the text font, so icons mix into ordinary labels.
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.push("phosphor".into());
            }
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(AdapterApp::new()))
        }),
    )
}
