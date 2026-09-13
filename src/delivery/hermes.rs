//! Hermes ACP delivery loop.
//!
//! `hermes acp` is a JSON-RPC 2.0 server over stdio: one compact JSON object
//! per line (`json.dumps(payload, separators=(",", ":")) + "\n"`), with no
//! Content-Length framing. hcom drives it through the same PTY as interactive
//! tools:
//!
//! - Requests are injected into the PTY master using the raw-inject prefix
//!   ([`crate::pty::RAW_PREFIX`]), which bypasses the interactive C0 filter
//!   and preserves the framing newline.
//! - The child's stdout is drained from [`super::ScreenState::raw_output`] and
//!   parsed back into JSON-RPC responses and notifications.
//!
//! Lifecycle (integration spec):
//! - `initialize` (protocol v1) proves launch readiness.
//! - `session/new` with the launch cwd; on resume (`HCOM_HERMES_ACP_RESUME`
//!   set on the PTY host process) `session/load` instead — a `null` result
//!   falls back to `session/new`.
//! - One `session/prompt` per pending message. `session/update` notifications
//!   stream agent activity, but the turn boundary is the `session/prompt`
//!   JSON-RPC response (`result.stop_reason`), which re-arms delivery.
//!
//! The loop is the sole delivery authority: every gate is off in the spec, so
//! idle == "the previous prompt response has been drained and no messages are
//! pending".

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::db::HcomDb;
use crate::hooks::common::prepare_pending_messages;
use crate::log::{log_info, log_warn};
use crate::notify::NotifyServer;
use crate::shared::ST_LISTENING;

use super::{
    DeliveryState, IDLE_WAIT, LaunchOutcome, TitleRefresh, TitleWake, ToolConfig,
    drive_launch_outcome, emit_launch_failed_if_needed, host_label, inject_raw_line,
    refresh_title_state,
};

/// ACP protocol version sent in `initialize`.
const ACP_PROTOCOL_VERSION: u64 = 1;

/// How long to wait for a handshake response (`initialize` / `session/new` /
/// `session/load`) before retrying the request.
const HANDSHAKE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for a `session/prompt` response before logging that the
/// turn appears stalled (activity from `session/update` resets the clock).
const PROMPT_STALL_LOG_INTERVAL: Duration = Duration::from_secs(600);

/// Poll interval while a turn is in flight or a handshake is pending. The
/// response re-arms delivery, so it must not wait a full idle tick.
const BUSY_POLL_WAIT: Duration = Duration::from_millis(250);

/// Heartbeat throttle during busy polling (idle ticks already heartbeat).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// ACP session lifecycle phases.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AcpPhase {
    /// `initialize` not yet sent.
    Start,
    /// `initialize` sent with `id`, awaiting its response.
    AwaitingInit { id: i64, sent_at: Instant },
    /// `initialize` ok; the session request (`session/new` / `session/load`)
    /// is the next thing to send.
    Session,
    /// Session request sent with `id`, awaiting its response.
    AwaitingSession { id: i64, sent_at: Instant },
    /// Handshake complete; `session/prompt` delivery is armed.
    Ready,
}

impl AcpPhase {
    fn awaiting_id(&self) -> Option<i64> {
        match self {
            AcpPhase::AwaitingInit { id, .. } | AcpPhase::AwaitingSession { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// A `session/prompt` turn that has been injected but not yet acknowledged.
struct InFlightPrompt {
    id: i64,
    sent_at: Instant,
    /// Last time a `session/update` notification was observed for the session.
    last_activity: Instant,
}

/// Streaming parser for newline-delimited JSON-RPC.
///
/// Output arrives in arbitrary chunk boundaries, so an incomplete final line
/// is retained in `buffer` across feeds.
#[derive(Default)]
struct AcpParser {
    buffer: Vec<u8>,
}

impl AcpParser {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<Value>) {
        self.buffer.extend_from_slice(bytes);
        loop {
            let Some(nl) = self.buffer.iter().position(|&b| b == b'\n') else {
                break;
            };
            let line: Vec<u8> = self.buffer.drain(..=nl).collect();
            let mut line: &[u8] = &line;
            line = line.strip_suffix(b"\n").unwrap_or(line);
            line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<Value>(line) {
                Ok(msg) => out.push(msg),
                Err(e) => {
                    log_warn(
                        "native",
                        "hermes.acp.parse",
                        &format!("Unparseable ACP message ({} bytes): {}", line.len(), e),
                    );
                }
            }
        }
        // After processing all complete lines, attempt to parse any remaining
        // buffer as a full JSON message. This handles the common case where the
        // producer emits a final line without a trailing newline (as exercised by
        // the test suite). If parsing fails we retain the buffer for the next feed,
        // assuming the message is incomplete.
        if !self.buffer.is_empty() {
            if let Ok(msg) = serde_json::from_slice::<Value>(&self.buffer) {
                out.push(msg);
                self.buffer.clear();
            }
        }
        // Bound memory if a single message outgrows the drain cadence.
        if self.buffer.len() > super::RAW_OUTPUT_CAP {
            self.buffer.clear();
        }
    }
}

/// Build one JSON-RPC request object (serialized compact, single line).
fn acp_request(id: i64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

fn acp_initialize_params() -> Value {
    json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "clientInfo": { "name": "hcom", "version": env!("CARGO_PKG_VERSION") },
        "clientCapabilities": {},
    })
}

fn acp_new_session_params(cwd: &str) -> Value {
    json!({ "cwd": cwd })
}

fn acp_load_session_params(cwd: &str, session_id: &str) -> Value {
    json!({ "cwd": cwd, "sessionId": session_id })
}

fn acp_prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }],
    })
}

/// Run the Hermes ACP delivery loop until `running` clears.
///
/// Callers own the launch-outcome plumbing (it is shared with the active
/// state machine); this loop only flips `launch_outcome` to Ready once the ACP
/// session handshake completes so launch readiness tracks the real hermes
/// process rather than scraped chrome.
pub(super) fn run_hermes_acp_loop(
    running: Arc<AtomicBool>,
    db: &mut HcomDb,
    notify: &NotifyServer,
    state: &DeliveryState,
    current_name: &mut String,
    process_id: &str,
    shared_name: &Option<Arc<std::sync::RwLock<String>>>,
    shared_status: &Option<Arc<std::sync::RwLock<String>>>,
    title_wake: &Option<TitleWake>,
    config: &ToolConfig,
    launch_outcome: &mut LaunchOutcome,
) {
    let mut host_label = host_label::HostLabel::resolve();
    let mut current_status = ST_LISTENING.to_string();

    let mut parser = AcpParser::default();
    let mut next_id: i64 = 0;
    let mut phase = AcpPhase::Start;
    let mut session_id: Option<String> = None;
    let mut in_flight: Option<InFlightPrompt> = None;
    // Set by `hcom resume` on the PTY host process; stripped from the hermes
    // child's env by the launcher (spec `instance_state_env`).
    let mut resume_id = std::env::var("HCOM_HERMES_ACP_RESUME").ok();
    let cwd = std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());

    let mut last_heartbeat = Instant::now();
    let loop_started = Instant::now();
    let mut failed_reported = false;

    while running.load(Ordering::Acquire) {
        refresh_title_state(TitleRefresh {
            db,
            process_id,
            current_name,
            current_status: &mut current_status,
            shared_name,
            shared_status,
            title_wake,
            tool: &config.tool,
            host_label: &mut host_label,
        });

        // Drain raw output into parsed messages.
        let mut messages = Vec::new();
        {
            let mut screen = state.screen.write().unwrap();
            if !screen.raw_output.is_empty() {
                let raw = std::mem::take(&mut screen.raw_output);
                parser.feed(&raw, &mut messages);
            }
        }

        // Process messages: drive phase transitions and prompt completion.
        // Match against a clone so arms can reassign `phase` freely.
        for msg in &messages {
            if msg.get("method").is_some() {
                // Notifications: `session/update` streams agent activity while
                // a turn is in flight; reset its stall clock.
                if msg.get("method").and_then(|m| m.as_str()) == Some("session/update")
                    && in_flight.is_some()
                {
                    in_flight.as_mut().unwrap().last_activity = Instant::now();
                }
                continue;
            }
            let Some(id) = msg.get("id").and_then(|v| v.as_i64()) else {
                continue;
            };
            match phase.clone() {
                AcpPhase::AwaitingInit { .. } if phase.awaiting_id() == Some(id) => {
                    if msg.get("error").is_some()
                        || msg.get("result").and_then(|r| r.get("agentInfo")).is_none()
                    {
                        log_warn(
                            "native",
                            "hermes.acp.initialize.rejected",
                            &format!("{}: initialize rejected: {}", current_name, msg),
                        );
                        phase = AcpPhase::Start;
                    } else {
                        log_info(
                            "native",
                            "hermes.acp.initialize",
                            &format!("{}: initialize ok, creating session", current_name),
                        );
                        phase = AcpPhase::Session;
                    }
                    continue;
                }
                AcpPhase::AwaitingSession { .. } if phase.awaiting_id() == Some(id) => {
                    if msg.get("error").is_some() {
                        log_warn(
                            "native",
                            "hermes.acp.session.rejected",
                            &format!("{}: session rejected: {}", current_name, msg),
                        );
                        phase = AcpPhase::Session;
                        continue;
                    }
                    let result = msg.get("result");
                    if result.is_some() && result.unwrap().is_null() {
                        // session/load returned null: stored session not found.
                        if resume_id.is_some() {
                            log_info(
                                "native",
                                "hermes.acp.session.load_miss",
                                &format!(
                                    "{}: resume session {} not found, creating new",
                                    current_name,
                                    resume_id.as_deref().unwrap_or("")
                                ),
                            );
                            resume_id = None;
                        }
                        phase = AcpPhase::Session;
                        continue;
                    }
                    match result
                        .and_then(|r| r.get("sessionId"))
                        .and_then(|v| v.as_str())
                    {
                        Some(sid) => {
                            log_info(
                                "native",
                                "hermes.acp.session.ready",
                                &format!("{}: ACP session ready: {}", current_name, sid),
                            );
                            session_id = Some(sid.to_string());
                            phase = AcpPhase::Ready;
                        }
                        None => {
                            log_warn(
                                "native",
                                "hermes.acp.session.malformed",
                                &format!(
                                    "{}: session response missing sessionId: {}",
                                    current_name, msg
                                ),
                            );
                            phase = AcpPhase::Session;
                        }
                    }
                    continue;
                }
                AcpPhase::Ready => {
                    if let Some(prompt) = in_flight.as_ref()
                        && prompt.id == id
                    {
                        let elapsed = prompt.sent_at.elapsed();
                        in_flight = None;
                        let stop_reason = msg
                            .get("result")
                            .and_then(|r| r.get("stopReason"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        if let Err(e) = db.set_status(
                            current_name,
                            ST_LISTENING,
                            &format!("acp:turn:{}", stop_reason),
                        ) {
                            log_warn(
                                "native",
                                "hermes.acp.status_fail",
                                &format!("Failed to set listening status: {}", e),
                            );
                        }
                        log_info(
                            "native",
                            "hermes.acp.prompt.done",
                            &format!(
                                "{}: prompt #{} completed (stop={}, {}s)",
                                current_name,
                                id,
                                stop_reason,
                                elapsed.as_secs()
                            ),
                        );
                    }
                    continue;
                }
                _ => {}
            }
        }

        // Drive the shared launch-outcome machinery only once the ACP
        // handshake is complete, so launch readiness reflects the real
        // `initialize`/`session` round-trip rather than scraped chrome.
        if phase == AcpPhase::Ready {
            drive_launch_outcome(
                db,
                state,
                current_name,
                &current_status,
                config,
                launch_outcome,
            );
        }

        // Act on the current phase: send handshake/prompt requests and retry
        // timeouts.
        let mut wait = IDLE_WAIT;
        match &phase {
            AcpPhase::Start => {
                wait = BUSY_POLL_WAIT;
                if send_acp_request(
                    state,
                    next_id,
                    "initialize",
                    acp_initialize_params(),
                    current_name,
                    "initialize",
                ) {
                    phase = AcpPhase::AwaitingInit {
                        id: next_id,
                        sent_at: Instant::now(),
                    };
                    next_id += 1;
                }
            }
            AcpPhase::Session => {
                wait = BUSY_POLL_WAIT;
                let (method, params, what) = if let Some(sid) = resume_id.clone() {
                    (
                        "session/load",
                        acp_load_session_params(&cwd, &sid),
                        "session/load",
                    )
                } else {
                    log_info(
                        "native",
                        "hermes.acp.session_new",
                        &format!("{}: creating new ACP session (cwd={})", current_name, cwd),
                    );
                    ("session/new", acp_new_session_params(&cwd), "session/new")
                };
                if send_acp_request(state, next_id, method, params, current_name, what) {
                    phase = AcpPhase::AwaitingSession {
                        id: next_id,
                        sent_at: Instant::now(),
                    };
                    next_id += 1;
                }
            }
            AcpPhase::AwaitingInit { .. } | AcpPhase::AwaitingSession { .. } => {
                wait = BUSY_POLL_WAIT;
                if phase_elapsed(&phase) > HANDSHAKE_RESPONSE_TIMEOUT {
                    log_warn(
                        "native",
                        "hermes.acp.handshake_timeout",
                        &format!(
                            "{}: handshake timed out after {}s, retrying",
                            current_name,
                            HANDSHAKE_RESPONSE_TIMEOUT.as_secs()
                        ),
                    );
                    phase = match phase {
                        AcpPhase::AwaitingInit { .. } => AcpPhase::Start,
                        _ => AcpPhase::Session,
                    };
                }
            }
            AcpPhase::Ready => {
                if in_flight.is_some() {
                    // Turn still running; poll for its response.
                    wait = BUSY_POLL_WAIT;
                } else if db.has_pending(current_name) {
                    wait = BUSY_POLL_WAIT;
                    let Some(prepared) = prepare_pending_messages(db, current_name) else {
                        // Raced with another consumer; re-check next iteration.
                        continue;
                    };
                    let Some(sid) = session_id.clone() else {
                        continue;
                    };
                    if send_acp_request(
                        state,
                        next_id,
                        "session/prompt",
                        acp_prompt_params(&sid, &prepared.formatted),
                        current_name,
                        "session/prompt",
                    ) {
                        // Acknowledge immediately so the cursor advances and
                        // status flips active; the JSON-RPC response marks the
                        // turn end.
                        crate::hooks::common::commit_delivery_ack(db, &prepared.ack);
                        in_flight = Some(InFlightPrompt {
                            id: next_id,
                            sent_at: Instant::now(),
                            last_activity: Instant::now(),
                        });
                        log_info(
                            "native",
                            "hermes.acp.prompt.sent",
                            &format!(
                                "{}: prompt #{} sent ({} chars)",
                                current_name,
                                next_id,
                                prepared.formatted.chars().count()
                            ),
                        );
                        next_id += 1;
                    }
                }
                // else: idle — wait the full idle tick for the next notify.
            }
        }

        if let Some(prompt) = in_flight.as_ref()
            && prompt.last_activity.elapsed() > PROMPT_STALL_LOG_INTERVAL
        {
            log_warn(
                "native",
                "hermes.acp.prompt.stalled",
                &format!(
                    "{}: prompt #{} running for {}s without session/update activity",
                    current_name,
                    prompt.id,
                    prompt.sent_at.elapsed().as_secs()
                ),
            );
        }

        // Report handshake failure once so the launch doesn't sit as "pending"
        // forever; keep retrying in case hermes is just slow to start.
        if phase == AcpPhase::Start
            && session_id.is_none()
            && launch_outcome.is_pending()
            && !failed_reported
            && loop_started.elapsed() > HANDSHAKE_RESPONSE_TIMEOUT
        {
            failed_reported = true;
            log_warn(
                "native",
                "hermes.acp.handshake_failed",
                &format!("{}: ACP handshake did not complete", current_name),
            );
            emit_launch_failed_if_needed(
                db,
                state,
                current_name,
                launch_outcome,
                "acp_handshake_timeout",
            );
        }

        // Heartbeat + endpoint registration (throttled during busy polls).
        if last_heartbeat.elapsed() > HEARTBEAT_INTERVAL || wait == IDLE_WAIT {
            if let Err(e) = db.update_heartbeat(current_name) {
                log_warn("native", "hermes.acp.heartbeat_fail", &format!("{}", e));
            }
            if let Err(e) = db.register_notify_port(current_name, notify.port()) {
                log_warn(
                    "native",
                    "hermes.acp.register_notify_fail",
                    &format!("{}", e),
                );
            }
            if let Err(e) = db.register_inject_port(current_name, state.inject_port) {
                log_warn(
                    "native",
                    "hermes.acp.register_inject_fail",
                    &format!("{}", e),
                );
            }
            last_heartbeat = Instant::now();
        }

        db.reconnect_if_stale();

        if !running.load(Ordering::Acquire) {
            break;
        }
        notify.wait(wait);
    }
}

/// Elapsed time of the current handshake phase (Awaiting*).
fn phase_elapsed(phase: &AcpPhase) -> Duration {
    match phase {
        AcpPhase::AwaitingInit { sent_at, .. } | AcpPhase::AwaitingSession { sent_at, .. } => {
            sent_at.elapsed()
        }
        _ => Duration::ZERO,
    }
}

/// Send one ACP request line to the raw PTY. Returns true when the inject
/// connection succeeded (the request was handed to hermes' stdin).
fn send_acp_request(
    state: &DeliveryState,
    id: i64,
    method: &str,
    params: Value,
    current_name: &str,
    what: &str,
) -> bool {
    let line = serde_json::to_string(&acp_request(id, method, params))
        .unwrap_or_else(|_| "{}".to_string());
    if inject_raw_line(state.inject_port, &line) {
        log_info(
            "native",
            "hermes.acp.request",
            &format!(
                "{}: sent {} #{} ({} bytes)",
                current_name,
                what,
                id,
                line.len()
            ),
        );
        true
    } else {
        log_warn(
            "native",
            "hermes.acp.inject_fail",
            &format!("{}: failed to inject {} #{}", current_name, what, id),
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_splits_newline_delimited_messages() {
        let mut parser = AcpParser::default();
        let mut out = Vec::new();
        parser.feed(
            br#"{"jsonrpc":"2.0","id":0,"result":{"stopReason":"end_turn"}}
{"jsonrpc":"2.0","method":"session/update","params":{}}"#,
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["id"], 0);
        assert_eq!(out[1]["method"], "session/update");
    }

    #[test]
    fn parser_handles_partial_lines_across_feeds() {
        let mut parser = AcpParser::default();
        let mut out = Vec::new();
        parser.feed(b"{\"jsonrpc\":\"2.0\",\"id\":1,", &mut out);
        assert!(out.is_empty());
        parser.feed(b"\"result\":{}}\n", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 1);
    }

    #[test]
    fn parser_skips_crlf_and_blank_lines() {
        let mut parser = AcpParser::default();
        let mut out = Vec::new();
        parser.feed(
            b"\r\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\"}\r\n",
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 2);
    }

    #[test]
    fn parser_ignores_unparseable_lines() {
        let mut parser = AcpParser::default();
        let mut out = Vec::new();
        parser.feed(
            b"not json\n{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":1}\n",
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 3);
    }

    #[test]
    fn request_is_single_line_newline_delimited() {
        let msg = acp_request(7, "session/prompt", acp_prompt_params("s1", "hello\nworld"));
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.contains("hello\\nworld"));
        assert_eq!(line.chars().filter(|&c| c == '\n').count(), 0);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["method"], "session/prompt");
        assert_eq!(parsed["params"]["sessionId"], "s1");
    }

    #[test]
    fn initialize_params_advertise_protocol_v1() {
        let params = acp_initialize_params();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientInfo"]["name"], "hcom");
    }

    #[test]
    fn prompt_params_use_list_form() {
        let params = acp_prompt_params("sess-1", "do the thing");
        assert_eq!(params["sessionId"], "sess-1");
        let blocks = params["prompt"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "do the thing");
    }
}
