//! Replaces the running exe with the one from the latest GitHub release.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::adapter::{Level, Shared, log_error};

const LATEST: &str = "https://api.github.com/repos/itshypax/AUBLstAdapter/releases/latest";

#[derive(Clone)]
pub struct Ready {
    pub version: String,
    pub file: PathBuf,
}

fn version(tag: &str) -> Option<Vec<u32>> {
    tag.trim_start_matches('v').split('.').map(|part| part.parse().ok()).collect()
}

fn newer(tag: &str, current: &str) -> bool {
    matches!((version(tag), version(current)), (Some(tag), Some(current)) if tag > current)
}

/// Runs once at startup: tidies up after the last update, then downloads a newer
/// release next to the exe. Failures only go to error.log, offline is normal.
pub fn fetch(shared: &Mutex<Shared>) -> Option<Ready> {
    let exe = std::env::current_exe().ok()?;
    if remove_old(&exe) {
        let text = format!("Auf Version {} aktualisiert", env!("CARGO_PKG_VERSION"));
        shared.lock().unwrap().push(Level::Good, text);
    }
    // A dev build would swap itself for the release.
    if cfg!(debug_assertions) {
        return None;
    }
    download(&exe, shared).map_err(|e| log_error(&format!("Update: {e}"))).ok().flatten()
}

fn download(exe: &Path, shared: &Mutex<Shared>) -> Result<Option<Ready>, String> {
    let http: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(300))).build().into();
    let release: Value = http
        .get(LATEST)
        .header("User-Agent", "AUBLstAdapter")
        .header("Accept", "application/vnd.github+json")
        .call()
        .and_then(|mut res| res.body_mut().read_json())
        .map_err(|e| format!("{LATEST}: {e}"))?;
    let tag = release["tag_name"].as_str().unwrap_or_default();
    if !newer(tag, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let asset = release["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(".exe"))))
        .ok_or(format!("Release {tag} hat keine Exe"))?;
    let url = asset["browser_download_url"].as_str().ok_or("Exe ohne Download-Link")?;
    let version = tag.trim_start_matches('v').to_string();
    shared.lock().unwrap().push(Level::Info, format!("Version {version} gefunden, wird geladen"));

    let file = exe.with_extension("new");
    let mut out = fs::File::create(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let res = http.get(url).header("User-Agent", "AUBLstAdapter").call().map_err(|e| format!("{url}: {e}"))?;
    let written = io::copy(&mut res.into_body().into_reader(), &mut out).map_err(|e| format!("{url}: {e}"));
    drop(out);
    // A cut connection leaves a short file; GitHub states the size, and TLS covers the rest.
    if written.as_ref().ok() != asset["size"].as_u64().as_ref() {
        let _ = fs::remove_file(&file);
        let error = written.err().unwrap_or_else(|| "Download unvollständig".into());
        shared.lock().unwrap().push(Level::Warn, format!("Update auf {version} nicht geladen"));
        return Err(error);
    }
    Ok(Some(Ready { version, file }))
}

/// The previous exe stays locked until its process ends, which may take a moment
/// after it started this one.
fn remove_old(exe: &Path) -> bool {
    let old = exe.with_extension("old");
    for _ in 0..20 {
        match fs::remove_file(&old) {
            Ok(()) => return true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
    false
}

/// Windows lets a running exe be renamed but not overwritten, so it moves aside first.
fn swap(exe: &Path, new: &Path) -> io::Result<()> {
    let old = exe.with_extension("old");
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)?;
    fs::rename(new, exe).inspect_err(|_| {
        let _ = fs::rename(&old, exe);
    })
}

/// Puts the new exe in place and starts it; the caller exits right after.
pub fn install(ready: &Ready) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    swap(&exe, &ready.file)?;
    std::process::Command::new(&exe).spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_higher_version_counts_as_newer() {
        assert!(newer("v1.4.0", "1.3.0"));
        assert!(newer("v1.10.0", "1.9.2"));
        assert!(newer("v2.0", "1.9.9"));
        assert!(!newer("v1.3.0", "1.3.0"));
        assert!(!newer("v1.2.9", "1.3.0"));
        assert!(!newer("v1.4.0-beta", "1.3.0"));
        assert!(!newer("", "1.3.0"));
    }

    #[test]
    fn swap_moves_the_running_exe_aside() {
        let dir = std::env::temp_dir().join("aublst-adapter-update-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (exe, new) = (dir.join("AUBLstAdapter.exe"), dir.join("AUBLstAdapter.new"));
        fs::write(&exe, "old").unwrap();
        fs::write(&new, "new").unwrap();
        swap(&exe, &new).unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(fs::read_to_string(exe.with_extension("old")).unwrap(), "old");
        assert!(!new.exists());
        assert!(remove_old(&exe));
        assert!(!remove_old(&exe));

        // Without a downloaded file the current exe must stay where it is.
        assert!(swap(&exe, &new).is_err());
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
        fs::remove_dir_all(&dir).unwrap();
    }
}
