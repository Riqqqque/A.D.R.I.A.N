//! Settings, kept in %APPDATA%\adrian\desktop\config\config.json. The pairing
//! token is encrypted with Windows DPAPI, so only this Windows user can read it.

use adrian_pc_control::{Policy, Shortcut};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct Settings {
    /// e.g. ws://homeserver:8791 (the path /satellite is added).
    pub server_url: String,
    /// The pairing token, DPAPI-encrypted, as hex.
    pub token_protected: String,
    /// Mic loudness needed to count as talking (0.005-0.1). 0: automatic.
    pub mic_threshold: f32,
    pub pc: Policy,
    /// Don't install new versions by themselves (only say there is one).
    pub updates_off: bool,
    /// Don't tell the server when a game is being played (so no quiet mode).
    pub quiet_off: bool,
}

pub fn path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("ai", "adrian", "desktop")?;
    Some(dirs.config_dir().join("config.json"))
}

pub fn load() -> Settings {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(s: &Settings) -> Result<(), String> {
    let p = path().ok_or("no settings folder")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

/// What the settings page sends on Save.
pub struct Form {
    pub server_url: String,
    pub token: Option<String>,
    pub mic_threshold: f32,
    pub pc: Policy,
    /// The shortcuts box, as typed. It replaces whatever shortcuts `pc` holds.
    pub shortcuts: String,
    pub auto_update: Option<bool>,
    /// "Quiet while I play". None from a page that doesn't have the switch.
    pub quiet_while_playing: Option<bool>,
}

/// What a Save changed that needs acting on.
pub struct Saved {
    /// The server address or the token is different now.
    pub connection: bool,
    /// Automatic updates were off and are on now.
    pub updates_on: bool,
}

impl Saved {
    /// Whether to connect again. A reconnect drops whatever she's in the middle
    /// of, so not while she's connected on an unchanged connection. Without a
    /// connection any Save is worth a try, even with the same token.
    pub fn reconnect(&self, connected: bool) -> bool {
        self.connection || !connected
    }
}

/// Save `form` over `s`. All or nothing: it's checked and written (by `write`)
/// on a copy, and `s` only changes once that worked.
pub fn commit(
    s: &mut Settings,
    form: Form,
    write: impl FnOnce(&Settings) -> Result<(), String>,
) -> Result<Saved, String> {
    if normalize_url(&form.server_url).is_none() {
        return Err("Enter the server address, like homeserver:8791.".into());
    }
    let mut next = s.clone();
    next.server_url = form.server_url.trim().to_string();
    if let Some(t) = form
        .token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        if !t.starts_with("sat_") {
            return Err("That doesn't look like a pairing token (they start with sat_).".into());
        }
        next.set_token(&t)?;
    }
    next.mic_threshold = form.mic_threshold.clamp(0.0, 0.1);
    next.pc = Policy {
        shortcuts: parse_shortcuts(&form.shortcuts)?,
        ..form.pc
    };
    if let Some(on) = form.auto_update {
        next.updates_off = !on;
    }
    if let Some(on) = form.quiet_while_playing {
        next.quiet_off = !on;
    }
    write(&next)?;
    let saved = Saved {
        connection: (next.url(), next.token()) != (s.url(), s.token()),
        updates_on: s.updates_off && !next.updates_off,
    };
    *s = next;
    Ok(saved)
}

/// The shortcuts as lines for the settings box. With arguments the target is
/// quoted, so `parse_shortcuts` can tell them from spaces in a path.
pub fn shortcut_lines(list: &[Shortcut]) -> String {
    let line = |s: &Shortcut| {
        if s.args.is_empty() {
            format!("{} = {}", s.name, s.target)
        } else {
            format!("{} = \"{}\" {}", s.name, s.target, s.args)
        }
    };
    list.iter().map(line).collect::<Vec<_>>().join("\n")
}

/// The settings box back into shortcuts: `name = target`, or
/// `name = "target" args`. A line it can't read is an error that names the
/// line, never something to drop.
pub fn parse_shortcuts(text: &str) -> Result<Vec<Shortcut>, String> {
    let unreadable = |line: &str| {
        let shown: String = line.chars().take(60).collect();
        format!("Can't read this shortcut: {shown}. Use name = target.")
    };
    let mut list = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // Spaces around the = win, so a name or a URL can hold an = of its own.
        let (name, rest) = line
            .split_once(" = ")
            .or_else(|| line.split_once('='))
            .ok_or_else(|| unreadable(line))?;
        let (name, rest) = (name.trim(), rest.trim());
        let (target, args) = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').ok_or_else(|| unreadable(line))?,
            None => (rest, ""),
        };
        let target = target.trim();
        if name.is_empty() || target.is_empty() {
            return Err(unreadable(line));
        }
        list.push(Shortcut {
            name: name.to_string(),
            target: target.to_string(),
            args: args.trim().to_string(),
        });
    }
    Ok(list)
}

impl Settings {
    pub fn token(&self) -> Option<String> {
        let bytes = from_hex(&self.token_protected)?;
        String::from_utf8(dpapi::unprotect(&bytes)?).ok()
    }

    pub fn set_token(&mut self, token: &str) -> Result<(), String> {
        self.token_protected =
            to_hex(&dpapi::protect(token.as_bytes()).ok_or("Windows couldn't encrypt the token")?);
        Ok(())
    }

    /// The WebSocket URL to dial: scheme and /satellite path filled in.
    pub fn url(&self) -> Option<String> {
        normalize_url(&self.server_url)
    }
}

pub fn normalize_url(raw: &str) -> Option<String> {
    let t = raw.trim().trim_end_matches('/');
    if t.is_empty() || t.chars().any(char::is_whitespace) {
        return None;
    }
    let with_scheme = if t.starts_with("ws://") || t.starts_with("wss://") {
        t.to_string()
    } else if let Some(rest) = t.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = t.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        format!("ws://{t}")
    };
    let after = with_scheme.split_once("://").map(|(_, r)| r).unwrap_or("");
    Some(if after.contains('/') {
        with_scheme
    } else {
        format!("{with_scheme}/satellite")
    })
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(windows)]
mod dpapi {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
    };

    fn run(data: &[u8], protect: bool) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            let r = if protect {
                CryptProtectData(&input, None, None, None, None, 0, &mut out)
            } else {
                CryptUnprotectData(&input, None, None, None, None, 0, &mut out)
            };
            r.ok()?;
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(HLOCAL(out.pbData.cast()));
            Some(v)
        }
    }

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, true)
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, false)
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
    pub fn unprotect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_in_the_url() {
        assert_eq!(
            normalize_url("homeserver:8791").as_deref(),
            Some("ws://homeserver:8791/satellite")
        );
        assert_eq!(
            normalize_url("https://adrian.example.com/").as_deref(),
            Some("wss://adrian.example.com/satellite")
        );
        assert_eq!(
            normalize_url("ws://10.0.0.5:8791/custom").as_deref(),
            Some("ws://10.0.0.5:8791/custom")
        );
        assert_eq!(normalize_url("  "), None);
        assert_eq!(normalize_url("bad host"), None);
    }

    fn on_server_a() -> Settings {
        Settings {
            server_url: "a:8791".into(),
            ..Default::default()
        }
    }

    fn form(server_url: &str) -> Form {
        Form {
            server_url: server_url.into(),
            token: None,
            mic_threshold: 0.0,
            pc: Policy::default(),
            shortcuts: String::new(),
            auto_update: None,
            quiet_while_playing: None,
        }
    }

    #[test]
    fn a_refused_token_changes_nothing() {
        let mut s = on_server_a();
        let bad = Form {
            token: Some("not-a-token".into()),
            mic_threshold: 0.05,
            ..form("b:8791")
        };
        assert!(commit(&mut s, bad, |_| panic!("nothing to write")).is_err());
        assert_eq!(s.server_url, "a:8791");
        assert_eq!(s.mic_threshold, 0.0);
    }

    #[test]
    fn a_failed_write_changes_nothing() {
        let mut s = on_server_a();
        let off = Form {
            pc: Policy {
                shutdown: false,
                ..Policy::default()
            },
            auto_update: Some(false),
            ..form("b:8791")
        };
        assert!(commit(&mut s, off, |_| Err("disk full".into())).is_err());
        assert_eq!(s.server_url, "a:8791");
        assert!(s.pc.shutdown);
        assert!(!s.updates_off);
    }

    #[test]
    fn quiet_while_playing_is_on_unless_turned_off() {
        let mut s = on_server_a();
        assert!(!s.quiet_off);
        // A page without the switch leaves it as it was.
        commit(&mut s, form("a:8791"), |_| Ok(())).unwrap();
        assert!(!s.quiet_off);
        let off = Form {
            quiet_while_playing: Some(false),
            ..form("a:8791")
        };
        commit(&mut s, off, |_| Ok(())).unwrap();
        assert!(s.quiet_off);
        // Older settings files don't have it: on.
        let old: Settings = serde_json::from_str(r#"{"server_url":"a:8791"}"#).unwrap();
        assert!(!old.quiet_off);
    }

    #[test]
    fn a_good_save_changes_everything_and_says_what_moved() {
        let mut s = on_server_a();
        s.updates_off = true;
        let good = Form {
            mic_threshold: 0.05,
            auto_update: Some(true),
            ..form("b:8791")
        };
        let saved = commit(&mut s, good, |next| {
            assert_eq!(next.server_url, "b:8791");
            Ok(())
        })
        .unwrap();
        assert!(saved.connection && saved.updates_on);
        assert_eq!(s.server_url, "b:8791");
        assert_eq!(s.mic_threshold, 0.05);
        // The same again moves nothing.
        let saved = commit(&mut s, form("b:8791"), |_| Ok(())).unwrap();
        assert!(!saved.connection && !saved.updates_on);
    }

    #[test]
    fn saving_with_no_connection_tries_again_even_when_nothing_changed() {
        let saved = Saved {
            connection: false,
            updates_on: false,
        };
        assert!(saved.reconnect(false));
        // Connected, it would only drop what she's in the middle of.
        assert!(!saved.reconnect(true));
    }

    #[test]
    fn an_unreadable_shortcut_is_named_and_stops_the_save() {
        for bad in [
            "studio C:\\x.lnk",
            "= C:\\x.lnk",
            "studio =",
            "studio = \"C:\\no closing quote.exe --flag",
            "studio = \"\"",
        ] {
            let e = parse_shortcuts(&format!("ok = C:\\ok.lnk\n{bad}\n")).unwrap_err();
            assert!(e.contains(bad.trim()), "{e}");
        }
        let mut s = on_server_a();
        s.pc.shortcuts = parse_shortcuts("kept = C:\\kept.lnk").unwrap();
        let bad = Form {
            shortcuts: "studio C:\\x.lnk".into(),
            ..form("a:8791")
        };
        assert!(commit(&mut s, bad, |_| panic!("nothing to write")).is_err());
        assert_eq!(s.pc.shortcuts.len(), 1);
    }

    #[test]
    fn shortcuts_keep_their_args_through_a_save() {
        let before = vec![
            Shortcut {
                name: "with args".into(),
                target: "C:\\Program Files\\x.exe".into(),
                args: "--foo --bar".into(),
            },
            Shortcut {
                name: "spaces".into(),
                target: "C:\\Program Files\\y.exe".into(),
                args: String::new(),
            },
            Shortcut {
                name: "site".into(),
                target: "https://example.com/?a=b".into(),
                args: String::new(),
            },
        ];
        let after = parse_shortcuts(&shortcut_lines(&before)).unwrap();
        assert_eq!(after.len(), before.len());
        for (a, b) in after.iter().zip(&before) {
            assert_eq!((&a.name, &a.target, &a.args), (&b.name, &b.target, &b.args));
        }
    }

    #[test]
    fn reads_shortcuts_as_typed() {
        let list = parse_shortcuts("\n a=b \n\nquoted = \"C:\\p q\\x.lnk\"\n").unwrap();
        assert_eq!((list[0].name.as_str(), list[0].target.as_str()), ("a", "b"));
        assert_eq!(list[1].target, "C:\\p q\\x.lnk");
        assert_eq!(list[1].args, "");
    }

    #[cfg(windows)]
    #[test]
    fn keeps_the_token_encrypted() {
        let mut s = Settings::default();
        s.set_token("sat_secret").unwrap();
        assert!(!s.token_protected.contains(&to_hex(b"sat_secret")));
        assert_eq!(s.token().as_deref(), Some("sat_secret"));
    }
}
