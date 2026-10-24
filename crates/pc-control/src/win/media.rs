//! Media keys without pressing keys: Windows' media sessions (the same thing
//! the volume flyout controls), which also say what's playing.

use super::Com;
use crate::Outcome;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};

/// "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify" or "Spotify.exe" → "Spotify";
/// "MSEdge" / "Chrome" stay as they are.
fn app_name(id: &str) -> String {
    let s = id.rsplit('!').next().unwrap_or(id);
    let s = s.strip_suffix(".exe").unwrap_or(s);
    let s = s.rsplit('.').next().unwrap_or(s);
    match s.to_lowercase().as_str() {
        "msedge" => "Edge".into(),
        "chrome" => "Chrome".into(),
        _ => s.to_string(),
    }
}

fn describe(s: &Session) -> windows::core::Result<String> {
    let app = app_name(&s.SourceAppUserModelId()?.to_string());
    let state = match s.GetPlaybackInfo()?.PlaybackStatus()? {
        Status::Playing => "playing",
        Status::Paused => "paused on",
        Status::Stopped => "stopped on",
        _ => "has",
    };
    let p = s.TryGetMediaPropertiesAsync()?.get()?;
    let (title, artist) = (p.Title()?.to_string(), p.Artist()?.to_string());
    Ok(match (title.is_empty(), artist.is_empty()) {
        (true, _) => format!(
            "{app} is {}.",
            if state == "playing" {
                "playing something"
            } else {
                "open but not playing"
            }
        ),
        (false, true) => format!("{app} is {state} \u{201c}{title}\u{201d}."),
        (false, false) => format!("{app} is {state} \u{201c}{title}\u{201d} by {artist}."),
    })
}

/// Browsers: YouTube, Netflix, Twitch and the like play inside one of these.
const BROWSERS: [&str; 8] = [
    "chrome", "edge", "firefox", "opera", "brave", "vivaldi", "arc", "browser",
];
const IN_BROWSER: [&str; 7] = [
    "youtube",
    "netflix",
    "twitch",
    "soundcloud",
    "hulu",
    "max",
    "prime video",
];

/// How well a player matches what they asked for ("spotify", "youtube", "the browser"); 0 = not it.
fn matches(asked: &str, app: &str, title: &str) -> u8 {
    let asked = asked.trim().to_lowercase();
    let asked = asked.strip_prefix("the ").unwrap_or(&asked);
    let app = app.to_lowercase();
    if asked.is_empty() {
        return 1;
    }
    if app == asked || app.starts_with(asked) || asked.starts_with(app.as_str()) {
        return 3;
    }
    let browser = BROWSERS.contains(&app.as_str());
    if browser && (asked == "browser" || IN_BROWSER.contains(&asked)) {
        // "YouTube": a browser tab, best of all the one whose title says so.
        return if title.to_lowercase().contains(asked) {
            3
        } else {
            2
        };
    }
    0
}

struct Player {
    s: Session,
    app: String,
    title: String,
    playing: bool,
}

fn players(mgr: &Manager) -> windows::core::Result<Vec<Player>> {
    let mut out = vec![];
    for s in mgr.GetSessions()? {
        let app = app_name(&s.SourceAppUserModelId()?.to_string());
        let playing = s
            .GetPlaybackInfo()
            .and_then(|i| i.PlaybackStatus())
            .map(|p| p == Status::Playing)
            .unwrap_or(false);
        let title = s
            .TryGetMediaPropertiesAsync()
            .and_then(|a| a.get())
            .and_then(|p| p.Title())
            .map(|t| t.to_string())
            .unwrap_or_default();
        out.push(Player {
            s,
            app,
            title,
            playing,
        });
    }
    Ok(out)
}

/// Which player an action is for. Named: that app (YouTube etc. means a browser).
/// Not named: pausing, skipping or stopping goes to what's actually playing;
/// playing goes to the one Windows shows (the last one used).
fn pick(list: Vec<Player>, current: Option<String>, action: &str, asked: &str) -> Option<Player> {
    let mut best: Option<(u8, Player)> = None;
    for p in list {
        let m = matches(asked, &p.app, &p.title);
        if m == 0 {
            continue;
        }
        let is_current = current.as_deref() == Some(p.app.as_str());
        let wants_playing = !matches!(action, "play");
        // Rank: how well the name fits, then playing (for pause/skip/stop/status) or current (for play).
        let score = m * 4
            + match (wants_playing, p.playing, is_current) {
                (true, true, _) => 3,
                (true, false, true) => 1,
                (false, _, true) => 3,
                (false, false, false) => 1,
                _ => 0,
            };
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, p));
        }
    }
    best.map(|(_, p)| p)
}

pub fn run(action: &str, app: &str) -> Outcome {
    let _com = Com::init();
    let result = (|| -> windows::core::Result<Outcome> {
        let mgr = Manager::RequestAsync()?.get()?;
        let list = players(&mgr)?;
        if list.is_empty() {
            return Ok(Outcome::no(
                "Nothing on this PC is playing media I can control.",
            ));
        }
        // What's playing, everywhere: every player, not just one.
        if action == "status" && app.trim().is_empty() && list.len() > 1 {
            let lines: Vec<String> = list.iter().filter_map(|p| describe(&p.s).ok()).collect();
            return Ok(Outcome::ok(lines.join(" ")));
        }
        let current = mgr
            .GetCurrentSession()
            .and_then(|s| s.SourceAppUserModelId())
            .map(|id| app_name(&id.to_string()))
            .ok();
        let Some(Player { s, .. }) = pick(list, current, action, app) else {
            return Ok(Outcome::no(format!(
                "{} isn't playing anything I can control. Open it and start something first.",
                app.trim()
            )));
        };
        let done = match action {
            "status" => return Ok(Outcome::ok(describe(&s)?)),
            "play_pause" | "pause" | "play" => match action {
                "pause" => s.TryPauseAsync()?.get()?,
                "play" => s.TryPlayAsync()?.get()?,
                _ => s.TryTogglePlayPauseAsync()?.get()?,
            },
            "next" => s.TrySkipNextAsync()?.get()?,
            "previous" => s.TrySkipPreviousAsync()?.get()?,
            "stop" => s.TryStopAsync()?.get()?,
            _ => {
                return Ok(Outcome::no(
                    "Media action must be play_pause, play, pause, next, previous, stop or status.",
                ));
            }
        };
        if !done {
            return Ok(Outcome::no(format!(
                "{} didn't accept that.",
                app_name(&s.SourceAppUserModelId()?.to_string())
            )));
        }
        // Give the app a moment to switch tracks before reading what's on.
        std::thread::sleep(std::time::Duration::from_millis(
            if action == "next" || action == "previous" {
                700
            } else {
                300
            },
        ));
        Ok(Outcome::ok(describe(&s).unwrap_or_else(|_| "Done.".into())))
    })();
    result.unwrap_or_else(|e| Outcome::no(format!("Windows media error: {}", e.message())))
}

#[cfg(test)]
mod tests {
    use super::{app_name, matches};

    #[test]
    fn names_apps() {
        assert_eq!(
            app_name("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"),
            "Spotify"
        );
        assert_eq!(app_name("Spotify.exe"), "Spotify");
        assert_eq!(app_name("MSEdge"), "Edge");
        assert_eq!(app_name("Chrome"), "Chrome");
    }

    #[test]
    fn matches_what_they_asked_for() {
        assert_eq!(matches("spotify", "Spotify", ""), 3);
        assert_eq!(matches("the spotify", "Spotify", ""), 3);
        assert_eq!(matches("youtube", "Chrome", "lofi hip hop - YouTube"), 3);
        assert_eq!(matches("youtube", "Edge", "Some video"), 2);
        assert_eq!(matches("youtube", "Spotify", ""), 0);
        assert_eq!(matches("spotify", "Chrome", ""), 0);
        assert_eq!(matches("", "Spotify", ""), 1);
    }
}
