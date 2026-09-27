//! Per-device signatures on relay publishes (observe mode).
//!
//! Every relay payload is sealed with the relay's shared PSK, which proves "a
//! member of this relay sealed it" - and nothing more. Any member can claim to
//! be any device: a state snapshot for another device's topic, a tombstone that
//! makes peers drop another device's instances, or an RPC control request
//! naming another device as its sender.
//!
//! Each device therefore also signs the exact sealed bytes it publishes with its
//! own Ed25519 key, carried in MQTT v5 user properties so older builds ignore
//! it. Receivers check the signature only AFTER the AEAD open succeeded (so a
//! stranger without the PSK can never get a key pinned), pin each device's key
//! the first time they see it, and report a missing signature from a device
//! that has signed before, an invalid signature, or a changed key.
//!
//! This build only observes and reports; nothing is rejected. Enforcement comes
//! once every device signs and the reports stay clean.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rumqttc::v5::mqttbytes::v5::PublishProperties;

use crate::db::HcomDb;
use crate::log;

use super::{safe_kv_get, safe_kv_set};

pub(crate) const PROP_KEY: &str = "hcom-key";
pub(crate) const PROP_SIG: &str = "hcom-sig";
const KEY_FILE: &str = "device_sign.pk8";
const PIN_PREFIX: &str = "relay_sigkey_";
const STATUS_PREFIX: &str = "relay_sigstatus_";

fn key_path() -> PathBuf {
    crate::paths::hcom_dir().join(".tmp").join(KEY_FILE)
}

/// Load this device's signing key from `path`, creating it on first use.
/// Creation is create-if-absent (a hard link from a private temp file), so two
/// processes racing on first use end up with the same key rather than one
/// silently replacing the other. A file that exists but does not parse is left
/// alone and yields None: publishing unsigned is recoverable, overwriting a
/// key peers have pinned is not.
pub(crate) fn load_or_create_keypair_at(path: &Path) -> Option<Ed25519KeyPair> {
    if let Ok(bytes) = std::fs::read(path) {
        return match Ed25519KeyPair::from_pkcs8(&bytes) {
            Ok(pair) => Some(pair),
            Err(_) => {
                log::log_warn(
                    "relay",
                    "relay.sig_key_unreadable",
                    &format!("{} does not parse; publishing unsigned", path.display()),
                );
                None
            }
        };
    }
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).ok()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    // Private from the first byte: the final path is a hard link to this inode, so its mode
    // must not depend on the umask or on the directory staying private. And EXCLUSIVE: an
    // existing file at the temp path (on Windows a shared HCOM_DIR is not restricted) is never
    // truncated and written into, it is refused and this process publishes unsigned
    // (upstream review of #144).
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut file| {
        use std::io::Write;
        file.write_all(pkcs8.as_ref())
    });
    if written.is_err() {
        log::log_warn(
            "relay",
            "relay.sig_key_tmp_refused",
            &format!(
                "{} could not be created exclusively; publishing unsigned",
                tmp.display()
            ),
        );
        return None;
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    if linked.is_err() && !path.exists() {
        return None;
    }
    // Whoever won the race, the file on disk is the key.
    Ed25519KeyPair::from_pkcs8(&std::fs::read(path).ok()?).ok()
}

fn device_keypair() -> Option<&'static Ed25519KeyPair> {
    static KEYPAIR: OnceLock<Option<Ed25519KeyPair>> = OnceLock::new();
    KEYPAIR
        .get_or_init(|| load_or_create_keypair_at(&key_path()))
        .as_ref()
}

fn properties_with(pair: &Ed25519KeyPair, sealed: &[u8]) -> PublishProperties {
    let signature = pair.sign(sealed);
    PublishProperties {
        user_properties: vec![
            (PROP_KEY.to_string(), B64.encode(pair.public_key().as_ref())),
            (PROP_SIG.to_string(), B64.encode(signature.as_ref())),
        ],
        ..Default::default()
    }
}

/// Publish properties carrying this device's signature over `sealed`, or None
/// when no key is available (then the publish goes out unsigned).
pub(crate) fn publish_properties(sealed: &[u8]) -> Option<PublishProperties> {
    device_keypair().map(|pair| properties_with(pair, sealed))
}

/// Short fingerprint of this device's public key, for status output.
pub(crate) fn own_key_fingerprint() -> Option<String> {
    device_keypair().map(|pair| fingerprint(&B64.encode(pair.public_key().as_ref())))
}

fn fingerprint(key_b64: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key_b64.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    /// Signed by the key pinned for this device.
    Valid,
    /// Signed; first key seen for this device, now pinned.
    FirstSeen,
    /// No signature. `pinned` means the device has signed before (a downgrade).
    Missing { pinned: bool },
    /// A signature that does not verify over the received bytes.
    Invalid,
    /// Valid signature, but by a different key than the one pinned.
    KeyChanged,
}

impl Verdict {
    fn label(&self) -> &'static str {
        match self {
            Verdict::Valid | Verdict::FirstSeen => "valid",
            Verdict::Missing { pinned: false } => "unsigned (older build)",
            Verdict::Missing { pinned: true } => "MISSING after signing before",
            Verdict::Invalid => "INVALID signature",
            Verdict::KeyChanged => "KEY CHANGED since pinned",
        }
    }
}

fn prop<'a>(props: &'a [(String, String)], name: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Judge one authenticated publish from `device_id`. Pins the key on first sight.
pub(crate) fn check(
    db: &HcomDb,
    device_id: &str,
    sealed: &[u8],
    props: &[(String, String)],
) -> Verdict {
    let pin_key = format!("{PIN_PREFIX}{device_id}");
    let pinned = safe_kv_get(db, &pin_key);
    let (Some(key_b64), Some(sig_b64)) = (prop(props, PROP_KEY), prop(props, PROP_SIG)) else {
        return Verdict::Missing {
            pinned: pinned.is_some(),
        };
    };
    let (Ok(key), Ok(sig)) = (B64.decode(key_b64), B64.decode(sig_b64)) else {
        return Verdict::Invalid;
    };
    if UnparsedPublicKey::new(&ED25519, &key)
        .verify(sealed, &sig)
        .is_err()
    {
        return Verdict::Invalid;
    }
    match pinned {
        None => {
            safe_kv_set(db, &pin_key, Some(key_b64));
            Verdict::FirstSeen
        }
        Some(p) if p == key_b64 => Verdict::Valid,
        Some(_) => Verdict::KeyChanged,
    }
}

/// Check and report, never reject. Logs when a device's verdict changes, so a
/// steady state is quiet and every transition is on record.
pub(crate) fn observe(
    db: &HcomDb,
    device_id: &str,
    sealed: &[u8],
    props: &[(String, String)],
) -> Verdict {
    let verdict = check(db, device_id, sealed, props);
    let status_key = format!("{STATUS_PREFIX}{device_id}");
    let label = verdict.label();
    if safe_kv_get(db, &status_key).as_deref() != Some(label) {
        safe_kv_set(db, &status_key, Some(label));
        let detail = format!(
            "device={} verdict={}",
            super::device_id_prefix(device_id),
            label
        );
        match verdict {
            Verdict::Valid | Verdict::FirstSeen | Verdict::Missing { pinned: false } => {
                log::log_info("relay", "relay.sig", &detail)
            }
            _ => log::log_warn("relay", "relay.sig", &detail),
        }
    }
    verdict
}

/// One line per device whose signature state is known, for `hcom relay status`.
pub(crate) fn status_lines(db: &HcomDb) -> Vec<String> {
    db.kv_prefix(STATUS_PREFIX)
        .unwrap_or_default()
        .into_iter()
        .map(|(key, label)| {
            let device = key.trim_start_matches(STATUS_PREFIX).to_string();
            let short = safe_kv_get(db, &format!("relay_uuid_short_{device}"))
                .unwrap_or_else(|| super::device_id_prefix(&device).to_string());
            let pin = safe_kv_get(db, &format!("{PIN_PREFIX}{device}"))
                .map(|k| format!(" key {}", fingerprint(&k)))
                .unwrap_or_default();
            format!("{short}: {label}{pin}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> (tempfile::TempDir, HcomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        (dir, db)
    }

    fn fresh_pair(dir: &Path, name: &str) -> Ed25519KeyPair {
        load_or_create_keypair_at(&dir.join(name)).unwrap()
    }

    fn props_of(pair: &Ed25519KeyPair, sealed: &[u8]) -> Vec<(String, String)> {
        properties_with(pair, sealed).user_properties
    }

    #[test]
    fn a_key_is_created_once_and_reloaded_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_sign.pk8");
        let first = load_or_create_keypair_at(&path).unwrap();
        let again = load_or_create_keypair_at(&path).unwrap();
        assert_eq!(first.public_key().as_ref(), again.public_key().as_ref());
    }

    #[test]
    fn an_existing_temp_file_is_refused_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_sign.pk8");
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, b"planted").unwrap();
        assert!(load_or_create_keypair_at(&path).is_none());
        assert_eq!(std::fs::read(&tmp).unwrap(), b"planted", "left untouched");
        assert!(
            !path.exists(),
            "no key written through a file it did not create"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_new_key_file_is_private_whatever_the_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_sign.pk8");
        load_or_create_keypair_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file mode {mode:o}");
    }

    #[test]
    fn an_unparseable_key_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device_sign.pk8");
        std::fs::write(&path, b"not a key").unwrap();
        assert!(load_or_create_keypair_at(&path).is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"not a key");
    }

    #[test]
    fn first_signature_pins_then_verifies() {
        let (dir, db) = test_db();
        let pair = fresh_pair(dir.path(), "a.pk8");
        let sealed = b"sealed envelope bytes";
        let props = props_of(&pair, sealed);
        assert_eq!(check(&db, "dev-a", sealed, &props), Verdict::FirstSeen);
        assert_eq!(check(&db, "dev-a", sealed, &props), Verdict::Valid);
    }

    #[test]
    fn tampered_bytes_do_not_verify() {
        let (dir, db) = test_db();
        let pair = fresh_pair(dir.path(), "a.pk8");
        let props = props_of(&pair, b"original");
        assert_eq!(check(&db, "dev-a", b"tampered", &props), Verdict::Invalid);
        assert!(
            safe_kv_get(&db, "relay_sigkey_dev-a").is_none(),
            "nothing pinned"
        );
    }

    #[test]
    fn another_key_for_a_pinned_device_is_reported() {
        let (dir, db) = test_db();
        let owner = fresh_pair(dir.path(), "owner.pk8");
        let impostor = fresh_pair(dir.path(), "impostor.pk8");
        check(&db, "dev-a", b"one", &props_of(&owner, b"one"));
        assert_eq!(
            check(&db, "dev-a", b"two", &props_of(&impostor, b"two")),
            Verdict::KeyChanged
        );
    }

    #[test]
    fn unsigned_is_benign_until_a_device_has_signed() {
        let (dir, db) = test_db();
        assert_eq!(
            check(&db, "dev-a", b"x", &[]),
            Verdict::Missing { pinned: false }
        );
        let pair = fresh_pair(dir.path(), "a.pk8");
        check(&db, "dev-a", b"x", &props_of(&pair, b"x"));
        assert_eq!(
            check(&db, "dev-a", b"y", &[]),
            Verdict::Missing { pinned: true }
        );
    }

    #[test]
    fn observe_records_each_transition_once() {
        let (dir, db) = test_db();
        let pair = fresh_pair(dir.path(), "a.pk8");
        observe(&db, "dev-a", b"x", &[]);
        assert_eq!(
            safe_kv_get(&db, "relay_sigstatus_dev-a").as_deref(),
            Some("unsigned (older build)")
        );
        observe(&db, "dev-a", b"x", &props_of(&pair, b"x"));
        observe(&db, "dev-a", b"y", &props_of(&pair, b"y"));
        assert_eq!(
            safe_kv_get(&db, "relay_sigstatus_dev-a").as_deref(),
            Some("valid")
        );
        let lines = status_lines(&db);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("valid key "), "{lines:?}");
    }
}
