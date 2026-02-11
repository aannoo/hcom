//! SQLite database access for hcom
//!
//! Minimal read/write access to ~/.hcom/hcom.db for:
//! - Reading unread messages (events table with type='message')
//! - Updating cursor position (instances.last_event_id)
//! - Reading instance status
//! - Registering notify endpoints

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

/// Message from the events table
#[derive(Debug, Clone)]
pub struct Message {
    pub from: String,
    pub intent: Option<String>,
    pub thread: Option<String>,
    pub event_id: Option<i64>,
}

/// Instance status info
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceStatus {
    pub status: String,
    pub detail: String,
    pub last_event_id: i64,
}

/// Database handle for hcom operations
pub struct HcomDb {
    conn: Connection,
}

impl HcomDb {
    /// Open the hcom database at ~/.hcom/hcom.db
    pub fn open() -> Result<Self> {
        let db_path = crate::paths::db_path();
        Self::open_at(&db_path)
    }

    /// Open the hcom database at a specific path (for testing)
    pub fn open_at(db_path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open database: {}", db_path.display()))?;

        // Enable WAL mode for concurrent access + foreign keys for CASCADE
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

        Ok(Self { conn })
    }

    /// Get instance status by name
    ///
    /// Returns:
    /// - Ok(Some(status)) if instance exists
    /// - Ok(None) if instance not found
    /// - Err if database error occurs
    pub fn get_instance_status(&self, name: &str) -> Result<Option<InstanceStatus>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, status, status_detail, last_event_id
             FROM instances WHERE name = ?"
        )?;

        match stmt.query_row(params![name], |row| {
            Ok(InstanceStatus {
                status: row.get::<_, String>(1).unwrap_or_else(|_| "unknown".to_string()),
                detail: row.get::<_, String>(2).unwrap_or_default(),
                last_event_id: row.get::<_, i64>(3).unwrap_or(0),
            })
        }) {
            Ok(status) => Ok(Some(status)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get unread messages for an instance
    ///
    /// Returns messages where:
    /// - event.id > instance.last_event_id
    /// - event.type = 'message'
    /// - instance is in scope (broadcast or direct)
    pub fn get_unread_messages(&self, name: &str) -> Vec<Message> {
        // Get last_event_id for this instance
        let last_event_id = match self.get_instance_status(name) {
            Ok(Some(status)) => status.last_event_id,
            Ok(None) => 0, // No instance found
            Err(e) => {
                eprintln!("[hcom] DB error in get_unread_messages (get_instance_status): {}", e);
                0 // Fallback to 0
            }
        };

        let mut stmt = match self.conn.prepare(
            "SELECT id, timestamp, data FROM events
             WHERE id > ? AND type = 'message'
             ORDER BY id"
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        let rows = match stmt.query_map(params![last_event_id], |row| {
            let id: i64 = row.get(0)?;
            let timestamp: String = row.get(1)?;
            let data: String = row.get(2)?;
            Ok((id, timestamp, data))
        }) {
            Ok(r) => r,
            Err(_) => return vec![],
        };

        let mut messages = Vec::new();
        for (id, _timestamp, data) in rows.flatten() {
                // Parse JSON data
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) {
                    let from = json.get("from")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();

                    // Skip own messages
                    if from == name {
                        continue;
                    }

                    // Check scope - "broadcast" delivers to all, "mentions" checks mentions array
                    let scope = json.get("scope")
                        .and_then(|s| s.as_str())
                        .unwrap_or("broadcast");

                    let should_deliver = match scope {
                        "broadcast" => true,
                        "mentions" => {
                            // Check if receiver is in mentions array
                            let mentions: Vec<String> = json.get("mentions")
                                .and_then(|m| m.as_array())
                                .map(|arr| arr.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect())
                                .unwrap_or_default();
                            mentions.contains(&name.to_string())
                        }
                        _ => false, // Unknown scope
                    };

                    if !should_deliver {
                        continue;
                    }

                    let intent = json.get("intent")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let thread = json.get("thread")
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    messages.push(Message {
                        from,
                        intent,
                        thread,
                        event_id: Some(id),
                    });
                }
        }

        messages
    }

    /// Register notify endpoint for PTY wake-ups
    ///
    /// Inserts or updates notify_endpoints table with (instance, kind='pty', port)
    pub fn register_notify_port(&self, name: &str, port: u16) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        self.conn.execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
             VALUES (?, 'pty', ?, ?)
             ON CONFLICT(instance, kind) DO UPDATE SET
                 port = excluded.port,
                 updated_at = excluded.updated_at",
            params![name, port as i64, now],
        )?;

        Ok(())
    }

    /// Register inject port for screen queries
    pub fn register_inject_port(&self, name: &str, port: u16) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        self.conn.execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
             VALUES (?, 'inject', ?, ?)
             ON CONFLICT(instance, kind) DO UPDATE SET
                 port = excluded.port,
                 updated_at = excluded.updated_at",
            params![name, port as i64, now],
        )?;

        Ok(())
    }

    /// Check if instance status is "listening" (idle)
    /// Check if instance is idle (safe for PTY injection).
    ///
    /// Returns true only when status is "listening" AND detail is not "cmd:listen".
    /// The "cmd:listen" detail is set by `hcom listen` as its first operation,
    /// ensuring the gate blocks before any async setup (endpoint registration, etc.).
    pub fn is_idle(&self, name: &str) -> bool {
        match self.get_instance_status(name) {
            Ok(Some(s)) => s.status == "listening" && s.detail != "cmd:listen",
            _ => false,
        }
    }

    /// Update heartbeat timestamp and re-assert tcp_mode to prove instance is alive.
    ///
    /// Sets both last_stop (heartbeat) and tcp_mode=true atomically.
    /// Re-asserting tcp_mode on every heartbeat self-heals after DB resets,
    /// instance re-creation, or any state loss — the delivery thread is the
    /// source of truth for whether TCP delivery is active.
    pub fn update_heartbeat(&self, name: &str) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        self.conn.execute(
            "UPDATE instances SET last_stop = ?, tcp_mode = 1 WHERE name = ?",
            params![now, name],
        )?;
        Ok(())
    }

    /// Update instance position with tcp_mode flag
    pub fn update_tcp_mode(&self, name: &str, tcp_mode: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE instances SET tcp_mode = ? WHERE name = ?",
            params![tcp_mode as i32, name],
        )?;
        Ok(())
    }

    /// Set instance status (for cleanup)
    pub fn set_status(&self, name: &str, status: &str, context: &str) -> Result<()> {
        // Check if this is first status update (status_context="new" → ready event)
        let is_new = self.get_status(name)?
            .map(|(_, ctx)| ctx == "new")
            .unwrap_or(false);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Update last_stop heartbeat when entering listening state (matches Python set_status)
        if status == "listening" {
            self.conn.execute(
                "UPDATE instances SET status = ?, status_context = ?, status_time = ?, last_stop = ? WHERE name = ?",
                params![status, context, now, now, name],
            )?;
        } else {
            self.conn.execute(
                "UPDATE instances SET status = ?, status_context = ?, status_time = ? WHERE name = ?",
                params![status, context, now, name],
            )?;
        }

        // Emit ready event and batch notification on first status update
        if is_new {
            if let Err(e) = self.emit_ready_event(name, status, context) {
                eprintln!("[hcom] Failed to emit ready event: {}", e);
            }
        }

        Ok(())
    }

    /// Emit "ready" life event and check for batch completion notification.
    ///
    /// Called on first status update (when status_context was "new").
    /// Mirrors the Python logic in instances.py set_status().
    fn emit_ready_event(&self, name: &str, status: &str, context: &str) -> Result<()> {
        let launcher = std::env::var("HCOM_LAUNCHED_BY").unwrap_or_else(|_| "unknown".to_string());
        let batch_id = std::env::var("HCOM_LAUNCH_BATCH_ID").ok();

        let mut event_data = serde_json::json!({
            "action": "ready",
            "by": launcher,
            "status": status,
            "context": context,
        });
        if let Some(ref bid) = batch_id {
            event_data["batch_id"] = serde_json::Value::String(bid.clone());
        }

        let ts = chrono_now_iso();
        self.conn.execute(
            "INSERT INTO events (timestamp, type, instance, data) VALUES (?, 'life', ?, ?)",
            params![ts, name, event_data.to_string()],
        )?;

        // Check batch completion and send launcher notification
        if launcher != "unknown" {
            if let Some(ref bid) = batch_id {
                self.check_batch_completion(&launcher, bid)?;
            }
        }

        Ok(())
    }

    /// Check if all instances in a launch batch are ready; send notification if so.
    fn check_batch_completion(&self, launcher: &str, batch_id: &str) -> Result<()> {
        // Find the launch event for this batch
        let launch_data: Option<String> = self.conn.query_row(
            "SELECT data FROM events
             WHERE type = 'life' AND instance = ?
               AND json_extract(data, '$.action') = 'batch_launched'
               AND json_extract(data, '$.batch_id') = ?
             LIMIT 1",
            params![launcher, batch_id],
            |row| row.get(0),
        ).ok();

        let Some(data_str) = launch_data else { return Ok(()) };
        let data: serde_json::Value = serde_json::from_str(&data_str)?;
        let expected = data.get("launched").and_then(|v| v.as_u64()).unwrap_or(0);
        if expected == 0 { return Ok(()) }

        // Count ready events with matching batch_id
        let ready_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE type = 'life'
               AND json_extract(data, '$.action') = 'ready'
               AND json_extract(data, '$.batch_id') = ?",
            params![batch_id],
            |row| row.get(0),
        )?;

        if (ready_count as u64) < expected { return Ok(()) }

        // Check idempotency — don't send duplicate notification
        let already_sent: bool = self.conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE type = 'message'
               AND instance = 'sys_[hcom-launcher]'
               AND json_extract(data, '$.text') LIKE ?
             LIMIT 1",
            params![format!("%batch: {}%", batch_id)],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )?;

        if already_sent { return Ok(()) }

        // Get instance names from this batch
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT instance FROM events
             WHERE type = 'life'
               AND json_extract(data, '$.action') = 'ready'
               AND json_extract(data, '$.batch_id') = ?",
        )?;
        let names: Vec<String> = stmt.query_map(params![batch_id], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let instances_list = names.join(", ");
        let text = format!(
            "@{} All {} instances ready: {} (batch: {})",
            launcher, expected, instances_list, batch_id
        );

        // Insert system message
        let ts = chrono_now_iso();
        let msg_data = serde_json::json!({
            "from": "[hcom-launcher]",
            "text": text,
            "scope": "mentions",
            "mentions": [launcher],
            "system": true,
        });
        self.conn.execute(
            "INSERT INTO events (timestamp, type, instance, data) VALUES (?, 'message', 'sys_[hcom-launcher]', ?)",
            params![ts, msg_data.to_string()],
        )?;

        Ok(())
    }

    /// Update gate blocking status WITHOUT logging a status event.
    ///
    /// Used for transient PTY gate states (tui:*) that shouldn't pollute the events table.
    /// Only updates the instance row; TUI reads this for display but no event is created.
    ///
    /// Args:
    ///   context: Gate context like "tui:not-ready", "tui:user-active", etc.
    ///   detail: Human-readable description like "user is typing"
    /// Preserve status_detail when it's "cmd:listen" — gate diagnostics must not
    /// overwrite the flag that blocks PTY injection during `hcom listen`.
    pub fn set_gate_status(&self, name: &str, context: &str, detail: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE instances SET status_context = ?,
                status_detail = CASE WHEN status_detail = 'cmd:listen' THEN status_detail ELSE ? END
             WHERE name = ?",
            params![context, detail, name],
        )?;
        Ok(())
    }

    /// Update instance PID after spawn
    pub fn update_instance_pid(&self, name: &str, pid: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE instances SET pid = ? WHERE name = ?",
            params![pid as i64, name],
        )?;
        Ok(())
    }

    /// Store launch_context JSON (terminal preset, pane_id, env snapshot).
    /// Only writes if launch_context is currently empty (don't overwrite hook-captured context).
    pub fn store_launch_context(&self, name: &str, context_json: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE instances SET launch_context = ? WHERE name = ? AND (launch_context IS NULL OR launch_context = '')",
            params![context_json, name],
        )?;
        Ok(())
    }

    /// Get instance tag (for display name computation).
    /// Returns Some(tag) if tag is non-empty, None otherwise.
    pub fn get_instance_tag(&self, name: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT tag FROM instances WHERE name = ?",
                params![name],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .filter(|t| !t.is_empty())
    }

    /// Get current status and context for gate blocking logic
    ///
    /// Returns:
    /// - Ok(Some((status, context))) if instance exists
    /// - Ok(None) if instance not found
    /// - Err if database error occurs
    pub fn get_status(&self, name: &str) -> Result<Option<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT status, status_context FROM instances WHERE name = ?"
        )?;

        match stmt.query_row(params![name], |row| {
            Ok((
                row.get::<_, String>(0).unwrap_or_else(|_| "unknown".to_string()),
                row.get::<_, String>(1).unwrap_or_default(),
            ))
        }) {
            Ok(status) => Ok(Some(status)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete process binding (for cleanup)
    pub fn delete_process_binding(&self, process_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM process_bindings WHERE process_id = ?",
            params![process_id],
        )?;
        Ok(())
    }

    /// Get process binding to check for name changes
    ///
    /// Returns:
    /// - Ok(Some(instance_name)) if binding exists
    /// - Ok(None) if binding not found
    /// - Err if database error occurs
    pub fn get_process_binding(&self, process_id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT instance_name FROM process_bindings WHERE process_id = ?"
        )?;

        match stmt.query_row(params![process_id], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Migrate notify endpoints from old instance to new instance
    pub fn migrate_notify_endpoints(&self, old_name: &str, new_name: &str) -> Result<()> {
        if old_name == new_name {
            return Ok(());
        }

        // Delete existing endpoints for new name
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ?",
            params![new_name],
        )?;

        // Move endpoints from old to new
        self.conn.execute(
            "UPDATE notify_endpoints SET instance = ? WHERE instance = ?",
            params![new_name, old_name],
        )?;

        Ok(())
    }

    /// Get last_event_id for an instance (cursor position for message delivery).
    ///
    /// Returns 0 if instance not found or on error.
    pub fn get_cursor(&self, name: &str) -> i64 {
        match self.get_instance_status(name) {
            Ok(Some(status)) => status.last_event_id,
            Ok(None) => 0, // No instance found
            Err(e) => {
                eprintln!("[hcom] DB error in get_cursor (get_instance_status): {}", e);
                0 // Fallback to 0
            }
        }
    }

    /// Check if there are pending (unread) messages for an instance.
    ///
    /// Returns true if any messages exist with id > instance.last_event_id.
    pub fn has_pending(&self, name: &str) -> bool {
        !self.get_unread_messages(name).is_empty()
    }

    /// Get transcript path for an instance
    ///
    /// Returns:
    /// - Ok(Some(path)) if instance exists and has non-empty transcript_path
    /// - Ok(None) if instance not found or transcript_path is empty
    /// - Err if database error occurs
    pub fn get_transcript_path(&self, name: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT transcript_path FROM instances WHERE name = ?"
        )?;

        match stmt.query_row(params![name], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(path) if !path.is_empty() => Ok(Some(path)),
            Ok(_) => Ok(None), // Empty path
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get instance snapshot for life event logging before deletion
    ///
    /// Returns:
    /// - Ok(Some(snapshot)) if instance exists
    /// - Ok(None) if instance not found
    /// - Err if database error occurs
    pub fn get_instance_snapshot(&self, name: &str) -> Result<Option<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT transcript_path, session_id, tool, directory, parent_name, tag,
                    wait_timeout, subagent_timeout, hints, pid, created_at, background,
                    agent_id, launch_args, origin_device_id, background_log_file, last_event_id
             FROM instances WHERE name = ?"
        )?;

        match stmt.query_row(params![name], |row| {
            Ok(serde_json::json!({
                "transcript_path": row.get::<_, String>(0).unwrap_or_default(),
                "session_id": row.get::<_, String>(1).unwrap_or_default(),
                "tool": row.get::<_, String>(2).unwrap_or_default(),
                "directory": row.get::<_, String>(3).unwrap_or_default(),
                "parent_name": row.get::<_, String>(4).unwrap_or_default(),
                "tag": row.get::<_, String>(5).unwrap_or_default(),
                "wait_timeout": row.get::<_, Option<i64>>(6).unwrap_or(None),
                "subagent_timeout": row.get::<_, Option<i64>>(7).unwrap_or(None),
                "hints": row.get::<_, String>(8).unwrap_or_default(),
                "pid": row.get::<_, Option<i64>>(9).unwrap_or(None),
                "created_at": row.get::<_, String>(10).unwrap_or_default(),
                "background": row.get::<_, i64>(11).unwrap_or(0),
                "agent_id": row.get::<_, String>(12).unwrap_or_default(),
                "launch_args": row.get::<_, String>(13).unwrap_or_default(),
                "origin_device_id": row.get::<_, String>(14).unwrap_or_default(),
                "background_log_file": row.get::<_, String>(15).unwrap_or_default(),
                "last_event_id": row.get::<_, i64>(16).unwrap_or(0),
            }))
        }) {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete instance row from database
    pub fn delete_instance(&self, name: &str) -> Result<bool> {
        let rows = self.conn.execute(
            "DELETE FROM instances WHERE name = ?",
            params![name],
        )?;
        Ok(rows > 0)
    }

    /// Log a life event (started/stopped) to the events table
    pub fn log_life_event(
        &self,
        instance: &str,
        action: &str,
        by: &str,
        reason: &str,
        snapshot: Option<serde_json::Value>,
    ) -> Result<()> {
        let data = match snapshot {
            Some(s) => serde_json::json!({
                "action": action,
                "by": by,
                "reason": reason,
                "snapshot": s
            }),
            None => serde_json::json!({
                "action": action,
                "by": by,
                "reason": reason
            }),
        };

        let ts = chrono_now_iso();

        self.conn.execute(
            "INSERT INTO events (timestamp, type, instance, data)
             VALUES (?, 'life', ?, ?)",
            params![ts, instance, data.to_string()],
        )?;

        Ok(())
    }

    /// Delete notify endpoints for an instance
    pub fn delete_notify_endpoints(&self, name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ?",
            params![name],
        )?;
        Ok(())
    }

    /// Log a status event to the events table
    ///
    /// Used by TranscriptWatcher to log tool:apply_patch, tool:shell, and prompt events.
    pub fn log_status_event(
        &self,
        instance: &str,
        status: &str,
        context: &str,
        detail: Option<&str>,
        timestamp: Option<&str>,
    ) -> Result<()> {
        // Build data JSON
        let data = match detail {
            Some(d) => serde_json::json!({
                "status": status,
                "context": context,
                "detail": d
            }),
            None => serde_json::json!({
                "status": status,
                "context": context
            }),
        };

        // Use provided timestamp or generate current
        let ts = match timestamp {
            Some(t) => t.to_string(),
            None => chrono_now_iso(),
        };

        self.conn.execute(
            "INSERT INTO events (timestamp, type, instance, data)
             VALUES (?, 'status', ?, ?)",
            params![ts, instance, data.to_string()],
        )?;

        Ok(())
    }

    /// Update instance status if timestamp is newer than current
    ///
    /// Used by TranscriptWatcher to update instance cache with retroactive events.
    pub fn update_status_if_newer(
        &self,
        name: &str,
        status: &str,
        context: &str,
        detail: Option<&str>,
        timestamp: &str,
    ) -> Result<()> {
        // Parse timestamp to epoch seconds
        let event_time = parse_iso_timestamp(timestamp).unwrap_or(0);

        // Get current status_time
        let current_time: i64 = self.conn.query_row(
            "SELECT status_time FROM instances WHERE name = ?",
            params![name],
            |row| row.get(0),
        ).unwrap_or(0);

        // Only update if event is newer
        if event_time >= current_time {
            match detail {
                Some(d) => {
                    self.conn.execute(
                        "UPDATE instances SET status = ?, status_context = ?, status_detail = ?, status_time = ? WHERE name = ?",
                        params![status, context, d, event_time, name],
                    )?;
                }
                None => {
                    self.conn.execute(
                        "UPDATE instances SET status = ?, status_context = ?, status_time = ? WHERE name = ?",
                        params![status, context, event_time, name],
                    )?;
                }
            }
        }

        Ok(())
    }
}

/// Generate ISO timestamp for current time using chrono
fn chrono_now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
}

/// Parse ISO 8601 timestamp to epoch seconds using chrono
fn parse_iso_timestamp(ts: &str) -> Option<i64> {
    // Try parsing with timezone offset (e.g., 2026-01-25T00:11:38.208360+00:00)
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return Some(dt.timestamp());
    }
    // Try parsing with just 'Z' suffix
    if let Ok(dt) = ts.parse::<DateTime<Utc>>() {
        return Some(dt.timestamp());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;

    /// Create a test database with instances table
    fn setup_test_db() -> (Connection, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!("test_hcom_{}_{}.db", std::process::id(), test_id));

        let conn = Connection::open(&db_path).unwrap();

        // Create minimal schema
        conn.execute_batch(
            "CREATE TABLE instances (
                name TEXT PRIMARY KEY,
                status TEXT,
                status_context TEXT,
                status_detail TEXT,
                last_event_id INTEGER,
                transcript_path TEXT,
                session_id TEXT,
                tool TEXT,
                directory TEXT,
                parent_name TEXT,
                tag TEXT,
                wait_timeout INTEGER,
                subagent_timeout INTEGER,
                hints TEXT,
                pid INTEGER,
                created_at TEXT,
                background INTEGER,
                agent_id TEXT,
                launch_args TEXT,
                origin_device_id TEXT,
                background_log_file TEXT,
                status_time INTEGER
            );

            CREATE TABLE process_bindings (
                process_id TEXT PRIMARY KEY,
                instance_name TEXT
            );"
        ).unwrap();

        (conn, db_path)
    }

    /// Clean up test database
    fn cleanup_test_db(path: PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_get_instance_status_propagates_prepare_error() {
        // Verify that SQL errors are propagated as Err (not silently converted to None)
        let (conn, db_path) = setup_test_db();

        // Drop the instances table to cause SQL error
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        // Now HcomDb will fail when trying to query
        let db = HcomDb::open_at(&db_path).unwrap();

        let result = db.get_instance_status("test");

        // SQL error should be propagated as Err, not None
        assert!(result.is_err(), "SQL error should propagate as Err, not None");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_instance_status_returns_ok_none_when_not_found() {
        // Verify that "not found" is distinguished from "error" via Ok(None)

        let (_conn, db_path) = setup_test_db();
        let db = HcomDb::open_at(&db_path).unwrap();

        // Query non-existent instance
        let result = db.get_instance_status("nonexistent");

        // Should be Ok(None) - not found is not an error
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_status_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        let db = HcomDb::open_at(&db_path).unwrap();
        let result = db.get_status("test");

        assert!(result.is_err(), "SQL error should propagate as Err");
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_process_binding_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE process_bindings", []).unwrap();
        drop(conn);

        let db = HcomDb::open_at(&db_path).unwrap();
        let result = db.get_process_binding("test_pid");

        assert!(result.is_err(), "SQL error should propagate as Err");
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_transcript_path_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        let db = HcomDb::open_at(&db_path).unwrap();
        let result = db.get_transcript_path("test");

        assert!(result.is_err(), "SQL error should propagate as Err");
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_instance_snapshot_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        let db = HcomDb::open_at(&db_path).unwrap();
        let result = db.get_instance_snapshot("test");

        assert!(result.is_err(), "SQL error should propagate as Err");
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_all_methods_return_ok_none_when_not_found() {
        let (_conn, db_path) = setup_test_db();
        let db = HcomDb::open_at(&db_path).unwrap();

        // All these should return Ok(None) for non-existent data
        assert!(db.get_instance_status("nonexistent").unwrap().is_none());
        assert!(db.get_status("nonexistent").unwrap().is_none());
        assert!(db.get_process_binding("nonexistent").unwrap().is_none());
        assert!(db.get_transcript_path("nonexistent").unwrap().is_none());
        assert!(db.get_instance_snapshot("nonexistent").unwrap().is_none());

        cleanup_test_db(db_path);
    }

    fn setup_test_db_with_endpoints() -> (Connection, PathBuf) {
        let (conn, db_path) = setup_test_db();
        conn.execute_batch(
            "CREATE TABLE notify_endpoints (
                instance TEXT NOT NULL,
                kind TEXT NOT NULL,
                port INTEGER NOT NULL,
                updated_at REAL,
                PRIMARY KEY (instance, kind)
            );"
        ).unwrap();
        (conn, db_path)
    }

    #[test]
    fn test_register_inject_port_inserts() {
        let (_conn, db_path) = setup_test_db_with_endpoints();
        let db = HcomDb::open_at(&db_path).unwrap();

        db.register_inject_port("test", 5555).unwrap();

        let port: i64 = db.conn.query_row(
            "SELECT port FROM notify_endpoints WHERE instance = 'test' AND kind = 'inject'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(port, 5555);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_register_inject_port_upserts() {
        let (_conn, db_path) = setup_test_db_with_endpoints();
        let db = HcomDb::open_at(&db_path).unwrap();

        db.register_inject_port("test", 5555).unwrap();
        db.register_inject_port("test", 6666).unwrap();

        let port: i64 = db.conn.query_row(
            "SELECT port FROM notify_endpoints WHERE instance = 'test' AND kind = 'inject'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(port, 6666);

        // Should be exactly one row
        let count: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM notify_endpoints WHERE instance = 'test'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 1);

        cleanup_test_db(db_path);
    }
}
