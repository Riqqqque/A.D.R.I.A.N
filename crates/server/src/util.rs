//! Small shared helpers: CSPRNG ids/tokens, hashing, time.

use rand::Rng;
use sha2::Digest;

/// Random hex id (128-bit). Used for session ids, alarm ids, media tokens.
pub fn new_id() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    to_hex(&b)
}

/// Human-enterable token like `ADRN-XXXX-XXXX-XXXX` (256-bit entropy shown
/// as base32-ish groups) for the one-use setup token and pairing codes.
pub fn new_token() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789"; // no I,L,O,0,1
    let mut b = [0u8; 12];
    rand::rng().fill_bytes(&mut b);
    let s: String = b
        .iter()
        .map(|v| ALPHABET[(*v as usize) % ALPHABET.len()] as char)
        .collect();
    format!("ADRN-{}-{}-{}", &s[0..4], &s[4..8], &s[8..12])
}

/// SHA-256 hex — used to store only hashes of bearer-style tokens.
pub fn sha256_hex(input: &str) -> String {
    let d = sha2::Sha256::digest(input.as_bytes());
    to_hex(&d)
}

pub fn to_hex(b: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &v in b {
        s.push(HEX[(v >> 4) as usize] as char);
        s.push(HEX[(v & 0xf) as usize] as char);
    }
    s
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Redact a secret for display: show last 4 chars only.
pub fn redact(secret: &str) -> String {
    if secret.len() <= 4 {
        "****".into()
    } else {
        format!("****{}", &secret[secret.len() - 4..])
    }
}
