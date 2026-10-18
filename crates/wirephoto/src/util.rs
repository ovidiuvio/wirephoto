//! Small shared helpers: IDs, etags, times, encodings.

use base64::Engine;
use rand::RngExt;
use sha2::{Digest, Sha256};

const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `prefix` + 16 random base62 characters (~95 bits).
pub fn random_id(prefix: &str) -> String {
    let mut rng = rand::rng();
    let mut s = String::with_capacity(prefix.len() + 16);
    s.push_str(prefix);
    for _ in 0..16 {
        s.push(ALPHABET[rng.random_range(0..ALPHABET.len())] as char);
    }
    s
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rng().fill(&mut b);
    b
}

pub fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// A file's etag: changes whenever its bytes may have (size, modification time or inode).
pub fn file_etag(size: u64, mtime_ns: i64, ino: u64) -> String {
    let mut h = Sha256::new();
    h.update(size.to_le_bytes());
    h.update(mtime_ns.to_le_bytes());
    h.update(ino.to_le_bytes());
    hex::encode(&h.finalize()[..8])
}

/// A folder's etag: changes when it is renamed or moved.
pub fn folder_etag(library: &str, path: &str) -> String {
    let mut h = Sha256::new();
    h.update(library.as_bytes());
    h.update([0]);
    h.update(path.as_bytes());
    hex::encode(&h.finalize()[..8])
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Nanoseconds since the epoch as RFC 3339 UTC, to the second.
pub fn rfc3339_ns(ns: i64) -> String {
    chrono::DateTime::from_timestamp(ns.div_euclid(1_000_000_000), 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
