//! Instance table accessors and row conversion.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::HcomDb;
use crate::shared::constants::ST_LISTENING;
use crate::shared::time::now_epoch_i64;

/// Instance status info
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceStatus {
    pub status: String,
    pub detail: String,
    pub last_event_id: i64,
}

/// Full instance row from the instances table.
#[derive(Debug, Clone)]
pub struct InstanceRow {
    pub name: String,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub parent_name: Option<String>,
    pub agent_id: Option<String>,
    pub tag: Option<String>,
    pub last_event_id: i64,
    pub last_stop: i64,
    pub status: String,
    pub status_time: i64,
    pub status_context: String,
    pub status_detail: String,
    pub directory: String,
    pub created_at: f64,
    pub transcript_path: String,
    pub tool: String,
    pub background: i64,
    pub background_log_file: String,
    pub tcp_mode: i64,
    pub wait_timeout: Option<i64>,
    pub subagent_timeout: Option<i64>,
    pub hints: Option<String>,
    pub origin_device_id: Option<String>,
    pub pid: Option<i64>,
    pub launch_args: Option<String>,
    pub terminal_preset_requested: Option<String>,
    pub terminal_preset_effective: Option<String>,
    pub launch_context: Option<String>,
    pub name_announced: i64,
    pub running_tasks: Option<String>,
    pub idle_since: Option<String>,
}

impl InstanceRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            name: row.get("name")?,
            session_id: row
                .get::<_, Option<String>>("session_id")?
                .filter(|s| !s.is_empty()),
            parent_session_id: row
                .get::<_, Option<String>>("parent_session_id")?
                .filter(|s| !s.is_empty()),
            parent_name: row
                .get::<_, Option<String>>("parent_name")?
                .filter(|s| !s.is_empty()),
            agent_id: row
                .get::<_, Option<String>>("agent_id")?
                .filter(|s| !s.is_empty()),
            tag: row
                .get::<_, Option<String>>("tag")?
                .filter(|s| !s.is_empty()),
            last_event_id: row.get::<_, Option<i64>>("last_event_id")?.unwrap_or(0),
            last_stop: row.get::<_, Option<i64>>("last_stop")?.unwrap_or(0),
            status: row
                .get::<_, Option<String>>("status")?
                .unwrap_or_else(|| "inactive".into()),
            status_time: row.get::<_, Option<i64>>("status_time")?.unwrap_or(0),
            status_context: row
                .get::<_, Option<String>>("status_context")?
                .unwrap_or_default(),
            status_detail: row
                .get::<_, Option<String>>("status_detail")?
                .unwrap_or_default(),
            directory: row
                .get::<_, Option<String>>("directory")?
                .unwrap_or_default(),
            created_at: row.get::<_, Option<f64>>("created_at")?.unwrap_or(0.0),
            transcript_path: row
                .get::<_, Option<String>>("transcript_path")?
                .unwrap_or_default(),
            tool: row
                .get::<_, Option<String>>("tool")?
                .unwrap_or_else(|| "claude".into()),
            background: row.get::<_, Option<i64>>("background")?.unwrap_or(0),
            background_log_file: row
                .get::<_, Option<String>>("background_log_file")?
                .unwrap_or_default(),
            tcp_mode: row.get::<_, Option<i64>>("tcp_mode")?.unwrap_or(0),
            wait_timeout: row.get::<_, Option<i64>>("wait_timeout")?,
            subagent_timeout: row.get::<_, Option<i64>>("subagent_timeout")?,
            hints: row
                .get::<_, Option<String>>("hints")?
                .filter(|s| !s.is_empty()),
            origin_device_id: row
                .get::<_, Option<String>>("origin_device_id")?
                .filter(|s| !s.is_empty()),
            pid: row.get::<_, Option<i64>>("pid")?,
            launch_args: row
                .get::<_, Option<String>>("launch_args")?
                .filter(|s| !s.is_empty()),
            terminal_preset_requested: row
                .get::<_, Option<String>>("terminal_preset_requested")?
                .filter(|s| !s.is_empty()),
            terminal_preset_effective: row
                .get::<_, Option<String>>("terminal_preset_effective")?
                .filter(|s| !s.is_empty()),
            launch_context: row
                .get::<_, Option<String>>("launch_context")?
                .filter(|s| !s.is_empty()),
            name_announced: row.get::<_, Option<i64>>("name_announced")?.unwrap_or(0),
            running_tasks: row
                .get::<_, Option<String>>("running_tasks")?
                .filter(|s| !s.is_empty()),
            idle_since: row
                .get::<_, Option<String>>("idle_since")?
                .filter(|s| !s.is_empty()),
        })
    }
}

/// Get instance by name. Returns full row as JSON or None.
/// Column list for instance SELECT queries. Must match instance_row_to_json index order.
pub(super) const INSTANCE_COLUMNS: &str =
    "name, session_id, parent_session_id, parent_name, tag, last_event_id,
     status, status_time, status_context, status_detail, last_stop, directory,
     created_at, transcript_path, tcp_mode, wait_timeout, background,
     background_log_file, name_announced, agent_id, running_tasks,
     origin_device_id, hints, subagent_timeout, tool, launch_args,
     terminal_preset_requested, terminal_preset_effective,
     idle_since, pid, launch_context";

impl HcomDb {
    /// Get instance status by name
    ///
    /// Returns:
    /// - Ok(Some(status)) if instance exists
    /// - Ok(None) if instance not found
    /// - Err if database error occurs
    pub fn get_instance_status(&self, name: &str) -> Result<Option<InstanceStatus>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT name, status, status_detail, last_event_id
             FROM instances WHERE name = ?",
        )?;

        match stmt.query_row(params![name], |row| {
            Ok(InstanceStatus {
                status: row
                    .get::<_, String>(1)
                    .unwrap_or_else(|_| "unknown".to_string()),
                detail: row.get::<_, String>(2).unwrap_or_default(),
                last_event_id: row.get::<_, i64>(3).unwrap_or(0),
            })
        }) {
            Ok(status) => Ok(Some(status)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set instance status in the live `instances` row.
    ///
    /// Side effect: the first call after instance creation (`status_context == "new"`)
    /// emits a `life` event with `action: "ready"` and may trigger batch-completion
    /// notification. For transient TUI-only context, use `set_gate_status()` instead.
    pub fn set_status(&self, name: &str, status: &str, context: &str) -> Result<()> {
        // Check if this is first status update (status_context="new" → ready event)
        let is_new = self
            .get_status(name)?
            .map(|(_, ctx)| ctx == "new")
            .unwrap_or(false);

        let now = now_epoch_i64();

        // Update last_stop heartbeat when entering listening state
        if status == ST_LISTENING {
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
                crate::log::log_error("db", "set_status.emit_ready_event", &format!("{e}"));
            }
        }

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
    /// Merges incoming keys into existing JSON, only filling fields that are
    /// currently missing or empty so late-bound PTY metadata can be persisted
    /// without clobbering richer hook-captured context.
    pub fn store_launch_context(&self, name: &str, context_json: &str) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let existing_json: Option<String> = self
                .conn
                .query_row(
                    "SELECT launch_context FROM instances WHERE name = ?",
                    params![name],
                    |row| row.get(0),
                )
                .optional()?;

            let existing_json = existing_json.unwrap_or_default();
            if existing_json.is_empty() {
                self.conn.execute(
                    "UPDATE instances SET launch_context = ? WHERE name = ?",
                    params![context_json, name],
                )?;
                return Ok(());
            }

            let mut existing = match serde_json::from_str::<
                serde_json::Map<String, serde_json::Value>,
            >(&existing_json)
            {
                Ok(map) => map,
                Err(_) => return Ok(()),
            };
            let incoming = match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                context_json,
            ) {
                Ok(map) => map,
                Err(_) => return Ok(()),
            };

            let mut changed = false;
            for (key, value) in incoming {
                let should_fill = existing.get(&key).is_none_or(|current| {
                    current.is_null() || current.as_str().is_some_and(str::is_empty)
                });
                if should_fill {
                    existing.insert(key, value);
                    changed = true;
                }
            }

            if changed {
                self.conn.execute(
                    "UPDATE instances SET launch_context = ? WHERE name = ?",
                    params![
                        serde_json::to_string(&existing).unwrap_or_else(|_| "{}".to_string()),
                        name
                    ],
                )?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
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
        let mut stmt = self
            .conn
            .prepare_cached("SELECT status, status_context FROM instances WHERE name = ?")?;

        match stmt.query_row(params![name], |row| {
            Ok((
                row.get::<_, String>(0)
                    .unwrap_or_else(|_| "unknown".to_string()),
                row.get::<_, String>(1).unwrap_or_default(),
            ))
        }) {
            Ok(status) => Ok(Some(status)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get transcript path for an instance
    ///
    /// Returns:
    /// - Ok(Some(path)) if instance exists and has non-empty transcript_path
    /// - Ok(None) if instance not found or transcript_path is empty
    /// - Err if database error occurs
    pub fn get_transcript_path(&self, name: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT transcript_path FROM instances WHERE name = ?")?;

        match stmt.query_row(params![name], |row| row.get::<_, String>(0)) {
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
        let mut stmt = self.conn.prepare_cached(
            "SELECT transcript_path, session_id, tool, directory, parent_name, tag,
                    wait_timeout, subagent_timeout, hints, pid, created_at, background,
                    agent_id, launch_args, origin_device_id, background_log_file, last_event_id
             FROM instances WHERE name = ?",
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
                "created_at": row.get::<_, f64>(10).unwrap_or(0.0),
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
        let rows = self
            .conn
            .execute("DELETE FROM instances WHERE name = ?", params![name])?;
        Ok(rows > 0)
    }

    /// Check whether `name`'s *current* identity is a subagent slot.
    ///
    /// Classification rules, in order:
    /// 1. If a live instance row exists, it defines the current identity:
    ///    non-empty `parent_name` → true (subagent); empty → false
    ///    (top-level, regardless of any historical subagent events).
    /// 2. Otherwise, consult the *most recent* `life.stopped` event: true
    ///    iff its snapshot has a non-empty `agent_id`.
    ///
    /// Older subagent history does not poison a name that has since been
    /// reused top-level (either live or via a more recent top-level stop).
    pub fn was_subagent_name(&self, name: &str) -> bool {
        if let Ok(Some(data)) = self.get_instance(name) {
            return data
                .get("parent_name")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty());
        }

        self.conn
            .query_row(
                "SELECT COALESCE(json_extract(data, '$.snapshot.agent_id'), '') != ''
                 FROM events
                 WHERE type = 'life'
                   AND instance = ?
                   AND json_extract(data, '$.action') = 'stopped'
                 ORDER BY id DESC LIMIT 1",
                params![name],
                |row| row.get::<_, bool>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Find the most recent stopped instance whose snapshot carries the given
    /// session_id. life.stopped events are the source of truth: they persist
    /// across the `session_bindings` cascade, so they're the right thing to
    /// consult when reclaiming hcom identity by UUID after stop/kill.
    pub fn find_stopped_instance_by_session_id(&self, session_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT instance FROM events
                 WHERE type = 'life'
                   AND json_extract(data, '$.action') = 'stopped'
                   AND json_extract(data, '$.snapshot.session_id') = ?
                 ORDER BY id DESC LIMIT 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Convert a row from INSTANCE_COLUMNS SELECT to JSON.
    fn instance_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "name": row.get::<_, String>(0).unwrap_or_default(),
            "session_id": row.get::<_, Option<String>>(1).unwrap_or(None),
            "parent_session_id": row.get::<_, Option<String>>(2).unwrap_or(None),
            "parent_name": row.get::<_, Option<String>>(3).unwrap_or(None),
            "tag": row.get::<_, Option<String>>(4).unwrap_or(None),
            "last_event_id": row.get::<_, i64>(5).unwrap_or(0),
            "status": row.get::<_, String>(6).unwrap_or_default(),
            "status_time": row.get::<_, i64>(7).unwrap_or(0),
            "status_context": row.get::<_, String>(8).unwrap_or_default(),
            "status_detail": row.get::<_, String>(9).unwrap_or_default(),
            "last_stop": row.get::<_, i64>(10).unwrap_or(0),
            "directory": row.get::<_, Option<String>>(11).unwrap_or(None),
            "created_at": row.get::<_, f64>(12).unwrap_or(0.0),
            "transcript_path": row.get::<_, String>(13).unwrap_or_default(),
            "tcp_mode": row.get::<_, i64>(14).unwrap_or(0),
            "wait_timeout": row.get::<_, i64>(15).unwrap_or(86400),
            "background": row.get::<_, i64>(16).unwrap_or(0),
            "background_log_file": row.get::<_, String>(17).unwrap_or_default(),
            "name_announced": row.get::<_, i64>(18).unwrap_or(0),
            "agent_id": row.get::<_, Option<String>>(19).unwrap_or(None),
            "running_tasks": row.get::<_, String>(20).unwrap_or_default(),
            "origin_device_id": row.get::<_, String>(21).unwrap_or_default(),
            "hints": row.get::<_, String>(22).unwrap_or_default(),
            "subagent_timeout": row.get::<_, Option<i64>>(23).unwrap_or(None),
            "tool": row.get::<_, String>(24).unwrap_or_default(),
            "launch_args": row.get::<_, String>(25).unwrap_or_default(),
            "terminal_preset_requested": row.get::<_, String>(26).unwrap_or_default(),
            "terminal_preset_effective": row.get::<_, String>(27).unwrap_or_default(),
            "idle_since": row.get::<_, String>(28).unwrap_or_default(),
            "pid": row.get::<_, Option<i64>>(29).unwrap_or(None),
            "launch_context": row.get::<_, String>(30).unwrap_or_default(),
        }))
    }

    pub fn get_instance(&self, name: &str) -> Result<Option<serde_json::Value>> {
        let sql = format!("SELECT {} FROM instances WHERE name = ?", INSTANCE_COLUMNS);
        let mut stmt = self.conn.prepare_cached(&sql)?;

        match stmt.query_row(params![name], Self::instance_row_to_json) {
            Ok(inst) => Ok(Some(inst)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Look up instance name by agent_id (Claude Code sends short IDs like 'a6d9caf').
    pub fn get_instance_by_agent_id(&self, agent_id: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT name FROM instances WHERE agent_id = ?",
            params![agent_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Save (upsert) instance from a map of field names to values.
    pub fn save_instance(
        &self,
        data: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bool> {
        if data.is_empty() {
            return Ok(false);
        }

        let columns: Vec<&str> = data.keys().map(|k| k.as_str()).collect();
        for col in &columns {
            Self::validate_column(col)?;
        }
        let placeholders = vec!["?"; columns.len()].join(", ");
        let update_clause: String = columns
            .iter()
            .filter(|&&k| k != "name")
            .map(|k| format!("{} = excluded.{}", k, k))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO instances ({}) VALUES ({}) ON CONFLICT(name) DO UPDATE SET {}",
            columns.join(", "),
            placeholders,
            update_clause
        );

        let values: Vec<Box<dyn rusqlite::types::ToSql>> = columns
            .iter()
            .map(|&col| -> Box<dyn rusqlite::types::ToSql> {
                let val = &data[col];
                match val {
                    serde_json::Value::String(s) => Box::new(s.clone()),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            Box::new(i)
                        } else if let Some(f) = n.as_f64() {
                            Box::new(f)
                        } else {
                            Box::new(val.to_string())
                        }
                    }
                    serde_json::Value::Bool(b) => Box::new(*b as i32),
                    serde_json::Value::Null => Box::new(rusqlite::types::Null),
                    _ => Box::new(val.to_string()),
                }
            })
            .collect();

        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        self.conn.execute(&sql, refs.as_slice())?;
        Ok(true)
    }

    /// Update specific instance fields.
    pub fn update_instance(
        &self,
        name: &str,
        updates: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bool> {
        if updates.is_empty() {
            return Ok(true);
        }

        let entries: Vec<(&String, &serde_json::Value)> = updates.iter().collect();
        for (k, _) in &entries {
            Self::validate_column(k)?;
        }

        let set_clause: String = entries
            .iter()
            .map(|(k, _)| format!("{} = ?", k))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!("UPDATE instances SET {} WHERE name = ?", set_clause);

        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = entries
            .iter()
            .map(|(_, val)| -> Box<dyn rusqlite::types::ToSql> {
                match val {
                    serde_json::Value::String(s) => Box::new(s.clone()),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            Box::new(i)
                        } else if let Some(f) = n.as_f64() {
                            Box::new(f)
                        } else {
                            Box::new(val.to_string())
                        }
                    }
                    serde_json::Value::Bool(b) => Box::new(*b as i32),
                    serde_json::Value::Null => Box::new(rusqlite::types::Null),
                    _ => Box::new(val.to_string()),
                }
            })
            .collect();
        values.push(Box::new(name.to_string()));

        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        self.conn.execute(&sql, refs.as_slice())?;
        Ok(true)
    }

    /// Iterate all instances, returning Vec of JSON objects.
    pub fn iter_instances(&self) -> Result<Vec<serde_json::Value>> {
        let sql = format!(
            "SELECT {} FROM instances ORDER BY created_at DESC",
            INSTANCE_COLUMNS
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;

        let rows: Vec<serde_json::Value> = stmt
            .query_map([], Self::instance_row_to_json)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    /// Get full instance row by name.
    pub fn get_instance_full(&self, name: &str) -> Result<Option<InstanceRow>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT * FROM instances WHERE name = ?")?;
        match stmt.query_row(params![name], InstanceRow::from_row) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all instance rows.
    pub fn iter_instances_full(&self) -> Result<Vec<InstanceRow>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT * FROM instances ORDER BY created_at DESC")?;
        let rows = stmt
            .query_map([], InstanceRow::from_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Save (INSERT OR REPLACE) an instance row.
    /// Uses a JSON Value map for flexible field specification.
    pub fn save_instance_named(
        &self,
        name: &str,
        data: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<bool> {
        // Build column list and values dynamically
        let mut cols = vec!["name"];
        let mut placeholders = vec!["?"];
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(name.to_string())];

        for (key, val) in data {
            if key == "name" {
                continue;
            }
            cols.push(Self::validate_column(key)?);
            placeholders.push("?");
            values.push(Self::json_value_to_sql(val));
        }

        let sql = format!(
            "INSERT OR REPLACE INTO instances ({}) VALUES ({})",
            cols.join(", "),
            placeholders.join(", ")
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let rows = self.conn.execute(&sql, refs.as_slice())?;
        Ok(rows > 0)
    }

    /// Update specific fields on an instance row.
    /// Uses a JSON Value map for flexible field specification.
    pub fn update_instance_fields(
        &self,
        name: &str,
        updates: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let mut set_parts = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        for (key, val) in updates {
            set_parts.push(format!("{} = ?", Self::validate_column(key)?));
            values.push(Self::json_value_to_sql(val));
        }

        values.push(Box::new(name.to_string()));

        let sql = format!(
            "UPDATE instances SET {} WHERE name = ?",
            set_parts.join(", ")
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        self.conn.execute(&sql, refs.as_slice())?;
        Ok(())
    }

    /// Validate column name against SQL injection (whitelist of known columns).
    fn validate_column(key: &str) -> Result<&str> {
        const VALID_COLUMNS: &[&str] = &[
            "name",
            "session_id",
            "parent_session_id",
            "parent_name",
            "agent_id",
            "tag",
            "last_event_id",
            "last_stop",
            "status",
            "status_time",
            "status_context",
            "status_detail",
            "directory",
            "created_at",
            "transcript_path",
            "tool",
            "background",
            "background_log_file",
            "tcp_mode",
            "wait_timeout",
            "subagent_timeout",
            "hints",
            "origin_device_id",
            "pid",
            "launch_args",
            "launch_context",
            "name_announced",
            "running_tasks",
            "idle_since",
            "terminal_preset_requested",
            "terminal_preset_effective",
        ];
        if VALID_COLUMNS.contains(&key) {
            Ok(key)
        } else {
            Err(anyhow::anyhow!("Invalid column name: {}", key))
        }
    }

    /// Convert a serde_json::Value to a boxed ToSql for dynamic SQL binding.
    fn json_value_to_sql(val: &serde_json::Value) -> Box<dyn rusqlite::types::ToSql> {
        match val {
            serde_json::Value::Null => Box::new(rusqlite::types::Null),
            serde_json::Value::Bool(b) => Box::new(if *b { 1i64 } else { 0i64 }),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Box::new(i)
                } else if let Some(f) = n.as_f64() {
                    Box::new(f)
                } else {
                    Box::new(rusqlite::types::Null)
                }
            }
            serde_json::Value::String(s) => Box::new(s.clone()),
            _ => Box::new(val.to_string()),
        }
    }

    /// Check if instance is idle (safe for PTY injection).
    /// Returns true only when status is "listening" AND detail is not "cmd:listen".
    /// The "cmd:listen" detail is set by `hcom listen` as its first operation,
    /// ensuring the gate blocks before any async setup (endpoint registration, etc.).
    pub fn is_idle(&self, name: &str) -> bool {
        match self.get_instance_status(name) {
            Ok(Some(s)) => s.status == ST_LISTENING && s.detail != "cmd:listen",
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
        let now = now_epoch_i64();

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
}

#[cfg(test)]
mod tests {
    use super::super::HcomDb;
    use super::super::tests::{cleanup_test_db, setup_full_test_db, setup_test_db};
    use rusqlite::params;

    #[test]
    fn test_get_instance_status_propagates_prepare_error() {
        // Verify that SQL errors are propagated as Err (not silently converted to None)
        let (conn, db_path) = setup_test_db();

        // Drop the instances table to cause SQL error
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        // Now HcomDb will fail when trying to query
        let db = HcomDb::open_raw(&db_path).unwrap();

        let result = db.get_instance_status("test");

        // SQL error should be propagated as Err, not None
        let err = result.expect_err("SQL error should propagate as Err, not None");
        assert!(
            err.to_string().contains("instances"),
            "expected missing instances table error, got: {err:#}"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_instance_status_returns_ok_none_when_not_found() {
        // Verify that "not found" is distinguished from "error" via Ok(None)

        let (_conn, db_path) = setup_test_db();
        let db = HcomDb::open_raw(&db_path).unwrap();

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

        let db = HcomDb::open_raw(&db_path).unwrap();
        let result = db.get_status("test");

        let err = result.expect_err("SQL error should propagate as Err");
        assert!(
            err.to_string().contains("instances"),
            "expected missing instances table error, got: {err:#}"
        );
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_transcript_path_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        let result = db.get_transcript_path("test");

        let err = result.expect_err("SQL error should propagate as Err");
        assert!(
            err.to_string().contains("instances"),
            "expected missing instances table error, got: {err:#}"
        );
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_instance_snapshot_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        let result = db.get_instance_snapshot("test");

        let err = result.expect_err("SQL error should propagate as Err");
        assert!(
            err.to_string().contains("instances"),
            "expected missing instances table error, got: {err:#}"
        );
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_store_launch_context_merges_late_pty_metadata() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tool, created_at, launch_context) VALUES (?1, ?2, ?3, ?4)",
                params![
                    "luna",
                    "codex",
                    1.0f64,
                    r#"{"process_id":"proc-1","terminal_preset_effective":"kitty-split","terminal_preset":"kitty-split"}"#
                ],
            )
            .unwrap();

        db.store_launch_context(
            "luna",
            r#"{"process_id":"proc-2","kitty_listen_on":"unix:/tmp/kitty","terminal_id":"11","pane_id":"11"}"#,
        )
        .unwrap();

        let launch_context: String = db
            .conn
            .query_row(
                "SELECT launch_context FROM instances WHERE name = ?",
                params!["luna"],
                |row| row.get(0),
            )
            .unwrap();
        let launch_context: serde_json::Value = serde_json::from_str(&launch_context).unwrap();

        assert_eq!(launch_context["process_id"], "proc-1");
        assert_eq!(launch_context["terminal_preset_effective"], "kitty-split");
        assert_eq!(launch_context["terminal_preset"], "kitty-split");
        assert_eq!(launch_context["kitty_listen_on"], "unix:/tmp/kitty");
        assert_eq!(launch_context["terminal_id"], "11");
        assert_eq!(launch_context["pane_id"], "11");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_save_and_get_instance() {
        let (db, db_path) = setup_full_test_db();

        let mut data = std::collections::HashMap::new();
        data.insert("name".to_string(), serde_json::json!("luna"));
        data.insert("tool".to_string(), serde_json::json!("claude"));
        data.insert("created_at".to_string(), serde_json::json!(1000.0));
        data.insert("status".to_string(), serde_json::json!("active"));

        db.save_instance(&data).unwrap();

        let inst = db.get_instance("luna").unwrap().unwrap();
        assert_eq!(inst["name"], "luna");
        assert_eq!(inst["tool"], "claude");
        assert_eq!(inst["status"], "active");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_update_instance() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, status, created_at) VALUES ('luna', 'active', 1000.0)",
                [],
            )
            .unwrap();

        let mut updates = std::collections::HashMap::new();
        updates.insert("status".to_string(), serde_json::json!("listening"));
        updates.insert("tag".to_string(), serde_json::json!("api"));

        db.update_instance("luna", &updates).unwrap();

        let inst = db.get_instance("luna").unwrap().unwrap();
        assert_eq!(inst["status"], "listening");
        assert_eq!(inst["tag"], "api");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_iter_instances() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 2000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let instances = db.iter_instances().unwrap();
        assert_eq!(instances.len(), 2);
        // Should be ordered by created_at DESC
        assert_eq!(instances[0]["name"], "luna");
        assert_eq!(instances[1]["name"], "nova");

        cleanup_test_db(db_path);
    }
}
