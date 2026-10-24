//! Settings, kept in %APPDATA%\adrian\desktop\config\config.json. The pairing
//! token is encrypted with Windows DPAPI, so only this Windows user can read it.

use adrian_pc_control::Policy;
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

    #[cfg(windows)]
    #[test]
    fn keeps_the_token_encrypted() {
        let mut s = Settings::default();
        s.set_token("sat_secret").unwrap();
        assert!(!s.token_protected.contains(&to_hex(b"sat_secret")));
        assert_eq!(s.token().as_deref(), Some("sat_secret"));
    }
}
