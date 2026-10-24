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
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW, SW_MINIMIZE,
    SW_RESTORE, SetForegroundWindow, ShowWindow, WM_CLOSE, WS_EX_TOOLWINDOW,
};
use windows::core::PWSTR;

#[derive(Clone, Debug)]
pub struct Win {
    pub hwnd: HWND,
    pub title: String,
    pub exe: String,
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
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut all as *mut Vec<HWND> as isize));
    }
    let me = std::process::id();
    let mut out = Vec::new();
    for hwnd in all {
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
            if cloaked != 0 {
                continue;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == me {
                continue;
            }
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf) as usize;
            let title = from_wide(&buf[..n.min(buf.len())]);
            let exe = exe_path(pid).unwrap_or_default();
            // Shell windows aren't apps, except Explorer folder windows.
            let stem = exe_stem(&exe);
            if SHELL.contains(&stem.as_str()) && !(stem == "explorer" && title != "Program Manager")
            {
                continue;
            }
            out.push(Win { hwnd, title, exe });
        }
    }
    out
}

/// A friendly name for a program: "chrome" → "Chrome".
fn label(w: &Win) -> String {
    let s = exe_stem(&w.exe);
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => w.title.clone(),
    }
}

/// The windows a spoken name means: every window of the best-matching program.
fn find(query: &str) -> Result<Vec<Win>, String> {
    let wins = list();
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
        return Err(format!("Nothing called \"{query}\" is open."));
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
        return Err(format!(
            "Several open windows match \"{query}\": {}. Ask which one.",
            names.join(", ")
        ));
    }
    Ok(top)
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
    if wins.iter().any(|w| index.is_game(&w.exe)) {
        return Outcome::no(format!(
            "{app} is a game. I don't touch running games, so anti-cheat never sees me. They can close or switch to it themselves."
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
