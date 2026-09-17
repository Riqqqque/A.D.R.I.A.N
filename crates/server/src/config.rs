//! Server configuration sourced from env/CLI. Secrets never live here —
//! they are entered through the setup flow and stored server-side only.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding adrian.db, backups, media cache. Local disk only —
    /// SQLite documents correctness issues on network filesystems.
    pub data_dir: PathBuf,
    /// Address the HTTP server binds. Defaults to loopback; LAN exposure is
    /// an explicit choice, not a default.
    pub bind: SocketAddr,
    /// Directory containing the built SvelteKit static output.
    pub web_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let data_dir = std::env::var("ADRIAN_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("data"));
        let bind = std::env::var("ADRIAN_BIND")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8787)));
        let web_dir = std::env::var("ADRIAN_WEB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("web/build"));
        Self {
            data_dir,
            bind,
            web_dir,
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("adrian.db")
    }
}
