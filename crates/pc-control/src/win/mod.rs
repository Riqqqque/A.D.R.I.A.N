//! The Windows implementation of each action.

pub(crate) mod activity;
mod apps;
pub(crate) mod audio;
mod clipboard;
mod health;
mod info;
mod layout;
mod media;
mod nvml;
mod open;
mod screen;
mod system;
mod wnd;

use crate::{Outcome, Policy};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

pub use apps::Index;

/// Rebuilt at most this often (new installs show up without a restart).
const INDEX_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// How long an action waits for the first build before building it itself.
/// With `apps`' own limit on the Start menu list, that stays well under the
/// server's 15 s wait for an answer (it used to be 15 s by itself).
const FIRST_BUILD_WAIT: Duration = Duration::from_secs(2);

pub struct Inner {
    index: Arc<Mutex<Option<Index>>>,
    building: Arc<Mutex<bool>>,
}

impl Inner {
    pub fn new(policy: &Policy) -> Self {
        let me = Inner {
            index: Arc::new(Mutex::new(None)),
            building: Arc::new(Mutex::new(false)),
        };
        me.refresh(policy);
        me
    }

    /// Start a rebuild in the background unless one is running or the index is fresh.
    pub fn refresh(&self, policy: &Policy) {
        let fresh = self
            .index
            .lock()
            .map(|i| {
                i.as_ref()
                    .is_some_and(|i| i.built.elapsed() < INDEX_MAX_AGE)
            })
            .unwrap_or(false);
        {
            let mut b = self.building.lock().unwrap_or_else(|e| e.into_inner());
            if *b || fresh {
                return;
            }
            *b = true;
        }
        let (index, building, policy) = (self.index.clone(), self.building.clone(), policy.clone());
        std::thread::spawn(move || {
            // Cleared however the build ends: a build that panicked used to
            // leave this set, so no later one ever started.
            struct Done(Arc<Mutex<bool>>);
            impl Drop for Done {
                fn drop(&mut self) {
                    *self.0.lock().unwrap_or_else(|e| e.into_inner()) = false;
                }
            }
            let _done = Done(building);
            let built = apps::build(&policy);
            *index.lock().unwrap_or_else(|e| e.into_inner()) = Some(built);
        });
    }

    /// The index, waiting a little for the first build if it's still running,
    /// then building it here. What's built here is kept, so a first build that
    /// never finishes costs one action this wait, not every action after it.
    fn index(&self, policy: &Policy) -> Index {
        let deadline = Instant::now() + FIRST_BUILD_WAIT;
        loop {
            // Copied out first: `refresh` takes this lock too, and a guard kept
            // across it (as an `if let` on the lock did) hung the thread for
            // good once the index was ten minutes old, and with it everything
            // else that reads the index, the game-in-front check included.
            let now = self.index.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(i) = now {
                if i.built.elapsed() >= INDEX_MAX_AGE {
                    self.refresh(policy);
                }
                return i;
            }
            if Instant::now() > deadline {
                let built = apps::build(policy);
                self.index
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert_with(|| built.clone());
                return built;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub fn run(inner: &Inner, policy: &Policy, name: &str, action: &str, args: &Value) -> Outcome {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    match name {
        "pc_apps" => {
            let index = inner.index(policy);
            match action {
                "open" => apps::open(&index, &s("name")),
                "close" | "focus" | "minimize" => wnd::act(&index, action, &s("name")),
                "running" => wnd::running(&index),
                "show_desktop" => match shell_open(
                    "explorer.exe",
                    Some("shell:::{3080F90D-D7AD-11D9-BD98-0000947B0257}"),
                ) {
                    Ok(()) => Outcome::ok("Showing the desktop."),
                    Err(e) => Outcome::no(e),
                },
                "list" => apps::list(&index, &s("name")),
                "which" => apps::which(&index, &s("name")),
                _ => Outcome::no(
                    "pc_apps action must be open, close, focus, minimize, running, show_desktop or list.",
                ),
            }
        }
        "pc_windows" => layout::run(&inner.index(policy), action, args),
        "pc_open" => open::run(args),
        "pc_media" => media::run(action, &s("app")),
        "pc_volume" => audio::run(action, args, || inner.index(policy)),
        "pc_system" => system::run(
            action,
            args.get("confirmed").and_then(Value::as_bool) == Some(true),
        ),
        "pc_info" => match action {
            "processes" => info::processes(),
            "health" => health::run(),
            _ => info::summary(),
        },
        "pc_clipboard" => clipboard::read(),
        "pc_screen" => screen::run(&inner.index(policy), action, &s("name")),
        _ => Outcome::no(format!("Unknown PC action {name}.")),
    }
}

/// COM on this thread for the duration of a call.
pub(crate) struct Com(bool);
impl Com {
    pub fn init() -> Self {
        // S_OK or S_FALSE: ours to undo. RPC_E_CHANGED_MODE: the thread already has COM.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Com(hr.is_ok())
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Open a file, folder, URL or program the way double-clicking it would.
pub(crate) fn shell_open(file: &str, params: Option<&str>) -> Result<(), String> {
    let f = HSTRING::from(file);
    let r = unsafe {
        match params {
            Some(p) => ShellExecuteW(
                None,
                w!("open"),
                &f,
                &HSTRING::from(p),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            ),
            None => ShellExecuteW(
                None,
                w!("open"),
                &f,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            ),
        }
    };
    // ShellExecute reports success as a value above 32.
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("Windows couldn't open it (code {}).", r.0 as isize))
    }
}

/// A wide string buffer → String, up to the first NUL.
pub(crate) fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// "C:\\Program Files\\Spotify\\Spotify.exe" → "spotify".
pub(crate) fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    file.strip_suffix(".exe")
        .or_else(|| file.strip_suffix(".EXE"))
        .unwrap_or(file)
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{INDEX_MAX_AGE, Index, Inner};
    use crate::Policy;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn an_index_past_its_age_is_handed_back_and_rebuilt_without_hanging() {
        let Some(old) = Instant::now().checked_sub(INDEX_MAX_AGE + Duration::from_secs(1)) else {
            return; // the clock started too recently to make an old index
        };
        let inner = Arc::new(Inner {
            index: Arc::new(Mutex::new(Some(Index { apps: Vec::new(), game_roots: vec!["d:/games/".into()], built: old }))),
            building: Arc::new(Mutex::new(false)),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let me = inner.clone();
        std::thread::spawn(move || {
            let _ = tx.send(me.index(&Policy::default()).game_roots);
        });
        // This used to hang for good: the stale check called `refresh` with the lock held.
        let roots = rx.recv_timeout(Duration::from_secs(5)).expect("index() hung on a stale index");
        assert_eq!(roots, vec!["d:/games/".to_string()]);
        // The lock is free for everyone else (the game-in-front check takes it every 2 s).
        assert!(inner.index.try_lock().is_ok() || *inner.building.lock().unwrap());
    }
}
