//! Self-update from the GitHub releases. A few minutes after start and then
//! every six hours it looks at the newest release; when that's newer than this
//! build it downloads the app, checks it against the release's `.sha256`, runs
//! it once to prove it starts and is that version, and waits until she's idle
//! in the tray (no window, not talking, not listening) to swap the file and
//! start the new copy. The copy it replaced stays as ADRIAN.old.exe, the way
//! back, until the new one has really worked: the server has welcomed it and
//! it has been up for a few minutes. Turned off in settings, it only says that
//! a new version exists.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

const REPO: &str = "Riqqqque/adrian-discord-voice";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const FIRST_CHECK: Duration = Duration::from_secs(3 * 60);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How long a download gets to answer `--self-check`: it inherits the tray's
/// low priority, and the virus scanner goes through a new exe first.
const SELF_CHECK: Duration = Duration::from_secs(30);

/// How updates stand.
#[derive(Clone, Default)]
pub struct UpdateView {
    pub version: String,
    /// "", "checking", "ready" (downloaded, installs when idle), "available" (auto-update off), "error"
    pub state: String,
    pub latest: Option<String>,
    pub message: Option<String>,
}

impl UpdateView {
    /// The line in settings, for one position of the automatic-updates switch:
    /// what's true depends on the switch as well as on what happened.
    pub fn line(&self, auto: bool) -> String {
        let v = &self.version;
        let latest = self.latest.as_deref().unwrap_or_default();
        match self.state.as_str() {
            "ready" if auto => format!(
                "Version {v}. {latest} is downloaded and installs the next time she's idle in the tray."
            ),
            "ready" => {
                format!(
                    "Version {v}. {latest} is downloaded and installs when automatic updates are on."
                )
            }
            "available" if auto => format!("Version {v}. {latest} is out: save to download it."),
            "available" => format!(
                "Version {v}. {latest} is out: turn on automatic updates, or download it from GitHub."
            ),
            "error" => format!(
                "Version {v}. The last update check failed: {}",
                self.message.as_deref().unwrap_or_default()
            ),
            _ if latest == v => format!("Version {v}, up to date."),
            _ => format!("Version {v}."),
        }
    }
}

pub static STATUS: Mutex<Option<UpdateView>> = Mutex::new(None);

/// Set by `check_now`, read by the wait between checks.
static CHECK_NOW: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// When this run started, for how long it has been up.
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);
/// The server has welcomed this run. It stays set: `Shared::online` goes false
/// with every dropped link, which doesn't undo that this version got through.
static WELCOMED: AtomicBool = AtomicBool::new(false);

/// Cut the wait for the next check short: automatic updates were just turned on.
pub fn check_now() {
    *CHECK_NOW.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    CHECK_NOW.1.notify_all();
}

/// Wait out `time` between checks, unless `check_now` ends it early.
fn rest(time: Duration) {
    let asked = CHECK_NOW.0.lock().unwrap_or_else(|e| e.into_inner());
    let (mut asked, _) = CHECK_NOW
        .1
        .wait_timeout_while(asked, time, |asked| !*asked)
        .unwrap_or_else(|e| e.into_inner());
    *asked = false;
}

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

/// Whether a `--self-check` run proves a download is the release it's meant to
/// be. A build that dies at start can't update itself out of that, and one that
/// reports a lower version than its tag would update again after every restart.
fn accepted(code: Option<i32>, stdout: &str, tag: &str) -> bool {
    code == Some(0) && stdout.trim() == tag
}

/// Run a program for its exit code and output; None if it won't start or
/// outlasts `limit`.
fn run_for(cmd: &mut Command, limit: Duration) -> Option<(Option<i32>, String)> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;
    let mut out = child.stdout.take()?;
    // On its own thread, so nothing here can wait past the limit.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = out.read_to_string(&mut text);
        let _ = tx.send(text);
    });
    let end = Instant::now() + limit;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    Some((code, rx.recv_timeout(Duration::from_secs(2)).ok()?))
}

/// Start a download once, before anything is swapped. One that fails is thrown
/// away and this copy carries on; the next check tries again.
fn prove(new: &Path, tag: &str) -> Result<(), String> {
    let seen = run_for(Command::new(new).arg("--self-check"), SELF_CHECK);
    if seen.is_some_and(|(code, out)| accepted(code, &out, tag)) {
        return Ok(());
    }
    let _ = std::fs::remove_file(new);
    Err(format!("the download of {tag} failed its start check"))
}

/// Put a verified download on disk and prove it. All the way to the disk: a
/// power cut after the swap mustn't leave an ADRIAN.exe whose bytes were still
/// in memory.
fn save(new: &Path, bytes: &[u8], tag: &str) -> Result<(), String> {
    let written = File::create(new).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    written.map_err(|e| format!("can't write {}: {e}", new.display()))?;
    prove(new, tag)
}

/// Download and verify the new app next to this one; returns its path.
fn fetch(
    agent: &ureq::Agent,
    exe_url: &str,
    sum_url: &str,
    here: &Path,
    tag: &str,
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
    save(&new, &bytes, tag)?;
    Ok(new)
}

/// Put `new` in `here`'s place and keep `here` as ADRIAN.old.exe. A running exe
/// can't be overwritten, but once it has a second name the first can be taken
/// over in one step, so there's an app to start at every moment, even if the
/// power goes mid-update. Without hard links it takes two renames, with nothing
/// at `here` between them. `step` runs after each change to the folder.
fn swap(new: &Path, here: &Path, step: impl Fn()) -> Result<(), String> {
    let old = here.with_extension("old.exe");
    let _ = std::fs::remove_file(&old);
    if std::fs::hard_link(here, &old).is_ok() {
        step();
        if std::fs::rename(new, here).is_ok() {
            return Ok(());
        }
        let _ = std::fs::remove_file(&old);
    }
    std::fs::rename(here, &old).map_err(|e| e.to_string())?;
    step();
    if let Err(e) = std::fs::rename(new, here) {
        let _ = std::fs::rename(&old, here);
        return Err(e.to_string());
    }
    Ok(())
}

/// Swap the files and start the new copy; the caller exits right after.
fn install(new: &Path, here: &Path) -> Result<(), String> {
    swap(new, here, || {})?;
    // ADRIAN.old.exe is this copy now, the new one's way back: a first welcome
    // landing before this copy exits has to leave it alone.
    WELCOMED.store(true, Ordering::Relaxed);
    std::process::Command::new(here)
        .args(["--background", "--after-update"])
        .spawn()
        .map_err(|e| {
            // Couldn't start it: put the old one back.
            let _ = std::fs::rename(here, new);
            let _ = std::fs::rename(here.with_extension("old.exe"), here);
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
    let new = fetch(&agent, &exe, &sum, &here, &v)?;
    install(&new, &here)?;
    Ok(format!(
        "Updated {VERSION} -> {v}; the new version is starting."
    ))
}

/// A download an earlier run left behind. Only for the copy that stays running:
/// that copy may be waiting to install this very file.
pub fn clean_up() {
    if let Ok(here) = std::env::current_exe() {
        remove_download(&here);
    }
}

fn remove_download(here: &Path) {
    if let Some(dir) = here.parent() {
        let _ = std::fs::remove_file(dir.join("ADRIAN.new.exe"));
    }
}

/// Whether this version has really worked, so the copy it replaced can go. That
/// copy is the only way back, and starting proves little: it takes a welcome
/// from the server and `FIRST_CHECK` of staying up, whichever comes later. With
/// no welcome (never paired, server down) it stays for a later run to settle.
fn proven(welcomed: bool, up: Duration) -> bool {
    welcomed && up >= FIRST_CHECK
}

fn settle_old_copy(here: &Path, welcomed: bool, up: Duration) {
    if proven(welcomed, up) {
        let _ = std::fs::remove_file(here.with_extension("old.exe"));
    }
}

/// The server welcomed this PC. Past `FIRST_CHECK` this is the moment the old
/// copy goes, since `run` may not wake for hours; before that `run` sees it when
/// the wait ends. Only the first welcome acts, and `install` uses that up: a
/// welcome while this copy swaps itself out mustn't take the way back it made.
pub fn welcomed() {
    if WELCOMED.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Ok(here) = std::env::current_exe() {
        settle_old_copy(&here, true, STARTED.elapsed());
    }
}

/// Runs for the life of the app. `enabled` reads the setting; `idle` says
/// whether swapping now would interrupt anything; `quit` ends this copy.
pub fn run(enabled: impl Fn() -> bool, idle: impl Fn() -> bool, quit: impl Fn()) {
    let Ok(here) = std::env::current_exe() else {
        return;
    };
    LazyLock::force(&STARTED);
    std::thread::sleep(FIRST_CHECK);
    // Only here, not in the loop: after a failed install that file can be the
    // only copy left.
    settle_old_copy(&here, WELCOMED.load(Ordering::Relaxed), FIRST_CHECK);
    let agent = agent();
    loop {
        set("checking", None, None);
        match latest(&agent) {
            Ok((v, _, _)) if !newer(&v, VERSION) => set("", Some(&v), None),
            Ok((v, _, _)) if !enabled() => set("available", Some(&v), None),
            Ok((v, exe, sum)) => match fetch(&agent, &exe, &sum, &here, &v) {
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
        rest(EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty folder of its own under the system's temp folder.
    fn temp_folder(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("adrian-update-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_the_right_version_passes_the_self_check() {
        assert!(accepted(Some(0), "0.15.3\r\n", "0.15.3"));
        // an old build under a new tag: installing it would repeat at every restart
        assert!(!accepted(Some(0), "0.15.2\r\n", "0.15.3"));
        assert!(!accepted(Some(0), "", "0.15.3"));
        assert!(!accepted(Some(1), "0.15.3\r\n", "0.15.3"));
        assert!(!accepted(None, "0.15.3\r\n", "0.15.3"));
    }

    #[cfg(windows)]
    #[test]
    fn reads_what_a_self_check_says() {
        let echo = |text: &str| {
            let line = format!("echo {text}");
            run_for(Command::new("cmd").args(["/c", &line]), SELF_CHECK).unwrap()
        };
        let (code, out) = echo("0.15.3");
        assert!(accepted(code, &out, "0.15.3"));
        let (code, out) = echo("0.15.2");
        assert!(!accepted(code, &out, "0.15.3"));
        let exit = run_for(Command::new("cmd").args(["/c", "exit 1"]), SELF_CHECK);
        assert_eq!(exit.map(|(code, _)| code), Some(Some(1)));
    }

    #[cfg(windows)]
    #[test]
    fn gives_up_on_a_self_check_that_hangs() {
        let start = Instant::now();
        let mut hung = Command::new("ping");
        hung.args(["-n", "30", "127.0.0.1"]);
        assert!(run_for(&mut hung, Duration::from_millis(300)).is_none());
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_download_that_wont_start_is_thrown_away() {
        let dir = temp_folder("prove");
        let new = dir.join("ADRIAN.new.exe");
        assert!(save(&new, b"not a program", "0.15.3").is_err());
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The app, a download, and where the replaced copy goes, in a temp folder.
    fn app_folder(what: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let dir = temp_folder(what);
        let (here, new) = (dir.join("ADRIAN.exe"), dir.join("ADRIAN.new.exe"));
        std::fs::write(&here, b"this version").unwrap();
        std::fs::write(&new, b"next version").unwrap();
        let old = dir.join("ADRIAN.old.exe");
        (dir, here, new, old)
    }

    #[test]
    fn a_power_cut_at_any_step_of_a_swap_leaves_an_app_to_start() {
        let (dir, here, new, old) = app_folder("swap");
        swap(&new, &here, || assert!(here.exists())).unwrap();
        assert_eq!(std::fs::read(&here).unwrap(), b"next version");
        assert_eq!(std::fs::read(&old).unwrap(), b"this version");
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Swapping out the exe that's running is the whole job, so try it on one.
    #[cfg(windows)]
    #[test]
    fn swaps_out_a_copy_that_is_running() {
        let (dir, here, new, old) = app_folder("running");
        let root = std::env::var("SystemRoot").unwrap();
        std::fs::copy(format!("{root}\\System32\\ping.exe"), &here).unwrap();
        let mut running = Command::new(&here)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let gap = std::cell::Cell::new(false);
        let swapped = swap(&new, &here, || gap.set(gap.get() || !here.exists()));
        let still_up = running.try_wait().unwrap().is_none();
        let _ = running.kill();
        let _ = running.wait();
        swapped.unwrap();
        assert!(still_up, "it has to be running for this to prove anything");
        assert!(!gap.get(), "the two renames were needed");
        assert_eq!(std::fs::read(&here).unwrap(), b"next version");
        assert!(old.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_with_no_download_leaves_the_app_as_it_was() {
        let (dir, here, new, old) = app_folder("no-download");
        std::fs::remove_file(&new).unwrap();
        assert!(swap(&new, &here, || {}).is_err());
        assert_eq!(std::fs::read(&here).unwrap(), b"this version");
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_copy_that_wont_start_is_swapped_back() {
        let (dir, here, new, old) = app_folder("wont-start");
        // Neither file is a program, so this can't start anything.
        assert!(install(&new, &here).is_err());
        assert_eq!(std::fs::read(&here).unwrap(), b"this version");
        assert_eq!(std::fs::read(&new).unwrap(), b"next version");
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_old_copy_outlasts_the_start_of_the_new_one() {
        let dir = temp_folder("leftovers");
        let here = dir.join("ADRIAN.exe");
        let old = dir.join("ADRIAN.old.exe");
        let new = dir.join("ADRIAN.new.exe");
        for f in [&here, &old, &new] {
            std::fs::write(f, b"x").unwrap();
        }
        // At start: only the stale download goes.
        remove_download(&here);
        assert!(!new.exists());
        assert!(old.exists());
        // Up long enough, but no welcome yet: it stays.
        settle_old_copy(&here, false, FIRST_CHECK);
        assert!(old.exists());
        // Welcomed too: now the old copy goes.
        settle_old_copy(&here, true, FIRST_CHECK);
        assert!(!old.exists());
        assert!(here.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_old_copy_waits_for_a_welcome_and_the_full_wait() {
        let early = FIRST_CHECK - Duration::from_secs(1);
        let long = 1000 * FIRST_CHECK;
        // A welcome right after start: the wait still has to run out.
        assert!(!proven(true, early));
        assert!(proven(true, FIRST_CHECK));
        // Never paired, or the server is down: uptime alone proves nothing.
        assert!(!proven(false, early));
        assert!(!proven(false, FIRST_CHECK));
        assert!(!proven(false, long));
        // A welcome long after start: it goes then.
        assert!(proven(true, long));
    }

    fn view(state: &str) -> UpdateView {
        UpdateView {
            version: "0.15.2".into(),
            state: state.into(),
            latest: Some("0.15.3".into()),
            message: None,
        }
    }

    #[test]
    fn a_downloaded_update_only_promises_an_install_with_the_switch_on() {
        let u = view("ready");
        assert!(u.line(true).contains("next time she's idle"));
        let off = u.line(false);
        assert!(!off.contains("next time"), "{off}");
        assert!(off.contains("when automatic updates are on"), "{off}");
    }

    #[test]
    fn with_the_switch_on_it_no_longer_says_to_turn_it_on() {
        let u = view("available");
        assert!(u.line(false).contains("turn on automatic updates"));
        let on = u.line(true);
        assert!(!on.contains("turn on"), "{on}");
    }

    #[test]
    fn the_quiet_lines_ignore_the_switch() {
        let mut u = view("");
        u.latest = Some("0.15.2".into());
        assert_eq!(u.line(true), "Version 0.15.2, up to date.");
        assert_eq!(u.line(false), "Version 0.15.2, up to date.");
        assert_eq!(view("checking").line(true), "Version 0.15.2.");
        let mut failed = view("error");
        failed.message = Some("no network".into());
        assert!(failed.line(false).ends_with("failed: no network"));
    }

    #[test]
    fn turning_updates_on_ends_the_wait_for_the_next_check() {
        let start = Instant::now();
        let asker = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            check_now();
        });
        rest(Duration::from_secs(30));
        asker.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        // Used up: the wait after this one runs its full length.
        let start = Instant::now();
        rest(Duration::from_millis(100));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

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
