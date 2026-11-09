//! Reading what's in front for [`crate::activity`]: the foreground window's
//! program (for a Store app, the one inside its frame) and title, whether it
//! fills its screen, and Steam's running game. Only reads; never touches the
//! window or its program.

use super::Inner;
use super::apps::Index;
use super::from_wide;
use super::screen::borderless_in_front;
use super::wnd::{exe_path, store_app};
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
    // A Store app shows itself through ApplicationFrameHost: judge the app
    // inside. Left as the frame (never a game) when it can't be found.
    let inside = if crate::activity::exe_stem(&exe) == "applicationframehost" {
        store_app(hwnd, pid, &title, &[])
    } else {
        None
    };
    // Borderless full screen, which the shell doesn't always report.
    let fullscreen = fullscreen || borderless_in_front(hwnd);
    // Read before taking the index lock, so the registry read never holds it.
    let steam = if exe.is_empty() { steam_app() } else { 0 };
    let index = inner.index.lock().unwrap_or_else(|e| e.into_inner());
    judge(index.as_ref(), exe, inside, title, fullscreen, || steam)
}

/// Whether the program in front is a game: `exe` is the window's own program,
/// `inside` the Store app found in its frame, if it is one. A Store app counts
/// only as a known game or one in a game folder (XboxGames and the like), never
/// for being full screen: that's how video and streaming apps always look.
fn judge(
    index: Option<&Index>,
    exe: String,
    inside: Option<String>,
    title: String,
    fullscreen: bool,
    steam_app: impl FnOnce() -> u32,
) -> Option<Playing> {
    let in_store_frame = inside.is_some();
    let exe = inside.unwrap_or(exe);
    let game_folder = if exe.is_empty() {
        None
    } else {
        index.filter(|i| i.is_game(&exe)).and_then(|i| {
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
        in_store_frame,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn index() -> Index {
        Index {
            apps: Vec::new(),
            game_roots: vec![
                "c:\\xboxgames\\".into(),
                "d:\\xboxgames\\".into(),
                "d:\\steamlibrary\\steamapps\\common\\".into(),
            ],
            built: Instant::now(),
        }
    }

    const FRAME: &str = "C:\\Windows\\System32\\ApplicationFrameHost.exe";

    /// A Store app found inside its frame, full screen.
    fn store_full_screen(app: &str) -> Option<Playing> {
        judge(
            Some(&index()),
            FRAME.into(),
            Some(app.into()),
            "Title".into(),
            true,
            || 0,
        )
    }

    #[test]
    fn a_store_video_app_full_screen_isnt_a_game() {
        for app in [
            "C:\\Windows\\System32\\WWAHost.exe",
            "C:\\Program Files\\WindowsApps\\4DF9E0F8.Netflix_6.99.5_x64__mcm4njqhnhss8\\Netflix.exe",
            "C:\\Program Files\\WindowsApps\\AmazonVideo.PrimeVideo_1.0.1_x64__pwbj9vvecjh7j\\PrimeVideo.exe",
            "C:\\Program Files\\WindowsApps\\Disney.37853FC22B2CE_1.0.2_x64__6rarf9sa4v8jt\\DisneyPlus.exe",
            "C:\\Program Files\\WindowsApps\\HuluLLC.HuluPlus_3.0.0_x64__fphbd361v8tya\\Hulu.exe",
            "C:\\Program Files\\WindowsApps\\AppleInc.AppleTVWin_1.0.0_x64__nzyj5cx40ttqa\\AppleTV.exe",
            "C:\\Program Files\\WindowsApps\\CrunchyrollLLC.Crunchyroll_1.0.0_x64__abcdefghjkmnp\\Crunchyroll.exe",
            "C:\\Program Files\\WindowsApps\\SpotifyAB.SpotifyMusic_1.250.0_x64__zpdnekdrzrea0\\Spotify.exe",
            "C:\\Program Files\\WindowsApps\\GoogleInc.YouTube_1.0.0_x64__abcdefghjkmnp\\YouTube.exe",
            "C:\\Program Files\\WindowsApps\\Microsoft.ZuneVideo_10.0_x64__8wekyb3d8bbwe\\Video.UI.exe",
            // Any other Store app: full screen alone says nothing about it.
            "C:\\Program Files\\WindowsApps\\Example.Puzzle_1.0_x64__abcdefghjkmnp\\Puzzle.exe",
        ] {
            assert_eq!(store_full_screen(app), None, "{app}");
        }
    }

    #[test]
    fn a_store_game_in_a_game_folder_full_screen_is_a_game() {
        let p = store_full_screen("D:\\XboxGames\\Some Game\\Content\\game.exe");
        assert_eq!(p.and_then(|p| p.name).as_deref(), Some("Some Game"));
        // A known Store game, even before the app index is built.
        let mc = judge(
            None,
            FRAME.into(),
            Some(
                "C:\\Program Files\\WindowsApps\\Microsoft.MinecraftUWP_1.21.0_x64__8wekyb3d8bbwe\\Minecraft.Windows.exe"
                    .into(),
            ),
            "Minecraft".into(),
            true,
            || 0,
        );
        assert_eq!(mc.and_then(|p| p.name).as_deref(), Some("Minecraft"));
    }

    #[test]
    fn a_known_game_is_a_game_wherever_it_runs() {
        let cod = judge(
            Some(&index()),
            "E:\\Games\\Call of Duty\\cod.exe".into(),
            None,
            "Call of Duty".into(),
            false,
            || 0,
        );
        assert_eq!(cod.and_then(|p| p.name).as_deref(), Some("Call of Duty"));
        let apex = judge(
            Some(&index()),
            "D:\\SteamLibrary\\steamapps\\common\\Apex Legends\\r5apex.exe".into(),
            None,
            String::new(),
            true,
            || 0,
        );
        assert_eq!(apex.and_then(|p| p.name).as_deref(), Some("Apex Legends"));
    }

    #[test]
    fn full_screen_outside_a_store_frame_still_counts() {
        let indie = judge(
            Some(&index()),
            "E:\\Indie\\Something\\game.exe".into(),
            None,
            String::new(),
            true,
            || 0,
        );
        assert_eq!(indie, Some(Playing { name: None }));
        // A Store web app's host seen on its own is never a game either.
        let host = judge(
            Some(&index()),
            "C:\\Windows\\System32\\WWAHost.exe".into(),
            None,
            String::new(),
            true,
            || 0,
        );
        assert_eq!(host, None);
        // A frame whose app couldn't be found stays the frame.
        let frame = judge(
            Some(&index()),
            FRAME.into(),
            None,
            String::new(),
            true,
            || 0,
        );
        assert_eq!(frame, None);
    }

    #[test]
    fn an_unreadable_program_asks_steam() {
        let p = judge(None, String::new(), None, String::new(), false, || 570);
        assert_eq!(p, Some(Playing { name: None }));
        assert_eq!(
            judge(None, String::new(), None, String::new(), false, || 0),
            None
        );
    }
}
