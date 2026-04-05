//! Instance lifecycle — name generation, status state machine, session binding.

use anyhow::Result;
use std::collections::HashSet;
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use crate::db::{HcomDb, InstanceRow};
use crate::shared::time::{now_epoch_f64, now_epoch_i64};

/// Parameters for `set_status` — fields beyond name/status/context default to empty.
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

use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_INACTIVE, ST_LAUNCHING, ST_LISTENING};

/// Return type for get_instance_status() — structured access to computed status.
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
// On macOS, Instant (mach_absolute_time) does NOT advance during sleep,
// but SystemTime (gettimeofday) does. Large drift = just woke from sleep.

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

/// Detect sleep/wake via wall-vs-monotonic drift, return whether grace period is active.
///
/// Called from get_instance_status() and cleanup_stale_instances() so whichever runs
/// first after wake will detect the drift and grant grace.
///
/// For long-lived processes (TUI, PTY delivery), the in-process Mutex state works
/// because the process accumulates readings across calls. For short-lived hook processes,
/// we also persist the last wall-clock reading to a file so the next invocation can
/// detect drift even though the Mutex was freshly initialized.
pub fn is_in_wake_grace() -> bool {
    is_in_wake_grace_with_persistence(None)
}

/// Wake grace detection with optional DB-backed persistence for short-lived processes.
///
/// When `db` is Some, persists wall-clock readings to KV so successive short-lived
/// hook invocations can detect sleep/wake across process boundaries. When None,
/// uses only in-process state (sufficient for long-lived TUI/PTY processes).
pub fn is_in_wake_grace_with_persistence(db: Option<&crate::db::HcomDb>) -> bool {
    let now_mono = Instant::now();
    let now_wall = now_epoch_f64();

    let mut state = match WAKE_STATE.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };

    // For short-lived processes: bootstrap from persisted state on first call
    if state.last_mono.is_none() {
        if let Some(db) = db {
            // Read last wall-clock from KV to detect drift across processes
            if let Ok(Some(persisted_wall)) = db.kv_get("_wake_last_wall") {
                if let Ok(last_wall) = persisted_wall.parse::<f64>() {
                    let wall_elapsed = now_wall - last_wall;
                    // Wall clock jumped but monotonic is 0 (fresh process) — that's drift
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
                    // Also check if a grace period was active when last process exited
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

            // Persist grace deadline as wall-clock time for cross-process visibility
            if let Some(db) = db {
                let grace_wall = now_wall + WAKE_GRACE_PERIOD;
                let _ = db.kv_set("_wake_grace_until", Some(&grace_wall.to_string()));
            }
        }
    }

    state.last_mono = Some(now_mono);
    state.last_wall = now_wall;

    // Persist wall-clock reading for next short-lived process
    if let Some(db) = db {
        let _ = db.kv_set("_wake_last_wall", Some(&now_wall.to_string()));
    }

    match state.grace_until_mono {
        Some(deadline) => now_mono < deadline,
        None => false,
    }
}

/// Compute current status from stored fields and heartbeat.
pub fn get_instance_status(data: &InstanceRow, db: &HcomDb) -> ComputedStatus {
    let status = &data.status;
    let status_time = data.status_time;
    let status_context = &data.status_context;
    let wake_grace = is_in_wake_grace();
    let now = now_epoch_i64();

    // Launching: instance created but session not yet bound
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
        } else {
            let detail = get_or_finalize_launch_failure_detail(db, data)
                .or_else(|| extract_launch_failure_detail(data))
                .unwrap_or_else(|| {
                    "launch probably failed - check logs or hcom list -v".to_string()
                });
            return ComputedStatus {
                status: ST_INACTIVE.to_string(),
                age_string: format_age(age),
                description: detail,
                age_seconds: age,
                context: "launch_failed".to_string(),
            };
        }
    }

    let mut current_status = status.to_string();
    let mut current_context = status_context.to_string();

    // Compute age from status_time, fallback to created_at
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

    // Listening: heartbeat timeout check
    if current_status == ST_LISTENING {
        let last_stop = data.last_stop;
        let is_remote = data.origin_device_id.is_some();

        if is_remote {
            age = 0; // Trust synced status
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
                    age = 0; // Grace: heartbeat will refresh soon
                } else {
                    current_status = ST_INACTIVE.to_string();
                    current_context = "stale:listening".to_string();
                    age = heartbeat_age;
                }
            } else {
                age = 0; // Heartbeat within threshold
            }
        }
    }
    // Non-inactive, non-listening: activity timeout check
    else if current_status != ST_INACTIVE {
        let status_age = if status_time > 0 {
            now - status_time
        } else {
            let created_at = data.created_at as i64;
            if created_at > 0 { now - created_at } else { 0 }
        };

        if status_age > STATUS_ACTIVITY_TIMEOUT {
            let is_remote = data.origin_device_id.is_some();
            if !is_remote {
                // Check if heartbeat is fresh (PTY delivery thread keeps it alive)
                let last_stop = data.last_stop;
                if last_stop > 0 && (now - last_stop) < HEARTBEAT_THRESHOLD_TCP {
                    // Heartbeat fresh — process alive, skip stale
                } else if wake_grace {
                    // Grace: heartbeat will refresh soon
                } else {
                    let prev = current_status.clone();
                    current_status = ST_INACTIVE.to_string();
                    current_context = format!("stale:{}", prev);
                    age = status_age;
                }
            }
        }
    }

    // Build description
    let description = get_status_description(&current_status, &current_context);

    // Adhoc: strip "inactive: " prefix
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

    // Extract simple context key
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
    update_instance_position(db, &data.name, &updates);

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

/// Build human-readable status description from status + context tokens.
pub fn get_status_description(status: &str, context: &str) -> String {
    match status {
        ST_ACTIVE => {
            if let Some(sender) = context.strip_prefix("deliver:") {
                format!("active: msg from {}", sender)
            } else if let Some(tool) = context.strip_prefix("tool:") {
                format!("active: {}", tool)
            } else if let Some(tool) = context.strip_prefix("approved:") {
                format!("active: approved {}", tool)
            } else if context == "resuming" {
                "resuming...".to_string()
            } else if context.is_empty() {
                "active".to_string()
            } else {
                format!("active: {}", context)
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
                format!("blocked: {}", context)
            }
        }
        ST_INACTIVE => {
            if context.starts_with("stale:") {
                "inactive: stale".to_string()
            } else if let Some(reason) = context.strip_prefix("exit:") {
                format!("inactive: {}", reason)
            } else if context == "unknown" {
                "inactive: unknown".to_string()
            } else if context.is_empty() {
                "inactive".to_string()
            } else {
                format!("inactive: {}", context)
            }
        }
        _ => "unknown".to_string(),
    }
}

pub fn is_remote_instance(data: &InstanceRow) -> bool {
    data.origin_device_id.is_some()
}

pub fn is_subagent_instance(data: &InstanceRow) -> bool {
    data.parent_session_id.is_some()
}

/// Check if a session/instance is in subagent context (Task active).
///
/// Accepts either a session_id or instance name. Returns true if the instance
/// has active running tasks (i.e. a Task subagent is executing).
pub fn in_subagent_context(db: &HcomDb, session_id: &str) -> bool {
    let instance_name = match db.get_session_binding(session_id) {
        Ok(Some(name)) => name,
        _ => session_id.to_string(),
    };

    let row: Option<Option<String>> = db
        .conn()
        .query_row(
            "SELECT running_tasks FROM instances WHERE name = ? LIMIT 1",
            rusqlite::params![instance_name],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok();

    match row {
        Some(Some(rt_json)) if !rt_json.is_empty() => {
            let rt = parse_running_tasks(Some(&rt_json));
            rt.active
        }
        _ => false,
    }
}

pub fn is_launching_placeholder(data: &InstanceRow) -> bool {
    data.session_id.is_none()
        && data.status_context == "new"
        && (data.status == ST_INACTIVE || data.status == "pending")
}

/// Get full display name: "{tag}-{name}" if tag exists, else just "{name}".
pub fn get_full_name(data: &InstanceRow) -> String {
    match &data.tag {
        Some(tag) if !tag.is_empty() => format!("{}-{}", tag, data.name),
        _ => data.name.clone(),
    }
}

/// Get display name for a base name by loading instance data.
pub fn get_display_name(db: &HcomDb, base_name: &str) -> String {
    match db.get_instance_full(base_name) {
        Ok(Some(data)) => get_full_name(&data),
        _ => base_name.to_string(),
    }
}

/// Resolve base name or tag-name (e.g., "team-luna") to base name.
/// Handles multi-hyphen tags like "vc-p0-p1-parallel-vani" → tag="vc-p0-p1-parallel", name="vani".
pub fn resolve_display_name(db: &HcomDb, input_name: &str) -> Option<String> {
    // Direct match
    if let Ok(Some(_)) = db.get_instance_full(input_name) {
        return Some(input_name.to_string());
    }
    // Try all possible tag-name split points (tags can contain hyphens)
    for (i, _) in input_name.match_indices('-') {
        let tag = &input_name[..i];
        let name = &input_name[i + 1..];
        if name.is_empty() {
            continue;
        }
        if let Ok(Some(data)) = db.get_instance_full(name) {
            if data.tag.as_deref() == Some(tag) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Resolve base name or tag-name using live instances first, then stopped snapshots.
pub fn resolve_display_name_or_stopped(db: &HcomDb, input_name: &str) -> Option<String> {
    if let Some(name) = resolve_display_name(db, input_name) {
        return Some(name);
    }

    // Direct stopped-instance match by base name.
    if db
        .conn()
        .query_row(
            "SELECT instance FROM events
             WHERE type = 'life'
               AND instance = ?1
               AND json_extract(data, '$.action') = 'stopped'
             LIMIT 1",
            rusqlite::params![input_name],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .is_some()
    {
        return Some(input_name.to_string());
    }

    // Stopped tag-name match against the stored snapshot tag (try all split points).
    for (i, _) in input_name.match_indices('-') {
        let tag = &input_name[..i];
        let name = &input_name[i + 1..];
        if name.is_empty() {
            continue;
        }
        if db
            .conn()
            .query_row(
                "SELECT instance FROM events
                 WHERE type = 'life'
                   AND instance = ?1
                   AND json_extract(data, '$.action') = 'stopped'
                   AND json_extract(data, '$.snapshot.tag') = ?2
                 LIMIT 1",
                rusqlite::params![name, tag],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .is_some()
        {
            return Some(name.to_string());
        }
    }

    None
}

/// Parsed running_tasks JSON field.
#[derive(Debug, Clone, Default)]
pub struct RunningTasks {
    pub active: bool,
    pub subagents: Vec<serde_json::Value>,
}

pub fn parse_running_tasks(json_str: Option<&str>) -> RunningTasks {
    let Some(s) = json_str else {
        return RunningTasks::default();
    };
    if s.is_empty() {
        return RunningTasks::default();
    }

    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::Object(obj)) => RunningTasks {
            active: obj.get("active").and_then(|v| v.as_bool()).unwrap_or(false),
            subagents: obj
                .get("subagents")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        },
        _ => RunningTasks::default(),
    }
}

// Names are 4-letter CVCV (consonant-vowel-consonant-vowel) patterns.
// Curated "gold" names score highest, generated names fill the pool.

const CONSONANTS: &[u8] = b"bdfghklmnprstvz";
const VOWELS: &[u8] = b"aeiou";

/// Curated gold names (high recognition, pleasant).
fn gold_names() -> HashSet<&'static str> {
    [
        // Real/common names
        "luna", "nova", "nora", "zara", "kira", "mila", "lola", "lara", "sara", "rhea", "nina",
        "mira", "tara", "sora", "cora", "dora", "gina", "lina", "viva", "risa", "mimi", "coco",
        "koko", "lili", "navi", "ravi", "rani", "riko", "niko", "mako", "saki", "maki", "nami",
        "loki", "rori", "lori", "mori", "nori", "tori", "gigi", "hana", "hiro", "tomo", "sumi",
        "vega", "kobe", "rafa", "lana", "lena", "dara", "niro", "yuki", "yuri", "maya", "juno",
        "nico", "rosa", "vera", "rina", "mika", "yoko", "yumi", "ruby", "lily", "cici", "hera",
        // Real words
        "miso", "taro", "boba", "kava", "soda", "cola", "coda", "data", "beta", "sofa", "mono",
        "moto", "tiki", "koda", "kali", "gala", "hula", "kula", "puma", "yoga", "zola", "zori",
        "veto", "vivo", "dino", "nemo", "hero", "zero", "memo", "demo", "polo", "solo", "logo",
        "halo", "dojo", "judo", "sumo", "tofu", "guru", "vino", "diva", "dodo", "silo", "peso",
        "lulu", "pita", "feta", "bobo", "brie", "fava", "duma", "beto", "moku", "bozo", "tuna",
        "lava", "hobo", "kiwi", "mojo", "yoyo", "sake", "wiki", "fiji", "bali", "kona", "poke",
        "cafe", "soho", "boho", "nano", "zulu", "deli", "rose", "jedi", "yoda",
        // Invented but natural-sounding
        "zumi", "reko", "valo", "kazu", "mero", "niru", "piko", "hazu", "toku", "veki",
    ]
    .into_iter()
    .collect()
}

fn banned_names() -> HashSet<&'static str> {
    [
        "help", "exit", "quit", "sudo", "bash", "curl", "grep", "init", "list", "send", "stop",
        "test", "meta",
    ]
    .into_iter()
    .collect()
}

fn score_name(name: &str, gold: &HashSet<&str>, banned: &HashSet<&str>) -> i32 {
    if banned.contains(name) {
        return i32::MIN / 2;
    }

    let mut score: i32 = 0;
    let bytes = name.as_bytes();

    // Strong preference for curated names
    if gold.contains(name) {
        score += 4000;
    }

    // Friendly flow letters
    if bytes
        .iter()
        .any(|&c| matches!(c, b'l' | b'r' | b'n' | b'm'))
    {
        score += 40;
    }

    // Slight spice: prefer exactly one v/z
    let vz_count = bytes.iter().filter(|&&c| c == b'v' || c == b'z').count();
    if vz_count == 1 {
        score += 12;
    } else if vz_count >= 2 {
        score -= 15;
    }

    // Avoid doubled vowels (e.g., "mama" pattern)
    if bytes.len() >= 4 && bytes[1] == bytes[3] {
        score -= 8;
    }

    // Name-like endings (a, e, o)
    if bytes.len() >= 4 && matches!(bytes[3], b'a' | b'e' | b'o') {
        score += 6;
    }

    score
}

#[derive(Clone)]
struct ScoredName {
    score: i32,
    name: String,
}

/// Build scored pool of all valid CVCV names plus curated gold names.
fn build_name_pool(limit: usize) -> Vec<ScoredName> {
    let gold = gold_names();
    let banned = banned_names();
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    // Generate all CVCV combinations
    for &c1 in CONSONANTS {
        for &v1 in VOWELS {
            for &c2 in CONSONANTS {
                for &v2 in VOWELS {
                    let name = format!("{}{}{}{}", c1 as char, v1 as char, c2 as char, v2 as char);
                    if banned.contains(name.as_str()) {
                        continue;
                    }
                    let s = score_name(&name, &gold, &banned);
                    seen.insert(name.clone());
                    candidates.push(ScoredName { score: s, name });
                }
            }
        }
    }

    // Inject gold names that don't match CVCV pattern (e.g., coco, juno, maya)
    for &name in &gold {
        if !seen.contains(name) && !banned.contains(name) {
            let s = score_name(name, &gold, &banned);
            seen.insert(name.to_string());
            candidates.push(ScoredName {
                score: s,
                name: name.to_string(),
            });
        }
    }

    // Sort by score descending
    candidates.sort_by_key(|b| std::cmp::Reverse(b.score));
    candidates.truncate(limit);
    candidates
}

/// Pre-built name pool (lazily initialized).
fn name_pool() -> &'static Vec<ScoredName> {
    use std::sync::OnceLock;
    static POOL: OnceLock<Vec<ScoredName>> = OnceLock::new();
    POOL.get_or_init(|| build_name_pool(5000))
}

/// Check if name is too similar to alive instances (Hamming distance <= 2).
fn is_too_similar(name: &str, alive_names: &HashSet<String>) -> bool {
    let name_bytes = name.as_bytes();
    for other in alive_names {
        if other.len() != name.len() {
            continue;
        }
        let diff = name_bytes
            .iter()
            .zip(other.as_bytes())
            .filter(|(a, b)| a != b)
            .count();
        if diff <= 2 {
            return true;
        }
    }
    false
}

/// Allocate a name with bias toward high-scoring names.
/// Three tiers: (1) weighted sampling + similarity, (2) greedy + similarity,
/// (3) greedy without similarity (last resort).
fn allocate_name(
    is_taken: &dyn Fn(&str) -> bool,
    alive_names: &HashSet<String>,
    attempts: usize,
    top_window: usize,
    temperature: f64,
) -> Result<String> {
    use rand::Rng;
    let pool = name_pool();
    let mut rng = rand::rng();

    let window_size = top_window.min(pool.len()).max(50);
    let window = &pool[..window_size];

    // Compute softmax weights (numerically stable)
    let max_score = window.iter().map(|x| x.score).max().unwrap_or(0) as f64;
    let weights: Vec<f64> = window
        .iter()
        .map(|x| ((x.score as f64 - max_score) / temperature).exp())
        .collect();
    let total_weight: f64 = weights.iter().sum();

    // Tier 1: Weighted sampling with similarity check
    for _ in 0..attempts {
        let r: f64 = rng.random::<f64>() * total_weight;
        let mut cumulative = 0.0;
        let mut chosen_idx = 0;
        for (i, w) in weights.iter().enumerate() {
            cumulative += w;
            if cumulative >= r {
                chosen_idx = i;
                break;
            }
        }
        let choice = &window[chosen_idx].name;
        if !is_taken(choice) && !is_too_similar(choice, alive_names) {
            return Ok(choice.clone());
        }
    }

    // Tier 2: Greedy with similarity check
    for item in pool.iter() {
        if !is_taken(&item.name) && !is_too_similar(&item.name, alive_names) {
            return Ok(item.name.clone());
        }
    }

    // Tier 3: Greedy without similarity (last resort)
    for item in pool.iter() {
        if !is_taken(&item.name) {
            return Ok(item.name.clone());
        }
    }

    Err(anyhow::anyhow!("No available names left in pool"))
}

/// Hash any string to a memorable 4-char name.
/// Used for device short IDs. Uses FNV-1a hash for distribution.
pub fn hash_to_name(input: &str, collision_attempt: u32) -> String {
    let pool = name_pool();
    let hash_words = &pool[..pool.len().min(500)];

    // FNV-1a hash (32-bit)
    let mut h: u32 = 2166136261;
    for c in input.bytes() {
        h ^= c as u32;
        h = h.wrapping_mul(16777619);
    }
    h = h.wrapping_add(collision_attempt.wrapping_mul(31337));

    let idx = (h as usize) % hash_words.len();
    hash_words[idx].name.clone()
}

/// Generate a unique instance name with flock-based reservation.
/// Creates a placeholder row in DB to prevent TOCTOU races.
pub fn generate_unique_name(db: &HcomDb) -> Result<String> {
    use std::fs::{File, create_dir_all};

    let lock_path = db
        .path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".tmp")
        .join("name_gen.lock");
    if let Some(parent) = lock_path.parent() {
        create_dir_all(parent)?;
    }

    let lock_file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;

    // Acquire exclusive file lock
    use nix::fcntl::{Flock, FlockArg};
    let flock = Flock::lock(lock_file, FlockArg::LockExclusive)
        .map_err(|(_, e)| anyhow::anyhow!("flock failed: {}", e))?;

    let result = (|| -> Result<String> {
        // Build set of taken names (alive + stopped)
        let instances = db.iter_instances_full()?;
        let alive_names: HashSet<String> = instances.iter().map(|r| r.name.clone()).collect();
        let mut taken_names = alive_names.clone();

        // Also check stopped instances from events to avoid name reuse
        let stopped: Vec<String> = {
            let mut stmt = db.conn().prepare(
                "SELECT DISTINCT instance FROM events
                 WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped'",
            )?;
            stmt.query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect()
        };
        taken_names.extend(stopped);

        let name = allocate_name(
            &|n| taken_names.contains(n) || db.get_instance_full(n).ok().flatten().is_some(),
            &alive_names,
            200,
            1200,
            900.0,
        )?;

        // Reserve with placeholder row
        let now = now_epoch_i64();
        let last_event_id = db.get_last_event_id();
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!(name));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("last_event_id".into(), serde_json::json!(last_event_id));
        db.save_instance_named(&name, &data)?;

        Ok(name)
    })();

    // Unlock (drop the flock guard)
    let _file = Flock::unlock(flock);

    result
}

/// Update instance position atomically.
/// If instance doesn't exist, UPDATE silently affects 0 rows.
pub fn update_instance_position(
    db: &HcomDb,
    name: &str,
    updates: &serde_json::Map<String, serde_json::Value>,
) {
    // Convert booleans to integers for SQLite
    let mut update_copy = updates.clone();
    for bool_field in &["tcp_mode", "background", "name_announced"] {
        if let Some(val) = update_copy.get(*bool_field) {
            if let Some(b) = val.as_bool() {
                update_copy.insert(
                    (*bool_field).to_string(),
                    serde_json::json!(if b { 1 } else { 0 }),
                );
            }
        }
    }

    if let Err(e) = db.update_instance_fields(name, &update_copy) {
        crate::log::log_error(
            "core",
            "db.error",
            &format!("update_instance_position: {} - {}", name, e),
        );
    }
}

/// Persist terminal launch metadata without clobbering other launch_context fields.
///
/// The launcher owns the authoritative preset decision. launch_context is only
/// for late-bound metadata such as pane_id, terminal_id, and env snapshot.
pub fn persist_terminal_launch_context(
    db: &HcomDb,
    instance_name: &str,
    requested_preset: Option<&str>,
    effective_preset: &str,
    process_id: Option<&str>,
) {
    let mut ctx = db
        .get_instance_full(instance_name)
        .ok()
        .flatten()
        .and_then(|pos| pos.launch_context)
        .and_then(|json| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json).ok()
        })
        .unwrap_or_default();

    if let Some(pid) = process_id.filter(|v| !v.is_empty()) {
        ctx.insert("process_id".into(), serde_json::json!(pid));
    }
    if !effective_preset.is_empty() {
        ctx.insert(
            "terminal_preset_effective".into(),
            serde_json::json!(effective_preset),
        );
        // Legacy compatibility for older readers and migration logic.
        ctx.insert(
            "terminal_preset".into(),
            serde_json::json!(effective_preset),
        );
    }
    if let Some(requested) = requested_preset.filter(|v| !v.is_empty() && *v != "default") {
        ctx.insert(
            "terminal_preset_requested".into(),
            serde_json::json!(requested),
        );
    }

    let mut updates = serde_json::Map::new();
    updates.insert(
        "terminal_preset_requested".into(),
        serde_json::json!(
            requested_preset
                .filter(|v| !v.is_empty() && *v != "default")
                .unwrap_or("")
        ),
    );
    updates.insert(
        "terminal_preset_effective".into(),
        serde_json::json!(effective_preset),
    );
    updates.insert(
        "launch_context".into(),
        serde_json::json!(serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".to_string())),
    );
    update_instance_position(db, instance_name, &updates);
}

/// Capture environment context and store it for the instance.
/// Captures git branch, terminal program, tty, and relevant env vars.
pub fn capture_and_store_launch_context(db: &HcomDb, instance_name: &str) {
    let new_ctx = capture_context();

    // Preserve fields from prior context that can't be recaptured in hook env
    let preserve_keys = ["pane_id", "terminal_id", "kitty_listen_on", "process_id"];
    let mut ctx = new_ctx;

    let missing: Vec<&str> = preserve_keys
        .iter()
        .filter(|k| {
            ctx.get(**k)
                .and_then(|v| v.as_str())
                .is_none_or(|s| s.is_empty())
        })
        .copied()
        .collect();

    if !missing.is_empty() {
        if let Ok(Some(pos)) = db.get_instance_full(instance_name) {
            if let Some(old_json) = &pos.launch_context {
                if let Ok(old_ctx) = serde_json::from_str::<serde_json::Value>(old_json) {
                    for k in &missing {
                        if let Some(val) = old_ctx.get(*k) {
                            if let Some(s) = val.as_str() {
                                if !s.is_empty() {
                                    ctx.insert(k.to_string(), val.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let json = serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".to_string());
    let mut updates = serde_json::Map::new();
    updates.insert("launch_context".into(), serde_json::json!(json));
    update_instance_position(db, instance_name, &updates);
}

/// Capture launch context snapshot.
fn capture_context() -> serde_json::Map<String, serde_json::Value> {
    let mut ctx = serde_json::Map::new();

    // Git branch
    let git_branch = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    ctx.insert("git_branch".into(), serde_json::json!(git_branch));

    // TTY
    let tty = std::process::Command::new("tty")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    ctx.insert("tty".into(), serde_json::json!(tty));

    // Env vars (only include if set)
    let env_keys = [
        "TERM_PROGRAM",
        "TERM_SESSION_ID",
        "WINDOWID",
        "ITERM_SESSION_ID",
        "KITTY_WINDOW_ID",
        "KITTY_PID",
        "KITTY_LISTEN_ON",
        "ALACRITTY_WINDOW_ID",
        "WEZTERM_PANE",
        "GNOME_TERMINAL_SCREEN",
        "KONSOLE_DBUS_WINDOW",
        "TERMINATOR_UUID",
        "TILIX_ID",
        "GUAKE_TAB_UUID",
        "WT_SESSION",
        "ConEmuHWND",
        "TMUX_PANE",
        "STY",
        "ZELLIJ_SESSION_NAME",
        "ZELLIJ_PANE_ID",
        "SSH_TTY",
        "SSH_CONNECTION",
        "WSL_DISTRO_NAME",
        "VSCODE_PID",
        "CURSOR_AGENT",
        "INSIDE_EMACS",
        "NVIM_LISTEN_ADDRESS",
        "CODESPACE_NAME",
        "GITPOD_WORKSPACE_ID",
        "CLOUD_SHELL",
        "REPL_ID",
    ];
    let mut env_map = serde_json::Map::new();
    for key in &env_keys {
        if let Ok(val) = std::env::var(key) {
            if !val.is_empty() {
                env_map.insert((*key).to_string(), serde_json::json!(val));
            }
        }
    }
    ctx.insert("env".into(), serde_json::Value::Object(env_map));

    // Pane IDs are late-bound. The launcher already persisted the effective preset.
    if let Ok(preset_name) = std::env::var("HCOM_LAUNCHED_PRESET") {
        if !preset_name.is_empty() {
            if let Some(pane_id_env) = crate::config::get_merged_preset_pane_id_env(&preset_name) {
                if let Ok(pane_id) = std::env::var(pane_id_env) {
                    if !pane_id.is_empty() {
                        ctx.insert("pane_id".into(), serde_json::json!(pane_id));
                    }
                }
            }
        }
    }

    // Process ID for kitty close-by-env matching
    if let Ok(pid) = std::env::var("HCOM_PROCESS_ID") {
        if !pid.is_empty() {
            ctx.insert("process_id".into(), serde_json::json!(pid));

            // Terminal ID from parent's stdout capture
            let id_file = crate::paths::hcom_dir()
                .join(".tmp")
                .join("terminal_ids")
                .join(&pid);
            if id_file.exists() {
                if let Ok(content) = std::fs::read_to_string(&id_file) {
                    let terminal_id = content.trim().to_string();
                    if !terminal_id.is_empty() {
                        ctx.insert("terminal_id".into(), serde_json::json!(terminal_id));
                    }
                }
                let _ = std::fs::remove_file(&id_file);
            }
        }
    }

    ctx
}

/// Set instance status with timestamp and log status change event.
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
    // Check if first status update
    let (current_data, db_error) = match db.get_instance_full(instance_name) {
        Ok(data) => (data, false),
        Err(e) => {
            eprintln!("[hcom] warn: set_status DB read failed for {instance_name}: {e}");
            (None, true)
        }
    };
    // On DB error, assume not new to avoid spurious ready events
    let is_new = if db_error {
        false
    } else {
        current_data
            .as_ref()
            .map(|d| d.status_context == "new")
            .unwrap_or(true)
    };

    let now = now_epoch_i64();

    // Build updates
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

    update_instance_position(db, instance_name, &updates);

    // Wake delivery loop if status changed
    if status_changed {
        let _ = notify_instance_with_db(db, instance_name);
    }

    // First status update: log "ready" life event
    if is_new {
        let launcher = launcher_override
            .map(|s| s.to_string())
            .or_else(|| std::env::var("HCOM_LAUNCHED_BY").ok())
            .unwrap_or_else(|| "unknown".to_string());
        let batch_id = batch_id_override
            .map(|s| s.to_string())
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
            crate::log::log_error("core", "db.error", &format!("ready event: {}", e));
        }

        // Check batch completion
        if launcher != "unknown" {
            if let Some(ref bid) = batch_id {
                if let Err(e) = db.check_batch_completion(&launcher, bid) {
                    crate::log::log_error(
                        "core",
                        "db.error",
                        &format!("batch notification: {}", e),
                    );
                }
            }
        }
    }

    // Log status change event (best-effort)
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

/// Wake an instance by connecting to its registered notify endpoints.
///
/// `kinds` filters which endpoint types to notify (e.g. `&["pty", "listen"]`).
/// Pass an empty slice to notify all endpoint kinds for the instance.
pub fn notify_instance_endpoints(db: &HcomDb, instance_name: &str, kinds: &[&str]) {
    use std::net::TcpStream;

    let ports: Vec<i64> = if kinds.is_empty() {
        db.conn()
            .prepare("SELECT port FROM notify_endpoints WHERE instance = ?")
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![instance_name], |row| row.get::<_, i64>(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default()
    } else {
        let placeholders: String = kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT port FROM notify_endpoints WHERE instance = ? AND kind IN ({})",
            placeholders
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(instance_name.to_string())];
        for k in kinds {
            params.push(Box::new(k.to_string()));
        }
        db.conn()
            .prepare(&sql)
            .and_then(|mut stmt| {
                stmt.query_map(
                    rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
                    |row| row.get::<_, i64>(0),
                )
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default()
    };

    for port in ports {
        if port > 0 && port <= 65535 {
            let addr = format!("127.0.0.1:{}", port);
            if let Ok(addr) = addr.parse() {
                let _ = TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(100));
            }
        }
    }
}

pub fn notify_instance_with_db(db: &HcomDb, instance_name: &str) -> Result<()> {
    notify_instance_endpoints(db, instance_name, &["pty", "listen", "listen_filter"]);
    Ok(())
}

/// Notify all instances via their TCP notify ports (wake delivery loops).
/// Used after sending messages or receiving relay events.
pub fn notify_all_instances(db: &HcomDb) {
    use std::net::TcpStream;

    let Ok(mut stmt) = db
        .conn()
        .prepare("SELECT DISTINCT port FROM notify_endpoints WHERE port > 0")
    else {
        return;
    };

    let ports: Vec<i64> = stmt
        .query_map([], |row| row.get(0))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .collect();

    for port in ports {
        if port > 0 && port <= 65535 {
            let addr = format!("127.0.0.1:{port}");
            let _ = TcpStream::connect_timeout(
                &addr.parse().unwrap(),
                std::time::Duration::from_millis(50),
            );
        }
    }
}

/// Bind session_id to canonical instance for process_id.
/// Handles 4 paths: canonical exists (with placeholder merge/switch), placeholder bind,
/// and two no-op paths.
pub fn bind_session_to_process(
    db: &HcomDb,
    session_id: &str,
    process_id: Option<&str>,
) -> Option<String> {
    if session_id.is_empty() {
        crate::log::log_info("binding", "bind_session_to_process.no_session_id", "");
        return None;
    }

    crate::log::log_info(
        "binding",
        "bind_session_to_process.entry",
        &format!("session_id={}, process_id={:?}", session_id, process_id),
    );

    // Find placeholder from process binding
    let (placeholder_name, placeholder_data) = if let Some(pid) = process_id {
        match db.get_process_binding(pid) {
            Ok(Some(name)) => {
                let data = match db.get_instance_full(&name) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("[hcom] warn: get_instance_full failed for {name}: {e}");
                        None
                    }
                };
                (Some(name), data)
            }
            _ => (None, None),
        }
    } else {
        (None, None)
    };

    // Find canonical from session binding
    let canonical = match db.get_session_binding(session_id) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[hcom] warn: get_session_binding failed for {session_id}: {e}");
            None
        }
    };

    // Path 1: Canonical exists (session already bound)
    if let Some(ref canonical_name) = canonical {
        crate::log::log_info(
            "binding",
            "bind_session_to_process.canonical_exists",
            &format!(
                "canonical={}, placeholder={:?}",
                canonical_name, placeholder_name
            ),
        );

        // Reset last_stop on resume
        let now = now_epoch_i64();
        let mut resume_updates = serde_json::Map::new();
        resume_updates.insert("last_stop".into(), serde_json::json!(now));

        if let Some(ref ph_name) = placeholder_name {
            if ph_name != canonical_name {
                // Always migrate notify_endpoints
                if let Err(e) = db.migrate_notify_endpoints(ph_name, canonical_name) {
                    crate::log::log_error(
                        "binding",
                        "bind_canonical.migrate_endpoints",
                        &format!("{e}"),
                    );
                }

                let is_true_placeholder = placeholder_data
                    .as_ref()
                    .map(|d| d.session_id.is_none())
                    .unwrap_or(false);

                if is_true_placeholder {
                    // Path 1a: True placeholder merge
                    if let Some(ref ph_data) = placeholder_data {
                        if let Some(ref tag) = ph_data.tag {
                            resume_updates.insert("tag".into(), serde_json::json!(tag));
                        }
                        if ph_data.background != 0 {
                            resume_updates
                                .insert("background".into(), serde_json::json!(ph_data.background));
                        }
                        if let Some(ref args) = ph_data.launch_args {
                            resume_updates.insert("launch_args".into(), serde_json::json!(args));
                        }
                        // Reset status_context for ready event
                        if std::env::var("HCOM_LAUNCHED").as_deref() == Ok("1") {
                            resume_updates
                                .insert("status_context".into(), serde_json::json!("new"));
                        }
                    }

                    // Delete true placeholder (temporary identity)
                    match db.delete_instance(ph_name) {
                        Ok(true) => {} // Success
                        Ok(false) => {
                            // Not found — rollback notify_endpoints migration
                            if let Err(e) = db.migrate_notify_endpoints(canonical_name, ph_name) {
                                crate::log::log_error(
                                    "binding",
                                    "bind_canonical.rollback_endpoints",
                                    &format!("{e}"),
                                );
                            }
                        }
                        Err(e) => {
                            crate::log::log_error(
                                "binding",
                                "bind_canonical.delete_placeholder",
                                &format!("{e}"),
                            );
                            if let Err(e) = db.migrate_notify_endpoints(canonical_name, ph_name) {
                                crate::log::log_error(
                                    "binding",
                                    "bind_canonical.rollback_endpoints",
                                    &format!("{e}"),
                                );
                            }
                        }
                    }
                } else {
                    // Path 1b: Session switch — mark old instance inactive
                    set_status(
                        db,
                        ph_name,
                        ST_INACTIVE,
                        "exit:session_switch",
                        Default::default(),
                    );
                    if let Err(e) = db.delete_session_bindings_for_instance(ph_name) {
                        crate::log::log_error(
                            "binding",
                            "bind_canonical.delete_session_bindings",
                            &format!("{e}"),
                        );
                    }
                }
            }
        }

        // Apply resume updates
        update_instance_position(db, canonical_name, &resume_updates);

        if let Some(pid) = process_id {
            if let Err(e) = db.set_process_binding(pid, session_id, canonical_name) {
                crate::log::log_error(
                    "binding",
                    "bind_canonical.set_process_binding",
                    &format!("{e}"),
                );
            }
        }

        return Some(canonical_name.clone());
    }

    // Path 2: No canonical, but placeholder exists — bind session to placeholder
    if let Some(ref ph_name) = placeholder_name {
        crate::log::log_info(
            "binding",
            "bind_session_to_process.bind_placeholder",
            &format!("placeholder={}, session_id={}", ph_name, session_id),
        );

        // Clear UNIQUE constraint conflicts
        if let Err(e) = db.clear_session_id_from_other_instances(session_id, ph_name) {
            crate::log::log_error("binding", "bind_placeholder.clear_session", &format!("{e}"));
        }

        let mut updates = serde_json::Map::new();
        updates.insert("session_id".into(), serde_json::json!(session_id));
        update_instance_position(db, ph_name, &updates);

        if let Err(e) = db.rebind_session(session_id, ph_name) {
            crate::log::log_error(
                "binding",
                "bind_placeholder.rebind_session",
                &format!("{e}"),
            );
        }
        if let Some(pid) = process_id {
            if let Err(e) = db.set_process_binding(pid, session_id, ph_name) {
                crate::log::log_error(
                    "binding",
                    "bind_placeholder.set_process_binding",
                    &format!("{e}"),
                );
            }
        }

        return Some(ph_name.clone());
    }

    // Path 3/4: No canonical, no placeholder — no-op
    crate::log::log_info("binding", "bind_session_to_process.return_none", "");
    None
}

/// Initialize instance in DB with required fields (idempotent).
#[allow(clippy::too_many_arguments)]
pub fn initialize_instance_in_position_file(
    db: &HcomDb,
    instance_name: &str,
    session_id: Option<&str>,
    parent_session_id: Option<&str>,
    parent_name: Option<&str>,
    agent_id: Option<&str>,
    transcript_path: Option<&str>,
    tool: Option<&str>,
    background: bool,
    tag: Option<&str>,
    wait_timeout: Option<i64>,
    subagent_timeout: Option<i64>,
    hints: Option<&str>,
    cwd_override: Option<&str>,
) -> bool {
    let cwd = cwd_override.map(|s| s.to_string()).unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    let is_launched = std::env::var("HCOM_LAUNCHED").as_deref() == Ok("1");

    // Check if already exists
    match db.get_instance_full(instance_name) {
        Ok(Some(existing)) => {
            let mut updates = serde_json::Map::new();
            updates.insert("directory".into(), serde_json::json!(cwd));

            if let Some(sid) = session_id {
                updates.insert("session_id".into(), serde_json::json!(sid));
            }
            if let Some(psid) = parent_session_id {
                updates.insert("parent_session_id".into(), serde_json::json!(psid));
            }
            if let Some(pn) = parent_name {
                updates.insert("parent_name".into(), serde_json::json!(pn));
            }
            if let Some(aid) = agent_id {
                updates.insert("agent_id".into(), serde_json::json!(aid));
            }
            if let Some(tp) = transcript_path {
                updates.insert("transcript_path".into(), serde_json::json!(tp));
            }
            if let Some(t) = tool {
                updates.insert("tool".into(), serde_json::json!(t));
            }
            if let Some(t) = tag {
                updates.insert("tag".into(), serde_json::json!(t));
            }
            if background {
                updates.insert("background".into(), serde_json::json!(1));
            }

            // Fix last_event_id for true placeholders
            let is_true_placeholder = existing.session_id.is_none();
            if existing.last_event_id == 0 && is_true_placeholder {
                let current_max = db.get_last_event_id();
                let launch_event_id = std::env::var("HCOM_LAUNCH_EVENT_ID")
                    .ok()
                    .and_then(|s| s.parse::<i64>().ok());

                let eid = match launch_event_id {
                    Some(id) if id <= current_max => id,
                    _ => current_max,
                };
                updates.insert("last_event_id".into(), serde_json::json!(eid));
            }

            if is_launched {
                updates.insert("status_context".into(), serde_json::json!("new"));
            }

            if !updates.is_empty() {
                let _ = db.update_instance_fields(instance_name, &updates);
            }

            true
        }
        Ok(None) => {
            // New instance
            let now = now_epoch_f64();
            let current_max = db.get_last_event_id();
            let launch_event_id = std::env::var("HCOM_LAUNCH_EVENT_ID")
                .ok()
                .and_then(|s| s.parse::<i64>().ok());

            let initial_event_id = match launch_event_id {
                Some(id) if id <= current_max => id,
                _ => current_max,
            };

            let mut data = serde_json::Map::new();
            data.insert("name".into(), serde_json::json!(instance_name));
            data.insert("last_event_id".into(), serde_json::json!(initial_event_id));
            data.insert("directory".into(), serde_json::json!(cwd));
            data.insert("last_stop".into(), serde_json::json!(0));
            data.insert("created_at".into(), serde_json::json!(now));
            data.insert(
                "session_id".into(),
                match session_id {
                    Some(s) if !s.is_empty() => serde_json::json!(s),
                    _ => serde_json::Value::Null,
                },
            );
            data.insert("transcript_path".into(), serde_json::json!(""));
            data.insert("name_announced".into(), serde_json::json!(0));
            data.insert("tag".into(), serde_json::Value::Null);
            data.insert("status".into(), serde_json::json!(ST_INACTIVE));
            data.insert("status_time".into(), serde_json::json!(now_epoch_i64()));
            data.insert("status_context".into(), serde_json::json!("new"));
            data.insert("tool".into(), serde_json::json!(tool.unwrap_or("claude")));
            data.insert(
                "background".into(),
                serde_json::json!(if background { 1 } else { 0 }),
            );

            // Set tag: use provided tag, or fall back to config tag for real instances
            if let Some(t) = tag {
                data.insert("tag".into(), serde_json::json!(t));
            } else if session_id.is_some() || parent_session_id.is_some() || is_launched {
                if let Ok(hcom_config) = crate::config::HcomConfig::load(None) {
                    if !hcom_config.tag.is_empty() {
                        data.insert("tag".into(), serde_json::json!(hcom_config.tag));
                    }
                }
            }

            if let Some(wt) = wait_timeout {
                data.insert("wait_timeout".into(), serde_json::json!(wt));
            }
            if let Some(st) = subagent_timeout {
                data.insert("subagent_timeout".into(), serde_json::json!(st));
            }
            if let Some(h) = hints {
                data.insert("hints".into(), serde_json::json!(h));
            }
            if let Some(psid) = parent_session_id {
                data.insert("parent_session_id".into(), serde_json::json!(psid));
            }
            if let Some(pn) = parent_name {
                data.insert("parent_name".into(), serde_json::json!(pn));
            }
            if let Some(aid) = agent_id {
                data.insert("agent_id".into(), serde_json::json!(aid));
            }
            if let Some(tp) = transcript_path {
                data.insert("transcript_path".into(), serde_json::json!(tp));
            }

            match db.save_instance_named(instance_name, &data) {
                Ok(true) => {
                    // Log creation event
                    let launcher =
                        std::env::var("HCOM_LAUNCHED_BY").unwrap_or_else(|_| "unknown".to_string());
                    let event_data = serde_json::json!({
                        "action": "created",
                        "by": launcher,
                        "is_hcom_launched": is_launched,
                        "is_subagent": parent_session_id.is_some(),
                        "parent_name": parent_name.unwrap_or(""),
                    });
                    let _ = db.log_event("life", instance_name, &event_data);
                    // Auto-subscribe to default event presets from config
                    auto_subscribe_defaults(db, instance_name, tool.unwrap_or(""));
                    true
                }
                _ => true, // IntegrityError = another process won the race, treat as success
            }
        }
        Err(_) => false,
    }
}

/// Create orphaned PTY identity — called when process binding exists but session_id
/// is fresh (e.g., after /clear). Generates new name, creates instance, binds it.
pub fn create_orphaned_pty_identity(
    db: &HcomDb,
    session_id: &str,
    process_id: Option<&str>,
    tool: &str,
) -> Option<String> {
    let name = match generate_unique_name(db) {
        Ok(n) => n,
        Err(e) => {
            crate::log::log_error(
                "instances",
                "create_orphaned_pty_identity.name_gen",
                &e.to_string(),
            );
            return None;
        }
    };

    let success = initialize_instance_in_position_file(
        db,
        &name,
        Some(session_id),
        None, // parent_session_id
        None, // parent_name
        None, // agent_id
        None, // transcript_path
        Some(tool),
        false, // background
        None,  // tag
        None,  // wait_timeout
        None,  // subagent_timeout
        None,  // hints
        None,  // cwd_override
    );

    if !success {
        return None;
    }

    // Bind session
    if let Err(e) = db.rebind_session(session_id, &name) {
        eprintln!("[hcom] warn: rebind_session failed for {name}: {e}");
    }
    if let Some(pid) = process_id {
        if let Err(e) = db.set_process_binding(pid, session_id, &name) {
            eprintln!("[hcom] warn: set_process_binding failed for {name}: {e}");
        }
    }

    Some(name)
}

/// Delete placeholder instances that have been launching too long.
pub fn cleanup_stale_placeholders(db: &HcomDb) -> i32 {
    let mut deleted = 0;
    let now = now_epoch_f64();

    if let Ok(instances) = db.iter_instances_full() {
        for data in &instances {
            if !is_launching_placeholder(data) {
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
    // During wake grace: skip all stale cleanup
    if is_in_wake_grace() {
        return 0;
    }

    // Cleanup remote instances whose device hasn't pushed in >90s
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

            // Exit contexts: 1min cleanup
            if matches!(
                context.as_str(),
                "killed" | "closed" | "timeout" | "interrupted" | "session_switch"
            ) && age > 60
            {
                crate::hooks::common::stop_instance(db, &data.name, "system", "exit_cleanup");
                deleted += 1;
                return deleted; // One per cycle
            }

            // Stale: shorter threshold
            if context == "stale" && max_stale_seconds > 0 && age > max_stale_seconds {
                crate::hooks::common::stop_instance(db, &data.name, "system", "stale_cleanup");
                deleted += 1;
                return deleted;
            }

            // Any other inactive: longer threshold
            if max_inactive_seconds > 0 && age > max_inactive_seconds {
                crate::hooks::common::stop_instance(db, &data.name, "system", "inactive_cleanup");
                deleted += 1;
                return deleted;
            }
        }
    }

    deleted
}

/// Delete remote instance rows whose device hasn't pushed in >90s.
fn cleanup_stale_remote_instances(db: &HcomDb) {
    let now = now_epoch_f64();
    let sync_map: std::collections::HashMap<String, String> = db
        .kv_prefix("relay_sync_time_")
        .unwrap_or_default()
        .into_iter()
        .collect();

    if let Ok(instances) = db.iter_instances_full() {
        let device_ids: HashSet<String> = instances
            .iter()
            .filter_map(|d| d.origin_device_id.clone())
            .collect();

        for device_id in device_ids {
            let sync_val = sync_map.get(&format!("relay_sync_time_{}", device_id));
            let sync_time: f64 = sync_val.and_then(|s| s.parse().ok()).unwrap_or(0.0);
            if sync_time > 0.0 && (now - sync_time) <= REMOTE_DEVICE_STALE_THRESHOLD {
                continue;
            }
            // Stale device — delete its instances
            if let Err(e) = db.conn().execute(
                "DELETE FROM instances WHERE origin_device_id = ?",
                rusqlite::params![device_id],
            ) {
                crate::log::log_warn("cleanup", "remote_stale_cleanup_fail", &e.to_string());
            } else {
                crate::log::log_info(
                    "cleanup",
                    "remote_device_stale",
                    &device_id[..8.min(device_id.len())],
                );
            }
        }
    }
}

/// Resolve instance name for a process_id via process_bindings.
pub fn resolve_process_binding(db: &HcomDb, process_id: Option<&str>) -> Option<String> {
    let pid = process_id?;
    db.get_process_binding(pid).ok()?
}

/// Resolve instance via process binding, session binding, or transcript marker.
pub fn resolve_instance_from_binding(
    db: &HcomDb,
    session_id: Option<&str>,
    process_id: Option<&str>,
) -> Option<InstanceRow> {
    // Path 1: Process binding
    if let Some(pid) = process_id {
        if let Ok(Some(name)) = db.get_process_binding(pid) {
            if let Ok(Some(instance)) = db.get_instance_full(&name) {
                return Some(instance);
            }
        }
    }

    // Path 2: Session binding
    if let Some(sid) = session_id {
        if let Some(name) = db.get_session_binding(sid).ok().flatten() {
            if let Ok(Some(instance)) = db.get_instance_full(&name) {
                return Some(instance);
            }
        }
    }

    None
}

/// Auto-subscribe instance to default event subscriptions from config.
/// Called during instance creation.
fn auto_subscribe_defaults(db: &HcomDb, instance_name: &str, tool: &str) {
    if !matches!(tool, "claude" | "gemini" | "codex" | "opencode") {
        return;
    }

    // Clean up stale subscriptions from previously stopped instances with reused names
    let _ = db.cleanup_subscriptions(instance_name);
    let _ = db.cleanup_thread_memberships_for_name_reuse(instance_name);
    let config = match crate::config::HcomConfig::load(None) {
        Ok(c) => c,
        Err(_) => return,
    };
    if config.auto_subscribe.is_empty() {
        return;
    }

    use std::collections::HashMap;

    // Map preset names to filter flags
    let preset_to_flags: HashMap<&str, Vec<(&str, &str)>> = HashMap::from([
        ("collision", vec![("collision", "1")]),
        ("created", vec![("action", "created")]),
        ("stopped", vec![("action", "stopped")]),
        ("blocked", vec![("status", "blocked")]),
    ]);

    for preset in config
        .auto_subscribe
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if let Some(flag_pairs) = preset_to_flags.get(preset) {
            let mut filters: HashMap<String, Vec<String>> = HashMap::new();
            for (key, val) in flag_pairs {
                filters
                    .entry(key.to_string())
                    .or_default()
                    .push(val.to_string());
            }
            let _ = crate::commands::events::create_filter_subscription(
                db,
                &filters,
                &[],
                instance_name,
                false,
                true,
            );
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
            "test_instances_{}_{}.db",
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

    #[test]
    fn test_name_pool_populated() {
        let pool = name_pool();
        assert!(pool.len() > 1000, "pool should have >1000 names");
        // Gold names should be at the top
        let top_100: HashSet<&str> = pool[..100].iter().map(|x| x.name.as_str()).collect();
        assert!(top_100.contains("luna"), "luna should be in top 100");
        assert!(top_100.contains("nova"), "nova should be in top 100");
    }

    #[test]
    fn test_banned_names_excluded() {
        let pool = name_pool();
        let all_names: HashSet<&str> = pool.iter().map(|x| x.name.as_str()).collect();
        assert!(!all_names.contains("help"));
        assert!(!all_names.contains("send"));
        assert!(!all_names.contains("list"));
        assert!(!all_names.contains("stop"));
    }

    #[test]
    fn test_gold_names_score_higher() {
        let gold = gold_names();
        let banned = banned_names();
        let gold_score = score_name("luna", &gold, &banned);
        let non_gold = score_name("bxzx", &gold, &banned); // unlikely to be gold
        assert!(gold_score > non_gold, "gold names should score higher");
    }

    #[test]
    fn test_hamming_similarity_check() {
        let mut alive = HashSet::new();
        alive.insert("luna".to_string());

        // 1 char different = too similar
        assert!(is_too_similar("lina", &alive));
        assert!(is_too_similar("luno", &alive));
        // 2 chars different = too similar
        assert!(is_too_similar("lino", &alive));
        // 3+ chars different = ok
        assert!(!is_too_similar("miso", &alive));
        assert!(!is_too_similar("kira", &alive));
    }

    #[test]
    fn test_allocate_name_avoids_taken() {
        let taken: HashSet<String> = ["luna", "nova", "kira"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let alive = taken.clone();
        let name = allocate_name(&|n| taken.contains(n), &alive, 200, 1200, 900.0).unwrap();
        assert!(!taken.contains(&name));
    }

    #[test]
    fn test_hash_to_name_deterministic() {
        let n1 = hash_to_name("device-123", 0);
        let n2 = hash_to_name("device-123", 0);
        assert_eq!(n1, n2);
    }

    #[test]
    fn test_hash_to_name_collision_avoidance() {
        let n1 = hash_to_name("device-123", 0);
        let n2 = hash_to_name("device-123", 1);
        assert_ne!(n1, n2);
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
    fn test_persist_terminal_launch_context_stores_presets_in_launch_context() {
        let (db, path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params!["luna", "codex", 1.0f64],
            )
            .unwrap();

        persist_terminal_launch_context(&db, "luna", Some("kitty"), "kitty-tab", Some("proc-1"));

        let row = db.get_instance_full("luna").unwrap().unwrap();
        let ctx: serde_json::Value =
            serde_json::from_str(row.launch_context.as_deref().unwrap_or("{}")).unwrap();

        assert_eq!(
            ctx.get("terminal_preset_effective")
                .and_then(|v| v.as_str()),
            Some("kitty-tab")
        );
        assert_eq!(
            ctx.get("terminal_preset").and_then(|v| v.as_str()),
            Some("kitty-tab")
        );
        assert_eq!(
            ctx.get("terminal_preset_requested")
                .and_then(|v| v.as_str()),
            Some("kitty")
        );
        assert_eq!(
            ctx.get("process_id").and_then(|v| v.as_str()),
            Some("proc-1")
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
            last_stop: now - 2, // Fresh heartbeat
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
            last_stop: now - 100, // Old heartbeat
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
        // Remote instances trust synced status
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
    fn test_is_launching_placeholder() {
        let ph = InstanceRow {
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            session_id: None,
            ..default_instance()
        };
        assert!(is_launching_placeholder(&ph));

        let bound = InstanceRow {
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            session_id: Some("sid-123".into()),
            ..default_instance()
        };
        assert!(!is_launching_placeholder(&bound));
    }

    #[test]
    fn test_is_remote_instance() {
        let local = default_instance();
        assert!(!is_remote_instance(&local));

        let remote = InstanceRow {
            origin_device_id: Some("device-123".into()),
            ..default_instance()
        };
        assert!(is_remote_instance(&remote));
    }

    #[test]
    fn test_get_full_name() {
        let plain = InstanceRow {
            name: "luna".into(),
            tag: None,
            ..default_instance()
        };
        assert_eq!(get_full_name(&plain), "luna");

        let tagged = InstanceRow {
            name: "luna".into(),
            tag: Some("team".into()),
            ..default_instance()
        };
        assert_eq!(get_full_name(&tagged), "team-luna");
    }

    #[test]
    fn test_resolve_display_name_or_stopped_tagged_snapshot() {
        let (db, _path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data)
                 VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'life', 'luna', ?1)",
                rusqlite::params![
                    serde_json::json!({
                        "action": "stopped",
                        "snapshot": {
                            "tag": "team"
                        }
                    })
                    .to_string()
                ],
            )
            .unwrap();

        assert_eq!(
            resolve_display_name_or_stopped(&db, "team-luna").as_deref(),
            Some("luna")
        );
        assert_eq!(
            resolve_display_name_or_stopped(&db, "luna").as_deref(),
            Some("luna")
        );
    }

    #[test]
    fn test_parse_running_tasks() {
        assert!(!parse_running_tasks(None).active);
        assert!(!parse_running_tasks(Some("")).active);
        assert!(!parse_running_tasks(Some("invalid")).active);

        let rt = parse_running_tasks(Some(r#"{"active":true,"subagents":[{"agent_id":"a1"}]}"#));
        assert!(rt.active);
        assert_eq!(rt.subagents.len(), 1);
    }

    #[test]
    fn test_bind_session_path2_placeholder() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        // Create placeholder instance (no session_id)
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        db.save_instance_named("luna", &data).unwrap();

        // Create process binding
        db.set_process_binding("pid-123", "", "luna").unwrap();

        // Bind session to process
        let result = bind_session_to_process(&db, "sid-456", Some("pid-123"));
        assert_eq!(result, Some("luna".to_string()));

        // Verify session_id was set
        let inst = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sid-456"));

        // Verify session binding was created
        let binding = db.get_session_binding("sid-456").unwrap();
        assert_eq!(binding, Some("luna".to_string()));

        cleanup(path);
    }

    #[test]
    fn test_bind_session_path1a_true_placeholder_merge() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        // Create canonical instance (with session_id)
        let mut canonical_data = serde_json::Map::new();
        canonical_data.insert("name".into(), serde_json::json!("miso"));
        canonical_data.insert("session_id".into(), serde_json::json!("sid-789"));
        canonical_data.insert("created_at".into(), serde_json::json!(now));
        canonical_data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("miso", &canonical_data).unwrap();
        db.rebind_session("sid-789", "miso").unwrap();

        // Create placeholder (no session_id, has tag)
        let mut ph_data = serde_json::Map::new();
        ph_data.insert("name".into(), serde_json::json!("temp"));
        ph_data.insert("tag".into(), serde_json::json!("team"));
        ph_data.insert("created_at".into(), serde_json::json!(now));
        ph_data.insert("status".into(), serde_json::json!("pending"));
        ph_data.insert("status_context".into(), serde_json::json!("new"));
        db.save_instance_named("temp", &ph_data).unwrap();

        // Process binding points to placeholder
        db.set_process_binding("pid-123", "", "temp").unwrap();

        // Bind session (session already has canonical "miso")
        let result = bind_session_to_process(&db, "sid-789", Some("pid-123"));
        assert_eq!(result, Some("miso".to_string()));

        // Placeholder should be deleted
        assert!(db.get_instance_full("temp").unwrap().is_none());

        // Tag should be merged to canonical
        let inst = db.get_instance_full("miso").unwrap().unwrap();
        assert_eq!(inst.tag.as_deref(), Some("team"));

        cleanup(path);
    }

    #[test]
    fn test_bind_session_path1b_session_switch_marks_old_inactive() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        // Canonical instance already bound to sid-789
        let mut canonical_data = serde_json::Map::new();
        canonical_data.insert("name".into(), serde_json::json!("miso"));
        canonical_data.insert("session_id".into(), serde_json::json!("sid-789"));
        canonical_data.insert("created_at".into(), serde_json::json!(now));
        canonical_data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("miso", &canonical_data).unwrap();
        db.rebind_session("sid-789", "miso").unwrap();

        // Placeholder already has a different session_id, so this must go through path 1b.
        let mut ph_data = serde_json::Map::new();
        ph_data.insert("name".into(), serde_json::json!("temp"));
        ph_data.insert("session_id".into(), serde_json::json!("sid-old"));
        ph_data.insert("created_at".into(), serde_json::json!(now));
        ph_data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("temp", &ph_data).unwrap();
        db.rebind_session("sid-old", "temp").unwrap();
        db.set_process_binding("pid-123", "sid-old", "temp")
            .unwrap();

        let result = bind_session_to_process(&db, "sid-789", Some("pid-123"));
        assert_eq!(result, Some("miso".to_string()));

        // Placeholder is retained but marked inactive for session switch.
        let placeholder = db.get_instance_full("temp").unwrap().unwrap();
        assert_eq!(placeholder.status, ST_INACTIVE);
        assert_eq!(placeholder.status_context, "exit:session_switch");

        // Old session binding is cleared from the placeholder instance.
        assert_eq!(db.get_session_binding("sid-old").unwrap(), None);

        // Process binding now points to canonical.
        assert_eq!(
            db.get_process_binding("pid-123").unwrap(),
            Some("miso".to_string())
        );

        cleanup(path);
    }

    #[test]
    fn test_bind_session_no_match() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        // No process binding, no session binding
        let result = bind_session_to_process(&db, "sid-999", None);
        assert_eq!(result, None);

        cleanup(path);
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
    fn test_create_orphaned_pty_identity_basic() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let result = create_orphaned_pty_identity(&db, "sess-orphan", Some("pid-orphan"), "claude");
        assert!(result.is_some(), "should create orphaned identity");

        let name = result.unwrap();
        // Verify instance exists with correct fields
        let inst = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sess-orphan"));
        assert_eq!(inst.tool, "claude");

        // Verify session binding created
        assert_eq!(
            db.get_session_binding("sess-orphan").unwrap(),
            Some(name.clone())
        );

        // Verify process binding created
        assert_eq!(db.get_process_binding("pid-orphan").unwrap(), Some(name));

        cleanup(path);
    }

    #[test]
    fn test_create_orphaned_pty_identity_no_process_id() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let result = create_orphaned_pty_identity(&db, "sess-orphan2", None, "gemini");
        assert!(result.is_some());

        let name = result.unwrap();
        let inst = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(inst.tool, "gemini");

        // Session binding exists, no process binding
        assert_eq!(db.get_session_binding("sess-orphan2").unwrap(), Some(name));

        cleanup(path);
    }

    #[test]
    fn test_cleanup_stale_placeholders_deletes_old() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        // Create a placeholder older than CLEANUP_PLACEHOLDER_THRESHOLD (120s)
        let old_time = now_epoch_f64() - 200.0; // 200s ago
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

        // Create a fresh placeholder (< 120s old)
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

        // Instance with session_id is not a placeholder (even if old and status_context=new)
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

    #[test]
    fn test_resolve_from_binding_process_binding() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("session_id".into(), serde_json::json!("sess-1"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("luna", &data).unwrap();
        db.set_process_binding("pid-1", "sess-1", "luna").unwrap();

        let result = resolve_instance_from_binding(&db, None, Some("pid-1"));
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "luna");

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_session_binding() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("nova"));
        data.insert("session_id".into(), serde_json::json!("sess-2"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("status".into(), serde_json::json!("active"));
        db.save_instance_named("nova", &data).unwrap();
        db.rebind_session("sess-2", "nova").unwrap();

        let result = resolve_instance_from_binding(&db, Some("sess-2"), None);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "nova");

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_process_over_session() {
        // Process binding should take priority over session binding
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut d1 = serde_json::Map::new();
        d1.insert("name".into(), serde_json::json!("luna"));
        d1.insert("created_at".into(), serde_json::json!(now));
        d1.insert("status".into(), serde_json::json!("active"));
        db.save_instance_named("luna", &d1).unwrap();
        db.set_process_binding("pid-1", "", "luna").unwrap();

        let mut d2 = serde_json::Map::new();
        d2.insert("name".into(), serde_json::json!("nova"));
        d2.insert("session_id".into(), serde_json::json!("sess-2"));
        d2.insert("created_at".into(), serde_json::json!(now));
        d2.insert("status".into(), serde_json::json!("active"));
        db.save_instance_named("nova", &d2).unwrap();
        db.rebind_session("sess-2", "nova").unwrap();

        let result = resolve_instance_from_binding(&db, Some("sess-2"), Some("pid-1"));
        assert_eq!(result.unwrap().name, "luna"); // process wins

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_process_binding_instance_deleted() {
        // Process binding exists but instance row was deleted — should fall through
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        db.set_process_binding("pid-ghost", "", "ghost").unwrap();
        // No instance "ghost" exists

        let result = resolve_instance_from_binding(&db, None, Some("pid-ghost"));
        assert!(result.is_none());

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_no_match() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let result = resolve_instance_from_binding(&db, Some("nonexistent"), Some("nope"));
        assert!(result.is_none());

        cleanup(path);
    }

    #[test]
    fn test_session_binding_cascade_on_instance_delete() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("session_id".into(), serde_json::json!("sess-1"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("status".into(), serde_json::json!("active"));
        db.save_instance_named("luna", &data).unwrap();
        db.rebind_session("sess-1", "luna").unwrap();

        // Confirm binding exists
        assert_eq!(
            db.get_session_binding("sess-1").unwrap(),
            Some("luna".to_string())
        );

        // Delete instance — CASCADE should remove session binding
        db.delete_instance("luna").unwrap();
        assert_eq!(db.get_session_binding("sess-1").unwrap(), None);

        cleanup(path);
    }

    #[test]
    fn test_hamming_distance_with_many_alive() {
        // With many alive names, similarity rejection should still work
        let mut alive = HashSet::new();
        alive.insert("luna".to_string());
        alive.insert("nova".to_string());
        alive.insert("miso".to_string());
        alive.insert("kira".to_string());
        alive.insert("duma".to_string());

        // 1 char different from "luna"
        assert!(is_too_similar("lina", &alive));
        // 1 char different from "nova"
        assert!(is_too_similar("nava", &alive));
        // 3+ chars different from all
        assert!(!is_too_similar("bize", &alive));
    }

    #[test]
    fn test_hamming_distance_different_lengths() {
        // Names of different length are never considered too similar
        let mut alive = HashSet::new();
        alive.insert("luna".to_string());

        assert!(!is_too_similar("lu", &alive));
        assert!(!is_too_similar("lunaa", &alive));
        assert!(!is_too_similar("l", &alive));
    }

    #[test]
    fn test_bind_session_idempotent_same_session() {
        // Calling bind_session_to_process twice with same session+process should be idempotent
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        db.save_instance_named("luna", &data).unwrap();
        db.set_process_binding("pid-1", "", "luna").unwrap();

        // First bind
        let r1 = bind_session_to_process(&db, "sess-1", Some("pid-1"));
        assert_eq!(r1, Some("luna".to_string()));

        // Second bind with same session — should still resolve to luna (now via canonical path)
        let r2 = bind_session_to_process(&db, "sess-1", Some("pid-1"));
        assert_eq!(r2, Some("luna".to_string()));

        // Instance still intact
        let inst = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sess-1"));

        cleanup(path);
    }

    #[test]
    fn test_auto_subscribe_creates_collision_subscription() {
        let (db, path) = setup_test_db();

        // Directly test create_filter_subscription (the core of auto_subscribe_defaults)
        use std::collections::HashMap;
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("collision".to_string(), vec!["1".to_string()]);

        let result = crate::commands::events::create_filter_subscription(
            &db,
            &filters,
            &[],
            "test-agent",
            false,
            true,
        );
        assert_eq!(result, 0, "subscription creation should succeed");

        // Verify subscription was stored in kv
        let rows: Vec<String> = db
            .conn()
            .prepare("SELECT key FROM kv WHERE key LIKE 'events_sub:%'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(rows.len(), 1, "should have 1 subscription");

        cleanup(path);
    }

    #[test]
    fn test_auto_subscribe_silent_no_stdout() {
        let (db, path) = setup_test_db();

        use std::collections::HashMap;
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("action".to_string(), vec!["created".to_string()]);

        // Silent mode should not panic or produce errors
        let result = crate::commands::events::create_filter_subscription(
            &db,
            &filters,
            &[],
            "test-agent",
            false,
            true,
        );
        assert_eq!(result, 0);

        cleanup(path);
    }

    #[test]
    fn test_auto_subscribe_duplicate_is_noop() {
        let (db, path) = setup_test_db();

        use std::collections::HashMap;
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("collision".to_string(), vec!["1".to_string()]);

        // First call creates
        let r1 = crate::commands::events::create_filter_subscription(
            &db,
            &filters,
            &[],
            "test-agent",
            false,
            true,
        );
        assert_eq!(r1, 0);

        // Second call with same filters is a no-op (duplicate)
        let r2 = crate::commands::events::create_filter_subscription(
            &db,
            &filters,
            &[],
            "test-agent",
            false,
            true,
        );
        assert_eq!(r2, 0);

        // Still only 1 subscription
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM kv WHERE key LIKE 'events_sub:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "duplicate should not create second subscription");

        cleanup(path);
    }

    #[test]
    fn test_auto_subscribe_skips_non_tool() {
        // auto_subscribe_defaults guards on tool type — non-tools should be skipped
        let (db, path) = setup_test_db();

        auto_subscribe_defaults(&db, "test-agent", "unknown_tool");

        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM kv WHERE key LIKE 'events_sub:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "non-tool should not create subscriptions");

        cleanup(path);
    }

    #[test]
    fn test_auto_subscribe_name_reuse_cleans_thread_memberships() {
        let (db, path) = setup_test_db();

        let normal = serde_json::json!({
            "id": "sub-normal",
            "caller": "test-agent",
            "sql": "type = 'message'",
            "last_id": 0
        });
        let thread_member = serde_json::json!({
            "id": "sub-thread",
            "caller": "test-agent",
            "thread_name": "debate-1",
            "auto_thread_member": true,
            "delivery_only": true,
            "created": 1000.0,
            "last_id": 0,
            "once": false
        });
        db.kv_set("events_sub:sub-normal", Some(&normal.to_string()))
            .unwrap();
        db.kv_set("events_sub:sub-thread", Some(&thread_member.to_string()))
            .unwrap();

        auto_subscribe_defaults(&db, "test-agent", "codex");

        assert!(db.kv_get("events_sub:sub-normal").unwrap().is_none());
        assert!(db.kv_get("events_sub:sub-thread").unwrap().is_none());

        cleanup(path);
    }
}
