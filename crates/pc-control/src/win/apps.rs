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
    /// Whether a program is a game: one known by its name (wherever it's
    /// installed), or one in a game's install folder.
    pub fn is_game(&self, exe_path: &str) -> bool {
        if crate::activity::known_program(&crate::activity::exe_stem(exe_path)).is_some() {
            return true;
        }
        let p = exe_path.to_lowercase().replace('/', "\\");
        !NOT_GAMES.iter().any(|n| p.contains(n))
            && self.game_roots.iter().any(|r| p.starts_with(r.as_str()))
    }

    /// Whether a window's program has to be left alone: a game, or a program
    /// that can't be identified, which might be one.
    pub fn is_protected(&self, exe_path: &str) -> bool {
        exe_path.is_empty() || self.is_game(exe_path)
    }
}

/// Where launchers with no install list we read put their games, under Program
/// Files. The launchers themselves stay closable, like the Riot Client.
const LAUNCHER_FOLDERS: [&str; 6] = [
    "EA Games",
    "Ubisoft\\Ubisoft Game Launcher\\games",
    "Call of Duty",
    "Overwatch",
    "Roblox\\Versions",
    "Rockstar Games",
];

/// Lowercase parts of paths in a game's folder that aren't games: Roblox keeps
/// Studio, its editor, right beside the player, and Rockstar's launcher and
/// Social Club sit beside its games.
const NOT_GAMES: [&str; 3] = [
    "\\robloxstudio",
    "\\rockstar games\\launcher\\",
    "\\rockstar games\\social club\\",
];

/// Game folders found by where they usually are, not from a launcher's list:
/// the Xbox app's on every fixed drive (`drives` as "C:\\"), and the defaults
/// of the launchers above. Lowercase, like `Index::game_roots`. These end in a
/// backslash, so a folder whose name only starts the same way doesn't count.
fn other_roots(
    drives: &[String],
    local_app_data: Option<&str>,
    program_files: &[String],
) -> Vec<String> {
    let mut roots: Vec<String> = drives.iter().map(|d| format!("{d}XboxGames\\")).collect();
    roots.extend(local_app_data.map(|l| format!("{l}\\Roblox\\Versions\\")));
    for dir in program_files {
        roots.extend(LAUNCHER_FOLDERS.iter().map(|f| format!("{dir}\\{f}\\")));
    }
    roots.iter().map(|r| r.to_lowercase()).collect()
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
    battle_net(&mut game_roots);
    let program_files: Vec<String> = ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .collect();
    game_roots.extend(other_roots(
        &super::info::fixed_drives(),
        std::env::var("LOCALAPPDATA").ok().as_deref(),
        &program_files,
    ));
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

/// Battle.net's install list is bigger than this only when something's wrong.
const PRODUCT_DB_MAX: u64 = 4 * 1024 * 1024;

/// Battle.net games' folders (they go wherever the person chose, often
/// another drive), from the agent's install list. Only their windows are
/// protected; Battle.net itself stays closable, like the Riot Client.
fn battle_net(roots: &mut Vec<String>) {
    let data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
    let db = Path::new(&data).join("Battle.net\\Agent\\product.db");
    if !std::fs::metadata(&db).is_ok_and(|m| m.len() <= PRODUCT_DB_MAX) {
        return;
    }
    if let Ok(bytes) = std::fs::read(&db) {
        roots.extend(battle_net_roots(&bytes));
    }
}

/// One protobuf varint at `*i`, or None when it runs past the end.
fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b.get(*i)?;
        *i += 1;
        v |= u64::from(c & 0x7f) << shift;
        if c < 0x80 {
            return Some(v);
        }
    }
    None
}

/// The length-delimited fields of a protobuf message, as (number, bytes).
/// Stops at the first thing it can't read, so a cut-off file gives what came
/// before the cut.
fn pb_fields(b: &[u8]) -> Vec<(u64, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let Some(key) = varint(b, &mut i) else { break };
        let skip = match key & 7 {
            0 => varint(b, &mut i).map(|_| 0),
            1 => Some(8),
            2 => {
                let Some(n) = varint(b, &mut i) else { break };
                let Some(end) = usize::try_from(n)
                    .ok()
                    .and_then(|n| i.checked_add(n))
                    .filter(|&e| e <= b.len())
                else {
                    break;
                };
                out.push((key >> 3, &b[i..end]));
                Some(end - i)
            }
            5 => Some(4),
            _ => None,
        };
        match skip
            .and_then(|s| i.checked_add(s))
            .filter(|&e| e <= b.len())
        {
            Some(e) => i = e,
            None => break,
        }
    }
    out
}

/// Install folders in Battle.net's `product.db`: each product install, field
/// one, holds its uid (field one), product code (two) and settings (three),
/// whose first field is the folder. Lowercase with a closing backslash, like
/// the other roots.
fn battle_net_roots(db: &[u8]) -> Vec<String> {
    let mut roots = Vec::new();
    for (n, install) in pb_fields(db) {
        if n != 1 {
            continue;
        }
        let fields = pb_fields(install);
        let text = |n: u64| {
            fields
                .iter()
                .find(|(f, _)| *f == n)
                .and_then(|(_, v)| std::str::from_utf8(v).ok())
        };
        // Battle.net itself, its agent and its runtime.
        let own = ["agent", "battle.net", "bna", "bdk"];
        if text(1).is_some_and(|u| own.contains(&u)) || text(2).is_some_and(|c| own.contains(&c)) {
            continue;
        }
        let Some(path) = fields
            .iter()
            .find(|(f, _)| *f == 3)
            .and_then(|(_, s)| pb_fields(s).into_iter().find(|(f, _)| *f == 1))
            .and_then(|(_, p)| std::str::from_utf8(p).ok())
        else {
            continue;
        };
        let p = path.trim().replace('/', "\\").to_lowercase();
        let p = p.trim_end_matches('\\');
        // A folder, not a bare drive.
        if p.split('\\').filter(|s| !s.is_empty()).count() < 2 {
            continue;
        }
        roots.push(format!("{p}\\"));
    }
    roots
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
    use super::{Index, battle_net_roots, other_roots, vdf_values};

    fn index(game_roots: Vec<String>) -> Index {
        Index {
            apps: Vec::new(),
            game_roots,
            built: std::time::Instant::now(),
        }
    }

    #[test]
    fn known_games_are_games_wherever_they_are() {
        let index = index(Vec::new());
        assert!(index.is_game("E:\\Anywhere\\Grand Theft Auto V\\GTA5.exe"));
        assert!(index.is_game("D:\\Games\\Call of Duty\\_retail_\\cod.exe"));
        assert!(index.is_protected("D:\\Games\\Apex\\r5apex_dx12.exe"));
        // Java isn't a game by its name; only Minecraft's title would say so.
        assert!(!index.is_game("C:\\Program Files\\Java\\bin\\javaw.exe"));
        assert!(!index.is_game("C:\\Windows\\System32\\notepad.exe"));
    }

    #[test]
    fn rockstar_games_are_games_and_its_launcher_isnt() {
        let index = index(other_roots(&[], None, &["C:\\Program Files".to_string()]));
        assert!(
            index.is_game("C:\\Program Files\\Rockstar Games\\Red Dead Redemption 2\\RDR2.exe")
        );
        assert!(!index.is_game("C:\\Program Files\\Rockstar Games\\Launcher\\Launcher.exe"));
        assert!(
            !index.is_game("C:\\Program Files\\Rockstar Games\\Social Club\\SocialClubHelper.exe")
        );
        assert_eq!(
            crate::activity::folder_name(
                "c:\\program files\\rockstar games\\",
                "C:\\Program Files\\Rockstar Games\\Red Dead Redemption 2\\RDR2.exe"
            )
            .as_deref(),
            Some("Red Dead Redemption 2")
        );
    }

    /// A protobuf length-delimited field.
    fn field(n: u8, body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 128);
        let mut v = vec![(n << 3) | 2, body.len() as u8];
        v.extend_from_slice(body);
        v
    }

    fn install(uid: &str, code: &str, path: &str) -> Vec<u8> {
        let mut v = field(1, uid.as_bytes());
        v.extend(field(2, code.as_bytes()));
        // A varint field before the path, as real settings have.
        let mut settings = field(1, path.as_bytes());
        settings.extend([2 << 3, 1]);
        v.extend(field(3, &settings));
        v.extend([4 << 3, 0x96, 0x01]);
        field(1, &v)
    }

    #[test]
    fn reads_battle_net_game_folders() {
        let mut db = install("agent", "agent", "C:/ProgramData/Battle.net/Agent");
        db.extend(install(
            "battle.net",
            "bna",
            "C:/Program Files (x86)/Battle.net",
        ));
        db.extend(install("example", "exmp", "D:/Games/Example Game"));
        db.extend(install("other", "othr", "E:\\Library\\Other\\"));
        db.extend(install("bare", "bare", "F:/"));
        // Other top-level fields, then a cut-off install at the end.
        db.extend([6 << 3, 0xA3, 0xD6, 0xFB, 0x01]);
        db.extend(field(8, b"opaque"));
        let mut cut = install("cut", "cut", "G:/Games/Cut");
        cut.truncate(cut.len() - 5);
        db.extend(cut);
        let roots = battle_net_roots(&db);
        assert_eq!(
            roots,
            vec![
                "d:\\games\\example game\\".to_string(),
                "e:\\library\\other\\".to_string()
            ]
        );
        let index = index(roots);
        assert!(index.is_game("D:\\Games\\Example Game\\_retail_\\Example.exe"));
        assert!(!index.is_game("C:\\Program Files (x86)\\Battle.net\\Battle.net.exe"));
        assert!(!index.is_game("D:\\Games\\Example Gamebar\\x.exe"));
        assert_eq!(
            crate::activity::folder_name(
                "d:\\games\\example game\\",
                "D:\\Games\\Example Game\\_retail_\\Example.exe"
            )
            .as_deref(),
            Some("Example Game")
        );
    }

    #[test]
    fn never_trips_on_a_broken_install_list() {
        for junk in [
            &[][..],
            &[0x0A][..],
            &[0x0A, 0xFF][..],
            &[
                0x0A, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01,
            ][..],
            &[0x0A, 0x7F, 0x01][..],
            &[0x0B][..],
            &[0x09, 1, 2, 3][..],
            &[0xFF; 32][..],
        ] {
            assert!(battle_net_roots(junk).is_empty(), "{junk:?}");
        }
    }

    #[test]
    fn knows_game_folders() {
        let mut game_roots = vec![
            "d:\\steamlibrary\\steamapps\\common".to_string(),
            "c:\\riot games\\valorant".to_string(),
        ];
        game_roots.extend(other_roots(
            &["C:\\".to_string(), "D:\\".to_string()],
            Some("C:\\Users\\player\\AppData\\Local"),
            &[
                "C:\\Program Files".to_string(),
                "C:\\Program Files (x86)".to_string(),
            ],
        ));
        let index = Index {
            apps: Vec::new(),
            game_roots,
            built: std::time::Instant::now(),
        };
        assert!(index.is_game("D:\\SteamLibrary\\steamapps\\common\\Apex Legends\\r5apex.exe"));
        assert!(index.is_game("C:/Riot Games/VALORANT/live/VALORANT.exe"));
        assert!(index.is_game("D:\\XboxGames\\Some Game\\Content\\game.exe"));
        assert!(index.is_game(
            "C:\\Users\\player\\AppData\\Local\\Roblox\\Versions\\version-abc\\RobloxPlayerBeta.exe"
        ));
        assert!(index.is_game("C:\\Program Files (x86)\\Call of Duty\\cod.exe"));
        assert!(index.is_game("C:\\Program Files\\EA Games\\Some Game\\game.exe"));
        assert!(index.is_game(
            "C:\\Program Files (x86)\\Ubisoft\\Ubisoft Game Launcher\\games\\Some Game\\game.exe"
        ));
        assert!(!index.is_game("C:\\Riot Games\\Riot Client\\RiotClientServices.exe"));
        assert!(!index.is_game("C:\\Program Files\\Spotify\\Spotify.exe"));
        assert!(!index.is_game("C:\\Program Files (x86)\\Battle.net\\Battle.net.exe"));
        // Everyday programs, and neighbours of the folders above.
        assert!(!index.is_game("C:\\Windows\\System32\\notepad.exe"));
        assert!(!index.is_game(
            "C:\\Program Files\\WindowsApps\\Microsoft.WindowsNotepad_11.0_x64__8wekyb3d8bbwe\\Notepad\\Notepad.exe"
        ));
        assert!(!index.is_game(
            "C:\\Users\\player\\AppData\\Local\\Roblox\\Versions\\version-abc\\RobloxStudioBeta.exe"
        ));
        assert!(!index.is_game("C:\\Program Files\\EA Games Tools\\tool.exe"));
        assert!(!index.is_game("D:\\XboxGamesBackup\\notes.exe"));
    }

    #[test]
    fn leaves_alone_a_program_it_cant_identify() {
        let index = Index {
            apps: Vec::new(),
            game_roots: vec!["c:\\games".into()],
            built: std::time::Instant::now(),
        };
        assert!(index.is_protected(""));
        assert!(index.is_protected("C:\\Games\\game.exe"));
        assert!(!index.is_protected("C:\\Program Files\\Spotify\\Spotify.exe"));
        // Not labelled a game in "what's open", just left alone.
        assert!(!index.is_game(""));
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
