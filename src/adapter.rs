use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value, json};

pub const LOG_FILE: &str = "logfile.txt";
const MARKER: &str = "!LSTcmd::";
const PLAYER_MARKER: &str = "advertising game info: players=";

/// Everything both worker threads need once a session token exists.
#[derive(Clone)]
pub struct Session {
    pub api: String,
    pub game_dir: PathBuf,
    pub sync_file: PathBuf,
    pub token: String,
    pub mod_id: String,
    pub http: ureq::Agent,
    pub stop: Arc<AtomicBool>,
    // The log thread (handshake) and the command thread both write the sync file.
    pub sync_lock: Arc<Mutex<()>>,
    pub status: Arc<Mutex<String>>,
}

impl Session {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn report(&self, text: impl Into<String>) {
        *self.status.lock().unwrap() = text.into();
    }

    fn fail(&self, text: String) {
        log_error(&text);
        self.report(text);
    }

    /// Sleeps in short steps so closing the window doesn't wait for a full interval.
    fn pause(&self, total: Duration) {
        let step = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        while waited < total && !self.stopped() {
            thread::sleep(step);
            waited += step;
        }
    }

    pub fn write_sync(&self, lines: &[String], truncate: bool) -> io::Result<()> {
        let _guard = self.sync_lock.lock().unwrap();
        write_sync_file(&self.sync_file, lines, truncate)
    }
}

pub fn post(http: &ureq::Agent, url: &str, body: &Value) -> Result<Value, String> {
    let mut res = http.post(url).send_json(body).map_err(|e| format!("{url}: {e}"))?;
    res.body_mut().read_json::<Value>().map_err(|e| format!("{url}: {e}"))
}

pub fn log_error(message: &str) {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("error.log")))
        .unwrap_or_else(|| PathBuf::from("error.log"));
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}: {message}", chrono::Local::now().format("%d.%m.%Y %H:%M:%S"));
    }
}

/// How a JSON value lands in a command line for the game. Mirrors what the C# adapter
/// printed, so numbers keep their exact text (serde_json's arbitrary_precision) and
/// booleans come out as "True"/"False".
fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// Turns one pending command from AUBLst into the line the game reads from the sync
/// file. None for unknown types and for commands missing a field the line needs;
/// those stay unacknowledged.
pub fn command_line(cmd: &Value) -> Option<String> {
    let payload = match &cmd["payload"] {
        Value::String(s) => serde_json::from_str::<Value>(s).ok()?,
        v @ Value::Object(_) => v.clone(),
        _ => return None,
    };
    let field = |key: &str| payload.get(key).map(text);
    let event_id = present(payload.get("event_game_id"))
        .or(present(payload.get("event_id")))
        .map_or("-1".to_string(), text);
    let target = || {
        let t = payload.get("target")?;
        Some((text(t.get("x")?), text(t.get("y")?)))
    };

    Some(match cmd["type"].as_str()? {
        "assign" => {
            let player = present(payload.get("assign_to_player_id"))
                .map(text)
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "-1".into());
            let mode = field("mode").unwrap_or_else(|| "-1".into());
            let (x, y) = target()?;
            format!("203|{}|{event_id}|{x}|{y}|{player}|{mode}|", field("game_vehicle_id")?)
        }
        "unassign" => format!("204|{}|", field("game_vehicle_id")?),
        "event_create" => {
            let (x, y) = target()?;
            format!("110|{}|{x}|{y}|{}|", field("event_id")?, field("name")?)
        }
        "event_delete" => format!("112|{}|{}|", field("event_id")?, field("event_game_id")?),
        "message_create" => format!("311|{event_id}|{}|{}|", field("short_message")?, field("long_message")?),
        "message_revoke" => format!("312|{event_id}|{}|", field("short_message")?),
        _ => return None,
    })
}

/// What the game reported since the last successful sync.
#[derive(Default)]
pub struct Batch {
    vehicles: Vec<Value>,
    hospitals: Vec<Value>,
    events: Vec<Value>,
    messages: Vec<Value>,
    players: Map<String, Value>,
    time: Map<String, Value>,
}

impl Batch {
    /// Takes one log line. Returns the handshake text when the game announced a new
    /// round, which the caller answers through the sync file.
    pub fn process_line(&mut self, line: &str) -> Option<String> {
        if let Some(rest) = line.strip_prefix(PLAYER_MARKER) {
            self.players = rest
                .split(',')
                .enumerate()
                .map(|(i, name)| {
                    let letters: String = name.trim().chars().filter(char::is_ascii_alphabetic).collect();
                    (i.to_string(), Value::String(letters))
                })
                .collect();
            return None;
        }
        let message = line.strip_prefix(MARKER)?;
        let mut chars = message.chars();
        let kind = chars.next()?;
        let rest = chars.as_str();
        if kind == 'i' {
            return Some(rest.to_string());
        }
        let Ok(Value::Object(data)) = serde_json::from_str::<Value>(rest) else {
            return None;
        };
        match kind {
            'v' => self.vehicles.push(Value::Object(data)),
            'e' => self.events.push(Value::Object(data)),
            'h' => self.hospitals.push(Value::Object(data)),
            'm' => self.messages.push(Value::Object(data)),
            't' => self.time = data,
            _ => {}
        }
        None
    }

    /// Players and time only ride along; on their own they don't trigger a sync.
    pub fn has_data(&self) -> bool {
        !(self.vehicles.is_empty() && self.hospitals.is_empty() && self.events.is_empty() && self.messages.is_empty())
    }

    pub fn payload(&self, token: &str, mod_id: &str) -> Value {
        let mut data = json!({ "session_token": token, "mod_id": mod_id });
        let mut put = |key: &str, value: Value| data[key] = value;
        if !self.vehicles.is_empty() {
            put("vehicles", Value::Array(self.vehicles.clone()));
        }
        if !self.hospitals.is_empty() {
            put("hospitals", Value::Array(self.hospitals.clone()));
        }
        if !self.events.is_empty() {
            put("events", Value::Array(self.events.clone()));
        }
        if !self.players.is_empty() {
            put("players", Value::Object(self.players.clone()));
        }
        if !self.messages.is_empty() {
            put("messages", Value::Array(self.messages.clone()));
        }
        if !self.time.is_empty() {
            put("time", Value::Object(self.time.clone()));
        }
        data
    }
}

/// Reads the complete lines added since `pos`. A line the game is still writing stays
/// in the file for the next call instead of being cut in half.
pub fn read_new_lines(path: &Path, pos: &mut u64) -> io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    // The game starts its log over on every launch. Holding on to the old position
    // would seek past the end of the new one and miss the next handshake.
    if file.metadata()?.len() < *pos {
        *pos = 0;
    }
    file.seek(SeekFrom::Start(*pos))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let Some(last_newline) = buf.iter().rposition(|b| *b == b'\n') else {
        return Ok(Vec::new());
    };
    let complete = &buf[..=last_newline];
    *pos += complete.len() as u64;
    Ok(String::from_utf8_lossy(complete)
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect())
}

/// Appends command lines and the "+" end marker the game stops reading at.
/// `truncate` is only for the handshake: that is the one moment the game starts
/// reading at zero again. Truncating at any other time leaves the running game
/// seeking past the end, and every later command is lost without an error.
pub fn write_sync_file(path: &Path, lines: &[String], truncate: bool) -> io::Result<()> {
    let mut last_error = None;
    // The game may hold the file for a moment while it reads.
    for _ in 0..50 {
        let result = (|| {
            if truncate {
                File::create(path)?;
            } else {
                trim_end_marker(path)?;
            }
            let mut out = String::new();
            for line in lines {
                out.push_str(line);
                out.push_str("\r\n");
            }
            out.push_str("+\r\n");
            OpenOptions::new().create(true).append(true).open(path)?.write_all(out.as_bytes())
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(e) => last_error = Some(e),
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(last_error.unwrap())
}

/// Cuts the trailing "+" line so new commands land exactly where the game stopped.
/// Takes the marker with or without line breaks around it: the game reads in text
/// mode and only looks at the "+", and a marker left in place blocks it for the round.
fn trim_end_marker(path: &Path) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let window = len.min(4) as usize; // at most "\n+\r\n"
    if window == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-(window as i64)))?;
    let mut tail = vec![0u8; window];
    file.read_exact(&mut tail)?;
    let mut end = window;
    if tail[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && tail[end - 1] == b'\r' {
        end -= 1;
    }
    if end == 0 || tail[end - 1] != b'+' {
        return Ok(());
    }
    let marker_start = len - window as u64 + end as u64 - 1;
    // A data line ending in "+" is no marker.
    if marker_start > 0 && tail[end - 2] != b'\n' {
        return Ok(());
    }
    file.set_len(marker_start)
}

/// Follows the game log and forwards what the mod reports, once per second.
pub fn run_log_monitor(s: Session) {
    let log = s.game_dir.join(LOG_FILE);
    if let Err(e) = File::create(&log) {
        s.fail(format!("Logdatei {} lässt sich nicht leeren: {e}", log.display()));
    }
    let mut pos = 0;
    let mut batch = Batch::default();
    while !s.stopped() {
        match read_new_lines(&log, &mut pos) {
            Ok(lines) => {
                for line in lines {
                    if let Some(handshake) = batch.process_line(&line) {
                        if let Err(e) = s.write_sync(&[format!("42|{handshake}")], true) {
                            s.fail(format!("Sync-Datei: {e}"));
                        }
                    }
                }
            }
            Err(e) => s.fail(format!("Logdatei {}: {e}", log.display())),
        }
        if batch.has_data() {
            let url = format!("{}?action=sync", s.api);
            match post(&s.http, &url, &batch.payload(&s.token, &s.mod_id)) {
                // Only drop what the server confirmed; anything else goes out again next round.
                Ok(res) if res["ok"] == true => {
                    batch = Batch::default();
                    s.report(format!("Verbunden, zuletzt gesendet {}", chrono::Local::now().format("%H:%M:%S")));
                }
                Ok(res) => s.fail(format!("Sync abgelehnt: {res}")),
                Err(e) => s.fail(format!("Sync fehlgeschlagen: {e}")),
            }
        }
        s.pause(Duration::from_secs(1));
    }
}

/// Fetches pending commands every five seconds and hands them to the game.
pub fn run_command_poller(s: Session) {
    while !s.stopped() {
        if let Err(e) = poll_commands(&s) {
            s.fail(e);
        }
        s.pause(Duration::from_secs(5));
    }
}

fn poll_commands(s: &Session) -> Result<(), String> {
    let url = format!("{}?action=commands_pending", s.api);
    let res: Value = s
        .http
        .get(&url)
        .query("session_token", &s.token)
        .call()
        .and_then(|mut r| r.body_mut().read_json::<Value>())
        .map_err(|e| format!("Befehle abholen: {e}"))?;
    let Some(commands) = res["commands"].as_array() else {
        return Ok(());
    };
    let mut lines = Vec::new();
    let mut ids = Vec::new();
    for cmd in commands {
        match command_line(cmd) {
            Some(line) => {
                lines.push(line);
                ids.push(Value::String(text(&cmd["id"])));
            }
            None => log_error(&format!("Befehl nicht verstanden: {cmd}")),
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    // Acknowledge only what reached the file; the rest comes again next poll.
    s.write_sync(&lines, false).map_err(|e| format!("Sync-Datei: {e}"))?;
    post(
        &s.http,
        &format!("{}?action=commands_ack", s.api),
        &json!({ "session_token": s.token, "command_ids": ids }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(kind: &str, payload: Value) -> Value {
        json!({ "id": 7, "type": kind, "payload": payload.to_string() })
    }

    #[test]
    fn commands_become_game_lines() {
        let assign = cmd(
            "assign",
            json!({ "game_vehicle_id": 12, "event_id": 5, "event_game_id": null,
                    "target": { "x": 100.0, "y": -2.5 }, "assign_to_player_id": "", "mode": 1 }),
        );
        assert_eq!(command_line(&assign).unwrap(), "203|12|5|100.0|-2.5|-1|1|");

        // event_game_id wins over event_id, a missing mode is -1
        let assign = cmd(
            "assign",
            json!({ "game_vehicle_id": "12", "event_id": 5, "event_game_id": 9,
                    "target": { "x": 1, "y": 2 }, "assign_to_player_id": 3 }),
        );
        assert_eq!(command_line(&assign).unwrap(), "203|12|9|1|2|3|-1|");

        // vehicles_alarm without an event
        let alarm = cmd("assign", json!({ "game_vehicle_id": 4, "target": { "x": 1, "y": 2 }, "assign_to_player_id": null }));
        assert_eq!(command_line(&alarm).unwrap(), "203|4|-1|1|2|-1|-1|");

        assert_eq!(command_line(&cmd("unassign", json!({ "game_vehicle_id": 12 }))).unwrap(), "204|12|");
        assert_eq!(
            command_line(&cmd("event_create", json!({ "event_id": 3, "name": "Brand", "target": { "x": 1, "y": 2 } }))).unwrap(),
            "110|3|1|2|Brand|"
        );
        assert_eq!(command_line(&cmd("event_delete", json!({ "event_id": 3, "event_game_id": 8 }))).unwrap(), "112|3|8|");
        assert_eq!(
            command_line(&cmd("message_create", json!({ "event_id": 3, "short_message": "a", "long_message": "b" }))).unwrap(),
            "311|3|a|b|"
        );
        assert_eq!(command_line(&cmd("message_revoke", json!({ "event_game_id": 8, "short_message": "a" }))).unwrap(), "312|8|a|");

        // payload as an object instead of a JSON string
        let direct = json!({ "id": 1, "type": "unassign", "payload": { "game_vehicle_id": 2 } });
        assert_eq!(command_line(&direct).unwrap(), "204|2|");

        assert_eq!(command_line(&cmd("unknown", json!({}))), None);
        assert_eq!(command_line(&cmd("unassign", json!({}))), None);
        assert_eq!(command_line(&cmd("assign", json!({ "game_vehicle_id": 1 }))), None);
    }

    #[test]
    fn log_lines_fill_the_batch() {
        let mut b = Batch::default();
        assert_eq!(b.process_line(r#"!LSTcmd::v{"id":1}"#), None);
        assert_eq!(b.process_line(r#"!LSTcmd::t{"h":12}"#), None);
        assert_eq!(b.process_line("!LSTcmd::vkaputt"), None);
        assert_eq!(b.process_line("advertising game info: players= Anna_1, B o b ,"), None);
        assert_eq!(b.process_line("irgendwas anderes"), None);
        assert_eq!(b.process_line("!LSTcmd::i3|abc").as_deref(), Some("3|abc"));
        assert!(b.has_data());
        assert_eq!(
            b.payload("tok", "mod"),
            json!({ "session_token": "tok", "mod_id": "mod", "vehicles": [{ "id": 1 }],
                    "players": { "0": "Anna", "1": "Bob", "2": "" }, "time": { "h": 12 } })
        );

        let mut only_players = Batch::default();
        only_players.process_line("advertising game info: players=Anna");
        assert!(!only_players.has_data());
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("emlst-adapter-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn sync_file_keeps_one_end_marker() {
        let p = temp("sync.txt");
        write_sync_file(&p, &[], false).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "+\r\n");
        write_sync_file(&p, &["204|1|".into()], false).unwrap();
        write_sync_file(&p, &["204|2|".into()], false).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "204|1|\r\n204|2|\r\n+\r\n");

        // marker without line break, LF only, and a data line ending in "+"
        std::fs::write(&p, "a\n+").unwrap();
        write_sync_file(&p, &["b".into()], false).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\r\n+\r\n");
        std::fs::write(&p, "a+\r\n").unwrap();
        write_sync_file(&p, &["b".into()], false).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a+\r\nb\r\n+\r\n");

        write_sync_file(&p, &["42|x".into()], true).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "42|x\r\n+\r\n");
    }

    #[test]
    fn log_reader_waits_for_complete_lines_and_follows_restarts() {
        let p = temp("log.txt");
        std::fs::write(&p, "eins\r\nzw").unwrap();
        let mut pos = 0;
        assert_eq!(read_new_lines(&p, &mut pos).unwrap(), vec!["eins"]);
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"ei\r\n").unwrap();
        assert_eq!(read_new_lines(&p, &mut pos).unwrap(), vec!["zwei"]);
        assert!(read_new_lines(&p, &mut pos).unwrap().is_empty());
        std::fs::write(&p, "neu\n").unwrap();
        assert_eq!(read_new_lines(&p, &mut pos).unwrap(), vec!["neu"]);
    }
}
