//! Relay worker process — manages the MQTT relay as a standalone process.
//!
//! Entry point for `hcom relay-worker`. Handles PID file management,
//! signal handling, auto-exit watchdog, and relay lifecycle.
//!
//! Auto-spawn: `maybe_auto_spawn()` checks config, PID, and instance count
//! before spawning a new relay-worker process.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::HcomConfig;
use crate::db::HcomDb;
use crate::log;
use crate::relay::client::RelayCommand;

// ── PID file helpers ────────────────────────────────────────────────

fn pid_file_path() -> PathBuf {
    crate::paths::hcom_dir().join(".tmp").join("relay.pid")
}

fn write_pid_file() {
    let pid = std::process::id().to_string();
    crate::paths::atomic_write(&pid_file_path(), &pid);
}

fn read_pid_file() -> Option<u32> {
    let path = pid_file_path();
    let content = std::fs::read_to_string(&path).ok()?;
    let pid: u32 = content.trim().parse().ok()?;
    if crate::pidtrack::is_alive(pid) {
        Some(pid)
    } else {
        // Stale PID file — clean up
        let _ = std::fs::remove_file(&path);
        None
    }
}

/// Remove PID file.
fn remove_pid_file() {
    let _ = std::fs::remove_file(pid_file_path());
}

/// Check if a relay-worker process is currently running.
pub fn is_relay_worker_running() -> bool {
    read_pid_file().is_some()
}

// ── Drop guard for PID file cleanup ─────────────────────────────────

struct PidFileGuard;

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        remove_pid_file();
    }
}

// ── Worker entry point ──────────────────────────────────────────────

/// Run the relay-worker process. Called from router dispatch.
pub fn run() -> i32 {
    // Check if already running
    if let Some(existing_pid) = read_pid_file() {
        eprintln!("relay-worker already running (PID {})", existing_pid);
        return 1;
    }

    // Write PID file (guard removes on exit)
    write_pid_file();
    let _pid_guard = PidFileGuard;

    log::log_info(
        "relay",
        "relay_worker.start",
        &format!("pid={}", std::process::id()),
    );

    // Load config
    let config = match HcomConfig::load(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: Failed to load config: {e}");
            return 1;
        }
    };

    if !super::is_relay_enabled(&config) {
        eprintln!("Error: Relay not configured or disabled");
        return 1;
    }

    // Connect to MQTT
    let (relay, connection, cmd_tx) = match super::client::MqttRelay::connect(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: Failed to connect: {e}");
            return 1;
        }
    };

    // Bind TCP notify listener for CLI → daemon push wake.
    // CLI callers (hcom send, hooks) connect to trigger immediate push.
    let notify_port = setup_notify_listener(&cmd_tx);

    // Install signal handlers via signal-hook (sets AtomicBool on SIGTERM/SIGINT).
    // The watchdog thread checks this flag — no separate signal-polling thread needed.
    let shutdown = Arc::new(AtomicBool::new(false));
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))
    {
        log::log_error(
            "relay",
            "signal.register.sigterm",
            &format!("Failed to register SIGTERM handler: {}", e),
        );
    }
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))
    {
        log::log_error(
            "relay",
            "signal.register.sigint",
            &format!("Failed to register SIGINT handler: {}", e),
        );
    }

    // Spawn auto-exit watchdog thread (also monitors shutdown flag)
    let cmd_tx_watchdog = cmd_tx;
    std::thread::spawn(move || {
        auto_exit_watchdog(cmd_tx_watchdog, shutdown);
    });

    // Run relay event loop (blocks until shutdown)
    relay.run(connection);

    // Clear notify port so CLI callers stop trying to connect
    if notify_port.is_some() {
        if let Ok(db) = HcomDb::open() {
            super::safe_kv_set(&db, "relay_daemon_port", None);
        }
    }

    log::log_info("relay", "relay_worker.stop", "exited cleanly");
    0
}

/// Bind TCP listener on random port for CLI→daemon push notifications.
/// Stores port in KV `relay_daemon_port`. Returns port on success.
fn setup_notify_listener(cmd_tx: &std::sync::mpsc::Sender<RelayCommand>) -> Option<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();

    // Store port in DB so CLI callers can find us
    if let Ok(db) = HcomDb::open() {
        super::safe_kv_set(&db, "relay_daemon_port", Some(&port.to_string()));
    }

    log::log_info(
        "relay",
        "relay_worker.notify_listen",
        &format!("port={}", port),
    );

    // Spawn thread to accept connections and send Push commands.
    // Each incoming TCP connection (no data, just connect+close) triggers a push.
    let cmd_tx = cmd_tx.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(conn) => {
                    drop(conn); // Close immediately — connection itself is the signal
                    if cmd_tx.send(RelayCommand::Push).is_err() {
                        break; // Relay shut down
                    }
                }
                Err(_) => break,
            }
        }
    });

    Some(port)
}

/// Auto-exit watchdog: every 30s, check if any local instances exist.
/// If none for 2 consecutive checks, or shutdown signal received, send Shutdown.
fn auto_exit_watchdog(cmd_tx: std::sync::mpsc::Sender<RelayCommand>, shutdown: Arc<AtomicBool>) {
    let mut consecutive_empty = 0u32;
    let mut db = HcomDb::open().ok();

    loop {
        std::thread::sleep(Duration::from_secs(30));

        if shutdown.load(Ordering::Relaxed) {
            let _ = cmd_tx.send(RelayCommand::Shutdown);
            return;
        }

        // Re-open DB if previous connection failed
        if db.is_none() {
            db = HcomDb::open().ok();
        }

        let count = match &db {
            Some(d) => local_instance_count(d),
            None => {
                consecutive_empty = 0;
                continue;
            }
        };

        if count == 0 {
            consecutive_empty += 1;
            if consecutive_empty >= 2 {
                log::log_info(
                    "relay",
                    "relay_worker.auto_exit",
                    "no local instances for 2 checks",
                );
                let _ = cmd_tx.send(RelayCommand::Shutdown);
                return;
            }
        } else {
            consecutive_empty = 0;
        }
    }
}

/// Count active local (non-remote) instances.
/// Mirrors the filter in ensure_worker(true) so the watchdog exits when no syncable
/// instances remain, not merely when all instances are stopped/dead.
fn local_instance_count(db: &HcomDb) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM instances \
             WHERE COALESCE(origin_device_id, '') = '' \
             AND status NOT IN ('stopped', 'dead')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
}

// ── Auto-spawn ──────────────────────────────────────────────────────

/// Spawn the relay-worker process (caller must check preconditions).
/// Detaches via setsid() so the worker survives terminal close.
/// Returns true if spawned successfully, false if already running or spawn failed.
fn do_spawn() -> bool {
    if is_relay_worker_running() {
        return false;
    }

    let binary = match std::env::current_exe() {
        Ok(b) => b,
        Err(_) => return false,
    };

    let mut cmd = Command::new(&binary);
    cmd.arg("relay-worker")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Detach into own session so it survives parent terminal close (no SIGHUP)
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    match cmd.spawn() {
        Ok(child) => {
            log::log_info(
                "relay",
                "relay_worker.spawned",
                &format!("pid={}", child.id()),
            );
            true
        }
        Err(e) => {
            log::log_warn("relay", "relay_worker.spawn_err", &format!("{}", e));
            false
        }
    }
}

/// Ensure the relay worker is running.
///
/// `require_instances` — if true, only spawn when active local instances exist
/// (auto-spawn from hooks/send/TUI: no-op when nothing to sync). Fire-and-forget:
/// no readiness wait, events push on the worker's next cycle.
///
/// If false, spawns whenever relay is enabled (relay connect/new/on, daemon start).
/// On the explicit command path, polls until the notify port is live (max 500ms)
/// even when the worker was already running, to handle the startup window before
/// port bind.
///
/// Returns true if the worker is running (and port-ready when require_instances=false).
pub fn ensure_worker(require_instances: bool) -> bool {
    let config = match HcomConfig::load(None) {
        Ok(c) => c,
        Err(_) => return false,
    };

    if !super::is_relay_enabled(&config) {
        return false;
    }

    if require_instances {
        // Auto-spawn path: fire-and-forget, no readiness check.
        if is_relay_worker_running() {
            return true;
        }
        let db = match HcomDb::open() {
            Ok(db) => db,
            Err(_) => return false,
        };
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM instances \
                 WHERE COALESCE(origin_device_id, '') = '' \
                 AND status NOT IN ('stopped', 'dead')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if count == 0 {
            return false;
        }
        return do_spawn();
    }

    // Explicit command path: ensure running AND port-ready.
    if is_relay_worker_running() {
        // Process exists but may be in startup window before port bind.
        return poll_until_ready(300);
    }
    if !do_spawn() {
        // TOCTOU: another process may have spawned between our check and do_spawn().
        if is_relay_worker_running() {
            return poll_until_ready(300);
        }
        return false;
    }
    poll_until_ready(500)
}

/// Spawn the relay worker if relay is enabled and not already running.
/// Fire-and-forget: no instance check, no readiness wait.
/// Used by trigger_push() when no daemon is running, so events push on the
/// worker's first cycle instead of sitting in the DB indefinitely.
pub fn try_spawn_worker() {
    let config = match HcomConfig::load(None) {
        Ok(c) => c,
        Err(_) => return,
    };
    if super::is_relay_enabled(&config) {
        do_spawn();
    }
}

/// Poll until the worker's TCP notify port is in KV and accepting connections.
/// Opens DB once before the loop to avoid repeated open overhead.
/// Returns true if ready within timeout_ms, false on timeout.
fn poll_until_ready(timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_millis(timeout_ms);
    let db = HcomDb::open().ok();

    while start.elapsed() < deadline {
        if let Some(ref db) = db {
            if let Some(port_str) = super::safe_kv_get(db, "relay_daemon_port") {
                if let Ok(port) = port_str.trim().parse::<u16>() {
                    use std::net::{SocketAddr, TcpStream};
                    let addr = SocketAddr::from(([127, 0, 0, 1], port));
                    if TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(50))
                        .is_ok()
                    {
                        return true;
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    false
}

/// Return the PID of the running relay worker, or None if not running.
pub fn relay_worker_pid() -> Option<u32> {
    read_pid_file()
}

/// Remove the relay worker PID file (for post-SIGKILL cleanup).
pub fn remove_relay_pid_file() {
    remove_pid_file();
}

/// Stop a running relay-worker by sending SIGTERM to the PID from PID file.
pub fn stop_relay_worker() -> bool {
    if let Some(pid) = read_pid_file() {
        // SAFETY: Sending SIGTERM to a known PID.
        let ret = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if ret == 0 {
            log::log_info("relay", "relay_worker.stopped", &format!("pid={}", pid));
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_file_path() {
        crate::config::Config::init();
        let path = pid_file_path();
        assert!(path.to_string_lossy().contains("relay.pid"));
    }
}
