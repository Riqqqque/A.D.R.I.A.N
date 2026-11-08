//! Opening web pages, YouTube, Spotify searches and the usual folders.

use super::shell_open;
use crate::Outcome;
use serde_json::Value;
use std::net::{IpAddr, ToSocketAddrs};
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

/// Longest address that opens. A long query string is how a page gets handed
/// what she knows.
const MAX_URL: usize = 300;

/// Last labels that never reach the public web.
const PRIVATE_SUFFIXES: [&str; 8] = [
    "localhost",
    "local",
    "lan",
    "home",
    "internal",
    "intranet",
    "corp",
    "arpa",
];

/// A name on the public web, from `host` or `host:port`. Only plain hostname
/// characters pass, so a user@ part, a backslash, an escape or a look-alike
/// digit can't make the browser go somewhere other than what this reads.
fn public_host(authority: &str) -> bool {
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        Some(_) => return false,
        None => authority,
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    let plain = host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let labels: Vec<&str> = host.split('.').collect();
    let last = labels[labels.len() - 1];
    // No top-level domain starts with a digit, but 127.0.0.1, 127.1 and 0x7f.0.0.1 do.
    plain
        && labels.len() > 1
        && labels.iter().all(|l| !l.is_empty())
        && !last.starts_with(|c: char| c.is_ascii_digit())
        && !PRIVATE_SUFFIXES.contains(&last)
}

/// Only plain web pages on the public web: no file:, javascript: or app
/// protocols from the model, and nothing on this PC or the LAN.
pub fn web_url(u: &str) -> Option<&str> {
    let lower = u.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    let ok = u.len() <= MAX_URL
        && !u
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"');
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    (ok && public_host(authority)).then_some(u)
}

/// An address on the public internet: not this PC, the LAN, a VPN's shared
/// range (Tailscale's 100.64/10) or link-local.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            !(v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || a == 0
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(v4) => public_ip(IpAddr::V4(v4)),
            None => {
                !(v.is_loopback()
                    || v.is_unspecified()
                    || v.is_unique_local()
                    || v.is_unicast_link_local())
            }
        },
    }
}

/// Whether every address the page's name points at is public. A name that
/// passes `web_url` can still lead home: 127.0.0.1.nip.io, localtest.me. A name
/// that doesn't resolve is refused too, since its answer can't be checked.
fn resolves_public(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let https = lower.starts_with("https://");
    let rest = lower.split_once("://").map_or(lower.as_str(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(0)),
        None => (authority, if https { 443 } else { 80 }),
    };
    match (host.trim_end_matches('.'), port).to_socket_addrs() {
        Ok(addrs) => {
            let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            !ips.is_empty() && ips.into_iter().all(public_ip)
        }
        Err(_) => false,
    }
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
            Some(u) if !resolves_public(u) => {
                return Outcome::no(
                    "That address leads to this PC or the home network, so it wasn't opened.",
                );
            }
            Some(u) => (u.to_string(), "the page".to_string()),
            None => {
                return Outcome::no("Only public http and https web addresses can be opened.");
            }
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
    fn only_public_addresses_count() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.50",
            "172.16.0.1",
            "169.254.1.1",
            "100.100.1.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "142.250.72.14", "100.128.0.1", "2606:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn a_name_for_this_pc_is_refused() {
        // localhost resolves without any network, in every setup.
        assert!(!resolves_public("http://localhost:8080/"));
    }

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

    #[test]
    fn opens_public_pages() {
        for u in [
            "https://example.com",
            "http://example.com/a?b=c#d",
            "HTTPS://Example.COM/Path",
            "https://www.reddit.com/r/pcgaming",
            "https://modrinth.com:443/mod/jei",
            "https://example.com./x",
            "https://xn--bcher-kva.example/",
            // Country and brand domains that happen to spell hex.
            "https://example.de/",
            "https://example.ca/",
            "https://example.cc/",
            "https://example.cafe/",
        ] {
            assert_eq!(web_url(u), Some(u), "{u}");
        }
    }

    #[test]
    fn refuses_this_pc_and_the_lan() {
        for u in [
            "http://localhost/",
            "http://localhost:3000/",
            "http://LOCALHOST/",
            "http://localhost./",
            "http://app.localhost/",
            "http://printer.local/",
            "http://printer.local./",
            "http://router.lan/",
            "http://nas.home/",
            "http://nas.home.arpa/",
            "http://db.internal/",
            "http://intranet/",
            "http://127.0.0.1/",
            "http://192.168.1.1/admin",
            "http://192.168.1.1:8080/",
            "http://8.8.8.8/",
            "http://127.1/",
            "http://2130706433/",
            "http://0x7f.0.0.1/",
            "http://0x7f000001/",
            "http://[::1]/",
            "http://[fe80::1]:8080/",
        ] {
            assert_eq!(web_url(u), None, "{u}");
        }
    }

    #[test]
    fn refuses_addresses_that_hide_their_host() {
        for u in [
            "https://steamcommunity.com@evil.example/",
            "https://user:pw@evil.example/",
            "https://evil.example\\@good.example/",
            "https://good.example\\.evil.example/",
            "https://evil.example%2f@good.example/",
            "http://127%2e0%2e0%2e1/",
            "http://a.loc%61l/",
            "http://１２７.０.０.１/",
            "https://exa_mple.com:80x/",
            "https://exa..mple.com/",
            "https:///example.com",
            "https://",
        ] {
            assert_eq!(web_url(u), None, "{u}");
        }
    }

    #[test]
    fn refuses_long_addresses() {
        let at_limit = format!("https://example.com/{}", "a".repeat(280));
        assert_eq!(at_limit.len(), 300);
        assert!(web_url(&at_limit).is_some());
        assert!(web_url(&format!("{at_limit}a")).is_none());
    }
}
