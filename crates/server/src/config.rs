//! Server configuration sourced from env/CLI. Secrets never live here —
//! they are entered through the setup flow and stored server-side only.

use std::net::{IpAddr, SocketAddr};
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
    /// Address of the TLS-terminating reverse proxy (`ADRIAN_TRUSTED_PROXY`).
    /// Only a connection from exactly this address may speak for the client
    /// through `X-Forwarded-For` and `X-Forwarded-Proto`. Unset, both are
    /// ignored.
    pub trusted_proxy: Option<IpAddr>,
}

/// PEM certificate chain and private key for built-in TLS.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl Config {
    /// Read configuration from the environment. Fails on a half-set TLS
    /// configuration, which would otherwise silently serve plain HTTP, and
    /// on an unusable `ADRIAN_TRUSTED_PROXY`.
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
        let trusted_proxy =
            parse_trusted_proxy(std::env::var("ADRIAN_TRUSTED_PROXY").ok().as_deref())?;
        Ok(Self {
            data_dir,
            bind,
            web_dir,
            tls,
            trusted_proxy,
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

/// Blank means no proxy. A value that isn't one IP address is refused rather
/// than ignored: a typo would put every client in the proxy's single
/// rate-limit bucket with nothing to show why.
fn parse_trusted_proxy(raw: Option<&str>) -> Result<Option<IpAddr>, String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => s
            .parse::<IpAddr>()
            .map(|ip| Some(ip.to_canonical()))
            .map_err(|_| format!("ADRIAN_TRUSTED_PROXY must be one IP address, got '{s}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_proxy_is_off_unless_it_is_one_address() {
        assert_eq!(parse_trusted_proxy(None), Ok(None));
        assert_eq!(parse_trusted_proxy(Some("  ")), Ok(None));
        assert_eq!(
            parse_trusted_proxy(Some(" 10.0.0.1 ")),
            Ok(Some("10.0.0.1".parse().unwrap()))
        );
        // Written as a mapped address it still has to match an IPv4 peer.
        assert_eq!(
            parse_trusted_proxy(Some("::ffff:10.0.0.1")),
            Ok(Some("10.0.0.1".parse().unwrap()))
        );
        for bad in [
            "proxy.lan",
            "10.0.0.1:443",
            "10.0.0.0/24",
            "10.0.0.1,10.0.0.2",
        ] {
            assert!(parse_trusted_proxy(Some(bad)).is_err(), "{bad}");
        }
    }
}
