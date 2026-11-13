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
    /// Guards the sync file, which both threads write. The flag says whether a game
    /// round is reading it: commands written outside a round are wiped by the next
    /// handshake while the server already counts them as delivered.
    pub sync: Arc<Mutex<bool>>,
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
        fail(&self.status, text);
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
        let _guard = self.sync.lock().unwrap();
        write_sync_file(&self.sync_file, lines, truncate)
    }

    fn round_active(&self) -> bool {
        *self.sync.lock().unwrap()
    }

    fn set_round(&self, active: bool) {
        *self.sync.lock().unwrap() = active;
    }

    /// Answers the game's handshake. From here on the game reads the sync file from the start.
    fn handshake(&self, text: &str) -> io::Result<()> {
        let mut active = self.sync.lock().unwrap();
        write_sync_file(&self.sync_file, &[format!("42|{text}")], true)?;
        *active = true;
        Ok(())
    }

    /// Writes commands only while a round is running. Ok(false) means nothing was
    /// written, so the commands must stay unacknowledged.
    fn deliver(&self, lines: &[String]) -> io::Result<bool> {
        let active = self.sync.lock().unwrap();
        if !*active {
            return Ok(false);
        }
        write_sync_file(&self.sync_file, lines, false)?;
        Ok(true)
    }
}

#[derive(Debug)]
pub struct HttpError {
    /// The server answered, so sending the same data again won't help. False for
    /// connection trouble, which is worth retrying.
    pub rejected: bool,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// AUBLst reports errors as HTTP 4xx/5xx; their body says why, so it ends up in the message.
fn reply(url: &str, res: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value, HttpError> {
    let failed = |e: ureq::Error| HttpError {
        rejected: matches!(e, ureq::Error::StatusCode(_)),
        message: format!("{url}: {e}"),
    };
    let mut res = res.map_err(failed)?;
    let status = res.status();
    let body = res.body_mut().read_to_string().map_err(failed)?;
    let rejected = |why: String| {
        let mut message = format!("{url}: {why}: {body}");
        message.truncate(message.floor_char_boundary(500));
        HttpError { rejected: true, message }
    };
    if !status.is_success() {
        return Err(rejected(format!("HTTP {}", status.as_u16())));
    }
    serde_json::from_str(&body).map_err(|e| rejected(e.to_string()))
}

pub fn post(http: &ureq::Agent, url: &str, body: &Value) -> Result<Value, HttpError> {
    reply(url, http.post(url).send_json(body))
}

/// Resolves a relative path against the exe's folder instead of whatever working
/// directory a launcher picked.
pub fn beside_exe(path: impl AsRef<Path>) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(&path)))
        .unwrap_or_else(|| path.as_ref().to_path_buf())
}

pub fn log_error(message: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(beside_exe("error.log")) {
        let _ = writeln!(f, "{}: {message}", chrono::Local::now().format("%d.%m.%Y %H:%M:%S"));
    }
}

pub fn fail(status: &Mutex<String>, text: String) {
    log_error(&text);
    *status.lock().unwrap() = text;
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
    rejections: u32,
}

/// How often the server may answer a sync without "ok" before the batch is dropped.
/// A batch the server can't take would otherwise block every later update.
const MAX_REJECTIONS: u32 = 5;

impl Batch {
    /// Counts a sync the server answered without "ok". True once the batch should be dropped.
    pub fn rejected(&mut self) -> bool {
        self.rejections += 1;
        self.rejections >= MAX_REJECTIONS
    }

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
            'v' => merge(&mut self.vehicles, data, "game_vehicle_id"),
            'e' => self.events.push(Value::Object(data)),
            'h' => merge(&mut self.hospitals, data, "game_hospital_id"),
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

/// Keeps one entry per id, newer fields winning, as the server ends up storing it anyway.
/// While the server is down the game reports every second, and a list of every report
/// soon outgrows what the server accepts in one request.
fn merge(list: &mut Vec<Value>, data: Map<String, Value>, key: &str) {
    let id = data.get(key).map(text);
    let same = |v: &&mut Value| id.is_some() && v.get(key).map(text) == id;
    match list.iter_mut().find(same) {
        Some(Value::Object(old)) => old.extend(data),
        _ => list.push(Value::Object(data)),
    }
}

/// Reads the complete lines added since `pos`. A line the game is still writing stays
/// in the file for the next call instead of being cut in half. The flag is true when
/// the log started over, which means the game was restarted.
pub fn read_new_lines(path: &Path, pos: &mut u64) -> io::Result<(Vec<String>, bool)> {
    let mut file = File::open(path)?;
    // Holding on to the old position would seek past the end of the new log and miss
    // the next handshake.
    let restarted = file.metadata()?.len() < *pos;
    if restarted {
        *pos = 0;
    }
    file.seek(SeekFrom::Start(*pos))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let Some(last_newline) = buf.iter().rposition(|b| *b == b'\n') else {
        return Ok((Vec::new(), restarted));
    };
    let complete = &buf[..=last_newline];
    *pos += complete.len() as u64;
    let lines = String::from_utf8_lossy(complete)
        .lines()
        // A game that keeps writing at its own offset after we emptied the log at
        // start leaves a run of NUL bytes in front of its next line.
        .map(|l| l.trim_start_matches('\0').trim_end_matches('\r').to_string())
        .collect();
    Ok((lines, restarted))
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
    // Set after the game restarted: until its handshake, commands would be wiped.
    // At adapter start this is unknown, so a game already in its round (it sends
    // data without a new handshake) opens the sync file as well.
    let mut awaiting_handshake = false;
    let mut was_running = game_running();
    while !s.stopped() {
        let mut running = game_running();
        // The shrinking log misses a restart when the new game writes past the old
        // position within a second. Emptying the log once the game is gone avoids
        // that; a closed game no longer writes to it.
        if was_running && !running {
            match File::create(&log) {
                Ok(_) => {
                    pos = 0;
                    awaiting_handshake = true;
                    s.set_round(false);
                }
                Err(_) => running = true, // the old process may still hold it, try again next second
            }
        }
        was_running = running;

        match read_new_lines(&log, &mut pos) {
            Ok((lines, restarted)) => take_lines(&s, &mut batch, &lines, restarted, &mut awaiting_handshake),
            Err(e) => s.fail(format!("Logdatei {}: {e}", log.display())),
        }
        if batch.has_data() {
            let url = format!("{}?action=sync", s.api);
            let payload = batch.payload(&s.token, &s.mod_id);
            let rejection = match post(&s.http, &url, &payload) {
                // Only drop what the server confirmed; anything else goes out again next round.
                Ok(res) if res["ok"] == true => {
                    batch = Batch::default();
                    s.report(format!("Verbunden, zuletzt gesendet {}", chrono::Local::now().format("%H:%M:%S")));
                    None
                }
                Ok(res) => Some(res.to_string()),
                Err(e) if e.rejected => Some(e.message),
                // Network trouble is no verdict on the data; keep retrying.
                Err(e) => {
                    s.fail(format!("Sync fehlgeschlagen: {e}"));
                    None
                }
            };
            if let Some(answer) = rejection {
                if batch.rejected() {
                    let mut dropped = payload.to_string();
                    dropped.truncate(dropped.floor_char_boundary(4000));
                    s.fail(format!(
                        "Sync {MAX_REJECTIONS}-mal abgelehnt, Daten verworfen. Antwort: {answer}. Daten: {dropped}"
                    ));
                    batch = Batch::default();
                } else {
                    s.fail(format!("Sync abgelehnt: {answer}"));
                }
            }
        }
        s.pause(Duration::from_secs(1));
    }
}

/// Feeds new log lines into the batch and decides when the game reads the sync file:
/// after a restart only its handshake opens the round, otherwise any mod line does.
fn take_lines(s: &Session, batch: &mut Batch, lines: &[String], restarted: bool, awaiting_handshake: &mut bool) {
    if restarted {
        *awaiting_handshake = true;
        s.set_round(false);
    }
    for line in lines {
        if let Some(handshake) = batch.process_line(line) {
            match s.handshake(&handshake) {
                Ok(()) => *awaiting_handshake = false,
                Err(e) => s.fail(format!("Sync-Datei: {e}")),
            }
        } else if line.starts_with(MARKER) && !*awaiting_handshake && !s.round_active() {
            s.set_round(true);
        }
    }
}

/// Fetches pending commands every five seconds and hands them to the game.
pub fn run_command_poller(s: Session) {
    let mut unacked = Vec::new();
    while !s.stopped() {
        if !game_running() {
            s.set_round(false);
        }
        if let Err(e) = poll_commands(&s, &mut unacked) {
            s.fail(e);
        }
        s.pause(Duration::from_secs(5));
    }
}

/// Whether Em4.exe runs. A closed game leaves its log untouched, so the log alone can't
/// tell that the round is over; only the next game's handshake would, and that one
/// wipes whatever was delivered in between.
fn game_running() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    // SAFETY: plain Win32 calls on a snapshot handle we own and close; entry.dwSize is
    // set as the API requires.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return true; // can't tell, so don't hold commands back
        }
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = false;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let len = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case("Em4.exe") {
                found = true;
                break;
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        found
    }
}

/// `unacked` holds ids already in the sync file whose ack failed. The server sends them
/// again until the ack gets through, and writing them twice would run them twice.
fn poll_commands(s: &Session, unacked: &mut Vec<String>) -> Result<(), String> {
    // Outside a round the commands stay on the server until the game is there to read them.
    if !s.round_active() {
        return Ok(());
    }
    let url = format!("{}?action=commands_pending", s.api);
    let res = reply(&url, s.http.get(&url).query("session_token", &s.token).call())
        .map_err(|e| format!("Befehle abholen: {e}"))?;
    let mut lines = Vec::new();
    let mut ids = Vec::new();
    for cmd in res["commands"].as_array().into_iter().flatten() {
        let id = text(&cmd["id"]);
        if unacked.contains(&id) {
            continue;
        }
        match command_line(cmd) {
            Some(line) => {
                lines.push(line);
                ids.push(id);
            }
            None => log_error(&format!("Befehl nicht verstanden: {cmd}")),
        }
    }
    // Acknowledge only what reached the file; the rest comes again next poll. The round
    // may have ended while the request ran, so `deliver` checks once more under the lock.
    if !lines.is_empty() {
        if !s.deliver(&lines).map_err(|e| format!("Sync-Datei: {e}"))? {
            return Ok(());
        }
        unacked.extend(ids);
    }
    if unacked.is_empty() {
        return Ok(());
    }
    post(
        &s.http,
        &format!("{}?action=commands_ack", s.api),
        &json!({ "session_token": s.token, "command_ids": unacked }),
    )
    .map_err(|e| format!("Befehle bestätigen: {e}"))?;
    unacked.clear();
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

        let mut merged = Batch::default();
        merged.process_line(r#"!LSTcmd::v{"game_vehicle_id":1,"x":1,"s":2}"#);
        merged.process_line(r#"!LSTcmd::v{"game_vehicle_id":"1","x":5}"#);
        merged.process_line(r#"!LSTcmd::v{"game_vehicle_id":2}"#);
        merged.process_line(r#"!LSTcmd::v{"x":3}"#);
        assert_eq!(
            merged.payload("t", "m")["vehicles"],
            json!([{ "game_vehicle_id": "1", "x": 5, "s": 2 }, { "game_vehicle_id": 2 }, { "x": 3 }])
        );
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
        assert_eq!(read_new_lines(&p, &mut pos).unwrap(), (vec!["eins".to_string()], false));
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"ei\r\n").unwrap();
        assert_eq!(read_new_lines(&p, &mut pos).unwrap(), (vec!["zwei".to_string()], false));
        assert_eq!(read_new_lines(&p, &mut pos).unwrap(), (vec![], false));
        std::fs::write(&p, "neu\n").unwrap();
        assert_eq!(read_new_lines(&p, &mut pos).unwrap(), (vec!["neu".to_string()], true));
        std::fs::write(&p, b"neu\n\0\0\0!LSTcmd::v{}\n").unwrap();
        assert_eq!(read_new_lines(&p, &mut pos).unwrap(), (vec!["!LSTcmd::v{}".to_string()], false));
    }

    #[test]
    fn batch_is_dropped_after_repeated_rejections() {
        let mut b = Batch::default();
        for _ in 1..MAX_REJECTIONS {
            assert!(!b.rejected());
        }
        assert!(b.rejected());
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder().http_status_as_error(false).build().into()
    }

    fn session(sync_file: &Path, api: &str) -> Session {
        Session {
            api: api.into(),
            game_dir: PathBuf::new(),
            sync_file: sync_file.into(),
            token: "tok".into(),
            mod_id: String::new(),
            http: agent(),
            stop: Arc::new(AtomicBool::new(false)),
            sync: Arc::new(Mutex::new(false)),
            status: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Answers one request per (status, body) in order and returns the raw requests.
    fn serve(responses: Vec<(&'static str, &'static str)>) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api.php", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut conn, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = conn.read(&mut buf).unwrap();
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let Some(head_end) = text.find("\r\n\r\n") else { continue };
                    let length = text
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse().ok()))
                        .unwrap_or(0);
                    if n == 0 || request.len() >= head_end + 4 + length {
                        break;
                    }
                }
                write!(conn, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    .unwrap();
                requests.push(String::from_utf8_lossy(&request).into_owned());
            }
            requests
        });
        (url, server)
    }

    #[test]
    fn server_errors_count_as_rejections() {
        let (url, server) = serve(vec![
            ("400 Bad Request", r#"{"error":"PIN fehlt"}"#),
            ("200 OK", "<b>Warning</b>"),
            ("500 Internal Server Error", ""),
            ("200 OK", r#"{"ok":true}"#),
        ]);
        let e = post(&agent(), &url, &json!({})).unwrap_err();
        assert!(e.rejected && e.message.contains("PIN fehlt"), "{e}");
        assert!(post(&agent(), &url, &json!({})).unwrap_err().rejected);
        // an agent that turns the status into an error still counts it as the server's answer
        assert!(post(&ureq::Agent::new_with_defaults(), &url, &json!({})).unwrap_err().rejected);
        assert_eq!(post(&agent(), &url, &json!({})).unwrap(), json!({ "ok": true }));
        server.join().unwrap();

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(!post(&agent(), &format!("http://{closed}/"), &json!({})).unwrap_err().rejected);
    }

    #[test]
    fn a_failed_ack_does_not_write_commands_twice() {
        let pending = r#"{"commands":[{"id":7,"type":"unassign","payload":"{\"game_vehicle_id\":2}"}]}"#;
        let (url, server) = serve(vec![
            ("200 OK", pending),
            ("500 Internal Server Error", ""),
            ("200 OK", pending),
            ("200 OK", r#"{"ok":true}"#),
        ]);
        let sync_file = temp("ack.txt");
        let s = session(&sync_file, &url);
        s.handshake("1|x").unwrap();
        let mut unacked = Vec::new();
        assert!(poll_commands(&s, &mut unacked).is_err());
        assert_eq!(unacked, ["7"]);
        poll_commands(&s, &mut unacked).unwrap();
        assert!(unacked.is_empty());
        assert_eq!(std::fs::read_to_string(&sync_file).unwrap(), "42|1|x\r\n204|2|\r\n+\r\n");
        let requests = server.join().unwrap();
        let ack: Value = serde_json::from_str(requests[3].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(ack["command_ids"], json!(["7"]));
    }

    #[test]
    fn after_a_restart_only_the_handshake_opens_the_round() {
        let sync_file = temp("take.txt");
        let s = session(&sync_file, "");
        let mut batch = Batch::default();
        let mut awaiting = false;
        let data = [r#"!LSTcmd::v{"game_vehicle_id":1}"#.to_string()];
        // adapter started while a round runs: the game sends data without a new handshake
        take_lines(&s, &mut batch, &data, false, &mut awaiting);
        assert!(s.round_active());
        take_lines(&s, &mut batch, &data, true, &mut awaiting);
        assert!(!s.round_active());
        take_lines(&s, &mut batch, &["!LSTcmd::i5|y".into()], false, &mut awaiting);
        assert!(s.round_active());
        assert_eq!(std::fs::read_to_string(&sync_file).unwrap(), "42|5|y\r\n+\r\n");
    }

    #[test]
    fn commands_only_reach_the_file_during_a_round() {
        let sync_file = temp("round.txt");
        let s = session(&sync_file, "");
        assert!(!s.deliver(&["204|1|".into()]).unwrap());
        assert!(!sync_file.exists());

        s.handshake("7|x").unwrap();
        assert!(s.deliver(&["204|1|".into()]).unwrap());
        assert_eq!(std::fs::read_to_string(&sync_file).unwrap(), "42|7|x\r\n204|1|\r\n+\r\n");

        s.set_round(false);
        assert!(!s.deliver(&["204|2|".into()]).unwrap());
        assert_eq!(std::fs::read_to_string(&sync_file).unwrap(), "42|7|x\r\n204|1|\r\n+\r\n");
    }
}
