//! MQTT relay for cross-device synchronization.
//!
//! The relay syncs instance state and events across devices via MQTT pub/sub.
//!
//! Topic layout:
//!   {relay_id}/{device_uuid}  — retained state per device
//!   {relay_id}/control        — non-retained control events (stop/kill)

pub mod broker;
pub mod client;
pub mod control;
pub mod crypto;
pub mod pull;
pub mod push;
pub mod replay;
pub mod token;
pub mod worker;

pub use worker::observe_pid_file;

use crate::config::HcomConfig;
use crate::db::HcomDb;
use crate::instance_names;

/// Public MQTT brokers (TLS, port 8883/8886). Tried in order during initial setup;
/// first success gets pinned to config. Append-only (never insert/reorder) to preserve
/// v0x01 token compatibility.
pub const DEFAULT_BROKERS: &[(&str, u16)] = &[
    ("broker.emqx.io", 8883),
    ("broker.hivemq.com", 8883),
    ("test.mosquitto.org", 8886),
];

/// Threshold (seconds) after which a device with no state updates is considered offline.
/// Used for reconnect detection, stale-device cleanup, and status display.
pub const DEVICE_STALE_SECS: f64 = 90.0;

/// KV key where the relay worker writes a monotonically-increasing epoch timestamp
/// on every event-loop tick. Readers use this as the authoritative liveness signal
/// — it survives unclean exit (SIGKILL/panic) because a dead process can't refresh it,
/// which the pidfile alone cannot detect (PID reuse, bash launcher collision).
pub const HEARTBEAT_KEY: &str = "relay_worker_heartbeat";

/// Heartbeat older than this is considered stale — the worker is either dead or
/// wedged. Worker ticks heartbeat every ~1s, so 10s tolerates normal jitter while
/// flagging real unresponsiveness well before users notice queue backups.
pub const HEARTBEAT_STALE_SECS: f64 = 10.0;

/// Life event actions for device join/leave notifications.
pub const ACTION_DEVICE_JOIN: &str = "relay_device_join";
pub const ACTION_DEVICE_LEAVE: &str = "relay_device_leave";

/// Truncate a device UUID to its first 8 characters for logging.
pub fn device_id_prefix(device_id: &str) -> &str {
    &device_id[..8.min(device_id.len())]
}

/// Check if relay is configured AND enabled (relay_id set + relay_enabled flag).
pub fn is_relay_enabled(config: &HcomConfig) -> bool {
    !config.relay_id.is_empty() && config.relay_enabled
}

/// Decode the configured 32-byte PSK from the base64 stored in `relay.psk`.
///
/// Returns `Err` if the field is empty or malformed. Callers that need to
/// publish or open envelopes (worker, control sender) treat this as fatal —
/// the user must rerun `hcom relay new` to regenerate a key.
pub fn load_psk(config: &HcomConfig) -> Result<[u8; 32], String> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    if config.relay_psk.is_empty() {
        return Err(
            "relay key not set — run `hcom relay new` to generate one and re-share the token"
                .to_string(),
        );
    }
    let trimmed = config.relay_psk.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD
        .decode(trimmed.as_bytes())
        .map_err(|e| format!("relay key is not valid base64: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "relay key must be 32 bytes after decoding, got {}",
            bytes.len()
        ));
    }
    let mut psk = [0u8; 32];
    psk.copy_from_slice(&bytes);
    Ok(psk)
}

/// Encode a 32-byte PSK as base64url (no padding) for storage in config.toml.
pub fn encode_psk(psk: &[u8; 32]) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(psk)
}

/// State topic: {relay_id}/{device_uuid} — retained, one per device.
pub fn state_topic(relay_id: &str, device_uuid: &str) -> String {
    format!("{}/{}", relay_id, device_uuid)
}

/// Control topic: {relay_id}/control — non-retained, shared.
pub fn control_topic(relay_id: &str) -> String {
    format!("{}/control", relay_id)
}

/// Wildcard subscription: {relay_id}/+ (matches all device + control topics).
pub fn wildcard_topic(relay_id: &str) -> String {
    format!("{}/+", relay_id)
}

/// Parse broker URL into (host, port, use_tls).
/// Supports mqtts://host:port, mqtt://host:port, or bare host:port.
pub fn parse_broker_url(url: &str) -> Option<(String, u16, bool)> {
    if url.is_empty() {
        return None;
    }
    let use_tls = !url.starts_with("mqtt://");
    let stripped = url
        .trim_start_matches("mqtts://")
        .trim_start_matches("mqtt://");
    let (host, port) = if let Some(colon_pos) = stripped.rfind(':') {
        let host = &stripped[..colon_pos];
        let port = stripped[colon_pos + 1..].parse::<u16>().ok()?;
        (host.to_string(), port)
    } else {
        (stripped.to_string(), if use_tls { 8883 } else { 1883 })
    };
    Some((host, port, use_tls))
}

/// Get broker (host, port, use_tls) from config. Returns None if relay not configured.
pub fn get_broker_from_config(config: &HcomConfig) -> Option<(String, u16, bool)> {
    if !is_relay_enabled(config) {
        return None;
    }
    if config.relay.is_empty() {
        return None;
    }
    parse_broker_url(&config.relay)
}

/// Get or create persistent device UUID
/// Reads from ~/.hcom/.tmp/device_id; creates with a new UUID if missing or empty.
///
/// Returns None only on genuine I/O failure (cannot create parent dir, cannot
/// acquire lock, cannot persist UUID). Concurrent callers are serialized via
/// flock on a sibling lock file, so the loser observes the winner's persisted
/// UUID rather than racing to generate a divergent one. An existing empty file
/// is treated as missing and refilled under lock — recovers from prior aborted
/// writes that left a 0-byte file.
pub fn read_device_uuid() -> Option<String> {
    let path = crate::paths::hcom_dir().join(".tmp").join("device_id");
    read_or_create_device_uuid_at(&path)
}

/// Path-parameterized core of `read_device_uuid`. Split out so tests can drive
/// it against a tempdir path without touching the global Config / HCOM_DIR env.
fn read_or_create_device_uuid_at(path: &std::path::Path) -> Option<String> {
    // Fast path: file already populated.
    if let Some(uuid) = read_nonempty(path) {
        return Some(uuid);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }

    // Serialize creation across processes via flock on a sibling lock file.
    // Mirrors the pattern in instance_names::generate_unique_name.
    let lock_path = path.with_file_name("device_id.lock");
    let lock_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;

    use nix::fcntl::{Flock, FlockArg};
    let _flock = Flock::lock(lock_file, FlockArg::LockExclusive).ok()?;

    // Re-check under lock — a concurrent caller may have written by now.
    if let Some(uuid) = read_nonempty(path) {
        return Some(uuid);
    }

    // We hold the lock; the file is missing or empty. Generate and persist.
    let device_id = uuid::Uuid::new_v4().to_string();
    if !crate::paths::atomic_write(path, &device_id) {
        return None;
    }
    Some(device_id)
}

/// Read a file and return its trimmed content if non-empty.
fn read_nonempty(path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Get device short ID — FNV-1a hash to CVCV word, uppercased.
pub fn device_short_id(device_uuid: &str) -> String {
    instance_names::hash_to_name(device_uuid, 0).to_uppercase()
}

/// Add device short ID suffix to a name (e.g., "luna" → "luna:XABC").
pub fn add_device_suffix(name: &str, short_id: &str) -> String {
    format!("{}:{}", name, short_id)
}

/// Safe KV get that won't crash on DB errors.
pub(crate) fn safe_kv_get(db: &HcomDb, key: &str) -> Option<String> {
    db.kv_get(key).ok().flatten()
}

/// Safe KV set that won't crash on DB errors.
pub(crate) fn safe_kv_set(db: &HcomDb, key: &str, value: Option<&str>) {
    let _ = db.kv_set(key, value);
}

/// Record a fresh worker heartbeat. Called by the worker's main loop ~once per second
/// and once at pidfile-write so the startup window doesn't look stale.
pub(crate) fn write_worker_heartbeat(db: &HcomDb) {
    let now = crate::shared::time::now_epoch_f64();
    safe_kv_set(db, HEARTBEAT_KEY, Some(&format!("{now}")));
}

/// Clear the heartbeat on clean worker shutdown so readers see "no worker" immediately
/// instead of waiting out HEARTBEAT_STALE_SECS.
pub(crate) fn clear_worker_heartbeat(db: &HcomDb) {
    safe_kv_set(db, HEARTBEAT_KEY, None);
}

/// Age in seconds of the last heartbeat write, or None if no heartbeat is recorded.
pub fn worker_heartbeat_age(db: &HcomDb) -> Option<f64> {
    let ts: f64 = safe_kv_get(db, HEARTBEAT_KEY)?.parse().ok()?;
    let now = crate::shared::time::now_epoch_f64();
    Some((now - ts).max(0.0))
}

/// True iff a heartbeat is recorded and it is younger than HEARTBEAT_STALE_SECS.
pub fn is_worker_heartbeat_fresh(db: &HcomDb) -> bool {
    worker_heartbeat_age(db).is_some_and(|age| age < HEARTBEAT_STALE_SECS)
}

/// Clear all per-device relay KV entries and reset global relay counters.
/// Called on `relay new` so stale device mappings from the previous relay group
/// don't contaminate the new one.
pub fn clear_relay_device_state(db: &HcomDb) {
    let prefixes = [
        "relay_short_",
        "relay_caps_",
        "relay_events_",
        "relay_reset_",
        "relay_sync_time_",
        "relay_state_ts_",
        "relay_ctrl_",
    ];
    for prefix in &prefixes {
        if let Ok(entries) = db.kv_prefix(prefix) {
            for (key, _) in entries {
                safe_kv_set(db, &key, None);
            }
        }
    }
    // Reset global counters
    for key in &[
        "relay_device_count",
        "relay_last_push",
        "relay_last_push_id",
        "relay_last_sync",
        "relay_local_reset_ts",
    ] {
        safe_kv_set(db, key, None);
    }
    // Remove remote instances from the old relay
    let _ = db.conn().execute(
        "DELETE FROM instances WHERE origin_device_id IS NOT NULL AND origin_device_id != ''",
        [],
    );
}

// ── Relay health: derived effective state ──────────────────────────
//
// Two types split responsibility cleanly:
//   RelayObservation — pure snapshot of all relay-related signals (config flags,
//     raw KV, pidfile state, daemon port). Readers produce one with `observe_relay`.
//   RelayHealth — the derived effective answer every UI surface renders. Computed
//     purely from RelayObservation by `derive_relay_health` — no filesystem, no
//     DB access, no side effects. Unit tests assemble an observation by hand and
//     assert the enum variant.
//
// The split exists so "what should we display" lives in exactly one function,
// not duplicated across commands/status.rs, commands/relay.rs, and tui/render.

/// Values the worker writes to `relay_status` KV. Constants instead of literals
/// so derivation precedence and KV producers can't drift on a typo.
pub const RAW_STATUS_OK: &str = "ok";
pub const RAW_STATUS_ERROR: &str = "error";

/// Why a relay is in error, for the `RelayHealth::Error` variant. Readers can
/// branch on this for nicer wording without having to reparse `detail`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayErrorReason {
    /// Worker wrote `relay_status=error` itself — most commonly an MQTT auth or
    /// broker disconnect. `detail` carries the message the worker stored.
    Reported,
    /// Pidfile points at a PID that is no longer alive. Worker crashed without
    /// running PidFileGuard::drop (SIGKILL, OOM, panic during Drop).
    StalePidfile,
    /// No worker process, no heartbeat, but `relay_status=ok` is still in KV.
    /// The worker was reaped and nothing flipped the status — the "false-green"
    /// bug that motivated this whole machinery.
    Ghost,
}

impl RelayErrorReason {
    /// Render this reason as the user-facing detail string, given the optional
    /// pid the derive layer attached. Lives here so commands/status.rs and
    /// commands/relay.rs can't drift on the wording.
    pub fn label(self, detail: Option<&str>, pid: Option<u32>) -> String {
        match self {
            RelayErrorReason::Reported => detail
                .map(str::to_string)
                .unwrap_or_else(|| "unknown error".to_string()),
            RelayErrorReason::StalePidfile => match pid {
                Some(p) => format!("stale pidfile (PID {p} not running)"),
                None => "stale pidfile".to_string(),
            },
            RelayErrorReason::Ghost => {
                "worker died without clearing status (no process, no heartbeat)".to_string()
            }
        }
    }
}

/// Effective relay state. Every user-facing surface should match on this and
/// render accordingly. Raw KV values live in `RelayObservation` for forensics.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayHealth {
    /// No relay configured (`relay_id` empty).
    NotConfigured,
    /// Relay configured but disabled (`relay_enabled = false`).
    Disabled,
    /// Enabled but nothing is running and nothing is wrong — cold-start before
    /// first auto-spawn, or quiescent after a clean shutdown.
    Waiting,
    /// Worker process exists and is alive but hasn't produced a heartbeat yet
    /// (startup window between `write_pid_file` and the first main-loop tick).
    Starting { pid: u32 },
    /// Worker is alive, heartbeat is fresh, and the worker last self-reported
    /// `relay_status=ok` — the only "all good" variant. Carries no payload:
    /// the heartbeat age changes every tick by construction, so storing it
    /// here would defeat enum-equality short-circuiting in render diffing.
    /// Forensic age is in JSON's `raw.heartbeat_age_s`.
    Connected,
    /// Worker process alive but heartbeat is older than `HEARTBEAT_STALE_SECS`
    /// — main loop is wedged (DB contention, deadlock) but hasn't died.
    Stale { age_s: f64, pid: u32 },
    /// Something's wrong; see `reason` and `detail` for why.
    Error {
        reason: RelayErrorReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
}

/// Pure snapshot of every observable relay signal. `derive_relay_health`
/// consumes this; tests assemble it by hand.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayObservation {
    pub configured: bool,
    pub enabled: bool,
    pub raw_status: Option<String>,
    pub raw_error: Option<String>,
    pub heartbeat_age_s: Option<f64>,
    /// `(pid, is_alive)` when a pidfile exists, else `None`. Split so the
    /// derivation can distinguish "no pidfile" (legitimate idle) from "pidfile
    /// points at dead PID" (worker crashed without cleanup).
    pub pidfile: Option<(u32, bool)>,
    pub last_push: f64,
    pub broker: Option<String>,
}

/// Read every relay signal from config + DB + filesystem into a plain struct.
/// Pure read — no KV writes, no pidfile cleanup. Call `derive_relay_health`
/// on the result to get the effective state.
pub fn observe_relay(config: &HcomConfig, db: &HcomDb) -> RelayObservation {
    RelayObservation {
        configured: !config.relay_id.is_empty(),
        enabled: config.relay_enabled,
        raw_status: safe_kv_get(db, "relay_status"),
        raw_error: safe_kv_get(db, "relay_last_error"),
        heartbeat_age_s: worker_heartbeat_age(db),
        pidfile: crate::relay::worker::observe_pid_file(),
        last_push: safe_kv_get(db, "relay_last_push")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0),
        broker: if config.relay.is_empty() {
            None
        } else {
            Some(config.relay.clone())
        },
    }
}

/// Derive effective health from an observation. Pure — no I/O, no mutation.
///
/// Precedence (top-down, first match wins):
///   1. not configured                                       → NotConfigured
///   2. !enabled                                             → Disabled
///   3. raw_status="error"                                   → Error(Reported, raw_error)
///   4. pidfile present, pid dead                            → Error(StalePidfile, pid)
///   5. pidfile present, pid alive, heartbeat missing        → Starting { pid }
///   6. pidfile present, pid alive, heartbeat stale          → Stale { age, pid }
///   7. pidfile present, pid alive, heartbeat fresh, ok      → Connected { age }
///   8. pidfile present, pid alive, heartbeat fresh, !ok     → Starting { pid }
///   9. no pidfile, raw_status="ok" (or fresh heartbeat)     → Error(Ghost)
///   10. no pidfile, anything else                           → Waiting
pub fn derive_relay_health(obs: &RelayObservation) -> RelayHealth {
    if !obs.configured {
        return RelayHealth::NotConfigured;
    }
    if !obs.enabled {
        return RelayHealth::Disabled;
    }

    // A worker that wrote "error" itself is authoritative — that's the freshest
    // signal about whether MQTT is working, independent of whether the loop is
    // ticking heartbeats. Fresh heartbeat + raw=error = worker is alive and
    // retrying, but last observed state is broken. User-facing: Error.
    if obs.raw_status.as_deref() == Some(RAW_STATUS_ERROR) {
        return RelayHealth::Error {
            reason: RelayErrorReason::Reported,
            detail: obs.raw_error.clone(),
            pid: obs.pidfile.map(|(p, _)| p),
        };
    }

    match obs.pidfile {
        Some((pid, false)) => RelayHealth::Error {
            reason: RelayErrorReason::StalePidfile,
            detail: None,
            pid: Some(pid),
        },
        Some((pid, true)) => match obs.heartbeat_age_s {
            None => RelayHealth::Starting { pid },
            Some(age) if age >= HEARTBEAT_STALE_SECS => RelayHealth::Stale { age_s: age, pid },
            Some(_) => {
                if obs.raw_status.as_deref() == Some(RAW_STATUS_OK) {
                    RelayHealth::Connected
                } else {
                    // Ticking but no ConnAck yet — still coming up.
                    RelayHealth::Starting { pid }
                }
            }
        },
        None => {
            // No process evidence. "ok" in KV or a lingering fresh heartbeat
            // without a pidfile means the worker died ungracefully and the
            // runtime state wasn't cleared — the ghost case.
            let hb_fresh = obs
                .heartbeat_age_s
                .is_some_and(|age| age < HEARTBEAT_STALE_SECS);
            if hb_fresh || obs.raw_status.as_deref() == Some(RAW_STATUS_OK) {
                RelayHealth::Error {
                    reason: RelayErrorReason::Ghost,
                    detail: obs.raw_error.clone(),
                    pid: None,
                }
            } else {
                // Missing / "disconnected" / empty status, no pid, no heartbeat.
                // Clean idle — either pre-first-spawn or post-clean-shutdown.
                RelayHealth::Waiting
            }
        }
    }
}

/// Convenience wrapper for callers that just want the answer.
pub fn relay_health(config: &HcomConfig, db: &HcomDb) -> RelayHealth {
    derive_relay_health(&observe_relay(config, db))
}

/// Runtime-health KV keys cleared on relay disable. Deliberately excludes
/// activity/watermark keys (`relay_last_push`, `relay_last_push_id`,
/// `relay_last_sync`) — those are correctness invariants for re-enable in the
/// same group (watermark tells the worker what's already been pushed). Group
/// rotation nukes those separately via `clear_relay_device_state`.
const RUNTIME_HEALTH_KV_KEYS: &[&str] = &[
    "relay_status",
    "relay_last_error",
    "relay_status_owner",
    "relay_daemon_port",
    "relay_daemon_fail_count",
    HEARTBEAT_KEY,
];

/// Clear runtime-health KV when the subsystem transitions off. Keeps activity
/// watermarks so a subsequent `relay on` doesn't re-push already-synced events.
pub fn clear_runtime_relay_kv(db: &HcomDb) {
    for key in RUNTIME_HEALTH_KV_KEYS {
        safe_kv_set(db, key, None);
    }
}

/// Relay status for TUI/CLI display. Bundles the derived health with the raw
/// observation so callers can render the canonical answer and still show raw
/// fields for debugging.
#[derive(Debug, Clone)]
pub struct RelayStatus {
    pub configured: bool,
    pub enabled: bool,
    pub status: Option<String>,
    pub error: Option<String>,
    pub last_push: f64,
    pub broker: Option<String>,
    /// Age of the most recent worker heartbeat, in seconds. None if no heartbeat
    /// has ever been recorded.
    pub heartbeat_age: Option<f64>,
    /// PID recorded in the pidfile (regardless of liveness). Carried so JSON
    /// callers can populate `raw.pid` without re-observing the pidfile —
    /// derive already paid for the read.
    pub pidfile_pid: Option<u32>,
    /// Derived effective state. All user-facing surfaces should match on this.
    pub health: RelayHealth,
}

/// Get relay status from config + DB.
pub fn get_relay_status(config: &HcomConfig, db: &HcomDb) -> RelayStatus {
    let obs = observe_relay(config, db);
    let health = derive_relay_health(&obs);
    RelayStatus {
        configured: obs.configured,
        enabled: obs.enabled,
        status: obs.raw_status.clone(),
        error: obs.raw_error.clone(),
        last_push: obs.last_push,
        broker: obs.broker.clone(),
        heartbeat_age: obs.heartbeat_age_s,
        pidfile_pid: obs.pidfile.map(|(p, _)| p),
        health,
    }
}

/// Check if daemon is actively handling relay polling.
///
/// Validates port is actually reachable via TCP probe to handle stale ports from crashed daemons.
/// Only clears port after 3 consecutive failures to avoid stampede from transient timeouts.
pub fn is_relay_handled_by_daemon(db: &HcomDb) -> bool {
    let port_str = match safe_kv_get(db, "relay_daemon_port") {
        Some(p) => p,
        None => return false,
    };
    let port: u16 = match port_str.trim().parse() {
        Ok(p) => p,
        Err(_) => return false,
    };

    // TCP probe with 100ms timeout
    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(100)) {
        Ok(_) => {
            safe_kv_set(db, "relay_daemon_fail_count", None); // Reset on success
            true
        }
        Err(_) => {
            // Atomic increment via SQL — only clear after 3 consecutive failures
            if let Ok(()) = db.conn().execute_batch(
                "INSERT INTO kv (key, value) VALUES ('relay_daemon_fail_count', '1') \
                 ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            ) {
                let fail_count: i64 = db
                    .conn()
                    .query_row(
                        "SELECT value FROM kv WHERE key = 'relay_daemon_fail_count'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(1);
                if fail_count >= 3 {
                    safe_kv_set(db, "relay_daemon_port", None);
                    safe_kv_set(db, "relay_daemon_fail_count", None);
                }
            }
            false
        }
    }
}

/// Notify the relay daemon to push immediately via TCP connect.
/// Returns true if daemon was successfully notified.
pub fn notify_relay_daemon() -> bool {
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(_) => return false,
    };
    let port_str = match safe_kv_get(&db, "relay_daemon_port") {
        Some(p) => p,
        None => return false,
    };
    let port: u16 = match port_str.trim().parse() {
        Ok(p) => p,
        Err(_) => return false,
    };

    use std::net::{SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(50))
        .map(|_| true) // Connection itself is the signal; drop closes it
        .unwrap_or(false)
}

/// Notify the relay daemon to push immediately.
/// Handles three cases:
///  - Daemon running and ready: TCP notify succeeds, returns immediately.
///  - Daemon running but in startup window (port not yet bound): retries notify for up
///    to 150ms so the push isn't delayed to the worker's next periodic cycle.
///  - No daemon: spawns one (fire-and-forget); events push on its first cycle.
pub fn trigger_push() {
    if notify_relay_daemon() {
        return;
    }
    if worker::is_relay_worker_running() {
        // Startup window: worker is running but port not yet in KV.
        // Retry notify briefly before giving up.
        let start = std::time::Instant::now();
        let limit = std::time::Duration::from_millis(150);
        while start.elapsed() < limit {
            std::thread::sleep(std::time::Duration::from_millis(30));
            if notify_relay_daemon() {
                return;
            }
        }
        // Still not ready — events push on the worker's next periodic cycle.
    } else {
        // No worker — spawn one so events push on its first cycle.
        worker::try_spawn_worker();
    }
}

/// Wait for relay data. Returns true if new remote events arrived in DB.
/// Detects new events by the `:` in instance name (relay-imported events use
/// namespaced names like `luna:ABCD`). Breaks if relay import naming changes.
pub fn relay_wait(timeout_secs: f64) -> bool {
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(_) => return false,
    };

    let before: i64 = db
        .conn()
        .query_row(
            "SELECT COALESCE(MAX(id), 0) FROM events WHERE instance LIKE '%:%'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    std::thread::sleep(std::time::Duration::from_secs_f64(timeout_secs.min(1.0)));

    let after: i64 = db
        .conn()
        .query_row(
            "SELECT COALESCE(MAX(id), 0) FROM events WHERE instance LIKE '%:%'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    after > before
}

/// Set relay status in DB KV with PID ownership guard.
///
/// `is_worker` should be true for daemon relay threads, false for CLI callers.
/// Non-worker callers bail if a daemon is actively handling relay (relay_daemon_port set).
/// On "ok", the caller claims ownership via relay_status_owner PID.
/// On error, only the owning PID (or non-daemon callers) can write.
pub fn set_relay_status(db: &HcomDb, status: &str, error: Option<&str>, is_worker: bool) {
    let pid = std::process::id().to_string();
    let daemon_active = if !is_worker {
        is_relay_handled_by_daemon(db)
    } else {
        false
    };

    // Non-worker callers bail if daemon is active
    if !is_worker && daemon_active {
        return;
    }

    if status == RAW_STATUS_OK {
        // Claim ownership and clear error
        safe_kv_set(db, "relay_status_owner", Some(&pid));
        safe_kv_set(db, "relay_status", Some(RAW_STATUS_OK));
        safe_kv_set(db, "relay_last_error", None);
    } else {
        // Only write error if we own the status or daemon isn't active
        let owner = safe_kv_get(db, "relay_status_owner");
        if owner.as_deref() == Some(&pid) || !daemon_active {
            safe_kv_set(db, "relay_status", Some(status));
            match error {
                Some(e) => safe_kv_set(db, "relay_last_error", Some(e)),
                None => safe_kv_set(db, "relay_last_error", None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_broker_url_mqtts() {
        let (host, port, tls) = parse_broker_url("mqtts://broker.emqx.io:8883").unwrap();
        assert_eq!(host, "broker.emqx.io");
        assert_eq!(port, 8883);
        assert!(tls);
    }

    #[test]
    fn test_parse_broker_url_mqtt() {
        let (host, port, tls) = parse_broker_url("mqtt://localhost:1883").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 1883);
        assert!(!tls);
    }

    #[test]
    fn test_parse_broker_url_default_port() {
        let (host, port, tls) = parse_broker_url("mqtts://broker.emqx.io").unwrap();
        assert_eq!(host, "broker.emqx.io");
        assert_eq!(port, 8883);
        assert!(tls);
    }

    #[test]
    fn test_parse_broker_url_empty() {
        assert!(parse_broker_url("").is_none());
    }

    #[test]
    fn test_topics() {
        assert_eq!(
            state_topic("relay-123", "device-abc"),
            "relay-123/device-abc"
        );
        assert_eq!(control_topic("relay-123"), "relay-123/control");
        assert_eq!(wildcard_topic("relay-123"), "relay-123/+");
    }

    #[test]
    fn test_device_short_id() {
        // Uses hash_to_name (FNV-1a → CVCV word),
        assert_eq!(device_short_id("abcd-1234-efgh"), "VUNO");
        assert_eq!(device_short_id("12345678"), "MOVA");
        assert_eq!(device_short_id("device-123"), "REVA");
    }

    #[test]
    fn test_is_relay_enabled() {
        let mut config = HcomConfig::default();
        // Default: relay_id empty, relay_enabled true → not enabled
        assert!(!is_relay_enabled(&config));

        config.relay_id = "some-id".to_string();
        assert!(is_relay_enabled(&config));

        config.relay_enabled = false;
        assert!(!is_relay_enabled(&config));
    }

    #[test]
    fn test_read_device_uuid_creates_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".tmp").join("device_id");
        let uuid = read_or_create_device_uuid_at(&path).expect("should create");
        assert!(!uuid.is_empty());
        // Subsequent call must return the SAME persisted UUID.
        let again = read_or_create_device_uuid_at(&path).expect("should read");
        assert_eq!(uuid, again);
    }

    #[test]
    fn test_read_device_uuid_repairs_empty_file() {
        // Regression: prior implementation used create_new which refused to
        // replace an existing-but-empty file, so a 0-byte device_id (left by
        // an aborted write) caused permanent None.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("device_id");
        std::fs::write(&path, "").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        let uuid = read_or_create_device_uuid_at(&path).expect("should repair");
        assert!(!uuid.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), uuid);
    }

    #[test]
    fn test_read_device_uuid_repairs_whitespace_only_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("device_id");
        std::fs::write(&path, "   \n\t  ").unwrap();

        let uuid = read_or_create_device_uuid_at(&path).expect("should repair");
        assert!(!uuid.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), uuid);
    }

    #[test]
    fn test_read_device_uuid_concurrent_first_callers_agree() {
        // Regression: concurrent first callers used to each generate their own
        // UUID, return their own in-memory copy, and disagree on disk. With
        // flock + read-under-lock, all callers must observe the SAME UUID.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".tmp").join("device_id");

        let n = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(n));
        let path_arc = std::sync::Arc::new(path.clone());

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let b = std::sync::Arc::clone(&barrier);
                let p = std::sync::Arc::clone(&path_arc);
                std::thread::spawn(move || {
                    b.wait();
                    read_or_create_device_uuid_at(&p)
                })
            })
            .collect();

        let results: Vec<Option<String>> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let first = results[0].as_ref().expect("at least one must succeed");
        for (i, r) in results.iter().enumerate() {
            let r = r
                .as_ref()
                .unwrap_or_else(|| panic!("thread {i} returned None"));
            assert_eq!(
                r, first,
                "thread {i} got divergent UUID — race not serialized"
            );
        }
        // And the persisted file matches.
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), first);
    }

    // ── RelayHealth derivation matrix ────────────────────────────────
    //
    // Each test pins one cell of the 10-row precedence matrix agreed with
    // dinu. Keep names aligned to precedence rules for grep-friendly failure
    // output. The `obs()` helper builds an Enabled observation; individual
    // tests tweak only the fields under test.

    fn obs() -> RelayObservation {
        RelayObservation {
            configured: true,
            enabled: true,
            raw_status: None,
            raw_error: None,
            heartbeat_age_s: None,
            pidfile: None,
            last_push: 0.0,
            broker: None,
        }
    }

    #[test]
    fn derive_01_not_configured() {
        let o = RelayObservation {
            configured: false,
            enabled: true, // shouldn't matter
            ..obs()
        };
        assert_eq!(derive_relay_health(&o), RelayHealth::NotConfigured);
    }

    #[test]
    fn derive_02_disabled_takes_precedence_over_runtime_state() {
        // If disable didn't clear runtime KV we'd still see raw_status=ok here;
        // Disabled must short-circuit regardless.
        let o = RelayObservation {
            enabled: false,
            raw_status: Some("ok".into()),
            heartbeat_age_s: Some(1.0),
            pidfile: Some((12345, true)),
            ..obs()
        };
        assert_eq!(derive_relay_health(&o), RelayHealth::Disabled);
    }

    #[test]
    fn derive_03_reported_error_wins_over_pid_and_heartbeat() {
        // Worker self-report is authoritative — fresh heartbeat doesn't rescue us.
        let o = RelayObservation {
            raw_status: Some("error".into()),
            raw_error: Some("not authorized".into()),
            heartbeat_age_s: Some(0.5),
            pidfile: Some((4242, true)),
            ..obs()
        };
        match derive_relay_health(&o) {
            RelayHealth::Error {
                reason,
                detail,
                pid,
            } => {
                assert_eq!(reason, RelayErrorReason::Reported);
                assert_eq!(detail.as_deref(), Some("not authorized"));
                assert_eq!(pid, Some(4242));
            }
            other => panic!("expected Error(Reported), got {other:?}"),
        }
    }

    #[test]
    fn derive_04_pidfile_present_pid_dead_is_stale_pidfile_error() {
        let o = RelayObservation {
            pidfile: Some((9999, false)),
            ..obs()
        };
        match derive_relay_health(&o) {
            RelayHealth::Error { reason, pid, .. } => {
                assert_eq!(reason, RelayErrorReason::StalePidfile);
                assert_eq!(pid, Some(9999));
            }
            other => panic!("expected Error(StalePidfile), got {other:?}"),
        }
    }

    #[test]
    fn derive_05_pid_alive_heartbeat_missing_is_starting() {
        let o = RelayObservation {
            pidfile: Some((111, true)),
            heartbeat_age_s: None,
            ..obs()
        };
        assert_eq!(derive_relay_health(&o), RelayHealth::Starting { pid: 111 });
    }

    #[test]
    fn derive_06_pid_alive_heartbeat_stale_is_stale() {
        let o = RelayObservation {
            pidfile: Some((222, true)),
            heartbeat_age_s: Some(HEARTBEAT_STALE_SECS + 5.0),
            ..obs()
        };
        match derive_relay_health(&o) {
            RelayHealth::Stale { age_s, pid } => {
                assert!(age_s >= HEARTBEAT_STALE_SECS);
                assert_eq!(pid, 222);
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn derive_07_pid_alive_heartbeat_fresh_status_ok_is_connected() {
        let o = RelayObservation {
            pidfile: Some((333, true)),
            heartbeat_age_s: Some(0.5),
            raw_status: Some(RAW_STATUS_OK.into()),
            ..obs()
        };
        assert_eq!(derive_relay_health(&o), RelayHealth::Connected);
    }

    #[test]
    fn derive_connected_is_stable_across_heartbeat_ticks() {
        // Render diffing relies on PartialEq short-circuiting when health hasn't
        // meaningfully changed. If Connected carried the heartbeat age, every
        // 1Hz tick would re-render the relay indicator for nothing.
        let mk = |age: f64| RelayObservation {
            pidfile: Some((1, true)),
            heartbeat_age_s: Some(age),
            raw_status: Some(RAW_STATUS_OK.into()),
            ..obs()
        };
        assert_eq!(derive_relay_health(&mk(0.1)), derive_relay_health(&mk(8.5)));
    }

    #[test]
    fn derive_08_pid_alive_heartbeat_fresh_status_not_ok_is_starting() {
        // Covers the startup window: worker is ticking but hasn't received ConnAck.
        // raw_status is empty or "disconnected" — anything except "ok" or "error".
        let o = RelayObservation {
            pidfile: Some((444, true)),
            heartbeat_age_s: Some(0.2),
            raw_status: None,
            ..obs()
        };
        assert_eq!(derive_relay_health(&o), RelayHealth::Starting { pid: 444 });

        // Also cover explicit "disconnected" sentinel in case any code path writes it.
        let o = RelayObservation {
            pidfile: Some((445, true)),
            heartbeat_age_s: Some(0.2),
            raw_status: Some("disconnected".into()),
            ..obs()
        };
        assert_eq!(derive_relay_health(&o), RelayHealth::Starting { pid: 445 });
    }

    #[test]
    fn derive_09_no_pid_status_ok_is_ghost_error() {
        // The pone bug: worker reaped, status KV never flipped to error.
        let o = RelayObservation {
            pidfile: None,
            heartbeat_age_s: None,
            raw_status: Some("ok".into()),
            ..obs()
        };
        match derive_relay_health(&o) {
            RelayHealth::Error { reason, pid, .. } => {
                assert_eq!(reason, RelayErrorReason::Ghost);
                assert_eq!(pid, None);
            }
            other => panic!("expected Error(Ghost), got {other:?}"),
        }
    }

    #[test]
    fn derive_09b_no_pid_fresh_heartbeat_is_ghost() {
        // Heartbeat without pidfile is anomalous — treat as ghost.
        let o = RelayObservation {
            pidfile: None,
            heartbeat_age_s: Some(0.5),
            raw_status: None,
            ..obs()
        };
        match derive_relay_health(&o) {
            RelayHealth::Error { reason, .. } => {
                assert_eq!(reason, RelayErrorReason::Ghost);
            }
            other => panic!("expected Error(Ghost), got {other:?}"),
        }
    }

    #[test]
    fn derive_10_no_pid_no_heartbeat_no_status_is_waiting() {
        // Cold start or clean post-shutdown idle.
        let o = RelayObservation {
            pidfile: None,
            heartbeat_age_s: None,
            raw_status: None,
            ..obs()
        };
        assert_eq!(derive_relay_health(&o), RelayHealth::Waiting);

        // "disconnected" (or any non-ok, non-error status) with no pid also Waiting.
        let o = RelayObservation {
            pidfile: None,
            heartbeat_age_s: None,
            raw_status: Some("disconnected".into()),
            ..obs()
        };
        assert_eq!(derive_relay_health(&o), RelayHealth::Waiting);
    }

    // ── disable clears runtime KV, preserves activity watermarks ────

    fn test_db() -> HcomDb {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        std::mem::forget(dir);
        db
    }

    #[test]
    fn clear_runtime_relay_kv_nukes_runtime_health_fields() {
        let db = test_db();
        for key in RUNTIME_HEALTH_KV_KEYS {
            safe_kv_set(&db, key, Some("present"));
        }
        clear_runtime_relay_kv(&db);
        for key in RUNTIME_HEALTH_KV_KEYS {
            assert!(
                safe_kv_get(&db, key).is_none(),
                "{key} should be cleared after disable"
            );
        }
    }

    #[test]
    fn clear_runtime_relay_kv_preserves_activity_watermarks() {
        // Regression guard: relay_last_push_id is a broker watermark. Clearing
        // it on disable would cause re-push of already-synced events when the
        // user toggles relay back on inside the same group.
        let db = test_db();
        safe_kv_set(&db, "relay_last_push_id", Some("12345"));
        safe_kv_set(&db, "relay_last_push", Some("1700000000.0"));
        safe_kv_set(&db, "relay_last_sync", Some("1700000001.0"));
        safe_kv_set(&db, "relay_status", Some("ok"));

        clear_runtime_relay_kv(&db);

        assert_eq!(
            safe_kv_get(&db, "relay_last_push_id").as_deref(),
            Some("12345")
        );
        assert_eq!(
            safe_kv_get(&db, "relay_last_push").as_deref(),
            Some("1700000000.0")
        );
        assert_eq!(
            safe_kv_get(&db, "relay_last_sync").as_deref(),
            Some("1700000001.0")
        );
        assert!(safe_kv_get(&db, "relay_status").is_none());
    }
}
