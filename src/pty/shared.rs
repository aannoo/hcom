//! Portable PTY orchestration shared by the Unix poll loop (`super`) and the
//! Windows ConPTY proxy (`super::win`).
//!
//! These were originally methods on the Unix `Proxy`. They were lifted to free
//! functions taking explicit parameters (every `self.X` became an argument) so
//! the Windows proxy can drive the exact same delivery-thread startup, approval
//! publishing, screen-state refresh, title escaping, and launch-failure
//! finalization. The bodies are byte-for-byte the Unix originals apart from the
//! `self.X` → parameter substitution; the Unix correctness rests on that.

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::Config;
use crate::db::HcomDb;
use crate::delivery::{
    APPROVAL_SCRAPE_CLEAR_MS, DeliveryState, ScreenState, ToolConfig, latch_scraped_approval,
    run_delivery_loop,
};
use crate::log::{log_error, log_info, log_warn};
use crate::notify::NotifyServer;
use crate::shared::{ST_BLOCKED, ST_LISTENING, status_icon};
use crate::tool::Tool;

use super::PtyTarget;
use super::screen::ScreenTracker;

/// User-activity cooldown applied uniformly across tools (0.5s). Dim detection
/// enables this for Claude.
pub(super) const USER_ACTIVITY_COOLDOWN_MS: u64 = 500;

/// Update shared delivery state from screen tracker.
///
/// `publish` is the caller's approval-status publisher (it owns the
/// `instance_name`/`current_status` plumbing); it is invoked on the approval
/// edge exactly as the Unix proxy invoked `self.publish_approval_status`.
pub(super) fn update_delivery_state(
    screen_state: &Arc<RwLock<ScreenState>>,
    screen: &ScreenTracker,
    target: &PtyTarget,
    launch_phase_active: &Arc<AtomicBool>,
    publish: &dyn Fn(bool),
) {
    let mut approval_changed = None;
    if let Ok(mut state) = screen_state.write() {
        state.ready = screen.is_ready();
        // Cursor and Codex can briefly erase their approval surfaces during
        // redraws (Codex does this on focus changes). Latch positive detection
        // until output settles so a partial frame cannot clear blocked status.
        let scrape_latched_tool = matches!(target.known_tool(), Some(Tool::Codex | Tool::Cursor));
        let scraped_approval = match target.known_tool() {
            Some(Tool::Codex) => screen.is_waiting_approval() || screen.is_codex_approval_visible(),
            Some(Tool::Cursor) => screen.is_cursor_approval_visible(),
            _ => false,
        };
        state.approval_scrape_latched = latch_scraped_approval(
            state.approval_scrape_latched,
            scraped_approval,
            screen.is_output_stable(APPROVAL_SCRAPE_CLEAR_MS),
        );
        let approval = (scrape_latched_tool && state.approval_scrape_latched)
            || (target.name() == "antigravity" && screen.is_antigravity_approval_visible());
        if approval != state.approval {
            approval_changed = Some(approval);
        }
        state.approval = approval;
        let input_text = screen.get_input_box_text(target.name());
        let new_prompt_empty = input_text.as_ref().is_some_and(|t| t.is_empty());
        // Stamp submit-edge cooldown when input transitions from a known
        // non-empty value to empty or briefly undetected. Guards against
        // the race where the delivery gate sees `prompt_empty + listening`
        // in the gap before the tool's UserPromptSubmit hook flips status
        // to active. Requiring a previously-known non-empty input avoids
        // stamping on the initial false->true edge at startup.
        if super::prompt_submit_observed(state.input_text.as_deref(), input_text.as_deref()) {
            state.last_prompt_submit = Some(Instant::now());
        }
        state.prompt_empty = new_prompt_empty;
        state.input_text = input_text;
        // visible_tail is only consumed by the launch-blocked heuristic;
        // skip the screen walk + allocation once launch phase is over.
        state.visible_tail = if launch_phase_active.load(Ordering::Acquire) {
            screen.visible_tail(5, 500)
        } else {
            None
        };
        state.last_output = screen.last_output_instant();
        state.cols = screen.cols();
    }

    if let Some(approval) = approval_changed {
        publish(approval);
    }
}

/// Clear a pending approval answered by an injected keystroke.
///
/// Only acts when approval is currently showing, so routine message
/// injection (approval already false) is a no-op and never falsely stamps
/// user-active state. Cursor's approval is authoritative-by-prompt — it
/// clears only when the prompt leaves the screen — so it is excluded here,
/// matching the interactive stdin handler.
///
/// Returns `true` when the approval was cleared; the caller is then responsible
/// for clearing the tracker's approval (`screen.clear_approval()` on Unix, an
/// atomic request consumed by the reader thread on Windows).
pub(super) fn clear_injected_approval_state(
    target: &PtyTarget,
    screen_state: &Arc<RwLock<ScreenState>>,
    publish: &dyn Fn(bool),
) -> bool {
    if target.name() == "cursor" {
        return false;
    }
    let approval_cleared = match screen_state.write() {
        Ok(mut state) if state.approval => {
            state.approval = false;
            true
        }
        _ => false,
    };
    if approval_cleared {
        publish(false);
        return true;
    }
    false
}

/// Record a genuine user keystroke against the shared delivery state.
///
/// Genuine keystrokes answering a title-detected approval clear it immediately.
/// Cursor's approval is screen-scraped and authoritative-by-prompt, so it clears
/// only when the prompt actually leaves the screen.
///
/// Returns `true` only when a standing approval was actually cleared — matching
/// `clear_injected_approval_state`. The Unix caller ignores this and clears its
/// tracker inline on every non-cursor keystroke; the Windows caller uses it to
/// gate the tracker-clear atomic it consumes, so a keystroke with no approval
/// showing does not wipe the OSC scrape buffer (`output_buffer`) and lose an
/// approval edge that arrives in the same window.
pub(super) fn note_user_keystroke(
    target: &PtyTarget,
    screen_state: &Arc<RwLock<ScreenState>>,
    publish: &dyn Fn(bool),
) -> bool {
    let cursor_scrape = target.name() == "cursor";
    let mut approval_cleared = false;
    if let Ok(mut state) = screen_state.write() {
        state.last_user_input = Instant::now();
        if !cursor_scrape {
            approval_cleared = state.approval;
            state.approval = false;
        }
    }
    if approval_cleared {
        publish(false);
    }
    approval_cleared
}

/// Publish PTY approval edges independently of the delivery queue.
///
/// Approval is agent state: `hcom list` must report it even when no message
/// is pending. Clearing is guarded by the PTY-owned context so lifecycle
/// hooks that already moved the agent to active are never overwritten.
pub(super) fn publish_approval_status(
    approval: bool,
    instance_name_cfg: Option<&str>,
    current_status: &Arc<RwLock<String>>,
) {
    let Ok(db) = HcomDb::open() else {
        log_warn(
            "native",
            "pty.approval_status_open_failed",
            "Failed to open database for PTY approval status",
        );
        return;
    };

    let config = Config::get();
    let instance_name = config
        .process_id
        .as_deref()
        .and_then(|process_id| db.get_process_binding(process_id).ok().flatten())
        .or_else(|| instance_name_cfg.map(str::to_string))
        .or(config.instance_name);
    let Some(instance_name) = instance_name.filter(|name| !name.is_empty()) else {
        return;
    };

    let current = match db.get_instance_full(&instance_name) {
        Ok(row) => row,
        Err(error) => {
            log_warn(
                "native",
                "pty.approval_status_failed",
                &format!(
                    "Failed to read status for approval={} on {}: {}",
                    approval, instance_name, error
                ),
            );
            return;
        }
    };
    let already_blocked = current
        .as_ref()
        .is_some_and(|row| row.status == ST_BLOCKED && row.status_context == "pty:approval");

    // Resolve the approval edge to publish: block on the rising edge, release
    // on the falling edge, and stay silent when the row already matches.
    let edge = if approval {
        (!already_blocked).then_some((ST_BLOCKED, "pty:approval"))
    } else {
        already_blocked.then_some((ST_LISTENING, "pty:approval_cleared"))
    };
    let Some((status, context)) = edge else {
        // No transition to publish. Still reflect a standing block in the
        // PTY-owned shared status so `hcom list` stays consistent.
        if already_blocked && let Ok(mut shared_status) = current_status.write() {
            *shared_status = ST_BLOCKED.to_string();
        }
        return;
    };

    // Write the instance row, then log a paired status event. The bare
    // `set_status` leaves `status_detail` (the gated-command preview) intact,
    // while the explicit event keeps the block/release visible to the events
    // table, `events sub`, and the TUI — mirroring how the sibling
    // launch_blocked path pairs a row write with its own emitted event.
    // Without the event, the row updates silently and event consumers never
    // see the approval gate (Codex's only PTY-driven block path).
    if let Err(error) = db.set_status(&instance_name, status, context) {
        log_warn(
            "native",
            "pty.approval_status_failed",
            &format!(
                "Failed to publish approval={} for {}: {}",
                approval, instance_name, error
            ),
        );
        return;
    }

    let position = current.as_ref().map(|row| row.last_event_id).unwrap_or(0);
    let detail = current
        .as_ref()
        .map(|row| row.status_detail.as_str())
        .unwrap_or("");
    let mut data = serde_json::json!({
        "status": status,
        "context": context,
        "position": position,
    });
    if !detail.is_empty() {
        data["detail"] = serde_json::json!(detail);
    }
    if let Err(error) = db.log_event("status", &instance_name, &data) {
        log_warn(
            "native",
            "pty.approval_status_event_failed",
            &format!(
                "Failed to emit approval status event ({}) for {}: {}",
                context, instance_name, error
            ),
        );
    }

    if let Ok(mut shared_status) = current_status.write() {
        *shared_status = status.to_string();
    }
}

/// Marker error: the delivery thread was spawned, but its init result did not
/// arrive within the timeout (or the channel disconnected).
///
/// The spawned thread is detached and still running — it may yet finish
/// `initialize_delivery_components` and enter `run_delivery_loop`. A caller that
/// retries `start_delivery_thread` on a plain init `Err` MUST NOT retry on this
/// one, or it would spawn a *second* delivery thread alongside the first (no
/// singleton guard in `run_delivery_loop`) and double-deliver. Callers detect it
/// with `err.downcast_ref::<DeliveryStartTimeout>()`.
#[derive(Debug)]
pub(super) struct DeliveryStartTimeout;

impl std::fmt::Display for DeliveryStartTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "delivery thread initialization timed out")
    }
}

impl std::error::Error for DeliveryStartTimeout {}

/// Start the delivery thread (and transcript watcher for Codex).
///
/// Returns `Ok(Some(handle))` when the delivery thread initialized successfully
/// (DB opened, notify server created). Returns `Ok(None)` when there is no
/// instance name (delivery disabled). Returns `Err` if initialization failed —
/// the caller maps that to a launch failure.
///
/// The `Err` is a [`DeliveryStartTimeout`] when the thread was spawned but its
/// init result timed out (the thread is detached and still live); any other
/// `Err` means init failed up front and no thread is running. Retrying callers
/// must distinguish the two — see [`DeliveryStartTimeout`].
#[allow(clippy::too_many_arguments)]
pub(super) fn start_delivery_thread(
    instance_name_cfg: Option<&str>,
    running: Arc<AtomicBool>,
    delivery_state: Arc<RwLock<ScreenState>>,
    launch_phase_active: Arc<AtomicBool>,
    inject_port: u16,
    target: PtyTarget,
    notify_port: Arc<AtomicU16>,
    current_name: Arc<RwLock<String>>,
    current_status: Arc<RwLock<String>>,
) -> Result<Option<JoinHandle<()>>> {
    let instance_name = match instance_name_cfg {
        Some(name) => name.to_string(),
        None => {
            // Try to get from environment (fallback for testing without explicit config)
            Config::get().instance_name.unwrap_or_default()
        }
    };

    if instance_name.is_empty() {
        // No instance name - skip delivery (hybrid mode or testing)
        crate::log::log_warn(
            "native",
            "delivery.skip.no_instance_name",
            "No instance name - delivery disabled. Set config.instance_name or HCOM_INSTANCE_NAME env var.",
        );
        return Ok(None);
    }

    // Create oneshot channel for init result
    let (init_tx, init_rx) = mpsc::channel();

    let user_activity_cooldown_ms = USER_ACTIVITY_COOLDOWN_MS;
    let notify_port_shared = notify_port;
    let shared_name = current_name;
    let shared_status = current_status;

    // For Codex: spawn transcript watcher thread
    if matches!(target.known_tool(), Some(Tool::Codex)) {
        let watcher_running = running.clone();
        let watcher_name = instance_name.clone();
        std::thread::spawn(move || {
            crate::hooks::codex_file_edits::run_transcript_watcher(
                watcher_running,
                watcher_name,
                Duration::from_secs(5),
            );
        });
    }

    let handle = std::thread::spawn(move || {
        log_info(
            "native",
            "delivery.start",
            &format!("Starting delivery thread for {}", instance_name),
        );

        // Initialize delivery components with dependency injection
        let (mut db, notify) = match super::initialize_delivery_components(
            &instance_name,
            HcomDb::open,
            NotifyServer::new,
        ) {
            Ok((db, notify)) => {
                log_info(
                    "native",
                    "delivery.init.success",
                    &format!("Initialized delivery for {}", instance_name),
                );
                // Store port for shutdown wakeup
                notify_port_shared.store(notify.port(), Ordering::Release);
                log_info(
                    "native",
                    "notify.registered",
                    &format!("Registered notify port {}", notify.port()),
                );
                // Register inject port for screen queries
                if let Err(e) = db.register_inject_port(&instance_name, inject_port) {
                    log_warn(
                        "native",
                        "inject.register_fail",
                        &format!("Failed to register inject port: {}", e),
                    );
                }

                // Signal successful initialization to parent
                let _ = init_tx.send(Ok(()));
                (db, notify)
            }
            Err(e) => {
                log_error(
                    "native",
                    "delivery.init.fail",
                    &format!("Failed to initialize delivery: {}", e),
                );
                let _ = init_tx.send(Err(e));
                return;
            }
        };

        // Create delivery state wrapper
        let state = DeliveryState {
            screen: delivery_state,
            launch_phase_active,
            inject_port,
            user_activity_cooldown_ms,
        };

        // Get tool config
        let config = ToolConfig::for_tool(target.delivery_tool());

        // Run delivery loop (pass shared state for main loop's OSC override)
        run_delivery_loop(
            running,
            &mut db,
            &notify,
            &state,
            &instance_name,
            &config,
            Some(shared_name),
            Some(shared_status),
        );

        log_info(
            "native",
            "delivery.stop",
            &format!("Delivery thread stopped for {}", instance_name),
        );
    });

    // Wait for initialization result (with timeout to avoid blocking forever)
    match init_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {
            log_info(
                "native",
                "delivery.init.success",
                "Delivery thread initialized successfully",
            );
            Ok(Some(handle))
        }
        Ok(Err(e)) => {
            log_error(
                "native",
                "delivery.init.fail",
                &format!("Delivery thread init failed: {}", e),
            );
            Err(e)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            log_error(
                "native",
                "delivery.init.timeout",
                "Delivery thread init timed out after 5s",
            );
            // The thread is detached and still running — flag this distinctly so
            // a retrying caller does not spawn a second delivery thread.
            Err(DeliveryStartTimeout.into())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            log_error(
                "native",
                "delivery.init.disconnect",
                "Delivery thread init channel disconnected",
            );
            // Disconnect means the spawned thread dropped its sender without
            // sending — it has returned/panicked, so retrying is unsafe in the
            // same way a timeout is (the thread may have partially registered).
            Err(DeliveryStartTimeout.into())
        }
    }
}

/// Finalize a launch failure once the child has exited before binding.
///
/// `tail` is the screen's visible tail (the Unix caller passes
/// `self.screen.visible_tail(8, 1000)`); kept as a parameter so the function
/// stays free of any tracker reference.
pub(super) fn finalize_launch_failure_after_exit(
    instance_name_cfg: Option<&str>,
    tail: Option<&str>,
    launch_phase_active: &Arc<AtomicBool>,
    elapsed: Duration,
    exit_code: i32,
) {
    let Some(instance_name) = instance_name_cfg else {
        return;
    };

    let Ok(db) = HcomDb::open() else {
        return;
    };
    let Ok(Some(instance)) = db.get_instance_full(instance_name) else {
        return;
    };

    if instance.session_id.is_some()
        || instance.status_context != "new"
        || (instance.status != crate::shared::ST_INACTIVE && instance.status != "pending")
    {
        return;
    }

    let elapsed_secs = elapsed.as_secs();
    let mut fallback =
        format!("exited {elapsed_secs}s after spawn before binding (exit code {exit_code})");
    if let Some(tail) = tail {
        fallback.push_str("\nPTY output:\n");
        fallback.push_str(tail);
    }
    let Some(detail) =
        crate::instance_lifecycle::finalize_launch_failure_detail(&db, &instance, Some(&fallback))
    else {
        return;
    };
    let _ = db.emit_launch_failed_event(
        instance_name,
        crate::shared::ST_INACTIVE,
        "launch_failed",
        "exited_before_bind",
        &detail,
    );
    launch_phase_active.store(false, Ordering::Release);

    if let Ok(process_id) = std::env::var("HCOM_PROCESS_ID")
        && !process_id.is_empty()
    {
        let _ = db.delete_process_binding(&process_id);
    }
}

/// Build the OSC 1/2 title-set escape for `name`/`status` under `tool_name`.
pub(super) fn build_title_escape(name: &str, status: &str, tool_name: &str) -> String {
    let icon = status_icon(status);
    let title = format!("{} {} [{}]", icon, name, tool_name);
    format!("\x1b]1;{}\x07\x1b]2;{}\x07", title, title)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_start_timeout_downcasts_through_anyhow() {
        // The Windows reader gates its no-retry decision on downcasting the
        // start_delivery_thread error to DeliveryStartTimeout. Guard that the
        // marker survives the anyhow round-trip and that an ordinary error does
        // not match it.
        let timeout: anyhow::Error = DeliveryStartTimeout.into();
        assert!(timeout.downcast_ref::<DeliveryStartTimeout>().is_some());

        let other = anyhow::anyhow!("Failed to open database");
        assert!(other.downcast_ref::<DeliveryStartTimeout>().is_none());
    }

    #[test]
    fn build_title_escape_formats_osc_1_and_2() {
        // listening icon is the green dot; assert exact OSC framing.
        let esc = build_title_escape("alpha", "listening", "claude");
        let icon = status_icon("listening");
        let title = format!("{} alpha [claude]", icon);
        assert_eq!(esc, format!("\x1b]1;{}\x07\x1b]2;{}\x07", title, title));
        assert!(esc.starts_with("\x1b]1;"));
        assert!(esc.contains("\x07\x1b]2;"));
        assert!(esc.ends_with('\x07'));
    }

    #[test]
    fn build_title_escape_uses_status_icon() {
        // Different statuses must change the embedded icon.
        let listening = build_title_escape("a", "listening", "claude");
        let blocked = build_title_escape("a", "blocked", "claude");
        assert_ne!(listening, blocked);
    }

    #[test]
    fn note_user_keystroke_cursor_is_noop_and_returns_false() {
        let target = PtyTarget::AdhocCommand("cursor".to_string());
        let state = Arc::new(RwLock::new(ScreenState {
            approval: true,
            ..ScreenState::default()
        }));
        let calls = std::cell::Cell::new(0);
        let publish = |_a: bool| calls.set(calls.get() + 1);
        // cursor name: must not clear approval, must not publish, returns false.
        let cleared = note_user_keystroke(&target, &state, &publish);
        assert!(!cleared);
        assert!(state.read().unwrap().approval, "cursor approval untouched");
        assert_eq!(calls.get(), 0, "cursor keystroke must not publish");
    }

    #[test]
    fn note_user_keystroke_clears_approval_for_non_cursor() {
        let target = PtyTarget::Known(Tool::Claude);
        let state = Arc::new(RwLock::new(ScreenState {
            approval: true,
            ..ScreenState::default()
        }));
        let calls = std::cell::Cell::new(0);
        let publish = |a: bool| {
            assert!(!a, "keystroke publishes the cleared (false) edge");
            calls.set(calls.get() + 1);
        };
        let cleared = note_user_keystroke(&target, &state, &publish);
        assert!(cleared, "a standing approval was cleared");
        assert!(!state.read().unwrap().approval, "approval cleared");
        assert_eq!(calls.get(), 1, "cleared edge published once");
    }

    #[test]
    fn note_user_keystroke_no_publish_when_not_blocked() {
        let target = PtyTarget::Known(Tool::Claude);
        let state = Arc::new(RwLock::new(ScreenState::default())); // approval=false
        let calls = std::cell::Cell::new(0);
        let publish = |_a: bool| calls.set(calls.get() + 1);
        // No approval was showing, so nothing is cleared and the Windows caller
        // must not request a tracker-clear (which would wipe the scrape buffer).
        let cleared = note_user_keystroke(&target, &state, &publish);
        assert!(
            !cleared,
            "no standing approval means no tracker clear requested"
        );
        assert_eq!(calls.get(), 0, "no edge to publish when already clear");
    }

    #[test]
    fn clear_injected_approval_state_cursor_returns_false() {
        let target = PtyTarget::AdhocCommand("cursor".to_string());
        let state = Arc::new(RwLock::new(ScreenState {
            approval: true,
            ..ScreenState::default()
        }));
        let publish = |_a: bool| panic!("cursor must not publish");
        assert!(!clear_injected_approval_state(&target, &state, &publish));
        assert!(state.read().unwrap().approval, "cursor approval untouched");
    }

    #[test]
    fn clear_injected_approval_state_clears_when_blocked() {
        let target = PtyTarget::Known(Tool::Claude);
        let state = Arc::new(RwLock::new(ScreenState {
            approval: true,
            ..ScreenState::default()
        }));
        let calls = std::cell::Cell::new(0);
        let publish = |a: bool| {
            assert!(!a);
            calls.set(calls.get() + 1);
        };
        assert!(clear_injected_approval_state(&target, &state, &publish));
        assert!(!state.read().unwrap().approval);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn clear_injected_approval_state_noop_when_not_blocked() {
        let target = PtyTarget::Known(Tool::Claude);
        let state = Arc::new(RwLock::new(ScreenState::default()));
        let publish = |_a: bool| panic!("must not publish when nothing to clear");
        assert!(!clear_injected_approval_state(&target, &state, &publish));
    }
}
