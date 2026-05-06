//! Notify endpoint and liveness helpers.

use anyhow::Result;
use rusqlite::params;

use super::HcomDb;
use crate::shared::constants::ST_LISTENING;
use crate::shared::time::{now_epoch_f64, now_epoch_i64};

impl HcomDb {
    /// Register notify endpoint for PTY wake-ups
    ///
    /// Inserts or updates notify_endpoints table with (instance, kind='pty', port)
    pub fn register_notify_port(&self, name: &str, port: u16) -> Result<()> {
        self.upsert_notify_endpoint(name, "pty", port)
    }

    /// Register inject port for screen queries
    pub fn register_inject_port(&self, name: &str, port: u16) -> Result<()> {
        self.upsert_notify_endpoint(name, "inject", port)
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

    /// Delete notify endpoints for an instance
    pub fn delete_notify_endpoints(&self, name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ?",
            params![name],
        )?;
        Ok(())
    }

    /// Insert or update a notify endpoint with specific kind.
    /// Used by listen command to register listen/listen_filter endpoints.
    pub fn upsert_notify_endpoint(&self, name: &str, kind: &str, port: u16) -> Result<()> {
        let now = now_epoch_f64();

        self.conn.execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(instance, kind) DO UPDATE SET
                 port = excluded.port,
                 updated_at = excluded.updated_at",
            params![name, kind, port as i64, now],
        )?;
        Ok(())
    }

    /// Delete a specific notify endpoint by instance and kind.
    pub fn delete_notify_endpoint(&self, name: &str, kind: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ? AND kind = ?",
            params![name, kind],
        )?;
        Ok(())
    }

    /// Check if any notify endpoint exists for an instance.
    pub fn has_notify_endpoint(&self, name: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM notify_endpoints WHERE instance = ? LIMIT 1",
                params![name],
                |_| Ok(()),
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::super::HcomDb;
    use super::super::tests::{cleanup_test_db, setup_test_db};
    use rusqlite::Connection;
    use std::path::PathBuf;

    fn setup_test_db_with_endpoints() -> (Connection, PathBuf) {
        let (conn, db_path) = setup_test_db();
        conn.execute_batch(
            "CREATE TABLE notify_endpoints (
                instance TEXT NOT NULL,
                kind TEXT NOT NULL,
                port INTEGER NOT NULL,
                updated_at REAL NOT NULL,
                PRIMARY KEY (instance, kind)
            );",
        )
        .unwrap();
        (conn, db_path)
    }

    #[test]
    fn test_register_inject_port_inserts() {
        let (_conn, db_path) = setup_test_db_with_endpoints();
        let db = HcomDb::open_raw(&db_path).unwrap();

        db.register_inject_port("test", 5555).unwrap();

        let port: i64 = db
            .conn
            .query_row(
                "SELECT port FROM notify_endpoints WHERE instance = 'test' AND kind = 'inject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(port, 5555);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_register_inject_port_upserts() {
        let (_conn, db_path) = setup_test_db_with_endpoints();
        let db = HcomDb::open_raw(&db_path).unwrap();

        db.register_inject_port("test", 5555).unwrap();
        db.register_inject_port("test", 6666).unwrap();

        let port: i64 = db
            .conn
            .query_row(
                "SELECT port FROM notify_endpoints WHERE instance = 'test' AND kind = 'inject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(port, 6666);

        // Should be exactly one row
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM notify_endpoints WHERE instance = 'test'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        cleanup_test_db(db_path);
    }
}
