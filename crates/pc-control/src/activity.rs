//! Whether someone is playing a game on this PC right now, for the server's
//! quiet mode: while they are, she only answers to her name and keeps
//! anything that can wait for after the match.
//!
//! Only what's in front counts: a game in a game folder (or known by name)
//! whose window has the focus, or a full-screen app that isn't a video player
//! or a browser. Everything is read from outside the game (the foreground
//! window, its program's path and title, Steam's registry value): nothing
//! hooks into it or sends it messages.
//!
//! The raw reading is debounced: playing starts after 5 seconds in a game and
//! ends after 20 seconds out of one, so alt-tabbing to Discord for a moment
//! doesn't end it.

use serde_json::{Value, json};

/// What's in front right now, as read by the Windows side.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Front {
    /// The foreground window's program, full path. Empty when it can't be read
    /// (an elevated or protected process).
    pub exe: String,
    pub title: String,
    /// A full-screen app is in front (Windows says "busy", "D3D full screen" or "presentation").
    pub fullscreen: bool,
    /// Steam's `RunningAppID`: a game started through Steam is running (0: none).
    pub steam_app: u32,
    /// The game's name, when the program is in a game's install folder.
    pub game_folder: Option<String>,
}

/// A game is being played: its name, if it can be told.
#[derive(Clone, Debug, PartialEq)]
pub struct Playing {
    pub name: Option<String>,
}

/// Programs whose name says which game they are, wherever they're installed.
const KNOWN: [(&str, &str); 16] = [
    ("valorant-win64-shipping", "VALORANT"),
    ("cod", "Call of Duty"),
    ("cod24-cod", "Call of Duty"),
    ("cs2", "Counter-Strike 2"),
    ("fortniteclient-win64-shipping", "Fortnite"),
    ("r5apex", "Apex Legends"),
    ("r5apex_dx12", "Apex Legends"),
    ("league of legends", "League of Legends"),
    ("robloxplayerbeta", "Roblox"),
    ("overwatch", "Overwatch"),
    ("rocketleague", "Rocket League"),
    ("gta5", "GTA V"),
    ("gta5_enhanced", "GTA V"),
    ("eldenring", "Elden Ring"),
    ("minecraft.windows", "Minecraft"),
    ("rainbowsix", "Rainbow Six Siege"),
];

/// Full screen but not a game: video players, browsers (a full-screen show), slides.
const NOT_GAMES: [&str; 19] = [
    "chrome",
    "msedge",
    "firefox",
    "brave",
    "opera",
    "vivaldi",
    "vlc",
    "mpv",
    "mpc-hc64",
    "mpc-be64",
    "potplayermini64",
    "plex",
    "plexamp",
    "netflix",
    "wmplayer",
    "video.ui",
    "powerpnt",
    "explorer",
    // Store apps (Netflix, Movies & TV) show their windows through it.
    "applicationframehost",
];

/// "C:\\Games\\Foo\\foo.exe" → "foo".
pub fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let lower = file.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

fn known(stem: &str, title: &str) -> Option<String> {
    if let Some((_, name)) = KNOWN.iter().find(|(s, _)| *s == stem) {
        return Some((*name).to_string());
    }
    // Minecraft: Java Edition is java(w).exe; only its window title says so.
    if (stem == "javaw" || stem == "java") && title.starts_with("Minecraft") {
        return Some("Minecraft".into());
    }
    None
}

/// Whether what's in front is a game. Her own window never is (the caller
/// leaves it out).
pub fn playing(f: &Front) -> Option<Playing> {
    let stem = exe_stem(&f.exe);
    let name = known(&stem, &f.title);
    if let Some(folder) = &f.game_folder {
        return Some(Playing {
            name: name.or_else(|| Some(folder.clone())),
        });
    }
    if name.is_some() {
        return Some(Playing { name });
    }
    if f.fullscreen && !NOT_GAMES.contains(&stem.as_str()) {
        return Some(Playing { name: None });
    }
    // A program that can't be read, with a Steam game running: most likely that
    // game (anti-cheat protects its process).
    if f.exe.is_empty() && f.steam_app != 0 {
        return Some(Playing { name: None });
    }
    None
}

/// The game a path in a game folder belongs to, by the folder: the first folder
/// below a library ("steamapps\\common\\Game", "XboxGames\\Game"), or the
/// install folder itself ("Riot Games\\VALORANT", "Call of Duty"). `root` is
/// lowercase, as the app index keeps them; the name keeps the path's own casing.
pub fn folder_name(root: &str, path: &str) -> Option<String> {
    let root = root.trim_end_matches('\\');
    let path = path.replace('/', "\\");
    if root.is_empty() || !path.to_lowercase().starts_with(root) {
        return None;
    }
    let last = root.rsplit('\\').next().unwrap_or(root);
    if last == "versions" && root.contains("roblox") {
        return Some("Roblox".into());
    }
    // Lowercasing kept the length for ASCII paths; anything else falls back to the root's own text.
    let (head, rest) = match (path.get(..root.len()), path.get(root.len()..)) {
        (Some(h), Some(r)) if h.to_lowercase() == root => (h.to_string(), r.to_string()),
        _ => (root.to_string(), String::new()),
    };
    let library = ["common", "xboxgames", "games", "ea games"].contains(&last);
    let name = if library {
        rest.trim_start_matches('\\').split('\\').next().unwrap_or("")
    } else {
        head.rsplit('\\').next().unwrap_or("")
    };
    let name = name.trim();
    // A program right in the library folder has no game folder to name it by.
    (!name.is_empty() && !name.to_lowercase().ends_with(".exe")).then(|| name.to_string())
}

/// What the PC tells the server: playing or not, and the game's name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Activity {
    pub gaming: bool,
    pub game: Option<String>,
}

impl Activity {
    /// `{"type":"activity","gaming":true,"game":"VALORANT"}` (no `game` when unknown or not playing).
    pub fn message(&self) -> Value {
        let mut m = json!({ "type": "activity", "gaming": self.gaming });
        if let (true, Some(g)) = (self.gaming, &self.game) {
            m["game"] = json!(g.chars().take(60).collect::<String>());
        }
        m
    }
}

/// Playing starts after this long in a game...
pub const START_MS: u64 = 5_000;
/// ...and ends after this long out of one.
pub const END_MS: u64 = 20_000;

/// Turns readings every couple of seconds into a steady playing / not playing.
#[derive(Default, Debug)]
pub struct Debounce {
    now: Activity,
    /// Since when the readings have disagreed with `now` (ms on the caller's clock).
    since: Option<u64>,
}

impl Debounce {
    pub fn current(&self) -> &Activity {
        &self.now
    }

    /// One reading at `now_ms`. Returns the new state when it changes.
    pub fn observe(&mut self, now_ms: u64, seen: Option<&Playing>) -> Option<Activity> {
        let in_game = seen.is_some();
        if in_game == self.now.gaming {
            self.since = None;
            // Same match: learn its name if it wasn't known at the start.
            if let (Some(p), None) = (seen, &self.now.game) {
                self.now.game = p.name.clone();
            }
            return None;
        }
        let since = *self.since.get_or_insert(now_ms);
        let wait = if in_game { START_MS } else { END_MS };
        if now_ms.saturating_sub(since) < wait {
            return None;
        }
        self.since = None;
        self.now = Activity {
            gaming: in_game,
            game: seen.and_then(|p| p.name.clone()),
        };
        Some(self.now.clone())
    }

    /// Quiet mode was turned off: not playing from now on, at once.
    pub fn reset(&mut self) -> Option<Activity> {
        self.since = None;
        if !self.now.gaming {
            return None;
        }
        self.now = Activity::default();
        Some(self.now.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front(exe: &str) -> Front {
        Front {
            exe: exe.into(),
            ..Front::default()
        }
    }

    #[test]
    fn a_game_folder_in_front_is_playing() {
        let f = Front {
            game_folder: Some("Hollow Knight".into()),
            ..front("D:\\SteamLibrary\\steamapps\\common\\Hollow Knight\\hollow_knight.exe")
        };
        assert_eq!(
            playing(&f),
            Some(Playing {
                name: Some("Hollow Knight".into())
            })
        );
    }

    #[test]
    fn a_known_game_gets_its_real_name_wherever_it_is() {
        let f = Front {
            game_folder: Some("live".into()),
            ..front("C:\\Riot Games\\VALORANT\\live\\ShooterGame\\Binaries\\Win64\\VALORANT-Win64-Shipping.exe")
        };
        assert_eq!(playing(&f).unwrap().name.as_deref(), Some("VALORANT"));
        let gta = front("E:\\Rockstar\\GTA5.exe");
        assert_eq!(playing(&gta).unwrap().name.as_deref(), Some("GTA V"));
    }

    #[test]
    fn minecraft_java_is_known_by_its_title() {
        let mut f = front("C:\\Program Files\\Java\\bin\\javaw.exe");
        assert_eq!(playing(&f), None);
        f.title = "Minecraft 1.21.11".into();
        assert_eq!(playing(&f).unwrap().name.as_deref(), Some("Minecraft"));
    }

    #[test]
    fn full_screen_counts_unless_its_a_show_or_a_browser() {
        let mut f = front("D:\\Indie\\Something\\game.exe");
        assert_eq!(playing(&f), None);
        f.fullscreen = true;
        assert_eq!(playing(&f), Some(Playing { name: None }));
        for video in [
            "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
            "C:\\Program Files\\VideoLAN\\VLC\\vlc.exe",
            "C:\\Windows\\explorer.exe",
            "C:\\Windows\\System32\\ApplicationFrameHost.exe",
        ] {
            let f = Front {
                fullscreen: true,
                ..front(video)
            };
            assert_eq!(playing(&f), None, "{video}");
        }
    }

    #[test]
    fn an_unreadable_program_counts_only_with_a_steam_game_running() {
        assert_eq!(playing(&front("")), None);
        let f = Front {
            steam_app: 730,
            ..front("")
        };
        assert!(playing(&f).is_some());
        // Steam's value alone (Wallpaper Engine sets it too) isn't enough.
        let f = Front {
            steam_app: 431960,
            ..front("C:\\Windows\\System32\\notepad.exe")
        };
        assert_eq!(playing(&f), None);
    }

    #[test]
    fn names_games_by_their_folder() {
        let steam = "d:\\steamlibrary\\steamapps\\common";
        let p = "D:\\SteamLibrary\\steamapps\\common\\Counter-Strike Global Offensive\\game\\bin\\win64\\cs2.exe";
        assert_eq!(
            folder_name(steam, p).as_deref(),
            Some("Counter-Strike Global Offensive")
        );
        assert_eq!(
            folder_name("c:\\riot games\\valorant", "C:\\Riot Games\\VALORANT\\live\\VALORANT.exe")
                .as_deref(),
            Some("VALORANT")
        );
        assert_eq!(
            folder_name(
                "c:\\program files (x86)\\call of duty\\",
                "C:\\Program Files (x86)\\Call of Duty\\_retail_\\cod.exe"
            )
            .as_deref(),
            Some("Call of Duty")
        );
        assert_eq!(
            folder_name("d:\\xboxgames\\", "D:\\XboxGames\\Starfield\\Content\\Starfield.exe")
                .as_deref(),
            Some("Starfield")
        );
        assert_eq!(
            folder_name(
                "c:\\users\\player\\appdata\\local\\roblox\\versions\\",
                "C:\\Users\\player\\AppData\\Local\\Roblox\\Versions\\version-1\\RobloxPlayerBeta.exe"
            )
            .as_deref(),
            Some("Roblox")
        );
        // Not under that root, or a program loose in the library folder.
        assert_eq!(folder_name(steam, "C:\\Windows\\notepad.exe"), None);
        assert_eq!(folder_name(steam, "D:\\SteamLibrary\\steamapps\\common\\x.exe"), None);
    }

    #[test]
    fn the_message_carries_the_name_only_while_playing() {
        let on = Activity {
            gaming: true,
            game: Some("VALORANT".into()),
        };
        assert_eq!(
            on.message(),
            json!({ "type": "activity", "gaming": true, "game": "VALORANT" })
        );
        let unnamed = Activity {
            gaming: true,
            game: None,
        };
        assert_eq!(
            unnamed.message(),
            json!({ "type": "activity", "gaming": true })
        );
        let off = Activity {
            gaming: false,
            game: Some("VALORANT".into()),
        };
        assert_eq!(off.message(), json!({ "type": "activity", "gaming": false }));
        let long = Activity {
            gaming: true,
            game: Some("x".repeat(500)),
        };
        assert_eq!(long.message()["game"].as_str().unwrap().len(), 60);
    }

    fn game(name: &str) -> Playing {
        Playing {
            name: Some(name.into()),
        }
    }

    #[test]
    fn playing_starts_after_five_seconds_in_a_game() {
        let mut d = Debounce::default();
        let g = game("VALORANT");
        assert_eq!(d.observe(0, Some(&g)), None);
        assert_eq!(d.observe(2_000, Some(&g)), None);
        assert_eq!(d.observe(4_999, Some(&g)), None);
        let on = d.observe(5_000, Some(&g)).expect("started");
        assert!(on.gaming);
        assert_eq!(on.game.as_deref(), Some("VALORANT"));
        // Said once, not on every reading after.
        assert_eq!(d.observe(7_000, Some(&g)), None);
    }

    #[test]
    fn a_quick_look_at_a_game_never_starts_it() {
        let mut d = Debounce::default();
        let g = game("VALORANT");
        d.observe(0, Some(&g));
        d.observe(4_000, None);
        // The five seconds start over.
        assert_eq!(d.observe(6_000, Some(&g)), None);
        assert_eq!(d.observe(10_000, Some(&g)), None);
        assert!(d.observe(11_000, Some(&g)).is_some());
    }

    #[test]
    fn alt_tabbing_away_for_a_while_doesnt_end_it() {
        let mut d = Debounce::default();
        let g = game("VALORANT");
        d.observe(0, Some(&g));
        d.observe(5_000, Some(&g));
        // Discord for 15 seconds, then back.
        assert_eq!(d.observe(10_000, None), None);
        assert_eq!(d.observe(25_000, None), None);
        assert_eq!(d.observe(27_000, Some(&g)), None);
        assert!(d.current().gaming);
        // Gone for good: over 20 seconds after the last reading in game.
        assert_eq!(d.observe(30_000, None), None);
        assert_eq!(d.observe(49_999, None), None);
        let off = d.observe(50_000, None).expect("ended");
        assert!(!off.gaming);
        assert_eq!(off.game, None);
    }

    #[test]
    fn learns_the_name_later_without_saying_it_again() {
        let mut d = Debounce::default();
        let unnamed = Playing { name: None };
        d.observe(0, Some(&unnamed));
        assert_eq!(d.observe(5_000, Some(&unnamed)).unwrap().game, None);
        assert_eq!(d.observe(7_000, Some(&game("Apex Legends"))), None);
        assert_eq!(d.current().game.as_deref(), Some("Apex Legends"));
    }

    #[test]
    fn turning_it_off_ends_it_at_once() {
        let mut d = Debounce::default();
        assert_eq!(d.reset(), None);
        let g = game("VALORANT");
        d.observe(0, Some(&g));
        d.observe(5_000, Some(&g));
        let off = d.reset().expect("ended");
        assert!(!off.gaming);
        assert_eq!(d.reset(), None);
    }

    #[test]
    fn a_clock_that_goes_back_doesnt_flip_it() {
        let mut d = Debounce::default();
        let g = game("VALORANT");
        d.observe(10_000, Some(&g));
        assert_eq!(d.observe(1_000, Some(&g)), None);
    }
}
