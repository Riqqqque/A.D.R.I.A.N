//! Matching spoken names ("vs code", "the steam app") to app and window names.

/// Lowercase, `&` → "and", only letters/digits/spaces, single spaces, no
/// leading "the" or trailing "app".
pub fn normalize(s: &str) -> String {
    let s = s
        .to_lowercase()
        .replace('&', " and ")
        .replace('+', " plus ");
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c);
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    let mut out = out.trim().to_string();
    for lead in ["the ", "my "] {
        if let Some(rest) = out.strip_prefix(lead) {
            out = rest.to_string();
        }
    }
    for tail in [" app", " application", " program"] {
        if let Some(rest) = out.strip_suffix(tail) {
            out = rest.to_string();
        }
    }
    out
}

fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn acronym(s: &str) -> String {
    s.split(' ').filter_map(|w| w.chars().next()).collect()
}

/// How well a spoken query names this item, 0 (not at all) to 100 (exactly).
/// Both sides must already be normalized.
pub fn score(query: &str, name: &str) -> u32 {
    if query.is_empty() || name.is_empty() {
        return 0;
    }
    if query == name {
        return 100;
    }
    let (qc, nc) = (compact(query), compact(name));
    if qc == nc {
        return 95;
    }
    let nw: Vec<&str> = name.split(' ').collect();
    let qw: Vec<&str> = query
        .split(' ')
        .filter(|q| !["with", "and", "plus", "on", "for"].contains(q))
        .collect();
    if qw.is_empty() {
        return 0;
    }
    // "visual studio" → "Visual Studio Code"; whole leading words
    if name.starts_with(query) && name.as_bytes().get(query.len()) == Some(&b' ') {
        return 85;
    }
    // every spoken word starts a word of the name, in order: "vis stu code",
    // "fl studio with auto tune" (little words like "with" don't count)
    let mut at = 0;
    let in_order = qw.iter().all(|q| {
        while at < nw.len() {
            at += 1;
            if nw[at - 1].starts_with(q) {
                return true;
            }
        }
        false
    });
    if in_order {
        return 75;
    }
    if nc.contains(&qc) && qc.len() >= 3 {
        return 65;
    }
    if qc.len() >= 2 && acronym(name) == qc {
        return 60;
    }
    // the name inside a longer request: "open spotify music"
    if nc.len() >= 4 && qc.contains(&nc) {
        return 40;
    }
    0
}

/// The outcome of looking a name up.
#[derive(Debug, PartialEq)]
pub enum Pick<T> {
    One(T),
    /// Several equally good matches: ask which one.
    Many(Vec<T>),
    None,
}

/// The best item for a query. Items with the same normalized name count once
/// (the first wins), so the user and all-users Start menus don't make a tie.
pub fn pick<T: Clone>(query: &str, items: &[T], name: impl Fn(&T) -> &str) -> Pick<T> {
    let q = normalize(query);
    let mut scored: Vec<(u32, String, &T)> = Vec::new();
    for it in items {
        let n = normalize(name(it));
        let s = score(&q, &n);
        if s > 0 && !scored.iter().any(|(_, seen, _)| *seen == n) {
            scored.push((s, n, it));
        }
    }
    let Some(top) = scored.iter().map(|(s, _, _)| *s).max() else {
        return Pick::None;
    };
    let mut best: Vec<(u32, String, &T)> =
        scored.into_iter().filter(|(s, _, _)| *s == top).collect();
    if best.len() == 1 {
        return Pick::One(best.remove(0).2.clone());
    }
    // "FL Studio" among "FL Studio 2025", "FL Studio 2026" and
    // "FL Studio 2026 + Auto-Tune": the newest plain version. Variants with
    // extra words are only picked when asked for by name.
    let versioned: Vec<(Vec<u64>, &T)> = best
        .iter()
        .filter_map(|(_, n, it)| {
            let rest = n.strip_prefix(q.as_str())?.trim();
            let parts: Option<Vec<u64>> = rest.split(' ').map(|p| p.parse().ok()).collect();
            parts.filter(|p| !p.is_empty()).map(|p| (p, *it))
        })
        .collect();
    if let Some((_, it)) = versioned.into_iter().max_by(|a, b| a.0.cmp(&b.0)) {
        return Pick::One(it.clone());
    }
    // A tie: list them, shortest names first, and let the person choose.
    best.sort_by_key(|(_, n, _)| n.len());
    Pick::Many(
        best.into_iter()
            .take(6)
            .map(|(_, _, t)| t.clone())
            .collect(),
    )
}

/// Spoken names for common things that the Start menu calls something else.
pub fn alias(query: &str) -> Option<&'static str> {
    Some(match normalize(query).as_str() {
        "chrome" => "google chrome",
        "firefox" => "mozilla firefox",
        "edge" => "microsoft edge",
        "vs code" | "vscode" | "code" => "visual studio code",
        "teams" => "microsoft teams",
        "store" | "app store" | "microsoft store" => "microsoft store",
        "cmd" | "command prompt" | "terminal" | "powershell" => "terminal",
        "calc" => "calculator",
        "obs" => "obs studio",
        "league" | "lol" => "league of legends",
        "val" => "valorant",
        "r6" | "siege" | "rainbow 6" => "rainbow six siege",
        "cs" | "cs2" | "counter strike" => "counter strike 2",
        "ow" | "ow2" => "overwatch",
        "cod" => "call of duty",
        "gta" | "gta 5" | "gta v" => "grand theft auto v",
        "mc" => "minecraft",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize("  The Steam App "), "steam");
        assert_eq!(normalize("Tom & Jerry+"), "tom and jerry plus");
        assert_eq!(
            normalize("Visual Studio Code (User)"),
            "visual studio code user"
        );
    }

    #[test]
    fn scores_in_order() {
        let n = |s: &str| normalize(s);
        assert_eq!(score(&n("discord"), &n("Discord")), 100);
        assert_eq!(score(&n("obsstudio"), &n("OBS Studio")), 95);
        assert_eq!(score(&n("visual studio"), &n("Visual Studio Code")), 85);
        assert_eq!(score(&n("vis stu code"), &n("Visual Studio Code")), 75);
        assert_eq!(score(&n("tify"), &n("Spotify")), 65);
        assert_eq!(score(&n("vsc"), &n("Visual Studio Code")), 60);
        assert_eq!(score(&n("open spotify music"), &n("Spotify")), 40);
        assert_eq!(score(&n("zzz"), &n("Spotify")), 0);
    }

    #[test]
    fn picks_one_many_or_none() {
        let apps = [
            "Discord",
            "Discord PTB",
            "Spotify",
            "Steam",
            "Steam VR",
            "Spotify",
        ];
        assert_eq!(pick("discord", &apps, |s| s), Pick::One("Discord"));
        assert_eq!(pick("spotify", &apps, |s| s), Pick::One("Spotify")); // duplicate counted once
        assert_eq!(
            pick("st", &apps, |s| s),
            Pick::Many(vec!["Steam", "Steam VR"])
        );
        assert_eq!(pick("nothing like it", &apps, |s| s), Pick::None);
        assert_eq!(
            pick("disc", &apps, |s| s),
            Pick::Many(vec!["Discord", "Discord PTB"])
        );
    }

    #[test]
    fn prefers_the_newest_plain_version() {
        let apps = [
            "FL Studio 2025",
            "FL Studio 2026 + Auto-Tune",
            "FL Studio 2026",
        ];
        assert_eq!(pick("fl studio", &apps, |s| s), Pick::One("FL Studio 2026"));
        assert_eq!(
            pick("fl studio with auto tune", &apps, |s| s),
            Pick::One("FL Studio 2026 + Auto-Tune")
        );
        assert_eq!(
            pick("fl studio 2025", &apps, |s| s),
            Pick::One("FL Studio 2025")
        );
    }

    #[test]
    fn aliases() {
        assert_eq!(alias("VS Code"), Some("visual studio code"));
        assert_eq!(alias("the chrome app"), Some("google chrome"));
        assert_eq!(alias("notepad"), None);
    }
}
