//! Open windows: list, close, focus, minimize. Windows of games are never
//! touched, and nothing is ever killed: closing asks the window to close.

use super::apps::Index;
use super::{exe_stem, from_wide};
use crate::Outcome;
use crate::matching::{alias, normalize, score};
use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetWindow,
    GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, PostMessageW, SW_MINIMIZE, SW_RESTORE, SetForegroundWindow, ShowWindow,
    WM_CLOSE, WS_EX_TOOLWINDOW,
};
use windows::core::PWSTR;

#[derive(Clone, Debug)]
pub struct Win {
    pub hwnd: HWND,
    pub title: String,
    /// For a Store app's frame, the app's own program, not ApplicationFrameHost.
    pub exe: String,
    /// DWMWA_CLOAKED: nonzero when the shell or the app hides it (another
    /// virtual desktop, a suspended Store app).
    pub cloaked: u32,
    /// A Store (UWP) app's ApplicationFrameHost frame.
    pub frame_host: bool,
}

/// Programs whose windows aren't apps people mean.
const SHELL: [&str; 6] = [
    "explorer",
    "textinputhost",
    "applicationframehost",
    "shellexperiencehost",
    "searchhost",
    "startmenuexperiencehost",
];

pub fn exe_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
    out.push(hwnd);
    TRUE
}

/// Real app windows: visible, titled, top-level, not tool windows, not hidden by the shell.
pub fn list() -> Vec<Win> {
    list_all()
        .into_iter()
        .filter(|w| w.cloaked == 0 && !w.frame_host)
        .collect()
}

/// Like [`list`], in z-order (front first), plus cloaked windows and Store
/// apps' frames, each named by the app inside it.
pub fn list_all() -> Vec<Win> {
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize));
    }
    let me = std::process::id();
    let mut out = Vec::new();
    for &hwnd in &all {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
                continue;
            }
            if GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                continue;
            }
            if (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
                continue;
            }
            let mut cloaked = 0u32;
            let _ =
                DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == me {
                continue;
            }
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf) as usize;
            let title = from_wide(&buf[..n.min(buf.len())]);
            let mut exe = exe_path(pid).unwrap_or_default();
            // Shell windows aren't apps, except Explorer folder windows and
            // the frames Store apps run in.
            let stem = exe_stem(&exe);
            let frame_host = stem == "applicationframehost";
            if frame_host {
                exe = store_app(hwnd, pid, &title, &all).unwrap_or_default();
            } else if SHELL.contains(&stem.as_str())
                && !(stem == "explorer" && title != "Program Manager")
            {
                continue;
            }
            out.push(Win {
                hwnd,
                title,
                exe,
                cloaked,
                frame_host,
            });
        }
    }
    out
}

unsafe extern "system" fn collect_children(hwnd: HWND, lp: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
    out.push(hwnd);
    TRUE
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    from_wide(&buf[..n.min(buf.len())])
}

/// The program of the Store app inside an ApplicationFrameHost frame: its
/// CoreWindow is a child of the frame, or, while the app is suspended or
/// minimized, a top-level window of its own with the same title.
fn store_app(frame: HWND, frame_pid: u32, title: &str, top: &[HWND]) -> Option<String> {
    let mut kids: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            frame,
            Some(collect_children),
            LPARAM(&mut kids as *mut Vec<HWND> as isize),
        );
    }
    let pid_of = |h: HWND| {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        pid
    };
    let inner = kids
        .into_iter()
        .map(pid_of)
        .find(|&p| p != 0 && p != frame_pid)
        .or_else(|| {
            top.iter()
                .copied()
                .filter(|&h| class_of(h) == "Windows.UI.Core.CoreWindow")
                .find(|&h| {
                    let mut buf = [0u16; 512];
                    let n = unsafe { GetWindowTextW(h, &mut buf) } as usize;
                    from_wide(&buf[..n.min(buf.len())]) == title
                })
                .map(pid_of)
        })?;
    exe_path(inner)
}

/// A friendly name for a program: "chrome" → "Chrome". A Store app goes by
/// its window's title ("Calculator"), since its program names aren't.
pub(super) fn label(w: &Win) -> String {
    if w.frame_host && !w.title.is_empty() {
        return w.title.clone();
    }
    let s = exe_stem(&w.exe);
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => w.title.clone(),
    }
}

/// What a spoken name finds among some windows.
pub(super) enum Found {
    /// Every window of the best-matching program, front first.
    Some(Vec<Win>),
    None,
    /// Several programs match: what to ask.
    Many(String),
}

/// The windows a spoken name means: every window of the best-matching program.
fn find(query: &str) -> Result<Vec<Win>, String> {
    match find_in(query, list()) {
        Found::Some(w) => Ok(w),
        Found::None => Err(format!("Nothing called \"{query}\" is open.")),
        Found::Many(ask) => Err(ask),
    }
}

/// [`find`] among these windows.
pub(super) fn find_in(query: &str, wins: Vec<Win>) -> Found {
    let q = normalize(alias(query).unwrap_or(query));
    let q2 = normalize(query);
    let mut best = 0;
    let mut hits: Vec<(u32, Win)> = Vec::new();
    for w in wins {
        let stem = normalize(&exe_stem(&w.exe));
        let title = normalize(&w.title);
        // "chrome" → chrome.exe; "google chrome" → the title's "- Google Chrome"
        let s = [
            score(&q, &stem),
            score(&q2, &stem),
            score(&q, &title).min(90),
            score(&q2, &title).min(90),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        if s == 0 {
            continue;
        }
        best = best.max(s);
        hits.push((s, w));
    }
    let top: Vec<Win> = hits
        .into_iter()
        .filter(|(s, _)| *s == best)
        .map(|(_, w)| w)
        .collect();
    if top.is_empty() {
        return Found::None;
    }
    let mut programs: Vec<String> = top.iter().map(|w| exe_stem(&w.exe)).collect();
    programs.sort();
    programs.dedup();
    if programs.len() > 1 {
        let names: Vec<String> = top
            .iter()
            .map(|w| format!("{} ({})", w.title, label(w)))
            .take(6)
            .collect();
        return Found::Many(format!(
            "Several open windows match \"{query}\": {}. Ask which one.",
            names.join(", ")
        ));
    }
    Found::Some(top)
}

pub fn act(index: &Index, action: &str, name: &str) -> Outcome {
    if name.is_empty() {
        return Outcome::no("Say which app.");
    }
    let wins = match find(name) {
        Ok(w) => w,
        Err(e) => return Outcome::no(e),
    };
    let app = label(&wins[0]);
    if wins.iter().any(|w| index.is_protected(&w.exe)) {
        let what = if wins.iter().any(|w| index.is_game(&w.exe)) {
            "is a game"
        } else {
            "might be a game: I can't tell which program it is"
        };
        return Outcome::no(format!(
            "{app} {what}. I don't touch running games, so anti-cheat never sees me. They can close or switch to it themselves."
        ));
    }
    unsafe {
        match action {
            "close" => {
                let mut n = 0;
                for w in &wins {
                    if PostMessageW(w.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() {
                        n += 1;
                    }
                }
                if n == 0 {
                    return Outcome::no(format!("{app} didn't accept the close request."));
                }
                Outcome::ok(format!(
                    "Asked {app} to close{}. If it has unsaved work it may ask first.",
                    if n > 1 {
                        format!(" ({n} windows)")
                    } else {
                        String::new()
                    }
                ))
            }
            "minimize" => {
                for w in &wins {
                    let _ = ShowWindow(w.hwnd, SW_MINIMIZE);
                }
                Outcome::ok(format!("Minimized {app}."))
            }
            _ => {
                let w = &wins[0];
                if IsIconic(w.hwnd).as_bool() {
                    let _ = ShowWindow(w.hwnd, SW_RESTORE);
                }
                if !SetForegroundWindow(w.hwnd).as_bool() {
                    // Windows only lets the active app hand over focus; a
                    // minimize and restore brings the window up anyway.
                    let _ = ShowWindow(w.hwnd, SW_MINIMIZE);
                    let _ = ShowWindow(w.hwnd, SW_RESTORE);
                }
                Outcome::ok(format!("Switched to {app}."))
            }
        }
    }
}

pub fn running(index: &Index) -> Outcome {
    let wins = list();
    if wins.is_empty() {
        return Outcome::ok("No app windows are open.");
    }
    let mut seen: Vec<String> = Vec::new();
    let mut lines = Vec::new();
    for w in &wins {
        let stem = exe_stem(&w.exe);
        if seen.contains(&stem) {
            continue;
        }
        seen.push(stem);
        let game = if index.is_game(&w.exe) { " (game)" } else { "" };
        lines.push(format!(
            "{}{game}: {}",
            label(w),
            w.title.chars().take(60).collect::<String>()
        ));
    }
    Outcome::ok(format!("Open: {}", lines.join("; ")))
}
