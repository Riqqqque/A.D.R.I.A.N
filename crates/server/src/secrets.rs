//! Encryption at rest for the `secrets` table (provider API keys, HA token).
//!
//! Values are sealed with ChaCha20-Poly1305 under a 256-bit data key. The
//! data key lives in a key file, by default `secret.key` in the data
//! directory, or wherever `ADRIAN_KEY_FILE` points:
//!
//! - On Windows the key file holds the data key wrapped with DPAPI
//!   (current-user scope), so copying the data directory to another account
//!   or machine yields nothing usable.
//! - Elsewhere the key file holds the raw key with mode 0600. Put it outside
//!   the data directory (`ADRIAN_KEY_FILE`) for it to protect anything beyond
//!   backups.
//!
//! Backups copy only the database, never the key file, so the secrets in a
//! backup are ciphertext. The secret name is bound as associated data, so a
//! sealed value cannot be swapped between rows.

use crate::error::ApiError;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand::Rng;
use std::path::{Path, PathBuf};

/// Prefix marking a sealed value; anything without it is legacy plaintext.
const PREFIX: &str = "enc:v1:";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

pub struct SecretBox {
    cipher: ChaCha20Poly1305,
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretBox(..)")
    }
}

impl SecretBox {
    /// Key file location: `ADRIAN_KEY_FILE`, else `<data_dir>/secret.key`.
    pub fn key_path(data_dir: &Path) -> PathBuf {
        std::env::var("ADRIAN_KEY_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("secret.key"))
    }

    /// Load the data key, creating one on first run.
    pub fn load_or_create(path: &Path) -> Result<Self, ApiError> {
        let key = match std::fs::read(path) {
            Ok(stored) => unwrap_key(&stored).map_err(|e| {
                ApiError::internal_msg(format!("cannot unlock secret key {}: {e}", path.display()))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut key = vec![0u8; KEY_LEN];
                rand::rng().fill_bytes(&mut key);
                let stored = wrap_key(&key).map_err(ApiError::internal_msg)?;
                write_key_file(path, &stored).map_err(ApiError::internal)?;
                tracing::info!(path = %path.display(), "created secret encryption key");
                key
            }
            Err(e) => return Err(ApiError::internal(e)),
        };
        if key.len() != KEY_LEN {
            return Err(ApiError::internal_msg(format!(
                "secret key {} has the wrong length",
                path.display()
            )));
        }
        let cipher = ChaCha20Poly1305::new_from_slice(&key)
            .map_err(|_| ApiError::internal_msg("secret key has the wrong length".to_string()))?;
        Ok(Self { cipher })
    }

    pub fn is_sealed(value: &str) -> bool {
        value.starts_with(PREFIX)
    }

    pub fn seal(&self, name: &str, plaintext: &str) -> Result<String, ApiError> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| ApiError::internal_msg("secret encryption failed"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(format!("{PREFIX}{}", B64.encode(out)))
    }

    /// Decrypt a stored value. Legacy plaintext passes through unchanged
    /// (it is re-sealed by [`crate::state::AppState::seal_legacy_secrets`]).
    pub fn open(&self, name: &str, stored: &str) -> Result<String, ApiError> {
        let Some(b64) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_string());
        };
        let unreadable = || {
            ApiError::NotConfigured(format!(
                "stored secret '{name}' cannot be decrypted with this server's key \
                 (restored from another machine or key file replaced?) — re-enter it"
            ))
        };
        let raw = B64.decode(b64).map_err(|_| unreadable())?;
        if raw.len() < NONCE_LEN {
            return Err(unreadable());
        }
        let (nonce, ct) = raw.split_at(NONCE_LEN);
        let pt = self
            .cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ct,
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| unreadable())?;
        String::from_utf8(pt).map_err(|_| unreadable())
    }
}

fn write_key_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    std::io::Write::write_all(&mut f, bytes)?;
    f.sync_all()
}

#[cfg(windows)]
fn wrap_key(key: &[u8]) -> Result<Vec<u8>, String> {
    dpapi::protect(key)
}

#[cfg(windows)]
fn unwrap_key(stored: &[u8]) -> Result<Vec<u8>, String> {
    dpapi::unprotect(stored)
}

#[cfg(not(windows))]
fn wrap_key(key: &[u8]) -> Result<Vec<u8>, String> {
    Ok(key.to_vec())
}

#[cfg(not(windows))]
fn unwrap_key(stored: &[u8]) -> Result<Vec<u8>, String> {
    Ok(stored.to_vec())
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copy DPAPI's LocalAlloc'd output into a Vec and free it.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: on success DPAPI returns a valid buffer of cbData bytes
        // allocated with LocalAlloc; we copy it before freeing.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as _);
            v
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: input points at live memory for the call; out is written
        // by the API and consumed by `take` only on success.
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI protect failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: as in `protect`.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI unprotect failed (different Windows user or machine?): {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { take(out) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_key_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        let a = SecretBox::load_or_create(&path).unwrap();
        let sealed = a.seal("provider:x", "sk-test-123").unwrap();
        assert!(SecretBox::is_sealed(&sealed));
        assert!(!sealed.contains("sk-test-123"));
        assert_eq!(a.open("provider:x", &sealed).unwrap(), "sk-test-123");
        // Same key file → same key.
        let b = SecretBox::load_or_create(&path).unwrap();
        assert_eq!(b.open("provider:x", &sealed).unwrap(), "sk-test-123");
        // Legacy plaintext passes through.
        assert_eq!(b.open("provider:x", "plain").unwrap(), "plain");
    }

    #[test]
    fn wrong_name_or_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = SecretBox::load_or_create(&dir.path().join("a.key")).unwrap();
        let b = SecretBox::load_or_create(&dir.path().join("b.key")).unwrap();
        let sealed = a.seal("ha", "token").unwrap();
        assert!(a.open("provider:x", &sealed).is_err(), "aad binds the name");
        assert!(b.open("ha", &sealed).is_err(), "other key cannot open");
    }

    #[cfg(windows)]
    #[test]
    fn key_file_is_dpapi_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        SecretBox::load_or_create(&path).unwrap();
        let stored = std::fs::read(&path).unwrap();
        assert_ne!(stored.len(), KEY_LEN, "raw key must not be on disk");
    }

    #[tokio::test]
    async fn state_seals_new_and_legacy_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            data_dir: dir.path().to_path_buf(),
            bind: "127.0.0.1:0".parse().unwrap(),
            web_dir: dir.path().to_path_buf(),
            tls: None,
        };
        let state = crate::build_state(config).await.unwrap();
        state.set_secret("provider:a", "sk-new").await.unwrap();
        state
            .db
            .exec(
                "INSERT INTO secrets(name,value,created_at) VALUES('ha','legacy-token',0)",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(state.seal_legacy_secrets().await.unwrap(), 1);
        let raw = state
            .db
            .q("SELECT value FROM secrets", vec![], |r| {
                r.get::<_, String>(0)
            })
            .await
            .unwrap();
        assert!(raw.iter().all(|v| SecretBox::is_sealed(v)), "{raw:?}");
        assert_eq!(
            state.get_secret("provider:a").await.unwrap().unwrap(),
            "sk-new"
        );
        assert_eq!(
            state.get_secret("ha").await.unwrap().unwrap(),
            "legacy-token"
        );
        assert!(state.has_secret("ha").await.unwrap());
    }
}
