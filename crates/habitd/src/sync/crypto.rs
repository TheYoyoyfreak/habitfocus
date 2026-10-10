//! End-to-end encryption of sync records.
//!
//! Every account has one random 256-bit key, created at registration and
//! shown to the user, who enters it on every other device. The server never
//! sees it. A record's data is sealed with XChaCha20-Poly1305 under a random
//! nonce; its metadata (id, position, device, tag) is bound as associated
//! data, so the server can't move a record to another place unnoticed.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

/// Prefix of a key as the user sees it.
const KEY_PREFIX: &str = "hfk1-";
/// Prefix of a sealed record's `raw`.
const RAW_PREFIX: &str = "hf1.";
const NONCE_LEN: usize = 24;

#[derive(Clone)]
pub struct Key([u8; 32]);

impl Key {
    pub fn generate() -> Key {
        Key(XChaCha20Poly1305::generate_key(&mut OsRng).into())
    }

    /// Reads a key as `to_text` writes it; spaces and line breaks are ignored.
    pub fn from_text(text: &str) -> Result<Key, String> {
        let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let encoded = text.strip_prefix(KEY_PREFIX).ok_or("a sync key starts with hfk1-")?;
        let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| "the sync key isn't valid; check for typos")?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| "the sync key has the wrong length; check for typos")?;
        Ok(Key(bytes))
    }

    pub fn to_text(&self) -> String {
        format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(self.0))
    }

    /// Names the key without revealing it, so a record sealed under another
    /// key is told apart from a damaged one.
    pub fn id(&self) -> String {
        let digest = Sha256::new().chain_update(b"habitfocus sync key id\0").chain_update(self.0).finalize();
        format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(&digest[..12]))
    }
}

/// Never prints the key.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Key({})", self.id())
    }
}

/// Seals `plaintext`; returns the record's `raw` and `cek` (the key id).
pub fn seal(key: &Key, plaintext: &[u8], aad: &str) -> (String, String) {
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    (seal_with_nonce(key, &nonce, plaintext, aad), key.id())
}

/// The `raw` of `plaintext` sealed under `nonce`.
fn seal_with_nonce(key: &Key, nonce: &XNonce, plaintext: &[u8], aad: &str) -> String {
    let sealed = XChaCha20Poly1305::new(&key.0.into())
        .encrypt(nonce, Payload { msg: plaintext, aad: aad.as_bytes() })
        .expect("encrypting into memory can't fail");
    let mut bytes = nonce.to_vec();
    bytes.extend_from_slice(&sealed);
    format!("{RAW_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Opens what `seal` sealed with the same `aad`.
pub fn open(key: &Key, raw: &str, cek: &str, aad: &str) -> Result<Vec<u8>, String> {
    if cek != key.id() {
        return Err("the data was encrypted with another sync key".into());
    }
    let bytes = raw
        .strip_prefix(RAW_PREFIX)
        .and_then(|encoded| URL_SAFE_NO_PAD.decode(encoded).ok())
        .filter(|bytes| bytes.len() > NONCE_LEN)
        .ok_or("a record is in an unknown format")?;
    let (nonce, sealed) = bytes.split_at(NONCE_LEN);
    XChaCha20Poly1305::new(&key.0.into())
        .decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad: aad.as_bytes() })
        .map_err(|_| "a record failed its integrity check".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_data_opens_only_with_its_key_and_place() {
        let key = Key::generate();
        let (raw, cek) = seal(&key, b"rows", "place a");
        assert!(raw.starts_with(RAW_PREFIX) && !raw.contains("rows"));
        assert_eq!(open(&key, &raw, &cek, "place a").unwrap(), b"rows");

        assert!(open(&key, &raw, &cek, "place b").unwrap_err().contains("integrity"), "moved elsewhere");
        let other = Key::generate();
        assert!(open(&other, &raw, &cek, "place a").unwrap_err().contains("another sync key"));
        let mut tampered = raw.clone();
        tampered.pop();
        tampered.push(if raw.ends_with('A') { 'B' } else { 'A' });
        assert!(open(&key, &tampered, &cek, "place a").is_err());
        assert_ne!(seal(&key, b"rows", "place a").0, raw, "a fresh nonce every time");
    }

    /// Other clients (the habitfocus_mobile app) check against these values
    /// that they seal byte for byte the same; don't change them.
    #[test]
    fn sealing_matches_the_published_vector() {
        let key = Key((0u8..32).collect::<Vec<_>>().try_into().unwrap());
        assert_eq!(key.to_text(), "hfk1-AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8");
        assert_eq!(key.id(), "hfk1-9VgONU3vH1mTPL10");
        let nonce = XNonce::from([7u8; NONCE_LEN]);
        let aad = "habitfocus sync|0199b4c2-0000-7000-8000-000000000000|3|0199b4c1-0000-7000-8000-000000000000|sessions|v1";
        let plaintext = br#"{"rows":[[1,{"habit":"read","started_at":1000,"finished_at":61000,"focused_ms":60000,"outcome":"completed"}]]}"#;
        let raw = seal_with_nonce(&key, &nonce, plaintext, aad);
        assert_eq!(raw, "hf1.BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHnZ0ptyka5q4nwUQLOwURQfqqshNQw7FaWllHOwhb_CuXtpAwfZETPvFu90Ftlq7rTPU1M-ynZsmiesl0hfCfx0yKj_8QKAz9JWLlTfl468surZQYaeXuTr80lTB4Wk_N0UyUZYMdNMbfq1aFEkE_03VfJBZu75AXJdL1RvCN");
        assert_eq!(open(&key, &raw, &key.id(), aad).unwrap(), plaintext);
    }

    #[test]
    fn keys_round_trip_as_text() {
        let key = Key::generate();
        let text = key.to_text();
        assert_eq!(text.len(), KEY_PREFIX.len() + 43);
        let spaced = format!(" {}\n{} ", &text[..20], &text[20..]);
        assert_eq!(Key::from_text(&spaced).unwrap().0, key.0);
        assert!(Key::from_text("hfk1-short").is_err());
        assert!(Key::from_text(&text[KEY_PREFIX.len()..]).is_err(), "prefix required");
        assert_ne!(key.id(), Key::generate().id());
        assert!(!format!("{key:?}").contains(&text[KEY_PREFIX.len()..]));
    }
}
