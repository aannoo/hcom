//! Symmetric AEAD for relay payloads.
//!
//! Wraps `XChaCha20Poly1305` so callers see only `seal`/`open`. Every payload
//! published to MQTT (state, events, control) is sealed with the per-relay PSK.
//!
//! Wire format:
//!
//! ```text
//!   1 byte  suite      (0x01 = XChaCha20-Poly1305)
//!  24 bytes nonce      (random per message — XChaCha extended nonce, safe at random)
//!   8 bytes ts_be      (u64 seconds since epoch, big-endian — replay binding)
//!   N bytes ciphertext (Poly1305 tag included by the AEAD impl)
//! ```
//!
//! Associated data is `relay_id || 0x00 || topic || 0x00 || ts_be`. Binding
//! topic and timestamp into the AAD prevents a valid ciphertext from being
//! replayed onto a different topic or re-dated.

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use rand::Rng;

pub const SUITE_XCHACHA20_POLY1305: u8 = 0x01;
pub const NONCE_LEN: usize = 24;
const TS_LEN: usize = 8;
pub const HEADER_LEN: usize = 1 + NONCE_LEN + TS_LEN;
pub const TAG_LEN: usize = 16;

#[derive(Debug)]
pub enum CryptoError {
    BadKey,
    BadEnvelope,
    UnknownSuite(u8),
    Decrypt,
    Encrypt,
    Rng,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadKey => write!(f, "bad relay key (must be 32 bytes)"),
            Self::BadEnvelope => write!(f, "bad envelope (truncated or malformed header)"),
            Self::UnknownSuite(b) => write!(f, "unknown crypto suite byte 0x{:02x}", b),
            Self::Decrypt => write!(f, "decryption failed"),
            Self::Encrypt => write!(f, "encryption failed"),
            Self::Rng => write!(f, "rng failure"),
        }
    }
}

impl std::error::Error for CryptoError {}

fn build_aad(relay_id: &str, topic: &str, ts_be: &[u8; TS_LEN]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(relay_id.len() + topic.len() + TS_LEN + 2);
    aad.extend_from_slice(relay_id.as_bytes());
    aad.push(0x00);
    aad.extend_from_slice(topic.as_bytes());
    aad.push(0x00);
    aad.extend_from_slice(ts_be);
    aad
}

/// Seal a plaintext payload, returning a wire envelope ready to publish.
pub fn seal(
    psk: &[u8; 32],
    relay_id: &str,
    topic: &str,
    plaintext: &[u8],
    ts_secs: u64,
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new_from_slice(psk).map_err(|_| CryptoError::BadKey)?;

    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);

    let ts_be = ts_secs.to_be_bytes();
    let aad = build_aad(relay_id, topic, &ts_be);

    let ct = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Encrypt)?;

    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.push(SUITE_XCHACHA20_POLY1305);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ts_be);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Parsed envelope header. The `nonce` and `ts_secs` are exposed for replay
/// checks after the full envelope authenticates.
pub struct Envelope<'a> {
    pub nonce: [u8; NONCE_LEN],
    pub ts_secs: u64,
    pub ciphertext: &'a [u8],
}

/// Parse the wire header without decrypting. Callers must authenticate the full
/// envelope before trusting these fields or applying replay policy.
pub fn parse_envelope(buf: &[u8]) -> Result<Envelope<'_>, CryptoError> {
    if buf.len() < HEADER_LEN + TAG_LEN {
        return Err(CryptoError::BadEnvelope);
    }
    let suite = buf[0];
    if suite != SUITE_XCHACHA20_POLY1305 {
        return Err(CryptoError::UnknownSuite(suite));
    }
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&buf[1..1 + NONCE_LEN]);
    let mut ts_be = [0u8; TS_LEN];
    ts_be.copy_from_slice(&buf[1 + NONCE_LEN..HEADER_LEN]);
    let ts_secs = u64::from_be_bytes(ts_be);
    Ok(Envelope {
        nonce,
        ts_secs,
        ciphertext: &buf[HEADER_LEN..],
    })
}

/// Open an envelope sealed with the same PSK + AAD. Caller-provided `relay_id`
/// and `topic` must match what the sender used; otherwise the AEAD tag fails.
pub fn open(
    psk: &[u8; 32],
    relay_id: &str,
    topic: &str,
    envelope: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let parsed = parse_envelope(envelope)?;
    let cipher = XChaCha20Poly1305::new_from_slice(psk).map_err(|_| CryptoError::BadKey)?;
    let ts_be = parsed.ts_secs.to_be_bytes();
    let aad = build_aad(relay_id, topic, &ts_be);
    cipher
        .decrypt(
            (&parsed.nonce).into(),
            Payload {
                msg: parsed.ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)
}

/// Generate a fresh 32-byte PSK from the OS RNG.
pub fn generate_psk() -> Result<[u8; 32], CryptoError> {
    let mut k = [0u8; 32];
    rand::rng().fill_bytes(&mut k);
    Ok(k)
}

/// Display fingerprint: first 8 base64url chars of `sha256(psk)`. Lets users
/// visually confirm two devices share the same key without leaking material.
pub fn fingerprint(psk: &[u8; 32]) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(psk);
    let b64 = URL_SAFE_NO_PAD.encode(&digest[..6]);
    debug_assert_eq!(b64.len(), 8);
    b64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[test]
    fn roundtrip_seal_open() {
        let k = key();
        let pt = b"hello world";
        let env = seal(&k, "relay-1", "relay-1/dev-a", pt, 1_700_000_000).unwrap();
        let out = open(&k, "relay-1", "relay-1/dev-a", &env).unwrap();
        assert_eq!(out, pt);
    }

    #[test]
    fn open_with_wrong_key_fails() {
        let k1 = key();
        let mut k2 = key();
        k2[0] ^= 0xFF;
        let env = seal(&k1, "r", "r/dev", b"x", 1).unwrap();
        assert!(matches!(
            open(&k2, "r", "r/dev", &env),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn open_with_wrong_topic_fails() {
        let k = key();
        let env = seal(&k, "r", "r/a", b"x", 1).unwrap();
        assert!(matches!(
            open(&k, "r", "r/b", &env),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn open_with_wrong_relay_id_fails() {
        let k = key();
        let env = seal(&k, "r1", "r1/a", b"x", 1).unwrap();
        assert!(matches!(
            open(&k, "r2", "r1/a", &env),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let k = key();
        let mut env = seal(&k, "r", "r/a", b"hello", 1).unwrap();
        let last = env.len() - 1;
        env[last] ^= 0x01;
        assert!(matches!(
            open(&k, "r", "r/a", &env),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn tampered_ts_fails() {
        let k = key();
        let mut env = seal(&k, "r", "r/a", b"hello", 1).unwrap();
        // ts_be lives at offset 1+24..1+24+8
        env[1 + NONCE_LEN] ^= 0x01;
        assert!(matches!(
            open(&k, "r", "r/a", &env),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn distinct_nonces() {
        let k = key();
        let mut nonces = std::collections::HashSet::new();
        for _ in 0..1000 {
            let env = seal(&k, "r", "r/a", b"x", 1).unwrap();
            let parsed = parse_envelope(&env).unwrap();
            assert!(nonces.insert(parsed.nonce));
        }
    }

    #[test]
    fn parse_envelope_rejects_short() {
        assert!(matches!(
            parse_envelope(&[0x01, 0x00]),
            Err(CryptoError::BadEnvelope)
        ));
    }

    #[test]
    fn parse_envelope_rejects_unknown_suite() {
        let buf = vec![0x99u8; HEADER_LEN + TAG_LEN];
        assert!(matches!(
            parse_envelope(&buf),
            Err(CryptoError::UnknownSuite(0x99))
        ));
    }

    #[test]
    fn fingerprint_is_stable() {
        let k = key();
        let f1 = fingerprint(&k);
        let f2 = fingerprint(&k);
        assert_eq!(f1, f2);
        assert_eq!(f1.len(), 8);
    }
}
