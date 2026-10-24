//! Everything that can be opened by name: Start menu apps (desktop and
//! Store), Steam, Epic and Riot games, a few Windows tools, and the PC's own
//! shortcuts.

use super::shell_open;
use crate::matching::{Pick, alias, pick};
use crate::{Outcome, Policy};
use serde::Deserialize;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{PCWSTR, w};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Shortcut,
    Windows,
    Game,
    App,
}

#[derive(Clone, Debug)]
pub enum Launch {
    /// A Start menu entry by its AppID, through shell:AppsFolder.
    AppsFolder(String),
    /// A file or program with arguments.
    File(String, String),
    /// A URL or URI (steam://, ms-settings:).
    Uri(String),
}

#[derive(Clone, Debug)]
pub struct App {
    pub name: String,
    pub kind: Kind,
    pub launch: Launch,
}

#[derive(Clone, Debug)]
pub struct Index {
    pub apps: Vec<App>,
    /// Lowercase folders games are installed in: their windows are never touched.
    pub game_roots: Vec<String>,
    pub built: Instant,
}

impl Index {
    /// Whether a program lives in a game's install folder.
    pub fn is_game(&self, exe_path: &str) -> bool {
        let p = exe_path.to_lowercase().replace('/', "\\");
        self.game_roots.iter().any(|r| p.starts_with(r.as_str()))
    }
}

/// Start menu entries that are never what anyone means.
fn junk(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "uninstall",
        "readme",
        "read me",
        "release notes",
        "documentation",
        "help",
        "license",
        "website",
        "support",
        "manual",
        "changelog",
    ]
    .iter()
    .any(|j| n.contains(j))
}

pub fn build(policy: &Policy) -> Index {
    let mut apps = Vec::new();
    let mut game_roots = Vec::new();
    // Order matters: on a name tie the earlier source wins.
    for s in &policy.shortcuts {
        let launch = if s.target.contains("://") {
            Launch::Uri(s.target.clone())
        } else {
            Launch::File(s.target.clone(), s.args.clone())
        };
        apps.push(App {
            name: s.name.clone(),
            kind: Kind::Shortcut,
            launch,
        });
    }
    for (name, launch) in [
        (
            "File Explorer",
            Launch::File("explorer.exe".into(), String::new()),
        ),
        ("Settings", Launch::Uri("ms-settings:".into())),
        (
            "Task Manager",
            Launch::File("taskmgr.exe".into(), String::new()),
        ),
        (
            "Control Panel",
            Launch::File("control.exe".into(), String::new()),
        ),
    ] {
        apps.push(App {
            name: name.into(),
            kind: Kind::Windows,
            launch,
        });
    }
    riot(&mut apps, &mut game_roots);
    steam(&mut apps, &mut game_roots);
    epic(&mut apps, &mut game_roots);
    start_apps(&mut apps);
    Index {
        apps,
        game_roots,
        built: Instant::now(),
    }
}

#[derive(Deserialize)]
struct StartApp {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "AppID")]
    app_id: String,
}

/// Every Start menu entry, desktop and Store apps alike (`Get-StartApps`).
fn start_apps(apps: &mut Vec<App>) {
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-StartApps | ConvertTo-Json -Compress",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let Ok(out) = out else { return };
    let text = String::from_utf8_lossy(&out.stdout);
    let list: Vec<StartApp> = serde_json::from_str(&text)
        .or_else(|_| serde_json::from_str::<StartApp>(&text).map(|a| vec![a]))
        .unwrap_or_default();
    for a in list {
        if !a.name.trim().is_empty() && !junk(&a.name) {
            apps.push(App {
                name: a.name,
                kind: Kind::App,
                launch: Launch::AppsFolder(a.app_id),
            });
        }
    }
}

fn reg_string(key: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    r.is_ok().then(|| super::from_wide(&buf))
}

/// Values of `"key"  "value"` lines in Valve's KeyValues text.
pub fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(needle.as_str()) else {
            continue;
        };
        let rest = rest.trim();
        if let Some(v) = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            out.push(v.replace("\\\\", "\\"));
        }
    }
    out
}

/// Installed Steam games, launched through steam://rungameid.
fn steam(apps: &mut Vec<App>, roots: &mut Vec<String>) {
    let base = reg_string(w!("Software\\Valve\\Steam"), w!("SteamPath"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Program Files (x86)\\Steam"));
    let Ok(lib) = std::fs::read_to_string(base.join("steamapps").join("libraryfolders.vdf")) else {
        return;
    };
    let mut libraries = vdf_values(&lib, "path");
    if libraries.is_empty() {
        libraries.push(base.to_string_lossy().into_owned());
    }
    for l in libraries {
        let apps_dir = Path::new(&l).join("steamapps");
        roots.push(apps_dir.join("common").to_string_lossy().to_lowercase());
        let Ok(entries) = std::fs::read_dir(&apps_dir) else {
            continue;
        };
        for e in entries.flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            if !(file.starts_with("appmanifest_") && file.ends_with(".acf")) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path()) else {
                continue;
            };
            let (Some(id), Some(name)) = (
                vdf_values(&text, "appid").into_iter().next(),
                vdf_values(&text, "name").into_iter().next(),
            ) else {
                continue;
            };
            let lower = name.to_lowercase();
            if [
                "redistributable",
                "steamworks",
                "proton",
                "steam linux runtime",
            ]
            .iter()
            .any(|j| lower.contains(j))
            {
                continue;
            }
            apps.push(App {
                name,
                kind: Kind::Game,
                launch: Launch::Uri(format!("steam://rungameid/{id}")),
            });
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EpicItem {
    display_name: String,
    #[serde(default)]
    catalog_namespace: String,
    #[serde(default)]
    catalog_item_id: String,
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    install_location: String,
    #[serde(default)]
    b_is_incomplete_install: bool,
}

/// Installed Epic Games Store games, launched through the Epic launcher URI.
fn epic(apps: &mut Vec<App>, roots: &mut Vec<String>) {
    let Ok(entries) =
        std::fs::read_dir("C:\\ProgramData\\Epic\\EpicGamesLauncher\\Data\\Manifests")
    else {
        return;
    };
    for e in entries.flatten() {
        if e.path().extension().and_then(|x| x.to_str()) != Some("item") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(item) = serde_json::from_str::<EpicItem>(&text) else {
            continue;
        };
        if item.b_is_incomplete_install || item.app_name.is_empty() {
            continue;
        }
        if !item.install_location.is_empty() {
            roots.push(item.install_location.to_lowercase().replace('/', "\\"));
        }
        let uri = format!(
            "com.epicgames.launcher://apps/{}%3A{}%3A{}?action=launch&silent=true",
            item.catalog_namespace, item.catalog_item_id, item.app_name
        );
        apps.push(App {
            name: item.display_name,
            kind: Kind::Game,
            launch: Launch::Uri(uri),
        });
    }
}

/// VALORANT and League of Legends through the Riot Client.
fn riot(apps: &mut Vec<App>, roots: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string("C:\\ProgramData\\Riot Games\\RiotClientInstalls.json")
    else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    let Some(client) = v.get("rc_default").and_then(|c| c.as_str()) else {
        return;
    };
    for (meta, name, product, folder) in [
        ("valorant.live", "VALORANT", "valorant", "VALORANT"),
        (
            "league_of_legends.live",
            "League of Legends",
            "league_of_legends",
            "League of Legends",
        ),
    ] {
        if Path::new("C:\\ProgramData\\Riot Games\\Metadata")
            .join(meta)
            .exists()
        {
            apps.push(App {
                name: name.into(),
                kind: Kind::Game,
                launch: Launch::File(
                    client.replace('/', "\\"),
                    format!("--launch-product={product} --launch-patchline=live"),
                ),
            });
            // The client sits next to the games: protect each game's own folder.
            if let Some(base) = Path::new(&client.replace('/', "\\"))
                .parent()
                .and_then(|p| p.parent())
            {
                roots.push(base.join(folder).to_string_lossy().to_lowercase());
            }
        }
    }
    // Vanguard itself.
    roots.push("c:\\program files\\riot vanguard".into());
}

fn launch(app: &App) -> Result<(), String> {
    match &app.launch {
        Launch::AppsFolder(id) => {
            shell_open("explorer.exe", Some(&format!("shell:AppsFolder\\{id}")))
        }
        Launch::Uri(u) => shell_open(u, None),
        Launch::File(f, args) => {
            let lower = f.to_lowercase();
            if lower.ends_with(".ps1") {
                // A double-clicked .ps1 opens in an editor; run it instead.
                let a = format!("-NoProfile -ExecutionPolicy Bypass -File \"{f}\" {args}");
                shell_open("powershell.exe", Some(a.trim()))
            } else {
                shell_open(f, (!args.is_empty()).then_some(args.as_str()))
            }
        }
    }
}

fn describe(app: &App) -> &'static str {
    match app.kind {
        Kind::Game => "game",
        Kind::Shortcut => "shortcut",
        Kind::Windows | Kind::App => "app",
    }
}

/// Find an app by spoken name, trying the common nickname too.
fn find(index: &Index, name: &str) -> Pick<App> {
    let direct = pick(name, &index.apps, |a| a.name.as_str());
    match (&direct, alias(name)) {
        (Pick::One(_), _) | (_, None) => direct,
        (_, Some(a)) => match pick(a, &index.apps, |a| a.name.as_str()) {
            Pick::None => direct,
            other => other,
        },
    }
}

/// What `open` would start, without starting it.
pub fn which(index: &Index, name: &str) -> Outcome {
    match find(index, name) {
        Pick::One(app) => Outcome::ok(format!(
            "{} ({}) via {:?}",
            app.name,
            describe(&app),
            app.launch
        )),
        Pick::Many(list) => Outcome::no(format!(
            "ask: {}",
            list.iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Pick::None => Outcome::no("nothing"),
    }
}

pub fn open(index: &Index, name: &str) -> Outcome {
    if name.is_empty() {
        return Outcome::no("Say which app or game to open.");
    }
    match find(index, name) {
        Pick::One(app) => match launch(&app) {
            Ok(()) => Outcome::ok(format!("Opening {} ({}).", app.name, describe(&app))),
            Err(e) => Outcome::no(format!("Couldn't open {}: {e}", app.name)),
        },
        Pick::Many(list) => Outcome::no(format!(
            "Several match \"{name}\": {}. Ask which one.",
            list.iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Pick::None => Outcome::no(format!(
            "Nothing called \"{name}\" is installed on this PC."
        )),
    }
}

/// Installed apps and games matching a filter (or the games, with no filter).
pub fn list(index: &Index, filter: &str) -> Outcome {
    let names: Vec<&str> = if filter.is_empty() {
        index
            .apps
            .iter()
            .filter(|a| a.kind == Kind::Game)
            .map(|a| a.name.as_str())
            .collect()
    } else {
        let f = crate::matching::normalize(alias(filter).unwrap_or(filter));
        index
            .apps
            .iter()
            .filter(|a| crate::matching::score(&f, &crate::matching::normalize(&a.name)) > 0)
            .map(|a| a.name.as_str())
            .collect()
    };
    let mut names: Vec<&str> = names;
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return Outcome::ok("None found.");
    }
    let shown: Vec<&str> = names.iter().take(40).copied().collect();
    Outcome::ok(format!(
        "{} found: {}{}",
        names.len(),
        shown.join(", "),
        if names.len() > 40 { ", and more" } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::{Index, vdf_values};

    #[test]
    fn knows_game_folders() {
        let index = Index {
            apps: Vec::new(),
            game_roots: vec![
                "d:\\steamlibrary\\steamapps\\common".into(),
                "c:\\riot games\\valorant".into(),
            ],
            built: std::time::Instant::now(),
        };
        assert!(index.is_game("D:\\SteamLibrary\\steamapps\\common\\Apex Legends\\r5apex.exe"));
        assert!(index.is_game("C:/Riot Games/VALORANT/live/VALORANT.exe"));
        assert!(!index.is_game("C:\\Riot Games\\Riot Client\\RiotClientServices.exe"));
        assert!(!index.is_game("C:\\Program Files\\Spotify\\Spotify.exe"));
    }

    #[test]
    fn reads_steam_files() {
        let lib = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}";
        assert_eq!(
            vdf_values(lib, "path"),
            vec!["C:\\Program Files (x86)\\Steam", "D:\\SteamLibrary"]
        );
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n}";
        assert_eq!(vdf_values(acf, "appid"), vec!["730"]);
        assert_eq!(vdf_values(acf, "name"), vec!["Counter-Strike 2"]);
    }
}
