//! Batch launch tracking and wait_for_launch polling.
//!
//! batch is ready, times out, or errors. Used by `hcom events --wait` and
//! the launcher to poll for readiness after `hcom N claude`.

use std::thread;
use std::time::{Duration, Instant};

use crate::db::HcomDb;
use crate::instance_lifecycle;
use rusqlite::params;
use std::collections::HashSet;

/// Result of a launch wait operation.
#[derive(Debug, Clone)]
pub struct LaunchResult {
    pub status: LaunchStatus,
    pub expected: Option<i64>,
    pub ready: Option<i64>,
    pub failed: Option<i64>,
    pub blocked: Option<i64>,
    pub instances: Vec<String>,
    pub failures: Vec<String>,
    pub blockers: Vec<String>,
    pub launcher: Option<String>,
    pub timestamp: Option<String>,
    pub batch_id: Option<String>,
    pub batches: Option<Vec<String>>,
    pub hint: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchStatus {
    Ready,
    Blocked,
    Timeout,
    Error,
    NoLaunches,
}

impl LaunchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LaunchStatus::Ready => "ready",
            LaunchStatus::Blocked => "blocked",
            LaunchStatus::Timeout => "timeout",
            LaunchStatus::Error => "error",
            LaunchStatus::NoLaunches => "no_launches",
        }
    }
}

/// Internal launch status data from DB queries.
struct LaunchData {
    expected: i64,
    ready: i64,
    failed: i64,
    blocked: i64,
    instances: Vec<String>,
    failures: Vec<String>,
    blockers: Vec<String>,
    launcher: String,
    timestamp: String,
    batch_id: Option<String>,
    batches: Option<Vec<String>>,
}

/// Count ready instances for a batch via 'ready' life events.
fn get_ready_for_batch(db: &HcomDb, batch_id: &str) -> (i64, Vec<String>) {
    let conn = db.conn();
    let mut stmt = match conn.prepare(
        "SELECT instance FROM events \
         WHERE type = 'life' \
         AND json_extract(data, '$.action') = 'ready' \
         AND json_extract(data, '$.batch_id') = ?",
    ) {
        Ok(s) => s,
        Err(_) => return (0, vec![]),
    };
    let instances: Vec<String> =
        match stmt.query_map(rusqlite::params![batch_id], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
            Err(_) => vec![],
        };
    let count = instances.len() as i64;
    (count, instances)
}

fn get_failed_for_batch(
    db: &HcomDb,
    batch_id: &str,
    launcher: &str,
    batch_event_id: i64,
    ready_instances: &[String],
) -> (i64, Vec<String>) {
    let mut seen = HashSet::new();
    let mut failures = launch_failed_events_for_batch(db, batch_id, &mut seen);
    let ready: HashSet<&str> = ready_instances.iter().map(String::as_str).collect();

    for name in get_batch_instance_names(db, batch_id) {
        if seen.contains(&name) {
            continue;
        }

        if let Ok(Some(inst)) = db.get_instance_full(&name)
            && let Some(detail) =
                instance_lifecycle::get_or_finalize_launch_failure_detail(db, &inst)
        {
            // Back-fill the batch-scoped life event from this CLI process
            // after row finalization. After this, the failure is visible
            // to event-stream consumers and subsequent polls find it via
            // launch_failed_events_for_batch instead of re-scanning the row.
            emit_row_finalized_event(db, &name, launcher, batch_id, &detail);
            seen.insert(name.clone());
            failures.push(format!("{name}: {detail}"));
            continue;
        }

        if ready.contains(name.as_str()) {
            continue;
        }

        if let Some(detail) = stopped_detail_for_instance(db, &name, batch_event_id) {
            seen.insert(name.clone());
            failures.push(format!("{name}: {detail}"));
        }
    }

    let count = failures.len() as i64;
    (count, failures)
}

/// Emit a batch-scoped `launch_failed` life event from a non-child process
/// (e.g. `wait_for_launch` running in a user CLI). Matches the shape used by
/// `HcomDb::emit_launch_failed_event` (which can't be used here because it
/// reads `HCOM_LAUNCHED_BY`/`HCOM_LAUNCH_BATCH_ID` from env vars that are not
/// set in this context).
fn emit_row_finalized_event(db: &HcomDb, name: &str, launcher: &str, batch_id: &str, detail: &str) {
    let event_data = serde_json::json!({
        "action": "launch_failed",
        "by": launcher,
        "status": "inactive",
        "context": "launch_failed",
        "reason": "row_finalized",
        "detail": detail,
        "batch_id": batch_id,
    });
    let _ = db.log_event("life", name, &event_data);
}

fn get_blocked_for_batch(db: &HcomDb, batch_id: &str) -> (i64, Vec<String>) {
    let conn = db.conn();
    let mut stmt = match conn.prepare(
        "SELECT instance, json_extract(data, '$.detail') FROM events \
         WHERE type = 'life' \
         AND json_extract(data, '$.action') = 'launch_blocked' \
         AND json_extract(data, '$.batch_id') = ?",
    ) {
        Ok(s) => s,
        Err(_) => return (0, vec![]),
    };
    let blockers: Vec<String> = match stmt.query_map(params![batch_id], |row| {
        let name: String = row.get(0)?;
        let detail: Option<String> = row.get(1).ok();
        Ok(format!(
            "{}: {}",
            name,
            detail.unwrap_or_else(|| "launch blocked".to_string())
        ))
    }) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => vec![],
    };
    let count = blockers.len() as i64;
    (count, blockers)
}

fn launch_failed_events_for_batch(
    db: &HcomDb,
    batch_id: &str,
    seen: &mut HashSet<String>,
) -> Vec<String> {
    let conn = db.conn();
    let mut stmt = match conn.prepare(
        "SELECT instance, json_extract(data, '$.reason'), json_extract(data, '$.detail') FROM events \
         WHERE type = 'life' \
         AND json_extract(data, '$.action') = 'launch_failed' \
         AND json_extract(data, '$.batch_id') = ?",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    match stmt.query_map(rusqlite::params![batch_id], |row| {
        let name: String = row.get(0)?;
        let reason: Option<String> = row.get(1).ok();
        let detail: Option<String> = row.get(2).ok();
        let text = detail
            .filter(|s| !s.is_empty())
            .or(reason.filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "launch failed".to_string());
        Ok(format!("{name}: {text}"))
    }) {
        Ok(rows) => rows
            .filter_map(|r| r.ok())
            .filter(|failure| {
                let Some((name, _)) = failure.split_once(": ") else {
                    return true;
                };
                seen.insert(name.to_string())
            })
            .collect(),
        Err(_) => vec![],
    }
}

fn stopped_detail_for_instance(db: &HcomDb, name: &str, batch_event_id: i64) -> Option<String> {
    // Filter by monotonic event id instead of a string timestamp — format-
    // agnostic and not coupled to RFC3339 remaining lexicographically
    // sortable.
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT json_extract(data, '$.by'), json_extract(data, '$.reason') FROM events \
             WHERE type = 'life' \
               AND instance = ?1 \
               AND json_extract(data, '$.action') = 'stopped' \
               AND id >= ?2 \
             ORDER BY id ASC LIMIT 1",
        )
        .ok()?;

    stmt.query_row(params![name, batch_event_id], |row| {
        let by: Option<String> = row.get(0).ok();
        let reason: Option<String> = row.get(1).ok();
        let mut detail = "launch stopped before it remained ready".to_string();
        if let Some(reason) = reason.filter(|s| !s.is_empty()) {
            detail.push_str(": ");
            detail.push_str(&reason);
        }
        if let Some(by) = by.filter(|s| !s.is_empty()) {
            detail.push_str(" by ");
            detail.push_str(&by);
        }
        Ok(detail)
    })
    .ok()
}

fn get_batch_instance_names(db: &HcomDb, batch_id: &str) -> Vec<String> {
    let conn = db.conn();
    let data_str: String = match conn.query_row(
        "SELECT data FROM events
         WHERE type = 'life'
           AND json_extract(data, '$.action') = 'batch_launched'
           AND json_extract(data, '$.batch_id') = ?
         ORDER BY id DESC
         LIMIT 1",
        params![batch_id],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let data: serde_json::Value = match serde_json::from_str(&data_str) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    data.get("instances")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn get_batch_failure_details_for_ids(db: &HcomDb, batch_ids: &[String]) -> Vec<String> {
    let mut details = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for batch_id in batch_ids {
        for name in get_batch_instance_names(db, batch_id) {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Ok(Some(inst)) = db.get_instance_full(&name) else {
                continue;
            };
            if let Some(detail) =
                instance_lifecycle::get_or_finalize_launch_failure_detail(db, &inst)
                    .or_else(|| instance_lifecycle::get_launch_blocker_detail(&inst))
            {
                details.push(format!("{}: {}", name, detail));
            }
        }
    }

    details
}

/// Aggregate multiple batches into a single LaunchData.
fn aggregate_batches(batches: &[BatchInfo], launcher: &str) -> LaunchData {
    let total_expected: i64 = batches.iter().map(|b| b.expected).sum();
    let total_ready: i64 = batches.iter().map(|b| b.ready).sum();
    let total_failed: i64 = batches.iter().map(|b| b.failed).sum();
    let total_blocked: i64 = batches.iter().map(|b| b.blocked).sum();
    let mut all_instances = Vec::new();
    let mut all_failures = Vec::new();
    let mut all_blockers = Vec::new();
    for b in batches {
        all_instances.extend(b.instances.clone());
        all_failures.extend(b.failures.clone());
        all_blockers.extend(b.blockers.clone());
    }
    let batch_ids: Vec<String> = batches.iter().map(|b| b.batch_id.clone()).collect();
    LaunchData {
        expected: total_expected,
        ready: total_ready,
        failed: total_failed,
        blocked: total_blocked,
        instances: all_instances,
        failures: all_failures,
        blockers: all_blockers,
        launcher: launcher.to_string(),
        timestamp: batches
            .first()
            .map(|b| b.timestamp.clone())
            .unwrap_or_default(),
        batch_id: None,
        batches: Some(batch_ids),
    }
}

/// Per-batch info used during aggregation.
struct BatchInfo {
    batch_id: String,
    launcher: String,
    expected: i64,
    ready: i64,
    failed: i64,
    blocked: i64,
    instances: Vec<String>,
    failures: Vec<String>,
    blockers: Vec<String>,
    timestamp: String,
}

/// Launch timeout in seconds — batches older than this are considered stale.
const LAUNCH_TIMEOUT_SECONDS: i64 = 60;

/// Query aggregated launch status across all pending/recent batches.
///
/// Queries batch_launched events, gets ready counts from 'ready' life events,
/// and aggregates.
fn get_launch_status(db: &HcomDb, launcher: Option<&str>) -> Option<LaunchData> {
    let conn = db.conn();

    let (sql, params): (String, Vec<String>) = if let Some(name) = launcher {
        (
            "SELECT id, timestamp, instance as launcher, \
                    json_extract(data, '$.batch_id') as batch_id, \
                    json_extract(data, '$.launched') as expected \
             FROM events \
             WHERE type = 'life' \
               AND json_extract(data, '$.action') = 'batch_launched' \
               AND instance = ?1 \
             ORDER BY id DESC LIMIT 20"
                .to_string(),
            vec![name.to_string()],
        )
    } else {
        (
            "SELECT id, timestamp, instance as launcher, \
                    json_extract(data, '$.batch_id') as batch_id, \
                    json_extract(data, '$.launched') as expected \
             FROM events \
             WHERE type = 'life' \
               AND json_extract(data, '$.action') = 'batch_launched' \
             ORDER BY id DESC LIMIT 20"
                .to_string(),
            vec![],
        )
    };

    let mut stmt = conn.prepare(&sql).ok()?;
    let launches: Vec<(i64, String, String, String, i64)> = if params.is_empty() {
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4).unwrap_or(0),
            ))
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect()
    } else {
        stmt.query_map(rusqlite::params![params[0]], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4).unwrap_or(0),
            ))
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect()
    };

    if launches.is_empty() {
        return None;
    }

    // Build batch info with ready counts
    let mut batches: Vec<BatchInfo> = Vec::new();
    for (batch_event_id, ts, lnchr, batch_id, expected) in &launches {
        let (ready_count, ready_instances) = get_ready_for_batch(db, batch_id);
        let (failed_count, failures) =
            get_failed_for_batch(db, batch_id, lnchr, *batch_event_id, &ready_instances);
        let (blocked_count, blockers) = get_blocked_for_batch(db, batch_id);
        batches.push(BatchInfo {
            batch_id: batch_id.clone(),
            launcher: lnchr.clone(),
            expected: *expected,
            ready: ready_count,
            failed: failed_count,
            blocked: blocked_count,
            instances: ready_instances,
            failures,
            blockers,
            timestamp: ts.clone(),
        });
    }

    let effective_launcher = launcher
        .map(String::from)
        .unwrap_or_else(|| batches[0].launcher.clone());

    // Cutoff for "recent" launches
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - LAUNCH_TIMEOUT_SECONDS;

    // Parse timestamp to epoch for comparison
    let ts_epoch = |ts: &str| -> i64 {
        chrono::DateTime::parse_from_rfc3339(ts)
            .or_else(|_| chrono::DateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.fZ"))
            .map(|dt| dt.timestamp())
            .unwrap_or(0)
    };

    // Priority 1: pending batches (ready < expected) that are recent
    let pending: Vec<&BatchInfo> = batches
        .iter()
        .filter(|b| b.ready + b.failed + b.blocked < b.expected && ts_epoch(&b.timestamp) > cutoff)
        .collect();
    if !pending.is_empty() {
        let owned: Vec<BatchInfo> = pending
            .into_iter()
            .map(|b| BatchInfo {
                batch_id: b.batch_id.clone(),
                launcher: b.launcher.clone(),
                expected: b.expected,
                ready: b.ready,
                failed: b.failed,
                blocked: b.blocked,
                instances: b.instances.clone(),
                failures: b.failures.clone(),
                blockers: b.blockers.clone(),
                timestamp: b.timestamp.clone(),
            })
            .collect();
        return Some(aggregate_batches(&owned, &effective_launcher));
    }

    // Priority 2: batches from last 60s
    let recent: Vec<&BatchInfo> = batches
        .iter()
        .filter(|b| ts_epoch(&b.timestamp) > cutoff)
        .collect();
    if !recent.is_empty() {
        let owned: Vec<BatchInfo> = recent
            .into_iter()
            .map(|b| BatchInfo {
                batch_id: b.batch_id.clone(),
                launcher: b.launcher.clone(),
                expected: b.expected,
                ready: b.ready,
                failed: b.failed,
                blocked: b.blocked,
                instances: b.instances.clone(),
                failures: b.failures.clone(),
                blockers: b.blockers.clone(),
                timestamp: b.timestamp.clone(),
            })
            .collect();
        return Some(aggregate_batches(&owned, &effective_launcher));
    }

    // Priority 3: most recent batch
    let first = &batches[0];
    Some(LaunchData {
        expected: first.expected,
        ready: first.ready,
        failed: first.failed,
        blocked: first.blocked,
        instances: first.instances.clone(),
        failures: first.failures.clone(),
        blockers: first.blockers.clone(),
        launcher: effective_launcher,
        timestamp: first.timestamp.clone(),
        batch_id: Some(first.batch_id.clone()),
        batches: None,
    })
}

/// Query batch status by ID prefix.
///
/// Sums expected across matching
/// batch_launched events, counts ready from 'ready' life events.
fn get_launch_batch(db: &HcomDb, batch_id: &str) -> Option<LaunchData> {
    let conn = db.conn();

    // Get aggregated launch info for this batch_id prefix
    let mut stmt = conn
        .prepare(
            "SELECT MIN(id) as batch_event_id, \
                MIN(timestamp) as timestamp, \
                instance as launcher, \
                json_extract(data, '$.batch_id') as batch_id, \
                SUM(json_extract(data, '$.launched')) as expected \
         FROM events \
         WHERE type = 'life' \
           AND json_extract(data, '$.action') = 'batch_launched' \
           AND json_extract(data, '$.batch_id') LIKE ?1 \
         GROUP BY json_extract(data, '$.batch_id')",
        )
        .ok()?;

    let like_pattern = format!("{}%", batch_id);
    let row: Option<(i64, String, String, String, i64)> = stmt
        .query_row(rusqlite::params![like_pattern], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4).unwrap_or(0),
            ))
        })
        .ok();

    let (batch_event_id, timestamp, launcher, resolved_batch_id, expected) = row?;

    let (ready_count, ready_instances) = get_ready_for_batch(db, &resolved_batch_id);
    let (failed_count, failures) = get_failed_for_batch(
        db,
        &resolved_batch_id,
        &launcher,
        batch_event_id,
        &ready_instances,
    );
    let (blocked_count, blockers) = get_blocked_for_batch(db, &resolved_batch_id);

    Some(LaunchData {
        expected,
        ready: ready_count,
        failed: failed_count,
        blocked: blocked_count,
        instances: ready_instances,
        failures,
        blockers,
        launcher,
        timestamp,
        batch_id: Some(resolved_batch_id),
        batches: None,
    })
}

/// Block until launch batch is ready, times out, or errors.
///
/// Args:
///   db: Database handle.
///   launcher: Instance name of the launcher (for aggregated lookup).
///   batch_id: Specific batch ID (takes priority over launcher).
///   timeout_secs: Max seconds to wait (default 30).
///
/// Returns LaunchResult with status and batch details.
pub fn wait_for_launch(
    db: &HcomDb,
    launcher: Option<&str>,
    batch_id: Option<&str>,
    timeout_secs: u64,
) -> LaunchResult {
    // Clean up stale placeholders before polling.
    // Stale placeholders can block launch detection.
    instance_lifecycle::cleanup_stale_placeholders(db);

    let fetch = |db: &HcomDb| -> Option<LaunchData> {
        if let Some(bid) = batch_id {
            get_launch_batch(db, bid)
        } else {
            get_launch_status(db, launcher)
        }
    };

    let mut status_data = match fetch(db) {
        Some(data) => data,
        None => {
            let msg = if launcher.is_some() {
                "You haven't launched any instances"
            } else {
                "No launches found"
            };
            return LaunchResult {
                status: LaunchStatus::NoLaunches,
                expected: None,
                ready: None,
                failed: None,
                blocked: None,
                instances: vec![],
                failures: vec![],
                blockers: vec![],
                launcher: None,
                timestamp: None,
                batch_id: None,
                batches: None,
                hint: None,
                message: Some(msg.into()),
            };
        }
    };

    // Poll until ready or timeout
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    while status_data.failed == 0
        && status_data.blocked == 0
        && status_data.ready < status_data.expected
        && start.elapsed() < timeout
    {
        thread::sleep(Duration::from_millis(500));

        match fetch(db) {
            Some(data) => status_data = data,
            None => {
                return LaunchResult {
                    status: LaunchStatus::Error,
                    expected: None,
                    ready: None,
                    failed: None,
                    blocked: None,
                    instances: vec![],
                    failures: vec![],
                    blockers: vec![],
                    launcher: None,
                    timestamp: None,
                    batch_id: None,
                    batches: None,
                    hint: None,
                    message: Some("Launch data disappeared (DB reset or pruned)".into()),
                };
            }
        }
    }

    let has_failure = status_data.failed > 0;
    let has_blocked = status_data.blocked > 0;
    let is_timeout =
        status_data.ready + status_data.failed + status_data.blocked < status_data.expected;

    let hint = if has_failure {
        let mut hint = format!(
            "Launch failed: {}/{} ready, {} failed (batch: {}).",
            status_data.ready,
            status_data.expected,
            status_data.failed,
            status_data.batch_id.as_deref().unwrap_or("?")
        );
        if !status_data.failures.is_empty() {
            hint.push_str(" Failed instances: ");
            hint.push_str(&status_data.failures.join("; "));
        }
        Some(hint)
    } else if has_blocked {
        let mut hint = format!(
            "Launch blocked: {}/{} ready, {} blocked (batch: {}).",
            status_data.ready,
            status_data.expected,
            status_data.blocked,
            status_data.batch_id.as_deref().unwrap_or("?")
        );
        if !status_data.blockers.is_empty() {
            hint.push_str(" Blocked instances: ");
            hint.push_str(&status_data.blockers.join("; "));
        }
        Some(hint)
    } else if is_timeout {
        let batch_ids: Vec<String> = if let Some(ref batches) = status_data.batches {
            batches.clone()
        } else if let Some(ref batch_id) = status_data.batch_id {
            vec![batch_id.clone()]
        } else {
            vec![]
        };
        let batch_display = batch_ids.first().map(|s| s.as_str()).unwrap_or("?");
        let mut hint = format!(
            "Launch failed: {}/{} ready after {}s (batch: {}). \
             Check ~/.hcom/.tmp/logs/background_*.log or hcom list -v",
            status_data.ready, status_data.expected, timeout_secs, batch_display
        );
        let failures = get_batch_failure_details_for_ids(db, &batch_ids);
        if !failures.is_empty() {
            hint.push_str(" Failed instances: ");
            hint.push_str(&failures.join("; "));
        }
        Some(hint)
    } else {
        None
    };

    LaunchResult {
        status: if has_failure {
            LaunchStatus::Error
        } else if has_blocked {
            LaunchStatus::Blocked
        } else if is_timeout {
            LaunchStatus::Timeout
        } else {
            LaunchStatus::Ready
        },
        expected: Some(status_data.expected),
        ready: Some(status_data.ready),
        failed: Some(status_data.failed),
        blocked: Some(status_data.blocked),
        instances: status_data.instances,
        failures: status_data.failures,
        blockers: status_data.blockers,
        launcher: Some(status_data.launcher),
        timestamp: Some(status_data.timestamp),
        batch_id: status_data.batch_id,
        batches: status_data.batches,
        hint,
        message: None,
    }
}

/// Serialize LaunchResult to JSON for CLI output.
impl LaunchResult {
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "status".into(),
            serde_json::Value::String(self.status.as_str().into()),
        );

        if let Some(expected) = self.expected {
            obj.insert("expected".into(), serde_json::json!(expected));
        }
        if let Some(ready) = self.ready {
            obj.insert("ready".into(), serde_json::json!(ready));
        }
        if let Some(failed) = self.failed {
            obj.insert("failed".into(), serde_json::json!(failed));
        }
        if let Some(blocked) = self.blocked {
            obj.insert("blocked".into(), serde_json::json!(blocked));
        }
        if !self.instances.is_empty() {
            obj.insert("instances".into(), serde_json::json!(self.instances));
        }
        if !self.failures.is_empty() {
            obj.insert("failures".into(), serde_json::json!(self.failures));
        }
        if !self.blockers.is_empty() {
            obj.insert("blockers".into(), serde_json::json!(self.blockers));
        }
        if let Some(ref launcher) = self.launcher {
            obj.insert("launcher".into(), serde_json::json!(launcher));
        }
        if let Some(ref ts) = self.timestamp {
            obj.insert("timestamp".into(), serde_json::json!(ts));
        }
        if let Some(ref bid) = self.batch_id {
            obj.insert("batch_id".into(), serde_json::json!(bid));
        }
        if let Some(ref batches) = self.batches {
            obj.insert("batches".into(), serde_json::json!(batches));
        }
        if let Some(ref hint) = self.hint {
            obj.insert("hint".into(), serde_json::json!(hint));
        }
        if self.status == LaunchStatus::Timeout {
            obj.insert("timed_out".into(), serde_json::json!(true));
        }
        if let Some(ref msg) = self.message {
            obj.insert("message".into(), serde_json::json!(msg));
        }

        serde_json::Value::Object(obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_test_db() -> (HcomDb, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        (db, dir)
    }

    #[test]
    fn test_launch_status_as_str() {
        assert_eq!(LaunchStatus::Ready.as_str(), "ready");
        assert_eq!(LaunchStatus::Blocked.as_str(), "blocked");
        assert_eq!(LaunchStatus::Timeout.as_str(), "timeout");
        assert_eq!(LaunchStatus::Error.as_str(), "error");
        assert_eq!(LaunchStatus::NoLaunches.as_str(), "no_launches");
    }

    #[test]
    fn test_launch_result_to_json_no_launches() {
        let result = LaunchResult {
            status: LaunchStatus::NoLaunches,
            expected: None,
            ready: None,
            failed: None,
            blocked: None,
            instances: vec![],
            failures: vec![],
            blockers: vec![],
            launcher: None,
            timestamp: None,
            batch_id: None,
            batches: None,
            hint: None,
            message: Some("No launches found".into()),
        };
        let json = result.to_json();
        assert_eq!(json["status"], "no_launches");
        assert_eq!(json["message"], "No launches found");
    }

    #[test]
    fn test_launch_result_to_json_ready() {
        let result = LaunchResult {
            status: LaunchStatus::Ready,
            expected: Some(3),
            ready: Some(3),
            failed: Some(0),
            blocked: Some(0),
            instances: vec!["luna".into(), "nova".into(), "peso".into()],
            failures: vec![],
            blockers: vec![],
            launcher: Some("bigboss".into()),
            timestamp: Some("2024-01-01T00:00:00Z".into()),
            batch_id: Some("batch-123".into()),
            batches: None,
            hint: None,
            message: None,
        };
        let json = result.to_json();
        assert_eq!(json["status"], "ready");
        assert_eq!(json["expected"], 3);
        assert_eq!(json["ready"], 3);
        assert_eq!(json["instances"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_launch_result_to_json_timeout() {
        let result = LaunchResult {
            status: LaunchStatus::Timeout,
            expected: Some(3),
            ready: Some(1),
            failed: Some(0),
            blocked: Some(0),
            instances: vec!["luna".into()],
            failures: vec![],
            blockers: vec![],
            launcher: Some("bigboss".into()),
            timestamp: Some("2024-01-01T00:00:00Z".into()),
            batch_id: Some("batch-123".into()),
            batches: None,
            hint: Some("Launch failed: 1/3 ready after 30s (batch: batch-123). Check ~/.hcom/.tmp/logs/background_*.log or hcom list -v".into()),
            message: None,
        };
        let json = result.to_json();
        assert_eq!(json["status"], "timeout");
        assert_eq!(json["timed_out"], true);
        assert!(json["hint"].as_str().unwrap().contains("Launch failed"));
    }

    #[test]
    fn test_wait_for_launch_returns_error_on_launch_failed_event() {
        let (db, _dir) = make_test_db();

        db.log_event(
            "life",
            "leku",
            &serde_json::json!({
                "action": "batch_launched",
                "batch_id": "batch-fail",
                "launched": 1,
                "instances": ["mari"]
            }),
        )
        .unwrap();
        db.log_event(
            "life",
            "mari",
            &serde_json::json!({
                "action": "launch_failed",
                "batch_id": "batch-fail",
                "reason": "ready_never_observed",
                "detail": "readiness was never observed"
            }),
        )
        .unwrap();

        let result = wait_for_launch(&db, None, Some("batch-fail"), 1);
        assert_eq!(result.status, LaunchStatus::Error);
        assert_eq!(result.ready, Some(0));
        assert_eq!(result.failed, Some(1));
        assert_eq!(
            result.failures,
            vec!["mari: readiness was never observed".to_string()]
        );
        let json = result.to_json();
        assert_eq!(json["status"], "error");
        assert!(json.get("timed_out").is_none());
    }

    #[test]
    fn test_wait_for_launch_counts_status_context_launch_failed() {
        let (db, _dir) = make_test_db();

        let mut data = serde_json::Map::new();
        data.insert("status".into(), serde_json::json!("inactive"));
        data.insert("status_context".into(), serde_json::json!("launch_failed"));
        data.insert(
            "status_detail".into(),
            serde_json::json!("tool startup failed"),
        );
        data.insert(
            "created_at".into(),
            serde_json::json!(crate::shared::time::now_epoch_i64()),
        );
        db.save_instance_named("mari", &data).unwrap();

        db.log_event(
            "life",
            "leku",
            &serde_json::json!({
                "action": "batch_launched",
                "batch_id": "batch-row-fail",
                "launched": 1,
                "instances": ["mari"]
            }),
        )
        .unwrap();

        let result = wait_for_launch(&db, None, Some("batch-row-fail"), 1);
        assert_eq!(result.status, LaunchStatus::Error);
        assert_eq!(result.ready, Some(0));
        assert_eq!(result.failed, Some(1));
        assert_eq!(
            result.failures,
            vec!["mari: tool startup failed".to_string()]
        );
    }

    #[test]
    fn test_wait_for_launch_returns_blocked_on_launch_blocked_event() {
        let (db, _dir) = make_test_db();

        db.log_event(
            "life",
            "leku",
            &serde_json::json!({
                "action": "batch_launched",
                "batch_id": "batch-blocked",
                "launched": 1,
                "instances": ["mari"]
            }),
        )
        .unwrap();
        db.log_event(
            "life",
            "mari",
            &serde_json::json!({
                "action": "launch_blocked",
                "batch_id": "batch-blocked",
                "detail": "launch blocked: run hcom term mari"
            }),
        )
        .unwrap();

        let result = wait_for_launch(&db, None, Some("batch-blocked"), 1);
        assert_eq!(result.status, LaunchStatus::Blocked);
        assert_eq!(result.ready, Some(0));
        assert_eq!(result.failed, Some(0));
        assert_eq!(result.blocked, Some(1));
        assert_eq!(
            result.blockers,
            vec!["mari: launch blocked: run hcom term mari".to_string()]
        );
        let json = result.to_json();
        assert_eq!(json["status"], "blocked");
        assert_eq!(json["blocked"], 1);
    }

    #[test]
    fn test_wait_for_launch_ignores_stopped_after_ready() {
        let (db, _dir) = make_test_db();

        db.log_event(
            "life",
            "leku",
            &serde_json::json!({
                "action": "batch_launched",
                "batch_id": "batch-stopped",
                "launched": 1,
                "instances": ["mari"]
            }),
        )
        .unwrap();
        db.log_event(
            "life",
            "mari",
            &serde_json::json!({
                "action": "ready",
                "batch_id": "batch-stopped",
                "status": "listening",
                "context": "ready_observed"
            }),
        )
        .unwrap();
        db.log_event(
            "life",
            "mari",
            &serde_json::json!({
                "action": "stopped",
                "by": "pty",
                "reason": "closed"
            }),
        )
        .unwrap();

        let result = wait_for_launch(&db, None, Some("batch-stopped"), 1);
        assert_eq!(result.status, LaunchStatus::Ready);
        assert_eq!(result.ready, Some(1));
        assert_eq!(result.failed, Some(0));
        assert!(result.failures.is_empty());
    }

    #[test]
    fn test_wait_for_launch_counts_stopped_before_ready_as_failed() {
        let (db, _dir) = make_test_db();

        db.log_event(
            "life",
            "leku",
            &serde_json::json!({
                "action": "batch_launched",
                "batch_id": "batch-stopped-before-ready",
                "launched": 1,
                "instances": ["mari"]
            }),
        )
        .unwrap();
        db.log_event(
            "life",
            "mari",
            &serde_json::json!({
                "action": "stopped",
                "by": "pty",
                "reason": "closed"
            }),
        )
        .unwrap();

        let result = wait_for_launch(&db, None, Some("batch-stopped-before-ready"), 1);
        assert_eq!(result.status, LaunchStatus::Error);
        assert_eq!(result.ready, Some(0));
        assert_eq!(result.failed, Some(1));
        assert_eq!(
            result.failures,
            vec!["mari: launch stopped before it remained ready: closed by pty".to_string()]
        );
    }

    #[test]
    fn test_wait_for_launch_does_not_finalize_fresh_placeholder() {
        let (db, _dir) = make_test_db();

        let mut data = serde_json::Map::new();
        data.insert("status".into(), serde_json::json!("inactive"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert(
            "created_at".into(),
            serde_json::json!(crate::shared::time::now_epoch_i64()),
        );
        data.insert("status_time".into(), serde_json::json!(0));
        data.insert("tool".into(), serde_json::json!("claude"));
        db.save_instance_named("mari", &data).unwrap();

        db.log_event(
            "life",
            "leku",
            &serde_json::json!({
                "action": "batch_launched",
                "batch_id": "batch-fresh-placeholder",
                "launched": 1,
                "instances": ["mari"]
            }),
        )
        .unwrap();

        let result = wait_for_launch(&db, None, Some("batch-fresh-placeholder"), 1);
        assert_eq!(result.status, LaunchStatus::Timeout);
        assert_eq!(result.ready, Some(0));
        assert_eq!(result.failed, Some(0));
        assert!(result.failures.is_empty());

        let stored = db.get_instance_full("mari").unwrap().unwrap();
        assert_eq!(stored.status_context, "new");
    }

    #[test]
    fn test_get_batch_failure_details_uses_batch_instances_and_status_detail() {
        let (db, _dir) = make_test_db();

        let mut data = serde_json::Map::new();
        data.insert("status".into(), serde_json::json!("inactive"));
        data.insert("status_context".into(), serde_json::json!("launch_failed"));
        data.insert(
            "created_at".into(),
            serde_json::json!(crate::shared::time::now_epoch_i64()),
        );
        data.insert(
            "status_detail".into(),
            serde_json::json!("Error: Operation not permitted (os error 1) Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."),
        );
        db.save_instance_named("mari", &data).unwrap();

        db.log_event(
            "life",
            "leku",
            &serde_json::json!({
                "action": "batch_launched",
                "batch_id": "batch-123",
                "launched": 1,
                "instances": ["mari"]
            }),
        )
        .unwrap();

        let details = get_batch_failure_details_for_ids(&db, &["batch-123".to_string()]);
        assert_eq!(
            details,
            vec!["mari: Error: Operation not permitted (os error 1) Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry.".to_string()]
        );
    }

    #[test]
    fn test_get_batch_failure_details_finalizes_new_instance() {
        let (db, _dir) = make_test_db();

        let mut data = serde_json::Map::new();
        data.insert("status".into(), serde_json::json!("inactive"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert(
            "created_at".into(),
            serde_json::json!(
                crate::shared::time::now_epoch_i64()
                    - crate::instance_lifecycle::LAUNCH_PLACEHOLDER_TIMEOUT
                    - 1
            ),
        );
        data.insert("tool".into(), serde_json::json!("codex"));
        data.insert(
            "launch_context".into(),
            serde_json::json!(r#"{"terminal_preset":"tmux","pane_id":""}"#),
        );
        db.save_instance_named("mari", &data).unwrap();

        db.log_event(
            "life",
            "leku",
            &serde_json::json!({
                "action": "batch_launched",
                "batch_id": "batch-456",
                "launched": 1,
                "instances": ["mari"]
            }),
        )
        .unwrap();

        let details = get_batch_failure_details_for_ids(&db, &["batch-456".to_string()]);
        assert_eq!(
            details,
            vec!["mari: launch probably failed - check logs or hcom list -v".to_string()]
        );

        let stored = db.get_instance_full("mari").unwrap().unwrap();
        assert_eq!(stored.status_context, "launch_failed");
        assert_eq!(
            stored.status_detail,
            "launch probably failed - check logs or hcom list -v"
        );
    }
}
