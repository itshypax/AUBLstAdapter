#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adapter;
mod theme;
mod update;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, Margin, RichText, Stroke, Ui, Vec2, vec2};
use serde_json::{Value, json};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use adapter::{Level, Session, Shared, log_error, post};
use theme::{icon, with_icon};

// Same key and value names as the C# adapter, so a switch keeps the chosen paths.
const REG_KEY: &str = r"Software\EMLst";
const REG_GAME_DIR: &str = "SelectedFolderPath";
const REG_CONFIG: &str = "SelectedConfigPath";
const REG_STATUS_OPEN: &str = "StatusOpen";
const REG_LOG_OPEN: &str = "LogOpen";
const DEFAULT_GAME_DIR: &str = r"C:\Program Files\sixteentons entertainment\Emergency4";
const DEFAULT_SYNC_FILE: &str = "input.txt";
const BIG_BUTTON: Vec2 = vec2(150.0, 40.0);
const LOGO: &[u8] = include_bytes!("../assets/aublst.png");

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

/// "7f3c9a1e5b2d4c86" as "7f3c 9a1e 5b2d 4c86", easier to read out and compare.
fn grouped(code: &str) -> String {
    code.chars().collect::<Vec<_>>().chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
}

/// What a folder or config box shows.
enum Pick {
    Ok,
    Missing,
    Broken(String),
}

/// (colour, name, detail, what the folded status says when this row is the problem)
type StatusRow = (Color32, &'static str, String, &'static str);

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
    logo: Option<egui::TextureHandle>,
    update: Arc<Mutex<Option<update::Ready>>>,
    update_waiting: bool,
}

impl AdapterApp {
    fn new(ctx: &egui::Context) -> Self {
        let logo = eframe::icon_data::from_png_bytes(LOGO).ok().map(|icon| {
            let image = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
            ctx.load_texture("logo", image, egui::TextureOptions::LINEAR)
        });
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
            logo,
            update: Arc::new(Mutex::new(None)),
            update_waiting: false,
        };
        app.check_game_dir();
        app.reload_config();
        let (shared, slot) = (app.shared.clone(), app.update.clone());
        std::thread::spawn(move || *slot.lock().unwrap() = update::fetch(&shared));
        app
    }

    /// A downloaded update goes in only while no session is open: the restart would drop it.
    fn apply_update(&mut self) {
        let Some(ready) = self.update.lock().unwrap().clone() else { return };
        if self.session.is_some() {
            if !self.update_waiting {
                self.note(Level::Info, &format!("Version {} geladen, wird nach dem Trennen installiert", ready.version));
                self.update_waiting = true;
            }
            return;
        }
        *self.update.lock().unwrap() = None;
        match update::install(&ready) {
            Ok(()) => std::process::exit(0),
            Err(e) => self.fail(format!("Update auf {} fehlgeschlagen: {e}", ready.version)),
        }
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
        let log_upload_session = session.clone();
        std::thread::spawn(move || adapter::run_log_uploader(log_upload_session));
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

    /// Swaps the label for `done` for a moment instead of showing a separate notice.
    fn copy_button(&mut self, ui: &mut Ui, key: &'static str, label: String, done: String, text: String, enabled: bool) {
        let shown = match self.copied {
            Some((k, at)) if k == key && at.elapsed() < Duration::from_millis(1500) => done,
            _ => label,
        };
        if ui.add_enabled(enabled, egui::Button::new(shown)).on_hover_text("Kopieren").clicked() {
            ui.ctx().copy_text(text);
            self.copied = Some((key, Instant::now()));
            ui.ctx().request_repaint_after(Duration::from_millis(1600));
        }
    }

    fn status_rows(&self) -> [StatusRow; 4] {
        let sh = self.shared.lock().unwrap();
        let game = if self.game_running { (theme::GOOD, "Läuft") } else { (theme::OFF, "Nicht gestartet") };
        let game = (game.0, "Spiel", game.1.to_string(), "Spiel nicht gestartet");
        if self.session.is_none() {
            return [
                (theme::OFF, "Server", "Nicht verbunden".into(), "Nicht verbunden"),
                game,
                (theme::OFF, "Mod", "Keine Runde".into(), ""),
                (theme::OFF, "Befehle", "Keine".into(), ""),
            ];
        }
        let server = if let Some(t) = &sh.server_down_since {
            (theme::BAD, format!("Antwortet seit {t} nicht"), "Server antwortet nicht")
        } else if sh.rejecting {
            (theme::BAD, "Lehnt die Spieldaten ab".into(), "Server lehnt Daten ab")
        } else if let Some(t) = &sh.last_sent {
            (theme::GOOD, format!("Daten zuletzt {t} gesendet"), "")
        } else {
            (theme::GOOD, "Noch keine Spieldaten".into(), "")
        };
        let round = match &sh.round_since {
            Some(t) => (theme::GOOD, format!("Seit {t} in der Runde"), ""),
            None => (theme::WARN, "Wartet auf den Rundenstart".into(), "Wartet auf die Mod"),
        };
        let commands = if sh.unacked > 0 {
            let verb = if sh.unacked == 1 { "wartet" } else { "warten" };
            (theme::WARN, format!("{} {verb} auf Bestätigung", sh.unacked), "Befehle unbestätigt")
        } else if sh.round_since.is_none() {
            (theme::WARN, "Bleiben auf dem Server".into(), "Befehle warten")
        } else if sh.delivered > 0 {
            (theme::GOOD, format!("{} ans Spiel übergeben", sh.delivered), "")
        } else {
            (theme::GOOD, "Noch keine".into(), "")
        };
        [
            (server.0, "Server", server.1, server.2),
            game,
            (round.0, "Mod", round.1, round.2),
            (commands.0, "Befehle", commands.1, commands.2),
        ]
    }

    fn header(&self, ui: &mut Ui, rows: &[StatusRow; 4]) {
        // Like the connection note in the AUBLst top bar: icon and coloured text, no frame.
        let (color, text) = if self.session.is_none() {
            (theme::DIM, "Getrennt")
        } else if rows.iter().any(|r| r.0 == theme::BAD) {
            (theme::BAD_TEXT, "Störung")
        } else if rows[2].0 != theme::GOOD {
            (theme::WARN_TEXT, "Wartet aufs Spiel")
        } else {
            (if rows.iter().all(|r| r.0 == theme::GOOD) { theme::GOOD_TEXT } else { theme::WARN_TEXT }, "Verbunden")
        };
        let wifi = if color == theme::GOOD_TEXT { icon::WIFI } else { icon::WIFI_OFF };
        ui.horizontal(|ui| {
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::from_texture((logo.id(), vec2(32.0, 32.0))).corner_radius(5));
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("AUBLst Adapter").size(16.0).family(theme::semibold()).color(theme::STRONG));
                ui.label(RichText::new("Emergency 4 und Leitstelle verbinden").size(11.5).color(theme::DIM));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(text).size(12.0).color(color));
                ui.label(RichText::new(wifi).size(14.0).color(color));
            });
        });
    }

    fn connection(&mut self, ui: &mut Ui) {
        let connected = self.session.is_some();
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if connected {
                    let label = RichText::new(with_icon(icon::DISCONNECT, "Trennen")).size(15.0).color(theme::STRONG);
                    if ui.add(egui::Button::new(label).fill(theme::DANGER_FILL).min_size(BIG_BUTTON)).clicked() {
                        self.disconnect();
                    }
                } else {
                    let label = RichText::new(with_icon(icon::CONNECT, "Code anfordern")).size(15.0).color(theme::ON_PRIMARY);
                    let button = egui::Button::new(label).fill(theme::PRIMARY).min_size(BIG_BUTTON);
                    let hover = "Für einen Code braucht es Spielordner und Config.";
                    if ui.add_enabled(self.ready(), button).on_disabled_hover_text(hover).clicked() {
                        self.request_token();
                    }
                }
                ui.add_enabled(
                    !connected,
                    egui::TextEdit::singleline(&mut self.pin)
                        .hint_text(
                            RichText::new(if connected { "Keine PIN" } else { "PIN (optional)" }).size(17.0).color(theme::HINT),
                        )
                        .font(egui::FontId::proportional(17.0))
                        .char_limit(10)
                        .margin(Margin::symmetric(10, 9))
                        .desired_width(f32::INFINITY),
                );
            });
        });

        egui::Frame::new()
            .fill(theme::FIELD)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .corner_radius(4)
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.set_min_height(28.0);
                    if connected {
                        ui.label(RichText::new(grouped(&self.token)).monospace().size(19.0).color(theme::GOOD));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let token = self.token.clone();
                            self.copy_button(ui, "code", icon::COPY.into(), icon::CHECK.into(), token, true);
                        });
                    } else {
                        ui.label(RichText::new("Noch kein Code").size(17.0).color(theme::HINT));
                    }
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
                    let (label, done) = (with_icon(icon::LINK, "Link kopieren"), with_icon(icon::CHECK, "Kopiert"));
                    self.copy_button(ui, "link", label, done, url.clone(), true);
                    ui.add(egui::Label::new(RichText::new(&url).color(theme::LINK)).truncate()).on_hover_text(&url);
                });
            });
        }
    }

    /// A folder or config box: name large, full path below. Yellow while nothing is
    /// chosen, red when the choice is unusable.
    fn pick_box(ui: &mut Ui, title: &str, icons: (&str, &str), path: &str, state: &Pick, enabled: bool) -> bool {
        let name = Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned());
        let (accent, fill, big, small) = match state {
            Pick::Ok => (theme::DIM, theme::BOX, (name, theme::STRONG), Some((path.to_string(), theme::DIM))),
            Pick::Missing => (theme::WARN, theme::WARN_BG, (format!("{title} nicht gewählt"), theme::WARN), None),
            Pick::Broken(e) => (theme::BAD, theme::BAD_BG, (name, theme::STRONG), Some((e.clone(), theme::BAD))),
        };
        let stroke = if matches!(state, Pick::Ok) { theme::BORDER } else { accent };
        let mut clicked = false;
        egui::Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0, stroke))
            .corner_radius(4)
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons.0).size(20.0).color(accent));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mut button = egui::Button::new(with_icon(icons.1, "Wählen"));
                        if !matches!(state, Pick::Ok) {
                            button = button.fill(theme::WARN_FILL);
                        }
                        clicked = ui.add_enabled(enabled, button).on_disabled_hover_text("Erst trennen").clicked();
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.add(egui::Label::new(RichText::new(big.0).size(14.5).color(big.1)).truncate())
                                .on_hover_text(path);
                            if let Some((text, color)) = small {
                                ui.add(egui::Label::new(RichText::new(text).size(11.5).color(color)).truncate());
                            }
                        });
                    });
                });
            });
        clicked
    }

    fn paths(&mut self, ui: &mut Ui) {
        let connected = self.session.is_some();
        let folder = (icon::FOLDER, icon::FOLDER);
        if Self::pick_box(ui, "Spielordner", folder, &self.game_dir, &self.game_dir_state, !connected)
            && let Some(dir) = rfd::FileDialog::new().set_directory(&self.game_dir).pick_folder()
        {
            self.game_dir = dir.display().to_string();
            write_setting(REG_GAME_DIR, &self.game_dir);
            self.check_game_dir();
        }
        let shown = adapter::beside_exe(&self.config_path).display().to_string();
        if Self::pick_box(ui, "Config", (icon::FILE, icon::FILE), &shown, &self.config_state, !connected)
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
            ui.label(RichText::new(note).color(theme::DIM));
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

    fn status(&mut self, ui: &mut Ui, rows: &[StatusRow; 4]) {
        // Folded, only the most pressing problem in a few words; the details are one click away.
        let worst = rows.iter().find(|r| r.0 == theme::BAD).or_else(|| rows.iter().find(|r| r.0 != theme::GOOD));
        let (color, summary) = match worst {
            _ if self.session.is_none() => (theme::OFF, "Nicht verbunden"),
            Some(&(color, _, _, short)) => (if color == theme::BAD { theme::BAD } else { theme::WARN }, short),
            None => (theme::GOOD, "Alles läuft"),
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
            // Fixed columns: a frame only knows its width after drawing, so a wrapping row can't break before it.
            for pair in rows.chunks(2) {
                ui.columns(2, |cols| {
                    for (ui, (color, key, value, _)) in cols.iter_mut().zip(pair) {
                        egui::Frame::new()
                            .stroke(Stroke::new(1.0, theme::BORDER))
                            .corner_radius(12)
                            .inner_margin(Margin::symmetric(9, 3))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    dot(ui, *color);
                                    ui.label(RichText::new(*key).color(theme::DIM));
                                    ui.add(egui::Label::new(value).truncate()).on_hover_text(value);
                                });
                            });
                    }
                });
            }
        }
    }

    fn log(&mut self, ui: &mut Ui) {
        let style = |level: Level| match level {
            Level::Info => (icon::INFO, theme::TEXT),
            Level::Good => (icon::GOOD, theme::GOOD),
            Level::Warn => (icon::WARN, theme::WARN),
            Level::Bad => (icon::BAD, theme::BAD),
        };
        // (shown, colour, copied)
        let (last, lines) = {
            let sh = self.shared.lock().unwrap();
            let entry = |e: &adapter::Entry| {
                let (icon, color) = style(e.level);
                (format!("{}  {icon}  {}", e.time, e.text), color, format!("{} {}", e.time, e.text))
            };
            let last = sh.log.back().map(entry);
            let lines: Vec<(String, Color32, String)> = if !self.log_open {
                Vec::new()
            } else if self.show_game_log {
                sh.game_log.iter().map(|l| (l.clone(), theme::DIM, l.clone())).collect()
            } else {
                sh.log.iter().map(entry).collect()
            };
            (last, lines)
        };

        let toggled = fold_header(ui, self.log_open, |ui| {
            let (text, color) = last.map_or(("Noch keine Einträge".into(), theme::DIM), |(t, c, _)| (t, c));
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
                let all = lines.iter().map(|(_, _, plain)| plain.as_str()).collect::<Vec<_>>().join("\r\n");
                let (label, done) = (with_icon(icon::COPY, "Kopieren"), with_icon(icon::CHECK, "Kopiert"));
                self.copy_button(ui, "log", label, done, all, !lines.is_empty());
                ui.checkbox(&mut self.follow_log, "Mitscrollen");
            });
        });
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace) + 3.0;
        egui::Frame::new()
            .fill(theme::FIELD)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .corner_radius(4)
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                egui::ScrollArea::both()
                    .auto_shrink(false)
                    .stick_to_bottom(self.follow_log)
                    .show_rows(ui, row_height, lines.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for (text, color, _) in &lines[range] {
                            ui.add(egui::Label::new(RichText::new(text).monospace().color(*color)).extend());
                        }
                    });
            });
    }
}

/// A section of the window: a card with a small heading.
fn card(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(8)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(RichText::new(title.to_uppercase()).size(11.0).family(theme::semibold()).color(theme::DIM));
            add(ui);
        });
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
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

impl eframe::App for AdapterApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Worker threads only update shared state; repaint now and then to show it.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        if self.game_checked.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.game_running = adapter::game_running();
            self.game_checked = Some(Instant::now());
        }
        self.apply_update();
        let rows = self.status_rows();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::same(12))).show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 10.0);
            ui.spacing_mut().button_padding = vec2(10.0, 5.0);
            self.header(ui, &rows);
            card(ui, "Verbindung", |ui| self.connection(ui));
            card(ui, "Spiel", |ui| self.paths(ui));
            card(ui, "Status und Log", |ui| {
                self.status(ui, &rows);
                ui.separator();
                self.log(ui);
            });
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
        .with_inner_size([520.0, 760.0])
        .with_min_inner_size([460.0, 520.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(LOGO) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "AUBLst Adapter",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            cc.egui_ctx.set_visuals_of(egui::Theme::Dark, theme::visuals());
            cc.egui_ctx.set_fonts(theme::fonts());
            Ok(Box::new(AdapterApp::new(&cc.egui_ctx)))
        }),
    )
}
