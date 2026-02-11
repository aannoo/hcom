//! Delivery loop for injecting messages into PTY
//!
//! Ported from Python push_delivery.py - notify-driven delivery with:
//! - Gate checks (idle, ready, prompt empty, output stable, user activity, approval)
//! - Three-phase verification (text render -> text clear -> cursor advance)
//! - Bounded retry with backoff
//!
//! ## Design Goals
//!
//! - Zero periodic DB polling when there are no pending messages
//! - Delivery attempts happen only after a wake event or bounded retry tick
//! - When unsafe (not at prompt, user typing, approval), retry backs off
//! - Verify delivery via cursor advance (hook reads messages, advances cursor)
//!
//! ## States
//!
//! - `Idle`: No pending messages, sleeping on notifier
//! - `Pending`: Messages exist, waiting for safe gate to inject
//! - `WaitTextRender`: Text-only injected, waiting for text to appear in input box
//! - `WaitTextClear`: Enter sent, waiting for text to clear from input box
//! - `VerifyCursor`: Waiting for cursor advance to confirm delivery

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::HcomDb;
use crate::log::{log_info, log_warn, log_error};
use crate::notify::NotifyServer;

/// Safely truncate a string to at most `max_chars` characters.
/// Unlike byte slicing `&s[..n]`, this won't panic on multi-byte UTF-8.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Build full display name: "{tag}-{name}" if tag exists, else "{name}".
fn full_display_name(db: &HcomDb, name: &str) -> String {
    match db.get_instance_tag(name) {
        Some(tag) => format!("{}-{}", tag, name),
        None => name.to_string(),
    }
}

/// Map status to icon (matches TUI/hcom list format)
pub fn status_icon(status: &str) -> &'static str {
    match status {
        "listening" => "◉",
        "active" => "▶",
        "blocked" => "■",
        "stopped" => "⊘",
        _ => "○", // inactive/unknown
    }
}

/// Human-readable descriptions for gate block reasons.
pub(crate) fn gate_block_detail(reason: &str) -> &'static str {
    match reason {
        "not_idle" => "waiting for idle status",
        "user_active" => "user is typing",
        "not_ready" => "prompt not visible",
        "output_unstable" => "output still streaming",
        "prompt_has_text" => "uncommitted text in prompt",
        "approval" => "waiting for user approval",
        _ => "blocked",
    }
}

/// Build message preview with DB access for Gemini/Codex injection.
///
/// Format: `<hcom>sender → recipient (+N)</hcom>`
///
/// ## Why different tools need different injection strategies:
///
/// - **Claude**: Injects minimal `<hcom>` trigger only. The Claude hook shows the full
///   message to human via system message in TUI + separate JSON for agent. Minimal
///   trigger is sufficient since hook handles both human and agent presentation.
///
/// - **Gemini**: Injects message preview for human visibility. The Gemini hook only
///   shows JSON to agent (no human-visible system message like Claude). Preview in
///   terminal gives human context since hook output is agent-only. BeforeAgent hook
///   still delivers full message to agent via additionalContext.
///
/// - **Codex**: Injects message preview + hcom listen instruction. Preview shows human
///   message context in terminal (like Gemini). Bash command output is truncated for
///   agent only (command execution-based delivery). No BeforeAgent-style hook exists -
///   Codex executes 'hcom listen' as shell command.
fn build_message_preview_with_db(db: &HcomDb, name: &str) -> String {
    let messages = db.get_unread_messages(name);
    if messages.is_empty() {
        return "<hcom></hcom>".to_string();
    }

    // Build preview from first message, matching Python format:
    // [intent:thread #id] sender → recipient
    let msg = &messages[0];

    // Build prefix
    let prefix = match (&msg.intent, &msg.thread) {
        (Some(i), Some(t)) => format!("{}:{}", i, t),
        (Some(i), None) => i.clone(),
        (None, Some(t)) => format!("thread:{}", t),
        (None, None) => "new message".to_string(),
    };
    let id_ref = msg.event_id.map(|id| format!(" #{}", id)).unwrap_or_default();
    let envelope = format!("[{}{}]", prefix, id_ref);

    let preview = if messages.len() == 1 {
        format!("{} {} → {}", envelope, msg.from, name)
    } else {
        format!("{} {} → {} (+{})", envelope, msg.from, name, messages.len() - 1)
    };

    // Truncate if needed (max 60 chars total)
    let wrapper_len = "<hcom></hcom>".len();
    let max_content = 60 - wrapper_len;
    let content = if preview.len() > max_content {
        format!("{}...", &preview[..max_content.saturating_sub(3)])
    } else {
        preview
    };

    format!("<hcom>{}</hcom>", content)
}

/// Build Codex inject text with hint after failed inject
/// Format: <hcom>sender → recipient (+N)</hcom> | Run: hcom listen
fn build_codex_inject_with_hint(db: &HcomDb, name: &str) -> String {
    let preview = build_message_preview_with_db(db, name);
    format!("{} | Run: hcom listen", preview)
}

/// Tool-specific configuration for delivery gate.
///
/// ## Status Semantics
///
/// - `status="blocked"` - Permission prompt showing. Set by:
///   - Claude/Gemini: hooks detect approval prompt
///   - Codex: PTY detects OSC9 escape sequence (primary mechanism, no hooks)
/// - `status="active"` - Agent processing. Messages not delivering is normal, no alert.
/// - `status="listening"` - Agent idle. Can show status_context for delivery issues.
///
/// ## Gate Logic
///
/// The gate answers one question: "If we inject a single line + Enter right now,
/// will it land as a fresh user turn without clobbering an approval prompt,
/// a running command, or the user's typing?"
///
/// NOTE: Gate check order determines gate.reason, but status updates check
/// screen.approval directly so Codex OSC9 works even when agent is active.
///
/// Gate checks are evaluated in order (fails fast):
/// 1. `require_idle` - DB status must be "listening" (set by hooks after turn completes).
///    Claude/Gemini hooks also set status="blocked" on approval which fails this check.
/// 2. `block_on_approval` - No pending approval prompt (OSC9 detection in PTY).
/// 3. `block_on_user_activity` - No keystrokes within cooldown (default 0.5s, 3s for Claude).
/// 4. `require_ready_prompt` - Ready pattern visible on screen (e.g., "? for shortcuts").
///    Pattern hidden when user has uncommitted text or is in a submenu (slash menu).
///    Note: Claude hides this in accept-edits mode, so Claude disables this check.
/// 5. `require_prompt_empty` - Check if prompt has no user text.
///    Claude-specific: Uses VT100 dim attribute detection to distinguish placeholder text
///    (dim) from user input (not dim). Implemented in screen.rs get_claude_input_text(). 
/// 6. `require_output_stable_seconds` - Screen unchanged for N seconds. Disabled for all tools since hooks already signal idle state reliably.
#[derive(Clone)]
pub struct ToolConfig {
    /// Tool name (claude, gemini, codex)
    pub tool: String,
    /// Require DB status == "listening" before inject
    pub require_idle: bool,
    /// Require ready pattern visible on screen
    pub require_ready_prompt: bool,
    /// Require prompt to be empty (no user text)
    pub require_prompt_empty: bool,
    /// Seconds of output stability required
    pub require_output_stable_seconds: f64,
    /// Block if user is actively typing
    pub block_on_user_activity: bool,
    /// Block if approval prompt detected
    pub block_on_approval: bool,
}

impl ToolConfig {
    /// Get config for Claude.
    ///
    /// - `require_ready_prompt=false`: Status bar ("? for shortcuts") hides in accept-edits mode.
    /// - `require_prompt_empty=true`: Uses vt100 dim attribute detection to distinguish
    ///   placeholder text from user input. Placeholder (dim) = safe, user text (not dim) = block.
    /// - `require_output_stable_seconds=0`: Disabled; hooks already signal idle state.
    pub fn claude() -> Self {
        Self {
            tool: "claude".to_string(),
            require_idle: true,
            require_ready_prompt: false,
            require_prompt_empty: true,
            require_output_stable_seconds: 0.0,
            block_on_user_activity: true,
            block_on_approval: true,
        }
    }

    /// Get config for Gemini.
    ///
    /// - `require_ready_prompt=true`: "Type your message" placeholder disappears instantly when
    ///   user types. Pattern visibility indicates 100% empty prompt. (but could be processing or idle)
    /// - `require_output_stable_seconds=0`: Disabled; hooks already signal idle state.
    ///
    /// Note: Previously used DebouncedIdleChecker (0.4s debounce) because AfterAgent fired
    /// multiple times per turn during tool loops. However, Gemini CLI commit 15c9f88da
    /// (Dec 2025) fixed the underlying skipNextSpeakerCheck bug - AfterAgent now fires
    /// consistently after processTurn completes, making debouncing unnecessary.
    pub fn gemini() -> Self {
        Self {
            tool: "gemini".to_string(),
            require_idle: true,
            require_ready_prompt: true,
            require_prompt_empty: false,
            require_output_stable_seconds: 0.0,  // Disabled: hooks already signal idle state
            block_on_user_activity: true,
            block_on_approval: true,
        }
    }

    /// Get config for Codex.
    ///
    /// - `require_ready_prompt=false`: "? for shortcuts" is dropped by Codex's responsive
    ///   footer in narrow terminals, making it unreliable as a gate signal.
    /// - `require_prompt_empty=true`: Uses vt100 dim attribute detection on the `›` prompt
    ///   character (always visible) to distinguish placeholder text from user input.
    /// - `require_idle=true`: Status tracking is reliable (~5s lag from transcript watcher).
    ///   Status correctly shows `listening` when idle, `active` when busy.
    pub fn codex() -> Self {
        Self {
            tool: "codex".to_string(),
            require_idle: true,
            require_ready_prompt: false,
            require_prompt_empty: true,
            require_output_stable_seconds: 0.0,
            block_on_user_activity: true,
            block_on_approval: true,
        }
    }

    /// Get config by tool name
    pub fn for_tool(tool: &str) -> Self {
        use crate::tool::Tool;
        use std::str::FromStr;

        match Tool::from_str(tool) {
            Ok(Tool::Claude) => Self::claude(),
            Ok(Tool::Gemini) => Self::gemini(),
            Ok(Tool::Codex) => Self::codex(),
            Err(_) => Self::claude(), // Default to Claude config for unknown tools
        }
    }
}

/// Gate evaluation result
pub struct GateResult {
    pub safe: bool,
    pub reason: &'static str,
}

/// Shared state for delivery thread
pub struct DeliveryState {
    pub screen: Arc<std::sync::RwLock<ScreenState>>,
    pub inject_port: u16,
    pub user_activity_cooldown_ms: u64,
}

/// Screen state snapshot for gate checks
#[derive(Clone)]
pub struct ScreenState {
    pub ready: bool,
    pub approval: bool,
    pub output_stable_1s: bool,
    pub prompt_empty: bool,
    pub input_text: Option<String>,
    pub last_user_input: Instant,
    /// Timestamp of last output (for stability-based recovery)
    pub last_output: Instant,
    /// Terminal width in columns
    pub cols: u16,
}

impl Default for ScreenState {
    fn default() -> Self {
        Self {
            ready: false,
            approval: false,
            output_stable_1s: false,
            prompt_empty: false,
            input_text: None,
            last_user_input: Instant::now(),
            last_output: Instant::now(),
            cols: 80,
        }
    }
}

impl DeliveryState {
    /// Check if user is actively typing (within cooldown)
    fn is_user_active(&self) -> bool {
        let screen = self.screen.read().unwrap();
        screen.last_user_input.elapsed().as_millis() < self.user_activity_cooldown_ms as u128
    }

    /// Check if user is actively typing using existing screen guard (avoids double lock)
    fn is_user_active_with_guard(&self, screen: &ScreenState) -> bool {
        screen.last_user_input.elapsed().as_millis() < self.user_activity_cooldown_ms as u128
    }
}

/// Evaluate gate conditions for message injection.
///
/// Returns whether it's safe to inject AND the reason if not.
/// NOTE: This only determines injection safety. Status updates (setting "blocked")
/// happen separately in the delivery loop by checking screen.approval directly.
///
/// Check order determines gate.reason but NOT status behavior:
/// 1. require_idle - if agent active, reason="not_idle"
/// 2. approval - if approval showing, reason="approval"
/// 3. etc.
///
/// The delivery loop checks screen.approval directly for status="blocked",
/// so Codex OSC9 detection works even when agent is active (gate returns "not_idle").
pub(crate) fn evaluate_gate(
    config: &ToolConfig,
    state: &DeliveryState,
    is_idle: bool,
) -> GateResult {
    let screen = state.screen.read().unwrap();

    // Check idle FIRST - if agent is busy, that's normal, don't alert
    if config.require_idle && !is_idle {
        return GateResult { safe: false, reason: "not_idle" };
    }
    // Approval check only runs if agent is idle (passed require_idle)
    if config.block_on_approval && screen.approval {
        return GateResult { safe: false, reason: "approval" };
    }
    if config.block_on_user_activity && state.is_user_active_with_guard(&screen) {
        return GateResult { safe: false, reason: "user_active" };
    }
    if config.require_ready_prompt && !screen.ready {
        return GateResult { safe: false, reason: "not_ready" };
    }
    if config.require_prompt_empty && !screen.prompt_empty {
        return GateResult { safe: false, reason: "prompt_has_text" };
    }
    // Check output stability (skip if <= 0, which disables the check)
    if config.require_output_stable_seconds > 0.0 && !screen.output_stable_1s {
        return GateResult { safe: false, reason: "output_unstable" };
    }

    GateResult { safe: true, reason: "ok" }
}

/// Inject text to PTY via TCP (text only, no Enter).
/// Strips all C0 control chars (0x00-0x1F) except tab. This blocks ESC (0x1B),
/// so ANSI escape sequences cannot pass through.
fn inject_text(port: u16, text: &str) -> bool {
    let safe_text: String = text.chars()
        .filter(|c| *c >= ' ' || *c == '\t')  // >= 0x20 or tab; blocks ESC, NULL, BEL, etc.
        .collect();

    if safe_text.is_empty() {
        return false;
    }

    match TcpStream::connect(format!("127.0.0.1:{}", port)) {
        Ok(mut stream) => {
            stream.write_all(safe_text.as_bytes()).is_ok()
        }
        Err(_) => false,
    }
}

/// Inject Enter key to PTY via TCP
fn inject_enter(port: u16) -> bool {
    match TcpStream::connect(format!("127.0.0.1:{}", port)) {
        Ok(mut stream) => {
            stream.write_all(b"\r").is_ok()
        }
        Err(_) => false,
    }
}

/// Two-phase retry policy with warm and cold phases.
///
/// Keeps retry maximum low for the first N seconds after messages become pending,
/// then allows a higher maximum (lower overhead) if the tool stays unsafe.
///
/// ## Why two phases?
///
/// - **Warm phase (0-60s)**: Fast retries (max 2s) for transient blocks.
///   Most delivery blocks are brief (user types, then stops; AI finishes turn).
///   Fast retries ensure messages arrive quickly once the block clears.
///
/// - **Cold phase (60s+)**: Slow retries (max 5s) for persistent blocks.
///   If the tool is genuinely unavailable (user walked away, long AI task),
///   slower retries reduce CPU usage and log spam without losing messages.
pub(crate) struct TwoPhaseRetryPolicy {
    /// Initial delay before first retry (seconds)
    initial: f64,
    /// Exponential backoff multiplier
    multiplier: f64,
    /// Maximum delay during warm phase (seconds)
    warm_maximum: f64,
    /// Duration of warm phase (seconds)
    warm_seconds: f64,
    /// Maximum delay during cold phase (seconds)
    cold_maximum: f64,
}

impl TwoPhaseRetryPolicy {
    pub(crate) fn default_policy() -> Self {
        Self {
            initial: 0.25,
            multiplier: 2.0,
            warm_maximum: 2.0,
            warm_seconds: 60.0,
            cold_maximum: 5.0,
        }
    }

    pub(crate) fn delay(&self, attempt: u32, pending_for: Option<Duration>) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        // Cap exponent to prevent overflow (2^10 = 1024 is already way past max)
        let exp = (attempt - 1).min(10) as i32;
        let d = self.initial * self.multiplier.powi(exp);

        // Use cold maximum after warm_seconds of pending
        let max_delay = match pending_for {
            Some(dur) if dur.as_secs_f64() >= self.warm_seconds => self.cold_maximum,
            _ => self.warm_maximum,
        };

        Duration::from_secs_f64(d.min(max_delay))
    }
}

/// Delivery loop states
#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Idle,
    Pending,
    WaitTextRender,
    WaitTextClear,
    VerifyCursor,
}

/// Run the delivery loop
///
/// This is the main delivery thread function. It:
/// 1. Waits for messages (notify-driven)
/// 2. Evaluates gate conditions
/// 3. Injects text and verifies delivery
/// 4. Retries with backoff on failure
///
/// The optional `shared_name` and `shared_status` Arcs are updated on rebind/status change
/// to keep the main PTY loop's OSC title override in sync.
#[allow(clippy::too_many_arguments)] // Tracked: hook-comms-8vs (refactor delivery loop)
pub fn run_delivery_loop(
    running: Arc<AtomicBool>,
    db: &HcomDb,
    notify: &NotifyServer,
    state: &DeliveryState,
    instance_name: &str,
    config: &ToolConfig,
    shared_name: Option<Arc<std::sync::RwLock<String>>>,
    shared_status: Option<Arc<std::sync::RwLock<String>>>,
) {
    let retry = TwoPhaseRetryPolicy::default_policy();
    let idle_wait = Duration::from_secs(30);

    // Phase timeouts
    let phase1_timeout = Duration::from_secs(2);
    let phase2_timeout = Duration::from_secs(2);
    let verify_timeout = Duration::from_secs(10);
    let max_enter_attempts = 3;

    // Resolve authoritative instance name from process binding (like Python PTY does).
    // The instance_name parameter is a fallback - the binding is the source of truth
    // because it can change (e.g., Claude session resume switches to canonical instance).
    let process_id = Config::get().process_id.unwrap_or_default();
    let mut current_name = if !process_id.is_empty() {
        match db.get_process_binding(&process_id) {
            Ok(Some(name)) => name,
            Ok(None) => instance_name.to_string(),
            Err(e) => {
                log_error("native", "delivery.init", &format!(
                    "DB error getting process binding: {} - using instance_name", e
                ));
                instance_name.to_string()
            }
        }
    } else {
        instance_name.to_string()
    };

    log_info("native", "delivery.init", &format!(
        "Delivery loop starting: name={}, process_id={}, tool={}, require_idle={}",
        current_name, process_id, config.tool, config.require_idle
    ));

    // Set initial listening status AFTER resolving authoritative name
    if let Err(e) = db.set_status(&current_name, "listening", "start") {
        log_error("native", "delivery.status.fail", &format!("Failed to set initial status: {}", e));
    }

    // Set tcp_mode flag to indicate native PTY is handling delivery.
    // Also re-asserted on every heartbeat (self-heals after DB reset/instance recreation).
    if let Err(e) = db.update_tcp_mode(&current_name, true) {
        log_warn("native", "delivery.tcp_mode_fail", &format!("Failed to set tcp_mode: {}", e));
    } else {
        log_info("native", "delivery.tcp_mode", &format!("Set tcp_mode=true for {}", current_name));
    }

    // Set shared display name for PTY title (tag-name or just name)
    if let Some(ref shared) = shared_name {
        if let Ok(mut s) = shared.write() {
            *s = full_display_name(db, &current_name);
        }
    }

    // State machine
    let mut delivery_state = State::Pending; // Start pending to check immediately
    let mut attempt: u32 = 0;
    let mut inject_attempt: u32 = 0;
    let mut enter_attempt: u32 = 0;
    let mut injected_text = String::new();
    let mut phase_started_at = Instant::now();
    let mut cursor_before: i64 = 0;
    let mut pending_since: Option<Instant> = Some(Instant::now()); // Track for two-phase retry

    // Gate block tracking for TUI status updates
    let mut block_since: Option<Instant> = None;
    let mut last_block_context: String = String::new();

    // Status tracking for terminal title updates
    let mut current_status = "listening".to_string();

    while running.load(Ordering::Acquire) {
        // Check for binding refresh (instance name change)
        if !process_id.is_empty() {
            match db.get_process_binding(&process_id) {
                Ok(Some(new_name)) if new_name != current_name => {
                    log_info("native", "delivery.binding_refresh", &format!(
                        "Instance name changed: {} -> {}", current_name, new_name
                    ));
                    // Migrate notify endpoints to new name
                    let _ = db.migrate_notify_endpoints(&current_name, &new_name);
                    // Update tcp_mode for new name
                    let _ = db.update_tcp_mode(&new_name, true);
                    // Update shared display name for main loop's title tracking
                    if let Some(ref shared) = shared_name {
                        if let Ok(mut s) = shared.write() {
                            *s = full_display_name(db, &new_name);
                        }
                    }
                    current_name = new_name;
                }
                Ok(None) => {
                    // No binding found - normal case, continue with current_name
                }
                Ok(Some(_)) => {
                    // Name hasn't changed - normal case
                }
                Err(e) => {
                    log_error("native", "delivery.binding_refresh", &format!(
                        "DB error checking process binding: {}", e
                    ));
                }
            }
        }

        // Check for status change (Python hooks update DB, notify wakes us)
        // None = instance deleted, Err = DB error, show stopped for both
        let new_status = match db.get_status(&current_name) {
            Ok(Some((status, _))) => status,
            Ok(None) => "stopped".to_string(), // Instance deleted
            Err(e) => {
                log_error("native", "delivery.status_check", &format!(
                    "DB error getting status: {}", e
                ));
                "stopped".to_string()
            }
        };
        if new_status != current_status {
            // Update shared status for main loop's title tracking
            if let Some(ref shared) = shared_status {
                if let Ok(mut s) = shared.write() {
                    *s = new_status.clone();
                }
            }
            current_status = new_status;
        }

        // Refresh display name in case tag changed at runtime (TUI Ctrl+T, config set)
        if let Some(ref shared) = shared_name {
            let new_display = full_display_name(db, &current_name);
            if let Ok(mut s) = shared.write() {
                if *s != new_display {
                    *s = new_display;
                }
            }
        }

        match delivery_state {
            State::Idle => {
                // Capture wall clock before wait to detect system sleep
                let wall_before = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                // Wait for notification or timeout
                let notified = notify.wait(idle_wait);

                if !running.load(Ordering::Acquire) {
                    log_info("native", "delivery.shutdown", "Running flag cleared, exiting loop");
                    break;
                }

                // Detect sleep/wake: wall clock jumped more than expected for idle_wait
                let wall_after = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let wall_elapsed = wall_after.saturating_sub(wall_before);
                if wall_elapsed > 45 {
                    log_info("native", "delivery.sleep_wake", &format!(
                        "System sleep detected for {}: wall clock jumped {}s during 30s poll",
                        current_name, wall_elapsed
                    ));
                }

                // Update heartbeat to prove we're alive (also re-asserts tcp_mode=true)
                if let Err(e) = db.update_heartbeat(&current_name) {
                    log_warn("native", "delivery.heartbeat_fail", &format!("Failed to update heartbeat: {}", e));
                }
                // Re-register endpoints (self-heals after DB reset/instance recreation)
                let _ = db.register_notify_port(&current_name, notify.port());
                let _ = db.register_inject_port(&current_name, state.inject_port);

                // Check for pending messages
                let has_pending = db.has_pending(&current_name);
                if has_pending {
                    log_info("native", "delivery.wake", &format!(
                        "Woke up (notified={}) with pending messages for {}",
                        notified, current_name
                    ));
                    delivery_state = State::Pending;
                    pending_since = Some(Instant::now()); // Start tracking pending time
                } else if notified {
                    // Woke by notification but no pending messages — log for diagnostics
                    log_info("native", "delivery.wake_no_pending", &format!(
                        "Woke up (notified=true) but no pending messages for {}",
                        current_name
                    ));
                }
            }

            State::Pending => {
                // Check if still pending
                if !db.has_pending(&current_name) {
                    log_info("native", "delivery.no_pending", &format!(
                        "No pending messages for {}", current_name
                    ));
                    delivery_state = State::Idle;
                    attempt = 0;
                    pending_since = None; // Clear pending tracking
                    continue;
                }

                // Evaluate gate
                let is_idle = if config.require_idle {
                    db.is_idle(&current_name)
                } else {
                    true
                };

                let gate = evaluate_gate(config, state, is_idle);

                if gate.safe {
                    log_info("native", "delivery.gate_pass", &format!(
                        "Gate passed, injecting to port {}",
                        state.inject_port
                    ));

                    // Snapshot cursor before injection
                    cursor_before = db.get_cursor(&current_name);

                    // Re-check pending immediately before inject
                    if !db.has_pending(&current_name) {
                        delivery_state = State::Idle;
                        attempt = 0;
                        pending_since = None;
                        inject_attempt = 0;
                        continue;
                    }

                    // Build inject text - use DB for Gemini/Codex message preview
                    // Codex: use hint version after failed inject attempt
                    use crate::tool::Tool;
                    use std::str::FromStr;

                    let parsed_tool = Tool::from_str(&config.tool).ok();
                    let text = match parsed_tool {
                        Some(Tool::Claude) => "<hcom>".to_string(),
                        Some(Tool::Codex) if inject_attempt > 0 => {
                            // Codex retry: add hint to prompt agent to run hcom listen
                            build_codex_inject_with_hint(db, &current_name)
                        }
                        _ => {
                            // Gemini/Codex first attempt: build preview from DB
                            build_message_preview_with_db(db, &current_name)
                        }
                    };
                    // Contract to minimal <hcom> if preview won't fit in input box
                    let cols = state.screen.read().map(|s| s.cols).unwrap_or(80);
                    let input_box_width = (cols as usize).saturating_sub(15).max(10);
                    let text = if text.len() > input_box_width {
                        "<hcom>".to_string()
                    } else {
                        text
                    };

                    if inject_text(state.inject_port, &text) {
                        log_info("native", "delivery.injected", &format!(
                            "Injected '{}' (len={}, inject_attempt={})",
                            truncate_chars(&text, 40),
                            text.len(),
                            inject_attempt
                        ));
                        injected_text = text;
                        phase_started_at = Instant::now();
                        enter_attempt = 0;
                        delivery_state = State::WaitTextRender;
                        continue;  // Skip retry delay - now in WaitTextRender phase
                    } else {
                        log_warn("native", "delivery.inject_fail", "TCP inject failed");
                        attempt += 1;
                    }
                } else {
                    // Gate blocked - refresh heartbeat so we don't go stale while waiting
                    // (DB status is still "listening" until message is delivered and hooks fire)
                    let _ = db.update_heartbeat(&current_name);

                    // Log gate failure
                    if attempt == 0 || attempt % 5 == 0 {
                        let screen = state.screen.read().unwrap();
                        log_info("native", "delivery.gate_blocked", &format!(
                            "Gate blocked: {} (attempt={}, ready={}, approval={}, stable={}, user_active={})",
                            gate.reason, attempt,
                            screen.ready, screen.approval, screen.output_stable_1s,
                            state.is_user_active()
                        ));
                    }

                    // Track when blocking started
                    if block_since.is_none() {
                        block_since = Some(Instant::now());
                    }

                    // Update status based on PTY-detected approval
                    // Check screen.approval directly, not gate.reason (gate may return
                    // "not_idle" even when approval is showing due to check order)
                    let approval_showing = {
                        let screen = state.screen.read().unwrap();
                        screen.approval
                    };
                    if approval_showing {
                        // Approval detected via PTY (OSC9 for Codex, screen pattern for others)
                        // Set status="blocked" immediately:
                        // - Codex: OSC9 is THE primary mechanism (no hooks)
                        // - Claude/Gemini: fallback if hooks didn't fire
                        let _ = db.set_status(&current_name, "blocked", "pty:approval");
                    } else if gate.reason == "not_idle" {
                        // Stability-based recovery: if status stuck "active" but output stable 10s,
                        // assume ESC cancelled or hook didn't fire - flip to listening.
                        // NOTE: Rust stability tracking is less accurate than Python's pyte dirty
                        // tracking (false positives from escape sequences), but still useful for
                        // true idle detection when no data arrives at all.
                        match db.get_status(&current_name) {
                            Ok(Some((status, _))) if status == "active" => {
                                let screen = state.screen.read().unwrap();
                                let stable_10s = screen.last_output.elapsed().as_millis() > 10000;
                                drop(screen);
                                if stable_10s {
                                    let _ = db.set_status(&current_name, "listening", "pty:recovered");
                                    log_info("native", "delivery.recovered", &format!(
                                        "Status recovered: output stable 10s, {} -> listening", status
                                    ));
                                    attempt = 0;
                                    continue;
                                }
                            }
                            Ok(Some(_)) | Ok(None) => {
                                // Status not "active" or not found - skip recovery
                            }
                            Err(e) => {
                                log_error("native", "delivery.recovery_check", &format!(
                                    "DB error checking status: {}", e
                                ));
                            }
                        }
                        // Fall through to TUI status update
                        if let Some(since) = block_since {
                            if since.elapsed().as_secs_f64() >= 2.0 {
                                match db.get_status(&current_name) {
                                    Ok(Some((status, _))) if status == "listening" => {
                                        let context = "tui:not-idle".to_string();
                                        if context != last_block_context {
                                            let _ = db.set_gate_status(&current_name, &context, "waiting for idle status");
                                            last_block_context = context;
                                        }
                                    }
                                    Ok(Some(_)) | Ok(None) => {
                                        // Status not "listening" or not found - skip
                                    }
                                    Err(e) => {
                                        log_error("native", "delivery.tui_status_update", &format!(
                                            "DB error checking status: {}", e
                                        ));
                                    }
                                }
                            }
                        }
                    } else if let Some(since) = block_since {
                        // After 2 seconds of blocking, update TUI status context
                        if since.elapsed().as_secs_f64() >= 2.0 {
                            // Only update if status is "listening" (don't overwrite active/blocked)
                            match db.get_status(&current_name) {
                                Ok(Some((status, _))) if status == "listening" => {
                                    // Format context like Python: tui:not-ready, tui:user-active, etc.
                                    let reason_formatted = gate.reason.replace("_", "-");
                                    let context = format!("tui:{}", reason_formatted);

                                    // Only update if context changed
                                    if context != last_block_context {
                                        let detail = gate_block_detail(gate.reason);
                                        let _ = db.set_gate_status(&current_name, &context, detail);
                                        last_block_context = context;
                                    }
                                }
                                Ok(Some(_)) | Ok(None) => {
                                    // Status not "listening" or not found - skip
                                }
                                Err(e) => {
                                    log_error("native", "delivery.gate_status_update", &format!(
                                        "DB error checking status: {}", e
                                    ));
                                }
                            }
                        }
                    }

                    attempt += 1;
                }

                // Wait before retry (two-phase: warm 2s for 60s, then cold 5s)
                let pending_for = pending_since.map(|t| t.elapsed());
                let delay = retry.delay(attempt, pending_for);
                if !delay.is_zero() {
                    let notified = notify.wait(delay);
                    if notified {
                        attempt = 0; // Reset on notification
                    }
                }
            }

            State::WaitTextRender => {
                let elapsed = phase_started_at.elapsed();

                if elapsed > phase1_timeout {
                    // Timeout - retry from pending
                    log_warn("native", "delivery.phase1_timeout", &format!(
                        "Text render timeout after {:?}, inject_attempt={}", elapsed, inject_attempt
                    ));
                    delivery_state = State::Pending;
                    inject_attempt += 1;
                    attempt += 1;
                    continue;
                }

                // Check if injected text appeared in input box
                let screen = state.screen.read().unwrap();
                // Debug: log what we see at start and every 500ms
                if elapsed.as_millis() < 50 || elapsed.as_millis() % 500 < 50 {
                    log_info("native", "delivery.phase1_poll", &format!(
                        "t={}ms input={:?} want={} ready={}",
                        elapsed.as_millis(),
                        screen.input_text.as_deref().unwrap_or("None"),
                        truncate_chars(&injected_text, 25),
                        screen.ready
                    ));
                }
                if let Some(ref input_text) = screen.input_text {
                    if !injected_text.is_empty() && input_text.contains(&injected_text) {
                        drop(screen);
                        log_info("native", "delivery.text_rendered",
                            "Injected text appeared in input box, sending Enter"
                        );
                        // Text appeared - send Enter
                        delivery_state = State::WaitTextClear;
                        phase_started_at = Instant::now();
                        enter_attempt = 0;

                        // Send Enter if safe
                        if !state.is_user_active() {
                            let screen = state.screen.read().unwrap();
                            if !screen.approval {
                                drop(screen);
                                log_info("native", "delivery.send_enter", "Sending Enter key");
                                inject_enter(state.inject_port);
                            } else {
                                log_info("native", "delivery.enter_blocked", "Enter blocked by approval prompt");
                            }
                        } else {
                            log_info("native", "delivery.enter_blocked", "Enter blocked by user activity");
                        }
                        continue;
                    }
                }
                drop(screen);

                std::thread::sleep(Duration::from_millis(10));
            }

            State::WaitTextClear => {
                let elapsed = phase_started_at.elapsed();

                // Check if text cleared (prompt is empty)
                let screen = state.screen.read().unwrap();
                let input_text = screen.input_text.clone();
                let text_cleared = input_text.as_ref().map(|t| t.is_empty()).unwrap_or(false);
                drop(screen);

                if text_cleared {
                    // Text cleared - verify cursor advance
                    log_info("native", "delivery.text_cleared", "Input box cleared, verifying cursor");
                    delivery_state = State::VerifyCursor;
                    phase_started_at = Instant::now();
                    continue;
                }

                if elapsed > phase2_timeout {
                    if enter_attempt < max_enter_attempts {
                        // Retry Enter with backoff
                        let screen = state.screen.read().unwrap();
                        let can_send = !state.is_user_active() && !screen.approval;
                        drop(screen);

                        if can_send {
                            log_info("native", "delivery.retry_enter", &format!(
                                "Retrying Enter (attempt={}, input_text={:?})",
                                enter_attempt, input_text
                            ));
                            inject_enter(state.inject_port);
                            enter_attempt += 1;
                            phase_started_at = Instant::now();
                            let backoff = Duration::from_millis(200 * (1 << enter_attempt));
                            std::thread::sleep(backoff);
                        } else {
                            log_info("native", "delivery.enter_retry_blocked", &format!(
                                "Enter retry blocked (user_active={})", state.is_user_active()
                            ));
                        }
                        continue;
                    }

                    // Max retries - go back to pending
                    log_warn("native", "delivery.phase2_max_retries", &format!(
                        "Max Enter retries ({}) reached, going back to pending", max_enter_attempts
                    ));
                    delivery_state = State::Pending;
                    inject_attempt += 1;
                    attempt += 1;
                    continue;
                }

                std::thread::sleep(Duration::from_millis(10));
            }

            State::VerifyCursor => {
                let elapsed = phase_started_at.elapsed();

                // Check if cursor advanced (hook processed messages)
                let current_cursor = db.get_cursor(&current_name);
                if current_cursor > cursor_before {
                    // Success! Clear gate block status
                    if !last_block_context.is_empty() {
                        let _ = db.set_gate_status(&current_name, "", "");
                        last_block_context.clear();
                    }
                    block_since = None;

                    log_info("native", "delivery.success", &format!(
                        "Cursor advanced {} -> {}, delivery successful",
                        cursor_before, current_cursor
                    ));
                    if db.has_pending(&current_name) {
                        log_info("native", "delivery.more_pending", "More messages pending, continuing");
                        delivery_state = State::Pending;
                        pending_since = Some(Instant::now()); // Reset pending timer for new batch
                    } else {
                        log_info("native", "delivery.complete", "All messages delivered, going idle");
                        delivery_state = State::Idle;
                        pending_since = None;
                    }
                    attempt = 0;
                    inject_attempt = 0;
                    continue;
                }

                if elapsed > verify_timeout {
                    inject_attempt += 1;
                    log_warn("native", "delivery.verify_timeout", &format!(
                        "Cursor verify timeout (before={}, current={}, inject_attempt={})",
                        cursor_before, current_cursor, inject_attempt
                    ));

                    if inject_attempt < 3 {
                        // Retry
                        log_info("native", "delivery.retry", &format!(
                            "Retrying delivery (inject_attempt={})", inject_attempt
                        ));
                        delivery_state = State::Pending;
                        attempt += 1;
                        continue;
                    }

                    // Check if messages are actually gone
                    if !db.has_pending(&current_name) {
                        // Success (cursor tracking issue but delivery worked)
                        // Clear gate block status
                        if !last_block_context.is_empty() {
                            let _ = db.set_gate_status(&current_name, "", "");
                            last_block_context.clear();
                        }
                        block_since = None;

                        log_info("native", "delivery.success_no_cursor",
                            "Messages gone despite cursor not advancing - delivery successful"
                        );
                        delivery_state = State::Idle;
                        pending_since = None;
                        attempt = 0;
                        inject_attempt = 0;
                        continue;
                    }

                    // Delivery failed - reset and wait
                    log_warn("native", "delivery.failed", &format!(
                        "Delivery failed after {} attempts, resetting", inject_attempt
                    ));
                    delivery_state = State::Pending;
                    attempt = 0;
                }

                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    // Cleanup on exit - matches Python _cleanup_pty() + stop_instance()
    log_info("native", "delivery.cleanup", &format!("Cleaning up instance {}", current_name));

    // Ownership check: verify we still own this instance name.
    // If a new process launched with the same name, the process_binding now points
    // to the new process — skip destructive cleanup to avoid nuking the new instance.
    let owns_instance = if process_id.is_empty() {
        true // No process_id to check — assume ownership (legacy/adhoc)
    } else {
        match db.get_process_binding(&process_id) {
            Ok(Some(bound_name)) => bound_name == current_name,
            Ok(None) => false, // Binding deleted — new process took over
            Err(_) => false,   // DB error — be conservative, don't delete
        }
    };

    if owns_instance {
        // 1. Get snapshot before deletion (for life event)
        let snapshot = match db.get_instance_snapshot(&current_name) {
            Ok(snap) => snap,
            Err(e) => {
                log_error("native", "delivery.cleanup", &format!(
                    "DB error getting instance snapshot: {}", e
                ));
                None
            }
        };

        // 2. Set status to "inactive" with appropriate context (matches Python)
        // exit:closed = normal exit, exit:killed = SIGHUP/SIGTERM
        let was_killed = crate::pty::EXIT_WAS_KILLED.load(std::sync::atomic::Ordering::Acquire);
        let (exit_context, exit_reason) = if was_killed {
            ("exit:killed", "killed")
        } else {
            ("exit:closed", "closed")
        };
        let _ = db.set_status(&current_name, "inactive", exit_context);

        // 3. Delete notify endpoints
        let _ = db.delete_notify_endpoints(&current_name);

        // 4. Log life event BEFORE delete — if log fails, row stays (stale cleanup catches it).
        //    Previous order (delete first) lost snapshots when log_life_event hit DB lock.
        if let Err(e) = db.log_life_event(&current_name, "stopped", "pty", exit_reason, snapshot) {
            log_warn("native", "delivery.life_event_fail", &format!("Failed to log life event: {}", e));
        }

        // 5. Delete instance row
        let _ = db.delete_instance(&current_name);
    } else {
        log_info("native", "delivery.cleanup_skipped", &format!(
            "Skipping instance cleanup for {} — name reassigned to new process", current_name
        ));
    }

    // Always clean up our own process binding (keyed by our process_id, not name)
    if !process_id.is_empty() {
        let _ = db.delete_process_binding(&process_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create DeliveryState with given screen state
    fn make_state(screen: ScreenState, cooldown_ms: u64) -> DeliveryState {
        DeliveryState {
            screen: Arc::new(std::sync::RwLock::new(screen)),
            inject_port: 0,
            user_activity_cooldown_ms: cooldown_ms,
        }
    }

    /// Helper: screen state where everything is safe for injection
    fn safe_screen() -> ScreenState {
        ScreenState {
            ready: true,
            approval: false,
            output_stable_1s: true,
            prompt_empty: true,
            input_text: None,
            last_user_input: Instant::now() - Duration::from_secs(10),
            last_output: Instant::now() - Duration::from_secs(10),
            cols: 80,
        }
    }

    // ---- evaluate_gate tests ----

    #[test]
    fn gate_all_conditions_pass() {
        let config = ToolConfig::claude();
        let state = make_state(safe_screen(), 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(result.safe);
        assert_eq!(result.reason, "ok");
    }

    #[test]
    fn gate_blocks_when_not_idle() {
        let config = ToolConfig::claude();
        let state = make_state(safe_screen(), 500);
        let result = evaluate_gate(&config, &state, false);
        assert!(!result.safe);
        assert_eq!(result.reason, "not_idle");
    }

    #[test]
    fn gate_blocks_on_approval() {
        let config = ToolConfig::claude();
        let mut screen = safe_screen();
        screen.approval = true;
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(!result.safe);
        assert_eq!(result.reason, "approval");
    }

    #[test]
    fn gate_blocks_on_user_activity() {
        let config = ToolConfig::claude();
        let mut screen = safe_screen();
        screen.last_user_input = Instant::now(); // just typed
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(!result.safe);
        assert_eq!(result.reason, "user_active");
    }

    #[test]
    fn gate_blocks_when_not_ready_for_gemini() {
        let config = ToolConfig::gemini();
        let mut screen = safe_screen();
        screen.ready = false;
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(!result.safe);
        assert_eq!(result.reason, "not_ready");
    }

    #[test]
    fn gate_claude_skips_ready_check() {
        // Claude has require_ready_prompt=false
        let config = ToolConfig::claude();
        let mut screen = safe_screen();
        screen.ready = false;
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(result.safe);
    }

    #[test]
    fn gate_blocks_on_prompt_text_for_claude() {
        let config = ToolConfig::claude();
        let mut screen = safe_screen();
        screen.prompt_empty = false;
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(!result.safe);
        assert_eq!(result.reason, "prompt_has_text");
    }

    #[test]
    fn gate_gemini_skips_prompt_empty_check() {
        // Gemini has require_prompt_empty=false
        let config = ToolConfig::gemini();
        let mut screen = safe_screen();
        screen.prompt_empty = false;
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(result.safe);
    }

    #[test]
    fn gate_output_unstable_only_when_configured() {
        // All tools have require_output_stable_seconds=0, so this gate never fires
        let config = ToolConfig::claude();
        let mut screen = safe_screen();
        screen.output_stable_1s = false;
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(result.safe); // 0s stable requirement = always passes

        // But if we made a config that requires it:
        let mut strict = ToolConfig::claude();
        strict.require_output_stable_seconds = 1.0;
        let mut screen2 = safe_screen();
        screen2.output_stable_1s = false;
        let state2 = make_state(screen2, 500);
        let result2 = evaluate_gate(&strict, &state2, true);
        assert!(!result2.safe);
        assert_eq!(result2.reason, "output_unstable");
    }

    #[test]
    fn gate_fail_fast_order() {
        // When multiple gates fail, first one wins
        let config = ToolConfig::gemini();
        let mut screen = safe_screen();
        screen.approval = true;
        screen.ready = false;
        let state = make_state(screen, 500);
        // not idle + approval + not ready → not_idle wins
        let result = evaluate_gate(&config, &state, false);
        assert_eq!(result.reason, "not_idle");
    }

    // ---- TwoPhaseRetryPolicy tests ----

    #[test]
    fn retry_attempt_zero_is_instant() {
        let policy = TwoPhaseRetryPolicy::default_policy();
        assert_eq!(policy.delay(0, None), Duration::ZERO);
    }

    #[test]
    fn retry_warm_phase_exponential() {
        let policy = TwoPhaseRetryPolicy::default_policy();
        let d1 = policy.delay(1, None).as_secs_f64();
        let d2 = policy.delay(2, None).as_secs_f64();
        let d3 = policy.delay(3, None).as_secs_f64();
        let d4 = policy.delay(4, None).as_secs_f64();
        assert!((d1 - 0.25).abs() < 0.01);
        assert!((d2 - 0.50).abs() < 0.01);
        assert!((d3 - 1.00).abs() < 0.01);
        assert!((d4 - 2.00).abs() < 0.01); // capped at warm_maximum
    }

    #[test]
    fn retry_warm_caps_at_2s() {
        let policy = TwoPhaseRetryPolicy::default_policy();
        let d10 = policy.delay(10, None).as_secs_f64();
        assert!((d10 - 2.0).abs() < 0.01);
    }

    #[test]
    fn retry_cold_phase_caps_at_5s() {
        let policy = TwoPhaseRetryPolicy::default_policy();
        let pending_long = Some(Duration::from_secs(120));
        let d10 = policy.delay(10, pending_long).as_secs_f64();
        assert!((d10 - 5.0).abs() < 0.01);
    }

    #[test]
    fn retry_high_attempt_no_overflow() {
        let policy = TwoPhaseRetryPolicy::default_policy();
        // Should not panic with very high attempt values
        let d = policy.delay(1000, None);
        assert!(d.as_secs_f64() <= 2.0 + 0.01);
    }

    // ---- Lookup functions ----

    #[test]
    fn status_icon_known_values() {
        assert_eq!(status_icon("listening"), "◉");
        assert_eq!(status_icon("active"), "▶");
        assert_eq!(status_icon("blocked"), "■");
        assert_eq!(status_icon("stopped"), "⊘");
        assert_eq!(status_icon("whatever"), "○");
    }

    #[test]
    fn gate_block_detail_known_reasons() {
        assert_eq!(gate_block_detail("not_idle"), "waiting for idle status");
        assert_eq!(gate_block_detail("approval"), "waiting for user approval");
        assert_eq!(gate_block_detail("unknown"), "blocked");
    }

    // ---- ToolConfig ----

    #[test]
    fn tool_config_for_tool_defaults_to_claude() {
        let config = ToolConfig::for_tool("nonexistent");
        assert!(config.require_prompt_empty);
        assert!(!config.require_ready_prompt);
    }

    #[test]
    fn tool_configs_match_expected_differences() {
        let claude = ToolConfig::claude();
        let gemini = ToolConfig::gemini();
        let codex = ToolConfig::codex();

        // Claude: no ready_prompt, yes prompt_empty
        assert!(!claude.require_ready_prompt);
        assert!(claude.require_prompt_empty);

        // Gemini: yes ready_prompt, no prompt_empty
        assert!(gemini.require_ready_prompt);
        assert!(!gemini.require_prompt_empty);

        // Codex: same as Claude (ready pattern unreliable in narrow terminals)
        assert!(!codex.require_ready_prompt);
        assert!(codex.require_prompt_empty);

        // All require idle
        assert!(claude.require_idle);
        assert!(gemini.require_idle);
        assert!(codex.require_idle);
    }
}
