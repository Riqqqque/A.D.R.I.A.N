//! What this PC allows. Lives in the satellite's config; the server can't change it.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Policy {
    /// Open, close, focus and minimize apps and games.
    pub apps: bool,
    /// Open web pages, YouTube and known folders.
    pub open: bool,
    /// Play/pause/skip whatever is playing.
    pub media: bool,
    /// Master and per-app volume.
    pub volume: bool,
    /// CPU, memory, disk, battery and the busiest programs.
    pub info: bool,
    pub lock: bool,
    pub screen_off: bool,
    /// These three also need the person's spoken yes (`confirmed`).
    pub sleep: bool,
    pub restart: bool,
    pub shutdown: bool,
    /// Named things this PC may start besides apps, e.g. a launcher script.
    pub shortcuts: Vec<Shortcut>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            apps: true,
            open: true,
            media: true,
            volume: true,
            info: true,
            lock: true,
            screen_off: true,
            sleep: true,
            restart: true,
            shutdown: true,
            shortcuts: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Shortcut {
    /// What people call it: "auto-tune studio".
    pub name: String,
    /// A file to open: an .exe, .lnk, .bat or .ps1 shortcut, or a URL.
    pub target: String,
    #[serde(default)]
    pub args: String,
}
