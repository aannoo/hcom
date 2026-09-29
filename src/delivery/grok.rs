//! Grok delivery through Grok's native prompt queue (ACP).
//!
//! `hcom grok` runs the TUI against a private leader socket. This thread
//! attaches a second client to the same session (`grok agent --leader
//! --leader-socket <sock> stdio`, the official stdio bridge, which handles
//! leader framing and reconnects) and queues each mailbox batch as an ordinary
//! prompt with `sendNow:false`. Nothing is typed into the TUI composer, so the
//! user's draft is never touched, and a busy session simply runs the batch
//! after its current work.
//!
//! Grok uses our `promptId` as the queue entry id. The batch is acknowledged
//! when Grok reports that entry running (`x.ai/queue/changed`), i.e. once it
//! is part of the model's turn, or when the prompt request returns a result.
//! A batch that never started (removed from the queue, transport lost) stays
//! unread and is queued again later: at-least-once, never silently dropped.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::db::HcomDb;
use crate::hooks::{DeliveryAck, common};
use crate::notify::NotifyServer;
use crate::shared::ST_LISTENING;

use super::{DeliveryState, LaunchOutcome, TitleWake, ToolConfig, log_info, log_warn};

const POLL: Duration = Duration::from_millis(100);
const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Wait before re-queueing a batch whose prompt was dropped before it ran
/// (user removed it from the queue, prompt error, transport lost).
const REQUEUE_DELAY: Duration = Duration::from_secs(10);
/// Leader-mode Grok ignores these (it warns and continues). Since hcom must
/// run Grok against a leader, reject them rather than silently drop a
/// restriction the user asked for.
const LEADER_IGNORED_FLAGS: &[&str] = &[
    "--allow",
    "--deny",
    "--allowedTools",
    "--disallowedTools",
    "--disable-web-search",
];

#[derive(Clone, Debug)]
pub(crate) struct Launch {
    command: String,
    prefix: Vec<String>,
    socket: String,
    no_subagents: bool,
}

/// Flags before a `--` prompt marker.
fn flags<'a>(args: &'a [&'a str]) -> impl Iterator<Item = &'a str> {
    args.iter().copied().take_while(|arg| *arg != "--")
}

impl Launch {
    pub(crate) fn validate_args(args: &[&str]) -> Result<()> {
        for arg in flags(args) {
            let flag = arg.split('=').next().unwrap_or(arg);
            if matches!(flag, "--leader" | "--no-leader" | "--leader-socket") {
                bail!("hcom manages Grok's leader connection; remove {flag}");
            }
            if LEADER_IGNORED_FLAGS.contains(&flag) {
                bail!(
                    "Grok ignores {flag} when attached to a leader, which hcom needs for \
                     message delivery; set the rule in Grok's config instead"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn new(command: &str, prefix: &[String], args: &[&str]) -> Result<Self> {
        Self::validate_args(args)?;
        Ok(Self {
            command: command.to_string(),
            prefix: prefix.to_vec(),
            socket: std::env::temp_dir()
                .join(format!("hcom-grok-{}.sock", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned(),
            no_subagents: flags(args).any(|arg| arg == "--no-subagents"),
        })
    }

    pub(crate) fn tui_args(&self) -> Vec<String> {
        vec![
            "--leader".into(),
            "--leader-socket".into(),
            self.socket.clone(),
        ]
    }

    /// Stop this launch's private leader and remove its socket files.
    ///
    /// Grok leaders run with `--no-exit-on-disconnect` (they are meant to be
    /// shared), so this one would outlive the agent. The leader writes its
    /// PID to `<socket>.lock`.
    pub(crate) fn stop_leader(&self) {
        let socket = std::path::Path::new(&self.socket);
        let lock = socket.with_extension("lock");
        let pid = std::fs::read_to_string(&lock)
            .ok()
            .and_then(|content| content.trim().parse::<u32>().ok());
        if let Some(pid) = pid.filter(|pid| crate::sys::process::is_alive(*pid)) {
            crate::sys::process::terminate(pid);
            let deadline = Instant::now() + Duration::from_secs(3);
            while crate::sys::process::is_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(POLL);
            }
            if crate::sys::process::is_alive(pid) {
                crate::sys::process::kill(pid);
            }
            log_info("native", "grok.leader.stopped", &format!("pid={pid}"));
        }
        let _ = std::fs::remove_file(socket);
        let _ = std::fs::remove_file(lock);
    }

    /// Env for the TUI and the ACP client. The leader is spawned by whichever
    /// connects first and inherits it; `--no-subagents` itself has no effect
    /// in leader mode.
    pub(crate) fn child_env(&self) -> Vec<(String, String)> {
        if self.no_subagents {
            vec![("GROK_SUBAGENTS".into(), "0".into())]
        } else {
            Vec::new()
        }
    }
}

#[derive(Debug)]
enum Event {
    Response(Value),
    Queue(Value),
    /// A request from Grok (permission prompt, ...). The TUI answers it.
    Interaction(String),
    Closed(String),
}

struct Client {
    child: Child,
    input: ChildStdin,
    events: mpsc::Receiver<Event>,
    next_id: u64,
}

impl Client {
    fn connect(
        launch: &Launch,
        session: &str,
        cwd: &str,
        running: &AtomicBool,
        deadline: Instant,
    ) -> Result<Self> {
        let mut command = Command::new(&launch.command);
        command
            .args(&launch.prefix)
            .args([
                "agent",
                "--leader",
                "--leader-socket",
                &launch.socket,
                "stdio",
            ])
            .current_dir(cwd)
            .envs(launch.child_env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn().context("start Grok ACP client")?;
        let input = child.stdin.take().context("ACP stdin unavailable")?;
        let output = child.stdout.take().context("ACP stdout unavailable")?;
        let stderr = child.stderr.take().context("ACP stderr unavailable")?;
        // The bridge is invisible to the user; its errors only surface here.
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    log_warn(
                        "native",
                        "grok.acp.stderr",
                        &super::truncate_chars(&line, 500),
                    );
                }
            }
        });
        let (sender, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let event = match line {
                    Ok(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(value) => classify_event(value),
                        Err(_) => Some(Event::Closed("invalid JSON from Grok ACP client".into())),
                    },
                    Err(error) => Some(Event::Closed(format!("ACP read failed: {error}"))),
                };
                if let Some(event) = event {
                    let closed = matches!(event, Event::Closed(_));
                    if sender.send(event).is_err() || closed {
                        return;
                    }
                }
            }
            let _ = sender.send(Event::Closed("Grok ACP client exited".into()));
        });
        let mut client = Self {
            child,
            input,
            events,
            next_id: 0,
        };
        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
                "clientInfo": {"name": "hcom", "version": env!("CARGO_PKG_VERSION")}
            }),
            running,
            deadline,
        )?;
        // Grok picks the method (cached login, API key, ...); re-deriving it
        // here would break setups it already handles.
        if let Some(method) = init["_meta"]["defaultAuthMethodId"].as_str() {
            client.request(
                "authenticate",
                json!({"methodId": method}),
                running,
                deadline,
            )?;
        }
        client.request(
            "session/load",
            json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
            running,
            deadline,
        )?;
        log_info(
            "native",
            "grok.acp.connected",
            &format!("session={session}"),
        );
        Ok(client)
    }

    fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        serde_json::to_writer(&mut self.input, &request)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        Ok(id)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        running: &AtomicBool,
        deadline: Instant,
    ) -> Result<Value> {
        let id = self.send(method, params)?;
        while running.load(Ordering::Acquire) && Instant::now() < deadline {
            match self.events.recv_timeout(POLL) {
                Ok(Event::Response(value)) if value["id"].as_u64() == Some(id) => {
                    if let Some(error) = value.get("error") {
                        bail!("{method}: {error}");
                    }
                    return Ok(value["result"].clone());
                }
                Ok(Event::Closed(error)) => bail!("{error}"),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("ACP reader stopped"),
            }
        }
        bail!("{method}: timed out")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn classify_event(value: Value) -> Option<Event> {
    match value.get("method").and_then(Value::as_str) {
        Some("x.ai/queue/changed" | "_x.ai/queue/changed") => {
            Some(Event::Queue(value["params"].clone()))
        }
        Some(method) if value.get("id").is_some() => Some(Event::Interaction(method.to_string())),
        Some(_) => None,
        None if value.get("id").is_some() => Some(Event::Response(value)),
        None => None,
    }
}

/// A queued batch awaiting evidence that Grok ran it.
struct InFlight {
    request_id: u64,
    prompt_id: String,
    ack: DeliveryAck,
    /// Seen waiting in Grok's queue.
    queued: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Part of a turn: acknowledge.
    Delivered,
    /// Dropped before it ran: leave unread and queue again later.
    Dropped(String),
}

/// What a queue snapshot says about our prompt, if anything.
fn queue_outcome(params: &Value, flight: &InFlight) -> Option<Outcome> {
    let running = params["runningPromptId"].as_str();
    if running == Some(flight.prompt_id.as_str()) {
        return Some(Outcome::Delivered);
    }
    let listed = params["entries"]
        .as_array()
        .is_some_and(|entries| entries.iter().any(|e| e["id"] == flight.prompt_id.as_str()));
    if listed || !flight.queued {
        return None;
    }
    // It left the queue without becoming the running prompt: either merged
    // into a combined turn with the entries ahead of it, or removed.
    if params["runningCombinedTexts"]
        .as_array()
        .is_some_and(|texts| texts.len() >= 2)
    {
        Some(Outcome::Delivered)
    } else {
        Some(Outcome::Dropped("removed from Grok's queue".into()))
    }
}

/// What the `session/prompt` response says about our prompt.
fn response_outcome(value: &Value) -> Outcome {
    if let Some(error) = value.get("error") {
        return Outcome::Dropped(format!("prompt failed: {error}"));
    }
    match value["result"]["stopReason"].as_str() {
        // Every other stop reason ends a turn that ran the prompt.
        Some("cancelled") => Outcome::Dropped("prompt cancelled".into()),
        _ => Outcome::Delivered,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    launch: &Launch,
    running: &Arc<AtomicBool>,
    db: &mut HcomDb,
    notify: &NotifyServer,
    state: &DeliveryState,
    process_id: &str,
    current_name: &mut String,
    config: &ToolConfig,
    shared_name: &Option<Arc<RwLock<String>>>,
    shared_status: &Option<Arc<RwLock<String>>>,
    title_wake: &Option<TitleWake>,
    host_label: &mut super::host_label::HostLabel,
    launch_outcome: &mut LaunchOutcome,
) {
    let mut client: Option<Client> = None;
    let mut session = String::new();
    let mut in_flight: Option<InFlight> = None;
    let mut current_status = ST_LISTENING.to_string();
    let mut heartbeat = Instant::now();
    let mut connect_failures: u32 = 0;
    let mut retry_at = Instant::now();
    let mut requeue_at = Instant::now();
    let launched_at = Instant::now();
    while running.load(Ordering::Acquire) {
        super::refresh_title_state(super::TitleRefresh {
            db,
            process_id,
            current_name,
            current_status: &mut current_status,
            shared_name,
            shared_status,
            title_wake,
            tool: &config.tool,
            host_label,
        });
        if client.is_some() {
            super::drive_launch_outcome(
                db,
                state,
                current_name,
                &current_status,
                config,
                launch_outcome,
            );
        }
        if heartbeat.elapsed() >= Duration::from_secs(5) {
            db.reconnect_if_stale();
            let _ = db.update_heartbeat(current_name);
            let _ = db.register_notify_port(current_name, notify.port());
            let _ = db.register_inject_port(current_name, state.inject_port);
            heartbeat = Instant::now();
        }
        let instance = match db.get_instance_full(current_name) {
            Ok(Some(instance)) => instance,
            Ok(None) => break,
            Err(error) => {
                log_warn("native", "grok.acp.instance_error", &format!("{error}"));
                notify.wait(POLL);
                continue;
            }
        };
        // The session id arrives with Grok's SessionStart hook.
        let Some(active_session) = instance.session_id.filter(|id| !id.is_empty()) else {
            notify.wait(POLL);
            continue;
        };
        if session != active_session {
            // New session (/new, resume): the old prompt belongs to it.
            if let Some(flight) = in_flight.take() {
                log_warn(
                    "native",
                    "grok.acp.requeue",
                    &format!("prompt={} session changed before it ran", flight.prompt_id),
                );
            }
            client = None;
            session = active_session;
            connect_failures = 0;
            retry_at = Instant::now();
        }
        if client.is_none() {
            if Instant::now() < retry_at {
                notify.wait(POLL);
                continue;
            }
            match Client::connect(
                launch,
                &session,
                &instance.directory,
                running,
                Instant::now() + SETUP_TIMEOUT,
            ) {
                Ok(connected) => {
                    client = Some(connected);
                    connect_failures = 0;
                }
                Err(error) => {
                    if !running.load(Ordering::Acquire) {
                        break;
                    }
                    connect_failures += 1;
                    let backoff =
                        Duration::from_secs(1 << connect_failures.min(5)).min(RECONNECT_MAX);
                    retry_at = Instant::now() + backoff;
                    let detail = format!("Grok ACP connection failed: {error:#}");
                    log_warn(
                        "native",
                        "grok.acp.connect_failed",
                        &format!("attempt={connect_failures} retry_in={backoff:?}: {detail}"),
                    );
                    let _ = db.set_gate_status(current_name, "acp_disconnected", &detail);
                    if launch_outcome.is_pending() && launched_at.elapsed() >= SETUP_TIMEOUT {
                        let _ = db.set_status(
                            current_name,
                            crate::shared::ST_BLOCKED,
                            "launch_blocked",
                        );
                        let _ = db.emit_launch_blocked_event(
                            current_name,
                            crate::shared::ST_BLOCKED,
                            "launch_blocked",
                            "grok_acp_connection",
                            &detail,
                        );
                        super::mark_launch_phase_complete(
                            state,
                            launch_outcome,
                            LaunchOutcome::Blocked,
                        );
                    }
                    continue;
                }
            }
        }
        let Some(conn) = client.as_mut() else {
            continue;
        };

        let mut outcome = None;
        let mut closed = None;
        while let Ok(event) = conn.events.try_recv() {
            match event {
                Event::Closed(error) => {
                    closed = Some(error);
                    break;
                }
                Event::Queue(params) => {
                    if params["sessionId"].as_str() != Some(session.as_str()) {
                        continue;
                    }
                    if let Some(flight) = in_flight.as_mut() {
                        flight.queued |= params["entries"].as_array().is_some_and(|entries| {
                            entries.iter().any(|e| e["id"] == flight.prompt_id.as_str())
                        });
                        if let Some(result) = queue_outcome(&params, flight) {
                            outcome = Some(result);
                        }
                    }
                }
                Event::Response(value) => {
                    if let Some(flight) = in_flight.as_ref()
                        && value["id"].as_u64() == Some(flight.request_id)
                    {
                        outcome = Some(response_outcome(&value));
                    }
                }
                Event::Interaction(method) => {
                    // The TUI answers; hcom only reports the wait. The next
                    // tool or turn-end hook clears it.
                    log_info(
                        "native",
                        "grok.acp.interaction",
                        &format!("TUI owns {method}"),
                    );
                    if method == "session/request_permission" {
                        crate::instance_lifecycle::set_status(
                            db,
                            current_name,
                            crate::shared::ST_BLOCKED,
                            "approval",
                            Default::default(),
                        );
                    }
                }
            }
            if outcome.is_some() {
                break;
            }
        }
        if let Some(error) = closed {
            client = None;
            retry_at = Instant::now() + Duration::from_secs(1);
            outcome = in_flight
                .as_ref()
                .map(|_| Outcome::Dropped(format!("{error} before the prompt ran")));
            log_warn("native", "grok.acp.disconnected", &error);
        }
        if let Some(outcome) = outcome
            && let Some(flight) = in_flight.take()
        {
            match outcome {
                Outcome::Delivered => {
                    common::commit_delivery_ack(db, &flight.ack);
                    log_info(
                        "native",
                        "grok.acp.ack",
                        &format!(
                            "instance={} prompt={} cursor={}",
                            flight.ack.instance_name, flight.prompt_id, flight.ack.last_event_id
                        ),
                    );
                }
                Outcome::Dropped(reason) => {
                    // Still unread; queue it again after a pause.
                    requeue_at = Instant::now() + REQUEUE_DELAY;
                    log_warn(
                        "native",
                        "grok.acp.requeue",
                        &format!("prompt={} {reason}; kept unread", flight.prompt_id),
                    );
                }
            }
        }
        let Some(conn) = client.as_mut() else {
            continue;
        };
        if in_flight.is_none()
            && Instant::now() >= requeue_at
            && !matches!(current_status.as_str(), "stopped" | "inactive")
            && let Some(prepared) = common::prepare_pending_messages(db, current_name)
        {
            let prompt_id = format!(
                "{}{}",
                crate::hooks::grok::HCOM_PROMPT_ID_PREFIX,
                uuid::Uuid::new_v4()
            );
            match conn.send(
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{"type": "text", "text": prepared.formatted}],
                    "_meta": {"promptId": prompt_id, "sendNow": false, "clientIdentifier": "hcom"}
                }),
            ) {
                Ok(request_id) => {
                    log_info(
                        "native",
                        "grok.acp.enqueued",
                        &format!(
                            "instance={current_name} prompt={prompt_id} cursor={}",
                            prepared.ack.last_event_id
                        ),
                    );
                    in_flight = Some(InFlight {
                        request_id,
                        prompt_id,
                        ack: prepared.ack,
                        queued: false,
                    });
                }
                Err(error) => {
                    log_warn("native", "grok.acp.write_failed", &format!("{error:#}"));
                    client = None;
                    retry_at = Instant::now() + Duration::from_secs(1);
                }
            }
        }
        notify.wait(POLL);
    }
    // Dropping the ACP client does not close the TUI's session.
    drop(client);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flight(queued: bool) -> InFlight {
        InFlight {
            request_id: 4,
            prompt_id: "hcom-1".into(),
            ack: DeliveryAck {
                instance_name: "nova".into(),
                last_event_id: 7,
                status_context: "deliver:luna".into(),
                msg_ts: String::new(),
                mark_announced: false,
            },
            queued,
        }
    }

    #[test]
    fn running_prompt_is_delivered() {
        let params = json!({"sessionId": "s", "runningPromptId": "hcom-1", "entries": []});
        assert_eq!(
            queue_outcome(&params, &flight(false)),
            Some(Outcome::Delivered)
        );
    }

    #[test]
    fn queued_prompt_waits() {
        let params =
            json!({"sessionId": "s", "runningPromptId": "user-1", "entries": [{"id": "hcom-1"}]});
        assert_eq!(queue_outcome(&params, &flight(false)), None);
        assert_eq!(queue_outcome(&params, &flight(true)), None);
    }

    #[test]
    fn prompt_leaving_queue_is_combined_or_dropped() {
        let unrelated = json!({"sessionId": "s", "runningPromptId": "user-1", "entries": []});
        // Not seen queued yet: the snapshot may predate our enqueue.
        assert_eq!(queue_outcome(&unrelated, &flight(false)), None);
        assert!(matches!(
            queue_outcome(&unrelated, &flight(true)),
            Some(Outcome::Dropped(_))
        ));
        let combined = json!({
            "sessionId": "s",
            "runningPromptId": "user-1",
            "runningCombinedTexts": ["fix it", "<hcom>...</hcom>"],
            "entries": []
        });
        assert_eq!(
            queue_outcome(&combined, &flight(true)),
            Some(Outcome::Delivered)
        );
    }

    #[test]
    fn response_outcomes() {
        for reason in ["end_turn", "max_tokens", "max_turn_requests", "refusal"] {
            let value = json!({"id": 4, "result": {"stopReason": reason}});
            assert_eq!(response_outcome(&value), Outcome::Delivered, "{reason}");
        }
        for value in [
            json!({"id": 4, "result": {"stopReason": "cancelled"}}),
            json!({"id": 4, "error": {"code": -32000, "message": "failed"}}),
        ] {
            assert!(matches!(response_outcome(&value), Outcome::Dropped(_)));
        }
    }

    #[test]
    fn classifies_queue_and_reverse_requests() {
        let queue = json!({"method": "x.ai/queue/changed", "params": {"sessionId": "s"}});
        assert!(matches!(classify_event(queue), Some(Event::Queue(_))));
        let permission =
            json!({"id": "ask-1", "method": "session/request_permission", "params": {}});
        assert!(matches!(
            classify_event(permission),
            Some(Event::Interaction(_))
        ));
        let response = json!({"id": 3, "result": {}});
        assert!(matches!(classify_event(response), Some(Event::Response(_))));
        assert!(classify_event(json!({"method": "session/update", "params": {}})).is_none());
    }

    #[test]
    fn rejects_flags_leader_mode_ignores() {
        for args in [
            vec!["--leader"],
            vec!["--leader-socket=/tmp/x"],
            vec!["--allow", "Bash"],
            vec!["--deny=bash"],
            vec!["--allowedTools", "Read(*)"],
            vec!["--disallowedTools", "Bash(*)"],
            vec!["--disable-web-search"],
        ] {
            assert!(Launch::validate_args(&args).is_err(), "{args:?}");
        }
        // After `--` they are prompt text.
        assert!(Launch::validate_args(&["--", "--deny", "--leader"]).is_ok());
        assert!(Launch::validate_args(&["--model", "grok-build", "--always-approve"]).is_ok());
    }

    #[test]
    fn no_subagents_reaches_leader_through_env() {
        let launch = Launch::new("grok", &["prefix".into()], &["--no-subagents"]).unwrap();
        assert_eq!(launch.prefix, ["prefix"]);
        assert_eq!(launch.tui_args()[0], "--leader");
        assert_eq!(launch.child_env(), [("GROK_SUBAGENTS".into(), "0".into())]);
        let plain = Launch::new("grok", &[], &["--", "--no-subagents"]).unwrap();
        assert!(plain.child_env().is_empty());
    }
}
