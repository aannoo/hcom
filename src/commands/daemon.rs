//! Relay daemon process management.
//!
//! Accessed via `hcom relay daemon [start|stop|restart|status]`.
//! Manages the `hcom relay-worker` background process for MQTT relay.

use std::thread;
use std::time::Duration;

pub(crate) fn daemon_status() -> i32 {
    match crate::relay::worker::relay_worker_pid() {
        Some(pid) => println!("Daemon: running (PID {pid})"),
        None => println!("Daemon not running"),
    }
    0
}

pub(crate) fn daemon_start() -> i32 {
    let was_running = crate::relay::worker::is_relay_worker_running();
    if crate::relay::worker::ensure_worker(false) {
        let pid = crate::relay::worker::relay_worker_pid();
        let pid_str = pid.map(|p| format!(" (PID {p})")).unwrap_or_default();
        if was_running {
            println!("Daemon already running{pid_str}");
        } else {
            println!("Daemon started{pid_str}");
        }
        0
    } else {
        // ensure_worker may have timed out on readiness — check if process actually started
        if crate::relay::worker::is_relay_worker_running() {
            let pid = crate::relay::worker::relay_worker_pid();
            let pid_str = pid.map(|p| format!(" (PID {p})")).unwrap_or_default();
            println!("Daemon started{pid_str} (notify port not yet ready)");
            0
        } else {
            eprintln!("Failed to start daemon (relay disabled or config error)");
            1
        }
    }
}

pub(crate) fn daemon_stop() -> i32 {
    let pid = match crate::relay::worker::relay_worker_pid() {
        Some(p) => p,
        None => {
            println!("Daemon not running");
            return 0;
        }
    };

    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    println!("Sent SIGTERM to daemon (PID {pid})");

    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        if !crate::pidtrack::is_alive(pid) {
            println!("Daemon stopped");
            crate::relay::worker::remove_relay_pid_file();
            return 0;
        }
    }

    println!("Daemon did not respond to SIGTERM, escalating to SIGKILL");
    let kill_ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    if kill_ret != 0 {
        eprintln!(
            "SIGKILL failed (errno {}), PID file retained",
            std::io::Error::last_os_error()
        );
        return 1;
    }
    println!("Daemon killed (SIGKILL)");
    crate::relay::worker::remove_relay_pid_file();
    0
}

pub fn cmd_daemon(argv: &[String]) -> i32 {
    let subcmd = argv.first().map(|s| s.as_str()).unwrap_or("status");

    match subcmd {
        "status" => daemon_status(),
        "start" => daemon_start(),
        "stop" => daemon_stop(),
        "restart" => {
            daemon_stop();
            thread::sleep(Duration::from_millis(500));
            daemon_start()
        }
        other => {
            eprintln!("Unknown daemon subcommand: {other}");
            eprintln!("Usage: hcom relay daemon [status|start|stop|restart]");
            1
        }
    }
}
