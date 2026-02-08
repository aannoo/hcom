//! Daemon lifecycle management and client entry point.
//!
//! Handles connecting to daemon, starting/stopping it, version checks,
//! and fallback to direct Python execution.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::log::{log_info, log_warn};

use super::connection::connect_with_timeout;
use super::protocol::{
    build_request, parse_listen_timeout, try_send, DaemonError, Response,
    ARGV_HOOKS, BLOCKING_HOOKS, LAUNCH_TOOLS, STDIN_HOOKS,
};

/// Client version from Cargo.toml - used to detect daemon version mismatch
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// Connection timeouts
const INITIAL_CONNECT_TIMEOUT_MS: u64 = 500;
const RETRY_CONNECT_TIMEOUT_MS: u64 = 200;
// Delays when we started the daemon ourselves
const DAEMON_START_RETRY_DELAYS_MS: [u64; 5] = [100, 200, 300, 400, 500];
// Longer delays when daemon is already starting (another client started it)
// Total: ~3s wait which covers daemon startup (~1.5s worst case)
const DAEMON_STARTING_RETRY_DELAYS_MS: [u64; 8] = [100, 200, 300, 400, 500, 500, 500, 500];

// Shutdown timeouts
const DAEMON_SHUTDOWN_POLL_INTERVAL_MS: u64 = 50;
const DAEMON_SHUTDOWN_MAX_POLLS: u32 = 100; // 50ms * 100 = 5s total

/// Check if this is a launch command: hcom [N] claude/gemini/codex
///
/// Launch commands should skip daemon and run Python directly because:
/// - count=1 needs os.execve() to run in current terminal (daemon can't execve)
/// - count>1 opens new windows anyway, no benefit from daemon
/// - Launches aren't performance-critical (happen once, spawn terminals)
fn is_launch_command(args: &[String]) -> bool {
    // Skip --name <value> prefix so we inspect the actual command
    let args = if args.len() >= 2 && args[0] == "--name" {
        &args[2..]
    } else {
        args
    };

    let first = match args.first() {
        Some(s) => s.as_str(),
        None => return false,
    };

    // --new-terminal opens a window and needs shell env (e.g. KITTY_LISTEN_ON)
    if first == "--new-terminal" {
        return true;
    }

    // Parse: [N] <tool> where N is optional count
    let idx = if first.parse::<u32>().is_ok() { 1 } else { 0 };

    // Check if next arg is a launch tool
    args.get(idx)
        .map(|s| LAUNCH_TOOLS.contains(&s.as_str()))
        .unwrap_or(false)
}

/// Run client mode - connect to daemon or fallback to Python.
pub fn run(args: &[String]) -> Result<()> {
    let run_start = Instant::now();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
    let is_hook = STDIN_HOOKS.contains(&cmd) || ARGV_HOOKS.contains(&cmd);
    let is_pty_mode = Config::get().pty_mode;

    log_info("client", "run.start", &format!(
        "cmd={} args_count={} pty_mode={}",
        cmd, args.len(), is_pty_mode
    ));

    // Blocking hooks can run for hours in vanilla mode (wait_timeout=86400).
    // Skip daemon for these - use Python directly to avoid timeout complexity.
    // Exception: PTY mode (HCOM_PTY_MODE=1) where poll exits immediately.
    if BLOCKING_HOOKS.contains(&cmd) && !is_pty_mode {
        log_info("client", "run.fallback", &format!(
            "reason=blocking_hook cmd={} pty_mode={}",
            cmd, is_pty_mode
        ));
        exec_python_fallback(args);
    }

    // listen with long timeout (>29min) bypasses daemon to avoid idle shutdown conflict
    // Daemon has 30min idle timeout; long listen would trigger it mid-request
    if cmd == "listen" {
        let timeout = parse_listen_timeout(args);
        if timeout > 29 * 60 {
            log_info("client", "run.fallback", &format!(
                "reason=long_listen timeout={}",
                timeout
            ));
            exec_python_fallback(args);
        }
    }

    // Launch commands skip daemon - Python handles terminal decisions directly.
    // count=1 needs execve (daemon can't), count>1 opens new windows anyway.
    if is_launch_command(args) {
        log_info("client", "run.fallback", "reason=launch_command");
        exec_python_fallback(args);
    }

    // "run" scripts can take minutes (ensemble, debate, confess launch agents and wait).
    // 30s default read timeout would kill them. Run Python directly like launch/listen.
    if cmd == "run" {
        log_info("client", "run.fallback", "reason=run_script");
        exec_python_fallback(args);
    }

    // daemon stop/restart skip daemon - the daemon will close connection during shutdown
    if args.first().map(|s| s.as_str()) == Some("daemon") {
        if let Some(subcmd) = args.get(1).map(|s| s.as_str()) {
            if subcmd == "stop" || subcmd == "restart" {
                log_info("client", "run.fallback", &format!("reason=daemon_{}", subcmd));
                exec_python_fallback(args);
            }
        }
    }

    let build_start = Instant::now();
    let request = build_request(args);
    let build_ms = build_start.elapsed().as_secs_f64() * 1000.0;
    let request_id = request.request_id.as_str();

    let sock_path = get_socket_path();

    log_info("client", "run.request", &format!(
        "request_id={} kind={} build={:.1}ms",
        request_id, request.kind, build_ms
    ));

    match try_daemon(&sock_path, &request) {
        Ok(response) => {
            let total_ms = run_start.elapsed().as_secs_f64() * 1000.0;
            log_info("client", "run.success", &format!(
                "request_id={} total={:.1}ms exit_code={} stdout_len={} stderr_len={}",
                request_id, total_ms, response.exit_code, response.stdout.len(), response.stderr.len()
            ));
            print!("{}", response.stdout);
            eprint!("{}", response.stderr);
            std::process::exit(response.exit_code);
        }
        Err(DaemonError::PermissionDenied(e)) => {
            let total_ms = run_start.elapsed().as_secs_f64() * 1000.0;
            log_info("client", "run.permission_denied_fallback", &format!(
                "request_id={} total={:.1}ms err={}",
                request_id, total_ms, e
            ));
            exec_python_fallback(args);
        }
        Err(e) => {
            let total_ms = run_start.elapsed().as_secs_f64() * 1000.0;
            let log_label = match &e {
                DaemonError::ConnectionFailed(_) => "run.connection_failed",
                DaemonError::ReadTimeout(_) => "run.timeout",
                DaemonError::Io { .. } => "run.io_error",
                DaemonError::Json { .. } => "run.json_error",
                _ => "run.error",
            };
            log_info("client", log_label, &format!(
                "request_id={} total={:.1}ms err={}",
                request_id, total_ms, e
            ));
            // Hooks must not print to stderr (corrupts JSON output)
            if !is_hook {
                let log_path = crate::paths::log_path();
                match &e {
                    DaemonError::ConnectionFailed(msg) => {
                        eprintln!("[hcom] Cannot connect to daemon: {}", msg);
                        eprintln!("[hcom] Check daemon log: {}", log_path.display());
                        eprintln!("[hcom] Try: hcom daemon restart");
                        eprintln!("[hcom] Or set HCOM_PYTHON_FALLBACK=1 to bypass");
                    }
                    DaemonError::ReadTimeout(msg) => {
                        eprintln!("[hcom] Daemon hung (timeout): {}", msg);
                        eprintln!("[hcom] Command may have partially executed - check results before retrying");
                        eprintln!("[hcom] If recurring, check daemon log: {}", log_path.display());
                    }
                    DaemonError::Io { source } => {
                        eprintln!("[hcom] Daemon I/O error: {}", source);
                        eprintln!("[hcom] Check daemon log: {}", log_path.display());
                    }
                    DaemonError::Json { source } => {
                        eprintln!("[hcom] Daemon JSON error: {}", source);
                        eprintln!("[hcom] Check daemon log: {}", log_path.display());
                    }
                    _ => {
                        eprintln!("[hcom] Daemon error: {}", e);
                        eprintln!("[hcom] Check daemon log: {}", log_path.display());
                    }
                }
            }
            std::process::exit(1);
        }
    }
}

/// Try to connect to daemon and send request.
/// Checks daemon version first - restarts daemon if version mismatch detected.
fn try_daemon(path: &Path, request: &super::protocol::Request) -> std::result::Result<Response, DaemonError> {
    let total_start = Instant::now();
    let request_id = request.request_id.as_str();

    // Check daemon version before connecting - restart if mismatch
    // This handles pip upgrades where daemon has old code loaded
    let version_start = Instant::now();
    let version_ok = check_daemon_version();
    let version_ms = version_start.elapsed().as_secs_f64() * 1000.0;

    if !version_ok {
        log_info("client", "try_daemon.version_mismatch", &format!(
            "request_id={} version_check={:.1}ms restarting_daemon=true",
            request_id, version_ms
        ));
        eprintln!("[hcom] Restarting daemon (version mismatch)");

        let stop_start = Instant::now();
        stop_daemon();
        let stop_ms = stop_start.elapsed().as_secs_f64() * 1000.0;

        let start_start = Instant::now();
        start_daemon();
        let start_ms = start_start.elapsed().as_secs_f64() * 1000.0;

        log_info("client", "try_daemon.restart", &format!(
            "request_id={} stop={:.1}ms start={:.1}ms",
            request_id, stop_ms, start_ms
        ));

        // Wait for new daemon to start
        for (i, delay) in DAEMON_START_RETRY_DELAYS_MS.iter().enumerate() {
            std::thread::sleep(Duration::from_millis(*delay));
            let connect_start = Instant::now();
            if let Ok(s) = connect_with_timeout(path, Duration::from_millis(RETRY_CONNECT_TIMEOUT_MS)) {
                // Verify version matches after restart — if not, don't send
                // (avoids restart loop where each hcom invocation kills the daemon again)
                if !check_daemon_version() {
                    let connect_ms = connect_start.elapsed().as_secs_f64() * 1000.0;
                    log_warn("client", "try_daemon.version_still_mismatched", &format!(
                        "request_id={} attempt={} connect={:.1}ms",
                        request_id, i + 1, connect_ms
                    ));
                    return Err(DaemonError::ConnectionFailed(
                        "Version mismatch persists after daemon restart. \
                         Rust binary and Python package versions are out of sync. \
                         Rebuild with: ./build.sh".to_string()
                    ));
                }
                let connect_ms = connect_start.elapsed().as_secs_f64() * 1000.0;
                log_info("client", "try_daemon.reconnect_success", &format!(
                    "request_id={} attempt={} connect={:.1}ms total_restart={:.1}ms",
                    request_id, i + 1, connect_ms, total_start.elapsed().as_secs_f64() * 1000.0
                ));
                return try_send(&s, request);
            }
        }
        let log_path = get_log_path();
        log_info("client", "try_daemon.reconnect_failed", &format!(
            "request_id={} total={:.1}ms",
            request_id, total_start.elapsed().as_secs_f64() * 1000.0
        ));
        return Err(DaemonError::ConnectionFailed(format!(
            "Failed to connect after version restart. Check daemon log: {}",
            log_path.display()
        )));
    }

    // Quick connect with short timeout - if daemon is dead, fail fast
    let connect_start = Instant::now();
    let stream = match connect_with_timeout(path, Duration::from_millis(INITIAL_CONNECT_TIMEOUT_MS)) {
        Ok(s) => {
            let connect_ms = connect_start.elapsed().as_secs_f64() * 1000.0;
            log_info("client", "try_daemon.connect", &format!(
                "request_id={} version_check={:.1}ms connect={:.1}ms",
                request_id, version_ms, connect_ms
            ));
            s
        }
        Err(e) => {
            let connect_ms = connect_start.elapsed().as_secs_f64() * 1000.0;

            // EPERM = sandbox blocking Unix sockets (e.g. Codex network_access:false).
            // Don't spawn a new daemon — it'll crash on bind() too, and kill the healthy one.
            // Fall back to Python direct execution instead.
            if e.raw_os_error() == Some(libc::EPERM) {
                log_info("client", "try_daemon.permission_denied", &format!(
                    "request_id={} version_check={:.1}ms connect={:.1}ms err={}",
                    request_id, version_ms, connect_ms, e
                ));
                return Err(DaemonError::PermissionDenied(format!(
                    "Socket connect blocked (sandbox?): {}", e
                )));
            }

            // Always try to start — Python flock makes this idempotent.
            // If a daemon is already starting, spawn exits immediately (lock blocked).
            // If a zombie holds the lock, Python kills it and takes over.
            log_info("client", "try_daemon.connect_failed", &format!(
                "request_id={} version_check={:.1}ms connect={:.1}ms err={} spawning=true",
                request_id, version_ms, connect_ms, e
            ));
            start_daemon();

            let start_ms = total_start.elapsed().as_secs_f64() * 1000.0;

            // Retry with delays covering daemon startup (~1.5s worst case)
            for (i, delay) in DAEMON_STARTING_RETRY_DELAYS_MS.iter().enumerate() {
                std::thread::sleep(Duration::from_millis(*delay));
                let retry_start = Instant::now();
                if let Ok(s) = connect_with_timeout(path, Duration::from_millis(RETRY_CONNECT_TIMEOUT_MS)) {
                    let retry_ms = retry_start.elapsed().as_secs_f64() * 1000.0;
                    log_info("client", "try_daemon.retry_success", &format!(
                        "request_id={} attempt={} elapsed={:.1}ms retry_connect={:.1}ms total={:.1}ms",
                        request_id, i + 1, start_ms, retry_ms, total_start.elapsed().as_secs_f64() * 1000.0
                    ));
                    return try_send(&s, request);
                }
            }
            let log_path = get_log_path();
            log_info("client", "try_daemon.all_retries_failed", &format!(
                "request_id={} elapsed={:.1}ms total={:.1}ms",
                request_id, start_ms, total_start.elapsed().as_secs_f64() * 1000.0
            ));
            return Err(DaemonError::ConnectionFailed(format!(
                "Failed to connect after retries. Check daemon log: {}",
                log_path.display()
            )));
        }
    };
    try_send(&stream, request)
}

/// Start daemon in background.
/// Uses HCOM_PYTHON env var if set, otherwise python3 (more portable than bare python).
/// Cleans stale socket/pid files only if daemon process is confirmed dead.
fn start_daemon() {
    let pid_path = crate::paths::pid_path();
    let socket_path = crate::paths::socket_path();

    // Only clean stale files if daemon process is confirmed dead.
    // Don't delete a live daemon's socket on transient connect failure.
    let daemon_alive = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .map(|pid| {
            // SAFETY: kill(pid, 0) checks process existence without sending a signal.
            // pid is from pidfile, validated as i32.
            let ret = unsafe { libc::kill(pid, 0) };
            ret == 0
        })
        .unwrap_or(false);

    if !daemon_alive {
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&pid_path);
    }

    let python = &Config::get().python;
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_path())
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());
    let _ = Command::new(python)
        .args(["-m", "hcom.daemon"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_file)
        .spawn();
}

/// Get socket path from centralized paths module.
fn get_socket_path() -> PathBuf {
    crate::paths::socket_path()
}

/// Get log path for error messages (daemon logs to unified hcom.log).
fn get_log_path() -> PathBuf {
    crate::paths::log_path()
}

/// Fall back to direct Python execution.
/// Uses HCOM_PYTHON env var if set, otherwise python3.
/// Sets HCOM_PYTHON_FALLBACK=1 to prevent Python from exec'ing back to Rust.
pub fn exec_python_fallback(args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let python = &Config::get().python;
    let err = Command::new(python)
        .env("HCOM_PYTHON_FALLBACK", "1")
        .args(["-m", "hcom"])
        .args(args)
        .exec();
    eprintln!("Failed to exec {}: {}", python, err);
    std::process::exit(1);
}

/// Check if running daemon version matches client version.
/// Returns true if versions match or version file doesn't exist (fresh start).
fn check_daemon_version() -> bool {
    let version_path = crate::paths::daemon_version_path();
    match std::fs::read_to_string(&version_path) {
        Ok(daemon_version) => daemon_version.trim() == CLIENT_VERSION,
        Err(_) => true, // No version file = fresh start, OK to proceed
    }
}

/// Stop running daemon by sending SIGTERM to PID and waiting for shutdown.
/// Waits for socket to disappear (daemon cleans up socket before releasing PID lock).
fn stop_daemon() {
    let pid_path = crate::paths::pid_path();
    let socket_path = crate::paths::socket_path();

    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            // SAFETY:
            // - PID validity: Read from pidfile and parsed as valid i32
            // - Stale/reused PID risk: Accepted for best-effort daemon stop. The daemon
            //   holds a file lock on the pidfile while running, but after crash/kill -9,
            //   the PID may be reused by another process. This is documented but accepted
            //   as the risk is low (would need exact PID reuse) and consequence is minor
            //   (SIGTERM to wrong process, which most processes ignore or handle gracefully).
            // - SIGTERM: Graceful shutdown signal, appropriate for daemon termination.
            //   Allows daemon to clean up resources (socket, locks) before exiting.
            // - Return value: Intentionally ignored (fire-and-forget semantics). We verify
            //   shutdown by polling below.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }

            // Poll PID for 5 seconds to check if process exits gracefully
            let mut process_alive = true;
            for _ in 0..DAEMON_SHUTDOWN_MAX_POLLS {
                std::thread::sleep(Duration::from_millis(DAEMON_SHUTDOWN_POLL_INTERVAL_MS));

                // Check if process is still alive using kill(pid, 0)
                // SAFETY: PID already validated, signal 0 doesn't send actual signal
                let result = unsafe { libc::kill(pid, 0) };
                if result == -1 {
                    // Process is dead (ESRCH error)
                    process_alive = false;
                    break;
                }
            }

            // Auto-escalate to SIGKILL if daemon didn't respond to SIGTERM
            if process_alive {
                log_warn("client", "stop_daemon.escalate_sigkill", &format!(
                    "Daemon (PID {}) did not respond to SIGTERM within 5s, sending SIGKILL",
                    pid
                ));
                // SAFETY:
                // - PID validity: Same PID from pidfile, already validated as i32
                // - Stale/reused PID risk: Same risk as SIGTERM above. The daemon was sent
                //   SIGTERM and given 5 seconds to shutdown gracefully. If it's still alive
                //   after 5s (verified by kill(pid, 0)), force kill is justified. PID reuse
                //   risk is mitigated by the 5s window and active process check.
                // - SIGKILL: Non-graceful immediate termination. Used automatically when
                //   daemon fails to respond to SIGTERM within 5s. Daemon will not clean up
                //   resources (socket, locks remain). Caller is responsible for cleanup via
                //   the stale file removal below.
                // - Return value: Intentionally ignored. Daemon may have exited between the
                //   liveness check and kill() call. Cleanup handles both cases.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    // Clean up any stale files
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(crate::paths::daemon_version_path());
}
