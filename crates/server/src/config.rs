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
    /// Serve HTTPS directly when both `ADRIAN_TLS_CERT` and `ADRIAN_TLS_KEY`
    /// are set. Plain HTTP otherwise.
    pub tls: Option<TlsFiles>,
}

/// PEM certificate chain and private key for built-in TLS.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl Config {
    /// Read configuration from the environment. Fails only on a half-set
    /// TLS configuration, which would otherwise silently serve plain HTTP.
    pub fn from_env() -> Result<Self, String> {
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
        let tls = match (
            std::env::var_os("ADRIAN_TLS_CERT"),
            std::env::var_os("ADRIAN_TLS_KEY"),
        ) {
            (Some(cert), Some(key)) => Some(TlsFiles {
                cert: cert.into(),
                key: key.into(),
            }),
            (None, None) => None,
            _ => {
                return Err(
                    "set both ADRIAN_TLS_CERT and ADRIAN_TLS_KEY to enable TLS, or neither".into(),
                );
            }
        };
        Ok(Self {
            data_dir,
            bind,
            web_dir,
            tls,
        })
    }

    /// Default base URL other devices use to reach this server when no
    /// `public_url` setting is stored.
    pub fn base_url(&self) -> String {
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        format!("{scheme}://{}", self.bind)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("adrian.db")
    }
}
