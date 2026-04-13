//! Relay MQTT roundtrip integration test.
//!
//! Two real hcom instances (separate HCOM_DIR), each with their own
//! daemon, talking through a real public MQTT broker.
//! Zero mocking, zero fake payloads.
//!
//! Phases:
//! 1. Device A: hcom relay new → daemon connects to broker
//! 2. Device A: hcom send → event pushed to broker
//! 3. Device B: hcom relay connect <token> → daemon connects, pulls
//! 4. Verify: Device B sees Device A's event in hcom events (namespaced)
//! 5. Verify: Device A sees Device B as remote device in relay status
//! 6. Verify: Device B → Device A relay also works
//! 7. Device A: real remote launch on Device B via RPC
//! 8. Device A: real remote kill of that launched process via RPC
//! 9. Cleanup: relay off, daemon stop, remove temp dirs
//!
//! Requires:
//! - hcom installed
//! - Network access to public MQTT brokers
//! - tmux installed
//! - claude installed and previously launched in /tmp (so the permission
//!   prompt is already approved — a fresh cwd blocks the TUI from drawing)
//!
//! Run:
//!     cargo test -p hcom --test test_relay_roundtrip -- --ignored --nocapture

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

// ── Logging ────────────────────────────────────────────────────────────

macro_rules! logln {
    ($log:expr, $($arg:tt)*) => {{
        let _msg = format!($($arg)*);
        println!("[{:.1?}] {}", $log.start.elapsed(), _msg);
        $log.log(&_msg);
    }};
}

struct TestLog {
    timestamped: PathBuf,
    latest: PathBuf,
    pub start: Instant,
}

impl TestLog {
    fn new() -> Self {
        let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-logs");
        fs::create_dir_all(&log_dir).ok();

        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let timestamped = log_dir.join(format!("relay_roundtrip_{ts}.log"));
        let latest = log_dir.join("test_relay_roundtrip.latest.log");

        let start = Instant::now();
        let header = format!(
            "[{}] Relay roundtrip test\nlog: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            timestamped.display(),
        );
        for path in [&timestamped, &latest] {
            let _ = fs::write(path, &header);
        }
        println!("{header}");

        TestLog {
            timestamped,
            latest,
            start,
        }
    }

    fn log(&self, text: &str) {
        let elapsed = self.start.elapsed();
        for path in [&self.timestamped, &self.latest] {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "[{elapsed:.1?}] {text}");
            }
        }
    }
}

impl Drop for TestLog {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        if std::thread::panicking() {
            self.log(&format!("TEST FAILED after {elapsed:.1?}"));
            println!("log: {}", self.latest.display());
        } else {
            self.log(&format!("TEST COMPLETE after {elapsed:.1?}"));
            println!("log: {}", self.latest.display());
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

fn hcom_with_dir(cmd: &str, hcom_dir: &str) -> Output {
    let mut command = Command::new("hcom");
    command
        .args(shell_words::split(cmd).unwrap())
        .env("HCOM_DIR", hcom_dir)
        .env("HCOM_DEV_ROOT", env!("CARGO_MANIFEST_DIR"))
        // Pin the default terminal to detached tmux. Not every RPC carries
        // an explicit `terminal` param (resume doesn't, for example), and
        // without this override the daemon falls back to env-detecting the
        // outer terminal — which in a typical dev loop (running tests from
        // kitty) produces a visible "kitty-split" popup. Matches what
        // test_pty_delivery.rs does for the same reason.
        .env("HCOM_TERMINAL", "tmux")
        // Keep claude cheap for the whole test: Haiku, every launch and
        // every resume. merge_tool_args in launcher.rs picks this up and
        // folds it into the final claude argv, so the resume path (which
        // doesn't take `--model` as a trailing arg cleanly) still honors it.
        .env("HCOM_CLAUDE_ARGS", "--model haiku");

    // Hermetic: strip identity/tag so launched instances keep their base
    // name (e.g. "nano", not "review-d-nano" when the outer agent is tagged
    // "review-d").
    for var in [
        "HCOM_TAG",
        "HCOM_INSTANCE_NAME",
        "HCOM_NAME",
        "HCOM_PROCESS_ID",
        "HCOM_LAUNCHED",
        "HCOM_LAUNCHED_BY",
        "HCOM_LAUNCH_BATCH_ID",
    ] {
        command.env_remove(var);
    }

    // Hermetic: strip outer terminal identity so the remote daemon doesn't
    // auto-detect the outer terminal (e.g. kitty) and fall back to a visible
    // preset (kitty-split, wezterm-split, tmux-split) when an RPC doesn't
    // carry an explicit `terminal` param. Resume in particular omits it, and
    // without this the daemon would briefly open a kitty pane on screen.
    // Kept in sync with terminal.rs::TERMINAL_CONTEXT_VARS.
    for var in [
        // Multiplexers
        "CMUX_WORKSPACE_ID",
        "CMUX_SURFACE_ID",
        "TMUX",
        "TMUX_PANE",
        "ZELLIJ_PANE_ID",
        "ZELLIJ_SESSION_NAME",
        // GPU/rich terminals
        "KITTY_WINDOW_ID",
        "KITTY_PID",
        "KITTY_LISTEN_ON",
        "WEZTERM_PANE",
        "WAVETERM_BLOCKID",
        // Bare terminal emulators
        "GHOSTTY_RESOURCES_DIR",
        "ITERM_SESSION_ID",
        "ALACRITTY_WINDOW_ID",
        "GNOME_TERMINAL_SCREEN",
        "KONSOLE_DBUS_WINDOW",
        "TERMINATOR_UUID",
        "TILIX_ID",
        "WT_SESSION",
        // Generic terminal identity
        "TERM_PROGRAM",
        "TERM_SESSION_ID",
        "COLORTERM",
    ] {
        command.env_remove(var);
    }

    command.output().expect("failed to execute hcom")
}

fn check(label: &str, cmd: &str, hcom_dir: &str) -> String {
    let out = hcom_with_dir(cmd, hcom_dir);
    assert!(
        out.status.success(),
        "Device {label}: hcom {cmd}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn poll_until<T>(
    mut f: impl FnMut() -> Option<T>,
    description: &str,
    timeout: Duration,
    interval: Duration,
) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            start.elapsed() < timeout,
            "Timeout ({timeout:?}): {description}"
        );
        thread::sleep(interval);
    }
}

fn parse_token(output: &str) -> Option<String> {
    // Token is the last word on any line containing "relay connect"
    for line in output.lines() {
        if line.contains("relay connect ") {
            return line.split_whitespace().last().map(|s| s.to_string());
        }
    }
    None
}

fn parse_device_id(status_output: &str) -> Option<String> {
    for line in status_output.lines() {
        if let Some(rest) = line.strip_prefix("Device:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn read_device_uuid(hcom_dir: &str) -> Option<String> {
    let path = Path::new(hcom_dir).join(".tmp/device_id");
    fs::read_to_string(path).ok().and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn parse_names(output: &str) -> Vec<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("Names: "))
        .map(|line| {
            line.split(", ")
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn tool_installed(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

fn list_instances(hcom_dir: &str) -> Vec<serde_json::Value> {
    let out = hcom_with_dir("list --json", hcom_dir);
    assert!(
        out.status.success(),
        "hcom list --json failed for {hcom_dir}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).expect("list --json must return JSON")
}

fn has_instance(hcom_dir: &str, name: &str) -> bool {
    list_instances(hcom_dir)
        .iter()
        .any(|inst| inst["name"].as_str() == Some(name))
}

fn find_instance_by_base(hcom_dir: &str, base: &str) -> Option<serde_json::Value> {
    list_instances(hcom_dir).into_iter().find(|inst| {
        inst["base_name"].as_str() == Some(base) || inst["name"].as_str() == Some(base)
    })
}

/// Poll a device's events for a recent rpc_result with the given action.
/// Returns the `data` object: `{request_id, action, ok, result}`.
fn poll_rpc_result_on_device(hcom_dir: &str, action: &str) -> serde_json::Value {
    poll_until(
        || {
            let out = hcom_with_dir("events --last 30", hcom_dir);
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let line = line.trim();
                if let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) {
                    if ev["type"].as_str() != Some("rpc_result") {
                        continue;
                    }
                    let data = &ev["data"];
                    if data["action"].as_str() == Some(action) {
                        return Some(data.clone());
                    }
                }
            }
            None
        },
        &format!("rpc_result(action={action}) on {hcom_dir}"),
        Duration::from_secs(15),
        Duration::from_millis(500),
    )
}

/// Claude's input prompt marker — present whenever the TUI is rendered,
/// independent of the dontAsk / accept-edits mode that hides the
/// "? for shortcuts" status bar.
const CLAUDE_PROMPT_MARKER: &str = "❯";

fn get_screen_local_json(hcom_dir: &str, name: &str) -> Option<serde_json::Value> {
    let out = hcom_with_dir(&format!("term {name} --json"), hcom_dir);
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn screen_lines_joined(screen: &serde_json::Value) -> String {
    screen["lines"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Poll for the lifecycle `life action=ready` event for any instance whose
/// name matches one of `names`. This is the canonical "agent is alive and
/// bound" signal — fires after claude's hooks first contact the daemon,
/// regardless of TUI ready_pattern visibility. Accepting multiple names
/// lets the caller pass both the base name and the tagged full name,
/// since different paths register events under different keys.
fn wait_for_ready_event_any(
    hcom_dir: &str,
    names: &[&str],
    after_id: i64,
    timeout: Duration,
) -> i64 {
    let start = Instant::now();
    let mut last_diag = Instant::now();
    loop {
        // Pull a broad window and filter locally — --agent on the CLI does
        // a substring match that can over- or under-include depending on
        // tag/base shape. We trust the explicit name check below.
        let out = hcom_with_dir("events --action ready --last 50", hcom_dir);
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let ev: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = ev["id"].as_i64().unwrap_or(0);
                if id <= after_id {
                    continue;
                }
                if ev["type"].as_str() != Some("life")
                    || ev["data"]["action"].as_str() != Some("ready")
                {
                    continue;
                }
                let ev_name = ev["instance"].as_str().unwrap_or("");
                if names.iter().any(|n| *n == ev_name) {
                    return id;
                }
            }
        }
        assert!(
            start.elapsed() < timeout,
            "Timeout ({timeout:?}): life action=ready event for any of {names:?}"
        );
        if last_diag.elapsed() >= Duration::from_secs(15) {
            last_diag = Instant::now();
            let out = hcom_with_dir("events --action ready --last 10", hcom_dir);
            let tail = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .map(|v| {
                    format!(
                        "id={} instance={:?}",
                        v["id"].as_i64().unwrap_or(0),
                        v["instance"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "    waiting ready event for {names:?} ({}s); recent ready events: {tail}",
                start.elapsed().as_secs()
            );
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Convenience: wait for a ready event for a single name.
fn wait_for_ready_event(hcom_dir: &str, name: &str, after_id: i64, timeout: Duration) -> i64 {
    wait_for_ready_event_any(hcom_dir, &[name], after_id, timeout)
}

/// After the lifecycle ready event, the inject port is registered and the
/// PTY screen is drivable. Confirm the TUI rendered: prompt marker present
/// and prompt_empty=true. Returns the screen JSON.
fn wait_for_screen_drawn(hcom_dir: &str, name: &str, timeout: Duration) -> serde_json::Value {
    poll_until(
        || {
            let s = get_screen_local_json(hcom_dir, name)?;
            let has_prompt = screen_lines_joined(&s).contains(CLAUDE_PROMPT_MARKER);
            let prompt_empty = s["prompt_empty"].as_bool() == Some(true);
            if has_prompt && prompt_empty {
                Some(s)
            } else {
                None
            }
        },
        &format!("claude TUI drawn for '{name}' (prompt marker + empty input)"),
        timeout,
        Duration::from_secs(1),
    )
}

/// Returns the highest event id currently visible on a device, for use as
/// `after_id` in `wait_for_ready_event`.
fn last_event_id(hcom_dir: &str) -> i64 {
    let out = hcom_with_dir("events --last 1", hcom_dir);
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter_map(|v| v["id"].as_i64())
        .next_back()
        .unwrap_or(0)
}

fn try_remote_launch_claude_tmux(
    hcom_dir: &str,
    target_device: &str,
) -> Result<(String, String), String> {
    // Model pinning comes via HCOM_CLAUDE_ARGS set in hcom_with_dir.
    // --dir is required for remote launches; use /tmp as cwd — it always
    // exists on both sides of the local-machine test and claude has already
    // been trusted there (a never-seen-before cwd triggers a permission
    // prompt that blocks the TUI from drawing).
    let cmd = format!("1 claude --device {target_device} --terminal tmux --dir /tmp --go");
    let out = hcom_with_dir(&cmd, hcom_dir);
    if !out.status.success() {
        return Err(format!(
            "hcom {cmd}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let names = parse_names(&stdout);
    let launched = names
        .into_iter()
        .next()
        .ok_or_else(|| format!("Could not parse launched instance name from:\n{stdout}"))?;
    Ok((launched, stdout))
}

/// Device A has no *local* instances (the one we launched is on Device B
/// and appears as an origin_device_id-tagged mirror row). The relay
/// worker's auto-exit watchdog checks every 30s and shuts the worker down
/// after 2 consecutive empty checks — so Device A's worker dies ~60s
/// after Phase 7, right before we need it for the long Phase 10 / 14
/// polling. Re-arm it before each long-running RPC on Device A.
fn ensure_relay_worker(hcom_dir: &str) {
    let out = hcom_with_dir("relay on", hcom_dir);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        eprintln!("    WARN: relay on failed on {hcom_dir}: {stdout} {stderr}");
    }
    // Brief settle so the daemon is actually listening before the next RPC.
    thread::sleep(Duration::from_millis(500));
}

/// Kill orphan debug relay-worker processes from previous failed test runs.
/// Without this, a stale daemon can hold MQTT connections and interfere with
/// new test runs (the test creates isolated HCOM_DIRs but can't find orphan
/// PIDs once the old temp dir is deleted).
fn kill_orphan_debug_daemons() {
    let Ok(output) = std::process::Command::new("pgrep")
        .args(["-f", "target/debug/hcom relay-worker"])
        .output()
    else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(pid) = line.trim().parse::<i32>() {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn kill_daemon(hcom_dir: &str) {
    let pid_path = Path::new(hcom_dir).join(".tmp").join("relay.pid");
    if let Ok(content) = fs::read_to_string(&pid_path) {
        if let Ok(pid) = content.trim().parse::<i32>() {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            // Wait up to 3s
            for _ in 0..30 {
                thread::sleep(Duration::from_millis(100));
                if unsafe { libc::kill(pid, 0) } != 0 {
                    return;
                }
            }
            // Still alive — SIGKILL
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

// ── Cleanup guard ──────────────────────────────────────────────────────

struct RelayGuard {
    dir_a: Option<PathBuf>,
    dir_b: Option<PathBuf>,
    local_kill_b: RefCell<Vec<String>>,
}

impl RelayGuard {
    fn register_local_b(&self, name: impl Into<String>) {
        self.local_kill_b.borrow_mut().push(name.into());
    }
}

impl Drop for RelayGuard {
    fn drop(&mut self) {
        if let Some(dir_b) = &self.dir_b {
            let d_str = dir_b.to_string_lossy();
            for name in self.local_kill_b.borrow().iter() {
                let _ = hcom_with_dir(&format!("kill {name}"), &d_str);
            }
        }
        for d in [&self.dir_a, &self.dir_b].into_iter().flatten() {
            let d_str = d.to_string_lossy();
            let _ = hcom_with_dir("relay off", &d_str);
            let _ = hcom_with_dir("relay daemon stop", &d_str);
            kill_daemon(&d_str);
            let _ = fs::remove_dir_all(d);
        }
    }
}

// ── Main test ──────────────────────────────────────────────────────────

#[test]
#[ignore]
fn test_relay_roundtrip() {
    kill_orphan_debug_daemons();

    let dir_a = tempfile::tempdir().expect("failed to create temp dir A");
    let dir_b = tempfile::tempdir().expect("failed to create temp dir B");

    // Prevent tempfile from auto-deleting — we manage cleanup via guard
    let dir_a_path = dir_a.keep();
    let dir_b_path = dir_b.keep();

    let guard = RelayGuard {
        dir_a: Some(dir_a_path.clone()),
        dir_b: Some(dir_b_path.clone()),
        local_kill_b: RefCell::new(Vec::new()),
    };

    let path_a = dir_a_path.to_string_lossy().to_string();
    let path_b = dir_b_path.to_string_lossy().to_string();

    let log = TestLog::new();

    logln!(log, "{}", "=".repeat(60));
    logln!(log, "Relay Roundtrip: two real hcom instances via MQTT");
    logln!(log, "{}", "=".repeat(60));
    logln!(log, "\n  Device A: {path_a}");
    logln!(log, "  Device B: {path_b}");

    // ── Phase 1: Device A creates relay group ────────────────────
    logln!(log, "\n[Phase 1] Device A: relay new...");

    let output = check("A", "relay new", &path_a);
    logln!(log, "{}", output.trim_end());

    let token = parse_token(&output).expect("Could not parse token from relay new output");
    logln!(log, "  OK: Token: {}...", &token[..token.len().min(24)]);

    // relay new auto-starts the daemon via ensure_worker; wait for it to connect
    // Wait for connected
    poll_until(
        || {
            let out = hcom_with_dir("relay status", &path_a);
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
                if stdout.contains("connected") {
                    return Some(());
                }
            }
            None
        },
        "Device A relay connected",
        Duration::from_secs(20),
        Duration::from_secs(1),
    );
    logln!(log, "  OK: Device A connected to broker");

    let status_a =
        String::from_utf8_lossy(&hcom_with_dir("relay status", &path_a).stdout).to_string();
    let short_a = parse_device_id(&status_a).expect("Could not parse Device A short ID");
    logln!(log, "  OK: Device A short ID: {short_a}");

    // ── Phase 2: Device A sends test message ─────────────────────
    logln!(log, "\n[Phase 2] Device A: sending test message...");

    let marker = format!("relay-rt-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    check(
        "A",
        &format!("send --from relaytest -- \"{marker}\""),
        &path_a,
    );
    logln!(log, "  OK: Sent: {marker}");

    poll_until(
        || {
            let out = hcom_with_dir("relay status", &path_a);
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let lower = stdout.to_lowercase();
                if lower.contains("up to date") {
                    return Some(());
                }
                eprintln!(
                    "    relay status: {}",
                    stdout
                        .lines()
                        .find(|l| l.to_lowercase().contains("queue"))
                        .unwrap_or("(no queue line)")
                );
            }
            None
        },
        "Device A push queue drained",
        Duration::from_secs(30),
        Duration::from_secs(2),
    );
    logln!(log, "  OK: Device A: pushed to broker");

    // ── Phase 3: Device B joins ──────────────────────────────────
    logln!(log, "\n[Phase 3] Device B: relay connect...");

    let output = check("B", &format!("relay connect {token}"), &path_b);
    logln!(log, "{}", output.trim_end());

    // relay connect auto-starts the daemon via ensure_worker; wait for it to connect
    poll_until(
        || {
            let out = hcom_with_dir("relay status", &path_b);
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
                if stdout.contains("connected") {
                    return Some(());
                }
            }
            None
        },
        "Device B relay connected",
        Duration::from_secs(20),
        Duration::from_secs(1),
    );
    logln!(log, "  OK: Device B connected to broker");

    // ── Phase 4: Device B sees relayed event ─────────────────────
    logln!(log, "\n[Phase 4] Device B: checking for relayed event...");

    let (ev, data) = poll_until(
        || {
            let out = hcom_with_dir("events --last 50", &path_b);
            if !out.status.success() {
                eprintln!(
                    "    events cmd failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line_count = stdout.lines().count();
            if line_count > 0 {
                eprintln!("    Device B has {line_count} events");
            }
            for line in stdout.lines() {
                let line = line.trim();
                if let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) {
                    let mut data = ev["data"].clone();
                    // data may be string-encoded JSON
                    if let Some(s) = data.as_str() {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                            data = parsed;
                        }
                    }
                    if let Some(text) = data["text"].as_str() {
                        if text.contains(&marker) {
                            return Some((ev, data));
                        }
                    }
                }
            }
            None
        },
        &format!("Device B sees '{marker}'"),
        Duration::from_secs(30),
        Duration::from_secs(2),
    );
    logln!(log, "  OK: Event received: type={}", ev["type"]);

    // Verify sender namespaced with Device A's short ID
    let expected_from = format!("relaytest:{short_a}");
    let actual_from = data["from"].as_str().unwrap_or("");
    assert_eq!(
        actual_from, expected_from,
        "from={actual_from}, expected {expected_from}"
    );
    logln!(log, "  OK: from namespaced: {actual_from}");

    // Verify _relay marker points back to Device A
    let actual_uuid_a = read_device_uuid(&path_a).expect("Could not read Device A UUID");
    let relay_marker = &data["_relay"];
    assert_eq!(
        relay_marker["device"].as_str().unwrap_or(""),
        actual_uuid_a,
        "_relay.device={}, expected {actual_uuid_a}",
        relay_marker["device"]
    );
    logln!(
        log,
        "  OK: _relay.device = Device A ({}...)",
        &actual_uuid_a[..actual_uuid_a.len().min(8)]
    );

    assert_eq!(
        relay_marker["short"].as_str().unwrap_or(""),
        short_a,
        "_relay.short={}, expected {short_a}",
        relay_marker["short"]
    );
    logln!(log, "  OK: _relay.short = {short_a}");

    // ── Phase 5: Device A sees Device B as remote ────────────────
    logln!(
        log,
        "\n[Phase 5] Device A: checking for Device B as remote..."
    );

    let status_b =
        String::from_utf8_lossy(&hcom_with_dir("relay status", &path_b).stdout).to_string();
    let short_b = parse_device_id(&status_b).expect("Could not parse Device B short ID");
    logln!(log, "  OK: Device B short ID: {short_b}");

    let remote_line: String = poll_until(
        || {
            let out = hcom_with_dir("relay status", &path_a);
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if (line.contains("online:") || line.contains("Remote devices:"))
                    && line.contains(&short_b)
                {
                    return Some(line.trim().to_string());
                }
            }
            let remote_line = stdout.lines().find(|l| {
                l.contains("online:") || l.contains("Remote") || l.contains("other devices")
            });
            eprintln!(
                "    relay status: {}",
                remote_line.unwrap_or("(no remote line)")
            );
            None
        },
        &format!("Device A sees {short_b} in remote devices"),
        Duration::from_secs(30),
        Duration::from_secs(2),
    );
    logln!(log, "  OK: {remote_line}");

    // ── Phase 6: Device B → Device A (bidirectional) ─────────────
    logln!(
        log,
        "\n[Phase 6] Device B: sending test message to Device A..."
    );

    let marker_b = format!("relay-rt-b-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    check(
        "B",
        &format!("send --from relaytest -- \"{marker_b}\""),
        &path_b,
    );
    logln!(log, "  OK: Sent: {marker_b}");

    poll_until(
        || {
            let out = hcom_with_dir("relay status", &path_b);
            if out.status.success() {
                let lower = String::from_utf8_lossy(&out.stdout).to_lowercase();
                if lower.contains("up to date") {
                    return Some(());
                }
            }
            None
        },
        "Device B push queue drained",
        Duration::from_secs(30),
        Duration::from_secs(2),
    );
    logln!(log, "  OK: Device B: pushed to broker");

    let actual_uuid_b = read_device_uuid(&path_b).expect("Could not read Device B UUID");
    let short_b_for_ns = short_b.clone();

    let (_, data_b) = poll_until(
        || {
            let out = hcom_with_dir("events --last 50", &path_a);
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if let Ok(ev) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    let mut data = ev["data"].clone();
                    if let Some(s) = data.as_str() {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                            data = parsed;
                        }
                    }
                    if data["text"]
                        .as_str()
                        .map(|t| t.contains(&marker_b))
                        .unwrap_or(false)
                    {
                        return Some((ev, data));
                    }
                }
            }
            None
        },
        &format!("Device A sees '{marker_b}'"),
        Duration::from_secs(30),
        Duration::from_secs(2),
    );
    logln!(log, "  OK: B→A event received");

    let expected_from_b = format!("relaytest:{short_b_for_ns}");
    let actual_from_b = data_b["from"].as_str().unwrap_or("");
    assert_eq!(
        actual_from_b, expected_from_b,
        "from={actual_from_b}, expected {expected_from_b}"
    );
    logln!(log, "  OK: from namespaced: {actual_from_b}");

    assert_eq!(
        data_b["_relay"]["device"].as_str().unwrap_or(""),
        actual_uuid_b,
        "_relay.device={}, expected {actual_uuid_b}",
        data_b["_relay"]["device"]
    );
    logln!(
        log,
        "  OK: _relay.device = Device B ({}...)",
        &actual_uuid_b[..actual_uuid_b.len().min(8)]
    );

    // ── Phase 7: Device A remotely launches on Device B ──────────
    logln!(log, "\n[Phase 7] Device A: remote launch on Device B...");

    assert!(
        tool_installed("claude"),
        "Phase 7 requires claude to be installed"
    );
    assert!(
        tool_installed("tmux"),
        "Phase 7 requires tmux to be installed"
    );

    let baseline_event_b = last_event_id(&path_b);
    let (launched_name, launch_output) = try_remote_launch_claude_tmux(&path_a, &short_b)
        .unwrap_or_else(|e| {
            panic!("Phase 7: remote launch failed: {e}");
        });
    logln!(log, "{}", launch_output.trim_end());
    logln!(
        log,
        "  OK: Remote launch succeeded with claude/tmux: {launched_name}"
    );
    guard.register_local_b(launched_name.clone());
    let launched_tool = "claude".to_string();
    let remote_name = format!("{launched_name}:{short_b}");

    poll_until(
        || has_instance(&path_b, &launched_name).then_some(()),
        &format!("Device B has launched local instance {launched_name}"),
        Duration::from_secs(30),
        Duration::from_secs(1),
    );
    logln!(
        log,
        "  OK: Device B lists local launched instance: {launched_name}"
    );

    poll_until(
        || has_instance(&path_a, &remote_name).then_some(()),
        &format!("Device A mirrors remote instance {remote_name}"),
        Duration::from_secs(30),
        Duration::from_secs(2),
    );
    logln!(
        log,
        "  OK: Device A mirrors launched remote instance: {remote_name}"
    );

    // Wait for the launched claude on Device B to actually be usable.
    // Without this, the rest of the phases race the tool's boot and see
    // "No inject port for ..." errors that silently get swallowed by weak
    // assertions. The lifecycle ready event is the canonical signal —
    // screen["ready"] is unreliable when the user has dontAsk mode on, but
    // the life event fires from hooks regardless.
    logln!(
        log,
        "  Waiting for claude lifecycle ready event on Device B..."
    );
    let _ready_event_id = wait_for_ready_event(
        &path_b,
        &launched_name,
        baseline_event_b,
        Duration::from_secs(90),
    );
    logln!(
        log,
        "  OK: Device B life action=ready event for {launched_name}"
    );
    let initial_screen = wait_for_screen_drawn(&path_b, &launched_name, Duration::from_secs(30));
    assert_eq!(initial_screen["prompt_empty"].as_bool(), Some(true));
    logln!(
        log,
        "  OK: claude TUI drawn (prompt marker '{CLAUDE_PROMPT_MARKER}' present, prompt empty)"
    );

    // ── Phase 8: term_screen on live instance ─────────────────────
    logln!(
        log,
        "\n[Phase 8] Device A: remote term_screen on Device B ({remote_name})..."
    );

    // Plain-text remote call: should print the formatted screen (containing
    // the claude ready banner). Fails closed if the RPC errored.
    let term_screen_out = hcom_with_dir(&format!("term {remote_name}"), &path_a);
    let term_screen_stdout = String::from_utf8_lossy(&term_screen_out.stdout).to_string();
    assert!(
        term_screen_out.status.success(),
        "remote term_screen CLI exited non-zero\nstdout: {term_screen_stdout}\nstderr: {}",
        String::from_utf8_lossy(&term_screen_out.stderr)
    );
    assert!(
        !term_screen_stdout.contains("Remote term screen failed"),
        "remote term_screen reported failure:\n{term_screen_stdout}"
    );
    assert!(
        term_screen_stdout.contains(CLAUDE_PROMPT_MARKER),
        "remote term_screen stdout missing claude prompt marker '{CLAUDE_PROMPT_MARKER}':\n{term_screen_stdout}"
    );
    logln!(
        log,
        "  OK: remote term_screen stdout contains claude prompt marker"
    );

    let rpc_screen = poll_rpc_result_on_device(&path_b, "term_screen");
    assert_eq!(
        rpc_screen["action"].as_str(),
        Some("term_screen"),
        "rpc_result action mismatch"
    );
    assert_eq!(
        rpc_screen["ok"].as_bool(),
        Some(true),
        "term_screen rpc not ok: {rpc_screen}"
    );
    let rpc_content = rpc_screen["result"]["content"].as_str().unwrap_or("");
    assert!(
        rpc_content.contains(CLAUDE_PROMPT_MARKER),
        "term_screen rpc_result.content missing claude prompt marker: {rpc_content}"
    );
    logln!(
        log,
        "  OK: term_screen rpc_result ok=true, content has prompt marker"
    );

    // ── Phase 9: term_inject on live instance ─────────────────────
    logln!(
        log,
        "\n[Phase 9] Device A: remote term_inject on Device B ({remote_name})..."
    );

    // Baseline: prompt should be empty on the claude home screen.
    let before =
        get_screen_local_json(&path_b, &launched_name).expect("local screen before inject");
    assert_eq!(
        before["prompt_empty"].as_bool(),
        Some(true),
        "prompt not empty before inject: {before}"
    );

    const INJECT_MARKER: &str = "relay-inject-marker";
    let inject_out = hcom_with_dir(
        &format!("term inject {remote_name} {INJECT_MARKER}"),
        &path_a,
    );
    let inject_stdout = String::from_utf8_lossy(&inject_out.stdout).to_string();
    assert!(
        inject_out.status.success(),
        "remote term_inject (text) exited non-zero\nstdout: {inject_stdout}\nstderr: {}",
        String::from_utf8_lossy(&inject_out.stderr)
    );
    assert!(
        !inject_stdout.contains("Remote term inject failed"),
        "remote term_inject reported failure:\n{inject_stdout}"
    );
    logln!(log, "  OK: remote term_inject (text) CLI succeeded");

    let rpc_inject_text = poll_rpc_result_on_device(&path_b, "term_inject");
    assert_eq!(
        rpc_inject_text["ok"].as_bool(),
        Some(true),
        "term_inject (text) rpc not ok: {rpc_inject_text}"
    );

    // Wait for the injected text to actually land in the claude input box.
    let screen_with_marker = poll_until(
        || {
            let s = get_screen_local_json(&path_b, &launched_name)?;
            let input = s["input_text"].as_str().unwrap_or("");
            if input.contains(INJECT_MARKER) {
                Some(s)
            } else {
                None
            }
        },
        "injected marker visible in claude input_text",
        Duration::from_secs(15),
        Duration::from_millis(500),
    );
    assert_eq!(
        screen_with_marker["prompt_empty"].as_bool(),
        Some(false),
        "prompt_empty should be false while marker sits in input"
    );
    logln!(
        log,
        "  OK: marker visible in input_text: {:?}",
        screen_with_marker["input_text"]
    );

    // Clear the input box by injecting Ctrl-U via a second inject with the
    // marker as its payload re-used (no-op) then --enter. Simpler: just
    // submit via --enter so the input_text clears. Phase 10 sends its own
    // real message separately via `hcom send`, so this enter only flushes
    // the marker and doesn't step on the test.
    let clear_out = hcom_with_dir(&format!("term inject {remote_name} --enter"), &path_a);
    assert!(clear_out.status.success());
    let rpc_inject_enter = poll_rpc_result_on_device(&path_b, "term_inject");
    assert_eq!(
        rpc_inject_enter["ok"].as_bool(),
        Some(true),
        "term_inject (enter) rpc not ok: {rpc_inject_enter}"
    );
    // Wait until claude consumed the marker out of the input line.
    poll_until(
        || {
            let s = get_screen_local_json(&path_b, &launched_name)?;
            let input = s["input_text"].as_str().unwrap_or("");
            if !input.contains(INJECT_MARKER) {
                Some(())
            } else {
                None
            }
        },
        "marker consumed from input_text after --enter",
        Duration::from_secs(15),
        Duration::from_millis(500),
    );
    logln!(
        log,
        "  OK: marker consumed from input after enter; both inject RPCs ok=true"
    );

    // ── Phase 10: real send+reply, then remote transcript ────────
    logln!(
        log,
        "\n[Phase 10] Device A: send real question and verify reply via transcript..."
    );

    let question = "Reply with exactly the single word PONG then stop.";
    let send_out = hcom_with_dir(
        &format!(
            "send @{launched_name}:{short_b} --from relaytest --intent request -- \"{question}\""
        ),
        &path_a,
    );
    assert!(
        send_out.status.success(),
        "hcom send failed: {}",
        String::from_utf8_lossy(&send_out.stderr)
    );
    logln!(log, "  OK: sent question to @{launched_name}:{short_b}");

    // Wait for the instance to process the message: status goes active
    // (while claude is thinking) then back to listening. We can't rely on
    // a reply message event — claude's hcom-send back to the sender fails
    // when the sender lives on another device (@short_id isn't a known
    // local agent on Device B). Instead we wait for the status round-trip
    // and then inspect the transcript, which the task brief specifically
    // allows as the OR path.
    poll_until(
        || {
            let out = hcom_with_dir(
                &format!("events --type status --agent {launched_name} --last 20"),
                &path_b,
            );
            if !out.status.success() {
                return None;
            }
            let mut saw_delivery = false;
            let mut saw_listening_after = false;
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let ev: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let data = &ev["data"];
                let ctx = data["context"].as_str().unwrap_or("");
                let status = data["status"].as_str().unwrap_or("");
                if ctx.starts_with("deliver:relaytest") {
                    saw_delivery = true;
                    continue;
                }
                if saw_delivery && status == "listening" {
                    saw_listening_after = true;
                }
            }
            if saw_delivery && saw_listening_after {
                Some(())
            } else {
                None
            }
        },
        &format!("{launched_name} processed message (delivery → listening)"),
        Duration::from_secs(120),
        Duration::from_secs(2),
    );
    logln!(
        log,
        "  OK: {launched_name} processed the message and returned to listening"
    );

    // Device A's worker has likely auto-exited during the long wait above
    // (watchdog exits after ~60s with no local instances). Re-arm before
    // the transcript RPC.
    ensure_relay_worker(&path_a);

    // Remote transcript: must contain PONG. Claude's response is in the
    // session JSONL, rendered by render_instance_transcript. We also verify
    // the incoming question landed via the deliver event above.
    let mut last_seen_len = 0usize;
    let tx_content: String = poll_until(
        || {
            let transcript_out = hcom_with_dir(
                &format!("transcript {remote_name} --last 5 --full"),
                &path_a,
            );
            if !transcript_out.status.success() {
                return None;
            }
            let rpc = poll_rpc_result_on_device(&path_b, "transcript");
            if rpc["ok"].as_bool() != Some(true) {
                return None;
            }
            let content = rpc["result"]["content"].as_str().unwrap_or("").to_string();
            if content.to_uppercase().contains("PONG") {
                return Some(content);
            }
            if content.len() != last_seen_len {
                last_seen_len = content.len();
                eprintln!("    transcript now {} bytes, no PONG yet", content.len());
            }
            None
        },
        "remote transcript contains PONG",
        Duration::from_secs(60),
        Duration::from_secs(2),
    );
    logln!(
        log,
        "  OK: remote transcript contains PONG reply ({} bytes). \
         Incoming question already verified via deliver:relaytest status event.",
        tx_content.len()
    );
    // The original question came from Device A via relay; it's recorded as
    // a delivered message event on Device B (verified by the deliver:relaytest
    // context assertion in the status round-trip above). Claude's session
    // JSONL reflects the assistant reply; combined, both sides are proven.

    // ── Phase 11: config_get on live instance ─────────────────────
    logln!(
        log,
        "\n[Phase 11] Device A: remote config_get on Device B ({remote_name})..."
    );

    ensure_relay_worker(&path_a);
    let config_get_out = hcom_with_dir(&format!("config -i {remote_name} --json"), &path_a);
    let config_get_stdout = String::from_utf8_lossy(&config_get_out.stdout).to_string();
    assert!(
        config_get_out.status.success(),
        "remote config_get CLI exited non-zero\nstdout: {config_get_stdout}\nstderr: {}",
        String::from_utf8_lossy(&config_get_out.stderr)
    );
    let config_json: serde_json::Value = serde_json::from_str(config_get_stdout.trim())
        .unwrap_or_else(|e| panic!("config_get stdout not JSON ({e}): {config_get_stdout}"));
    assert_eq!(
        config_json["name"].as_str(),
        Some(launched_name.as_str()),
        "config_get name mismatch: {config_json}"
    );
    assert_eq!(
        config_json["full_name"].as_str(),
        Some(launched_name.as_str()),
        "config_get full_name should equal base name (HCOM_TAG stripped): {config_json}"
    );
    assert!(
        config_json["tag"].is_null()
            || config_json["tag"].as_str().map(|s| s.is_empty()) == Some(true),
        "config_get tag should be null/empty: {config_json}"
    );
    assert!(
        config_json["timeout"].is_number(),
        "config_get timeout should be a number: {config_json}"
    );
    assert!(
        config_json["subagent_timeout"].is_null(),
        "config_get subagent_timeout should be null: {config_json}"
    );
    let rpc_config_get = poll_rpc_result_on_device(&path_b, "config_get");
    assert_eq!(
        rpc_config_get["ok"].as_bool(),
        Some(true),
        "config_get rpc not ok: {rpc_config_get}"
    );
    logln!(
        log,
        "  OK: config_get fields verified (name, full_name, tag, timeout, subagent_timeout)"
    );

    // ── Phase 12: config_set + verify persisted ───────────────────
    logln!(
        log,
        "\n[Phase 12] Device A: remote config_set tag on Device B ({remote_name})..."
    );

    let config_set_out = hcom_with_dir(
        &format!("config -i {remote_name} tag test-relay-tag"),
        &path_a,
    );
    let config_set_stdout = String::from_utf8_lossy(&config_set_out.stdout).to_string();
    assert!(
        config_set_out.status.success(),
        "remote config_set CLI exited non-zero\nstdout: {config_set_stdout}\nstderr: {}",
        String::from_utf8_lossy(&config_set_out.stderr)
    );
    let rpc_config_set = poll_rpc_result_on_device(&path_b, "config_set");
    assert_eq!(
        rpc_config_set["ok"].as_bool(),
        Some(true),
        "config_set rpc not ok: {rpc_config_set}"
    );

    // Re-fetch via RPC: tag should now be present.
    let refetch_out = hcom_with_dir(&format!("config -i {remote_name} --json"), &path_a);
    let refetch_stdout = String::from_utf8_lossy(&refetch_out.stdout).to_string();
    let refetch_json: serde_json::Value = serde_json::from_str(refetch_stdout.trim())
        .unwrap_or_else(|e| panic!("refetched config not JSON ({e}): {refetch_stdout}"));
    assert_eq!(
        refetch_json["tag"].as_str(),
        Some("test-relay-tag"),
        "tag not persisted in refetched config: {refetch_json}"
    );
    logln!(log, "  OK: refetched config has tag=test-relay-tag");

    // Double-check directly against Device B's SQLite DB.
    let db_path_b = Path::new(&path_b).join("hcom.db");
    let sql_out = Command::new("sqlite3")
        .arg(&db_path_b)
        .arg(format!(
            "SELECT tag FROM instances WHERE name='{launched_name}'"
        ))
        .output()
        .expect("failed to run sqlite3");
    assert!(
        sql_out.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&sql_out.stderr)
    );
    let sql_tag = String::from_utf8_lossy(&sql_out.stdout).trim().to_string();
    assert_eq!(
        sql_tag, "test-relay-tag",
        "Device B DB tag column != test-relay-tag: {sql_tag:?}"
    );
    logln!(log, "  OK: Device B DB tag column = test-relay-tag");

    // ── Phase 13: Device A remotely kills Device B instance ───────
    logln!(log, "\n[Phase 13] Device A: remote kill on Device B...");

    ensure_relay_worker(&path_a);
    let kill_output = check("A", &format!("kill {remote_name}"), &path_a);
    logln!(log, "{}", kill_output.trim_end());
    assert!(
        kill_output.contains("Sent SIGTERM")
            || kill_output.contains("already terminated")
            || kill_output.contains("already_dead"),
        "Unexpected remote kill output:\n{kill_output}"
    );
    logln!(log, "  OK: Remote kill RPC acknowledged for {remote_name}");

    poll_until(
        || (!has_instance(&path_b, &launched_name)).then_some(()),
        &format!("Device B clears killed instance {launched_name}"),
        Duration::from_secs(30),
        Duration::from_secs(1),
    );
    logln!(
        log,
        "  OK: Device B removed launched instance after remote kill"
    );

    poll_until(
        || (!has_instance(&path_a, &remote_name)).then_some(()),
        &format!("Device A clears mirrored remote instance {remote_name}"),
        Duration::from_secs(30),
        Duration::from_secs(2),
    );
    logln!(
        log,
        "  OK: Device A removed mirrored remote instance after kill"
    );

    // After the kill, a remote term_screen must fail (no inject port).
    let post_kill_term = hcom_with_dir(&format!("term {remote_name}"), &path_a);
    let post_kill_stdout = String::from_utf8_lossy(&post_kill_term.stdout).to_string();
    assert!(
        post_kill_stdout.contains("Remote term screen failed") || !post_kill_term.status.success(),
        "term_screen should fail after kill, got stdout:\n{post_kill_stdout}"
    );
    logln!(log, "  OK: term_screen after kill fails as expected");

    // ── Phase 14: resume the killed instance on Device B ──────────
    logln!(
        log,
        "\n[Phase 14] Device A: remote resume on Device B ({remote_name})..."
    );

    // Model pinned via HCOM_CLAUDE_ARGS in hcom_with_dir (haiku).
    ensure_relay_worker(&path_a);
    let resume_out = hcom_with_dir(&format!("r {remote_name}"), &path_a);
    let resume_stdout = String::from_utf8_lossy(&resume_out.stdout).to_string();
    let resume_stderr = String::from_utf8_lossy(&resume_out.stderr).to_string();
    logln!(
        log,
        "  resume stdout: {}",
        resume_stdout.lines().next().unwrap_or("(empty)")
    );
    if !resume_stderr.is_empty() {
        logln!(
            log,
            "  resume stderr: {}",
            resume_stderr.lines().next().unwrap_or("")
        );
    }

    // Register any spawned instances for cleanup BEFORE the poll/assertions,
    // using whatever names the CLI already printed. Resume can spawn a real
    // Claude agent in a detached tmux session — if a later poll or assertion
    // panics, the guard's Drop still needs to close those panes.
    for n in parse_names(&resume_stdout) {
        guard.register_local_b(n);
    }
    // Belt-and-suspenders: also kill the Phase 7 base name at teardown in
    // case the resume reuses it.
    guard.register_local_b(launched_name.clone());

    let rpc_resume = poll_rpc_result_on_device(&path_b, "resume");

    // Second pass: register any names the RPC handler itself returned that
    // weren't already visible in CLI stdout.
    if let Some(handles) = rpc_resume["result"]["handles"].as_array() {
        for h in handles {
            if let Some(name) = h.get("instance_name").and_then(|v| v.as_str()) {
                guard.register_local_b(name.to_string());
            }
        }
    }

    assert_eq!(
        rpc_resume["ok"].as_bool(),
        Some(true),
        "resume rpc not ok: {rpc_resume}"
    );
    let handles = rpc_resume["result"]["handles"]
        .as_array()
        .expect("resume result.handles missing");
    assert!(!handles.is_empty(), "resume returned zero handles");
    let resumed_name = handles[0]["instance_name"]
        .as_str()
        .expect("resume handle missing instance_name")
        .to_string();
    logln!(
        log,
        "  OK: resume rpc ok=true, resumed instance: {resumed_name}"
    );

    // Wait for the resumed instance to appear in Device B's list + reach
    // claude ready again (lifecycle event + drawn TUI). The resume RPC
    // returns as soon as it spawns the launch; the DB row registration
    // lags a few seconds while the daemon picks it up. Note: Phase 12 set
    // a tag on the instance — the resume inherits it, so the new full
    // name is `{tag}-{base}` rather than the plain base name. Match by
    // base_name to find it.
    let resumed_full_name: String = poll_until(
        || {
            find_instance_by_base(&path_b, &resumed_name)
                .and_then(|inst| inst["name"].as_str().map(|s| s.to_string()))
                .or_else(|| {
                    let list = hcom_with_dir("list --names", &path_b);
                    let names = String::from_utf8_lossy(&list.stdout).trim().to_string();
                    eprintln!(
                        "    waiting for resumed base='{resumed_name}', current names: {names:?}"
                    );
                    None
                })
        },
        &format!("Device B has resumed instance base='{resumed_name}'"),
        Duration::from_secs(60),
        Duration::from_secs(1),
    );
    logln!(
        log,
        "  OK: resumed instance present on Device B as '{resumed_full_name}' (base='{resumed_name}')"
    );
    guard.register_local_b(resumed_full_name.clone());

    let _resume_ready_id = wait_for_ready_event_any(
        &path_b,
        &[&resumed_full_name, &resumed_name],
        baseline_event_b,
        Duration::from_secs(90),
    );
    let resumed_screen =
        wait_for_screen_drawn(&path_b, &resumed_full_name, Duration::from_secs(30));
    logln!(
        log,
        "  OK: resumed instance PTY ready (life event + TUI drawn)"
    );

    // After a bootstrapped resume, claude either sees the [hcom:name]
    // marker injected into its first response/screen, OR the life event
    // log records a "bootstrap" action. Either way counts as proof the
    // resume actually rebooted claude, not just flipped a DB row.
    let screen_lines = screen_lines_joined(&resumed_screen);
    let screen_has_marker = screen_lines.contains("[hcom:");
    let events_have_bootstrap = {
        let out = hcom_with_dir(
            &format!("events --agent {resumed_full_name} --last 40"),
            &path_b,
        );
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        stdout.contains("bootstrap") || stdout.contains("[hcom:")
    };
    // Claude --resume reuses the existing session JSONL, so the original
    // [hcom:name] marker may already sit deep in claude's history rather
    // than being redrawn on screen. Pull a large transcript window and
    // look for it there.
    let transcript_has_marker = {
        ensure_relay_worker(&path_a);
        let out = hcom_with_dir(
            &format!("transcript {resumed_name}:{short_b} --last 50 --full"),
            &path_a,
        );
        if out.status.success() {
            let rpc = poll_rpc_result_on_device(&path_b, "transcript");
            rpc["result"]["content"]
                .as_str()
                .map(|s| s.contains("[hcom:"))
                .unwrap_or(false)
        } else {
            false
        }
    };
    // Last-resort evidence: hcom's hooks flip hooks_bound=true on first
    // daemon contact after a resume. If this is true, the rebind actually
    // happened even if the textual marker ended up somewhere we don't
    // scan.
    let hooks_bound = find_instance_by_base(&path_b, &resumed_name)
        .and_then(|inst| inst["hooks_bound"].as_bool())
        .unwrap_or(false);
    assert!(
        screen_has_marker || events_have_bootstrap || transcript_has_marker || hooks_bound,
        "no evidence of hcom rebind on resumed {resumed_full_name} \
         (screen_marker={screen_has_marker}, events={events_have_bootstrap}, \
          transcript_marker={transcript_has_marker}, hooks_bound={hooks_bound})"
    );
    logln!(
        log,
        "  OK: resumed instance is rebound to hcom (screen={screen_has_marker}, events={events_have_bootstrap}, transcript={transcript_has_marker}, hooks_bound={hooks_bound})"
    );

    // Cleanup handled by guard Drop
    logln!(log, "\n{}", "=".repeat(60));
    logln!(
        log,
        "ALL PHASES PASSED (including {launched_tool} remote launch/kill)"
    );
    logln!(log, "{}", "=".repeat(60));
}
