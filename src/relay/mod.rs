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
pub mod pull;
pub mod push;
pub mod token;
pub mod worker;

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

/// Check if relay is configured AND enabled (relay_id set + relay_enabled flag).
pub fn is_relay_enabled(config: &HcomConfig) -> bool {
    !config.relay_id.is_empty() && config.relay_enabled
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
/// Reads from ~/.hcom/.tmp/device_id; creates with a new UUID if missing.
pub fn read_device_uuid() -> String {
    let path = crate::paths::hcom_dir().join(".tmp").join("device_id");
    if let Ok(content) = std::fs::read_to_string(&path) {
        let trimmed = content.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    // Create new UUID
    let device_id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    crate::paths::atomic_write(&path, &device_id);
    device_id
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

/// Relay status for TUI/CLI display.
#[derive(Debug, Clone)]
pub struct RelayStatus {
    pub configured: bool,
    pub enabled: bool,
    pub status: Option<String>,
    pub error: Option<String>,
    pub last_push: f64,
    pub broker: Option<String>,
}

/// Get relay status from config + DB.
pub fn get_relay_status(config: &HcomConfig, db: &HcomDb) -> RelayStatus {
    RelayStatus {
        configured: !config.relay_id.is_empty(),
        enabled: config.relay_enabled,
        status: safe_kv_get(db, "relay_status"),
        error: safe_kv_get(db, "relay_last_error"),
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

    if status == "ok" {
        // Claim ownership and clear error
        safe_kv_set(db, "relay_status_owner", Some(&pid));
        safe_kv_set(db, "relay_status", Some("ok"));
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
}
