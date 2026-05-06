//! Session and process binding helpers.

use anyhow::{Result, bail};
use rusqlite::params;

use super::HcomDb;
use crate::shared::time::now_epoch_f64;

impl HcomDb {
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
        let mut stmt = self
            .conn
            .prepare_cached("SELECT instance_name FROM process_bindings WHERE process_id = ?")?;

        match stmt.query_row(params![process_id], |row| row.get::<_, String>(0)) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get process binding with session_id. Returns (session_id, instance_name).
    pub fn get_process_binding_full(
        &self,
        process_id: &str,
    ) -> Result<Option<(Option<String>, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT session_id, instance_name FROM process_bindings WHERE process_id = ?",
        )?;

        match stmt.query_row(params![process_id], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(pair) => Ok(Some(pair)),
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
                crate::log::log_error("db", "get_cursor.get_instance_status", &format!("{e}"));
                0
            }
        }
    }

    /// Check if instance has a session binding (session_id is set and non-empty).
    /// Used by OpenCode delivery thread to skip PTY injection when plugin is active.
    pub fn has_session(&self, name: &str) -> bool {
        match self.conn.query_row(
            "SELECT session_id FROM instances WHERE name = ?",
            params![name],
            |row| row.get::<_, String>(0),
        ) {
            Ok(sid) => !sid.is_empty(),
            _ => false,
        }
    }

    /// Check if there are pending (unread) messages for an instance.
    ///
    /// Lightweight check — parses only the JSON `data` column (skipping full
    /// Message construction) and returns on the first matching row.
    pub fn has_pending(&self, name: &str) -> bool {
        let last_event_id = match self.get_instance_status(name) {
            Ok(Some(status)) => status.last_event_id,
            Ok(None) => 0,
            Err(e) => {
                crate::log::log_error("db", "has_pending.get_instance_status", &format!("{e}"));
                0
            }
        };

        let mut stmt = match self
            .conn
            .prepare_cached("SELECT data FROM events WHERE id > ? AND type = 'message'")
        {
            Ok(s) => s,
            Err(e) => {
                crate::log::log_error("db", "has_pending.prepare", &format!("{e}"));
                return false;
            }
        };

        let rows = match stmt.query_map(params![last_event_id], |row| row.get::<_, String>(0)) {
            Ok(r) => r,
            Err(e) => {
                crate::log::log_error("db", "has_pending.query", &format!("{e}"));
                return false;
            }
        };

        for data in rows.flatten() {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) {
                if Self::should_deliver_to(&json, name) {
                    return true;
                }
            }
        }
        false
    }

    /// Get instance name bound to session_id, or None if not bound.
    pub fn get_session_binding(&self, session_id: &str) -> Result<Option<String>> {
        if session_id.is_empty() {
            return Ok(None);
        }
        match self.conn.query_row(
            "SELECT instance_name FROM session_bindings WHERE session_id = ?",
            params![session_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Create or update session binding.
    /// Returns error if session_id is already bound to a different instance.
    pub fn set_session_binding(&self, session_id: &str, instance_name: &str) -> Result<()> {
        if session_id.is_empty() || instance_name.is_empty() {
            return Ok(());
        }

        // Check for existing binding to different instance
        if let Some(existing) = self.get_session_binding(session_id)? {
            if existing != instance_name {
                // Check if this is a subagent trying to bind without --name <agent_id>
                if let Ok(Some(inst)) = self.get_instance(&existing) {
                    if let Some(rt) = inst.get("running_tasks").and_then(|v| v.as_str()) {
                        if let Ok(tasks) = serde_json::from_str::<serde_json::Value>(rt) {
                            if let Some(subs) = tasks.get("subagents").and_then(|v| v.as_array()) {
                                if !subs.is_empty() {
                                    let ids: Vec<&str> = subs
                                        .iter()
                                        .filter_map(|s| s.get("agent_id").and_then(|v| v.as_str()))
                                        .collect();
                                    bail!(
                                        "Session bound to parent '{}'. \
                                         Subagents must use: hcom start --name <agent_id>\n\
                                         Active agent_ids: {}",
                                        existing,
                                        ids.join(", ")
                                    );
                                }
                            }
                        }
                    }
                }
                bail!(
                    "Session {}... already bound to {}, cannot bind to {}",
                    &session_id[..session_id.len().min(8)],
                    existing,
                    instance_name
                );
            }
        }

        let now = now_epoch_f64();

        self.conn.execute(
            "INSERT INTO session_bindings (session_id, instance_name, created_at)
             VALUES (?, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET
                 instance_name = excluded.instance_name,
                 created_at = excluded.created_at",
            params![session_id, instance_name, now],
        )?;
        Ok(())
    }

    /// Clear session_id from any instance except exclude_instance.
    pub fn clear_session_id_from_other_instances(
        &self,
        session_id: &str,
        exclude_instance: &str,
    ) -> Result<()> {
        if session_id.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "UPDATE instances SET session_id = NULL WHERE session_id = ? AND name != ?",
            params![session_id, exclude_instance],
        )?;
        Ok(())
    }

    /// Explicitly rebind session to a different instance.
    pub fn rebind_session(&self, session_id: &str, new_instance_name: &str) -> Result<()> {
        if session_id.is_empty() || new_instance_name.is_empty() {
            return Ok(());
        }
        self.clear_session_id_from_other_instances(session_id, new_instance_name)?;
        self.upsert_session_binding(session_id, new_instance_name)
    }

    /// Internal helper: unconditional upsert of session binding.
    fn upsert_session_binding(&self, session_id: &str, instance_name: &str) -> Result<()> {
        let now = now_epoch_f64();
        self.conn.execute(
            "INSERT INTO session_bindings (session_id, instance_name, created_at)
             VALUES (?, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET
                 instance_name = excluded.instance_name,
                 created_at = excluded.created_at",
            params![session_id, instance_name, now],
        )?;
        Ok(())
    }

    /// Delete session binding.
    pub fn delete_session_binding(&self, session_id: &str) -> Result<()> {
        if session_id.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "DELETE FROM session_bindings WHERE session_id = ?",
            params![session_id],
        )?;
        Ok(())
    }

    /// Delete all session bindings for an instance.
    pub fn delete_session_bindings_for_instance(&self, instance_name: &str) -> Result<()> {
        if instance_name.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "DELETE FROM session_bindings WHERE instance_name = ?",
            params![instance_name],
        )?;
        Ok(())
    }

    /// Atomically rebind instance to new session.
    pub fn rebind_instance_session(&self, instance_name: &str, session_id: &str) -> Result<()> {
        if instance_name.is_empty() || session_id.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "DELETE FROM session_bindings WHERE instance_name = ?",
            params![instance_name],
        )?;
        self.conn.execute(
            "UPDATE instances SET session_id = NULL WHERE session_id = ? AND name != ?",
            params![session_id, instance_name],
        )?;
        self.upsert_session_binding(session_id, instance_name)?;
        Ok(())
    }

    /// Check if instance has a session binding (hooks active).
    pub fn has_session_binding(&self, instance_name: &str) -> bool {
        if instance_name.is_empty() {
            return false;
        }
        self.conn
            .query_row(
                "SELECT 1 FROM session_bindings WHERE instance_name = ? LIMIT 1",
                params![instance_name],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Check if instance has a process binding (hcom-launched).
    pub fn has_process_binding_for_instance(&self, instance_name: &str) -> bool {
        if instance_name.is_empty() {
            return false;
        }
        self.conn
            .query_row(
                "SELECT 1 FROM process_bindings WHERE instance_name = ? LIMIT 1",
                params![instance_name],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Set process binding (map process_id -> instance/session).
    /// Set process binding. Empty session_id is stored as NULL.
    pub fn set_process_binding(
        &self,
        process_id: &str,
        session_id: &str,
        instance_name: &str,
    ) -> Result<()> {
        let now = now_epoch_f64();
        // Normalize empty string to NULL
        let sid: Option<&str> = if session_id.is_empty() {
            None
        } else {
            Some(session_id)
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO process_bindings (process_id, session_id, instance_name, updated_at)
             VALUES (?, ?, ?, ?)",
            params![process_id, sid, instance_name, now],
        )?;
        Ok(())
    }

    /// Delete all process bindings for an instance.
    pub fn delete_process_bindings_for_instance(&self, instance_name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM process_bindings WHERE instance_name = ?",
            params![instance_name],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::HcomDb;
    use super::super::tests::{cleanup_test_db, setup_full_test_db, setup_test_db};

    #[test]
    fn test_get_process_binding_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE process_bindings", []).unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        let result = db.get_process_binding("test_pid");

        let err = result.expect_err("SQL error should propagate as Err");
        assert!(
            err.to_string().contains("process_bindings"),
            "expected missing process_bindings table error, got: {err:#}"
        );
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_session_binding_crud() {
        let (db, db_path) = setup_full_test_db();

        // Create instance first (FK constraint)
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        // No binding initially
        assert!(db.get_session_binding("sess-1").unwrap().is_none());

        // Set binding
        db.set_session_binding("sess-1", "luna").unwrap();
        assert_eq!(
            db.get_session_binding("sess-1").unwrap(),
            Some("luna".to_string())
        );

        // has_session_binding
        assert!(db.has_session_binding("luna"));

        // Delete binding
        db.delete_session_binding("sess-1").unwrap();
        assert!(db.get_session_binding("sess-1").unwrap().is_none());
        assert!(!db.has_session_binding("luna"));

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_session_binding_conflict() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('nova', 1000.0)",
                [],
            )
            .unwrap();

        // Bind session to luna
        db.set_session_binding("sess-1", "luna").unwrap();

        // Try binding same session to nova - should fail
        let result = db.set_session_binding("sess-1", "nova");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already bound to luna")
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_rebind_session() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, session_id, created_at) VALUES ('luna', 'sess-1', 1000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('nova', 1000.0)",
                [],
            )
            .unwrap();

        // Bind to luna first
        db.set_session_binding("sess-1", "luna").unwrap();

        // Rebind to nova (should clear from luna)
        db.rebind_session("sess-1", "nova").unwrap();
        assert_eq!(
            db.get_session_binding("sess-1").unwrap(),
            Some("nova".to_string())
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_rebind_instance_session() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        db.rebind_instance_session("luna", "sess-new").unwrap();
        assert_eq!(
            db.get_session_binding("sess-new").unwrap(),
            Some("luna".to_string())
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_process_binding_crud() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        // Set process binding
        db.set_process_binding("pid-123", "sess-1", "luna").unwrap();
        assert!(db.has_process_binding_for_instance("luna"));

        // Get binding
        let name = db.get_process_binding("pid-123").unwrap();
        assert_eq!(name, Some("luna".to_string()));

        // Delete
        db.delete_process_binding("pid-123").unwrap();
        assert!(!db.has_process_binding_for_instance("luna"));

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_delete_process_bindings_for_instance() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        db.set_process_binding("pid-1", "sess-1", "luna").unwrap();
        db.set_process_binding("pid-2", "sess-2", "luna").unwrap();
        assert!(db.has_process_binding_for_instance("luna"));

        db.delete_process_bindings_for_instance("luna").unwrap();
        assert!(!db.has_process_binding_for_instance("luna"));

        cleanup_test_db(db_path);
    }
}
