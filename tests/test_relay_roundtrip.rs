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
//! - cargo-built hcom test binary
//! - Network access to public MQTT brokers
//! - pinned claude installed; model calls are routed to a localhost mock
//!
//! Run:
//!     cargo test -p hcom --test test_relay_roundtrip -- --ignored --nocapture
//!
//! The harness uses platform-specific daemon cleanup where needed, but the
//! relay contract itself runs unchanged on Unix and Windows.

mod support;

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use support::claude_mock::{MODEL, claude_text, claude_tool_use, latest_user_turn};
use support::mock_http::{MockHttp, RecordedRequest, Reply};

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

fn hcom_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hcom"))
}

fn hcom_with_dir(cmd: &str, hcom_dir: &str) -> Output {
    let bin = hcom_bin();
    let mut command = Command::new(&bin);
    command
        .args(shell_words::split(cmd).unwrap())
        .env("HCOM_DIR", hcom_dir)
        .env("HCOM_DEV_ROOT", env!("CARGO_MANIFEST_DIR"))
        // Keep the relay test on the same deterministic localhost Claude mock
        // path as real_tool_claude.rs. merge_tool_args folds this into launch
        // and resume. Remote launch uses --headless, so no terminal emulator
        // (tmux/kitty/etc.) is required in CI.
        .env(
            "HCOM_CLAUDE_ARGS",
            format!("--model {MODEL} --permission-mode bypassPermissions --setting-sources user"),
        );

    let mut path_entries = Vec::new();
    if let Some(parent) = bin.parent() {
        path_entries.push(parent.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        path_entries.extend(std::env::split_paths(&path));
    }
    let path = std::env::join_paths(path_entries).expect("construct hcom test PATH");
    command.env("PATH", path);

    apply_env_passthrough(&mut command, hcom_dir);

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

    run_command_with_timeout(command, cmd, Duration::from_secs(90))
}

/// Capture through files rather than `Command::output()` pipes. On Windows a
/// detached relay worker can inherit the parent's anonymous pipe handles even
/// though its own stdio is null, preventing `output()` from ever observing EOF
/// after the short-lived CLI parent exits.
fn run_command_with_timeout(mut command: Command, label: &str, timeout: Duration) -> Output {
    let stdout_file = tempfile::tempfile().expect("create hcom stdout capture");
    let stderr_file = tempfile::tempfile().expect("create hcom stderr capture");
    command
        .stdout(Stdio::from(
            stdout_file.try_clone().expect("clone stdout capture"),
        ))
        .stderr(Stdio::from(
            stderr_file.try_clone().expect("clone stderr capture"),
        ));

    let mut child = command.spawn().expect("failed to execute hcom");
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = read_capture(&stdout_file);
                let stderr = read_capture(&stderr_file);
                panic!(
                    "hcom command timed out after {timeout:?}: {label}\n\
                     -- stdout --\n{}\n-- stderr --\n{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(error) => panic!("failed waiting for hcom command `{label}`: {error}"),
        }
    };

    Output {
        status,
        stdout: read_capture(&stdout_file),
        stderr: read_capture(&stderr_file),
    }
}

fn read_capture(file: &std::fs::File) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = file.try_clone().expect("clone command capture for reading");
    file.seek(SeekFrom::Start(0))
        .expect("rewind command capture");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).expect("read command capture");
    bytes
}

fn apply_env_passthrough(command: &mut Command, hcom_dir: &str) {
    let env_path = Path::new(hcom_dir).join("env");
    let Ok(content) = fs::read_to_string(env_path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.starts_with("HCOM_") {
            continue;
        }
        command.env(key, value.trim());
    }
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

/// Build a command for `tool` resolved against the real process `PATH`,
/// following npm's Windows `.cmd`/`.bat` shims that `CreateProcess` cannot
/// execute directly (mirrors `support::Hcom::external_cmd`, which resolves
/// against an isolated PATH instead of the real environment).
fn external_tool_command(tool: &str) -> Command {
    #[cfg(windows)]
    {
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        let resolved = std::env::split_paths(&path_var)
            .flat_map(|dir| {
                [".COM", ".EXE", ".BAT", ".CMD", ""]
                    .map(move |ext| dir.join(format!("{tool}{ext}")))
            })
            .find(|candidate| candidate.is_file());
        match resolved {
            Some(path)
                if matches!(
                    path.extension().and_then(std::ffi::OsStr::to_str),
                    Some(ext) if ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat")
                ) =>
            {
                let mut command = Command::new("cmd.exe");
                command.args(["/d", "/c"]).arg(path);
                command
            }
            Some(path) => Command::new(path),
            None => Command::new(tool),
        }
    }
    #[cfg(not(windows))]
    {
        Command::new(tool)
    }
}

fn assert_tool_pinned(tool: &str, expected_version: &str, install_hint: &str) {
    let output = external_tool_command(tool)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "Phase 7 requires {tool} {expected_version}, but `{tool} --version` failed: {e}. Install with: {install_hint}"
            )
        });
    assert!(
        output.status.success(),
        "Phase 7 requires {tool} {expected_version}; `{tool} --version` failed\nstdout: {}\nstderr: {}\nInstall with: {install_hint}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let version = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    assert!(
        version
            .split_whitespace()
            .any(|token| token.trim_start_matches('v') == expected_version),
        "Phase 7 requires {tool} {expected_version}, found `{}`. Install with: {install_hint}",
        version.trim()
    );
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

/// True if `text` has Claude's input prompt marker at the start of a rendered
/// screen line, present whenever the TUI is rendered, independent of the
/// dontAsk / accept-edits mode that hides the "? for shortcuts" status bar.
/// `--permission-mode bypassPermissions` (used by this test) renders a plain
/// `>` instead of the styled `❯`. Requiring the marker to lead the line (not
/// just appear anywhere) keeps this from matching an unrelated `>` in tips,
/// diffs, or other screen content.
fn screen_has_claude_prompt(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = strip_term_line_number_prefix(line).trim_start();
        trimmed.starts_with('❯') || trimmed.starts_with('>')
    })
}

/// Strip `hcom term`'s "  <N>: " row-index prefix (see `src/commands/term.rs`
/// `format!("  {i:3}: {text}")`), if present, so line-start checks work on
/// both `--json` line arrays (no prefix) and the default rendered output
/// (prefixed).
fn strip_term_line_number_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digits_end > 0 && trimmed[digits_end..].starts_with(": ") {
        &trimmed[digits_end + 2..]
    } else {
        line
    }
}

#[test]
fn screen_has_claude_prompt_matches_styled_and_ascii_markers() {
    assert!(screen_has_claude_prompt("❯ \n──────"));
    assert!(screen_has_claude_prompt("> \n──────"));
    assert!(screen_has_claude_prompt("  15: > \n  16: ──────"));
}

#[test]
fn screen_has_claude_prompt_ignores_unrelated_greater_than() {
    // A `>` appearing mid-line (a tip, a diff, redirected output) is not the
    // input prompt and must not produce a false positive.
    assert!(!screen_has_claude_prompt("Tip: pipe output > file.txt"));
    assert!(!screen_has_claude_prompt("  12: some text > more text"));
    assert!(!screen_has_claude_prompt("no prompt here at all"));
}

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
                if names.contains(&ev_name) {
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
            let has_prompt = screen_has_claude_prompt(&screen_lines_joined(&s));
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

fn drive_claude_startup(hcom_dir: &str, name: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_screen = String::new();
    while Instant::now() < deadline {
        let json_out = hcom_with_dir(&format!("term {name} --json"), hcom_dir);
        let screen_out = hcom_with_dir(&format!("term {name}"), hcom_dir);
        last_screen = String::from_utf8_lossy(&screen_out.stdout).to_string();
        let low = last_screen.to_lowercase();
        let bypass_confirm = low.contains("yes, i accept") && low.contains("bypass permissions");
        let onboarding = bypass_confirm
            || low.contains("text style")
            || low.contains("i trust this folder")
            || low.contains("is this a project")
            || low.contains("accessing workspace")
            || low.contains("do you trust")
            || low.contains("press enter to continue");
        if screen_out.status.success()
            && String::from_utf8_lossy(&json_out.stdout).contains("\"prompt_empty\":true")
            && !onboarding
        {
            return;
        }
        if bypass_confirm || onboarding {
            let key = if bypass_confirm { "2 " } else { "" };
            let inject = hcom_with_dir(&format!("term inject {name} {key}--enter"), hcom_dir);
            assert!(
                inject.status.success(),
                "drive startup inject failed\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&inject.stdout),
                String::from_utf8_lossy(&inject.stderr)
            );
        }
        thread::sleep(Duration::from_millis(800));
    }
    panic!("Claude did not reach input prompt within {timeout:?}; last screen:\n{last_screen}");
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

fn try_remote_launch_claude_headless(
    hcom_dir: &str,
    target_device: &str,
) -> Result<(String, String), String> {
    // Model pinning comes via HCOM_CLAUDE_ARGS set in hcom_with_dir.
    // --dir is required for remote launches; use the platform temp directory,
    // which exists on both sides of this local-machine test. --headless keeps the
    // launched Claude on hcom's detached PTY runner, preserving term screen /
    // inject coverage without requiring tmux or another terminal emulator.
    let launch_dir = std::env::temp_dir().to_string_lossy().replace('\\', "/");
    let cmd = format!(
        "1 claude --device {target_device} --headless --dir {} --go",
        shell_words::quote(&launch_dir)
    );
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

fn remote_term_screen_stdout(hcom_dir: &str, remote_name: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_stdout = String::new();
    let mut last_stderr = String::new();
    while Instant::now() < deadline {
        ensure_relay_worker(hcom_dir);
        let out = hcom_with_dir(&format!("term {remote_name}"), hcom_dir);
        last_stdout = String::from_utf8_lossy(&out.stdout).to_string();
        last_stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if out.status.success()
            && !last_stdout.contains("Remote term screen failed")
            && screen_has_claude_prompt(&last_stdout)
        {
            return last_stdout;
        }
        thread::sleep(Duration::from_secs(2));
    }
    panic!(
        "remote term_screen did not return prompt marker within 60s\nlast stdout: {last_stdout}\nlast stderr: {last_stderr}"
    );
}

fn write_claude_mock_env(hcom_dir: &Path, base_url: &str) {
    let claude_home = hcom_dir.join("claude-home");
    fs::create_dir_all(&claude_home).expect("create isolated Claude config dir");
    let env = [
        ("ANTHROPIC_BASE_URL", base_url.to_string()),
        (
            "ANTHROPIC_AUTH_TOKEN",
            "hcom-relay-test-dummy-token".to_string(),
        ),
        (
            "CLAUDE_CONFIG_DIR",
            claude_home.to_string_lossy().to_string(),
        ),
        ("DISABLE_LOGIN_COMMAND", "1".to_string()),
        ("DISABLE_UPDATES", "1".to_string()),
        ("DISABLE_TELEMETRY", "1".to_string()),
        ("DISABLE_GROWTHBOOK", "1".to_string()),
        ("DISABLE_ERROR_REPORTING", "1".to_string()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_string()),
        (
            "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL",
            "1".to_string(),
        ),
        ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1".to_string()),
        ("CLAUDE_CODE_DISABLE_THINKING", "1".to_string()),
        ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", "1".to_string()),
        ("DISABLE_PROMPT_CACHING", "1".to_string()),
        ("ENABLE_TOOL_SEARCH", "false".to_string()),
        ("CLAUDE_CODE_FORCE_SESSION_PERSISTENCE", "1".to_string()),
    ];
    let body = env
        .iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect::<String>();
    fs::write(hcom_dir.join("env"), body).expect("write Claude mock env passthrough");
}

fn relay_claude_mock_response(req: &RecordedRequest) -> Reply {
    const TOOL_RELAY_PONG: &str = "toolu_relay_pong_send";

    if req.method.eq_ignore_ascii_case("HEAD") {
        return Reply::Empty(200);
    }
    if req.path.contains("count_tokens") {
        return Reply::Json(serde_json::json!({"input_tokens": 1}).to_string());
    }
    if !req.path.contains("/v1/messages") {
        return Reply::Status(404);
    }
    let (tool_result, text) = latest_user_turn(&req.body).unwrap_or_default();
    if tool_result.as_deref() == Some(TOOL_RELAY_PONG) {
        return Reply::Sse(claude_text("msg_relay_pong_done", "PONG sent"));
    }
    if text.contains("Reply with exactly the single word PONG") {
        return Reply::Sse(claude_tool_use(
            "msg_relay_pong_tool",
            TOOL_RELAY_PONG,
            "Bash",
            &serde_json::json!({
                "command": "hcom send @bigboss --intent inform -- PONG",
                "description": "send the relay roundtrip PONG response",
            }),
        ));
    }
    Reply::Sse(claude_text("msg_relay_roundtrip", "OK"))
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
#[cfg(unix)]
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

#[cfg(windows)]
fn kill_orphan_debug_daemons() {
    // Windows has no built-in command-line process matcher equivalent to
    // pgrep. Each run uses unique HCOM_DIRs and its PID-file-owned daemons are
    // still cleaned by RelayGuard below.
}

fn kill_daemon(hcom_dir: &str) {
    let pid_path = Path::new(hcom_dir).join(".tmp").join("relay.pid");
    if let Ok(content) = fs::read_to_string(&pid_path)
        && let Ok(pid) = content.trim().parse::<i64>()
    {
        support::terminate_process_group(pid);
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

    let claude_mock =
        MockHttp::start(relay_claude_mock_response).expect("start localhost Claude mock provider");
    let mock_base_url = format!("http://127.0.0.1:{}", claude_mock.port());
    write_claude_mock_env(&dir_b_path, &mock_base_url);

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
        Duration::from_secs(60),
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
        Duration::from_secs(60),
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
                    if let Some(s) = data.as_str()
                        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
                    {
                        data = parsed;
                    }
                    if let Some(text) = data["text"].as_str()
                        && text.contains(&marker)
                    {
                        return Some((ev, data));
                    }
                }
            }
            None
        },
        &format!("Device B sees '{marker}'"),
        Duration::from_secs(60),
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
                    if let Some(s) = data.as_str()
                        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
                    {
                        data = parsed;
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

    let claude_version =
        std::env::var("HCOM_TEST_CLAUDE_VERSION").unwrap_or_else(|_| "2.1.185".to_string());
    assert_tool_pinned(
        "claude",
        &claude_version,
        &format!("scripts/install-mock-tools.sh @anthropic-ai/claude-code@{claude_version}"),
    );

    let baseline_event_b = last_event_id(&path_b);
    let (launched_name, launch_output) = try_remote_launch_claude_headless(&path_a, &short_b)
        .unwrap_or_else(|e| {
            panic!("Phase 7: remote launch failed: {e}");
        });
    logln!(log, "{}", launch_output.trim_end());
    logln!(
        log,
        "  OK: Remote launch succeeded with claude/headless: {launched_name}"
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
    drive_claude_startup(&path_b, &launched_name, Duration::from_secs(90));
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
        "  OK: claude TUI drawn (prompt marker present, prompt empty)"
    );

    // ── Phase 8: term_screen on live instance ─────────────────────
    logln!(
        log,
        "\n[Phase 8] Device A: remote term_screen on Device B ({remote_name})..."
    );

    // Plain-text remote call: should print the formatted screen (containing
    // the Claude ready banner). It is read-only over a public broker, so retry
    // transient publish/response misses instead of making the whole test flaky.
    remote_term_screen_stdout(&path_a, &remote_name);
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
        screen_has_claude_prompt(rpc_content),
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
    // Clearing the input line only proves that Claude accepted the submitted
    // marker. ConPTY can report that frame before the turn finishes, while
    // delivery is still gated. Wait for the stable idle prompt before sending
    // the Phase 10 message.
    wait_for_screen_drawn(&path_b, &launched_name, Duration::from_secs(60));
    poll_until(
        || {
            let instance = find_instance_by_base(&path_b, &launched_name)?;
            (instance["status"].as_str() == Some("listening")).then_some(())
        },
        "Claude returned to listening after marker turn",
        Duration::from_secs(60),
        Duration::from_millis(500),
    );
    logln!(
        log,
        "  OK: marker turn finished and prompt returned idle; both inject RPCs ok=true"
    );

    // ── Phase 10: real send+reply round-trip via relay ───────────
    logln!(
        log,
        "\n[Phase 10] Device A: send real question and verify reply event via relay..."
    );

    let question = "Reply with exactly the single word PONG then stop.";

    // Watermark Device A's events so we only count relayed replies that
    // arrive AFTER the question goes out.
    let pre_send_event_a = last_event_id(&path_a);

    // Send from bigboss (`-b`) rather than a synthetic `--from` label.
    // `--from <name>` is a CLI-only sender stamp with no return route on
    // the receiving device — the receiver sees `name:DEVICE` but can't
    // address it back, so the reply dead-ends locally. bigboss is the
    // one universally-addressable target; the agent's system prompt
    // ("Prioritize @bigboss") makes the reply land cleanly, and bigboss
    // messages relay back like any other event.
    let send_out = hcom_with_dir(
        &format!("send -b @{launched_name}:{short_b} --intent request -- \"{question}\""),
        &path_a,
    );
    assert!(
        send_out.status.success(),
        "hcom send failed: {}",
        String::from_utf8_lossy(&send_out.stderr)
    );
    logln!(
        log,
        "  OK: sent question from bigboss to @{launched_name}:{short_b}"
    );

    poll_until(
        || {
            let out = hcom_with_dir("events --type message --last 20", &path_b);
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains(question).then_some(())
        },
        "Device B received targeted Phase 10 message",
        Duration::from_secs(30),
        Duration::from_millis(500),
    );
    logln!(log, "  OK: Device B received targeted Phase 10 message");

    // Step 1: Device B's claude received and processed (status round-trip).
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
                if ctx.starts_with("deliver:bigboss") {
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

    // Device A's worker may have auto-exited during the long status wait
    // (watchdog exits after ~60s with no local instances). Re-arm so the
    // PONG reply event can land here.
    ensure_relay_worker(&path_a);

    // Step 2: the real round-trip — claude's PONG reply must reach
    // Device A as a relayed message event with `from = nara:TAMA`.
    // This proves the event actually traversed the relay, not just that
    // claude wrote something locally on Device B.
    let expected_from = format!("{launched_name}:{short_b}");
    let mut last_log_count = 0usize;
    let pong_deadline = Instant::now() + Duration::from_secs(90);
    let pong_event = loop {
        let out = hcom_with_dir("events --type message --last 50", &path_a);
        let mut found = None;
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut new_count = 0usize;
            for line in stdout.lines() {
                let ev: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = ev["id"].as_i64().unwrap_or(0);
                if id <= pre_send_event_a {
                    continue;
                }
                new_count += 1;
                let mut data = ev["data"].clone();
                if let Some(s) = data.as_str()
                    && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
                {
                    data = parsed;
                }
                let from = data["from"].as_str().unwrap_or("");
                let text = data["text"].as_str().unwrap_or("");
                if from == expected_from && text.to_uppercase().contains("PONG") {
                    found = Some(ev);
                    break;
                }
            }
            if found.is_none() && new_count != last_log_count {
                last_log_count = new_count;
                eprintln!(
                    "    {new_count} new message events on A, none from {expected_from} with PONG yet"
                );
            }
        }
        if let Some(ev) = found {
            break ev;
        }
        if Instant::now() >= pong_deadline {
            // The generic poll_until panic ("Timeout: description") gives no
            // insight into *why* the PONG never arrived. Pull Device B's live
            // screen and recent events so a CI failure here is diagnosable
            // without another round-trip.
            let device_b_screen = get_screen_local_json(&path_b, &launched_name)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<no screen>".to_string());
            let device_b_events = hcom_with_dir("events --last 20", &path_b);
            panic!(
                "Timeout (90s): Device A receives PONG reply event from {expected_from}\n\
                 Device B screen: {device_b_screen}\n\
                 Device B recent events:\n{}",
                String::from_utf8_lossy(&device_b_events.stdout)
            );
        }
        thread::sleep(Duration::from_secs(2));
    };
    logln!(
        log,
        "  OK: Device A received PONG reply event (id={}, from={expected_from})",
        pong_event["id"].as_i64().unwrap_or(0)
    );

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
    let db = rusqlite::Connection::open(&db_path_b).expect("open Device B database");
    let sql_tag: String = db
        .query_row(
            "SELECT tag FROM instances WHERE name = ?1",
            rusqlite::params![launched_name],
            |row| row.get(0),
        )
        .expect("read Device B instance tag");
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

    // After the kill, a remote term_screen must eventually fail (no inject
    // port). The killed instance's DB row clears as soon as its tracked child
    // process dies, but the PTY manager process behind the inject port can
    // legitimately outlive that by a couple of seconds — its reader thread
    // joins the ConPTY pipe with a bounded 2s timeout before tearing itself
    // down (see `join_with_timeout` in `src/pty/win.rs`) — so poll rather
    // than asserting on the very first check.
    poll_until(
        || {
            let post_kill_term = hcom_with_dir(&format!("term {remote_name}"), &path_a);
            let post_kill_stdout = String::from_utf8_lossy(&post_kill_term.stdout).to_string();
            (post_kill_stdout.contains("Remote term screen failed")
                || !post_kill_term.status.success())
            .then_some(())
        },
        "term_screen should fail after kill",
        Duration::from_secs(10),
        Duration::from_millis(300),
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
    // Claude agent in a detached headless PTY — if a later poll or assertion
    // panics, the guard's Drop still needs to kill that process group.
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

    drive_claude_startup(&path_b, &resumed_full_name, Duration::from_secs(90));
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

    let unexpected = claude_mock.unexpected();
    assert!(
        unexpected.is_empty(),
        "Claude mock received {} unexpected request(s):\n{}",
        unexpected.len(),
        unexpected
            .iter()
            .map(|req| format!("{} {}\n{}", req.method, req.path, req.body))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    let transport_errors = claude_mock.transport_errors();
    assert!(
        transport_errors.is_empty(),
        "Claude mock hit transport errors:\n{}",
        transport_errors.join("\n")
    );

    // Cleanup handled by guard Drop
    logln!(log, "\n{}", "=".repeat(60));
    logln!(
        log,
        "ALL PHASES PASSED (including {launched_tool} remote launch/kill)"
    );
    logln!(log, "{}", "=".repeat(60));
}
