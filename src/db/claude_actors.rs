//! Database-backed Claude shell actor capabilities.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use super::HcomDb;
use crate::shared::time::now_epoch_i64;

const CAPABILITY_TTL_SECS: i64 = 60 * 60;

impl HcomDb {
    /// Create or reuse an opaque actor capability for one exact shell tool use.
    /// Duplicate hook registrations receive the same token for the same tuple.
    pub fn issue_claude_actor_capability(
        &self,
        session_id: &str,
        tool_use_id: &str,
        agent_id: Option<&str>,
        instance_name: &str,
    ) -> Result<String> {
        let actor_key = agent_id.unwrap_or("");
        let now = now_epoch_i64();
        let expires_at = now + CAPABILITY_TTL_SECS;

        self.with_immediate_transaction(|txn| {
            txn.execute(
                "DELETE FROM claude_actor_capabilities WHERE expires_at <= ?",
                params![now],
            )?;

            let existing: Option<(String, String)> = txn
                .query_row(
                    "SELECT token, instance_name
                     FROM claude_actor_capabilities
                     WHERE session_id = ? AND tool_use_id = ? AND agent_id = ?",
                    params![session_id, tool_use_id, actor_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;

            if let Some((token, stored_instance)) = existing {
                if stored_instance != instance_name {
                    anyhow::bail!(
                        "Claude actor tuple already belongs to '{}' instead of '{}'",
                        stored_instance,
                        instance_name
                    );
                }
                txn.execute(
                    "UPDATE claude_actor_capabilities
                     SET expires_at = ?, last_seen = ?
                     WHERE token = ?",
                    params![expires_at, now, token],
                )?;
                return Ok(token);
            }

            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            txn.execute(
                "INSERT INTO claude_actor_capabilities
                 (token, session_id, tool_use_id, agent_id, instance_name, created_at, expires_at, last_seen)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    token,
                    session_id,
                    tool_use_id,
                    actor_key,
                    instance_name,
                    now,
                    expires_at,
                    now
                ],
            )?;
            Ok(token)
        })
    }

    /// Validate a capability and return its exact live actor row name.
    pub fn resolve_claude_actor_capability(
        &self,
        token: &str,
        expected_session_id: &str,
    ) -> Result<Option<String>> {
        if token.is_empty() || expected_session_id.is_empty() {
            return Ok(None);
        }

        let now = now_epoch_i64();
        let record: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT session_id, agent_id, instance_name
                 FROM claude_actor_capabilities
                 WHERE token = ? AND expires_at > ?",
                params![token, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let Some((session_id, agent_id, instance_name)) = record else {
            let _ = self.conn.execute(
                "DELETE FROM claude_actor_capabilities WHERE token = ?",
                params![token],
            );
            return Ok(None);
        };

        if session_id != expected_session_id {
            return Ok(None);
        }

        let valid = if agent_id.is_empty() {
            self.conn
                .query_row(
                    "SELECT 1 FROM instances
                     WHERE name = ? AND session_id = ?
                       AND (parent_name IS NULL OR parent_name = '')",
                    params![instance_name, session_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
        } else {
            self.conn
                .query_row(
                    "SELECT 1 FROM instances
                     WHERE name = ? AND agent_id = ? AND parent_session_id = ?
                       AND parent_name IS NOT NULL AND parent_name != ''",
                    params![instance_name, agent_id, session_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
        };

        if !valid {
            self.conn.execute(
                "DELETE FROM claude_actor_capabilities WHERE token = ?",
                params![token],
            )?;
            return Ok(None);
        }

        self.conn.execute(
            "UPDATE claude_actor_capabilities SET last_seen = ? WHERE token = ?",
            params![now, token],
        )?;
        Ok(Some(instance_name))
    }

    /// Revoke the capability associated with a completed/failed tool use.
    pub fn revoke_claude_actor_capability(
        &self,
        session_id: &str,
        tool_use_id: &str,
        agent_id: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM claude_actor_capabilities
             WHERE session_id = ? AND tool_use_id = ? AND agent_id = ?",
            params![session_id, tool_use_id, agent_id.unwrap_or("")],
        )?;
        Ok(())
    }

    pub fn revoke_claude_actor_capabilities_for_instance(&self, instance_name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM claude_actor_capabilities WHERE instance_name = ?",
            params![instance_name],
        )?;
        Ok(())
    }

    pub fn revoke_claude_actor_capabilities_for_session(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM claude_actor_capabilities WHERE session_id = ?",
            params![session_id],
        )?;
        Ok(())
    }

    /// Keep outstanding root shell capabilities attached when the root
    /// deliberately rebinds to another hcom name.
    pub fn rebind_claude_root_actor_state(
        &self,
        session_id: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<()> {
        if session_id.is_empty() || old_name.is_empty() || old_name == new_name {
            return Ok(());
        }

        self.conn.execute(
            "UPDATE claude_actor_capabilities
             SET instance_name = ?
             WHERE session_id = ? AND agent_id = '' AND instance_name = ?",
            params![new_name, session_id, old_name],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_db() -> (TempDir, HcomDb) {
        let temp = TempDir::new().unwrap();
        let db = HcomDb::open_raw(&temp.path().join("actors.db")).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'agent-1', 'claude',
                         'inactive', 0, 0, 0)",
                [],
            )
            .unwrap();
        (temp, db)
    }

    #[test]
    fn duplicate_hook_tuple_reuses_capability() {
        let (_temp, db) = setup_db();
        let first = db
            .issue_claude_actor_capability("sess-1", "tool-1", Some("agent-1"), "nova_task_1")
            .unwrap();
        let second = db
            .issue_claude_actor_capability("sess-1", "tool-1", Some("agent-1"), "nova_task_1")
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            db.resolve_claude_actor_capability(&first, "sess-1")
                .unwrap(),
            Some("nova_task_1".to_string())
        );
    }

    #[test]
    fn root_and_child_capabilities_resolve_exact_actors() {
        let (_temp, db) = setup_db();
        let root = db
            .issue_claude_actor_capability("sess-1", "tool-root", None, "nova")
            .unwrap();
        let child = db
            .issue_claude_actor_capability("sess-1", "tool-child", Some("agent-1"), "nova_task_1")
            .unwrap();

        assert_eq!(
            db.resolve_claude_actor_capability(&root, "sess-1").unwrap(),
            Some("nova".to_string())
        );
        assert_eq!(
            db.resolve_claude_actor_capability(&child, "sess-1")
                .unwrap(),
            Some("nova_task_1".to_string())
        );
        assert_eq!(
            db.resolve_claude_actor_capability(&root, "foreign-session")
                .unwrap(),
            None
        );
        assert_ne!(root, child);
    }

    #[test]
    fn invalid_expired_and_revoked_capabilities_fail_closed() {
        let (_temp, db) = setup_db();
        let token = db
            .issue_claude_actor_capability("sess-1", "tool-1", None, "nova")
            .unwrap();
        assert_eq!(
            db.resolve_claude_actor_capability("tampered", "sess-1")
                .unwrap(),
            None
        );

        db.conn()
            .execute(
                "UPDATE claude_actor_capabilities SET expires_at = 0 WHERE token = ?",
                params![token],
            )
            .unwrap();
        assert_eq!(
            db.resolve_claude_actor_capability(&token, "sess-1")
                .unwrap(),
            None
        );

        let token = db
            .issue_claude_actor_capability("sess-1", "tool-2", None, "nova")
            .unwrap();
        db.revoke_claude_actor_capability("sess-1", "tool-2", None)
            .unwrap();
        assert_eq!(
            db.resolve_claude_actor_capability(&token, "sess-1")
                .unwrap(),
            None
        );
    }

    #[test]
    fn tuple_cannot_be_reassigned_to_another_actor() {
        let (_temp, db) = setup_db();
        db.issue_claude_actor_capability("sess-1", "tool-1", Some("agent-1"), "nova_task_1")
            .unwrap();
        assert!(
            db.issue_claude_actor_capability("sess-1", "tool-1", Some("agent-1"), "nova")
                .is_err()
        );
    }
}
