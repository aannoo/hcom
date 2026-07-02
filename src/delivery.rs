//! PTY message delivery loop — injects messages via TCP, verifies via cursor advance.

#[path = "delivery/antigravity.rs"]
mod antigravity;

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::HcomDb;
use crate::log::{log_error, log_info, log_warn};
use crate::notify::NotifyServer;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_INACTIVE, ST_LISTENING};

/// Safely truncate a string to at most `max_chars` characters.
/// Unlike byte slicing `&s[..n]`, this won't panic on multi-byte UTF-8.
pub(crate) fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Build full display name: "{tag}-{name}" if tag exists, else "{name}".
fn full_display_name(db: &HcomDb, name: &str) -> String {
    match db.get_instance_tag(name) {
        Some(tag) => format!("{}-{}", tag, name),
        None => name.to_string(),
    }
}

/// Check process binding and update current_name if it changed.
/// Returns true if the name changed.
pub(crate) fn refresh_binding(
    db: &HcomDb,
    process_id: &str,
    current_name: &mut String,
    shared_name: &Option<Arc<std::sync::RwLock<String>>>,
) {
    if process_id.is_empty() {
        return;
    }
    match db.get_process_binding(process_id) {
        Ok(Some(new_name)) if new_name != *current_name => {
            log_info(
                "native",
                "delivery.binding_refresh",
                &format!("Instance name changed: {} -> {}", current_name, new_name),
            );
            if let Err(e) = db.migrate_notify_endpoints(current_name, &new_name) {
                log_warn(
                    "native",
                    "delivery.migrate_endpoints_fail",
                    &format!("{}", e),
                );
            }
            if let Err(e) = db.update_tcp_mode(&new_name, true) {
                log_warn("native", "delivery.update_tcp_mode_fail", &format!("{}", e));
            }
            if let Some(shared) = shared_name
                && let Ok(mut s) = shared.write()
            {
                *s = full_display_name(db, &new_name);
            }
            *current_name = new_name;
        }
        Ok(_) => {}
        Err(e) => {
            log_error(
                "native",
                "delivery.binding_refresh",
                &format!("DB error checking process binding: {}", e),
            );
        }
    }
}

/// Refresh shared status from DB. Updates current_status if changed.
pub(crate) fn refresh_status(
    db: &HcomDb,
    current_name: &str,
    current_status: &mut String,
    shared_status: &Option<Arc<std::sync::RwLock<String>>>,
) {
    let new_status = match db.get_status(current_name) {
        Ok(Some((status, _))) => status,
        Ok(None) => "stopped".to_string(),
        Err(e) => {
            log_error(
                "native",
                "delivery.status_check",
                &format!("DB error getting status: {}", e),
            );
            // Fail closed: don't inject into a PTY whose state we can't verify.
            "stopped".to_string()
        }
    };
    if new_status != *current_status {
        if let Some(shared) = shared_status
            && let Ok(mut s) = shared.write()
        {
            *s = new_status.clone();
        }
        *current_status = new_status;
    }
}

/// Refresh shared display name (picks up tag changes at runtime).
pub(crate) fn refresh_display_name(
    db: &HcomDb,
    current_name: &str,
    shared_name: &Option<Arc<std::sync::RwLock<String>>>,
) {
    if let Some(shared) = shared_name {
        let new_display = full_display_name(db, current_name);
        if let Ok(mut s) = shared.write()
            && *s != new_display
        {
            *s = new_display;
        }
    }
}

/// Inputs for one delivery-loop title refresh.
///
/// Bundling these lets `refresh_title_state` stay one call inside an already
/// hot loop without exploding the function signature.
struct TitleRefresh<'a> {
    db: &'a HcomDb,
    process_id: &'a str,
    current_name: &'a mut String,
    current_status: &'a mut String,
    shared_name: &'a Option<Arc<std::sync::RwLock<String>>>,
    shared_status: &'a Option<Arc<std::sync::RwLock<String>>>,
    tool: &'a str,
    host_label: &'a mut host_label::HostLabel,
}

/// Refresh OSC title state and push a matching label to terminals that expose
/// a programmatic label API (currently only herdr).
fn refresh_title_state(args: TitleRefresh<'_>) {
    let TitleRefresh {
        db,
        process_id,
        current_name,
        current_status,
        shared_name,
        shared_status,
        tool,
        host_label,
    } = args;
    refresh_binding(db, process_id, current_name, shared_name);
    refresh_status(db, current_name, current_status, shared_status);
    refresh_display_name(db, current_name, shared_name);
    host_label.sync(db, current_name, current_status, tool);
}

/// Mirror the OSC 1/2 title into the terminal's own label API for terminals
/// whose chrome doesn't render OSC titles. Currently only herdr; add a
/// `Backend` variant and a `resolve` arm to support another.
mod host_label {
    use std::time::Duration;

    use crate::db::HcomDb;
    use crate::identity;
    use crate::shared::format_pane_title;

    /// Long enough to absorb a slow herdr server tick, short enough that a
    /// dead socket doesn't visibly stall the delivery loop.
    const SOCKET_TIMEOUT: Duration = Duration::from_millis(200);

    /// Per-loop state: which backend (if any) we resolved at startup, and the
    /// last label we successfully pushed (for dedupe). On the first I/O error
    /// we drop the backend so subsequent iterations are no-ops — avoids log
    /// spam and per-tick socket churn when herdr exits mid-session.
    pub(super) struct HostLabel {
        backend: Option<Backend>,
        last_pushed: Option<String>,
    }

    enum Backend {
        Herdr {
            socket_path: String,
            pane_id: String,
        },
    }

    impl HostLabel {
        pub(super) fn resolve() -> Self {
            // `last_pushed` starts unset so the first delivery-loop iteration
            // *always* pushes a styled label. The built-in herdr preset
            // invokes `agent start {instance_name}` which leaves the pane
            // labeled with the bare instance name; the styled
            // `◉ luna [claude]` label only appears once we push it. Seeding
            // from HCOM_PANE_TITLE (which a custom template might or might
            // not have applied) would silently skip that first push and leave
            // the pane stuck on the bare name until a later status change.
            Self {
                backend: Backend::resolve(),
                last_pushed: None,
            }
        }

        pub(super) fn sync(&mut self, db: &HcomDb, name: &str, status: &str, tool: &str) {
            if self.backend.is_none() {
                return;
            }
            let label = pane_title_label(db, name, status, tool);
            if label.is_empty() || self.last_pushed.as_deref() == Some(label.as_str()) {
                return;
            }
            // Take the backend so we can drop it on failure without holding a
            // borrow across the I/O call.
            let backend = self.backend.take().expect("backend present");
            match backend.push(&label) {
                Ok(()) => {
                    self.backend = Some(backend);
                    self.last_pushed = Some(label);
                }
                Err(err) => {
                    crate::log::log_info(
                        "host_label",
                        "push_failed_disabling",
                        &format!("{}: {err}", backend.kind()),
                    );
                }
            }
        }
    }

    impl Backend {
        fn resolve() -> Option<Self> {
            if std::env::var("HERDR_ENV").ok().as_deref() == Some("1") {
                let socket_path = std::env::var("HERDR_SOCKET_PATH")
                    .ok()
                    .filter(|s| !s.is_empty())?;
                let pane_id = std::env::var("HERDR_PANE_ID")
                    .ok()
                    .filter(|s| !s.is_empty())?;
                return Some(Backend::Herdr {
                    socket_path,
                    pane_id,
                });
            }
            None
        }

        fn kind(&self) -> &'static str {
            match self {
                Backend::Herdr { .. } => "herdr",
            }
        }

        /// Push a visual label. Uses `pane.rename` (manual_label only) rather
        /// than `agent.rename` (which would also overwrite the herdr-canonical
        /// agent name with the status-icon-prefixed string and break
        /// `herdr agent send <name>` targeting).
        fn push(&self, label: &str) -> Result<(), String> {
            match self {
                Backend::Herdr {
                    socket_path,
                    pane_id,
                } => {
                    let request = serde_json::json!({
                        "id": "hcom:pane:rename",
                        "method": "pane.rename",
                        "params": { "pane_id": pane_id, "label": label },
                    });
                    send_unix_request(socket_path, &request)
                }
            }
        }
    }

    /// Build the same label hcom writes into OSC 1/2 (`◉ tag-luna [claude]`).
    fn pane_title_label(db: &HcomDb, name: &str, status: &str, tool: &str) -> String {
        let display = identity::get_display_name(db, name);
        format_pane_title(status, &display, tool)
    }

    #[cfg(unix)]
    fn send_unix_request(socket_path: &str, request: &serde_json::Value) -> Result<(), String> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;

        let mut stream =
            UnixStream::connect(socket_path).map_err(|e| format!("connect: {socket_path}: {e}"))?;
        let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
        writeln!(stream, "{request}").map_err(|e| format!("write: {e}"))?;
        let mut response = String::new();
        BufReader::new(&stream)
            .read_line(&mut response)
            .map_err(|e| format!("read: {e}"))?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn send_unix_request(_socket_path: &str, _request: &serde_json::Value) -> Result<(), String> {
        Err("unix sockets unavailable on this platform".into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::shared::ST_LISTENING;
        use serial_test::serial;

        #[test]
        fn pane_title_label_skips_when_tool_empty() {
            let dir = tempfile::tempdir().unwrap();
            // SAFETY: test-local HCOM_DIR.
            unsafe { std::env::set_var("HCOM_DIR", dir.path()) };
            let db = crate::db::HcomDb::open().unwrap();

            assert_eq!(pane_title_label(&db, "luna", ST_LISTENING, ""), "");
        }

        #[test]
        #[serial]
        fn resolve_does_not_seed_last_pushed_from_pane_title_env() {
            // The built-in herdr preset launches with `agent start
            // {instance_name}`, so herdr's initial pane label is the bare
            // name (e.g. `luna`). Seeding `last_pushed` from HCOM_PANE_TITLE
            // would silently swallow the first push and leave the pane
            // stuck on `luna` until the next status transition.
            // SAFETY: test is #[serial].
            unsafe {
                std::env::set_var("HCOM_PANE_TITLE", "\u{25c9} luna [claude]");
            }
            let label = HostLabel::resolve();
            // SAFETY: clear before assert so a panic doesn't leak env.
            unsafe {
                std::env::remove_var("HCOM_PANE_TITLE");
            }
            assert!(
                label.last_pushed.is_none(),
                "last_pushed must start unset so the first delivery-loop \
                 iteration always pushes a styled label"
            );
        }
    }
}

/// Human-readable descriptions for gate block reasons.
pub(crate) fn gate_block_detail(reason: &str) -> &'static str {
    match reason {
        "not_idle" => "waiting for idle status",
        "user_active" => "user is typing",
        "submit_settle" => "waiting for prompt submit to settle",
        "not_ready" => "prompt not visible",
        "output_unstable" => "output still streaming",
        "prompt_has_text" => "uncommitted text in prompt",
        "approval" => "waiting for user approval",
        _ => "blocked",
    }
}

/// Build PTY wake text for tools whose delivery path is not human-visible.
///
/// Claude and Codex inject the plain `<hcom>` trigger because their hooks already
/// print the full message in the TUI. Gemini, Antigravity, and OpenCode bootstrap
/// need a human-visible prompt line, but it must stay prompt-safe: metadata only,
/// no message body, no `@` autocomplete triggers, and no wrapping. If the compact
/// preview will not fit the current input width, use the same minimal trigger.
pub(crate) fn build_wake_inject_text(db: &HcomDb, recipient: &str, max_len: usize) -> String {
    let messages = db.get_unread_messages(recipient);
    if messages.is_empty() {
        return "<hcom>".to_string();
    }

    let recipient_display = sanitize_wake_preview_part(&full_display_name(db, recipient));
    let first_line = format_wake_message_line(db, &messages[0], &recipient_display);
    let inner = if messages.len() == 1 {
        first_line
    } else {
        format!("[{} new messages] | {}", messages.len(), first_line)
    };
    let preview = format!("<hcom>{inner}</hcom>");

    if preview.chars().count() > max_len || preview.contains('@') {
        "<hcom>".to_string()
    } else {
        preview
    }
}

fn sanitize_wake_preview_part(text: &str) -> String {
    let without_tags = strip_hcom_wrapper_tags(text);
    without_tags
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('@', "")
}

fn wake_message_prefix(msg: &crate::db::Message) -> String {
    let prefix = match (&msg.intent, &msg.thread) {
        (Some(i), Some(t)) => format!("{}:{}", i, sanitize_wake_preview_part(t)),
        (Some(i), None) => sanitize_wake_preview_part(i),
        (None, Some(t)) => format!("thread:{}", sanitize_wake_preview_part(t)),
        (None, None) => "new message".to_string(),
    };
    let id_ref = msg
        .event_id
        .map(|id| format!(" #{}", id))
        .unwrap_or_default();
    format!("[{}{}]", prefix, id_ref)
}

/// Strip tag-like sequences that could break the PTY `<hcom>...</hcom>` wrapper.
fn strip_hcom_wrapper_tags(text: &str) -> String {
    let mut s = text.to_string();
    for tag in ["</hcom>", "<hcom>"] {
        loop {
            let lower = s.to_lowercase();
            if let Some(i) = lower.find(tag) {
                s.replace_range(i..i + tag.len(), "");
            } else {
                break;
            }
        }
    }
    s
}

fn format_wake_message_line(
    db: &HcomDb,
    msg: &crate::db::Message,
    recipient_display: &str,
) -> String {
    let envelope = wake_message_prefix(msg);
    let sender_display = sanitize_wake_preview_part(&full_display_name(db, &msg.from));
    format!("{envelope} {sender_display} -> {recipient_display}")
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
/// 4. Submit-settle cooldown - Do not inject during the short screen/hook race after submit.
/// 5. `require_ready_prompt` - Ready pattern visible on screen (e.g., "? for shortcuts").
///    Pattern hidden when user has uncommitted text or is in a submenu (slash menu).
///    Note: Claude hides this in accept-edits mode, so Claude disables this check.
/// 6. `require_prompt_empty` - Check if prompt has no user text.
///    Claude-specific: Uses VT100 dim attribute detection to distinguish placeholder text
///    (dim) from user input (not dim). Implemented in screen.rs get_claude_input_text().
#[derive(Clone)]
pub struct ToolConfig {
    /// Tool name (claude, gemini, codex)
    pub tool: String,
    /// Require DB status == ST_LISTENING before inject
    pub require_idle: bool,
    /// Require ready pattern visible on screen
    pub require_ready_prompt: bool,
    /// Require prompt to be empty (no user text)
    pub require_prompt_empty: bool,
    /// Block if user is actively typing
    pub block_on_user_activity: bool,
    /// Block if approval prompt detected
    pub block_on_approval: bool,
    /// Whether the launch-readiness gate (separate from the delivery gate)
    /// requires the on-screen ready pattern. Decoupled from
    /// `require_ready_prompt` so tools can disable runtime delivery gates and
    /// still demand the ready pattern at launch time (opencode).
    pub launch_requires_ready: bool,
    /// Launch readiness is proven by the plugin's extension bind rather than the
    /// on-screen ready pattern. See [`GatesSpec::launch_ready_on_plugin_bind`].
    pub launch_ready_on_plugin_bind: bool,
}

impl ToolConfig {
    /// Build a `ToolConfig` from the per-tool [`IntegrationSpec.gates`].
    ///
    /// Gate booleans (and their rationale) live in `integration_spec.rs`.
    pub fn for_tool(tool: crate::tool::Tool) -> Self {
        let g = &tool.spec().gates;
        Self {
            tool: tool.as_str().to_string(),
            require_idle: g.require_idle,
            require_ready_prompt: g.require_ready_prompt,
            require_prompt_empty: g.require_prompt_empty,
            block_on_user_activity: g.block_on_user_activity,
            block_on_approval: g.block_on_approval,
            launch_requires_ready: g.launch_requires_ready,
            launch_ready_on_plugin_bind: g.launch_ready_on_plugin_bind,
        }
    }

    // Per-tool constructors retained as test helpers.
    #[cfg(test)]
    pub fn claude() -> Self {
        Self::for_tool(crate::tool::Tool::Claude)
    }
    #[cfg(test)]
    pub fn gemini() -> Self {
        Self::for_tool(crate::tool::Tool::Gemini)
    }
    #[cfg(test)]
    pub fn codex() -> Self {
        Self::for_tool(crate::tool::Tool::Codex)
    }
    #[cfg(test)]
    pub fn opencode() -> Self {
        Self::for_tool(crate::tool::Tool::OpenCode)
    }
    #[cfg(test)]
    pub fn antigravity() -> Self {
        Self::for_tool(crate::tool::Tool::Antigravity)
    }
    #[cfg(test)]
    pub fn cursor() -> Self {
        Self::for_tool(crate::tool::Tool::Cursor)
    }
    #[cfg(test)]
    pub fn copilot() -> Self {
        Self::for_tool(crate::tool::Tool::Copilot)
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
    /// True while the launch outcome is still Pending. Cleared once any
    /// terminal outcome (ready/failed/blocked) fires, so the PTY proxy can
    /// stop computing launch-only signals (e.g. `visible_tail`).
    pub launch_phase_active: Arc<AtomicBool>,
    pub inject_port: u16,
    pub user_activity_cooldown_ms: u64,
}

/// Terminal state of a single launch from the PTY delivery loop's perspective.
///
/// At most one terminal outcome (Ready/Failed/Blocked) is ever recorded per
/// loop. The Pending → terminal transition gates `maybe_emit_launch_blocked`
/// and the PTY-side `visible_tail` computation via `launch_phase_active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchOutcome {
    Pending,
    Ready,
    Failed,
    Blocked,
}

impl LaunchOutcome {
    fn is_pending(&self) -> bool {
        matches!(self, LaunchOutcome::Pending)
    }
}

/// Drive the launch-outcome state machine for one tick.
///
/// - Pending: emit Ready if screen is good, else maybe emit Blocked.
/// - Blocked: only emit Ready (recovery from launch_blocked, e.g. user
///   accepted agy's trust-folder prompt). Never re-block once cleared.
/// - Ready/Failed: terminal, no-op.
fn drive_launch_outcome(
    db: &HcomDb,
    state: &DeliveryState,
    current_name: &str,
    current_status: &str,
    config: &ToolConfig,
    launch_outcome: &mut LaunchOutcome,
) {
    match *launch_outcome {
        LaunchOutcome::Pending => {
            if launch_ready_observed(db, current_name, config, state) {
                emit_launch_ready_once(db, state, current_name, launch_outcome);
            } else {
                maybe_emit_launch_blocked(
                    db,
                    state,
                    current_name,
                    current_status,
                    config,
                    launch_outcome,
                );
            }
        }
        LaunchOutcome::Blocked => {
            if launch_ready_observed(db, current_name, config, state) {
                emit_launch_ready_once(db, state, current_name, launch_outcome);
            }
        }
        LaunchOutcome::Ready | LaunchOutcome::Failed => {}
    }
}

/// Screen state snapshot for gate checks
#[derive(Clone)]
pub struct ScreenState {
    pub ready: bool,
    pub approval: bool,
    pub prompt_empty: bool,
    pub input_text: Option<String>,
    pub visible_tail: Option<String>,
    pub last_user_input: Instant,
    /// Timestamp of last output (for stability-based recovery)
    pub last_output: Instant,
    /// Terminal width in columns
    pub cols: u16,
    /// Set when input_text transitions from non-empty to empty or temporarily
    /// undetected, i.e. a prompt was likely just submitted. The DB-side
    /// `status=active` update from the tool's UserPromptSubmit hook lags this
    /// screen-visible transition by a few hundred milliseconds, so the delivery
    /// gate must wait out that window or it will double-deliver: once via the
    /// hook (after the user's prompt runs) and once via PTY inject (during the
    /// race window where the gate sees
    /// `listening` + `prompt_empty`). See `SUBMIT_SETTLE_COOLDOWN_MS`.
    pub last_prompt_submit: Option<Instant>,
    /// Latched Cursor/Codex approval signal. Their TUI redraws can briefly erase
    /// both the dialog and title, which would flicker `approval` false while the
    /// prompt is still up. Latch true on any positive detection and only clear
    /// once output has settled. Antigravity keeps its immediate scrape.
    /// See `APPROVAL_SCRAPE_CLEAR_MS`.
    pub approval_scrape_latched: bool,
}

impl Default for ScreenState {
    fn default() -> Self {
        Self {
            ready: false,
            approval: false,
            prompt_empty: false,
            input_text: None,
            visible_tail: None,
            last_user_input: Instant::now(),
            last_output: Instant::now(),
            cols: 80,
            last_prompt_submit: None,
            approval_scrape_latched: false,
        }
    }
}

/// Window after an observed prompt-submit during which the delivery gate refuses
/// to inject. Covers the lag between the screen-visible input clear and the tool
/// hook's `status=active` update. Tuned from PTY test traces where the gap was
/// about 1s; round up for headroom.
pub(crate) const SUBMIT_SETTLE_COOLDOWN_MS: u64 = 1500;

/// How long screen output must be quiet before a negative approval scrape is
/// trusted to clear the latched signal. Redraw bursts (cursor's approval prompt
/// animating its selection / spinner) emit partial frames that scrape as "no
/// approval"; requiring a settled screen before clearing keeps the latch up
/// through the burst so the gate reports `approval`, not `prompt_has_text`.
pub(crate) const APPROVAL_SCRAPE_CLEAR_MS: u64 = 400;

/// Latch decision for screen-scraped approval (cursor). A partial-render frame
/// mid-redraw scrapes as "no approval"; holding the previous latch through such
/// transient false reads keeps the gate reporting `approval` instead of falling
/// through to `prompt_has_text`. The latch only clears once `output_settled`
/// (no redraw churn) confirms the prompt has genuinely left the screen.
pub(crate) fn latch_scraped_approval(prev: bool, scraped: bool, output_settled: bool) -> bool {
    if scraped {
        true
    } else if output_settled {
        false
    } else {
        prev
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
/// 3. block_on_user_activity - if user recently typed, reason="user_active"
/// 4. submit-settle cooldown - if prompt just submitted, reason="submit_settle"
/// 5. require_ready_prompt - if prompt not visible, reason="not_ready"
/// 6. require_prompt_empty - if prompt has user text, reason="prompt_has_text"
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
        return GateResult {
            safe: false,
            reason: "not_idle",
        };
    }
    // Approval check only runs if agent is idle (passed require_idle)
    if config.block_on_approval && screen.approval {
        return GateResult {
            safe: false,
            reason: "approval",
        };
    }
    if config.block_on_user_activity && state.is_user_active_with_guard(&screen) {
        return GateResult {
            safe: false,
            reason: "user_active",
        };
    }
    // Submit-edge cooldown: after the screen shows the input clearing, the
    // tool's hook hasn't yet flipped DB status to active. Without this,
    // `require_idle + prompt_empty` both look true and we double-inject. Only
    // applies to tools that gate on idleness; bootstrap-style paths (opencode)
    // run with `require_idle=false` and skip this entirely.
    if config.require_idle
        && let Some(submit_at) = screen.last_prompt_submit
        && submit_at.elapsed().as_millis() < SUBMIT_SETTLE_COOLDOWN_MS as u128
    {
        return GateResult {
            safe: false,
            reason: "submit_settle",
        };
    }
    if config.require_ready_prompt && !screen.ready {
        return GateResult {
            safe: false,
            reason: "not_ready",
        };
    }
    if config.require_prompt_empty && !screen.prompt_empty {
        return GateResult {
            safe: false,
            reason: "prompt_has_text",
        };
    }

    GateResult {
        safe: true,
        reason: "ok",
    }
}

fn launch_ready_observed(
    db: &HcomDb,
    name: &str,
    config: &ToolConfig,
    state: &DeliveryState,
) -> bool {
    let screen = state.screen.read().unwrap();
    if config.block_on_approval && screen.approval {
        return false;
    }
    if config.launch_ready_on_plugin_bind {
        // Authoritative readiness for plugin-driven tools (OMP): the extension's
        // bind (a kind='plugin' notify endpoint) proves both TUI construction
        // and extension load. It deliberately REPLACES on-screen scraping rather
        // than OR-ing with it — OMP's visible chrome is theme/preset dependent
        // (status-line presets omit the pi glyph), and a syntactically broken /
        // non-running extension could still render default chrome and be falsely
        // declared ready. Requiring the bind makes a dead extension block.
        return db.has_notify_endpoint_kind(name, "plugin");
    }
    if config.launch_requires_ready && !screen.ready {
        return false;
    }
    if config.require_prompt_empty && !screen.prompt_empty {
        return false;
    }
    true
}

/// Mark launch phase complete: clears the shared flag so the PTY proxy can
/// stop publishing launch-only signals.
fn mark_launch_phase_complete(
    state: &DeliveryState,
    outcome: &mut LaunchOutcome,
    next: LaunchOutcome,
) {
    *outcome = next;
    state.launch_phase_active.store(false, Ordering::Release);
}

fn emit_launch_ready_once(
    db: &HcomDb,
    state: &DeliveryState,
    current_name: &str,
    outcome: &mut LaunchOutcome,
) {
    // Allow Pending → Ready (first readiness) and Blocked → Ready (recovery,
    // e.g. user accepted agy's trust-folder prompt after launch_blocked fired).
    // Ready/Failed are terminal and re-fire is a no-op.
    let was_blocked = matches!(outcome, LaunchOutcome::Blocked);
    if !outcome.is_pending() && !was_blocked {
        return;
    }
    let context = if was_blocked {
        "launch_blocked_cleared"
    } else {
        "ready_observed"
    };
    if let Err(e) = db.set_status(current_name, ST_LISTENING, context) {
        log_warn(
            "native",
            "delivery.launch_ready_status_fail",
            &format!("Failed to mark launch ready for {}: {}", current_name, e),
        );
        return;
    }
    if let Err(e) = db.emit_ready_event(current_name, ST_LISTENING, context) {
        log_warn(
            "native",
            "delivery.launch_ready_event_fail",
            &format!("Failed to emit launch ready for {}: {}", current_name, e),
        );
        return;
    }
    mark_launch_phase_complete(state, outcome, LaunchOutcome::Ready);
}

fn emit_launch_failed_if_needed(
    db: &HcomDb,
    state: &DeliveryState,
    current_name: &str,
    outcome: &mut LaunchOutcome,
    reason: &str,
) {
    if !outcome.is_pending()
        || !state.launch_phase_active.load(Ordering::Acquire)
        || std::env::var("HCOM_LAUNCHED").as_deref() != Ok("1")
    {
        return;
    }
    let detail = "launch failed: readiness was never observed before the PTY delivery loop exited";
    if let Err(e) =
        db.emit_launch_failed_event(current_name, ST_INACTIVE, "launch_failed", reason, detail)
    {
        log_warn(
            "native",
            "delivery.launch_failed_event_fail",
            &format!("Failed to emit launch_failed for {}: {}", current_name, e),
        );
    }
    mark_launch_phase_complete(state, outcome, LaunchOutcome::Failed);
}

fn emit_launch_blocked_once(
    db: &HcomDb,
    state: &DeliveryState,
    current_name: &str,
    outcome: &mut LaunchOutcome,
    detail: &str,
) {
    if !outcome.is_pending() || std::env::var("HCOM_LAUNCHED").as_deref() != Ok("1") {
        return;
    }

    if let Err(e) = db.set_status(current_name, ST_BLOCKED, "launch_blocked") {
        log_warn(
            "native",
            "delivery.launch_blocked_status_fail",
            &format!(
                "Failed to set launch_blocked status for {}: {}",
                current_name, e
            ),
        );
        return;
    }

    if let Err(e) = db.emit_launch_blocked_event(
        current_name,
        ST_BLOCKED,
        "launch_blocked",
        "screen_settled_not_ready",
        detail,
    ) {
        log_warn(
            "native",
            "delivery.launch_blocked_event_fail",
            &format!("Failed to emit launch_blocked for {}: {}", current_name, e),
        );
    }
    mark_launch_phase_complete(state, outcome, LaunchOutcome::Blocked);
}

fn maybe_emit_launch_blocked(
    db: &HcomDb,
    state: &DeliveryState,
    current_name: &str,
    current_status: &str,
    config: &ToolConfig,
    outcome: &mut LaunchOutcome,
) {
    // Plugin-driven tools bind their extension slightly after the TUI settles;
    // give that bind a generous grace so a slow-but-valid launch is not
    // transient-blocked (drive_launch_outcome would recover it to Ready, but the
    // spurious blocked event is noisy). A genuinely dead extension still blocks
    // once the grace elapses with no kind='plugin' endpoint.
    const SETTLE_THRESHOLD: Duration = Duration::from_millis(1500);
    const PLUGIN_BIND_GRACE: Duration = Duration::from_secs(10);
    let settle_threshold = if config.launch_ready_on_plugin_bind {
        PLUGIN_BIND_GRACE
    } else {
        SETTLE_THRESHOLD
    };

    if !outcome.is_pending() || current_status == ST_ACTIVE {
        return;
    }

    let screen = state.screen.read().unwrap();
    let tail_text = screen.visible_tail.as_deref().unwrap_or("");
    // Gemini's animated startup banner keeps emitting output for ~60s, defeating
    // the settle heuristic. Its trust prompt is distinctive — fire immediately
    // when it appears rather than waiting for the banner animation to stop.
    let trust_prompt_visible = tail_text.contains("Do you trust the files in this folder?");
    if !trust_prompt_visible && screen.last_output.elapsed() < settle_threshold {
        return;
    }
    let Some(tail) = screen
        .visible_tail
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    else {
        return;
    };

    let detail = format!(
        "launch blocked: screen settled before readiness; run `hcom term {}`\n{}",
        current_name, tail
    );
    drop(screen);
    emit_launch_blocked_once(db, state, current_name, outcome, &detail);
}

/// Inject text to PTY via TCP (text only, no Enter).
/// Strips all C0 control chars (0x00-0x1F) except tab. This blocks ESC (0x1B),
/// so ANSI escape sequences cannot pass through.
pub(crate) fn inject_text(port: u16, text: &str) -> bool {
    let safe_text: String = text
        .chars()
        .filter(|c| *c >= ' ' || *c == '\t') // >= 0x20 or tab; blocks ESC, NULL, BEL, etc.
        .collect();

    if safe_text.is_empty() {
        return false;
    }

    match TcpStream::connect(format!("127.0.0.1:{}", port)) {
        Ok(mut stream) => stream.write_all(safe_text.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// Inject Enter key to PTY via TCP
pub(crate) fn inject_enter(port: u16) -> bool {
    match TcpStream::connect(format!("127.0.0.1:{}", port)) {
        Ok(mut stream) => stream.write_all(b"\r").is_ok(),
        Err(_) => false,
    }
}

/// Fixed retry delay between gate-blocked delivery attempts.
/// TCP notify handles the fast path (instant wake on status change);
/// this is the fallback polling interval for missed notifications.
/// Initial retry delay: 0.25s.
const RETRY_DELAY: Duration = Duration::from_millis(250);

/// Timeout for phase 1 (text render verification).
const PHASE1_TIMEOUT: Duration = Duration::from_secs(10);

/// Classify the prompt text relative to the text injected by this delivery attempt.
///
/// Only an exact match grants submit authority. A substring match is deliberately
/// classified as mixed because pressing Enter would also submit unrelated text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptOwnership {
    Exclusive,
    Mixed,
    Other,
}

fn prompt_ownership(input_text: Option<&str>, injected_text: &str) -> PromptOwnership {
    match input_text {
        Some(input) if !injected_text.is_empty() && input == injected_text => {
            PromptOwnership::Exclusive
        }
        Some(input) if !injected_text.is_empty() && input.contains(injected_text) => {
            PromptOwnership::Mixed
        }
        _ => PromptOwnership::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase1Decision {
    Rendered,
    MixedPrompt,
    Waiting,
    TimedOut,
}

/// Decide phase-1 state from one screen snapshot. Ownership checks intentionally
/// precede the deadline so a complete render observed at the boundary succeeds.
fn phase1_decision(
    input_text: Option<&str>,
    injected_text: &str,
    elapsed: Duration,
) -> Phase1Decision {
    match prompt_ownership(input_text, injected_text) {
        PromptOwnership::Exclusive => Phase1Decision::Rendered,
        PromptOwnership::Mixed => Phase1Decision::MixedPrompt,
        PromptOwnership::Other if elapsed > PHASE1_TIMEOUT => Phase1Decision::TimedOut,
        PromptOwnership::Other => Phase1Decision::Waiting,
    }
}

/// Timeout for phase 2 (text clear verification).
const PHASE2_TIMEOUT: Duration = Duration::from_secs(2);

/// Overall verification timeout for cursor advance.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait in idle state before checking again.
const IDLE_WAIT: Duration = Duration::from_secs(30);

/// Maximum number of Enter-key retries during phase 2 (text clear).
const MAX_ENTER_ATTEMPTS: u32 = 3;

/// Delivery state machine for the native PTY path (Claude/Gemini/Codex/Antigravity).
///
/// OpenCode bypasses this entirely — it early-returns with its own loop
/// inside `run_delivery_loop`.
/// - `Pending`: evaluates gate + idle checks, performs text injection
/// - `WaitTextRender`: confirms injected text appeared in the prompt, sends Enter on match
/// - `WaitTextClear`: verifies prompt cleared after Enter, retries Enter on timeout
/// - `VerifyCursor`: waits for hook-side cursor advance (falls back to has_pending==false)
///
/// Failed verification returns to `Pending`; success goes to `Idle` or `Pending` (if more queued).
#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Idle,
    Pending,
    WaitTextRender,
    WaitTextClear,
    VerifyCursor,
}

/// Run the delivery loop — surfaces out-of-band hcom messages into the tool's
/// conversation by injecting text at a safe prompt state.
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
    db: &mut HcomDb,
    notify: &NotifyServer,
    state: &DeliveryState,
    instance_name: &str,
    config: &ToolConfig,
    shared_name: Option<Arc<std::sync::RwLock<String>>>,
    shared_status: Option<Arc<std::sync::RwLock<String>>>,
) {
    // Resolve authoritative instance name from process binding.
    // The instance_name parameter is a fallback - the binding is the source of truth
    // because it can change (e.g., Claude session resume switches to canonical instance).
    let process_id = Config::get().process_id.unwrap_or_default();
    let mut current_name = if !process_id.is_empty() {
        match db.get_process_binding(&process_id) {
            Ok(Some(name)) => name,
            Ok(None) => instance_name.to_string(),
            Err(e) => {
                log_error(
                    "native",
                    "delivery.init",
                    &format!(
                        "DB error getting process binding: {} - using instance_name",
                        e
                    ),
                );
                instance_name.to_string()
            }
        }
    } else {
        instance_name.to_string()
    };

    log_info(
        "native",
        "delivery.init",
        &format!(
            "Delivery loop starting: name={}, process_id={}, tool={}, require_idle={}",
            current_name, process_id, config.tool, config.require_idle
        ),
    );

    let mut launch_outcome = LaunchOutcome::Pending;

    // Set initial listening status AFTER resolving authoritative name. This is
    // runtime state only; launch readiness is emitted explicitly below after
    // the delivery loop observes a usable screen state.
    if let Err(e) = db.set_status(&current_name, "listening", "start") {
        log_error(
            "native",
            "delivery.status.fail",
            &format!("Failed to set initial status: {}", e),
        );
    }

    // Set tcp_mode flag to indicate native PTY is handling delivery.
    // Also re-asserted on every heartbeat (self-heals after DB reset/instance recreation).
    if let Err(e) = db.update_tcp_mode(&current_name, true) {
        log_warn(
            "native",
            "delivery.tcp_mode_fail",
            &format!("Failed to set tcp_mode: {}", e),
        );
    } else {
        log_info(
            "native",
            "delivery.tcp_mode",
            &format!("Set tcp_mode=true for {}", current_name),
        );
    }

    // Set shared display name for PTY title (tag-name or just name)
    if let Some(ref shared) = shared_name
        && let Ok(mut s) = shared.write()
    {
        *s = full_display_name(db, &current_name);
    }

    // Resolve once: only delivery-loop iterations push labels, so a single
    // backend handle (or None) is captured at startup. First iteration will
    // push the initial label, subsequent iterations only push on change.
    let mut host_label = host_label::HostLabel::resolve();

    // OpenCode: plugin handles delivery after session exists. The delivery thread
    // only injects the FIRST message via PTY to bootstrap the session in the TUI.
    // After that, the plugin takes over (messages.transform for active, promptAsync for idle).
    use crate::tool::Tool;
    use std::str::FromStr;
    if matches!(
        Tool::from_str(&config.tool),
        Ok(Tool::OpenCode | Tool::Kilo | Tool::Pi | Tool::Omp)
    ) {
        log_info(
            "native",
            "delivery.opencode_mode",
            &format!(
                "OpenCode mode for {}: first-message PTY bootstrap, then plugin handles delivery",
                current_name
            ),
        );
        let mut first_message_injected = false;

        // Status tracking for terminal title updates
        let mut current_status = ST_LISTENING.to_string();

        while running.load(Ordering::Acquire) {
            refresh_title_state(TitleRefresh {
                db,
                process_id: &process_id,
                current_name: &mut current_name,
                current_status: &mut current_status,
                shared_name: &shared_name,
                shared_status: &shared_status,
                tool: &config.tool,
                host_label: &mut host_label,
            });
            drive_launch_outcome(
                db,
                state,
                &current_name,
                &current_status,
                config,
                &mut launch_outcome,
            );

            // Wait for notify or timeout
            notify.wait(IDLE_WAIT);
            if !running.load(Ordering::Acquire) {
                break;
            }

            // First-message bootstrap: inject via PTY to create session in TUI.
            // Only fires once — after this, the plugin handles all delivery.
            // Skip if plugin already has a session (e.g. user typed first, or session resumed).
            if !first_message_injected && db.has_session(&current_name) {
                first_message_injected = true;
                log_info(
                    "native",
                    "delivery.opencode_skip_inject",
                    &format!(
                        "{}: session already exists, plugin handles delivery",
                        current_name
                    ),
                );
            }
            if !first_message_injected && db.has_pending(&current_name) {
                let cols = state.screen.read().map(|s| s.cols).unwrap_or(80);
                let input_box_width = (cols as usize).saturating_sub(15).max(10);
                let text = build_wake_inject_text(db, &current_name, input_box_width);
                if inject_text(state.inject_port, &text) {
                    // OpenCode has no prompt-text parser here, so give the TUI
                    // enough time to render the injected bootstrap before Enter.
                    std::thread::sleep(Duration::from_millis(800));
                    if inject_enter(state.inject_port) {
                        first_message_injected = true;
                        log_info(
                            "native",
                            "delivery.bootstrap_inject",
                            &format!(
                                "Bootstrap inject for {}: '{}'",
                                current_name,
                                truncate_chars(&text, 40)
                            ),
                        );
                    }
                }
            }

            // Detect DB file replacement (hcom reset / schema bump) and reconnect
            db.reconnect_if_stale();

            // Heartbeat + port re-registration
            if let Err(e) = db.update_heartbeat(&current_name) {
                log_warn("native", "delivery.heartbeat_fail", &format!("{}", e));
            }
            if let Err(e) = db.register_notify_port(&current_name, notify.port()) {
                log_warn("native", "delivery.register_notify_fail", &format!("{}", e));
            }
            if let Err(e) = db.register_inject_port(&current_name, state.inject_port) {
                log_warn("native", "delivery.register_inject_fail", &format!("{}", e));
            }
        }
    } else {
        // Active delivery mode (existing state machine)

        // State machine
        let mut delivery_state = State::Pending; // Start pending to check immediately
        let mut attempt: u32 = 0;
        let mut inject_attempt: u32 = 0;
        let mut enter_attempt: u32 = 0;
        let mut injected_text = String::new();
        let mut phase_started_at = Instant::now();
        let mut cursor_before: i64 = 0;
        // Gate block tracking for TUI status updates
        let mut block_since: Option<Instant> = None;
        let mut last_block_context: String = String::new();

        // Status tracking for terminal title updates
        let mut current_status = ST_LISTENING.to_string();

        while running.load(Ordering::Acquire) {
            refresh_title_state(TitleRefresh {
                db,
                process_id: &process_id,
                current_name: &mut current_name,
                current_status: &mut current_status,
                shared_name: &shared_name,
                shared_status: &shared_status,
                tool: &config.tool,
                host_label: &mut host_label,
            });
            drive_launch_outcome(
                db,
                state,
                &current_name,
                &current_status,
                config,
                &mut launch_outcome,
            );

            match delivery_state {
                State::Idle => {
                    // Capture wall clock before wait to detect system sleep
                    let wall_before = crate::shared::time::now_epoch_i64() as u64;

                    // Recheck launch readiness promptly while the TUI is still
                    // painting its initial screen. Some tools can start the
                    // delivery loop just before their input prompt appears.
                    let idle_wait = if matches!(
                        launch_outcome,
                        LaunchOutcome::Pending | LaunchOutcome::Blocked
                    ) {
                        RETRY_DELAY
                    } else {
                        IDLE_WAIT
                    };
                    let notified = notify.wait(idle_wait);

                    if !running.load(Ordering::Acquire) {
                        log_info(
                            "native",
                            "delivery.shutdown",
                            "Running flag cleared, exiting loop",
                        );
                        break;
                    }

                    // Detect sleep/wake: wall clock jumped more than expected for IDLE_WAIT
                    let wall_after = crate::shared::time::now_epoch_i64() as u64;
                    let wall_elapsed = wall_after.saturating_sub(wall_before);
                    if wall_elapsed > 45 {
                        log_info(
                            "native",
                            "delivery.sleep_wake",
                            &format!(
                                "System sleep detected for {}: wall clock jumped {}s during 30s poll",
                                current_name, wall_elapsed
                            ),
                        );
                    }

                    // Detect DB file replacement (hcom reset / schema bump) and reconnect
                    db.reconnect_if_stale();

                    // Update heartbeat to prove we're alive (also re-asserts tcp_mode=true)
                    if let Err(e) = db.update_heartbeat(&current_name) {
                        log_warn(
                            "native",
                            "delivery.heartbeat_fail",
                            &format!("Failed to update heartbeat: {}", e),
                        );
                    }
                    // Re-register endpoints (self-heals after DB reset/instance recreation)
                    if let Err(e) = db.register_notify_port(&current_name, notify.port()) {
                        log_warn("native", "delivery.register_notify_fail", &format!("{}", e));
                    }
                    if let Err(e) = db.register_inject_port(&current_name, state.inject_port) {
                        log_warn("native", "delivery.register_inject_fail", &format!("{}", e));
                    }

                    // Check for pending messages
                    let has_pending = db.has_pending(&current_name);
                    if has_pending {
                        log_info(
                            "native",
                            "delivery.wake",
                            &format!(
                                "Woke up (notified={}) with pending messages for {}",
                                notified, current_name
                            ),
                        );
                        delivery_state = State::Pending;
                    } else if notified {
                        // Woke by notification but no pending messages — log for diagnostics
                        log_info(
                            "native",
                            "delivery.wake_no_pending",
                            &format!(
                                "Woke up (notified=true) but no pending messages for {}",
                                current_name
                            ),
                        );
                    }
                }

                State::Pending => {
                    // Check if still pending
                    if !db.has_pending(&current_name) {
                        log_info(
                            "native",
                            "delivery.no_pending",
                            &format!("No pending messages for {}", current_name),
                        );
                        delivery_state = State::Idle;
                        attempt = 0;
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
                        log_info(
                            "native",
                            "delivery.gate_pass",
                            &format!("Gate passed, injecting to port {}", state.inject_port),
                        );

                        // Snapshot cursor before injection
                        cursor_before = db.get_cursor(&current_name);

                        // Re-check pending immediately before inject
                        if !db.has_pending(&current_name) {
                            delivery_state = State::Idle;
                            attempt = 0;
                            inject_attempt = 0;
                            continue;
                        }

                        // Claude/Codex hooks show full delivery in the TUI, so
                        // they only need a trigger. Gemini-style paths use a
                        // compact, prompt-safe preview for human visibility.
                        use crate::tool::Tool;
                        use std::str::FromStr;

                        let parsed_tool = Tool::from_str(&config.tool).ok();
                        let cols = state.screen.read().map(|s| s.cols).unwrap_or(80);
                        let input_box_width = (cols as usize).saturating_sub(15).max(10);
                        let text = match parsed_tool {
                            Some(Tool::Claude) | Some(Tool::Codex) | Some(Tool::Cursor)
                            | Some(Tool::Kimi) | Some(Tool::Copilot) | Some(Tool::Pi)
                            | Some(Tool::Omp) => "<hcom>".to_string(),
                            _ => build_wake_inject_text(db, &current_name, input_box_width),
                        };

                        if inject_text(state.inject_port, &text) {
                            log_info(
                                "native",
                                "delivery.injected",
                                &format!(
                                    "Injected '{}' (len={}, inject_attempt={})",
                                    truncate_chars(&text, 40),
                                    text.len(),
                                    inject_attempt
                                ),
                            );
                            injected_text = text;
                            phase_started_at = Instant::now();
                            enter_attempt = 0;
                            delivery_state = State::WaitTextRender;
                            continue; // Skip retry delay - now in WaitTextRender phase
                        } else {
                            log_warn("native", "delivery.inject_fail", "TCP inject failed");
                            attempt += 1;
                        }
                    } else {
                        // Gate blocked - refresh heartbeat so we don't go stale while waiting
                        // (DB status is still "listening" until message is delivered and hooks fire)
                        if let Err(e) = db.update_heartbeat(&current_name) {
                            log_warn("native", "delivery.heartbeat_fail", &format!("{}", e));
                        }

                        // Log gate failure
                        if attempt == 0 || attempt.is_multiple_of(5) {
                            let screen = state.screen.read().unwrap();
                            log_info(
                                "native",
                                "delivery.gate_blocked",
                                &format!(
                                    "Gate blocked: {} (attempt={}, ready={}, approval={}, user_active={})",
                                    gate.reason,
                                    attempt,
                                    screen.ready,
                                    screen.approval,
                                    state.is_user_active()
                                ),
                            );
                        }

                        // Track when blocking started
                        if block_since.is_none() {
                            block_since = Some(Instant::now());
                        }

                        let approval_showing = {
                            let screen = state.screen.read().unwrap();
                            screen.approval
                        };
                        if !approval_showing && gate.reason == "not_idle" {
                            // Stability-based recovery: if status stuck "active" but output stable 10s,
                            // or stale PTY approval was left behind after the PTY cleared,
                            // flip back to listening.
                            // NOTE: stability tracking has false positives from escape sequences,
                            // but still useful for true idle detection when no data arrives at all.
                            match db.get_status(&current_name) {
                                Ok(Some((status, _))) if status == ST_ACTIVE => {
                                    let screen = state.screen.read().unwrap();
                                    let stable_10s =
                                        screen.last_output.elapsed().as_millis() > 10000;
                                    drop(screen);
                                    if stable_10s {
                                        if let Err(e) = db.set_status(
                                            &current_name,
                                            "listening",
                                            "pty:recovered",
                                        ) {
                                            log_warn(
                                                "native",
                                                "delivery.set_status_fail",
                                                &format!("Failed to set recovered status: {}", e),
                                            );
                                        }
                                        log_info(
                                            "native",
                                            "delivery.recovered",
                                            &format!(
                                                "Status recovered: output stable 10s, {} -> listening",
                                                status
                                            ),
                                        );
                                        attempt = 0;
                                        continue;
                                    }
                                }
                                Ok(Some(_)) | Ok(None) => {
                                    // Status not "active" or not found - skip recovery
                                }
                                Err(e) => {
                                    log_error(
                                        "native",
                                        "delivery.recovery_check",
                                        &format!("DB error checking status: {}", e),
                                    );
                                }
                            }
                            // Fall through to TUI status update
                            if let Some(since) = block_since
                                && since.elapsed().as_secs_f64() >= 2.0
                            {
                                match db.get_status(&current_name) {
                                    Ok(Some((status, _))) if status == ST_LISTENING => {
                                        let context = "tui:not-idle".to_string();
                                        if context != last_block_context {
                                            if let Err(e) = db.set_gate_status(
                                                &current_name,
                                                &context,
                                                "waiting for idle status",
                                            ) {
                                                log_warn(
                                                    "native",
                                                    "delivery.gate_status_fail",
                                                    &format!("{}", e),
                                                );
                                            }
                                            last_block_context = context;
                                        }
                                    }
                                    Ok(Some(_)) | Ok(None) => {
                                        // Status not "listening" or not found - skip
                                    }
                                    Err(e) => {
                                        log_error(
                                            "native",
                                            "delivery.tui_status_update",
                                            &format!("DB error checking status: {}", e),
                                        );
                                    }
                                }
                            }
                        } else if let Some(since) = block_since {
                            // After 2 seconds of blocking, update TUI status context
                            if since.elapsed().as_secs_f64() >= 2.0 {
                                // Only update if status is "listening" (don't overwrite active/blocked)
                                match db.get_status(&current_name) {
                                    Ok(Some((status, _))) if status == ST_LISTENING => {
                                        // Format context: tui:not-ready, tui:user-active, etc.
                                        let reason_formatted = gate.reason.replace("_", "-");
                                        let context = format!("tui:{}", reason_formatted);

                                        // Only update if context changed
                                        if context != last_block_context {
                                            let detail = gate_block_detail(gate.reason);
                                            let _ =
                                                db.set_gate_status(&current_name, &context, detail);
                                            last_block_context = context;
                                        }
                                    }
                                    Ok(Some(_)) | Ok(None) => {
                                        // Status not "listening" or not found - skip
                                    }
                                    Err(e) => {
                                        log_error(
                                            "native",
                                            "delivery.gate_status_update",
                                            &format!("DB error checking status: {}", e),
                                        );
                                    }
                                }
                            }
                        }

                        attempt += 1;
                    }

                    // Fixed 1s poll — TCP notify handles the fast path
                    if attempt > 0 {
                        let notified = notify.wait(RETRY_DELAY);
                        if notified {
                            attempt = 0;
                        }
                    }
                }

                State::WaitTextRender => {
                    let elapsed = phase_started_at.elapsed();

                    // Inspect the latest screen before applying the deadline. This
                    // avoids rejecting a render that completed at the timeout edge.
                    let screen = state.screen.read().unwrap();
                    let input_text = screen.input_text.clone();
                    let ready = screen.ready;
                    drop(screen);

                    // Debug: log what we see at start and every 500ms
                    if elapsed.as_millis() < 50 || elapsed.as_millis() % 500 < 50 {
                        log_info(
                            "native",
                            "delivery.phase1_poll",
                            &format!(
                                "t={}ms input={:?} want={} ready={}",
                                elapsed.as_millis(),
                                input_text.as_deref().unwrap_or("None"),
                                truncate_chars(&injected_text, 25),
                                ready
                            ),
                        );
                    }

                    match phase1_decision(input_text.as_deref(), &injected_text, elapsed) {
                        Phase1Decision::Rendered => {
                            log_info(
                                "native",
                                "delivery.text_rendered",
                                "Injected text exclusively owns the input box",
                            );

                            // Re-check all submit hazards from one fresh snapshot.
                            // The prompt can change between render detection and Enter.
                            let user_active = state.is_user_active();
                            let screen = state.screen.read().unwrap();
                            let approval = screen.approval;
                            let ownership =
                                prompt_ownership(screen.input_text.as_deref(), &injected_text);
                            drop(screen);

                            if ownership != PromptOwnership::Exclusive {
                                log_warn(
                                    "native",
                                    "delivery.prompt_ownership_lost",
                                    "Prompt changed before Enter; refusing automatic submission",
                                );
                                delivery_state = State::Pending;
                                inject_attempt += 1;
                                attempt += 1;
                                continue;
                            }

                            delivery_state = State::WaitTextClear;
                            phase_started_at = Instant::now();
                            enter_attempt = 0;

                            if !user_active && !approval {
                                log_info("native", "delivery.send_enter", "Sending Enter key");
                                inject_enter(state.inject_port);
                            } else if approval {
                                log_info(
                                    "native",
                                    "delivery.enter_blocked",
                                    "Enter blocked by approval prompt",
                                );
                            } else {
                                log_info(
                                    "native",
                                    "delivery.enter_blocked",
                                    "Enter blocked by user activity",
                                );
                            }
                            continue;
                        }
                        Phase1Decision::MixedPrompt => {
                            log_warn(
                                "native",
                                "delivery.mixed_prompt",
                                concat!(
                                    "Injected text is mixed with unrelated prompt text; ",
                                    "refusing automatic submission"
                                ),
                            );
                            delivery_state = State::Pending;
                            inject_attempt += 1;
                            attempt += 1;
                            continue;
                        }
                        Phase1Decision::TimedOut => {
                            log_warn(
                                "native",
                                "delivery.phase1_timeout",
                                &format!(
                                    "Text render timeout after {:?}, inject_attempt={}",
                                    elapsed, inject_attempt
                                ),
                            );
                            delivery_state = State::Pending;
                            inject_attempt += 1;
                            attempt += 1;
                            continue;
                        }
                        Phase1Decision::Waiting => {}
                    }

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
                        log_info(
                            "native",
                            "delivery.text_cleared",
                            "Input box cleared, verifying cursor",
                        );
                        delivery_state = State::VerifyCursor;
                        phase_started_at = Instant::now();
                        continue;
                    }

                    if elapsed > PHASE2_TIMEOUT {
                        if enter_attempt < MAX_ENTER_ATTEMPTS {
                            // Retry Enter with backoff
                            let user_active = state.is_user_active();
                            let screen = state.screen.read().unwrap();
                            let approval = screen.approval;
                            let ownership =
                                prompt_ownership(screen.input_text.as_deref(), &injected_text);
                            drop(screen);

                            if ownership != PromptOwnership::Exclusive {
                                log_warn(
                                    "native",
                                    "delivery.prompt_ownership_lost",
                                    concat!(
                                        "Prompt changed before Enter retry; ",
                                        "refusing automatic submission"
                                    ),
                                );
                                delivery_state = State::Pending;
                                inject_attempt += 1;
                                attempt += 1;
                                continue;
                            }

                            let can_send = !user_active && !approval;
                            if can_send {
                                log_info(
                                    "native",
                                    "delivery.retry_enter",
                                    &format!(
                                        "Retrying Enter (attempt={}, input_text={:?})",
                                        enter_attempt, input_text
                                    ),
                                );
                                inject_enter(state.inject_port);
                                enter_attempt += 1;
                                phase_started_at = Instant::now();
                                let backoff = Duration::from_millis(200 * (1 << enter_attempt));
                                std::thread::sleep(backoff);
                            } else {
                                log_info(
                                    "native",
                                    "delivery.enter_retry_blocked",
                                    &format!("Enter retry blocked (user_active={})", user_active),
                                );
                            }
                            continue;
                        }

                        // Max retries - go back to pending
                        log_warn(
                            "native",
                            "delivery.phase2_max_retries",
                            &format!(
                                "Max Enter retries ({}) reached, going back to pending",
                                MAX_ENTER_ATTEMPTS
                            ),
                        );
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
                            if let Err(e) = db.set_gate_status(&current_name, "", "") {
                                log_warn("native", "delivery.gate_clear_fail", &format!("{}", e));
                            }
                            last_block_context.clear();
                        }
                        block_since = None;

                        log_info(
                            "native",
                            "delivery.success",
                            &format!(
                                "Cursor advanced {} -> {}, delivery successful",
                                cursor_before, current_cursor
                            ),
                        );
                        if db.has_pending(&current_name) {
                            log_info(
                                "native",
                                "delivery.more_pending",
                                "More messages pending, continuing",
                            );
                            delivery_state = State::Pending;
                        } else {
                            log_info(
                                "native",
                                "delivery.complete",
                                "All messages delivered, going idle",
                            );
                            delivery_state = State::Idle;
                        }
                        attempt = 0;
                        inject_attempt = 0;
                        continue;
                    }

                    if elapsed > VERIFY_TIMEOUT {
                        inject_attempt += 1;
                        log_warn(
                            "native",
                            "delivery.verify_timeout",
                            &format!(
                                "Cursor verify timeout (before={}, current={}, inject_attempt={})",
                                cursor_before, current_cursor, inject_attempt
                            ),
                        );

                        if inject_attempt < 3 {
                            // Retry
                            log_info(
                                "native",
                                "delivery.retry",
                                &format!("Retrying delivery (inject_attempt={})", inject_attempt),
                            );
                            delivery_state = State::Pending;
                            attempt += 1;
                            continue;
                        }

                        // Cursor advance is the primary proof, but "no pending rows"
                        // is also sufficient — avoids wedging when hook delivery
                        // succeeded but cursor bookkeeping didn't advance.
                        if !db.has_pending(&current_name) {
                            // Success (cursor tracking issue but delivery worked)
                            // Clear gate block status
                            if !last_block_context.is_empty() {
                                if let Err(e) = db.set_gate_status(&current_name, "", "") {
                                    log_warn(
                                        "native",
                                        "delivery.gate_clear_fail",
                                        &format!("{}", e),
                                    );
                                }
                                last_block_context.clear();
                            }
                            block_since = None;

                            log_info(
                                "native",
                                "delivery.success_no_cursor",
                                "Messages gone despite cursor not advancing - delivery successful",
                            );
                            delivery_state = State::Idle;
                            attempt = 0;
                            inject_attempt = 0;
                            continue;
                        }

                        // Delivery failed - reset and wait
                        log_warn(
                            "native",
                            "delivery.failed",
                            &format!(
                                "Delivery failed after {} attempts, resetting",
                                inject_attempt
                            ),
                        );
                        delivery_state = State::Pending;
                        attempt = 0;
                    }

                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    } // end active delivery mode else block

    // Cleanup on exit — tear down PTY and stop instance
    log_info(
        "native",
        "delivery.cleanup",
        &format!("Cleaning up instance {}", current_name),
    );

    emit_launch_failed_if_needed(
        db,
        state,
        &current_name,
        &mut launch_outcome,
        "ready_never_observed",
    );

    let owns_instance = instance_owns_process_binding(db, &process_id, &current_name);

    if matches!(Tool::from_str(&config.tool), Ok(Tool::Antigravity)) {
        antigravity::cleanup_antigravity_pty_exit(db, &current_name, &process_id, owns_instance);
    } else {
        cleanup_pty_exit_default(db, &current_name, &process_id, owns_instance);
    }
}

/// True when this delivery thread's process_id still owns `current_name`.
fn instance_owns_process_binding(db: &HcomDb, process_id: &str, current_name: &str) -> bool {
    if process_id.is_empty() {
        return true;
    }
    match db.get_process_binding(process_id) {
        Ok(Some(bound_name)) => bound_name == current_name,
        Ok(None) => false,
        Err(_) => false,
    }
}

/// Hard PTY exit cleanup: inactive status, life event, delete instance row.
pub(crate) fn cleanup_deleted_instance(db: &mut HcomDb, current_name: &str) {
    let snapshot = match db.get_instance_snapshot(current_name) {
        Ok(Some(snap)) => Some(snap),
        Ok(None) => {
            log_info(
                "native",
                "delivery.cleanup_skipped",
                &format!(
                    "Skipping PTY stop event for {} because the instance row is already gone",
                    current_name
                ),
            );
            return;
        }
        Err(e) => {
            log_error(
                "native",
                "delivery.cleanup",
                &format!("DB error getting instance snapshot: {}", e),
            );
            None
        }
    };

    let was_killed = crate::pty::EXIT_WAS_KILLED.load(std::sync::atomic::Ordering::Acquire);
    let (exit_context, exit_reason) = if was_killed {
        ("exit:killed", "killed")
    } else {
        ("exit:closed", "closed")
    };
    if let Err(e) = db.set_status(current_name, "inactive", exit_context) {
        log_warn(
            "native",
            "delivery.set_status_fail",
            &format!("Failed to set inactive status: {}", e),
        );
    }

    if let Err(e) = db.delete_notify_endpoints(current_name) {
        log_warn(
            "native",
            "delivery.cleanup_endpoints_fail",
            &format!("{}", e),
        );
    }
    if let Err(e) = db.cleanup_subscriptions(current_name) {
        log_warn("native", "delivery.cleanup_subs_fail", &format!("{}", e));
    }
    if let Err(e) = db.log_life_event(current_name, "stopped", "pty", exit_reason, snapshot) {
        log_warn(
            "native",
            "delivery.life_event_fail",
            &format!("Failed to log life event: {}", e),
        );
    }
    if let Err(e) = db.delete_instance(current_name) {
        eprintln!("[hcom] warn: delete_instance failed for {current_name}: {e}");
    }
}

fn cleanup_pty_exit_default(
    db: &mut HcomDb,
    current_name: &str,
    process_id: &str,
    owns_instance: bool,
) {
    if owns_instance {
        cleanup_deleted_instance(db, current_name);
    } else {
        log_info(
            "native",
            "delivery.cleanup_skipped",
            &format!(
                "Skipping instance cleanup for {} — name reassigned to new process",
                current_name
            ),
        );
    }

    if !process_id.is_empty()
        && let Err(e) = db.delete_process_binding(process_id)
    {
        log_warn("native", "delivery.cleanup_binding_fail", &format!("{}", e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create DeliveryState with given screen state
    fn make_state(screen: ScreenState, cooldown_ms: u64) -> DeliveryState {
        DeliveryState {
            screen: Arc::new(std::sync::RwLock::new(screen)),
            launch_phase_active: Arc::new(AtomicBool::new(true)),
            inject_port: 0,
            user_activity_cooldown_ms: cooldown_ms,
        }
    }

    /// Helper: screen state where everything is safe for injection
    fn safe_screen() -> ScreenState {
        ScreenState {
            ready: true,
            approval: false,
            prompt_empty: true,
            input_text: None,
            visible_tail: None,
            last_user_input: Instant::now() - Duration::from_secs(10),
            last_output: Instant::now() - Duration::from_secs(10),
            cols: 80,
            last_prompt_submit: None,
            approval_scrape_latched: false,
        }
    }

    #[test]
    fn pty_cleanup_does_not_log_stop_after_instance_already_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let mut db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at)
                 VALUES ('buli', 'pi', 'active', 'running', 0, 0)",
                [],
            )
            .unwrap();

        let snapshot = db.get_instance_snapshot("buli").unwrap();
        db.log_life_event("buli", "stopped", "samu", "killed", snapshot)
            .unwrap();
        db.delete_instance("buli").unwrap();

        cleanup_deleted_instance(&mut db, "buli");

        let events: Vec<(String, String)> = db
            .conn()
            .prepare(
                "SELECT json_extract(data, '$.by'), json_extract(data, '$.reason')
                 FROM events
                 WHERE type = 'life'
                   AND instance = 'buli'
                   AND json_extract(data, '$.action') = 'stopped'
                 ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();

        assert_eq!(events, vec![("samu".to_string(), "killed".to_string())]);
    }

    // ---- phase-1 ownership tests ----

    #[test]
    fn phase1_timeout_is_ten_seconds() {
        assert_eq!(PHASE1_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn phase1_complete_render_wins_at_deadline() {
        assert_eq!(
            phase1_decision(
                Some("<hcom>"),
                "<hcom>",
                PHASE1_TIMEOUT + Duration::from_millis(1),
            ),
            Phase1Decision::Rendered,
        );
    }

    #[test]
    fn phase1_rejects_user_text_after_injected_text() {
        assert_eq!(
            phase1_decision(Some("<hcom> user draft"), "<hcom>", Duration::ZERO),
            Phase1Decision::MixedPrompt,
        );
    }

    #[test]
    fn phase1_rejects_user_text_before_injected_text() {
        assert_eq!(
            phase1_decision(Some("user draft <hcom>"), "<hcom>", Duration::ZERO),
            Phase1Decision::MixedPrompt,
        );
    }

    #[test]
    fn phase1_rejects_mixed_prompt_after_activity_cooldown() {
        assert_eq!(
            phase1_decision(
                Some("<hcom> user draft"),
                "<hcom>",
                Duration::from_millis(501),
            ),
            Phase1Decision::MixedPrompt,
        );
    }

    #[test]
    fn phase1_unrelated_text_times_out_normally() {
        assert_eq!(
            phase1_decision(
                Some("user draft"),
                "<hcom>",
                PHASE1_TIMEOUT + Duration::from_millis(1),
            ),
            Phase1Decision::TimedOut,
        );
    }

    #[test]
    fn submit_authority_requires_exact_prompt_ownership() {
        assert_eq!(
            prompt_ownership(Some("<hcom>"), "<hcom>"),
            PromptOwnership::Exclusive,
        );
        assert_eq!(
            prompt_ownership(Some("<hcom> user draft"), "<hcom>"),
            PromptOwnership::Mixed,
        );
        assert_eq!(
            prompt_ownership(Some("user draft"), "<hcom>"),
            PromptOwnership::Other,
        );
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
    fn antigravity_config_allows_ready_footer_with_placeholder_text() {
        let config = ToolConfig::antigravity();
        assert!(config.require_ready_prompt);
        assert!(config.require_prompt_empty);
        assert!(!config.block_on_user_activity);
    }

    #[test]
    fn gate_antigravity_blocks_prompt_text() {
        let config = ToolConfig::antigravity();
        let mut screen = safe_screen();
        screen.prompt_empty = false;
        screen.input_text = Some("uncommitted".to_string());
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(!result.safe);
        assert_eq!(result.reason, "prompt_has_text");
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
    fn gate_blocks_during_submit_settle_window() {
        let config = ToolConfig::codex();
        let mut screen = safe_screen();
        screen.last_prompt_submit = Some(Instant::now());
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(
            !result.safe,
            "gate must block during submit-settle window to prevent racing hook delivery"
        );
        assert_eq!(result.reason, "submit_settle");
    }

    #[test]
    fn gate_passes_after_submit_settle_expires() {
        let config = ToolConfig::codex();
        let mut screen = safe_screen();
        screen.last_prompt_submit =
            Some(Instant::now() - Duration::from_millis(SUBMIT_SETTLE_COOLDOWN_MS + 100));
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(result.safe);
        assert_eq!(result.reason, "ok");
    }

    #[test]
    fn gate_skips_submit_settle_when_idle_not_required() {
        // OpenCode bootstrap path runs with require_idle=false. The hook-vs-PTY
        // race that submit_settle guards against can't happen there, so the
        // cooldown shouldn't apply.
        let config = ToolConfig::opencode();
        let mut screen = safe_screen();
        screen.last_prompt_submit = Some(Instant::now());
        let state = make_state(screen, 500);
        let result = evaluate_gate(&config, &state, true);
        assert!(result.safe);
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

    fn open_ready_test_db() -> (tempfile::TempDir, HcomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        (dir, db)
    }

    #[test]
    fn launch_ready_observed_follows_tool_gate_shape() {
        let (_dir, db) = open_ready_test_db();
        let n = "toli";
        let mut screen = safe_screen();
        screen.ready = false;
        screen.prompt_empty = true;

        let state = make_state(screen.clone(), 500);
        assert!(launch_ready_observed(&db, n, &ToolConfig::codex(), &state));
        assert!(launch_ready_observed(&db, n, &ToolConfig::claude(), &state));
        assert!(!launch_ready_observed(
            &db,
            n,
            &ToolConfig::opencode(),
            &state
        ));
        assert!(!launch_ready_observed(
            &db,
            n,
            &ToolConfig::cursor(),
            &state
        ));

        let state = make_state(screen.clone(), 500);
        assert!(!launch_ready_observed(
            &db,
            n,
            &ToolConfig::gemini(),
            &state
        ));

        screen.ready = true;
        let state = make_state(screen.clone(), 500);
        assert!(launch_ready_observed(
            &db,
            n,
            &ToolConfig::opencode(),
            &state
        ));
        assert!(launch_ready_observed(&db, n, &ToolConfig::cursor(), &state));

        screen.prompt_empty = false;
        let state = make_state(screen, 500);
        assert!(!launch_ready_observed(&db, n, &ToolConfig::codex(), &state));
        assert!(!launch_ready_observed(
            &db,
            n,
            &ToolConfig::cursor(),
            &state
        ));
    }

    #[test]
    fn omp_launch_ready_requires_plugin_bind_not_screen() {
        // OMP readiness is bind-driven: a rendered/ready screen must NOT be
        // enough, and a kind='plugin' notify endpoint must flip it ready even
        // with no on-screen marker.
        let (_dir, db) = open_ready_test_db();
        let config = ToolConfig::for_tool(crate::tool::Tool::Omp);
        assert!(config.launch_ready_on_plugin_bind);

        let mut screen = safe_screen();
        screen.ready = true; // empty pattern => is_ready() always true
        screen.prompt_empty = true;
        let state = make_state(screen, 500);

        // No plugin endpoint yet -> not ready despite the "ready" screen.
        assert!(!launch_ready_observed(&db, "vupo", &config, &state));

        // A pty endpoint (registered at launch, before the extension binds) must
        // not count as readiness.
        db.upsert_notify_endpoint("vupo", "pty", 4001).unwrap();
        assert!(!launch_ready_observed(&db, "vupo", &config, &state));

        // The extension bind is the authoritative signal.
        db.upsert_notify_endpoint("vupo", "plugin", 4002).unwrap();
        assert!(launch_ready_observed(&db, "vupo", &config, &state));
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

    // ---- Screen-scraped approval latch ----

    #[test]
    fn latch_holds_through_transient_false_scrape() {
        // A positive scrape latches true regardless of prior state.
        assert!(latch_scraped_approval(false, true, false));
        assert!(latch_scraped_approval(false, true, true));
        // Latched true survives a transient false scrape while output is still
        // churning (a partial-render frame, not a real dismissal).
        assert!(latch_scraped_approval(true, false, false));
        // Once output settles and the scrape is still false, the prompt has
        // genuinely left the screen -> clear.
        assert!(!latch_scraped_approval(true, false, true));
        // Never spuriously latches from a clean idle state.
        assert!(!latch_scraped_approval(false, false, false));
        assert!(!latch_scraped_approval(false, false, true));
    }

    // ---- Lookup functions ----

    #[test]
    fn gate_block_detail_known_reasons() {
        assert_eq!(gate_block_detail("not_idle"), "waiting for idle status");
        assert_eq!(gate_block_detail("approval"), "waiting for user approval");
        assert_eq!(
            gate_block_detail("submit_settle"),
            "waiting for prompt submit to settle"
        );
        assert_eq!(gate_block_detail("unknown"), "blocked");
    }

    // ---- ToolConfig ----

    #[test]
    fn tool_config_for_adhoc_uses_adhoc_identity_and_gates() {
        let config = ToolConfig::for_tool(crate::tool::Tool::Adhoc);
        let gates = &crate::tool::Tool::Adhoc.spec().gates;
        assert_eq!(config.tool, "adhoc");
        assert_eq!(config.require_idle, gates.require_idle);
        assert_eq!(config.require_ready_prompt, gates.require_ready_prompt);
        assert_eq!(config.require_prompt_empty, gates.require_prompt_empty);
        assert_eq!(config.block_on_user_activity, gates.block_on_user_activity);
        assert_eq!(config.block_on_approval, gates.block_on_approval);
        assert_eq!(config.launch_requires_ready, gates.launch_requires_ready);
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

        // Copilot: footer-gated ready prompt + empty-prompt + approval gating.
        let copilot = ToolConfig::copilot();
        assert!(copilot.require_idle);
        assert!(copilot.require_ready_prompt);
        assert!(copilot.require_prompt_empty);
        assert!(copilot.block_on_user_activity);
        assert!(copilot.block_on_approval);
    }

    #[test]
    fn wake_inject_includes_prompt_safe_metadata_only() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hcom.db");
        let db = HcomDb::open_at(&db_path).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, status_context, created_at, last_event_id)
                 VALUES ('keno', 'listening', '', 1.0, 0)",
                [],
            )
            .unwrap();
        let data = serde_json::json!({
            "from": "life",
            "text": "ping. Always reply to @life, not @bigboss.",
            "scope": "mentions",
            "mentions": ["keno"],
            "intent": "request",
            "thread": "hcom-routing-test",
        });
        db.conn()
            .execute(
                "INSERT INTO events (type, timestamp, instance, data)
                 VALUES ('message', '2026-05-25T12:00:00Z', 'keno', ?1)",
                rusqlite::params![data.to_string()],
            )
            .unwrap();

        let text = build_wake_inject_text(&db, "keno", 120);
        assert!(text.starts_with("<hcom>"), "text={text}");
        assert!(text.ends_with("</hcom>"), "text={text}");
        assert!(text.contains("life"), "text={text}");
        assert!(text.contains("request"), "text={text}");
        assert!(!text.contains('@'));
        assert!(!text.contains("Always reply"));
    }

    #[test]
    fn wake_inject_falls_back_to_minimal_trigger_when_preview_would_wrap() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hcom.db");
        let db = HcomDb::open_at(&db_path).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, status_context, created_at, last_event_id)
                 VALUES ('keno', 'listening', '', 1.0, 0)",
                [],
            )
            .unwrap();
        let data = serde_json::json!({
            "from": "life",
            "text": "short",
            "scope": "mentions",
            "mentions": ["keno"],
            "intent": "request",
            "thread": "a-thread-name-that-is-too-wide-for-the-input",
        });
        db.conn()
            .execute(
                "INSERT INTO events (type, timestamp, instance, data)
                 VALUES ('message', '2026-05-25T12:00:00Z', 'keno', ?1)",
                rusqlite::params![data.to_string()],
            )
            .unwrap();

        assert_eq!(build_wake_inject_text(&db, "keno", 24), "<hcom>");
    }
}
