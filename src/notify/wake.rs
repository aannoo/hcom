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

/// Bound simultaneous connect attempts while preventing one unreachable endpoint
/// from serially consuming the full timeout for every other endpoint.
const MAX_WAKE_FANOUT_WORKERS: usize = 32;

/// SQL fragment listing instance loops. Inject ports speak RPC, and launch
/// confirmations have a separate batch-scoped wake path.
fn wake_kinds_sql_list() -> String {
    WakeKind::INSTANCE_LOOPS
        .iter()
        .map(|k| format!("'{}'", k.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Wake a specific instance's wake endpoints.
///
/// If `kinds` is empty, wakes the instance loops registered for the instance.
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

/// Wake launch confirmations for this batch, including aggregate waiters.
/// Events without batch metadata still wake all launch waiters; their status
/// query may discover a stopped or failed child, so skipping them is unsafe.
pub fn wake_launch_waiters(db: &HcomDb, batch_id: Option<&str>) {
    let kind = WakeKind::LaunchWait;
    let Ok(mut stmt) = db
        .conn()
        .prepare("SELECT instance, port FROM notify_endpoints WHERE kind = ? AND port > 0")
    else {
        return;
    };
    let endpoints: Vec<(String, u16)> = stmt
        .query_map(params![kind.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|(name, port)| u16::try_from(port).ok().map(|port| (name, port)))
        .collect();
    drop(stmt);
    for (name, port) in endpoints {
        let remove = || {
            let _ = db.conn().execute(
                "DELETE FROM notify_endpoints WHERE instance = ? AND kind = ? AND port = ?",
                params![name, kind.as_str(), port],
            );
        };
        let owner = name
            .strip_prefix("launch-wait:")
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        if let Some(pid) = owner
            .as_ref()
            .and_then(|owner| owner.get("pid"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
        {
            let expected_identity = owner
                .as_ref()
                .and_then(|owner| owner.get("process_identity"))
                .and_then(serde_json::Value::as_str);
            let stale = if let (Some(expected), Some(actual)) =
                (expected_identity, crate::sys::process::identity(pid))
            {
                expected != actual
            } else {
                !crate::sys::process::is_alive(pid)
            };
            if stale {
                // Reap dead or reused PIDs before filtering by batch, without
                // opening unrelated listeners just to probe their liveness.
                remove();
                continue;
            }
        }
        let scope = owner
            .as_ref()
            .and_then(|owner| owner.get("batch_prefix"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if let Some(batch_id) = batch_id
            && !scope.is_empty()
            && !batch_id
                .get(..scope.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scope))
        {
            continue;
        }
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if TcpStream::connect_timeout(&addr, Duration::from_millis(WAKE_FANOUT_MS)).is_err() {
            // An abruptly killed temporary waiter cannot run its Drop cleanup.
            // Reap only the endpoint we tried, in case a registration changed.
            remove();
        }
    }
}

/// Snapshot wake-endpoint ports for an instance.
///
/// Used by the stop pattern in `hooks::common::finalize_instance_inner`,
/// which must capture ports BEFORE `delete_notify_endpoints` removes the rows
/// and wake them AFTER `delete_instance` so listeners see the row gone.
pub fn snapshot_wake_ports(db: &HcomDb, instance: &str) -> Vec<u16> {
    lookup_ports(db, instance, WakeKind::ALL)
}

/// Connect-and-close on each port to fire a wake. Best-effort; errors ignored.
pub fn wake_ports(ports: &[u16], timeout_ms: u64) {
    let timeout = Duration::from_millis(timeout_ms);
    let valid_ports: Vec<u16> = ports.iter().copied().filter(|port| *port > 0).collect();
    for ports in valid_ports.chunks(MAX_WAKE_FANOUT_WORKERS) {
        std::thread::scope(|scope| {
            for &port in ports {
                scope.spawn(move || {
                    let addr = format!("127.0.0.1:{port}");
                    if let Ok(addr) = addr.parse() {
                        let _ = TcpStream::connect_timeout(&addr, timeout);
                    }
                });
            }
        });
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

    #[test]
    fn wake_launch_waiters_reaps_abandoned_waiters_and_keeps_other_kinds() {
        let db_path = temp_db_path("abandoned_waiter");
        let db = open_db_with_endpoints(&db_path);
        let probe = bind_probe();
        let dead_port = probe.local_addr().unwrap().port();
        drop(probe);
        let live = bind_probe();
        let unrelated = bind_probe();
        db.upsert_notify_endpoint("dead", "launch_wait", dead_port)
            .unwrap();
        db.upsert_notify_endpoint("live", "launch_wait", live.local_addr().unwrap().port())
            .unwrap();
        db.upsert_notify_endpoint("other", "pty", unrelated.local_addr().unwrap().port())
            .unwrap();
        wake_launch_waiters(&db, None);
        assert!(!db.has_notify_endpoint_kind("dead", "launch_wait"));
        assert!(db.has_notify_endpoint_kind("live", "launch_wait"));
        assert!(await_connect(&live, Duration::from_millis(500)));
        assert!(!await_connect(&unrelated, Duration::from_millis(50)));
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn launch_waiters_reap_dead_owners_from_unrelated_batches() {
        let db_path = temp_db_path("unrelated_dead_owner");
        let db = open_db_with_endpoints(&db_path);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let dead_probe = bind_probe();
        let live_probe = bind_probe();
        let dead = format!(
            "launch-wait:{}",
            serde_json::json!({"pid": pid, "batch_prefix": "old-batch"})
        );
        let current_pid = std::process::id();
        let live = format!(
            "launch-wait:{}",
            serde_json::json!({
                "pid": current_pid, "process_identity": crate::sys::process::identity(current_pid),
                "batch_prefix": "different-live-batch"
            })
        );
        db.upsert_notify_endpoint(
            &dead,
            "launch_wait",
            dead_probe.local_addr().unwrap().port(),
        )
        .unwrap();
        db.upsert_notify_endpoint(
            &live,
            "launch_wait",
            live_probe.local_addr().unwrap().port(),
        )
        .unwrap();
        wake_launch_waiters(&db, Some("new-batch"));
        assert!(!db.has_notify_endpoint_kind(&dead, "launch_wait"));
        assert!(db.has_notify_endpoint_kind(&live, "launch_wait"));
        assert!(!crate::sys::net::wait_readable(&live_probe, Duration::ZERO));
        assert!(!crate::sys::net::wait_readable(&dead_probe, Duration::ZERO));
        // Model PID reuse: a live PID with a different recorded creation time
        // must not keep an abandoned endpoint alive.
        if crate::sys::process::identity(current_pid).is_some() {
            let reused = format!(
                "launch-wait:{}",
                serde_json::json!({
                    "pid": current_pid, "process_identity": "previous-incarnation", "batch_prefix": "old-batch"
                })
            );
            db.upsert_notify_endpoint(
                &reused,
                "launch_wait",
                dead_probe.local_addr().unwrap().port(),
            )
            .unwrap();
            wake_launch_waiters(&db, Some("new-batch"));
            assert!(!db.has_notify_endpoint_kind(&reused, "launch_wait"));
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn ordinary_broadcasts_skip_launch_waiters() {
        let db_path = temp_db_path("skip_launch_waiters");
        let db = open_db_with_endpoints(&db_path);
        let launch = bind_probe();
        let delivery = bind_probe();
        db.upsert_notify_endpoint("waiter", "launch_wait", launch.local_addr().unwrap().port())
            .unwrap();
        db.upsert_notify_endpoint("agent", "pty", delivery.local_addr().unwrap().port())
            .unwrap();
        wake_all(&db);
        assert!(await_connect(&delivery, Duration::from_millis(500)));
        assert!(!crate::sys::net::wait_readable(&launch, Duration::ZERO));
        assert!(db.has_notify_endpoint_kind("waiter", "launch_wait"));
        let _ = std::fs::remove_file(db_path);
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

        db.upsert_notify_endpoint("inst", "hook", wake_port)
            .unwrap();
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

        db.upsert_notify_endpoint("inst", "hook", hook_port)
            .unwrap();
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
