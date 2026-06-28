//! PTY delivery live integration test.
//!
//! Launches a real AI tool instance in a terminal, tests message delivery and gate blocking.
//! Records full screen state at each phase for regression detection.
//!
//! Requires:
//! - tmux installed and available by default, or another terminal preset via HCOM_TEST_TERMINAL
//! - Target tool CLI installed (claude/gemini/codex/opencode/kilo/pi/cursor/copilot)
//!
//! Phases (claude/gemini/codex/antigravity/cursor/copilot):
//! 1. Launch tool via `hcom 1 <tool>` with HCOM_TERMINAL=<terminal>
//! 2. Wait for ready event, capture and validate full screen state
//! 3. Send message → verify delivery via events, capture post-delivery screen
//! 4. Inject uncommitted text → verify gate blocks delivery, capture screen
//! 5. Submit text → verify blocked message delivers
//! 6. Cleanup
//!
//! Phases (opencode/kilo/pi — PTY bootstrap injection):
//! 1. Launch tool in tmux, wait for ready event
//! 2. Send message → verify PTY bootstrap injection triggers plugin binding + delivery
//! 3. Send second message → verify plugin-based delivery (no PTY inject)
//! 4. Cleanup
//!
//! Run (must use --test-threads=1 — tests launch real agents and interfere in parallel):
//!     cargo test -p hcom --test test_pty_delivery -- --ignored --nocapture --test-threads=1
//!     cargo test -p hcom --test test_pty_delivery test_pty_claude -- --ignored --nocapture --test-threads=1
//!     HCOM_TEST_TERMINAL=kitty cargo test -p hcom --test test_pty_delivery test_pty_claude -- --ignored --nocapture --test-threads=1
//!
//! Why `#[ignore]`:
//! These tests are not part of `cargo test` deliberately. They launch real agent
//! CLIs in a real terminal, need each upstream tool installed on PATH,
//! take minutes per case, run actual agents (cost). They are
//! meant for manual runs ie version-bump. They are not literally 'ignored', just run when needed.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

// Serial execution guard — PTY tests set env vars and spawn real agents; parallel runs interfere.
static TEST_SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    // Recover from poison so a panic in one test (e.g. gemini Phase 3 race)
    // does not cascade-fail the next test (codex) with PoisonError. Each test
    // sets up its own fresh agent, so the guarded state is just "one PTY at a time".
    TEST_SERIAL
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Write to both stdout and the log file.
macro_rules! logln {
    ($log:expr, $($arg:tt)*) => {{
        let _msg = format!($($arg)*);
        println!("{}", _msg);
        $log.log(&_msg);
    }};
}

// ── Constants ──────────────────────────────────────────────────────────

/// Ready patterns — must match `IntegrationSpec.ready_pattern`. This test is
/// an integration test against the hcom binary so it can't import the crate;
/// the patterns are short and rarely change, drift is caught by the test
/// itself when the expected pattern fails to appear on screen.
fn ready_pattern(tool: &str) -> &'static str {
    match tool {
        "claude" => "? for shortcuts",
        "codex" => "\u{203a} ",
        "gemini" => "Type your message",
        "opencode" => "ctrl+p commands",
        // Kilo is an OpenCode-family fork: same TUI footer.
        "kilo" => "ctrl+p commands",
        "pi" => "/ commands",
        // OMP (Oh My Pi) is a Pi-family fork with the same TUI footer.
        "omp" => "/ commands",
        "antigravity" => "? for shortcuts",
        // Cursor has no stable ASCII ready footer (spec ready_pattern is empty,
        // so is_ready() is always true); readiness is asserted via ready/
        // prompt_empty directly. has_ready_pattern() gates the pattern check off
        // for cursor so it isn't run vacuously against an empty needle.
        "cursor" => "",
        "copilot" => "/ commands",
        _ => panic!("Unknown tool: {tool}"),
    }
}

/// Prompt markers: characters that screen.rs scans for to find the input line
fn prompt_marker(tool: &str) -> &'static str {
    match tool {
        "claude" => "❯",
        "codex" => "›",
        "gemini" => " > ",
        "antigravity" => ">",
        "cursor" => "→",
        "copilot" => "❯",
        _ => panic!("No prompt marker for {tool}"),
    }
}

/// Frame markers: border characters that help identify the input box structure
fn frame_marker(tool: &str) -> Option<&'static str> {
    match tool {
        "claude" => Some("─"),
        "codex" => None,
        "gemini" => None,
        "antigravity" => Some("─"),
        "cursor" => None,
        "copilot" => None,
        _ => None,
    }
}

/// Expected gate block context when prompt has text
fn gate_block_context(tool: &str) -> &'static str {
    match tool {
        "claude" => "tui:prompt-has-text",
        "codex" => "tui:prompt-has-text",
        "gemini" => "tui:not-ready",
        "antigravity" => "tui:prompt-has-text",
        // cursor: require_prompt_empty=true, so uncommitted text settles to
        // prompt_has_text (may transiently report user-active first; validate
        // only warns on mismatch).
        "cursor" => "tui:prompt-has-text",
        "copilot" => "tui:prompt-has-text",
        _ => panic!("No gate block context for {tool}"),
    }
}

/// Whether this tool gates on ready pattern
fn require_ready(tool: &str) -> bool {
    matches!(tool, "gemini")
}

/// Timeout for the Phase 2 clean-prompt delivery wait.
///
/// hcom delivers to an idle agent by injecting only the `<hcom>` trigger (see
/// `delivery.rs` build_wake_inject_text — Claude/Codex/Cursor all trigger-only);
/// the message body is then surfaced by a hook *during the agent's turn*. For
/// Claude/Codex/Gemini that hook fires fast, so 20s is ample. Cursor instead
/// delivers when the turn ends (stop → followup_message) or on its first tool
/// call (postToolUse → additional_context), so its first delivery is bounded by
/// a full model turn on `--model auto` — empirically 9–25s. Use the same 60s
/// budget Phase 4 already gives a turn-bounded delivery rather than letting a
/// slow-but-healthy turn read as failure. The assertion stays strict: it still
/// requires the real `deliver:` event, not merely the injected trigger.
fn clean_prompt_delivery_timeout(tool: &str) -> Duration {
    match tool {
        // Turn-bounded delivery: agentStop/followup_message fires at end of a full model turn
        "cursor" | "copilot" => Duration::from_secs(60),
        _ => Duration::from_secs(20),
    }
}

const SCREEN_FIELDS: &[&str] = &[
    "lines",
    "size",
    "cursor",
    "ready",
    "prompt_empty",
    "input_text",
];
const SENDER: &str = "ptytest";

// ── Helpers ────────────────────────────────────────────────────────────

fn configure_test_terminal_env() -> String {
    let terminal = std::env::var("HCOM_TEST_TERMINAL")
        .or_else(|_| std::env::var("HCOM_TERMINAL"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "tmux".to_string());

    // SAFETY: Integration tests run serially (serial_lock guards callers).
    unsafe {
        std::env::set_var("HCOM_TERMINAL", &terminal);
        std::env::set_var("HCOM_TAG", "ptytest");
    }

    terminal
}

fn hcom(cmd: &str) -> Output {
    Command::new("hcom")
        .args(shell_words::split(cmd).unwrap())
        .output()
        .expect("failed to execute hcom")
}

fn base_name_from_instance_name(instance_name: &str) -> String {
    let tag = std::env::var("HCOM_TAG").unwrap_or_default();
    let prefix = format!("{tag}-");
    instance_name
        .strip_prefix(&prefix)
        .unwrap_or(instance_name)
        .to_string()
}

fn parse_launch_base_name(out: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.lines().find_map(|line| {
        let names = line.trim().strip_prefix("Names: ")?;
        let first = names.split_whitespace().next()?;
        Some(base_name_from_instance_name(first))
    })
}

fn assert_launch_process_started(out: &Output) -> Option<String> {
    let base_name = parse_launch_base_name(out);
    if out.status.success() || (out.status.code() == Some(2) && base_name.is_some()) {
        return base_name;
    }

    panic!(
        "Launch failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn hcom_check(cmd: &str) -> String {
    let out = hcom(cmd);
    assert!(
        out.status.success(),
        "Command failed: hcom {cmd}\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn send_msg(msg: &str) {
    hcom_check(&format!("send --from {SENDER} --intent inform '{msg}'"));
}

fn get_screen(name: &str) -> Option<serde_json::Value> {
    let out = hcom(&format!("term {name} --json"));
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn get_events(instance: &str, last: u32, full: bool) -> Vec<serde_json::Value> {
    let full_flag = if full { " --full" } else { "" };
    let out = hcom(&format!(
        "events --agent {instance} --last {last}{full_flag}"
    ));
    if !out.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .collect()
}

fn get_last_event_id(name: &str) -> i64 {
    let events = get_events(name, 1, false);
    events.last().and_then(|e| e["id"].as_i64()).unwrap_or(0)
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
            "Timeout ({timeout:?}) waiting for: {description}"
        );
        thread::sleep(interval);
    }
}

/// Wait until the agent is *stably* idle: the most recent status event is
/// `listening` and no newer status event has appeared for `settle`.
///
/// A single `listening` event is terminal for claude/codex/gemini/antigravity,
/// but not for cursor: its `stop` hook delivers via `followup_message`, which
/// cursor auto-resubmits as the next user turn, so a delivery is followed by
/// another active→listening cycle. Phase 3's premise — "while the PTY prompt
/// holds uncommitted text, the queued message is not delivered" — only holds if
/// the agent has *no turn in flight*. With a turn still running, the hook
/// channel (postToolUse / stop), which by design bypasses the PTY prompt-empty
/// gate, can legitimately deliver mid-turn and the test would misread that
/// hook-channel delivery as a PTY gate failure. Draining to stable idle first
/// keeps Phase 3 honest: it isolates the PTY-inject path it actually asserts on.
fn wait_for_stable_idle(base_name: &str, settle: Duration, timeout: Duration, log: &TestLog) {
    let start = Instant::now();
    let mut last_status_id = -1i64;
    let mut stable_since = Instant::now();
    loop {
        let latest_status = get_events(base_name, 30, false)
            .into_iter()
            .filter(|ev| ev["type"].as_str() == Some("status"))
            .filter_map(|ev| ev["id"].as_i64().map(|id| (id, ev)))
            .max_by_key(|(id, _)| *id);
        if let Some((id, ev)) = latest_status {
            if id != last_status_id {
                last_status_id = id;
                stable_since = Instant::now();
            }
            let listening = ev["data"]["status"].as_str() == Some("listening");
            if listening && stable_since.elapsed() >= settle {
                logln!(
                    log,
                    "  OK: Agent stably idle for {:?} (last status id={id})",
                    settle
                );
                return;
            }
        }
        assert!(
            start.elapsed() < timeout,
            "Timeout ({timeout:?}) waiting for stable idle (base={base_name}, last status id={last_status_id})"
        );
        thread::sleep(Duration::from_millis(500));
    }
}

// ── Cleanup guard ──────────────────────────────────────────────────────

struct InstanceGuard {
    base_name: Option<String>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(name) = &self.base_name {
            eprintln!("\nCleaning up {name}...");
            let _ = hcom(&format!("kill {name}"));
            thread::sleep(Duration::from_secs(1));
        }
    }
}

// ── Logging ────────────────────────────────────────────────────────────

struct TestLog {
    timestamped: PathBuf,
    latest: PathBuf,
    start: Instant,
}

impl TestLog {
    fn new(tool: &str) -> Self {
        let log_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-logs");
        fs::create_dir_all(&log_dir).ok();

        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let timestamped = log_dir.join(format!("pty_delivery_{tool}_{ts}.log"));
        let latest = log_dir.join(format!("test_pty_delivery_{tool}.latest.log"));

        let start = Instant::now();
        let header = format!(
            "[{}] PTY delivery test: {tool}\nlog: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            timestamped.display(),
        );
        // Write header immediately — so log is non-empty even if test panics early.
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
        for path in [&self.timestamped, &self.latest] {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{text}");
            }
        }
    }

    fn log_screen(&self, screen: &serde_json::Value, label: &str) {
        self.log(&format!("\n── Screen Snapshot: {label}"));
        self.log(&format!("size: {}", screen["size"]));
        self.log(&format!("cursor: {}", screen["cursor"]));
        self.log(&format!("ready: {}", screen["ready"]));
        self.log(&format!("prompt_empty: {}", screen["prompt_empty"]));
        self.log(&format!("input_text: {:?}", screen["input_text"]));
        if let Some(lines) = screen["lines"].as_array() {
            for (i, line) in lines.iter().enumerate() {
                self.log(&format!("{i:3}: {}", line.as_str().unwrap_or("")));
            }
        }
        self.log("");
    }
}

impl Drop for TestLog {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let elapsed = self.start.elapsed();
            self.log(&format!("\n[{elapsed:.1?}] TEST FAILED (panicked above)"));
        } else {
            let elapsed = self.start.elapsed();
            self.log(&format!("\n[{elapsed:.1?}] TEST COMPLETE"));
        }
    }
}

// ── Validation ─────────────────────────────────────────────────────────

fn validate_screen_schema(screen: &serde_json::Value) {
    let keys: HashSet<&str> = screen
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    for field in SCREEN_FIELDS {
        assert!(keys.contains(field), "Screen JSON missing field: {field}");
    }
    assert!(screen["lines"].is_array(), "lines should be array");
    let size = screen["size"].as_array().unwrap();
    assert_eq!(size.len(), 2, "size should be [r,c]");
    let cursor = screen["cursor"].as_array().unwrap();
    assert_eq!(cursor.len(), 2, "cursor should be [r,c]");
    assert!(screen["ready"].is_boolean(), "ready should be bool");
    assert!(
        screen["prompt_empty"].is_boolean(),
        "prompt_empty should be bool"
    );
    assert!(
        screen["input_text"].is_null() || screen["input_text"].is_string(),
        "input_text should be str or null"
    );
}

/// Returns true if this tool has an ASCII ready-pattern footer to match.
/// Cursor signals readiness via prompt-empty instead (spec ready_pattern is
/// empty), so there is no pattern to assert — callers check `ready`/`prompt_empty`
/// directly rather than running a pattern match that would pass on `contains("")`.
fn has_ready_pattern(tool: &str) -> bool {
    !ready_pattern(tool).is_empty()
}

fn validate_ready_pattern(screen: &serde_json::Value, tool: &str) {
    let pattern = ready_pattern(tool);
    assert!(
        !pattern.is_empty(),
        "validate_ready_pattern called for {tool}, which has no ready pattern; \
         guard the call with has_ready_pattern() so the check isn't vacuous"
    );
    let screen_text: String = screen["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let present = screen_text.contains(pattern);

    if screen["ready"].as_bool().unwrap() && !present {
        panic!("ready=true but ready pattern '{pattern}' not found in screen lines");
    }
    if !screen["ready"].as_bool().unwrap() && present {
        eprintln!("  WARN: ready=false but pattern '{pattern}' found in screen (transient?)");
    }
}

fn validate_prompt_consistency(screen: &serde_json::Value) {
    let input_text = screen["input_text"].as_str().unwrap_or("");
    let prompt_empty = screen["prompt_empty"].as_bool().unwrap();

    if prompt_empty && !input_text.is_empty() {
        panic!("prompt_empty=true but input_text={input_text:?}");
    }
    if !prompt_empty && input_text.is_empty() {
        eprintln!("  WARN: prompt_empty=false but input_text is empty");
    }
}

fn validate_tool_ui_elements(screen: &serde_json::Value, tool: &str) {
    let lines = screen["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l.as_str())
        .collect::<Vec<_>>();
    let screen_text = lines.join("\n");

    let marker = prompt_marker(tool);
    assert!(
        screen_text.contains(marker),
        "Tool prompt marker '{marker}' not found — tool TUI may have changed (breaks screen.rs)"
    );
    eprintln!("  OK: Prompt marker '{marker}' present");

    if let Some(frame) = frame_marker(tool) {
        assert!(
            screen_text.contains(frame),
            "Tool frame marker '{frame}' not found — tool TUI may have changed (breaks screen.rs)"
        );
        eprintln!("  OK: Frame marker '{frame}' present");
    }

    if tool == "gemini" {
        validate_gemini_prompt_frame(&lines);
        eprintln!("  OK: Gemini prompt frame present around prompt line");
    }
}

fn screen_has_text(screen: &serde_json::Value, needle: &str) -> bool {
    screen["lines"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .filter_map(|v| v.as_str())
                .any(|line| line.contains(needle))
        })
        .unwrap_or(false)
}

fn initial_screen_ready(screen: &serde_json::Value, tool: &str) -> bool {
    if screen["ready"].as_bool() != Some(true) {
        return false;
    }
    if tool == "gemini" && screen_has_text(screen, "Executing Hook:") {
        return false;
    }
    true
}

fn plugin_initial_screen_ready(screen: &serde_json::Value, tool: &str) -> bool {
    if tool != "pi" {
        return initial_screen_ready(screen, tool);
    }

    // Pi's `/ commands` footer is visible at launch long enough for the PTY
    // wrapper to emit life.ready, but in the default tmux 24-line viewport the
    // startup help and tmux warning can push that footer out of the retained
    // screen before this test polls `term --json`. For plugin-backed Pi the
    // launch-ready event is the readiness contract; here we only need a
    // rendered screen to continue with bootstrap/plugin delivery assertions.
    screen["lines"].as_array().is_some_and(|lines| {
        lines
            .iter()
            .any(|line| line.as_str().is_some_and(|s| !s.trim().is_empty()))
    })
}

fn is_gemini_border_line(line: &str) -> bool {
    let trimmed = line.trim();
    let count = trimmed.chars().count();
    count >= 10
        && trimmed
            .chars()
            .all(|c| matches!(c, '─' | '▀' | '▄' | '╭' | '╮' | '╰' | '╯'))
        && trimmed.chars().any(|c| matches!(c, '─' | '▀' | '▄'))
}

fn validate_gemini_prompt_frame(lines: &[&str]) {
    let Some(prompt_idx) = lines.iter().rposition(|line| {
        line.find(" > ")
            .or_else(|| line.find(" * "))
            .is_some_and(|pos| pos <= 3)
    }) else {
        panic!("Gemini prompt line not found — tool TUI may have changed (breaks screen.rs)");
    };

    let has_top = prompt_idx > 0 && is_gemini_border_line(lines[prompt_idx - 1]);
    let has_bottom = prompt_idx + 1 < lines.len() && is_gemini_border_line(lines[prompt_idx + 1]);

    assert!(
        has_top && has_bottom,
        "Gemini prompt line was not framed by adjacent border rows — tool TUI may have changed (breaks screen.rs)"
    );
}

fn validate_delivery_events(instance: &str, baseline_id: i64, sender: &str, log: &TestLog) {
    let events = get_events(instance, 30, true);
    let delivery = events.iter().find(|ev| {
        ev["id"].as_i64().unwrap_or(0) > baseline_id
            && ev["type"].as_str() == Some("status")
            && ev["data"]["context"]
                .as_str()
                .is_some_and(|c| c.contains("deliver:"))
    });

    let delivery =
        delivery.unwrap_or_else(|| panic!("No delivery event found after id {baseline_id}"));
    let data = &delivery["data"];
    log.log(&format!(
        "Delivery event: {}",
        serde_json::to_string_pretty(delivery).unwrap()
    ));
    logln!(
        log,
        "  Delivery event: id={} context={} position={} msg_ts={}",
        delivery["id"],
        data["context"],
        data["position"],
        data["msg_ts"]
    );

    let ctx = data["context"].as_str().unwrap_or("");
    assert!(
        ctx.contains(sender),
        "Delivery context '{ctx}' doesn't reference sender '{sender}'"
    );
    logln!(log, "  OK: Delivery event references sender '{sender}'");

    let pos = data["position"].as_i64().unwrap_or(0);
    assert!(
        pos > baseline_id,
        "Delivery position {pos} not after baseline {baseline_id}"
    );
    logln!(
        log,
        "  OK: Delivery position {pos} > baseline {baseline_id}"
    );
}

fn validate_gate_block(instance: &str, tool: &str, after_id: i64, log: &TestLog) {
    let expected = gate_block_context(tool);
    let events = get_events(instance, 20, false);

    let gate_event = events.iter().find(|ev| {
        ev["id"].as_i64().unwrap_or(0) > after_id
            && ev["type"].as_str() == Some("status")
            && ev["data"]["context"]
                .as_str()
                .is_some_and(|c| c.starts_with("tui:"))
    });

    if let Some(ev) = gate_event {
        let ctx = ev["data"]["context"].as_str().unwrap_or("");
        let detail = ev["data"]["detail"].as_str().unwrap_or("");
        logln!(
            log,
            "  Gate block event: id={} context={ctx} detail={detail:?}",
            ev["id"]
        );
        if ctx == expected {
            logln!(log, "  OK: Gate blocked with expected context '{expected}'");
        } else {
            logln!(
                log,
                "  WARN: Expected gate context '{expected}', got '{ctx}'"
            );
        }
    } else {
        logln!(
            log,
            "  INFO: No gate block event found (may already have been in blocked state)"
        );
    }
}

// ── Main test flow (claude/gemini/codex) ───────────────────────────────

fn run_pty_test(tool: &str) {
    let _serial = serial_lock();

    let terminal = configure_test_terminal_env();
    let log = TestLog::new(tool);

    logln!(log, "{}", "=".repeat(60));
    logln!(log, "PTY Delivery Test: {tool} via {terminal}");
    logln!(log, "{}", "=".repeat(60));

    // Record last event ID before launch
    let pre_launch_id = {
        let out = hcom("events --last 1");
        if out.status.success() {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .filter_map(|v| v["id"].as_i64())
                .next_back()
                .unwrap_or(0)
        } else {
            0
        }
    };

    // ── Phase 1: Launch ──────────────────────────────────────────
    logln!(log, "\n[Phase 1] Launching {tool} in {terminal}...");
    let t0 = Instant::now();

    let model_flag = match tool {
        "claude" => " --model haiku",
        "codex" => " --model gpt-5.4-mini",
        "gemini" => " --model gemini-2.5-flash-lite",
        // `auto` is the only model guaranteed to launch across cursor plan tiers
        // (named models error on free plans).
        "cursor" => " --model auto",
        // copilot: no flag — its default model is probably cheap.
        _ => "",
    };
    let out = hcom(&format!("--go 1 {tool}{model_flag}"));
    let launched_base_name = assert_launch_process_started(&out);
    if out.status.code() == Some(2) {
        logln!(
            log,
            "  INFO: launch command still starting after inline wait; continuing with screen poll"
        );
    }

    logln!(log, "  Waiting for launched instance...");

    let mut guard = InstanceGuard { base_name: None };

    let base_name: String = if let Some(base_name) = launched_base_name {
        base_name
    } else {
        poll_until(
            || {
                let out = hcom("events --action ready --last 5");
                if !out.status.success() {
                    return None;
                }
                for line in String::from_utf8_lossy(&out.stdout).lines().rev() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(ev) = serde_json::from_str::<serde_json::Value>(line)
                        && ev["type"].as_str() == Some("life")
                        && ev["data"]["action"].as_str() == Some("ready")
                        && ev["id"].as_i64().unwrap_or(0) > pre_launch_id
                    {
                        return ev["instance"].as_str().map(|s| s.to_string());
                    }
                }
                None
            },
            "ready event from launched instance",
            Duration::from_secs(60),
            Duration::from_secs(2),
        )
    };

    guard.base_name = Some(base_name.clone());
    let tag = std::env::var("HCOM_TAG").unwrap_or_default();
    let instance_name = if tag.is_empty() {
        base_name.clone()
    } else {
        format!("{tag}-{base_name}")
    };

    let t_ready = t0.elapsed();
    logln!(
        log,
        "  OK: Instance launched: {instance_name} (base: {base_name}, ready in {t_ready:.1?})"
    );

    // Wait for screen to be ready
    let screen: serde_json::Value = poll_until(
        || {
            let s = get_screen(&base_name)?;
            if plugin_initial_screen_ready(&s, tool) {
                Some(s)
            } else {
                None
            }
        },
        "screen ready (TUI fully rendered)",
        Duration::from_secs(30),
        Duration::from_secs(1),
    );

    // ── Validate initial screen ──────────────────────────────────
    logln!(log, "\n[Validate] Initial screen state for {tool}...");
    validate_screen_schema(&screen);
    logln!(log, "  OK: Schema valid");
    if has_ready_pattern(tool) {
        validate_ready_pattern(&screen, tool);
        logln!(
            log,
            "  OK: Ready pattern '{}' consistent",
            ready_pattern(tool)
        );
    } else {
        logln!(
            log,
            "  SKIP: {tool} has no ready pattern; readiness asserted via ready/prompt_empty"
        );
    }
    assert_eq!(screen["ready"].as_bool(), Some(true));
    validate_prompt_consistency(&screen);
    logln!(
        log,
        "  OK: prompt_empty={} input_text={:?}",
        screen["prompt_empty"],
        screen["input_text"]
    );
    validate_tool_ui_elements(&screen, tool);
    assert_eq!(screen["prompt_empty"].as_bool(), Some(true));
    log.log_screen(&screen, &format!("{tool} — initial (prompt empty)"));

    // ── Phase 2: Delivery succeeds on clean prompt ───────────────
    logln!(log, "\n[Phase 2] Testing delivery on clean prompt...");

    let baseline_event = get_last_event_id(&base_name);
    logln!(log, "  OK: Baseline event ID: {baseline_event}");

    let t1 = Instant::now();
    // Phrasing matters on two axes:
    // 1. Gemini-2.5-flash-lite interprets human-style directives ("do not reply")
    //    as needing user confirmation and calls ask_user → approval gate,
    //    blocking forever. Keep the `[hcom heartbeat]` automated-signal framing.
    // 2. Codex and OpenCode would otherwise run `hcom send … --intent ack` and
    //    burn tokens (the ack is rejected by hcom since inform can't be acked,
    //    but the tool call still fires). Explicit "no tools, no hcom send" cuts
    //    that out without tripping (1).
    send_msg(&format!(
        "@{instance_name} [hcom heartbeat] automated test ping. acknowledge inline with \"ok\" only. no tools. no hcom send."
    ));
    logln!(log, "  OK: Message sent");

    // Wait for delivery event
    let delivery_event: serde_json::Value = poll_until(
        || {
            let events = get_events(&base_name, 30, true);
            events.into_iter().find(|ev| {
                ev["id"].as_i64().unwrap_or(0) > baseline_event
                    && ev["type"].as_str() == Some("status")
                    && ev["data"]["context"]
                        .as_str()
                        .is_some_and(|c| c.contains("deliver:"))
            })
        },
        "delivery event",
        clean_prompt_delivery_timeout(tool),
        Duration::from_secs(1),
    );
    let t_delivery = t1.elapsed();
    let new_event = delivery_event["id"].as_i64().unwrap_or(0);
    logln!(
        log,
        "  OK: Cursor advanced: {baseline_event} -> {new_event} (delivery in {t_delivery:.1?})"
    );

    // Wait for screen to settle
    poll_until(
        || {
            let s = get_screen(&base_name)?;
            if s["prompt_empty"].as_bool() != Some(true) {
                return None;
            }
            if require_ready(tool) && s["ready"].as_bool() != Some(true) {
                return None;
            }
            Some(())
        },
        "screen settles after delivery",
        Duration::from_secs(60),
        Duration::from_secs(1),
    );

    // Wait for the agent to actually return to `listening`. The screen check
    // above only confirms the input box is empty/ready; for gemini the input
    // box renders the placeholder while the agent is still mid-turn (BeforeAgent
    // → tool loop → AfterTool → AfterAgent), so screen-settle does NOT mean
    // "agent idle". Without this, Phase 3 can race a still-running Phase 2 turn
    // — AfterTool fires inside the gate-block window and delivers the queued
    // message via additionalContext (a legitimate hook path, but it defeats
    // the test's "no delivery while gate blocks PTY inject" premise).
    poll_until(
        || {
            let evs = get_events(&base_name, 30, false);
            evs.into_iter().find(|ev| {
                ev["id"].as_i64().unwrap_or(0) > new_event
                    && ev["type"].as_str() == Some("status")
                    && ev["data"]["status"].as_str() == Some("listening")
            })
        },
        "agent returns to listening after Phase 2 turn",
        Duration::from_secs(60),
        Duration::from_secs(1),
    );

    // cursor only: drain the followup_message auto-continue loop to true
    // quiescence so Phase 3 doesn't race a residual turn (see wait_for_stable_idle).
    if tool == "cursor" {
        wait_for_stable_idle(
            &base_name,
            Duration::from_secs(8),
            Duration::from_secs(120),
            &log,
        );
    }

    validate_delivery_events(&base_name, baseline_event, SENDER, &log);

    let screen = get_screen(&base_name).unwrap();
    validate_screen_schema(&screen);
    if require_ready(tool) {
        validate_ready_pattern(&screen, tool);
    }
    validate_prompt_consistency(&screen);
    validate_tool_ui_elements(&screen, tool);
    log.log_screen(&screen, &format!("{tool} — post-delivery"));

    // ── Phase 3: Delivery blocked by uncommitted text ────────────
    logln!(
        log,
        "\n[Phase 3] Testing delivery blocked by uncommitted text..."
    );

    poll_until(
        || {
            let s = get_screen(&base_name)?;
            if s["prompt_empty"].as_bool() != Some(true) {
                return None;
            }
            if require_ready(tool) && s["ready"].as_bool() != Some(true) {
                return None;
            }
            Some(())
        },
        "prompt empty before inject",
        Duration::from_secs(30),
        Duration::from_secs(1),
    );
    // Extra settle time
    thread::sleep(Duration::from_secs(2));

    hcom_check(&format!("term inject {base_name} uncommitted text here"));
    logln!(log, "  OK: Injected uncommitted text");

    // Verify text appears in input box
    let screen: serde_json::Value = poll_until(
        || {
            let s = get_screen(&base_name)?;
            let text = s["input_text"].as_str().unwrap_or("");
            if text.contains("uncommitted") {
                Some(s)
            } else {
                None
            }
        },
        "injected text visible in input box",
        Duration::from_secs(10),
        Duration::from_millis(500),
    );

    validate_screen_schema(&screen);
    assert_eq!(
        screen["prompt_empty"].as_bool(),
        Some(false),
        "Expected prompt_empty=false after inject"
    );
    let input_text = screen["input_text"].as_str().unwrap_or("");
    assert!(
        input_text.contains("uncommitted"),
        "input_text={input_text:?}"
    );
    validate_prompt_consistency(&screen);
    if has_ready_pattern(tool) {
        validate_ready_pattern(&screen, tool);
    }
    logln!(log, "  OK: Input text detected: {input_text:?}");
    log.log_screen(
        &screen,
        &format!("{tool} — after inject (uncommitted text)"),
    );

    let baseline_event2 = get_last_event_id(&base_name);

    send_msg(&format!(
        "@{instance_name} [hcom heartbeat-2 should-block] automated test ping. acknowledge inline with \"ok\" only. no tools. no hcom send."
    ));
    logln!(log, "  OK: Message sent (should be blocked)");

    // Wait and verify delivery does NOT happen
    logln!(log, "  Waiting 8s to confirm no delivery...");
    thread::sleep(Duration::from_secs(8));

    let screen = get_screen(&base_name).unwrap();
    validate_screen_schema(&screen);
    let text = screen["input_text"].as_str().unwrap_or("");
    assert!(
        text.contains("uncommitted"),
        "Uncommitted text was clobbered! input_text={text:?}"
    );
    logln!(log, "  OK: Uncommitted text preserved: {text:?}");
    validate_prompt_consistency(&screen);

    // Verify no delivery event during gate block
    let events_after = get_events(&base_name, 20, false);
    let delivery_during_block: Vec<_> = events_after
        .iter()
        .filter(|ev| {
            ev["id"].as_i64().unwrap_or(0) > baseline_event2
                && ev["type"].as_str() == Some("status")
                && ev["data"]["context"]
                    .as_str()
                    .is_some_and(|c| c.contains("deliver:"))
        })
        .collect();
    assert!(
        delivery_during_block.is_empty(),
        "Unexpected delivery during gate block: {:?}",
        delivery_during_block.first()
    );
    logln!(log, "  OK: No delivery event during gate block");

    validate_gate_block(&base_name, tool, baseline_event2, &log);
    log.log_screen(&screen, &format!("{tool} — gate blocked (text preserved)"));

    // ── Phase 4: Submit uncommitted text, unblock delivery ────────
    logln!(
        log,
        "\n[Phase 4] Submitting uncommitted text, waiting for blocked message delivery..."
    );

    let baseline_event3 = get_last_event_id(&base_name);

    hcom_check(&format!("term inject {base_name} --enter"));
    logln!(log, "  OK: Sent --enter to submit uncommitted text");

    // Wait for screen to settle
    poll_until(
        || {
            let s = get_screen(&base_name)?;
            if s["prompt_empty"].as_bool() != Some(true) {
                return None;
            }
            if require_ready(tool) && s["ready"].as_bool() != Some(true) {
                return None;
            }
            Some(())
        },
        "screen settles after submitting text",
        Duration::from_secs(60),
        Duration::from_secs(1),
    );

    // Wait for delivery of previously-blocked message
    let delivery3: serde_json::Value = poll_until(
        || {
            let evs = get_events(&base_name, 20, false);
            evs.into_iter().find(|ev| {
                ev["id"].as_i64().unwrap_or(0) > baseline_event3
                    && ev["type"].as_str() == Some("status")
                    && ev["data"]["context"]
                        .as_str()
                        .is_some_and(|c| c.contains("deliver:"))
            })
        },
        "delivery event for blocked message",
        Duration::from_secs(60),
        Duration::from_secs(1),
    );
    logln!(
        log,
        "  OK: Blocked message delivered: id={} context={}",
        delivery3["id"],
        delivery3["data"]["context"]
    );
    log.log(&format!(
        "Phase 4 delivery event: {}",
        serde_json::to_string_pretty(&delivery3).unwrap()
    ));

    // Capture final screen
    let screen = get_screen(&base_name).unwrap();
    validate_screen_schema(&screen);
    if require_ready(tool) {
        validate_ready_pattern(&screen, tool);
    }
    validate_prompt_consistency(&screen);
    log.log_screen(
        &screen,
        &format!("{tool} — after blocked message delivered"),
    );

    // Log all events for reference
    let all_events = get_events(&base_name, 50, false);
    log.log(&format!("\n── All events for {instance_name}"));
    for ev in &all_events {
        log.log(&serde_json::to_string(ev).unwrap());
    }

    // Cleanup handled by guard Drop
    logln!(log, "\n{}", "=".repeat(60));
    logln!(log, "{} — ALL PHASES PASSED", tool.to_uppercase());
    logln!(log, "  Log: {}", log.timestamped.display());
    logln!(log, "{}", "=".repeat(60));
}

// ── Plugin-backed test flow ────────────────────────────────────────────

/// Shared flow for plugin-backed tools (opencode, kilo, pi): the agent boots in
/// a PTY, the first message is delivered via bootstrap injection, and subsequent
/// messages are delivered by the tool plugin. OpenCode/Kilo share
/// `opencode-read`; Pi uses its own `pi-read` hook.
fn run_pty_test_plugin_family(tool: &str, read_hook: &str) {
    let _serial = serial_lock();

    let terminal = configure_test_terminal_env();
    let log = TestLog::new(tool);

    logln!(log, "{}", "=".repeat(60));
    logln!(
        log,
        "PTY Delivery Test: {tool} via {terminal} (bootstrap injection)"
    );
    logln!(log, "{}", "=".repeat(60));

    let pre_launch_id = {
        let out = hcom("events --last 1");
        if out.status.success() {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .filter_map(|v| v["id"].as_i64())
                .next_back()
                .unwrap_or(0)
        } else {
            0
        }
    };

    // ── Phase 1: Launch ──────────────────────────────────────────
    logln!(log, "\n[Phase 1] Launching {tool} in {terminal}...");
    let t0 = Instant::now();

    let out = hcom(&format!("--go 1 {tool}"));
    let launched_base_name = assert_launch_process_started(&out);
    if out.status.code() == Some(2) {
        logln!(
            log,
            "  INFO: launch command still starting after inline wait; continuing with screen poll"
        );
    }

    logln!(log, "  Waiting for launched instance...");
    let mut guard = InstanceGuard { base_name: None };

    let base_name: String = if let Some(base_name) = launched_base_name {
        base_name
    } else {
        poll_until(
            || {
                let out = hcom("events --action ready --last 5");
                if !out.status.success() {
                    return None;
                }
                for line in String::from_utf8_lossy(&out.stdout).lines().rev() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(ev) = serde_json::from_str::<serde_json::Value>(line)
                        && ev["type"].as_str() == Some("life")
                        && ev["data"]["action"].as_str() == Some("ready")
                        && ev["id"].as_i64().unwrap_or(0) > pre_launch_id
                    {
                        return ev["instance"].as_str().map(|s| s.to_string());
                    }
                }
                None
            },
            "ready event from launched instance",
            Duration::from_secs(60),
            Duration::from_secs(2),
        )
    };

    guard.base_name = Some(base_name.clone());
    let tag = std::env::var("HCOM_TAG").unwrap_or_default();
    let instance_name = if tag.is_empty() {
        base_name.clone()
    } else {
        format!("{tag}-{base_name}")
    };

    let t_ready = t0.elapsed();
    logln!(
        log,
        "  OK: Instance launched: {instance_name} (base: {base_name}, ready in {t_ready:.1?})"
    );

    // Wait for screen ready
    let screen: serde_json::Value = poll_until(
        || {
            let s = get_screen(&base_name)?;
            if plugin_initial_screen_ready(&s, tool) {
                Some(s)
            } else {
                None
            }
        },
        "screen ready (TUI fully rendered)",
        Duration::from_secs(30),
        Duration::from_secs(1),
    );

    logln!(log, "\n[Validate] Initial screen state for {tool}...");
    if matches!(tool, "pi" | "omp") {
        logln!(
            log,
            "  OK: {tool} screen rendered after life.ready (term ready={})",
            screen["ready"]
        );
        if screen["ready"].as_bool() == Some(true) {
            validate_ready_pattern(&screen, tool);
            logln!(
                log,
                "  OK: Ready pattern '{}' consistent",
                ready_pattern(tool)
            );
        } else {
            logln!(
                log,
                "  SKIP: {tool} ready footer scrolled out of tmux viewport after life.ready"
            );
        }
    } else {
        assert_eq!(
            screen["ready"].as_bool(),
            Some(true),
            "{tool} should be ready after poll"
        );
        logln!(log, "  OK: ready=true");
        validate_ready_pattern(&screen, tool);
        logln!(
            log,
            "  OK: Ready pattern '{}' consistent",
            ready_pattern(tool)
        );
    }
    assert!(
        screen["input_text"].is_null(),
        "{tool} input_text should be null, got {:?}",
        screen["input_text"]
    );
    logln!(log, "  OK: input_text=null (no input detection)");
    log.log_screen(&screen, &format!("{tool} — initial"));

    // ── Phase 2: Bootstrap injection (first message via PTY) ─────
    logln!(
        log,
        "\n[Phase 2] Testing bootstrap injection (first message via PTY)..."
    );

    let baseline_event = get_last_event_id(&base_name);
    logln!(log, "  OK: Baseline event ID: {baseline_event}");

    let t1 = Instant::now();
    send_msg(&format!("@{instance_name} bootstrap-test-1 do not reply"));
    logln!(log, "  OK: Message sent");

    // Wait for agent to go active
    let active_event: serde_json::Value = poll_until(
        || {
            let events = get_events(&base_name, 30, false);
            events.into_iter().find(|ev| {
                ev["id"].as_i64().unwrap_or(0) > baseline_event
                    && ev["type"].as_str() == Some("status")
                    && ev["data"]["status"].as_str() == Some("active")
            })
        },
        "agent goes active (processing bootstrap message)",
        Duration::from_secs(30),
        Duration::from_secs(1),
    );
    let active_id = active_event["id"].as_i64().unwrap_or(0);
    logln!(log, "  OK: Agent went active: event={active_id}");

    // Wait for listening after active
    let listening_event: serde_json::Value = poll_until(
        || {
            let events = get_events(&base_name, 30, false);
            events.into_iter().find(|ev| {
                ev["id"].as_i64().unwrap_or(0) > active_id
                    && ev["type"].as_str() == Some("status")
                    && ev["data"]["status"].as_str() == Some("listening")
            })
        },
        "agent returns to listening",
        Duration::from_secs(60),
        Duration::from_secs(1),
    );
    let t_delivery = t1.elapsed();
    logln!(
        log,
        "  OK: Bootstrap delivery complete: active→listening in {t_delivery:.1?}"
    );
    log.log(&format!(
        "Bootstrap: active={} listening={}",
        active_id, listening_event["id"]
    ));

    // Check hcom.log for bootstrap_inject (non-fatal)
    let log_path = dirs::home_dir().unwrap().join(".hcom/.tmp/logs/hcom.log");
    if let Ok(content) = fs::read_to_string(&log_path) {
        if content.contains("delivery.bootstrap_inject") && content.contains(&base_name) {
            logln!(log, "  OK: Bootstrap inject confirmed in hcom.log");
        } else {
            logln!(
                log,
                "  WARN: delivery.bootstrap_inject not found in hcom.log (may have rotated)"
            );
        }
    }

    let screen = get_screen(&base_name);
    if let Some(s) = &screen {
        validate_screen_schema(s);
        log.log_screen(s, &format!("{tool} — post-bootstrap-delivery"));
    }

    // ── Phase 3: Plugin delivery (second message) ────────────────
    logln!(
        log,
        "\n[Phase 3] Testing plugin delivery (second message)..."
    );

    // Wait for full quiescence before sending msg #2. The bootstrap path
    // triggers a "piggyback turn": PTY inject delivers msg #1 inline but does
    // NOT advance the read cursor. After the bootstrap turn ends and listening
    // fires, the plugin's idle handler re-fetches unread → finds msg #1 → fires
    // promptAsync → agent goes active again for ~6s until transform acks the
    // cursor. If msg #2 arrives during that window it merges into the ongoing
    // turn (no new active event fires), and the active poll below times out.
    //
    // Two co-conditions for true quiescence:
    //   1. Latest event is status=listening (agent is idle right now).
    //   2. `hcom <read-hook> --check` is "false" (cursor caught up; plugin
    //      won't re-trigger another piggyback). Listening alone is correlative
    //      — if pendingAckId or deliveryInFlight ever got stuck, listening
    //      could appear stable while the cursor was still behind.
    poll_until(
        || {
            let evs = get_events(&base_name, 1, false);
            let last = evs.last()?;
            if last["data"]["status"].as_str() != Some("listening") {
                return None;
            }
            let out = hcom(&format!("{read_hook} --name {base_name} --check"));
            if !out.status.success() {
                return None;
            }
            let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if body == "false" { Some(()) } else { None }
        },
        "agent quiescent (listening AND read cursor caught up)",
        Duration::from_secs(30),
        Duration::from_secs(1),
    );

    let baseline_event2 = get_last_event_id(&base_name);

    let t2 = Instant::now();
    send_msg(&format!("@{instance_name} plugin-test-2 do not reply"));
    logln!(log, "  OK: Second message sent");

    let active2: serde_json::Value = poll_until(
        || {
            let events = get_events(&base_name, 30, false);
            events.into_iter().find(|ev| {
                ev["id"].as_i64().unwrap_or(0) > baseline_event2
                    && ev["type"].as_str() == Some("status")
                    && ev["data"]["status"].as_str() == Some("active")
            })
        },
        "agent processes second message",
        Duration::from_secs(30),
        Duration::from_secs(1),
    );
    let active2_id = active2["id"].as_i64().unwrap_or(0);
    logln!(
        log,
        "  OK: Agent went active for second message: event={active2_id}"
    );

    poll_until(
        || {
            let events = get_events(&base_name, 30, false);
            events.into_iter().find(|ev| {
                ev["id"].as_i64().unwrap_or(0) > active2_id
                    && ev["type"].as_str() == Some("status")
                    && ev["data"]["status"].as_str() == Some("listening")
            })
        },
        "agent returns to listening after second message",
        Duration::from_secs(60),
        Duration::from_secs(1),
    );
    let t_plugin = t2.elapsed();
    logln!(
        log,
        "  OK: Plugin delivery complete: active→listening in {t_plugin:.1?}"
    );

    let screen = get_screen(&base_name);
    if let Some(s) = &screen {
        validate_screen_schema(s);
        log.log_screen(s, &format!("{tool} — post-plugin-delivery"));
    }

    // Log all events
    let all_events = get_events(&base_name, 50, false);
    log.log(&format!("\n── All events for {instance_name}"));
    for ev in &all_events {
        log.log(&serde_json::to_string(ev).unwrap());
    }

    // Cleanup handled by guard Drop
    logln!(log, "\n{}", "=".repeat(60));
    logln!(log, "{} — ALL PHASES PASSED", tool.to_uppercase());
    logln!(log, "  Log: {}", log.timestamped.display());
    logln!(log, "{}", "=".repeat(60));
}

// ── Test entries ───────────────────────────────────────────────────────

#[test]
#[ignore]
fn test_pty_claude() {
    run_pty_test("claude");
}

#[test]
#[ignore]
fn test_pty_gemini() {
    run_pty_test("gemini");
}

#[test]
#[ignore]
fn test_pty_codex() {
    run_pty_test("codex");
}

#[test]
#[ignore]
fn test_pty_opencode() {
    run_pty_test_plugin_family("opencode", "opencode-read");
}

#[test]
#[ignore]
fn test_pty_kilo() {
    run_pty_test_plugin_family("kilo", "opencode-read");
}

#[test]
#[ignore]
fn test_pty_pi() {
    run_pty_test_plugin_family("pi", "pi-read");
}
#[test]
#[ignore]
fn test_pty_omp() {
    run_pty_test_plugin_family("omp", "omp-read");
}

#[test]
#[ignore]
fn test_pty_antigravity() {
    run_pty_test("antigravity");
}

#[test]
#[ignore]
fn test_pty_cursor() {
    run_pty_test("cursor");
}

#[test]
#[ignore]
fn test_pty_copilot() {
    run_pty_test("copilot");
}
