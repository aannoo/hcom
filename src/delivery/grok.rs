//! Thin native ACP transport. The existing hcom mailbox remains authoritative.

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

#[derive(Clone, Debug)]
pub(crate) struct Launch {
    command: String,
    prefix: Vec<String>,
    socket: String,
    no_subagents: bool,
    policy_args: Vec<String>,
}

impl Launch {
    pub(crate) fn validate_args(args: &[&str]) -> Result<()> {
        if args.iter().take_while(|arg| **arg != "--").any(|arg| {
            matches!(*arg, "--leader" | "--no-leader" | "--leader-socket")
                || arg.starts_with("--leader-socket=")
        }) {
            bail!("hcom manages Grok's leader connection; remove custom leader flags");
        }
        Self::policy_args(args)?;
        Ok(())
    }

    fn policy_args(args: &[&str]) -> Result<Vec<String>> {
        let mut policy = Vec::new();
        let mut args = args.iter().take_while(|arg| **arg != "--");
        while let Some(arg) = args.next() {
            let flag = arg.split('=').next().unwrap_or(arg);
            if matches!(
                flag,
                "--allow"
                    | "--deny"
                    | "--allowedTools"
                    | "--disallowedTools"
                    | "--disable-web-search"
            ) {
                policy.push((*arg).to_string());
                if flag != "--disable-web-search" && !arg.contains('=') {
                    policy.push(
                        args.next()
                            .with_context(|| format!("{flag} requires a rule"))?
                            .to_string(),
                    );
                }
            }
        }
        Ok(policy)
    }

    pub(crate) fn check_policy_support(mut command: Command, args: &[&str]) -> Result<()> {
        if Self::policy_args(args)?.is_empty() {
            return Ok(());
        }
        // Probe the actual inherited-policy CLI entry, not a version number.
        command
            .args(["agent", "leader", "--launch-policy", "{}", "--help"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .context("check native Grok policy support")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => bail!(
                    "This Grok binary does not support leader policy inheritance; use a policy-capable Grok build for --allow/--deny or --disable-web-search"
                ),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("Native Grok policy support check timed out; launch stopped");
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error).context("check native Grok policy support");
                }
            }
        }
    }

    pub(crate) fn new(command: &str, prefix: &[String], args: &[&str]) -> Result<Self> {
        Self::validate_args(args)?;
        let mut probe = Command::new(command);
        probe.args(prefix);
        Self::check_policy_support(probe, args)?;
        Ok(Self {
            command: command.to_string(),
            prefix: prefix.to_vec(),
            socket: std::env::temp_dir()
                .join(format!("hcom-grok-{}.sock", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned(),
            no_subagents: args
                .iter()
                .take_while(|arg| **arg != "--")
                .any(|arg| *arg == "--no-subagents"),
            policy_args: Self::policy_args(args)?,
        })
    }

    pub(crate) fn tui_args(&self) -> Vec<String> {
        vec![
            "--leader".into(),
            "--leader-socket".into(),
            self.socket.clone(),
        ]
    }

    pub(crate) fn child_env(&self) -> Vec<(String, String)> {
        let mut env = vec![("HCOM_GROK_ACP".into(), "1".into())];
        if self.no_subagents {
            env.push(("GROK_SUBAGENTS".into(), "0".into()));
        }
        env
    }
}

enum Event {
    Response(Value),
    Queue(Value),
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
        if Instant::now() >= deadline {
            bail!("ACP setup deadline expired");
        }
        let mut command = Command::new(&launch.command);
        command
            .args(&launch.prefix)
            .args(&launch.policy_args)
            .args([
                "agent",
                "--leader",
                "--leader-socket",
                &launch.socket,
                "stdio",
            ]);
        command
            .current_dir(cwd)
            .envs(launch.child_env())
            .env("HCOM_LAUNCHED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn().context("start native Grok ACP client")?;
        let input = child.stdin.take().context("ACP stdin unavailable")?;
        let output = child.stdout.take().context("ACP stdout unavailable")?;
        let (sender, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let event = match line {
                    Ok(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(value) => classify_event(value),
                        Err(_) => Some(Event::Closed("invalid JSON from native ACP client".into())),
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
            let _ = sender.send(Event::Closed("native ACP client disconnected".into()));
        });
        let mut client = Self {
            child,
            input,
            events,
            next_id: 0,
        };
        client.request("initialize", json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "hcom", "version": env!("CARGO_PKG_VERSION")}
        }), running, deadline)?;
        client.request(
            "authenticate",
            json!({"methodId": "cached_token"}),
            running,
            deadline,
        )?;
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
        if Instant::now() >= deadline {
            bail!("{method}: ACP setup deadline expired");
        }
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
                Ok(event) => observe_event(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("ACP reader stopped"),
            }
        }
        bail!("{method}: setup timed out or PTY stopped")
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
        Some("_x.ai/queue/changed" | "x.ai/queue/changed") => {
            Some(Event::Queue(value["params"].clone()))
        }
        Some(method) if value.get("id").is_some() => Some(Event::Interaction(method.to_string())),
        Some(_) => None,
        None if value.get("id").is_some() => Some(Event::Response(value)),
        None => None,
    }
}

fn observe_event(event: Event) {
    match event {
        Event::Queue(params) => {
            let ids = params["entries"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| entry["id"].as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            log_info(
                "native",
                "grok.acp.queue",
                &format!(
                    "session={} running={} pending={ids:?}",
                    params["sessionId"].as_str().unwrap_or(""),
                    params["runningPromptId"].as_str().unwrap_or("")
                ),
            );
        }
        // The leader broadcasts interactions to the TUI as well. Do not race
        // the user's answer with a synthetic approval, cancellation or error.
        Event::Interaction(method) => log_info(
            "native",
            "grok.acp.interaction",
            &format!("TUI owns {method}"),
        ),
        _ => {}
    }
}

struct InFlight {
    request_id: u64,
    session: String,
    ack: DeliveryAck,
}

fn completed_response(value: &Value, id: u64) -> Option<Result<()>> {
    if value["id"].as_u64() != Some(id) {
        return None;
    }
    Some(if value.get("error").is_some() {
        Err(anyhow::anyhow!("prompt failed: {}", value["error"]))
    } else if value["result"]["stopReason"].as_str() == Some("end_turn") {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "prompt did not complete: {}",
            value["result"]["stopReason"]
        ))
    })
}

fn acknowledge(db: &HcomDb, flight: &InFlight) -> Result<()> {
    // Native hooks already publish current activity. Commit only the existing
    // mailbox cursor, monotonically, without overwriting a newer TUI status.
    let changed = db.conn().execute(
        "UPDATE instances SET last_event_id = MAX(last_event_id, ?1) WHERE name = ?2 AND session_id = ?3",
        rusqlite::params![flight.ack.last_event_id, flight.ack.instance_name, flight.session],
    )?;
    if changed != 1 {
        bail!("canonical instance/session changed before delivery acknowledgement");
    }
    Ok(())
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
    let mut halted: Option<String> = None;
    let mut current_status = ST_LISTENING.to_string();
    let mut heartbeat = Instant::now();
    let mut connect_attempts = 0;
    let mut retry_at = Instant::now();
    let mut connect_deadline = Instant::now() + SETUP_TIMEOUT;
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
        if client.is_some() && halted.is_none() {
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
        if let Some(error) = halted.as_deref() {
            let _ = db.set_gate_status(current_name, "acp_unacknowledged", error);
            notify.wait(POLL);
            continue;
        }
        let instance = match db.get_instance_full(current_name) {
            Ok(Some(instance)) => instance,
            Ok(None) => break,
            Err(error) => {
                halted = Some(format!("canonical instance unreadable: {error}"));
                continue;
            }
        };
        let Some(active_session) = instance.session_id.filter(|id| !id.is_empty()) else {
            notify.wait(POLL);
            continue;
        };
        if session != active_session || client.is_none() {
            if in_flight.is_some() {
                halted = Some(
                    "session changed with an unacknowledged ACP prompt; automatic replay stopped"
                        .into(),
                );
                continue;
            }
            if session != active_session {
                connect_attempts = 0;
                retry_at = Instant::now();
                connect_deadline = Instant::now() + SETUP_TIMEOUT;
            }
            if Instant::now() < retry_at {
                notify.wait(POLL);
                continue;
            }
            client.take();
            session = active_session;
            connect_attempts += 1;
            match Client::connect(
                launch,
                &session,
                &instance.directory,
                running,
                connect_deadline,
            ) {
                Ok(connected) => client = Some(connected),
                Err(error) => {
                    if !running.load(Ordering::Acquire) {
                        break;
                    }
                    if connect_attempts < 3 && Instant::now() < connect_deadline {
                        log_warn(
                            "native",
                            "grok.acp.connect_retry",
                            &format!("attempt={connect_attempts}: {error:#}; no prompt submitted"),
                        );
                        retry_at = Instant::now() + Duration::from_secs(1);
                        continue;
                    }
                    halted = Some(format!(
                        "ACP connection failed: {error:#}; mailbox retained"
                    ));
                    if launch_outcome.is_pending() {
                        let detail = halted.as_deref().unwrap_or("ACP setup failed");
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
                            detail,
                        );
                        super::mark_launch_phase_complete(
                            state,
                            launch_outcome,
                            LaunchOutcome::Blocked,
                        );
                    }
                    log_warn(
                        "native",
                        "grok.acp.blocked",
                        halted.as_deref().unwrap_or(""),
                    );
                    continue;
                }
            }
        }
        let Some(client) = client.as_mut() else {
            continue;
        };
        while let Ok(event) = client.events.try_recv() {
            match event {
                Event::Closed(error) => {
                    halted = Some(format!(
                        "{error}; mailbox retained, automatic replay stopped"
                    ));
                    break;
                }
                Event::Response(value) => {
                    if let Some(flight) = in_flight.as_ref()
                        && let Some(result) = completed_response(&value, flight.request_id)
                    {
                        match result.and_then(|()| acknowledge(db, flight)) {
                            Ok(()) => {
                                log_info(
                                    "native",
                                    "grok.acp.ack",
                                    &format!(
                                        "instance={} session={} cursor={}",
                                        flight.ack.instance_name,
                                        flight.session,
                                        flight.ack.last_event_id
                                    ),
                                );
                                in_flight = None;
                            }
                            Err(error) => {
                                halted = Some(format!(
                                    "{error:#}; mailbox retained, automatic replay stopped"
                                ));
                                break;
                            }
                        }
                    }
                }
                event => observe_event(event),
            }
        }
        if let Some(error) = halted.as_deref() {
            log_warn("native", "grok.acp.blocked", error);
            continue;
        }
        if in_flight.is_none()
            && !matches!(current_status.as_str(), "stopped" | "inactive")
            && let Some(prepared) = common::prepare_pending_messages(db, current_name)
        {
            let prompt_id = format!("hcom-{}", uuid::Uuid::new_v4());
            match client.send(
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
                            "instance={current_name} session={session} prompt={prompt_id} cursor={}",
                            prepared.ack.last_event_id
                        ),
                    );
                    in_flight = Some(InFlight {
                        request_id,
                        session: session.clone(),
                        ack: prepared.ack,
                    });
                }
                Err(error) => {
                    halted = Some(format!(
                        "ACP write failed: {error:#}; mailbox retained, automatic replay stopped"
                    ));
                    log_warn(
                        "native",
                        "grok.acp.blocked",
                        halted.as_deref().unwrap_or(""),
                    );
                }
            }
        }
        notify.wait(POLL);
    }
    // Dropping the native stdio client does not close the TUI's session.
    drop(client);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn only_matching_end_turn_acknowledges_delivery() {
        assert!(
            completed_response(&json!({"id": 4, "result": {"stopReason": "end_turn"}}), 4)
                .unwrap()
                .is_ok()
        );
        for result in [
            json!({"id": 4, "result": {"stopReason": "cancelled"}}),
            json!({"id": 4, "result": {"stopReason": "max_tokens"}}),
            json!({"id": 4, "result": {}}),
            json!({"id": 4, "error": {"code": -32000, "message": "failed"}}),
        ] {
            assert!(completed_response(&result, 4).unwrap().is_err());
        }
        assert!(
            completed_response(&json!({"id": 5, "result": {"stopReason": "end_turn"}}), 4)
                .is_none()
        );
    }

    #[test]
    fn queue_and_permission_events_are_not_receipts() {
        let queue = json!({"method": "_x.ai/queue/changed", "params": {"sessionId": "s", "runningPromptId": "p", "entries": []}});
        assert!(matches!(classify_event(queue), Some(Event::Queue(_))));
        let permission =
            json!({"id": "ask-1", "method": "session/request_permission", "params": {}});
        assert!(matches!(
            classify_event(permission),
            Some(Event::Interaction(_))
        ));
        assert!(classify_event(json!({"method": "session/update", "params": {}})).is_none());
    }

    #[test]
    #[serial]
    fn setup_failure_retries_then_reports_blocked_without_a_receipt() {
        let (_tmp, dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let mut db = HcomDb::open().unwrap();
        db.conn().execute(
            "INSERT INTO instances (name, session_id, directory, tool, status, created_at, last_event_id) VALUES ('nova', 'session-1', ?1, 'grok', 'listening', 0, 0)",
            rusqlite::params![dir.to_string_lossy()],
        ).unwrap();
        let launch =
            Launch::new(dir.join("missing-grok-binary").to_str().unwrap(), &[], &[]).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let phase = Arc::new(AtomicBool::new(true));
        let state = DeliveryState {
            screen: Arc::new(RwLock::new(super::super::ScreenState::default())),
            grok_unattended: true,
            grok_acp: Some(launch.clone()),
            launch_phase_active: phase.clone(),
            inject_port: 0,
            user_activity_cooldown_ms: 0,
        };
        let stop_running = running.clone();
        let stop_phase = phase.clone();
        let stopper = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while stop_phase.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            stop_running.store(false, Ordering::Release);
        });
        let mut outcome = LaunchOutcome::Pending;
        let mut name = "nova".to_string();
        let started = Instant::now();
        run(
            &launch,
            &running,
            &mut db,
            &NotifyServer::new().unwrap(),
            &state,
            "",
            &mut name,
            &ToolConfig::for_tool(crate::tool::Tool::Grok),
            &None,
            &None,
            &None,
            &mut super::super::host_label::HostLabel::resolve(),
            &mut outcome,
        );
        stopper.join().unwrap();
        assert_eq!(outcome, LaunchOutcome::Blocked);
        assert!(!phase.load(Ordering::Acquire));
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "setup must receive its bounded retries"
        );
        assert_eq!(db.get_cursor("nova"), 0);
    }

    #[test]
    fn silent_peer_uses_one_deadline_across_setup_requests() {
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt;
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/Q"]).creation_flags(0x08000000);
            command
        };
        #[cfg(not(windows))]
        let mut command = Command::new("cat");
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        // Keep the channel open but deliver no RPC response, as with a hung peer.
        let (_sender, events) = mpsc::channel();
        let mut client = Client {
            child,
            input,
            events,
            next_id: 0,
        };
        let running = AtomicBool::new(true);
        let deadline = Instant::now() + Duration::from_millis(100);
        let error = client
            .request("initialize", json!({}), &running, deadline)
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let sent = client.next_id;
        assert!(
            client
                .request("authenticate", json!({}), &running, deadline)
                .is_err()
        );
        assert_eq!(
            client.next_id, sent,
            "later setup steps must not reset an expired deadline"
        );
    }

    #[test]
    fn launch_preserves_prefix_and_rejects_competing_leader() {
        let launch = Launch::new("grok", &["prefix".into()], &["--resume", "session"]).unwrap();
        assert_eq!(launch.prefix, ["prefix"]);
        assert_eq!(launch.tui_args()[0], "--leader");
        for flag in [
            "--leader",
            "--no-leader",
            "--leader-socket",
            "--leader-socket=other",
        ] {
            assert!(Launch::new("grok", &[], &[flag]).is_err());
        }
        for args in [
            vec!["--allow", "Bash"],
            vec!["--deny=bash"],
            vec!["--allowedTools", "Read(*)"],
            vec!["--disallowedTools", "Bash(*)"],
            vec!["--disable-web-search"],
        ] {
            assert!(Launch::validate_args(&args).is_ok());
            assert!(!Launch::policy_args(&args).unwrap().is_empty());
        }
        assert!(Launch::policy_args(&["--no-subagents"]).unwrap().is_empty());
        let restricted = Launch::new("grok", &[], &["--no-subagents"]).unwrap();
        assert!(
            restricted
                .child_env()
                .contains(&("GROK_SUBAGENTS".into(), "0".into()))
        );
    }

    #[test]
    fn policy_probe_is_required_only_for_inherited_cli_restrictions() {
        let missing = || Command::new("hcom-test-missing-grok-policy-binary");
        assert!(Launch::check_policy_support(missing(), &["--no-subagents"]).is_ok());
        assert!(Launch::check_policy_support(missing(), &["--deny", "Bash"]).is_err());
    }

    #[test]
    fn policy_projection_preserves_values_and_stops_at_prompt_marker() {
        let args = [
            "--resume",
            "session",
            "--allow",
            "Bash(Write-Output *)",
            "--deny=Read(secret*)",
            "--disable-web-search",
            "--",
            "--deny",
            "--leader",
            "--no-subagents",
        ];
        assert_eq!(
            Launch::policy_args(&args).unwrap(),
            [
                "--allow",
                "Bash(Write-Output *)",
                "--deny=Read(secret*)",
                "--disable-web-search"
            ]
        );
        assert!(Launch::validate_args(&args).is_ok());
        let literal = Launch::new(
            "missing-grok-is-not-run",
            &[],
            &["--", "--deny", "--leader", "--no-subagents"],
        )
        .unwrap();
        assert!(literal.policy_args.is_empty());
        assert!(!literal.no_subagents);
    }

    #[test]
    #[serial]
    fn receipt_is_monotonic_session_scoped_and_preserves_hook_status() {
        let (_tmp, _dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn().execute(
            "INSERT INTO instances (name, session_id, tool, status, status_context, last_event_id, created_at) VALUES ('nova', 'session-1', 'grok', 'active', 'new-human-prompt', 9, 0)",
            [],
        ).unwrap();
        let mut flight = InFlight {
            request_id: 4,
            session: "session-1".into(),
            ack: DeliveryAck {
                instance_name: "nova".into(),
                last_event_id: 7,
                status_context: "deliver:sender".into(),
                msg_ts: String::new(),
                mark_announced: false,
            },
        };
        acknowledge(&db, &flight).unwrap();
        assert_eq!(db.get_cursor("nova"), 9);
        assert_eq!(
            db.get_status("nova").unwrap().unwrap(),
            ("active".into(), "new-human-prompt".into())
        );
        flight.ack.last_event_id = 12;
        flight.session = "old-session".into();
        assert!(acknowledge(&db, &flight).is_err());
        assert_eq!(db.get_cursor("nova"), 9);
        flight.session = "session-1".into();
        acknowledge(&db, &flight).unwrap();
        assert_eq!(db.get_cursor("nova"), 12);
    }
}
