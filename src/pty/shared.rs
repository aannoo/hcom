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
use crate::shared::{ST_BLOCKED, ST_LISTENING};
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

/// Outcome of [`start_delivery_thread`].
///
/// `Result::Err` (distinct from every variant here) is an up-front init failure:
/// the spawned thread already returned, so the attempt is safely **retryable**.
pub(super) enum DeliveryStart {
    /// Init succeeded (DB opened, notify server created). Join the handle at
    /// shutdown.
    Started(JoinHandle<()>),
    /// No instance name (delivery disabled). No thread was spawned.
    Disabled,
    /// The thread was spawned but its init result was not observed within the
    /// timeout (or the init channel disconnected). It is detached and may still
    /// be initializing, so it MUST be joined at shutdown and MUST NOT be retried
    /// — a retry would spawn a *second* delivery thread alongside it (there is no
    /// singleton guard in `run_delivery_loop`) and double-deliver.
    Pending(JoinHandle<()>, mpsc::Receiver<Result<()>>),
}

/// Start the delivery thread (and transcript watcher for Codex).
///
/// Returns [`DeliveryStart::Started`] when the delivery thread initialized
/// successfully (DB opened, notify server created), [`DeliveryStart::Disabled`]
/// when there is no instance name, and [`DeliveryStart::Pending`] when the thread
/// was spawned but its init result timed out / the channel disconnected (the
/// thread is detached and still live; non-retryable). Returns `Err` only on an
/// up-front init failure, where no thread is left running — that case is
/// retryable and the caller maps it to a launch failure.
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
) -> Result<DeliveryStart> {
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
        return Ok(DeliveryStart::Disabled);
    }

    // Create oneshot channel for init result
    let (init_tx, init_rx) = mpsc::channel();

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
                notify_port.store(notify.port(), Ordering::Release);
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

                // For Codex: spawn the transcript watcher only after delivery
                // init has succeeded (#5). Spawning it before init meant a failed
                // or timed-out init still left an orphan watcher running against a
                // session that never came up. Init success is reached exactly once
                // per live delivery thread, so the watcher starts exactly once.
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
            user_activity_cooldown_ms: USER_ACTIVITY_COOLDOWN_MS,
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
            Some(current_name),
            Some(current_status),
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
            Ok(DeliveryStart::Started(handle))
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
            // Detached thread is still running; keep the handle so shutdown can
            // join it, and flag as non-retryable (Pending).
            Ok(DeliveryStart::Pending(handle, init_rx))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            log_error(
                "native",
                "delivery.init.disconnect",
                "Delivery thread init channel disconnected",
            );
            // Sender dropped without sending: thread returned/panicked, possibly
            // after partial registration. Non-retryable like a timeout; keep the
            // handle so shutdown can join it.
            Ok(DeliveryStart::Pending(handle, init_rx))
        }
    }
}

/// Decide whether the delivery coordinator should start delivery this tick.
///
/// Pure helper so the decision is host-testable (the coordinator itself needs a
/// live ConPTY). Start once the tool is ready, once the start-timeout has
/// elapsed, or when we are shutting down.
///
/// The shutdown trigger only fires after the child has already exited — `run()`
/// flips `running` false only once the reader has hit EOF and been joined — so
/// there is no live child left to inject into and the delivery loop's `while
/// running` body runs zero iterations (its `register_notify_port` never runs).
/// This final start therefore exists so init-time registration (DB open, notify
/// server, inject-port registration) and the loop's post-loop cleanup get a
/// chance to run and settle the final DB status, not to hand an in-flight `hcom
/// deliver` a live consumer.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn should_start_delivery(
    ready: bool,
    elapsed: Duration,
    timeout: Duration,
    shutting_down: bool,
) -> bool {
    ready || elapsed > timeout || shutting_down
}

/// Leading-edge throttle for the `hcom term` screen snapshot the reader renders.
///
/// Rendering `get_screen_dump` costs ~150µs on a wide screen, so refreshing it on
/// every ConPTY chunk would burn measurable CPU (and steal reader time from
/// draining the pipe) under heavy output. Cap it at one render per `throttle`;
/// any chunk skipped here is marked dirty and picked up by a single trailing-edge
/// refresh once output goes quiet (see the reader loop's debounce). Pure so the
/// decision is host-testable.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn should_refresh_snapshot(since_last_snapshot: Duration, throttle: Duration) -> bool {
    since_last_snapshot >= throttle
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
    let title = crate::shared::format_pane_title(status, name, tool_name);
    format!("\x1b]1;{}\x07\x1b]2;{}\x07", title, title)
}

/// Build minimal launch_context JSON from env vars available in the PTY process.
/// Captures process_id and late-bound terminal metadata needed by kill.
/// The start hook captures the full context (git_branch, tty, env snapshot) later.
///
/// Portable: every operation here (`env::var`, `fs::read_to_string`, `thread::sleep`)
/// works identically on Windows. `TMUX_PANE`/`ZELLIJ_PANE_ID`/`KITTY_WINDOW_ID` simply
/// won't be set there — kitty and the multiplexers have no native Windows build — so
/// those fields degrade to absent, same as on Unix outside of tmux/zellij/kitty.
/// `WEZTERM_PANE` is meaningful on Windows too, since WezTerm is natively cross-platform.
pub(super) fn build_early_launch_context() -> String {
    use serde_json::{Map, Value};

    let mut ctx = Map::new();

    if let Ok(pid) = std::env::var("HCOM_PROCESS_ID")
        && !pid.is_empty()
    {
        ctx.insert("process_id".into(), Value::String(pid));
    }

    // Kitty socket path for close-on-kill (needed when launching from outside kitty)
    if let Ok(listen) = std::env::var("KITTY_LISTEN_ON")
        && !listen.is_empty()
    {
        ctx.insert("kitty_listen_on".into(), Value::String(listen));
    }

    // Capture pane_id from terminal env vars for same-window launches.
    let pane_id_vars: &[&str] = &[
        "WEZTERM_PANE",
        "TMUX_PANE",
        "KITTY_WINDOW_ID",
        "ZELLIJ_PANE_ID",
    ];
    for &var in pane_id_vars {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            ctx.insert("pane_id".into(), Value::String(val));
            break;
        }
    }

    // Read terminal_id from temp file written by parent's launch stdout capture.
    // This is the ID returned by `kitten @ launch` (or similar) and serves as
    // fallback for pane_id when the terminal env var isn't available.
    //
    // Race condition: parent writes this file after `kitten @ launch` returns
    // (~500ms after child starts), but we run within ~10-100ms of spawn.
    // Retry with backoff only when pane_id not already captured from env vars
    // (tmux/wezterm set env vars directly, no file needed).
    if let Some(process_id) = ctx.get("process_id").and_then(|v| v.as_str()) {
        let id_file = crate::paths::hcom_dir()
            .join(".tmp")
            .join("terminal_ids")
            .join(process_id);
        let needs_id = !ctx.contains_key("pane_id");
        let max_attempts: usize = if needs_id { 10 } else { 1 };
        let mut terminal_id_value = String::new();

        for attempt in 0..max_attempts {
            if let Ok(contents) = std::fs::read_to_string(&id_file) {
                let trimmed = contents.trim().to_string();
                if !trimmed.is_empty() {
                    terminal_id_value = trimmed;
                    break;
                }
            }
            if attempt + 1 < max_attempts {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        if !terminal_id_value.is_empty() {
            ctx.insert(
                "terminal_id".into(),
                Value::String(terminal_id_value.clone()),
            );
            if !ctx.contains_key("pane_id") {
                ctx.insert("pane_id".into(), Value::String(terminal_id_value));
            }
        }
        // Don't delete the file here — capture_context in the SessionStart hook
        // reads it to persist terminal_id into DB launch_context. If we delete
        // early, the hook finds exists=false and terminal_id is lost from DB.
    }

    Value::Object(ctx).to_string()
}

/// True when `seq` is the terminal's cursor-position query (`ESC[6n`, a DSR
/// with parameter 6). In headless mode there is no outer terminal to answer it,
/// so the reader must reply on the child's behalf or startup hangs (#1).
///
/// Deliberately narrow: only the bare `ESC[6n` query matches. A CPR *reply*
/// (`ESC[<r>;<c>R`), a private DSR (`ESC[?6n`), and a parameterless `ESC[n`
/// must not match — we only synthesize a reply to the child's own query.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn csi_is_dsr_cpr(seq: &[u8]) -> bool {
    seq == b"\x1b[6n"
}

/// Rebuild a DEC private mode-set (`ESC[? … h|l`), dropping only the Win32-input
/// (`9001`) and focus-reporting (`1004`) parameters and keeping every other mode
/// (#15).
///
/// The previous whole-prefix match (`starts_with(b"\x1b[?9001")`) dropped or kept
/// the entire CSI, so `ESC[?9001;25h` lost mode 25 and `ESC[?25;9001h` leaked
/// 9001 to the outer terminal. Filtering per parameter fixes both. Returns an
/// empty `Vec` when every parameter was dropped (emit nothing).
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn filter_dec_private_modes(seq: &[u8]) -> Vec<u8> {
    let final_byte = *seq.last().unwrap();
    let params = &seq[3..seq.len() - 1];
    let kept: Vec<&[u8]> = params
        .split(|&b| b == b';')
        .filter(|p| *p != b"9001" && *p != b"1004")
        .collect();
    if kept.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(seq.len());
    out.extend_from_slice(b"\x1b[?");
    for (i, p) in kept.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        out.extend_from_slice(p);
    }
    out.push(final_byte);
    out
}

/// Rewrites the child's DEC private-mode **sets** so the *outer* terminal is
/// never switched into Win32 input mode (`?9001`) or focus reporting (`?1004`),
/// and notices the child's cursor-position query (`ESC[6n`) so a headless reader
/// can answer it.
///
/// A ConPTY wrapper sits between the child and the real terminal. If the child's
/// `ESC[?9001h` reaches the outer terminal, that terminal starts encoding its
/// input — including its automatic `ESC[6n` (cursor position) reply — as Win32
/// input records. The child, which only understands a plain `ESC[15;1R`, then
/// waits forever for a reply it can parse. Stripping those mode-sets keeps the
/// outer terminal in normal VT mode so the DSR reply round-trips correctly.
///
/// The parser is stateful so sequences split across reads are handled, and every
/// other byte (including all other escape sequences) passes through unchanged.
/// DSR queries are still passed through: in interactive mode the real terminal
/// must see them to answer; the headless reply is gated separately in win.rs.
///
/// Lives here (rather than in `win.rs`) so its correctness-critical parsing runs
/// under the host test gate; `win.rs` owns only the headless DSR-reply write.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Default)]
pub(super) struct OutputModeFilter {
    state: FilterState,
    buf: Vec<u8>,
    dsr_seen: bool,
    pending_utf8: u8,
}

#[derive(Default, PartialEq)]
enum FilterState {
    #[default]
    Ground,
    Esc,
    Csi,
    OscStart,
    OscDigit(u8),
    StringSeq {
        strip: bool,
        saw_esc: bool,
        discarded: usize,
    },
    SingleShift,
    Nf,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl OutputModeFilter {
    pub(super) fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let output_start = out.len();
        for &b in input {
            match self.state {
                FilterState::Ground => {
                    if b == 0x1b {
                        self.buf.clear();
                        self.buf.push(b);
                        self.state = FilterState::Esc;
                    } else {
                        out.push(b);
                    }
                }
                FilterState::Esc => {
                    self.buf.push(b);
                    match b {
                        b'[' => self.state = FilterState::Csi,
                        b']' => self.state = FilterState::OscStart,
                        b'P' | b'^' | b'_' | b'X' => {
                            out.extend_from_slice(&self.buf);
                            self.buf.clear();
                            self.state = FilterState::StringSeq {
                                strip: false,
                                saw_esc: false,
                                discarded: 0,
                            };
                        }
                        b'N' | b'O' => {
                            out.extend_from_slice(&self.buf);
                            self.buf.clear();
                            self.state = FilterState::SingleShift;
                        }
                        0x20..=0x2f => {
                            out.extend_from_slice(&self.buf);
                            self.buf.clear();
                            self.state = FilterState::Nf;
                        }
                        _ => {
                            // A complete two-byte escape: pass through untouched.
                            out.extend_from_slice(&self.buf);
                            self.buf.clear();
                            self.state = FilterState::Ground;
                        }
                    }
                }
                FilterState::Csi => {
                    self.buf.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        // Completed CSI. Notice the child's cursor-position query
                        // (still pass it through — interactive needs the real
                        // terminal to answer), and filter DEC private mode-sets
                        // per-parameter.
                        if csi_is_dsr_cpr(&self.buf) {
                            self.dsr_seen = true;
                        }
                        if self.buf.starts_with(b"\x1b[?") && matches!(b, b'h' | b'l') {
                            out.extend_from_slice(&filter_dec_private_modes(&self.buf));
                        } else {
                            out.extend_from_slice(&self.buf);
                        }
                        self.buf.clear();
                        self.state = FilterState::Ground;
                    } else if self.buf.len() > 32 {
                        // Malformed/overlong — give up filtering, emit as-is.
                        // Combined mode-sets are short, so this never trips on a
                        // legitimate `?9001`/`?1004` sequence.
                        out.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.state = FilterState::Ground;
                    }
                }
                FilterState::OscStart => {
                    self.buf.push(b);
                    if matches!(b, b'0' | b'1' | b'2') {
                        self.state = FilterState::OscDigit(b);
                    } else {
                        // Not a title OSC. Emit its prefix immediately and keep
                        // tracking until BEL/ST so title writes cannot split it.
                        out.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.state = FilterState::StringSeq {
                            strip: false,
                            saw_esc: b == 0x1b,
                            discarded: 0,
                        };
                        if b == 0x07 {
                            self.state = FilterState::Ground;
                        }
                    }
                }
                FilterState::OscDigit(_digit) => {
                    self.buf.push(b);
                    if b == b';' {
                        // Confirmed OSC 0/1/2: discard the complete title,
                        // including a terminator that may arrive in a later read.
                        self.buf.clear();
                        self.state = FilterState::StringSeq {
                            strip: true,
                            saw_esc: false,
                            discarded: 0,
                        };
                    } else {
                        // Multi-digit/non-title OSC. Preserve it while tracking
                        // its boundary for safe insertion of hcom's title.
                        out.extend_from_slice(&self.buf);
                        self.buf.clear();
                        self.state = FilterState::StringSeq {
                            strip: false,
                            saw_esc: b == 0x1b,
                            discarded: 0,
                        };
                        if b == 0x07 {
                            self.state = FilterState::Ground;
                        }
                    }
                }
                FilterState::StringSeq {
                    strip,
                    saw_esc,
                    discarded,
                } => {
                    if !strip {
                        out.push(b);
                    }
                    if b == 0x07 || (saw_esc && b == b'\\') {
                        self.state = FilterState::Ground;
                    } else if strip && discarded >= 256 {
                        // Match the Unix title filter's fail-open bound: a
                        // malformed title must not swallow unbounded real output.
                        self.state = FilterState::Ground;
                    } else {
                        self.state = FilterState::StringSeq {
                            strip,
                            saw_esc: b == 0x1b,
                            discarded: discarded + usize::from(strip),
                        };
                    }
                }
                FilterState::SingleShift => {
                    out.push(b);
                    self.state = FilterState::Ground;
                }
                FilterState::Nf => {
                    out.push(b);
                    if (0x30..=0x7e).contains(&b) {
                        self.state = FilterState::Ground;
                    }
                }
            }
        }
        // If this read contained only a stripped title OSC, retain the previous
        // UTF-8 state: no real output arrived to complete the pending character.
        if out.len() > output_start {
            self.pending_utf8 = advance_pending_utf8(self.pending_utf8, &out[output_start..]);
        }
    }

    /// One-shot: returns `true` once after a cursor-position query (`ESC[6n`)
    /// was seen, then resets. The reader uses it to reply on the child's behalf
    /// when running headless.
    pub(super) fn take_dsr(&mut self) -> bool {
        std::mem::take(&mut self.dsr_seen)
    }

    /// True when an hcom title OSC can be appended without splitting a control
    /// sequence or a multi-byte UTF-8 character.
    pub(super) fn title_write_safe(&self) -> bool {
        self.state == FilterState::Ground && self.pending_utf8 == 0
    }
}

/// Advance the number of expected UTF-8 continuation bytes across arbitrary
/// read boundaries. Invalid input resets the guard and is otherwise untouched.
fn advance_pending_utf8(mut pending: u8, data: &[u8]) -> u8 {
    for &byte in data {
        if pending > 0 && byte & 0xc0 == 0x80 {
            pending -= 1;
            continue;
        }
        pending = if byte & 0xf8 == 0xf0 {
            3
        } else if byte & 0xf0 == 0xe0 {
            2
        } else if byte & 0xe0 == 0xc0 {
            1
        } else {
            0
        };
    }
    pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::status_icon;

    #[test]
    fn should_start_delivery_on_ready() {
        // Ready alone starts delivery even well before the timeout.
        assert!(should_start_delivery(
            true,
            Duration::from_millis(1),
            Duration::from_secs(10),
            false,
        ));
    }

    #[test]
    fn should_start_delivery_on_timeout() {
        // #8 regression: not ready, but elapsed exceeded the timeout → start.
        assert!(should_start_delivery(
            false,
            Duration::from_secs(11),
            Duration::from_secs(10),
            false,
        ));
    }

    #[test]
    fn should_start_delivery_on_shutdown() {
        // Shutting down forces a final start (child already exited) so init-time
        // registration and post-loop cleanup run, even if never ready and still
        // inside the timeout.
        assert!(should_start_delivery(
            false,
            Duration::from_millis(1),
            Duration::from_secs(10),
            true,
        ));
    }

    #[test]
    fn should_not_start_delivery_before_ready_or_timeout() {
        // #8 regression: not ready and still inside the timeout window while
        // running → must NOT start yet (the old reader could start too early).
        assert!(!should_start_delivery(
            false,
            Duration::from_secs(1),
            Duration::from_secs(10),
            false,
        ));
    }

    #[test]
    fn should_refresh_snapshot_only_after_throttle_elapsed() {
        let throttle = Duration::from_millis(100);
        // Fresh chunk right after a refresh: defer (mark dirty), don't re-render.
        assert!(!should_refresh_snapshot(Duration::from_millis(0), throttle));
        assert!(!should_refresh_snapshot(
            Duration::from_millis(99),
            throttle
        ));
        // At/after the throttle window: render now (leading edge).
        assert!(should_refresh_snapshot(
            Duration::from_millis(100),
            throttle
        ));
        assert!(should_refresh_snapshot(
            Duration::from_millis(250),
            throttle
        ));
    }

    #[test]
    fn csi_is_dsr_cpr_matches_only_the_cursor_position_query() {
        assert!(csi_is_dsr_cpr(b"\x1b[6n"));
        // A CPR reply, a private DSR, and a bare DSR must not match.
        assert!(!csi_is_dsr_cpr(b"\x1b[6;1R"));
        assert!(!csi_is_dsr_cpr(b"\x1b[?6n"));
        assert!(!csi_is_dsr_cpr(b"\x1b[n"));
    }

    fn filter_modes(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = OutputModeFilter::default();
        let mut out = Vec::new();
        for c in chunks {
            f.filter(c, &mut out);
        }
        out
    }

    #[test]
    fn filter_dec_private_modes_drops_only_targeted_params() {
        // Whole-prefix regression (#15): a targeted param anywhere in the list
        // must be dropped without losing the others, in either order.
        assert_eq!(filter_dec_private_modes(b"\x1b[?9001;25h"), b"\x1b[?25h");
        assert_eq!(filter_dec_private_modes(b"\x1b[?25;9001h"), b"\x1b[?25h");
        assert_eq!(
            filter_dec_private_modes(b"\x1b[?1004;2004h"),
            b"\x1b[?2004h"
        );
        assert_eq!(filter_dec_private_modes(b"\x1b[?9001;1004h"), b"");
        assert_eq!(
            filter_dec_private_modes(b"\x1b[?9001;1004;25h"),
            b"\x1b[?25h"
        );
        assert_eq!(filter_dec_private_modes(b"\x1b[?25;9001l"), b"\x1b[?25l");
    }

    #[test]
    fn output_mode_filter_drops_win32_and_focus_mode_sets() {
        // ESC[?9001h ESC[?1004h "hi" ESC[6n — mode-sets dropped, DSR passes.
        let input = b"\x1b[?9001h\x1b[?1004h hi \x1b[6n";
        assert_eq!(filter_modes(&[input]), b" hi \x1b[6n");
    }

    #[test]
    fn output_mode_filter_passes_other_sequences_and_text() {
        let input = b"\x1b[31mred\x1b[0m\x1b[2J plain";
        assert_eq!(filter_modes(&[input]), input);
    }

    #[test]
    fn output_mode_filter_keeps_other_modes_in_a_combined_set() {
        // #15: mixed sets keep non-targeted modes and drop targeted ones,
        // regardless of parameter order.
        assert_eq!(filter_modes(&[b"\x1b[?9001;25h"]), b"\x1b[?25h");
        assert_eq!(filter_modes(&[b"\x1b[?25;9001h"]), b"\x1b[?25h");
        assert_eq!(filter_modes(&[b"\x1b[4h\x1b[?25h"]), b"\x1b[4h\x1b[?25h");
    }

    #[test]
    fn output_mode_filter_handles_sequence_split_across_reads() {
        // A combined set split mid-sequence still filters per-parameter.
        assert_eq!(filter_modes(&[b"\x1b[?25;90", b"01h"]), b"\x1b[?25h");
        // The pure Win32-input set split mid-sequence is still fully dropped.
        assert_eq!(filter_modes(&[b"\x1b[?90", b"01h", b"X"]), b"X");
    }

    #[test]
    fn output_mode_filter_drops_mode_reset_too() {
        assert_eq!(filter_modes(&[b"\x1b[?9001l\x1b[?1004lY"]), b"Y");
    }

    #[test]
    fn output_mode_filter_take_dsr_is_one_shot() {
        let mut f = OutputModeFilter::default();
        let mut out = Vec::new();
        f.filter(b"\x1b[6n", &mut out);
        // DSR passes through to the outer terminal...
        assert_eq!(out, b"\x1b[6n");
        // ...and is latched exactly once.
        assert!(f.take_dsr());
        assert!(!f.take_dsr());
    }

    #[test]
    fn output_mode_filter_strips_title_osc_split_across_reads() {
        assert_eq!(
            filter_modes(&[b"before\x1b]2;Clau", b"de Code\x07after"]),
            b"beforeafter"
        );
        assert_eq!(
            filter_modes(&[b"\x1b", b"]1", b";icon\x1b", b"\\text"]),
            b"text"
        );
    }

    #[test]
    fn output_mode_filter_preserves_non_title_osc_and_tracks_its_boundary() {
        let chunks: &[&[u8]] = &[b"\x1b]8;;https://exam", b"ple.test\x1b\\link"];
        assert_eq!(filter_modes(chunks), chunks.concat());

        let mut f = OutputModeFilter::default();
        let mut out = Vec::new();
        f.filter(chunks[0], &mut out);
        assert!(!f.title_write_safe());
        f.filter(chunks[1], &mut out);
        assert!(f.title_write_safe());
    }

    #[test]
    fn output_mode_filter_defers_title_across_split_utf8() {
        let mut f = OutputModeFilter::default();
        let mut out = Vec::new();
        f.filter(&[0xe2], &mut out);
        assert!(!f.title_write_safe());
        f.filter(&[0x94], &mut out);
        assert!(!f.title_write_safe());
        f.filter(&[0x80], &mut out);
        assert!(f.title_write_safe());
        assert_eq!(out, "─".as_bytes());
    }

    #[test]
    fn output_mode_filter_title_only_read_preserves_pending_utf8() {
        let mut f = OutputModeFilter::default();
        let mut out = Vec::new();
        f.filter(&[0xe2, 0x94], &mut out);
        assert!(!f.title_write_safe());
        f.filter(b"\x1b]2;Claude Code\x07", &mut out);
        assert!(!f.title_write_safe());
        f.filter(&[0x80], &mut out);
        assert!(f.title_write_safe());
    }

    #[test]
    fn output_mode_filter_tracks_other_split_escape_boundaries() {
        let mut f = OutputModeFilter::default();
        let mut out = Vec::new();

        f.filter(b"\x1bPpayload", &mut out);
        assert!(!f.title_write_safe());
        f.filter(b"\x1b\\", &mut out);
        assert!(f.title_write_safe());

        f.filter(b"\x1bN", &mut out);
        assert!(!f.title_write_safe());
        f.filter(b"x", &mut out);
        assert!(f.title_write_safe());

        f.filter(b"\x1b(", &mut out);
        assert!(!f.title_write_safe());
        f.filter(b"B", &mut out);
        assert!(f.title_write_safe());

        assert_eq!(out, b"\x1bPpayload\x1b\\\x1bNx\x1b(B");
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

    // build_early_launch_context is portable (env::var/fs::read_to_string/thread::sleep
    // all work identically on Windows), so these run on every platform rather than
    // being Unix-only. Each test clears the env vars it touches before asserting so a
    // panic can't leak state into later tests; #[serial] additionally prevents these
    // from interleaving with each other.
    use serial_test::serial;

    fn clear_launch_context_env() {
        // SAFETY: tests are #[serial].
        unsafe {
            std::env::remove_var("HCOM_PROCESS_ID");
            std::env::remove_var("KITTY_LISTEN_ON");
            std::env::remove_var("WEZTERM_PANE");
            std::env::remove_var("TMUX_PANE");
            std::env::remove_var("KITTY_WINDOW_ID");
            std::env::remove_var("ZELLIJ_PANE_ID");
        }
    }

    #[test]
    #[serial]
    fn build_early_launch_context_empty_when_no_env_vars_set() {
        clear_launch_context_env();
        let json = build_early_launch_context();
        clear_launch_context_env();
        assert_eq!(json, "{}");
    }

    #[test]
    #[serial]
    fn build_early_launch_context_captures_kitty_listen_on() {
        clear_launch_context_env();
        // SAFETY: test is #[serial].
        unsafe {
            std::env::set_var("KITTY_LISTEN_ON", "unix:/tmp/kitty.sock");
        }
        let json = build_early_launch_context();
        clear_launch_context_env();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["kitty_listen_on"], "unix:/tmp/kitty.sock");
        assert!(parsed.get("pane_id").is_none());
    }

    #[test]
    #[serial]
    fn build_early_launch_context_prefers_first_pane_id_var_in_priority_order() {
        clear_launch_context_env();
        // SAFETY: test is #[serial].
        unsafe {
            std::env::set_var("WEZTERM_PANE", "wezterm-pane");
            std::env::set_var("TMUX_PANE", "tmux-pane");
        }
        let json = build_early_launch_context();
        clear_launch_context_env();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["pane_id"], "wezterm-pane");
    }

    #[test]
    #[serial]
    fn build_early_launch_context_ignores_multiplexer_only_vars_when_absent() {
        // TMUX_PANE/ZELLIJ_PANE_ID never being set on Windows must degrade to
        // simply absent fields, not an error — same as on Unix outside a
        // multiplexer. This is the "no platform branching needed" behavior.
        clear_launch_context_env();
        // SAFETY: test is #[serial].
        unsafe {
            std::env::set_var("KITTY_WINDOW_ID", "kitty-window-1");
        }
        let json = build_early_launch_context();
        clear_launch_context_env();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["pane_id"], "kitty-window-1");
    }
}
