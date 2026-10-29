//! Self-update from the GitHub releases. A few minutes after start and then
//! every six hours it looks at the newest release; when that's newer than this
//! build it downloads the app, checks it against the release's `.sha256`, and
//! waits until she's idle in the tray (no window, not talking, not listening)
//! to swap the file and start the new copy. Turned off in settings, it only
//! says that a new version exists.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const REPO: &str = "Riqqqque/adrian-discord-voice";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const FIRST_CHECK: Duration = Duration::from_secs(3 * 60);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// What the settings page shows.
#[derive(Serialize, Clone, Default)]
pub struct UpdateView {
    pub version: String,
    /// "", "checking", "ready" (downloaded, installs when idle), "available" (auto-update off), "error"
    pub state: String,
    pub latest: Option<String>,
    pub message: Option<String>,
}

pub static STATUS: Mutex<Option<UpdateView>> = Mutex::new(None);

pub fn status() -> UpdateView {
    STATUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| UpdateView {
            version: VERSION.into(),
            ..Default::default()
        })
}

fn set(state: &str, latest: Option<&str>, message: Option<String>) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(UpdateView {
        version: VERSION.into(),
        state: state.into(),
        latest: latest.map(str::to_string),
        message,
    });
}

/// True when version `a` is above `b` (dotted numbers; anything else counts as 0).
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|n| n.parse().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .user_agent(concat!("adrian-desktop/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The newest release's version and the URLs of the app and its checksum.
fn latest(agent: &ureq::Agent) -> Result<(String, String, String), String> {
    let v: serde_json::Value = agent
        .get(&format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_json()
        .map_err(|e| e.to_string())?;
    let tag = v["tag_name"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches('v')
        .to_string();
    if tag.is_empty()
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+-".contains(c))
    {
        return Err("no usable release".into());
    }
    let name = format!("adrian-pc-app-{tag}-windows-x64.exe");
    let url = |n: &str| {
        v["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == n))
            .and_then(|x| x["browser_download_url"].as_str())
            .map(str::to_string)
    };
    let (Some(exe), Some(sum)) = (url(&name), url(&format!("{name}.sha256"))) else {
        return Err(format!("release {tag} has no PC app download"));
    };
    // Only ever download from this project's own release pages.
    let ok = |u: &str| u.starts_with(&format!("https://github.com/{REPO}/releases/download/"));
    if !ok(&exe) || !ok(&sum) {
        return Err("unexpected download address".into());
    }
    Ok((tag, exe, sum))
}

fn download(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    agent
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|e| e.to_string())
}

/// Download and verify the new app next to this one; returns its path.
fn fetch(
    agent: &ureq::Agent,
    exe_url: &str,
    sum_url: &str,
    here: &Path,
) -> Result<PathBuf, String> {
    let sum = String::from_utf8(download(agent, sum_url, 4096)?).map_err(|e| e.to_string())?;
    let want = sum
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if want.len() != 64 {
        return Err("bad checksum file".into());
    }
    let bytes = download(agent, exe_url, 200 * 1024 * 1024)?;
    let got: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != want {
        return Err("the download didn't match its checksum".into());
    }
    let dir = here.parent().ok_or("no app folder")?;
    let new = dir.join("ADRIAN.new.exe");
    std::fs::write(&new, &bytes).map_err(|e| format!("can't write to {}: {e}", dir.display()))?;
    Ok(new)
}

/// Swap the files and start the new copy; the caller exits right after.
/// A running exe can be renamed on Windows, just not overwritten.
fn install(new: &Path, here: &Path) -> Result<(), String> {
    let old = here.with_extension("old.exe");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(here, &old).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(new, here) {
        let _ = std::fs::rename(&old, here);
        return Err(e.to_string());
    }
    std::process::Command::new(here)
        .args(["--background", "--after-update"])
        .spawn()
        .map_err(|e| {
            // Couldn't start it: put the old one back.
            let _ = std::fs::rename(here, new);
            let _ = std::fs::rename(&old, here);
            e.to_string()
        })?;
    Ok(())
}

/// `ADRIAN.exe --update-now`: check, download, verify and install at once
/// (no waiting for her to be idle). For testing, and for scripts.
pub fn now() -> Result<String, String> {
    let here = std::env::current_exe().map_err(|e| e.to_string())?;
    let agent = agent();
    let (v, exe, sum) = latest(&agent)?;
    if !newer(&v, VERSION) {
        return Ok(format!("Up to date ({VERSION}; newest release {v})."));
    }
    let new = fetch(&agent, &exe, &sum, &here)?;
    install(&new, &here)?;
    Ok(format!(
        "Updated {VERSION} -> {v}; the new version is starting."
    ))
}

/// Leftovers from the last update.
pub fn clean_up() {
    if let Ok(here) = std::env::current_exe() {
        let _ = std::fs::remove_file(here.with_extension("old.exe"));
        if let Some(dir) = here.parent() {
            let _ = std::fs::remove_file(dir.join("ADRIAN.new.exe"));
        }
    }
}

/// Runs for the life of the app. `enabled` reads the setting; `idle` says
/// whether swapping now would interrupt anything; `quit` ends this copy.
pub fn run(enabled: impl Fn() -> bool, idle: impl Fn() -> bool, quit: impl Fn()) {
    let Ok(here) = std::env::current_exe() else {
        return;
    };
    std::thread::sleep(FIRST_CHECK);
    let agent = agent();
    loop {
        set("checking", None, None);
        match latest(&agent) {
            Ok((v, _, _)) if !newer(&v, VERSION) => set("", Some(&v), None),
            Ok((v, _, _)) if !enabled() => set("available", Some(&v), None),
            Ok((v, exe, sum)) => match fetch(&agent, &exe, &sum, &here) {
                Ok(new) => {
                    set("ready", Some(&v), None);
                    // Wait for a quiet moment (checked every 30 s), then swap.
                    while !idle() || !enabled() {
                        std::thread::sleep(Duration::from_secs(30));
                    }
                    match install(&new, &here) {
                        Ok(()) => {
                            quit();
                            return;
                        }
                        Err(e) => set(
                            "error",
                            Some(&v),
                            Some(format!("couldn't install {v}: {e}")),
                        ),
                    }
                }
                Err(e) => set("error", Some(&v), Some(e)),
            },
            Err(e) => set("error", None, Some(e)),
        }
        std::thread::sleep(EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn compares_versions() {
        assert!(newer("0.15.0", "0.14.9"));
        assert!(newer("v0.15.1", "0.15.0"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.14.9", "0.14.9"));
        assert!(!newer("0.14.9", "0.15.0"));
        // a missing part counts as 0: 0.15 and 0.15.0 are the same
        assert!(!newer("0.15.0", "0.15"));
        assert!(!newer("0.15", "0.15.0"));
        assert!(newer("0.15.1", "0.15"));
    }
}
