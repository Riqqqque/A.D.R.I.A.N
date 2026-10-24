//! Opening web pages, YouTube, Spotify searches and the usual folders.

use super::shell_open;
use crate::Outcome;
use serde_json::Value;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures,
    FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};

/// Percent-encode a search query for a URL.
pub fn encode(q: &str) -> String {
    let mut out = String::new();
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Only plain web pages: no file:, javascript: or app protocols from the model.
pub fn web_url(u: &str) -> Option<&str> {
    let lower = u.to_ascii_lowercase();
    let ok = (lower.starts_with("https://") || lower.starts_with("http://"))
        && !u
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"');
    ok.then_some(u)
}

/// A YouTube video id: 11 of [A-Za-z0-9_-].
pub fn video_id(s: &str) -> bool {
    s.len() == 11
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn folder(name: &str) -> Option<String> {
    let id = match name.to_lowercase().trim() {
        "downloads" | "download" => &FOLDERID_Downloads,
        "documents" | "docs" | "my documents" => &FOLDERID_Documents,
        "desktop" => &FOLDERID_Desktop,
        "pictures" | "photos" | "images" => &FOLDERID_Pictures,
        "music" => &FOLDERID_Music,
        "videos" | "video" => &FOLDERID_Videos,
        _ => return None,
    };
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

pub fn run(args: &Value) -> Outcome {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let (url, yt, video, dir) = (s("url"), s("youtube"), s("video_id"), s("folder"));
    // Spotify: the app's own search page (its spotify: link), or the web player without the app.
    let spotify = s("spotify");
    if !spotify.is_empty() {
        let q = encode(&spotify).replace('+', "%20");
        let said = format!("Spotify results for \u{201c}{spotify}\u{201d}");
        return match shell_open(&format!("spotify:search:{q}"), None)
            .or_else(|_| shell_open(&format!("https://open.spotify.com/search/{q}"), None))
        {
            Ok(()) => Outcome::ok(format!("Opened {said}.")),
            Err(e) => Outcome::no(e),
        };
    }
    let (target, said) = if !url.is_empty() {
        match web_url(&url) {
            Some(u) => (u.to_string(), "the page".to_string()),
            None => return Outcome::no("Only http and https web addresses can be opened."),
        }
    } else if video_id(&video) {
        (
            format!("https://www.youtube.com/watch?v={video}"),
            "the YouTube video".to_string(),
        )
    } else if !yt.is_empty() {
        (
            format!(
                "https://www.youtube.com/results?search_query={}",
                encode(&yt)
            ),
            format!("YouTube results for \u{201c}{yt}\u{201d}"),
        )
    } else if !dir.is_empty() {
        match folder(&dir) {
            Some(p) => (p, format!("the {} folder", dir.to_lowercase())),
            None => {
                return Outcome::no(
                    "Folder must be downloads, documents, desktop, pictures, music or videos.",
                );
            }
        }
    } else {
        return Outcome::no(
            "Give a url, a youtube search, a spotify search, a video_id or a folder.",
        );
    };
    match shell_open(&target, None) {
        Ok(()) => Outcome::ok(format!("Opened {said}.")),
        Err(e) => Outcome::no(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_inputs() {
        assert_eq!(encode("lofi beats & chill"), "lofi+beats+%26+chill");
        assert!(web_url("https://example.com/a?b=c").is_some());
        assert!(web_url("file:///C:/Windows/System32/cmd.exe").is_none());
        assert!(web_url("javascript:alert(1)").is_none());
        assert!(web_url("https://x.com/\" --evil").is_none());
        assert!(video_id("dQw4w9WgXcQ"));
        assert!(!video_id("dQw4w9WgXc&"));
    }
}
