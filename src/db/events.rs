//! Event append/read methods and message delivery queries.

use anyhow::Result;
use rusqlite::params;

use super::{HcomDb, chrono_now_iso, subscriptions};

/// Message from the events table
#[derive(Debug, Clone)]
pub struct Message {
    pub from: String,
    pub text: String,
    pub intent: Option<String>,
    pub thread: Option<String>,
    pub event_id: Option<i64>,
    pub timestamp: Option<String>,
    pub delivered_to: Option<Vec<String>>,
    pub bundle_id: Option<String>,
    pub relay: bool,
}

impl HcomDb {
    /// Check if a message event should be delivered to the given receiver.
    ///
    /// Skips own messages. Checks scope: "broadcast" delivers to all,
    /// "mentions" checks the mentions array with cross-device base-name matching.
    ///
    /// `receiver` may be local (`luna`) or relay-namespaced (`luna:ABCD`).
    /// Mentions compare on base name so the same event JSON routes correctly
    /// on both local and relayed peers without rewriting stored scope.
    pub(super) fn should_deliver_to(json: &serde_json::Value, receiver: &str) -> bool {
        let from = json.get("from").and_then(|v| v.as_str()).unwrap_or("");
        if from == receiver {
            return false;
        }
        if let Some(delivered) = json.get("delivered_to").and_then(|v| v.as_array()) {
            return delivered.iter().any(|v| v.as_str() == Some(receiver));
        }
        let scope = json
            .get("scope")
            .and_then(|s| s.as_str())
            .unwrap_or("broadcast");
        match scope {
            "broadcast" => true,
            "mentions" => {
                let receiver_base = receiver.split(':').next().unwrap_or(receiver);
                json.get("mentions")
                    .and_then(|m| m.as_array())
                    .map(|arr| {
                        arr.iter().any(|v| {
                            v.as_str()
                                .map(|m| receiver_base == m.split(':').next().unwrap_or(m))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            }
            _ => false,
        }
    }

    /// Returns true iff there is at least one unread message that names this
    /// instance directly (`scope='mentions'` and the recipient is in the
    /// `mentions` array). Broadcasts are ignored.
    ///
    /// Used to gate dormant subagent activation: a SubagentStart-allocated
    /// row is in the broadcast recipient set, but we don't want a passing
    /// broadcast to wake a subagent nobody actually addressed.
    pub fn has_direct_unread(&self, name: &str) -> bool {
        let last_event_id = match self.get_instance_status(name) {
            Ok(Some(status)) => status.last_event_id,
            _ => 0,
        };
        let mut stmt = match self.conn.prepare_cached(
            "SELECT data FROM events
             WHERE id > ? AND type = 'message'
             ORDER BY id",
        ) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let rows = match stmt.query_map(params![last_event_id], |row| row.get::<_, String>(0)) {
            Ok(r) => r,
            Err(_) => return false,
        };
        for data in rows.flatten() {
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else {
                continue;
            };
            let scope = json
                .get("scope")
                .and_then(|s| s.as_str())
                .unwrap_or("broadcast");
            if scope != "mentions" {
                continue;
            }
            if Self::should_deliver_to(&json, name) {
                return true;
            }
        }
        false
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
                crate::log::log_error(
                    "db",
                    "get_unread_messages.get_instance_status",
                    &format!("{e}"),
                );
                0
            }
        };

        let mut stmt = match self.conn.prepare_cached(
            "SELECT id, timestamp, data FROM events
             WHERE id > ? AND type = 'message'
             ORDER BY id",
        ) {
            Ok(s) => s,
            Err(e) => {
                crate::log::log_error("db", "get_unread_messages.prepare", &format!("{e}"));
                return vec![];
            }
        };

        let rows = match stmt.query_map(params![last_event_id], |row| {
            let id: i64 = row.get(0)?;
            let timestamp: String = row.get(1)?;
            let data: String = row.get(2)?;
            Ok((id, timestamp, data))
        }) {
            Ok(r) => r,
            Err(e) => {
                crate::log::log_error("db", "get_unread_messages.query", &format!("{e}"));
                return vec![];
            }
        };

        let mut messages = Vec::new();
        for (id, timestamp, data) in rows.flatten() {
            // Parse JSON data
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) {
                if !Self::should_deliver_to(&json, name) {
                    continue;
                }

                let from = json
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let text = json
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let intent = json
                    .get("intent")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let thread = json
                    .get("thread")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let delivered_to = json
                    .get("delivered_to")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    });
                let bundle_id = json
                    .get("bundle_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let relay = json
                    .get("_relay")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                messages.push(Message {
                    from,
                    text,
                    intent,
                    thread,
                    event_id: Some(id),
                    timestamp: Some(timestamp.clone()),
                    delivered_to,
                    bundle_id,
                    relay,
                });
            }
        }

        messages
    }

    /// Emit "ready" life event and check for batch completion notification.
    ///
    /// Called on first status update (when status_context was "new").
    pub(super) fn emit_ready_event(&self, name: &str, status: &str, context: &str) -> Result<()> {
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

        self.log_event_with_ts("life", name, &event_data, None)?;

        // Check batch completion and send launcher notification
        if launcher != "unknown" {
            if let Some(ref bid) = batch_id {
                self.check_batch_completion(&launcher, bid)?;
            }
        }

        Ok(())
    }

    /// Check if all instances in a launch batch are ready; send notification if so.
    pub fn check_batch_completion(&self, launcher: &str, batch_id: &str) -> Result<()> {
        // Find the launch event for this batch
        let launch_data: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM events
             WHERE type = 'life' AND instance = ?
               AND json_extract(data, '$.action') = 'batch_launched'
               AND json_extract(data, '$.batch_id') = ?
             LIMIT 1",
                params![launcher, batch_id],
                |row| row.get(0),
            )
            .ok();

        let Some(data_str) = launch_data else {
            return Ok(());
        };
        let data: serde_json::Value = serde_json::from_str(&data_str)?;
        let expected = data.get("launched").and_then(|v| v.as_u64()).unwrap_or(0);
        if expected == 0 {
            return Ok(());
        }

        // Count ready events with matching batch_id
        let ready_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE type = 'life'
               AND json_extract(data, '$.action') = 'ready'
               AND json_extract(data, '$.batch_id') = ?",
            params![batch_id],
            |row| row.get(0),
        )?;

        if (ready_count as u64) < expected {
            return Ok(());
        }

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

        if already_sent {
            return Ok(());
        }

        // Get instance names from this batch
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT instance FROM events
             WHERE type = 'life'
               AND json_extract(data, '$.action') = 'ready'
               AND json_extract(data, '$.batch_id') = ?",
        )?;
        let names: Vec<String> = stmt
            .query_map(params![batch_id], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let instances_list = names.join(", ");
        let text = format!(
            "@{} All {} instances ready: {} (batch: {})",
            launcher, expected, instances_list, batch_id
        );

        // Insert system message
        let msg_data = serde_json::json!({
            "from": "[hcom-launcher]",
            "text": text,
            "scope": "mentions",
            "mentions": [launcher],
            "sender_kind": "system",
        });
        self.log_event_with_ts("message", "sys_[hcom-launcher]", &msg_data, None)?;

        Ok(())
    }

    /// Send a launcher-targeted notification for a failed launch instance.
    ///
    /// Used for early PTY startup failures so the launcher gets an active signal
    /// instead of having to poll `events launch`.
    pub fn notify_batch_failure(
        &self,
        launcher: &str,
        batch_id: &str,
        instance_name: &str,
        detail: &str,
    ) -> Result<()> {
        let text = format!(
            "@{} Launch failed: {}: {} (batch: {})",
            launcher, instance_name, detail, batch_id
        );

        let already_sent: bool = self.conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE type = 'message'
               AND instance = 'sys_[hcom-launcher]'
               AND json_extract(data, '$.text') = ?
             LIMIT 1",
            params![text],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )?;

        if already_sent {
            return Ok(());
        }

        let msg_data = serde_json::json!({
            "from": "[hcom-launcher]",
            "text": text,
            "scope": "mentions",
            "mentions": [launcher],
            "sender_kind": "system",
        });
        self.log_event_with_ts("message", "sys_[hcom-launcher]", &msg_data, None)?;

        Ok(())
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

        self.log_event_with_ts("life", instance, &data, None)?;

        Ok(())
    }

    /// Insert event and return its ID. Calls subscription check inline.
    pub fn log_event(
        &self,
        event_type: &str,
        instance: &str,
        data: &serde_json::Value,
    ) -> Result<i64> {
        self.log_event_with_ts(event_type, instance, data, None)
    }

    /// Insert event with optional timestamp. Returns event ID.
    pub fn log_event_with_ts(
        &self,
        event_type: &str,
        instance: &str,
        data: &serde_json::Value,
        timestamp: Option<&str>,
    ) -> Result<i64> {
        let ts = match timestamp {
            Some(t) => t.to_string(),
            None => chrono_now_iso(),
        };
        let data_str = serde_json::to_string(data)?;

        self.conn.execute(
            "INSERT INTO events (timestamp, type, instance, data) VALUES (?, ?, ?, ?)",
            params![ts, event_type, instance, data_str],
        )?;
        let event_id = self.conn.last_insert_rowid();

        // Check event subscriptions inline.
        subscriptions::process_logged_event(self, event_id, event_type, instance, data);

        Ok(event_id)
    }

    /// Get events since a given ID with optional filters.
    pub fn get_events_since(
        &self,
        last_event_id: i64,
        event_type: Option<&str>,
        instance: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let mut query =
            "SELECT id, timestamp, type, instance, data FROM events WHERE id > ?".to_string();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(last_event_id)];

        if let Some(et) = event_type {
            query.push_str(" AND type = ?");
            param_values.push(Box::new(et.to_string()));
        }
        if let Some(inst) = instance {
            query.push_str(" AND instance = ?");
            param_values.push(Box::new(inst.to_string()));
        }
        query.push_str(" ORDER BY id");

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&query)?;
        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                let id: i64 = row.get(0)?;
                let timestamp: String = row.get(1)?;
                let etype: String = row.get(2)?;
                let inst: String = row.get(3)?;
                let data_str: String = row.get(4)?;
                Ok((id, timestamp, etype, inst, data_str))
            })?
            .filter_map(|r| r.ok())
            .map(|(id, timestamp, etype, inst, data_str)| {
                let data: serde_json::Value =
                    serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "id": id,
                    "timestamp": timestamp,
                    "type": etype,
                    "instance": inst,
                    "data": data,
                })
            })
            .collect();
        Ok(rows)
    }

    /// Get current maximum event ID, or 0 if no events.
    pub fn get_last_event_id(&self) -> i64 {
        self.conn
            .query_row("SELECT MAX(id) FROM events", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .unwrap_or(None)
            .unwrap_or(0)
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

        self.log_event_with_ts("status", instance, &data, timestamp)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{cleanup_test_db, setup_full_test_db};

    #[test]
    fn test_log_event_returns_id() {
        let (db, db_path) = setup_full_test_db();

        let data = serde_json::json!({"status": "active", "context": "test"});
        let id1 = db.log_event("status", "luna", &data).unwrap();
        let id2 = db.log_event("status", "luna", &data).unwrap();

        assert!(id1 > 0);
        assert_eq!(id2, id1 + 1);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_events_since() {
        let (db, db_path) = setup_full_test_db();

        let data1 = serde_json::json!({"status": "active"});
        let data2 = serde_json::json!({"action": "ready"});
        let id1 = db.log_event("status", "luna", &data1).unwrap();
        let _id2 = db.log_event("life", "nova", &data2).unwrap();

        // Get all events
        let all = db.get_events_since(0, None, None).unwrap();
        assert_eq!(all.len(), 2);

        // Get events since first
        let since = db.get_events_since(id1, None, None).unwrap();
        assert_eq!(since.len(), 1);

        // Filter by type
        let status_only = db.get_events_since(0, Some("status"), None).unwrap();
        assert_eq!(status_only.len(), 1);

        // Filter by instance
        let nova_only = db.get_events_since(0, None, Some("nova")).unwrap();
        assert_eq!(nova_only.len(), 1);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_last_event_id() {
        let (db, db_path) = setup_full_test_db();

        assert_eq!(db.get_last_event_id(), 0);

        let data = serde_json::json!({"status": "active"});
        let id = db.log_event("status", "luna", &data).unwrap();
        assert_eq!(db.get_last_event_id(), id);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_notify_batch_failure_is_targeted_and_deduplicated() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('leku', 1000.0)",
                [],
            )
            .unwrap();

        db.notify_batch_failure("leku", "batch-1", "para", "boom")
            .unwrap();
        db.notify_batch_failure("leku", "batch-1", "para", "boom")
            .unwrap();

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE type = 'message'
                   AND instance = 'sys_[hcom-launcher]'
                   AND json_extract(data, '$.text') = '@leku Launch failed: para: boom (batch: batch-1)'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        cleanup_test_db(db_path);
    }

    /// Regression: a broadcast must NOT count as direct unread for a dormant
    /// subagent, otherwise SubagentStop wakes every dormant subagent on every
    /// broadcast and the "no message in → no keep-alive" gate is broken.
    #[test]
    fn test_has_direct_unread_ignores_broadcasts() {
        let (db, db_path) = setup_full_test_db();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at, last_event_id) \
                 VALUES ('luna_reviewer_1', 1000.0, 0)",
                [],
            )
            .unwrap();

        // Broadcast to everyone — must be ignored.
        db.log_event(
            "message",
            "sender",
            &serde_json::json!({"scope": "broadcast", "from": "sender", "text": "hi all"}),
        )
        .unwrap();
        assert!(!db.has_direct_unread("luna_reviewer_1"));

        // Direct mention of a different subagent — also ignored.
        db.log_event(
            "message",
            "sender",
            &serde_json::json!({
                "scope": "mentions",
                "mentions": ["other"],
                "from": "sender",
                "text": "hey other",
            }),
        )
        .unwrap();
        assert!(!db.has_direct_unread("luna_reviewer_1"));

        // Direct mention of this subagent — must trigger.
        db.log_event(
            "message",
            "sender",
            &serde_json::json!({
                "scope": "mentions",
                "mentions": ["luna_reviewer_1"],
                "from": "sender",
                "text": "hey you",
            }),
        )
        .unwrap();
        assert!(db.has_direct_unread("luna_reviewer_1"));

        cleanup_test_db(db_path);
    }
}
