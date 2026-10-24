//! The Windows implementation of each action.

mod apps;
mod audio;
mod info;
mod media;
mod open;
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
            let built = apps::build(&policy);
            *index.lock().unwrap_or_else(|e| e.into_inner()) = Some(built);
            *building.lock().unwrap_or_else(|e| e.into_inner()) = false;
        });
    }

    /// The index, waiting for the first build if it's still running.
    fn index(&self, policy: &Policy) -> Index {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(i) = self
                .index
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                if i.built.elapsed() >= INDEX_MAX_AGE {
                    self.refresh(policy);
                }
                return i.clone();
            }
            if Instant::now() > deadline {
                return apps::build(policy);
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
        "pc_open" => open::run(args),
        "pc_media" => media::run(action, &s("app")),
        "pc_volume" => audio::run(action, args),
        "pc_system" => system::run(
            action,
            args.get("confirmed").and_then(Value::as_bool) == Some(true),
        ),
        "pc_info" => match action {
            "processes" => info::processes(),
            _ => info::summary(),
        },
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
