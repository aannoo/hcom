//! TCP wake — connect-and-close pings that unblock poll loops in target processes.
//!
//! Pairs with [`crate::notify::server::NotifyServer`] (the receive side).
//! `notify_endpoints` rows of a [`WakeKind`] register a port; this module pokes them.
//!
//! `inject` endpoints are NOT pinged here — they speak a bidirectional protocol
//! (`commands::term`) and would mishandle a connect-drop.

use std::net::TcpStream;
use std::time::Duration;

use rusqlite::params;

use crate::db::HcomDb;

use super::WakeKind;

/// Timeout for broadcast wake (`wake_all`). Each call connects to N targets;
/// per-target latency is amortized across the fan-out.
pub const WAKE_FANOUT_MS: u64 = 50;

/// Timeout for single-target wake. If the connect misses, the wakeup is lost
/// until the next event, so we trade latency for reliability.
pub const WAKE_TARGETED_MS: u64 = 100;

/// SQL fragment listing the wake kinds — used to filter `notify_endpoints`
/// queries so inject ports are never pinged with connect-drop.
fn wake_kinds_sql_list() -> String {
    WakeKind::ALL
        .iter()
        .map(|k| format!("'{}'", k.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Wake a specific instance's wake endpoints.
///
/// If `kinds` is empty, wakes all wake kinds registered for the instance.
/// `inject` is never woken regardless.
pub fn wake(db: &HcomDb, instance: &str, kinds: &[WakeKind]) {
    let ports = lookup_ports(db, instance, kinds);
    wake_ports(&ports, WAKE_TARGETED_MS);
}

/// Wake every wake endpoint registered system-wide.
///
/// Used by `hcom send`, relay pull, and config changes to broadcast new state.
/// Filters out inject ports — their protocol is RPC, not connect-drop.
pub fn wake_all(db: &HcomDb) {
    let sql = format!(
        "SELECT DISTINCT port FROM notify_endpoints \
         WHERE port > 0 AND kind IN ({})",
        wake_kinds_sql_list()
    );
    let Ok(mut stmt) = db.conn().prepare(&sql) else {
        return;
    };

    let ports: Vec<u16> = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .filter_map(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0)
        .collect();

    wake_ports(&ports, WAKE_FANOUT_MS);
}

/// Snapshot wake-endpoint ports for an instance.
///
/// Used by the stop pattern in `hooks::common::finalize_instance_inner`,
/// which must capture ports BEFORE `delete_notify_endpoints` removes the rows
/// and wake them AFTER `delete_instance` so listeners see the row gone.
pub fn snapshot_wake_ports(db: &HcomDb, instance: &str) -> Vec<u16> {
    lookup_ports(db, instance, &[])
}

/// Connect-and-close on each port to fire a wake. Best-effort; errors ignored.
pub fn wake_ports(ports: &[u16], timeout_ms: u64) {
    let timeout = Duration::from_millis(timeout_ms);
    for &port in ports {
        if port == 0 {
            continue;
        }
        let addr = format!("127.0.0.1:{port}");
        if let Ok(addr) = addr.parse() {
            let _ = TcpStream::connect_timeout(&addr, timeout);
        }
    }
}

/// SELECT ports for an instance, filtered to wake kinds. Empty `kinds` means
/// all wake kinds. Inject is excluded in either case.
fn lookup_ports(db: &HcomDb, instance: &str, kinds: &[WakeKind]) -> Vec<u16> {
    let kinds_sql = if kinds.is_empty() {
        wake_kinds_sql_list()
    } else {
        kinds
            .iter()
            .map(|k| format!("'{}'", k.as_str()))
            .collect::<Vec<_>>()
            .join(",")
    };
    let sql = format!(
        "SELECT port FROM notify_endpoints \
         WHERE instance = ? AND kind IN ({kinds_sql})"
    );
    let Ok(mut stmt) = db.conn().prepare(&sql) else {
        return Vec::new();
    };
    stmt.query_map(params![instance], |row| row.get::<_, i64>(0))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .filter_map(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::HcomDb;
    use rusqlite::Connection;
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// Bind a non-blocking listener on an OS-assigned localhost port.
    fn bind_probe() -> TcpListener {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        listener
    }

    /// Wait up to `timeout` for the listener to accept a connection.
    /// Returns true on success, false if no connect arrived in time.
    fn await_connect(listener: &TcpListener, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok(_) => return true,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return false,
            }
        }
    }

    fn temp_db_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "hcom_wake_test_{}_{}_{}.db",
            std::process::id(),
            id,
            tag
        ))
    }

    fn open_db_with_endpoints(path: &std::path::Path) -> HcomDb {
        let conn = Connection::open(path).unwrap();
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
        drop(conn);
        HcomDb::open_raw(path).unwrap()
    }

    /// Bug fix: `wake_all` must NOT connect to inject ports — they speak a
    /// bidirectional RPC protocol, not connect-drop wake.
    #[test]
    fn wake_all_skips_inject_ports() {
        let db_path = temp_db_path("skip_inject");
        let db = open_db_with_endpoints(&db_path);

        let wake_probe = bind_probe();
        let inject_probe = bind_probe();
        let wake_port = wake_probe.local_addr().unwrap().port();
        let inject_port = inject_probe.local_addr().unwrap().port();

        db.upsert_notify_endpoint("inst", "pty", wake_port).unwrap();
        db.upsert_notify_endpoint("inst", "inject", inject_port)
            .unwrap();

        wake_all(&db);

        // Wake endpoint must receive the connect.
        assert!(
            await_connect(&wake_probe, Duration::from_millis(500)),
            "wake_all did not connect to the pty wake port"
        );
        // Inject endpoint must NOT — give it generous time before declaring it skipped.
        assert!(
            !await_connect(&inject_probe, Duration::from_millis(200)),
            "wake_all must not connect to inject ports (RPC protocol, not wake)"
        );

        let _ = std::fs::remove_file(db_path);
    }

    /// `wake(instance, &[])` (empty kinds = "all wake kinds") must also skip inject.
    #[test]
    fn wake_empty_kinds_skips_inject() {
        let db_path = temp_db_path("empty_kinds");
        let db = open_db_with_endpoints(&db_path);

        let wake_probe = bind_probe();
        let inject_probe = bind_probe();
        let wake_port = wake_probe.local_addr().unwrap().port();
        let inject_port = inject_probe.local_addr().unwrap().port();

        db.upsert_notify_endpoint("inst", "hook", wake_port).unwrap();
        db.upsert_notify_endpoint("inst", "inject", inject_port)
            .unwrap();

        wake(&db, "inst", &[]);

        assert!(
            await_connect(&wake_probe, Duration::from_millis(500)),
            "wake(empty kinds) did not connect to the hook wake port"
        );
        assert!(
            !await_connect(&inject_probe, Duration::from_millis(200)),
            "wake(empty kinds) must not connect to inject ports"
        );

        let _ = std::fs::remove_file(db_path);
    }

    /// `wake(instance, &[Kind])` must wake only the requested kind.
    #[test]
    fn wake_specific_kind_targets_only_that_kind() {
        let db_path = temp_db_path("specific_kind");
        let db = open_db_with_endpoints(&db_path);

        let hook_probe = bind_probe();
        let pty_probe = bind_probe();
        let hook_port = hook_probe.local_addr().unwrap().port();
        let pty_port = pty_probe.local_addr().unwrap().port();

        db.upsert_notify_endpoint("inst", "hook", hook_port).unwrap();
        db.upsert_notify_endpoint("inst", "pty", pty_port).unwrap();

        wake(&db, "inst", &[WakeKind::Hook]);

        assert!(
            await_connect(&hook_probe, Duration::from_millis(500)),
            "wake(Hook) did not connect to the hook port"
        );
        assert!(
            !await_connect(&pty_probe, Duration::from_millis(200)),
            "wake(Hook) must not connect to the pty port"
        );

        let _ = std::fs::remove_file(db_path);
    }

    /// `snapshot_wake_ports` is the API used by finalize_instance_inner to
    /// capture ports BEFORE row deletion. It must return wake ports and skip inject.
    #[test]
    fn snapshot_wake_ports_excludes_inject() {
        let db_path = temp_db_path("snapshot");
        let db = open_db_with_endpoints(&db_path);

        db.upsert_notify_endpoint("inst", "pty", 9001).unwrap();
        db.upsert_notify_endpoint("inst", "hook", 9002).unwrap();
        db.upsert_notify_endpoint("inst", "inject", 9003).unwrap();

        let mut ports = snapshot_wake_ports(&db, "inst");
        ports.sort();
        assert_eq!(
            ports,
            vec![9001, 9002],
            "snapshot must include wake kinds (pty, hook) and exclude inject"
        );

        let _ = std::fs::remove_file(db_path);
    }
}
