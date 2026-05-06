//! Instance lifecycle state machine and launch failure handling.

use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use crate::db::{HcomDb, InstanceRow};
use crate::shared::time::{now_epoch_f64, now_epoch_i64};
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_INACTIVE, ST_LAUNCHING, ST_LISTENING};

/// Parameters for `set_status` beyond the core name/status/context triplet.
#[derive(Debug, Default)]
pub struct StatusUpdate<'a> {
    pub detail: &'a str,
    pub msg_ts: &'a str,
    pub launcher_override: Option<&'a str>,
    pub batch_id_override: Option<&'a str>,
}

/// Max time between instance creation and session binding before launch is considered failed.
pub const LAUNCH_PLACEHOLDER_TIMEOUT: i64 = 30;

/// Heartbeat timeout with active TCP listener (PTY, hooks with notify).
/// 35s = 30s hook polling interval + 5s buffer.
pub const HEARTBEAT_THRESHOLD_TCP: i64 = 35;

/// Heartbeat timeout without TCP listener (adhoc instances).
pub const HEARTBEAT_THRESHOLD_NO_TCP: i64 = 10;

/// Heartbeat age when last_stop is missing (marker for unreliable data).
pub const UNKNOWN_HEARTBEAT_AGE: i64 = 999999;

/// Max time without status update before marking inactive (5 min).
pub const STATUS_ACTIVITY_TIMEOUT: i64 = 300;

/// How long placeholder instances can exist before cleanup (2 min).
pub const CLEANUP_PLACEHOLDER_THRESHOLD: i64 = 120;

/// Grace period after sleep/wake before resuming stale cleanup (60s).
pub const WAKE_GRACE_PERIOD: f64 = 60.0;

/// Remote device stale threshold (90s without push).
const REMOTE_DEVICE_STALE_THRESHOLD: f64 = 90.0;

/// Window for showing recently stopped instances (10 minutes).
pub const RECENTLY_STOPPED_WINDOW: f64 = 600.0;

/// Return type for `get_instance_status()` with structured status metadata.
#[derive(Debug, Clone)]
pub struct ComputedStatus {
    pub status: String,
    pub age_string: String,
    pub description: String,
    pub age_seconds: i64,
    /// Simple context key (e.g., "stale", "killed", "timeout").
    pub context: String,
}

pub use crate::shared::time::format_age;

// Tracks wall-clock vs monotonic-clock drift to detect system sleep.
// On macOS, Instant (mach_absolute_time) does not advance during sleep,
// but SystemTime (gettimeofday) does. Large drift means the system just woke.
struct WakeState {
    last_mono: Option<Instant>,
    last_wall: f64,
    grace_until_mono: Option<Instant>,
}

static WAKE_STATE: Mutex<WakeState> = Mutex::new(WakeState {
    last_mono: None,
    last_wall: 0.0,
    grace_until_mono: None,
});

/// Detect sleep/wake via wall-vs-monotonic drift and report whether grace is active.
pub fn is_in_wake_grace() -> bool {
    is_in_wake_grace_with_persistence(None)
}

/// Wake-grace detection with optional DB persistence for short-lived processes.
pub fn is_in_wake_grace_with_persistence(db: Option<&crate::db::HcomDb>) -> bool {
    let now_mono = Instant::now();
    let now_wall = now_epoch_f64();

    let mut state = match WAKE_STATE.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };

    if state.last_mono.is_none() {
        if let Some(db) = db {
            if let Ok(Some(persisted_wall)) = db.kv_get("_wake_last_wall") {
                if let Ok(last_wall) = persisted_wall.parse::<f64>() {
                    let wall_elapsed = now_wall - last_wall;
                    if wall_elapsed > 30.0 && wall_elapsed < 3600.0 {
                        crate::log::log_info(
                            "cleanup",
                            "sleep_wake_detected",
                            &format!(
                                "drift={:.0}s (cross-process), grace={:.0}s",
                                wall_elapsed, WAKE_GRACE_PERIOD
                            ),
                        );
                        state.grace_until_mono =
                            Some(now_mono + std::time::Duration::from_secs_f64(WAKE_GRACE_PERIOD));
                    }
                    if let Ok(Some(grace_until)) = db.kv_get("_wake_grace_until") {
                        if let Ok(grace_wall) = grace_until.parse::<f64>() {
                            if now_wall < grace_wall {
                                let remaining = grace_wall - now_wall;
                                state.grace_until_mono =
                                    Some(now_mono + std::time::Duration::from_secs_f64(remaining));
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(last_mono) = state.last_mono {
        let mono_elapsed = now_mono.duration_since(last_mono).as_secs_f64();
        let wall_elapsed = now_wall - state.last_wall;
        let drift = wall_elapsed - mono_elapsed;

        if drift > 30.0 {
            crate::log::log_info(
                "cleanup",
                "sleep_wake_detected",
                &format!("drift={:.0}s, grace={:.0}s", drift, WAKE_GRACE_PERIOD),
            );
            let grace_deadline = now_mono + std::time::Duration::from_secs_f64(WAKE_GRACE_PERIOD);
            state.grace_until_mono = Some(grace_deadline);

            if let Some(db) = db {
                let grace_wall = now_wall + WAKE_GRACE_PERIOD;
                let _ = db.kv_set("_wake_grace_until", Some(&grace_wall.to_string()));
            }
        }
    }

    state.last_mono = Some(now_mono);
    state.last_wall = now_wall;

    if let Some(db) = db {
        let _ = db.kv_set("_wake_last_wall", Some(&now_wall.to_string()));
    }

    match state.grace_until_mono {
        Some(deadline) => now_mono < deadline,
        None => false,
    }
}

/// Compute the current status from stored fields and heartbeat.
pub fn get_instance_status(data: &InstanceRow, db: &HcomDb) -> ComputedStatus {
    let status = &data.status;
    let status_time = data.status_time;
    let status_context = &data.status_context;
    let wake_grace = is_in_wake_grace();
    let now = now_epoch_i64();

    if status_context == "new" && (status == ST_INACTIVE || status == "pending") {
        let created_at = data.created_at as i64;
        let age = if created_at > 0 { now - created_at } else { 0 };
        if age < LAUNCH_PLACEHOLDER_TIMEOUT {
            return ComputedStatus {
                status: ST_LAUNCHING.to_string(),
                age_string: if age > 0 {
                    format_age(age)
                } else {
                    String::new()
                },
                description: "launching".to_string(),
                age_seconds: age,
                context: "new".to_string(),
            };
        }

        let detail = get_or_finalize_launch_failure_detail(db, data)
            .or_else(|| extract_launch_failure_detail(data))
            .unwrap_or_else(|| "launch probably failed - check logs or hcom list -v".to_string());
        return ComputedStatus {
            status: ST_INACTIVE.to_string(),
            age_string: format_age(age),
            description: detail,
            age_seconds: age,
            context: "launch_failed".to_string(),
        };
    }

    let mut current_status = status.to_string();
    let mut current_context = status_context.to_string();
    let mut age = if status_time > 0 {
        now - status_time
    } else {
        0
    };
    if status_time == 0 {
        let created_at = data.created_at as i64;
        if created_at > 0 {
            age = now - created_at;
        }
    }

    if current_status == ST_LISTENING {
        let last_stop = data.last_stop;
        let is_remote = data.origin_device_id.is_some();

        if is_remote {
            age = 0;
        } else {
            let heartbeat_age = if last_stop > 0 {
                now - last_stop
            } else if status_time > 0 {
                now - status_time
            } else {
                UNKNOWN_HEARTBEAT_AGE
            };

            let has_tcp = data.tcp_mode != 0 || db.has_notify_endpoint(&data.name);
            let threshold = if has_tcp {
                HEARTBEAT_THRESHOLD_TCP
            } else {
                HEARTBEAT_THRESHOLD_NO_TCP
            };

            if heartbeat_age > threshold {
                if wake_grace {
                    age = 0;
                } else {
                    current_status = ST_INACTIVE.to_string();
                    current_context = "stale:listening".to_string();
                    age = heartbeat_age;
                }
            } else {
                age = 0;
            }
        }
    } else if current_status != ST_INACTIVE {
        let status_age = if status_time > 0 {
            now - status_time
        } else {
            let created_at = data.created_at as i64;
            if created_at > 0 { now - created_at } else { 0 }
        };

        if status_age > STATUS_ACTIVITY_TIMEOUT && data.origin_device_id.is_none() {
            let last_stop = data.last_stop;
            if last_stop > 0 && (now - last_stop) < HEARTBEAT_THRESHOLD_TCP {
                // Fresh heartbeat means the process is alive even if the status is old.
            } else if wake_grace {
                // Grace: heartbeat should refresh after wake.
            } else {
                let prev = current_status.clone();
                current_status = ST_INACTIVE.to_string();
                current_context = format!("stale:{prev}");
                age = status_age;
            }
        }
    }

    let description = get_status_description(&current_status, &current_context);
    let description = if data.tool == "adhoc" && current_status == ST_INACTIVE {
        if let Some(rest) = description.strip_prefix("inactive: ") {
            rest.to_string()
        } else if description == "inactive" {
            String::new()
        } else {
            description
        }
    } else {
        description
    };

    let simple_context = if current_context.contains(':') {
        let (prefix, suffix) = current_context.split_once(':').unwrap();
        if prefix == "exit" {
            suffix.to_string()
        } else {
            prefix.to_string()
        }
    } else {
        current_context.clone()
    };

    ComputedStatus {
        status: current_status,
        age_string: format_age(age),
        description,
        age_seconds: age,
        context: simple_context,
    }
}

pub(crate) fn get_or_finalize_launch_failure_detail(
    db: &HcomDb,
    data: &InstanceRow,
) -> Option<String> {
    finalize_launch_failure_detail(db, data, None)
}

pub(crate) fn finalize_launch_failure_detail(
    db: &HcomDb,
    data: &InstanceRow,
    fallback_detail: Option<&str>,
) -> Option<String> {
    if data.status_context == "launch_failed" && !data.status_detail.is_empty() {
        return Some(data.status_detail.clone());
    }

    if data.status_context != "new" || (data.status != ST_INACTIVE && data.status != "pending") {
        return if data.status_context == "launch_failed" {
            extract_launch_failure_detail(data)
                .or_else(|| fallback_detail.map(ToString::to_string))
                .or_else(|| (!data.status_detail.is_empty()).then(|| data.status_detail.clone()))
        } else {
            None
        };
    }

    let detail = extract_launch_failure_detail(data)
        .or_else(|| fallback_detail.map(ToString::to_string))
        .unwrap_or_else(|| "launch probably failed - check logs or hcom list -v".to_string());

    let mut updates = serde_json::Map::new();
    updates.insert("status".into(), serde_json::json!(ST_INACTIVE));
    updates.insert("status_time".into(), serde_json::json!(now_epoch_i64()));
    updates.insert("status_context".into(), serde_json::json!("launch_failed"));
    updates.insert("status_detail".into(), serde_json::json!(detail.clone()));
    crate::instances::update_instance_position(db, &data.name, &updates);

    let mut event_data = serde_json::json!({
        "status": ST_INACTIVE,
        "context": "launch_failed",
        "position": data.last_event_id,
        "detail": detail.clone(),
    });
    if detail.is_empty() {
        event_data.as_object_mut().map(|obj| obj.remove("detail"));
    }
    let _ = db.log_event("status", &data.name, &event_data);

    Some(detail)
}

fn extract_launch_failure_detail(data: &InstanceRow) -> Option<String> {
    let info = crate::terminal::resolve_terminal_info(
        data.terminal_preset_effective.as_deref(),
        data.launch_context.as_deref(),
    );

    match info.preset_name.as_str() {
        "tmux" | "tmux-split" => capture_tmux_launch_failure(&info.pane_id, &data.tool),
        _ => None,
    }
}

fn capture_tmux_launch_failure(pane_id: &str, tool: &str) -> Option<String> {
    if pane_id.is_empty() {
        return None;
    }

    let output = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", pane_id])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    parse_tmux_launch_failure_output(&String::from_utf8_lossy(&output.stdout), tool)
}

fn add_tmux_server_remediation(detail: &str) -> String {
    if !detail.contains("Operation not permitted") {
        return detail.to_string();
    }
    format!(
        "{detail} Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."
    )
}

fn parse_tmux_launch_failure_output(captured: &str, _tool: &str) -> Option<String> {
    let mut warning: Option<String> = None;

    for line in captured.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("Error:") {
            return Some(add_tmux_server_remediation(trimmed));
        }
        if warning.is_none() && trimmed.starts_with("WARNING:") {
            warning = Some(add_tmux_server_remediation(trimmed));
        }
    }

    warning
}

/// Build a human-readable status description from status and context tokens.
pub fn get_status_description(status: &str, context: &str) -> String {
    match status {
        ST_ACTIVE => {
            if let Some(sender) = context.strip_prefix("deliver:") {
                format!("active: msg from {sender}")
            } else if let Some(tool) = context.strip_prefix("tool:") {
                format!("active: {tool}")
            } else if let Some(tool) = context.strip_prefix("approved:") {
                format!("active: approved {tool}")
            } else if context == "resuming" {
                "resuming...".to_string()
            } else if context.is_empty() {
                "active".to_string()
            } else {
                format!("active: {context}")
            }
        }
        ST_LISTENING => {
            if context == "tui:not-ready" {
                "listening: blocked".to_string()
            } else if context == "tui:not-idle" {
                "listening: waiting for idle".to_string()
            } else if context == "tui:user-active" {
                "listening: user typing".to_string()
            } else if context == "tui:output-unstable" {
                "listening: output streaming".to_string()
            } else if context == "tui:prompt-has-text" {
                "listening: uncommitted text".to_string()
            } else if let Some(reason) = context.strip_prefix("tui:") {
                format!("listening: {}", reason.replace('-', " "))
            } else if context == "suspended" {
                "listening: suspended".to_string()
            } else {
                "listening".to_string()
            }
        }
        ST_BLOCKED => {
            if context == "pty:approval" || context == "approval" {
                "blocked: approval pending".to_string()
            } else if context.is_empty() {
                "blocked: permission needed".to_string()
            } else {
                format!("blocked: {context}")
            }
        }
        ST_INACTIVE => {
            if context.starts_with("stale:") {
                "inactive: stale".to_string()
            } else if let Some(reason) = context.strip_prefix("exit:") {
                format!("inactive: {reason}")
            } else if context == "subagent:dormant" {
                "inactive: dormant subagent".to_string()
            } else if context == "unknown" {
                "inactive: unknown".to_string()
            } else if context.is_empty() {
                "inactive".to_string()
            } else {
                format!("inactive: {context}")
            }
        }
        _ => "unknown".to_string(),
    }
}

/// Set instance status with timestamp and log the status-change event.
pub fn set_status(
    db: &HcomDb,
    instance_name: &str,
    status: &str,
    context: &str,
    upd: StatusUpdate<'_>,
) {
    let StatusUpdate {
        detail,
        msg_ts,
        launcher_override,
        batch_id_override,
    } = upd;

    let (current_data, db_error) = match db.get_instance_full(instance_name) {
        Ok(data) => (data, false),
        Err(e) => {
            eprintln!("[hcom] warn: set_status DB read failed for {instance_name}: {e}");
            (None, true)
        }
    };
    let is_new = if db_error {
        false
    } else {
        current_data
            .as_ref()
            .map(|d| d.status_context == "new")
            .unwrap_or(true)
    };

    let now = now_epoch_i64();
    let mut updates = serde_json::Map::new();
    updates.insert("status".into(), serde_json::json!(status));
    updates.insert("status_time".into(), serde_json::json!(now));
    updates.insert("status_context".into(), serde_json::json!(context));
    updates.insert("status_detail".into(), serde_json::json!(detail));

    if status == ST_LISTENING {
        updates.insert("last_stop".into(), serde_json::json!(now));
    }

    let old_status = current_data.as_ref().map(|d| d.status.as_str());
    let status_changed = old_status != Some(status);

    crate::instances::update_instance_position(db, instance_name, &updates);

    if status_changed {
        crate::notify::wake(db, instance_name, crate::notify::WakeKind::DELIVERY_LOOPS);
    }

    if is_new {
        let launcher = launcher_override
            .map(ToString::to_string)
            .or_else(|| std::env::var("HCOM_LAUNCHED_BY").ok())
            .unwrap_or_else(|| "unknown".to_string());
        let batch_id = batch_id_override
            .map(ToString::to_string)
            .or_else(|| std::env::var("HCOM_LAUNCH_BATCH_ID").ok());

        let mut event_data = serde_json::json!({
            "action": "ready",
            "by": launcher,
            "status": status,
            "context": context,
        });
        if let Some(ref bid) = batch_id {
            event_data["batch_id"] = serde_json::json!(bid);
        }

        if let Err(e) = db.log_event("life", instance_name, &event_data) {
            crate::log::log_error("core", "db.error", &format!("ready event: {e}"));
        }

        if launcher != "unknown" {
            if let Some(ref bid) = batch_id {
                if let Err(e) = db.check_batch_completion(&launcher, bid) {
                    crate::log::log_error("core", "db.error", &format!("batch notification: {e}"));
                }
            }
        }
    }

    let position = current_data.as_ref().map(|d| d.last_event_id).unwrap_or(0);
    let mut data = serde_json::json!({
        "status": status,
        "context": context,
        "position": position,
    });
    if !detail.is_empty() {
        data["detail"] = serde_json::json!(detail);
    }
    if !msg_ts.is_empty() {
        data["msg_ts"] = serde_json::json!(msg_ts);
    }
    let _ = db.log_event("status", instance_name, &data);
}

/// Delete placeholder instances that have been launching too long.
pub fn cleanup_stale_placeholders(db: &HcomDb) -> i32 {
    let mut deleted = 0;
    let now = now_epoch_f64();

    if let Ok(instances) = db.iter_instances_full() {
        for data in &instances {
            if !crate::instances::is_launching_placeholder(data) {
                continue;
            }
            let created_at = data.created_at;
            if created_at > 0.0 && (now - created_at) > CLEANUP_PLACEHOLDER_THRESHOLD as f64 {
                crate::hooks::common::stop_instance(db, &data.name, "system", "stale_cleanup");
                deleted += 1;
            }
        }
    }
    deleted
}

/// Delete instances that have been inactive too long.
/// Three tiers: exit contexts (1 min), stale (1 hr), other inactive (12 hr).
pub fn cleanup_stale_instances(
    db: &HcomDb,
    max_stale_seconds: i64,
    max_inactive_seconds: i64,
) -> i32 {
    if is_in_wake_grace() {
        return 0;
    }

    cleanup_stale_remote_instances(db);

    let mut deleted = 0;

    if let Ok(instances) = db.iter_instances_full() {
        for data in &instances {
            let computed = get_instance_status(data, db);

            if computed.status != ST_INACTIVE {
                continue;
            }

            let context = &computed.context;
            let age = computed.age_seconds;

            if matches!(
                context.as_str(),
                "killed" | "closed" | "timeout" | "interrupted" | "session_switch"
            ) && age > 60
            {
                crate::hooks::common::stop_instance(db, &data.name, "system", "exit_cleanup");
                deleted += 1;
                return deleted;
            }

            if context == "stale" && max_stale_seconds > 0 && age > max_stale_seconds {
                crate::hooks::common::stop_instance(db, &data.name, "system", "stale_cleanup");
                deleted += 1;
                return deleted;
            }

            if max_inactive_seconds > 0 && age > max_inactive_seconds {
                crate::hooks::common::stop_instance(db, &data.name, "system", "inactive_cleanup");
                deleted += 1;
                return deleted;
            }
        }
    }

    deleted
}

fn cleanup_stale_remote_instances(db: &HcomDb) {
    let now = now_epoch_f64();
    let sync_map: std::collections::HashMap<String, String> = db
        .kv_prefix("relay_sync_time_")
        .unwrap_or_default()
        .into_iter()
        .collect();

    if let Ok(instances) = db.iter_instances_full() {
        let device_ids: std::collections::HashSet<String> = instances
            .iter()
            .filter_map(|d| d.origin_device_id.clone())
            .collect();

        for device_id in device_ids {
            let sync_val = sync_map.get(&format!("relay_sync_time_{device_id}"));
            let sync_time: f64 = sync_val.and_then(|s| s.parse().ok()).unwrap_or(0.0);
            if sync_time > 0.0 && (now - sync_time) <= REMOTE_DEVICE_STALE_THRESHOLD {
                continue;
            }
            if let Err(e) = db.conn().execute(
                "DELETE FROM instances WHERE origin_device_id = ?",
                rusqlite::params![device_id],
            ) {
                crate::log::log_warn("cleanup", "remote_stale_cleanup_fail", &e.to_string());
            } else {
                crate::log::log_info(
                    "cleanup",
                    "remote_device_stale",
                    crate::relay::device_id_prefix(&device_id),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;

    fn setup_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_instance_lifecycle_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA journal_mode=WAL;

             CREATE TABLE events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 timestamp TEXT NOT NULL,
                 type TEXT NOT NULL,
                 instance TEXT,
                 data TEXT NOT NULL
             );

             CREATE TABLE instances (
                 name TEXT PRIMARY KEY,
                 session_id TEXT UNIQUE,
                 parent_session_id TEXT,
                 parent_name TEXT,
                 tag TEXT,
                 last_event_id INTEGER DEFAULT 0,
                 status TEXT DEFAULT 'active',
                 status_time INTEGER DEFAULT 0,
                 status_context TEXT DEFAULT '',
                 status_detail TEXT DEFAULT '',
                 last_stop INTEGER DEFAULT 0,
                 directory TEXT,
                 created_at REAL NOT NULL DEFAULT 0,
                 transcript_path TEXT DEFAULT '',
                 tcp_mode INTEGER DEFAULT 0,
                 wait_timeout INTEGER DEFAULT 86400,
                 background INTEGER DEFAULT 0,
                 background_log_file TEXT DEFAULT '',
                 name_announced INTEGER DEFAULT 0,
                 agent_id TEXT UNIQUE,
                 running_tasks TEXT DEFAULT '',
                 origin_device_id TEXT DEFAULT '',
                 hints TEXT DEFAULT '',
                 subagent_timeout INTEGER,
                 tool TEXT DEFAULT 'claude',
                 launch_args TEXT DEFAULT '',
                 terminal_preset_requested TEXT DEFAULT '',
                 terminal_preset_effective TEXT DEFAULT '',
                 idle_since TEXT DEFAULT '',
                 pid INTEGER DEFAULT NULL,
                 launch_context TEXT DEFAULT '',
                 FOREIGN KEY (parent_session_id) REFERENCES instances(session_id) ON DELETE SET NULL
             );

             CREATE TABLE process_bindings (
                 process_id TEXT PRIMARY KEY,
                 session_id TEXT,
                 instance_name TEXT,
                 updated_at REAL NOT NULL
             );

             CREATE TABLE session_bindings (
                 session_id TEXT PRIMARY KEY,
                 instance_name TEXT NOT NULL,
                 created_at REAL NOT NULL,
                 FOREIGN KEY (instance_name) REFERENCES instances(name) ON DELETE CASCADE
             );

             CREATE TABLE notify_endpoints (
                 instance TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 port INTEGER NOT NULL,
                 updated_at REAL NOT NULL,
                 PRIMARY KEY (instance, kind)
             );

             CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        (db, db_path)
    }

    fn cleanup(path: PathBuf) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    fn default_instance() -> InstanceRow {
        InstanceRow {
            name: String::new(),
            session_id: None,
            parent_session_id: None,
            parent_name: None,
            agent_id: None,
            tag: None,
            last_event_id: 0,
            last_stop: 0,
            status: ST_INACTIVE.into(),
            status_time: 0,
            status_context: String::new(),
            status_detail: String::new(),
            directory: String::new(),
            created_at: 0.0,
            transcript_path: String::new(),
            tool: "claude".into(),
            background: 0,
            background_log_file: String::new(),
            tcp_mode: 0,
            wait_timeout: None,
            subagent_timeout: None,
            hints: None,
            origin_device_id: None,
            pid: None,
            launch_args: None,
            terminal_preset_requested: None,
            terminal_preset_effective: None,
            launch_context: None,
            name_announced: 0,
            running_tasks: None,
            idle_since: None,
        }
    }

    #[test]
    fn test_status_launching_new() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            created_at: now as f64,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_LAUNCHING);
        assert_eq!(result.context, "new");
        cleanup(path);
    }

    #[test]
    fn test_status_launch_failed() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            created_at: (now - LAUNCH_PLACEHOLDER_TIMEOUT - 1) as f64,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_INACTIVE);
        assert_eq!(result.context, "launch_failed");
        cleanup(path);
    }

    #[test]
    fn test_finalize_launch_failure_detail_uses_fallback() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("test"));
        row.insert("status".into(), serde_json::json!(ST_INACTIVE));
        row.insert("status_context".into(), serde_json::json!("new"));
        row.insert(
            "created_at".into(),
            serde_json::json!((now - LAUNCH_PLACEHOLDER_TIMEOUT - 1) as f64),
        );
        row.insert("status_time".into(), serde_json::json!(0));
        row.insert("tool".into(), serde_json::json!("codex"));
        db.save_instance_named("test", &row).unwrap();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            created_at: (now - LAUNCH_PLACEHOLDER_TIMEOUT - 1) as f64,
            ..default_instance()
        };

        let detail = finalize_launch_failure_detail(
            &db,
            &data,
            Some("process exited before startup completed (exit code 1)"),
        );
        assert_eq!(
            detail.as_deref(),
            Some("process exited before startup completed (exit code 1)")
        );

        let stored = db.get_instance_full("test").unwrap().unwrap();
        assert_eq!(stored.status_context, "launch_failed");
        assert_eq!(
            stored.status_detail,
            "process exited before startup completed (exit code 1)"
        );
        cleanup(path);
    }

    #[test]
    fn test_parse_tmux_launch_failure_output_prefers_error() {
        let captured = "\
Starting Codex...
WARNING: proceeding, even though we could not update PATH: Operation not permitted (os error 1)
Error: Operation not permitted (os error 1)
";

        let result = parse_tmux_launch_failure_output(captured, "codex");
        assert_eq!(
            result.as_deref(),
            Some(
                "Error: Operation not permitted (os error 1) Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."
            )
        );
    }

    #[test]
    fn test_parse_tmux_launch_failure_output_falls_back_to_warning() {
        let captured = "\
Starting Codex...
WARNING: proceeding, even though we could not update PATH: Operation not permitted (os error 1)
";

        let result = parse_tmux_launch_failure_output(captured, "codex");
        assert_eq!(
            result.as_deref(),
            Some(
                "WARNING: proceeding, even though we could not update PATH: Operation not permitted (os error 1) Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."
            )
        );
    }

    #[test]
    fn test_status_listening_fresh_heartbeat() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_LISTENING.into(),
            status_time: now - 5,
            last_stop: now - 2,
            tcp_mode: 1,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_LISTENING);
        assert_eq!(result.age_string, "now");
        cleanup(path);
    }

    #[test]
    fn test_status_listening_stale_heartbeat() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_LISTENING.into(),
            status_time: now - 100,
            last_stop: now - 100,
            tcp_mode: 1,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_INACTIVE);
        assert!(
            result.context.starts_with("stale"),
            "context should be stale, got: {}",
            result.context
        );
        cleanup(path);
    }

    #[test]
    fn test_status_active_stale_activity() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_ACTIVE.into(),
            status_context: "tool:Bash".into(),
            status_time: now - STATUS_ACTIVITY_TIMEOUT - 10,
            last_stop: 0,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_INACTIVE);
        assert!(result.context.starts_with("stale"));
        cleanup(path);
    }

    #[test]
    fn test_status_remote_instance_trusted() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_LISTENING.into(),
            status_time: now - 100,
            last_stop: 0,
            origin_device_id: Some("device-abc".into()),
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_LISTENING);
        cleanup(path);
    }

    #[test]
    fn test_status_descriptions() {
        assert_eq!(
            get_status_description(ST_ACTIVE, "tool:Bash"),
            "active: Bash"
        );
        assert_eq!(
            get_status_description(ST_ACTIVE, "deliver:luna"),
            "active: msg from luna"
        );
        assert_eq!(get_status_description(ST_ACTIVE, ""), "active");
        assert_eq!(get_status_description(ST_LISTENING, ""), "listening");
        assert_eq!(
            get_status_description(ST_LISTENING, "tui:not-ready"),
            "listening: blocked"
        );
        assert_eq!(
            get_status_description(ST_BLOCKED, ""),
            "blocked: permission needed"
        );
        assert_eq!(
            get_status_description(ST_INACTIVE, "stale:listening"),
            "inactive: stale"
        );
        assert_eq!(
            get_status_description(ST_INACTIVE, "exit:timeout"),
            "inactive: timeout"
        );
    }

    #[test]
    fn test_cleanup_stale_placeholders_deletes_old() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let old_time = now_epoch_f64() - 200.0;
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("stale"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(old_time));
        db.save_instance_named("stale", &data).unwrap();

        let deleted = cleanup_stale_placeholders(&db);
        assert_eq!(deleted, 1);
        assert!(db.get_instance_full("stale").unwrap().is_none());

        cleanup(path);
    }

    #[test]
    fn test_cleanup_stale_placeholders_keeps_fresh() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let now = now_epoch_f64();
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("fresh"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        db.save_instance_named("fresh", &data).unwrap();

        let deleted = cleanup_stale_placeholders(&db);
        assert_eq!(deleted, 0);
        assert!(db.get_instance_full("fresh").unwrap().is_some());

        cleanup(path);
    }

    #[test]
    fn test_cleanup_stale_placeholders_skips_non_placeholder() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let old_time = now_epoch_f64() - 200.0;
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("real"));
        data.insert("session_id".into(), serde_json::json!("sess-1"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(old_time));
        db.save_instance_named("real", &data).unwrap();

        let deleted = cleanup_stale_placeholders(&db);
        assert_eq!(deleted, 0);
        assert!(db.get_instance_full("real").unwrap().is_some());

        cleanup(path);
    }
}
