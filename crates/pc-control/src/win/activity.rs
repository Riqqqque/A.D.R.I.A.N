//! Reading what's in front for [`crate::activity`]: the foreground window's
//! program and title, and Steam's running game. Only reads; never touches
//! the window or its program.

use super::Inner;
use super::from_wide;
use super::wnd::exe_path;
use crate::activity::{Front, Playing, folder_name, playing};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
};
use windows::core::w;

/// Steam's `RunningAppID` (0 when no game is running, or no Steam).
fn steam_app() -> u32 {
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Valve\\Steam"),
            w!("RunningAppID"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut v as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if r.is_ok() { v } else { 0 }
}

/// The game in front, if any. Never waits for the app index: until it's built
/// only names and full screen count.
pub fn game_in_front(inner: &Inner, fullscreen: bool) -> Option<Playing> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // Her own window, or nothing that belongs to a program.
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    let exe = exe_path(pid).unwrap_or_default();
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
    let title = from_wide(&buf[..n.min(buf.len())]);
    let game_folder = if exe.is_empty() {
        None
    } else {
        let index = inner.index.lock().unwrap_or_else(|e| e.into_inner());
        index.as_ref().filter(|i| i.is_game(&exe)).and_then(|i| {
            let p = exe.to_lowercase().replace('/', "\\");
            // The deepest root it's under: a library inside another one names it best.
            i.game_roots
                .iter()
                .filter(|r| p.starts_with(r.as_str()))
                .max_by_key(|r| r.len())
                .and_then(|r| folder_name(r, &exe))
                .or_else(|| Some(crate::activity::exe_stem(&exe)))
        })
    };
    playing(&Front {
        steam_app: if exe.is_empty() { steam_app() } else { 0 },
        exe,
        title,
        fullscreen,
        game_folder,
    })
}
