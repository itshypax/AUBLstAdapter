use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

pub const LOG_FILE: &str = "logfile.txt";
const MARKER: &str = "!LSTcmd::";
const PLAYER_MARKER: &str = "advertising game info: players=";
const LOG_LINES: usize = 1000;
/// The mod repeats its feature line about every ten seconds; older means it stopped.
const FEATURES_FRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Level {
    Info,
    Good,
    Warn,
    Bad,
}

impl Level {
    fn wire(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Good => "good",
            Level::Warn => "warn",
            Level::Bad => "bad",
        }
    }
}

pub struct Entry {
    pub time: String,
    pub level: Level,
    pub text: String,
}

/// Entries waiting for `adapter_log`. A server that stays down must not grow it without end.
const OUTBOX_LIMIT: usize = 500;
/// The server takes at most this many entries per request.
const OUTBOX_BATCH: usize = 50;

/// What a log line may carry off this machine: the user profile becomes "~" (it holds
/// the Windows account name) and IPv4 addresses become "<IP>".
pub fn scrub(text: &str) -> String {
    let mut out = text.to_string();
    if let Ok(profile) = std::env::var("USERPROFILE")
        && !profile.is_empty()
    {
        out = replace_ignore_ascii_case(&out, &profile, "~");
    }
    // Profiles of other accounts, and paths written with forward slashes.
    for sep in ['\\', '/'] {
        let needle = format!(":{sep}users{sep}");
        while let Some(at) = out.to_ascii_lowercase().find(&needle) {
            let start = if at > 0 && out.as_bytes()[at - 1].is_ascii_alphabetic() { at - 1 } else { at };
            let name_start = at + needle.len();
            let end = out[name_start..].find(['\\', '/', '"']).map_or(out.len(), |i| name_start + i);
            out.replace_range(start..end, "~");
        }
    }
    scrub_ipv4(&out)
}

fn replace_ignore_ascii_case(text: &str, from: &str, to: &str) -> String {
    let (lower, from) = (text.to_ascii_lowercase(), from.to_ascii_lowercase());
    let mut out = String::new();
    let mut rest = 0;
    for (at, _) in lower.match_indices(&from) {
        out.push_str(&text[rest..at]);
        out.push_str(to);
        rest = at + from.len();
    }
    out.push_str(&text[rest..]);
    out
}

fn scrub_ipv4(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || !(bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.');
        if boundary && let Some(len) = ipv4_len(&bytes[i..]) {
            out.push_str("<IP>");
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Length of four dot-separated groups of 1 to 3 digits at the start, if that is what's there.
fn ipv4_len(b: &[u8]) -> Option<usize> {
    let mut i = 0;
    for group in 0..4 {
        let digits = b[i..].iter().take(4).take_while(|c| c.is_ascii_digit()).count();
        if !(1..=3).contains(&digits) {
            return None;
        }
        i += digits;
        if group < 3 {
            if b.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
    }
    // "1.2.3.4.5" is no address
    (!(b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit))).then_some(i)
}

pub fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// What the window shows. The worker threads write it, the window reads it every frame.
#[derive(Default)]
pub struct Shared {
    pub log: VecDeque<Entry>,
    pub game_log: VecDeque<String>,
    pub last_sent: Option<String>,
    pub server_down_since: Option<String>,
    pub rejecting: bool,
    pub round_since: Option<String>,
    pub delivered: usize,
    pub unacked: usize,
    /// The window log as it goes to the server, already scrubbed. Emptied by `reset`,
    /// so nothing from before the session reaches it.
    pub outbox: VecDeque<Value>,
    /// The mod's last feature line and when it came.
    pub game_features: Option<(Value, Instant)>,
}

impl Shared {
    pub fn push(&mut self, level: Level, text: String) {
        // A failure that repeats every second would bury everything else.
        if self.log.back().is_some_and(|e| e.text == text) {
            return;
        }
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        if self.outbox.len() == OUTBOX_LIMIT {
            self.outbox.pop_front();
        }
        self.outbox.push_back(json!({
            "time": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
            "level": level.wire(),
            "text": scrub(&text),
        }));
        self.log.push_back(Entry { time: now(), level, text });
    }

    /// Starts the status over for a new session; both logs stay readable.
    pub fn reset(&mut self) {
        *self = Shared {
            log: std::mem::take(&mut self.log),
            game_log: std::mem::take(&mut self.game_log),
            ..Default::default()
        };
    }
}

/// Everything both worker threads need once a session token exists.
#[derive(Clone)]
pub struct Session {
    pub api: Api,
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
    pub shared: Arc<Mutex<Shared>>,
}

impl Session {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn note(&self, level: Level, text: impl Into<String>) {
        self.shared.lock().unwrap().push(level, text.into());
    }

    fn fail(&self, text: String) {
        fail(&self.shared, text);
    }

    /// Status changes from a session that was already disconnected would overwrite
    /// the status of the next one, since a request can still be running for 10 s.
    fn update(&self, f: impl FnOnce(&mut Shared)) {
        if !self.stopped() {
            f(&mut self.shared.lock().unwrap());
        }
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
        let mut round = self.sync.lock().unwrap();
        if *round != active {
            *round = active;
            self.update(|sh| sh.round_since = active.then(now));
        }
    }

    /// Answers the game's handshake. From here on the game reads the sync file from the start.
    fn handshake(&self, text: &str) -> io::Result<()> {
        let mut active = self.sync.lock().unwrap();
        write_sync_file(&self.sync_file, &[format!("42|{text}")], true)?;
        *active = true;
        self.update(|sh| sh.round_since = Some(now()));
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

fn post(http: &ureq::Agent, url: &str, body: &Value) -> Result<Value, HttpError> {
    reply(url, http.post(url).send_json(body))
}

/// Where requests go. AUBLst from adapter API v1 on takes fixed paths with the session
/// code in a header, so it lands in no access log; other servers only know
/// `api.php?action=…` with the code in the body or URL.
#[derive(Clone, Debug, PartialEq)]
pub enum Api {
    V1(String),
    Legacy(String),
}

/// api.php actions and their v1 paths, as in AUBLst's backend/adapter-api.yaml.
const ROUTES: [(&str, &str); 5] = [
    ("session_create", "/sessions"),
    ("sync", "/session/sync"),
    ("commands_pending", "/session/commands"),
    ("commands_ack", "/session/commands/ack"),
    ("adapter_log", "/session/log"),
];

impl Api {
    /// Asks once per session. A server that can't tell stays on api.php, which every
    /// version understands.
    pub fn detect(http: &ureq::Agent, api_php: &str) -> Api {
        let url = format!("{api_php}?action=capabilities");
        let v1 = reply(&url, http.get(&url).call()).is_ok_and(|c| c["adapter_api"].as_u64().is_some_and(|v| v >= 1));
        match api_php.strip_suffix("api.php") {
            Some(base) if v1 => Api::V1(format!("{base}adapter.php/v1")),
            _ => Api::Legacy(api_php.to_string()),
        }
    }

    /// A GET without `body`. An empty token sends none, as for `session_create`.
    pub fn call(&self, http: &ureq::Agent, action: &str, token: &str, body: Option<&Value>) -> Result<Value, HttpError> {
        match self {
            Api::V1(base) => {
                let path = ROUTES.iter().find(|(a, _)| *a == action).map(|(_, p)| p).expect("every action has a route");
                let url = format!("{base}{path}");
                let auth = (!token.is_empty()).then(|| format!("Bearer {token}"));
                match body {
                    Some(body) => {
                        let req = http.post(&url);
                        let req = match &auth {
                            Some(a) => req.header("Authorization", a),
                            None => req,
                        };
                        reply(&url, req.send_json(body))
                    }
                    None => {
                        let req = http.get(&url);
                        let req = match &auth {
                            Some(a) => req.header("Authorization", a),
                            None => req,
                        };
                        reply(&url, req.call())
                    }
                }
            }
            Api::Legacy(api) => {
                let url = format!("{api}?action={action}");
                match body {
                    Some(body) => {
                        let mut body = body.clone();
                        if !token.is_empty() {
                            body["session_token"] = json!(token);
                        }
                        post(http, &url, &body)
                    }
                    None => reply(&url, http.get(&url).query("session_token", token).call()),
                }
            }
        }
    }
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

pub fn fail(shared: &Mutex<Shared>, text: String) {
    log_error(&text);
    shared.lock().unwrap().push(Level::Bad, text);
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

/// `text` for one field of a game line: a `|` or line break would split the line, and the
/// mod would misread every command after it.
fn field_text(v: &Value) -> String {
    text(v).replace(['|', '\r', '\n'], " ")
}

fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// Turns one pending command from AUBLst into the line the game reads from the sync
/// file. None for unknown types and for commands missing a field the line needs;
/// those stay unacknowledged.
pub fn command_line(cmd: &Value) -> Option<String> {
    let payload = payload(cmd)?;
    let field = |key: &str| payload.get(key).map(field_text);
    let event_id = present(payload.get("event_game_id"))
        .or(present(payload.get("event_id")))
        .map_or("-1".to_string(), field_text);
    let target = || {
        let t = payload.get("target")?;
        Some((field_text(t.get("x")?), field_text(t.get("y")?)))
    };

    Some(match cmd["type"].as_str()? {
        "assign" => {
            let player = present(payload.get("assign_to_player_id"))
                .map(field_text)
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
        // Codes and fields come from AUBLst unchanged, so a submod gets new commands
        // without a new adapter.
        "game_command" => {
            let code = payload.get("code")?.as_i64()?;
            let fields: String = payload.get("fields")?.as_array()?.iter().map(|f| field_text(f) + "|").collect();
            format!("{code}|{fields}")
        }
        _ => return None,
    })
}

/// AUBLst sends the payload as a JSON string; an object is taken as well.
fn payload(cmd: &Value) -> Option<Value> {
    match &cmd["payload"] {
        Value::String(s) => serde_json::from_str(s).ok(),
        v @ Value::Object(_) => Some(v.clone()),
        _ => None,
    }
}

/// The command as a person reads it in the window's log.
fn describe(cmd: &Value) -> String {
    let p = payload(cmd).unwrap_or_default();
    let f = |key: &str| p.get(key).map(text).unwrap_or_default();
    match cmd["type"].as_str().unwrap_or_default() {
        "assign" => match present(p.get("event_game_id")).or(present(p.get("event_id"))) {
            Some(event) => format!("Fahrzeug {} zu Einsatz {}", f("game_vehicle_id"), text(event)),
            None => format!("Fahrzeug {} alarmiert", f("game_vehicle_id")),
        },
        "unassign" => format!("Fahrzeug {} abgezogen", f("game_vehicle_id")),
        "event_create" => format!("neuer Einsatz „{}“", f("name")),
        "event_delete" => format!("Einsatz {} gelöscht", f("event_id")),
        "message_create" => format!("Meldung „{}“", f("short_message")),
        "message_revoke" => format!("Meldung „{}“ zurückgezogen", f("short_message")),
        "game_command" => format!("Befehl {}", f("code")),
        other => other.to_string(),
    }
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
    pub features: Option<Value>,
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
            'f' => self.features = Some(Value::Object(data)),
            _ => {}
        }
        None
    }

    /// Players and time only ride along; on their own they don't trigger a sync.
    pub fn has_data(&self) -> bool {
        !(self.vehicles.is_empty() && self.hospitals.is_empty() && self.events.is_empty() && self.messages.is_empty())
    }

    /// "42 Fahrzeuge, 6 Kliniken" for the window's log.
    fn summary(&self) -> String {
        [
            (self.vehicles.len(), "Fahrzeug", "Fahrzeuge"),
            (self.hospitals.len(), "Klinik", "Kliniken"),
            (self.events.len(), "Einsatz", "Einsätze"),
            (self.messages.len(), "Meldung", "Meldungen"),
        ]
        .iter()
        .filter(|(n, ..)| *n > 0)
        .map(|(n, one, many)| format!("{n} {}", if *n == 1 { one } else { many }))
        .collect::<Vec<_>>()
        .join(", ")
    }

    pub fn payload(&self, mod_id: &str) -> Value {
        let mut data = json!({ "mod_id": mod_id });
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
    // Only the first sync after a start or a failure goes into the window's log.
    let mut sending = false;
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
                    s.note(Level::Info, "Spiel beendet");
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
            let payload = batch.payload(&s.mod_id);
            let rejection = match s.api.call(&s.http, "sync", &s.token, Some(&payload)) {
                // Only drop what the server confirmed; anything else goes out again next round.
                Ok(res) if res["ok"] == true => {
                    if !sending {
                        s.note(Level::Good, format!("Gesendet: {}", batch.summary()));
                    }
                    sending = true;
                    s.update(|sh| {
                        sh.last_sent = Some(now());
                        sh.server_down_since = None;
                        sh.rejecting = false;
                    });
                    batch = Batch::default();
                    None
                }
                Ok(res) => Some(res.to_string()),
                Err(e) if e.rejected => Some(e.message),
                // Network trouble is no verdict on the data; keep retrying.
                Err(e) => {
                    sending = false;
                    s.update(|sh| {
                        sh.server_down_since.get_or_insert_with(now);
                    });
                    s.fail(format!("Server antwortet nicht, neuer Versuch: {e}"));
                    None
                }
            };
            if let Some(answer) = rejection {
                sending = false;
                s.update(|sh| {
                    sh.rejecting = true;
                    sh.server_down_since = None;
                });
                if batch.rejected() {
                    let mut dropped = payload.to_string();
                    dropped.truncate(dropped.floor_char_boundary(4000));
                    // The data holds player names and raw game reports, so it stays on this machine.
                    let text = format!("Sync {MAX_REJECTIONS}-mal abgelehnt, Daten verworfen. Antwort: {answer}");
                    log_error(&format!("{text}. Daten: {dropped}"));
                    s.note(Level::Bad, format!("{text}. Die Daten stehen im error.log."));
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
        s.note(Level::Info, "Spiel neu gestartet, warte auf die Mod");
    }
    s.update(|sh| {
        for line in lines {
            if sh.game_log.len() == LOG_LINES {
                sh.game_log.pop_front();
            }
            sh.game_log.push_back(line.clone());
        }
    });
    for line in lines {
        let handshake = batch.process_line(line);
        if let Some(features) = batch.features.take() {
            s.update(|sh| sh.game_features = Some((features, Instant::now())));
        }
        if let Some(handshake) = handshake {
            match s.handshake(&handshake) {
                Ok(()) => {
                    *awaiting_handshake = false;
                    s.note(Level::Good, "Mod hat die Runde gestartet");
                }
                Err(e) => s.fail(format!("Sync-Datei: {e}")),
            }
        } else if line.starts_with(MARKER) && !*awaiting_handshake && !s.round_active() {
            s.set_round(true);
            s.note(Level::Good, "Mod ist in einer laufenden Runde");
        }
    }
}

/// Fetches pending commands and hands them to the game, as often as the server asks
/// (see `next_poll`). After a failed request it waits five seconds, so a server in
/// trouble isn't asked every second.
pub fn run_command_poller(s: Session) {
    let mut unacked = Vec::new();
    while !s.stopped() {
        if !game_running() {
            s.set_round(false);
        }
        let wait = poll_commands(&s, &mut unacked).unwrap_or_else(|e| {
            s.fail(e);
            Duration::from_secs(5)
        });
        s.pause(wait);
    }
}

/// Sends the window log to the server every five seconds, for the operator's view.
pub fn run_log_uploader(s: Session) {
    while !s.stopped() {
        s.pause(Duration::from_secs(5));
        send_log(&s);
    }
}

/// Never writes to the window log itself: a failure would queue another entry to send.
/// Entries stay queued while the server is unreachable; a server that refuses them
/// (an AUBLst without `adapter_log`) won't take them later either, so they are dropped.
fn send_log(s: &Session) {
    // Goes along every time, even without entries: the dispatch shows from it whether
    // a command can reach the game right now.
    let status = adapter_status(&s.shared.lock().unwrap(), game_running());
    let mut first = true;
    loop {
        let chunk: Vec<Value> = {
            let mut sh = s.shared.lock().unwrap();
            let n = sh.outbox.len().min(OUTBOX_BATCH);
            sh.outbox.drain(..n).collect()
        };
        if (chunk.is_empty() && !first) || s.stopped() {
            return;
        }
        first = false;
        match s.api.call(&s.http, "adapter_log", &s.token, Some(&json!({ "entries": chunk, "status": status }))) {
            Ok(_) => {}
            Err(e) if e.rejected => log_error(&format!("Log senden abgelehnt: {e}")),
            Err(_) => {
                s.update(|sh| {
                    for entry in chunk.into_iter().rev() {
                        sh.outbox.push_front(entry);
                    }
                    sh.outbox.truncate(OUTBOX_LIMIT);
                });
                return;
            }
        }
    }
}

/// Whether the game (Em4.exe or Em4Deluxe.exe) runs. A closed game leaves its log untouched, so the log alone can't
/// tell that the round is over; only the next game's handshake would, and that one
/// wipes whatever was delivered in between.
/// `game_command` tells AUBLst this adapter passes game commands through; it only uses
/// them for codes the mod lists in `game_features`.
fn adapter_status(sh: &Shared, game: bool) -> Value {
    let mut status = json!({
        "game": game,
        "round": sh.round_since.is_some(),
        "version": env!("CARGO_PKG_VERSION"),
        "game_command": 1,
    });
    if let Some((features, seen)) = &sh.game_features
        && seen.elapsed() <= FEATURES_FRESH
    {
        status["game_features"] = features.clone();
    }
    status
}

pub fn game_running() -> bool {
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
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if crate::setup::GAME_EXES.iter().any(|exe| name.eq_ignore_ascii_case(exe)) {
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
/// When to ask again: AUBLst sends `poll_ms`, 1 s while a dispatch has the session
/// open and longer when nobody could send commands. Older servers send nothing.
fn next_poll(res: &Value) -> Duration {
    Duration::from_millis(res["poll_ms"].as_u64().unwrap_or(1000).clamp(1000, 10_000))
}

fn poll_commands(s: &Session, unacked: &mut Vec<String>) -> Result<Duration, String> {
    // Outside a round the commands stay on the server until the game is there to read them.
    if !s.round_active() {
        return Ok(Duration::from_secs(1));
    }
    let res = match s.api.call(&s.http, "commands_pending", &s.token, None) {
        Ok(res) => {
            s.update(|sh| sh.server_down_since = None);
            res
        }
        Err(e) => {
            if !e.rejected {
                s.update(|sh| {
                    sh.server_down_since.get_or_insert_with(now);
                });
            }
            return Err(format!("Befehle abholen: {e}"));
        }
    };
    let wait = next_poll(&res);
    let mut lines = Vec::new();
    let mut ids = Vec::new();
    let mut notes = Vec::new();
    for cmd in res["commands"].as_array().into_iter().flatten() {
        let id = text(&cmd["id"]);
        if unacked.contains(&id) {
            continue;
        }
        match command_line(cmd) {
            Some(line) => {
                lines.push(line);
                ids.push(id);
                notes.push(describe(cmd));
            }
            None => {
                log_error(&format!("Befehl nicht verstanden: {cmd}"));
                s.note(Level::Warn, format!("Befehl nicht verstanden: {}", describe(cmd)));
            }
        }
    }
    // Acknowledge only what reached the file; the rest comes again next poll. The round
    // may have ended while the request ran, so `deliver` checks once more under the lock.
    if !lines.is_empty() {
        if !s.deliver(&lines).map_err(|e| format!("Sync-Datei: {e}"))? {
            return Ok(wait);
        }
        for note in notes {
            s.note(Level::Info, format!("Ans Spiel: {note}"));
        }
        s.update(|sh| sh.delivered += lines.len());
        unacked.extend(ids);
    }
    if unacked.is_empty() {
        return Ok(wait);
    }
    let acked = s.api.call(&s.http, "commands_ack", &s.token, Some(&json!({ "command_ids": unacked })));
    if acked.is_ok() {
        unacked.clear();
    }
    s.update(|sh| sh.unacked = unacked.len());
    acked.map_err(|e| format!("Server hat {} Befehle nicht bestätigt: {e}", unacked.len()))?;
    Ok(wait)
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
    fn game_commands_pass_through() {
        assert_eq!(command_line(&cmd("game_command", json!({ "code": 505, "fields": ["1_RTW_A", 2] }))).unwrap(), "505|1_RTW_A|2|");
        assert_eq!(command_line(&cmd("game_command", json!({ "code": 501, "fields": ["505,506"] }))).unwrap(), "501|505,506|");
        assert_eq!(command_line(&cmd("game_command", json!({ "code": 501, "fields": [""] }))).unwrap(), "501||");
        assert_eq!(command_line(&cmd("game_command", json!({ "code": "x", "fields": [] }))), None);
        assert_eq!(command_line(&cmd("game_command", json!({ "fields": [] }))), None);
    }

    #[test]
    fn feature_line_is_kept_for_the_status() {
        let mut b = Batch::default();
        assert_eq!(b.process_line(r#"!LSTcmd::f{"commands":[501,505]}"#), None);
        assert_eq!(b.features.take(), Some(json!({ "commands": [501, 505] })));
    }

    #[test]
    fn status_reports_features_only_while_fresh() {
        let mut sh = Shared::default();
        assert_eq!(adapter_status(&sh, true)["game_command"], 1);
        assert!(adapter_status(&sh, true).get("game_features").is_none());
        sh.game_features = Some((json!({ "commands": [501] }), Instant::now()));
        assert_eq!(adapter_status(&sh, true)["game_features"], json!({ "commands": [501] }));
        sh.game_features = Some((json!({ "commands": [501] }), Instant::now() - Duration::from_secs(31)));
        assert!(adapter_status(&sh, true).get("game_features").is_none());
    }

    #[test]
    fn separators_never_split_a_line() {
        let line = command_line(&cmd("event_create", json!({ "event_id": 3, "name": "Brand|A\nB", "target": { "x": 1, "y": 2 } }))).unwrap();
        assert_eq!(line, "110|3|1|2|Brand A B|");
        assert_eq!(command_line(&cmd("game_command", json!({ "code": 506, "fields": ["1|RTW\r"] }))).unwrap(), "506|1 RTW |");
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
            b.payload("mod"),
            json!({ "mod_id": "mod", "vehicles": [{ "id": 1 }],
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
            merged.payload("m")["vehicles"],
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
            api: Api::Legacy(api.into()),
            game_dir: PathBuf::new(),
            sync_file: sync_file.into(),
            token: "tok".into(),
            mod_id: String::new(),
            http: agent(),
            stop: Arc::new(AtomicBool::new(false)),
            sync: Arc::new(Mutex::new(false)),
            shared: Arc::new(Mutex::new(Shared::default())),
        }
    }

    fn log_texts(s: &Session) -> Vec<String> {
        s.shared.lock().unwrap().log.iter().map(|e| e.text.clone()).collect()
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
    fn the_server_sets_the_poll_interval_within_bounds() {
        let wait = |v: Value| next_poll(&v).as_millis();
        assert_eq!(wait(json!({"commands": []})), 1000);
        assert_eq!(wait(json!({"poll_ms": 5000})), 5000);
        assert_eq!(wait(json!({"poll_ms": 10})), 1000);
        assert_eq!(wait(json!({"poll_ms": 600000})), 10_000);
        assert_eq!(wait(json!({"poll_ms": "5000"})), 1000);
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
        assert_eq!(s.shared.lock().unwrap().unacked, 1);
        poll_commands(&s, &mut unacked).unwrap();
        assert!(unacked.is_empty());
        assert_eq!(s.shared.lock().unwrap().unacked, 0);
        assert_eq!(s.shared.lock().unwrap().delivered, 1);
        assert_eq!(log_texts(&s).iter().filter(|t| *t == "Ans Spiel: Fahrzeug 2 abgezogen").count(), 1);
        assert_eq!(std::fs::read_to_string(&sync_file).unwrap(), "42|1|x\r\n204|2|\r\n+\r\n");
        let requests = server.join().unwrap();
        let ack: Value = serde_json::from_str(requests[3].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(ack["command_ids"], json!(["7"]));
    }

    #[test]
    fn api_v1_carries_the_code_in_the_header_and_old_servers_keep_api_php() {
        let (url, server) = serve(vec![
            ("200 OK", r#"{"api_version":1,"adapter_api":1}"#),
            ("200 OK", r#"{"commands":[]}"#),
            ("200 OK", r#"{"ok":true}"#),
            ("200 OK", r#"{"ok":true,"session_token":"ab12"}"#),
            ("200 OK", r#"{"api_version":1}"#),
        ]);
        let api = Api::detect(&agent(), &url);
        assert_eq!(api, Api::V1(url.replace("api.php", "adapter.php/v1")));
        api.call(&agent(), "commands_pending", "tok", None).unwrap();
        api.call(&agent(), "commands_ack", "tok", Some(&json!({ "command_ids": ["7"] }))).unwrap();
        api.call(&agent(), "session_create", "", Some(&json!({ "mod_id": "AUBMP" }))).unwrap();
        assert_eq!(Api::detect(&agent(), &url), Api::Legacy(url.clone()));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api.php?action=capabilities "));
        assert!(requests[1].starts_with("GET /adapter.php/v1/session/commands "));
        assert!(requests[2].starts_with("POST /adapter.php/v1/session/commands/ack "));
        assert!(requests[3].starts_with("POST /adapter.php/v1/sessions "));
        for r in &requests[1..3] {
            assert!(r.to_ascii_lowercase().contains("\r\nauthorization: bearer tok\r\n"), "{r}");
            assert!(!r.contains("session_token"), "{r}");
        }
        assert!(!requests[3].to_ascii_lowercase().contains("authorization"));

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let unreachable = format!("http://{closed}/api.php");
        assert_eq!(Api::detect(&agent(), &unreachable), Api::Legacy(unreachable.clone()));
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
        assert!(s.shared.lock().unwrap().round_since.is_some());
        assert_eq!(std::fs::read_to_string(&sync_file).unwrap(), "42|5|y\r\n+\r\n");
        assert_eq!(
            log_texts(&s),
            ["Mod ist in einer laufenden Runde", "Spiel neu gestartet, warte auf die Mod", "Mod hat die Runde gestartet"]
        );
        assert_eq!(s.shared.lock().unwrap().game_log.len(), 3);
        assert_eq!(batch.summary(), "1 Fahrzeug");
    }

    #[test]
    fn the_window_log_skips_repeats_and_stays_bounded() {
        let mut sh = Shared::default();
        sh.push(Level::Bad, "Server antwortet nicht".into());
        sh.push(Level::Bad, "Server antwortet nicht".into());
        assert_eq!(sh.log.len(), 1);
        for i in 0..LOG_LINES + 5 {
            sh.push(Level::Info, i.to_string());
        }
        assert_eq!(sh.log.len(), LOG_LINES);
        assert_eq!(sh.log.back().unwrap().text, (LOG_LINES + 4).to_string());

        sh.delivered = 3;
        sh.reset();
        assert_eq!((sh.delivered, sh.log.len()), (0, LOG_LINES));
    }

    #[test]
    fn log_leaves_the_machine_without_profile_paths_or_addresses() {
        let profile = std::env::var("USERPROFILE").unwrap();
        assert_eq!(scrub(&format!("Logdatei {profile}\\Spiele\\logfile.txt fehlt")), "Logdatei ~\\Spiele\\logfile.txt fehlt");
        assert_eq!(
            scrub(r#"Sync-Datei D:\USERS\Max Muster\Em4\input.txt und "c:/Users/anna" gesperrt"#),
            r#"Sync-Datei ~\Em4\input.txt und "~" gesperrt"#
        );
        assert_eq!(scrub("ä:\\Users\\x"), "ä~");
        assert_eq!(
            scrub("http://192.168.0.12:8080/api.php: Verbindung zu 10.0.0.1 fehlgeschlagen"),
            "http://<IP>:8080/api.php: Verbindung zu <IP> fehlgeschlagen"
        );
        assert_eq!(scrub("Version 1.2.3.4.5, 1234.1.1.1, 3 Fahrzeuge"), "Version 1.2.3.4.5, 1234.1.1.1, 3 Fahrzeuge");
    }

    #[test]
    fn the_window_log_is_sent_and_kept_while_the_server_is_down() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let s = session(&temp("upload.txt"), &format!("http://{closed}/api.php"));
        for i in 0..OUTBOX_BATCH + 3 {
            s.note(Level::Warn, format!("Eintrag {i}"));
        }
        send_log(&s);
        assert_eq!(s.shared.lock().unwrap().outbox.len(), OUTBOX_BATCH + 3);

        let (url, server) = serve(vec![("200 OK", r#"{"ok":true}"#), ("400 Bad Request", r#"{"error":"Unknown action"}"#)]);
        let s = Session { api: Api::Legacy(url), ..s };
        send_log(&s);
        assert!(s.shared.lock().unwrap().outbox.is_empty(), "a refused batch is dropped as well");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api.php?action=adapter_log "));
        let body: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["session_token"], "tok");
        assert_eq!(body["entries"].as_array().unwrap().len(), OUTBOX_BATCH);
        assert_eq!((body["entries"][0]["level"].as_str(), body["entries"][0]["text"].as_str()), (Some("warn"), Some("Eintrag 0")));

        let mut sh = Shared::default();
        for i in 0..OUTBOX_LIMIT + 1 {
            sh.push(Level::Info, i.to_string());
        }
        assert_eq!(sh.outbox.len(), OUTBOX_LIMIT);
        sh.reset();
        assert!(sh.outbox.is_empty(), "nothing from before a session goes out");
    }

    #[test]
    fn commands_read_like_dispatch() {
        let assign = cmd("assign", json!({ "game_vehicle_id": "83-1", "event_game_id": 17 }));
        assert_eq!(describe(&assign), "Fahrzeug 83-1 zu Einsatz 17");
        assert_eq!(describe(&cmd("assign", json!({ "game_vehicle_id": 4 }))), "Fahrzeug 4 alarmiert");
        assert_eq!(describe(&cmd("event_create", json!({ "name": "Brand" }))), "neuer Einsatz „Brand“");
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
