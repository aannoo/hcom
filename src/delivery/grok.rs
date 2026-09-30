//! Grok: session binding, status and message delivery, all over Grok's own ACP.
//! hcom installs no Grok hooks.
//!
//! `hcom grok` runs the TUI against a private leader socket. This thread
//! attaches a second client to that leader (`grok agent --leader
//! --leader-socket <sock> stdio`, the official stdio bridge, which handles
//! leader framing and reconnects). On this leader only the TUI opens
//! sessions, so:
//!
//! - **Binding.** Every visible resident session in the roster
//!   (`x.ai/sessions/list`) belongs to the TUI. hcom loads each with
//!   `noReplay` (live events, no history) and binds the instance to the TUI's
//!   current one: the first to appear, then any that newly appears idle
//!   (`/new`, `/resume` from disk). Switching the TUI to a session that is
//!   already open emits nothing, so hcom follows that switch when a user
//!   prompt starts running there.
//! - **Status.** The leader broadcasts each session's queue, tool calls,
//!   pending approvals and turn ends to every attached client.
//! - **Delivery.** Each mailbox batch is queued as an ordinary prompt with
//!   `sendNow:false`. Nothing is typed into the TUI composer, so the user's
//!   draft is never touched, and a busy session runs the batch after its
//!   current work.
//!
//! Grok uses our `promptId` as the queue entry id. The batch is acknowledged
//! when Grok reports that entry running (`x.ai/queue/changed`), i.e. once it
//! is part of the model's turn, or when the prompt request returns a result.
//! A batch that never started (removed from the queue, transport lost) stays
//! unread and is queued again later: at-least-once, never silently dropped.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::db::HcomDb;
use crate::hooks::{DeliveryAck, common};
use crate::instance_lifecycle as lifecycle;
use crate::notify::NotifyServer;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_LISTENING};

use super::{DeliveryState, LaunchOutcome, TitleWake, ToolConfig, log_info, log_warn};

const POLL: Duration = Duration::from_millis(100);
const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Fast roster poll interval: until the TUI has a session (e.g. its
/// `--resume` picker is still open), and for a while after a hint that it
/// opened one.
const ROSTER_FAST_POLL: Duration = Duration::from_millis(500);
/// The hints (setup broadcasts) can precede the new session's roster entry.
const ROSTER_FAST_WINDOW: Duration = Duration::from_secs(5);
/// Backstop for a switch whose hint was missed.
const ROSTER_SLOW_POLL: Duration = Duration::from_secs(15);
/// Wait before re-queueing a batch whose prompt was dropped before it ran
/// (user removed it from the queue, prompt error, transport lost).
const REQUEUE_DELAY: Duration = Duration::from_secs(10);
/// Wait before loading the bound session again after a failed load.
const LOAD_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Prompts hcom queues carry this `promptId` prefix.
const HCOM_PROMPT_ID_PREFIX: &str = "hcom-";
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

/// Grok gets nothing injected per run: everything hcom needs arrives over the
/// ACP client above. Registering as per-run keeps it out of the global
/// hook-install paths (`hcom hooks`, status, launch).
pub(crate) static PER_RUN: crate::hooks::runtime::PerRunAdapter =
    crate::hooks::runtime::PerRunAdapter {
        prepare: |ctx| {
            Ok(crate::hooks::runtime::RuntimeInjection {
                args: ctx.args.clone(),
                env: Vec::new(),
            })
        },
        cleanup_legacy: |_| Ok(()),
        ensure_permissions: None,
        managed_value_flags: &[],
        strip_legacy_args: None,
    };

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
    /// PID to the socket's sibling `.lock` (`x.sock` -> `x.lock`).
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
    /// `method` without the ext `_` prefix, `params` unwrapped.
    Notification {
        method: String,
        params: Value,
    },
    /// A request from Grok. Shared interactions (permission, question, plan
    /// approval) reach every client, first answer wins; the TUI answers them
    /// unless hcom auto-approves.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
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
        log_info("native", "grok.acp.connected", "");
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

    /// Answer a request from Grok.
    fn respond(&mut self, id: Value, result: Value) -> Result<()> {
        serde_json::to_writer(
            &mut self.input,
            &json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        Ok(())
    }

    /// Blocking request; only for the handshake, before anything else is in
    /// flight (other events are discarded while waiting).
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
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return value.get("id").is_some().then_some(Event::Response(value));
    };
    let mut params = value.get("params").cloned().unwrap_or(Value::Null);
    let method = match method.strip_prefix('_') {
        // Gateway-wrapped ext: `{"method":"_x.ai/foo","params":{"method":"x.ai/foo","params":{…}}}`.
        Some(bare) => match params.get("method").and_then(Value::as_str) {
            Some(inner) => {
                let inner = inner.to_string();
                params = params.get("params").cloned().unwrap_or(Value::Null);
                inner
            }
            None => bare.to_string(),
        },
        None => method.to_string(),
    };
    Some(match value.get("id") {
        Some(id) => Event::Request {
            id: id.clone(),
            method,
            params,
        },
        None => Event::Notification { method, params },
    })
}

/// The `session/request_permission` option id of the given kind.
fn permission_option(params: &Value, kind: &str) -> Option<String> {
    params["options"]
        .as_array()?
        .iter()
        .find(|option| option["kind"] == kind)
        .and_then(|option| str_of(option, "optionId"))
        .map(str::to_string)
}

/// The shell command a permission request is for, if it is one.
fn permission_command(params: &Value) -> Option<&str> {
    let call = &params["toolCall"];
    let input = &call["rawInput"];
    // POSIX shell only: `is_safe_hcom_command` parses POSIX quoting, and
    // PowerShell reads `\;` as a separator, so it never auto-approves.
    let posix = match input["variant"].as_str() {
        Some(variant) => variant == "Bash",
        None => call["kind"] == "execute" && !cfg!(windows),
    };
    posix.then(|| str_of(input, "command")).flatten()
}

// ── Session tracking ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Prompt,
    Tool { name: String, input: Value },
    Blocked(String),
    Listening(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Effect {
    /// The TUI is now on this session.
    Bind {
        session: String,
        cwd: String,
    },
    /// Subscribe to this session's live events.
    Load {
        session: String,
        cwd: String,
    },
    Status(Status),
    RefreshRoster,
}

#[derive(Debug)]
struct RosterEntry {
    cwd: String,
    working: bool,
    last_change: i64,
}

/// Which session the TUI is on and what it is doing, from the leader's
/// broadcasts. Pure: the run loop applies the effects.
#[derive(Default)]
struct Tracker {
    bound: Option<String>,
    /// Visible resident sessions (id → cwd) at the last roster; `None` before
    /// the first roster of a connection.
    resident: Option<HashMap<String, String>>,
    /// Subagent session → the session that spawned it. Subagents never bind;
    /// only the bound session's own subagents speak for it.
    children: HashMap<String, String>,
    /// Each session's running queue entry, to see turn starts once.
    running: HashMap<String, String>,
    /// For restoring the tool status once an approval is answered.
    last_tool: Option<(String, Value)>,
    /// Open approvals/questions (by tool call id) in the bound session and
    /// its subagents; the turn stays blocked until all are answered.
    interactions: HashSet<String>,
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn turn_end_context(update: &Value) -> String {
    match str_of(update, "stop_reason").unwrap_or("end_turn") {
        "end_turn" => String::new(),
        "cancelled" => "cancelled".into(),
        "error" => format!(
            "failure:{}",
            str_of(update, "error_kind").unwrap_or("error")
        ),
        // rate_limit, max_tokens, refusal, ...
        other => format!("failure:{other}"),
    }
}

impl Tracker {
    fn is_bound(&self, session: &str) -> bool {
        self.bound.as_deref() == Some(session)
    }

    fn bind(&mut self, session: &str, cwd: &str) -> Effect {
        self.bound = Some(session.to_string());
        self.last_tool = None;
        self.interactions.clear();
        Effect::Bind {
            session: session.to_string(),
            cwd: cwd.to_string(),
        }
    }

    /// Reset per-connection state after (re)connecting.
    fn reconnected(&mut self) {
        self.resident = None;
        self.running.clear();
        self.interactions.clear();
    }

    fn on_roster(&mut self, sessions: &[Value]) -> Vec<Effect> {
        let visible: HashMap<String, RosterEntry> = sessions
            .iter()
            .filter(|entry| entry["resident"] == true)
            .filter_map(|entry| {
                let id = str_of(entry, "sessionId")?;
                (!self.children.contains_key(id)).then(|| {
                    (
                        id.to_string(),
                        RosterEntry {
                            cwd: str_of(entry, "cwd").unwrap_or_default().to_string(),
                            working: entry["activity"] == "working",
                            last_change: entry["lastChangeUnixMs"].as_i64().unwrap_or(0),
                        },
                    )
                })
            })
            .collect();
        let previous = self.resident.replace(
            visible
                .iter()
                .map(|(id, entry)| (id.clone(), entry.cwd.clone()))
                .collect(),
        );
        let is_new = |id: &str| previous.as_ref().is_none_or(|prev| !prev.contains_key(id));
        let mut effects: Vec<Effect> = visible
            .iter()
            .filter(|(id, _)| is_new(id))
            .map(|(id, entry)| Effect::Load {
                session: id.clone(),
                cwd: entry.cwd.clone(),
            })
            .collect();
        let newest = |candidates: &mut dyn Iterator<Item = (&String, &RosterEntry)>| {
            candidates
                .max_by_key(|(_, entry)| entry.last_change)
                .map(|(id, entry)| (id.clone(), entry.cwd.clone()))
        };
        let target = if self.bound.is_none() {
            newest(&mut visible.iter())
        } else if previous.is_some() {
            // Newly resident and idle: the TUI just created or resumed it. A
            // new session that is already working is followed at its first
            // user prompt instead.
            newest(
                &mut visible
                    .iter()
                    .filter(|(id, entry)| is_new(id) && !entry.working),
            )
        } else {
            None
        };
        if let Some((session, cwd)) = target.filter(|(id, _)| !self.is_bound(id)) {
            effects.push(self.bind(&session, &cwd));
        }
        effects
    }

    /// A shared interaction Grok is asking a human about. Only these block:
    /// approvals Grok decides itself never reach the prompter.
    fn on_request(&mut self, method: &str, params: &Value) -> Vec<Effect> {
        let what = match method {
            "session/request_permission" => "approval",
            "x.ai/ask_user_question" => "question",
            "x.ai/exit_plan_mode" => "plan_approval",
            "x.ai/mcp/elicit" => "mcp_input",
            _ => return Vec::new(),
        };
        let session = str_of(params, "sessionId").unwrap_or_default();
        // A subagent waiting on a human blocks its parent's turn too.
        if !self.speaks_for_bound(session) {
            return Vec::new();
        }
        let call = params
            .pointer("/toolCall/toolCallId")
            .and_then(Value::as_str)
            .or_else(|| str_of(params, "toolCallId"))
            .unwrap_or(method);
        self.interactions.insert(call.to_string());
        vec![Effect::Status(Status::Blocked(what.to_string()))]
    }

    /// Whether a permission request belongs to the bound session or its
    /// subagents, i.e. is hcom's to auto-approve.
    fn owns(&self, params: &Value) -> bool {
        self.speaks_for_bound(str_of(params, "sessionId").unwrap_or_default())
    }

    /// The bound session itself, or a subagent (at any depth) it spawned. A
    /// subagent of a session the TUI switched away from is not.
    fn speaks_for_bound(&self, session: &str) -> bool {
        let mut current = session;
        // Bounded walk: spawn records cannot form a cycle, but never trust that.
        for _ in 0..16 {
            if self.is_bound(current) {
                return true;
            }
            match self.children.get(current) {
                Some(parent) => current = parent,
                None => return false,
            }
        }
        false
    }

    fn on_notification(&mut self, method: &str, params: &Value) -> Vec<Effect> {
        let session = str_of(params, "sessionId").unwrap_or_default();
        match method {
            "x.ai/queue/changed" => {
                let Some(prompt) = str_of(params, "runningPromptId") else {
                    self.running.remove(session);
                    return Vec::new();
                };
                if self.running.get(session).map(String::as_str) == Some(prompt) {
                    return Vec::new();
                }
                self.running.insert(session.to_string(), prompt.to_string());
                // hcom's own batches set their status on ack.
                let user_prompt = !prompt.starts_with(HCOM_PROMPT_ID_PREFIX)
                    && str_of(params, "runningKind").is_none_or(|kind| kind == "prompt");
                if !user_prompt {
                    return Vec::new();
                }
                if self.is_bound(session) {
                    return vec![Effect::Status(Status::Prompt)];
                }
                // Only the TUI's user prompts here: it switched to this session.
                let cwd = self
                    .resident
                    .as_ref()
                    .and_then(|resident| resident.get(session))
                    .cloned();
                match cwd {
                    Some(cwd) if !self.children.contains_key(session) => {
                        vec![self.bind(session, &cwd), Effect::Status(Status::Prompt)]
                    }
                    _ => Vec::new(),
                }
            }
            "session/update" if self.is_bound(session) => {
                let update = &params["update"];
                if update["sessionUpdate"] != "tool_call" {
                    return Vec::new();
                }
                let name = update
                    .pointer("/_meta/x.ai~1tool/name")
                    .and_then(Value::as_str)
                    .or_else(|| str_of(update, "title"))
                    .unwrap_or("tool")
                    .to_string();
                let input = update.get("rawInput").cloned().unwrap_or(Value::Null);
                self.last_tool = Some((name.clone(), input.clone()));
                vec![Effect::Status(Status::Tool { name, input })]
            }
            "x.ai/session_notification" => {
                let update = &params["update"];
                let kind = update["sessionUpdate"].as_str().unwrap_or_default();
                if kind == "subagent_spawned" {
                    if let Some(child) = str_of(update, "child_session_id") {
                        self.children.insert(child.to_string(), session.to_string());
                    }
                    return Vec::new();
                }
                match kind {
                    // Also announced for approvals Grok decides itself (rules,
                    // auto mode), so only the request counts (`on_request`).
                    "interaction_resolved" => {
                        let removed = self
                            .interactions
                            .remove(str_of(update, "tool_call_id").unwrap_or_default());
                        if !removed || !self.interactions.is_empty() {
                            return Vec::new();
                        }
                        vec![Effect::Status(match self.last_tool.clone() {
                            Some((name, input)) => Status::Tool { name, input },
                            None => Status::Prompt,
                        })]
                    }
                    "turn_completed" if self.is_bound(session) => {
                        // A queued prompt may already be running.
                        let ended = str_of(update, "prompt_id");
                        match self.running.get(session) {
                            Some(running) if Some(running.as_str()) != ended => Vec::new(),
                            _ => vec![Effect::Status(Status::Listening(turn_end_context(update)))],
                        }
                    }
                    _ => Vec::new(),
                }
            }
            "x.ai/sessions/changed" => {
                if let Some(resident) = self.resident.as_mut() {
                    for removed in params["removed"].as_array().into_iter().flatten() {
                        if let Some(id) = removed.as_str() {
                            resident.remove(id);
                        }
                    }
                }
                let unknown = params["upserted"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| str_of(entry, "sessionId"))
                    .any(|id| {
                        !self.children.contains_key(id)
                            && self
                                .resident
                                .as_ref()
                                .is_none_or(|resident| !resident.contains_key(id))
                    });
                if unknown {
                    vec![Effect::RefreshRoster]
                } else {
                    Vec::new()
                }
            }
            // Broadcast when a session is set up, without saying which.
            "x.ai/models/update" | "x.ai/mcp/servers_updated" | "x.ai/announcements/update" => {
                vec![Effect::RefreshRoster]
            }
            _ => Vec::new(),
        }
    }
}

// ── Delivery acknowledgement ────────────────────────────────────────────

/// A queued batch awaiting evidence that Grok ran it.
struct InFlight {
    /// The `session/prompt` request on the current connection; `None` once
    /// that connection is gone (ids restart on the next one).
    request_id: Option<u64>,
    session: String,
    prompt_id: String,
    /// The exact prompt text. It carries the batch's message ids, so it
    /// identifies this batch inside a combined turn.
    text: String,
    ack: DeliveryAck,
    /// The session's `updates.jsonl`, checked before re-queueing.
    transcript: std::path::PathBuf,
    /// When Grok answered `removedFromQueue`: either removed, or merged into
    /// the prompt ahead of it (which then runs its text). Only a combined-turn
    /// snapshot tells them apart, and it may arrive after the response.
    removed_at: Option<Instant>,
    /// When the ACP connection that queued it dropped. Grok keeps a queued
    /// prompt when its client disconnects, so the batch is followed through
    /// the reconnected client's queue snapshots, not treated as dropped.
    detached_at: Option<Instant>,
}

/// How long to wait after `removedFromQueue` for the snapshot showing a merge.
const MERGE_EVIDENCE_WAIT: Duration = Duration::from_secs(2);
/// How long a batch whose connection dropped waits for a queue snapshot
/// before the transcript decides.
const DETACHED_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Part of a turn: acknowledge.
    Delivered,
    /// Dropped before it ran: leave unread and queue again later.
    Dropped(String),
}

/// What a queue snapshot proves about our prompt. Only identity counts: our
/// prompt id running, or our exact text among the running combined texts
/// (Grok merges queued plain prompts into the front one).
fn queue_outcome(params: &Value, flight: &InFlight) -> Option<Outcome> {
    let running = params["runningPromptId"].as_str() == Some(flight.prompt_id.as_str());
    let merged = params["runningCombinedTexts"]
        .as_array()
        .is_some_and(|texts| texts.iter().any(|t| t.as_str() == Some(&flight.text)));
    if running || merged {
        return Some(Outcome::Delivered);
    }
    // After a reconnect nothing else will report it: still listed means
    // still queued; gone means it ran (the transcript will show it) or was
    // removed.
    if flight.detached_at.is_some() {
        let queued = params["entries"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|e| e["id"] == flight.prompt_id));
        return (!queued).then(|| Outcome::Dropped("gone from Grok's queue".into()));
    }
    // Absence proves nothing, even after `removedFromQueue`: the merge
    // snapshot can come later. `MERGE_EVIDENCE_WAIT` decides that case.
    None
}

/// Whether the session transcript shows the prompt text as sent: durable
/// proof it ran when the live evidence was lost (bridge disconnect, a merge
/// snapshot that never arrived).
fn ran_in_transcript(path: &std::path::Path, text: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    // Only the live branch: a rewound batch is gone from the agent's context.
    crate::transcript::grok::live_lines(&content)
        .into_iter()
        .rev()
        .any(|line| {
            line.contains("user_message_chunk")
                && serde_json::from_str::<Value>(line).is_ok_and(|value| {
                    let update = &value["params"]["update"];
                    update["sessionUpdate"] == "user_message_chunk"
                        && update["content"]["text"]
                            .as_str()
                            .is_some_and(|sent| sent.contains(text))
                })
        })
}

/// What the `session/prompt` response proves. `None`: removed from the queue,
/// pending evidence of a merge.
fn response_outcome(value: &Value) -> Option<Outcome> {
    if let Some(error) = value.get("error") {
        return Some(Outcome::Dropped(format!("prompt failed: {error}")));
    }
    if value["result"]["_meta"]["completionKind"] == "removedFromQueue" {
        return None;
    }
    // Any other result, `cancelled` included, ends a turn that ran it.
    Some(Outcome::Delivered)
}

// ── Run loop ────────────────────────────────────────────────────────────

enum Pending {
    Roster,
    Load(String),
}

/// The roster result, which the stdio bridge may wrap once more.
fn roster_sessions(result: &Value) -> &[Value] {
    result
        .get("sessions")
        .or_else(|| result.pointer("/result/sessions"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Point the instance at `session`, as a hook-based SessionStart would.
/// Returns the instance name, which a switch to a session bound to another
/// identity changes.
fn apply_bind(
    db: &HcomDb,
    process_id: &str,
    current_name: &str,
    session: &str,
    cwd: &str,
) -> String {
    let first = db
        .get_instance_full(current_name)
        .ok()
        .flatten()
        .is_none_or(|instance| instance.session_id.is_none());
    // Resolves resume placeholders and identity switches; may rename.
    let name = crate::instance_binding::bind_session_to_process(db, session, Some(process_id))
        .unwrap_or_else(|| current_name.to_string());
    let _ = db.rebind_instance_session(&name, session);
    let mut updates = serde_json::Map::new();
    if !cwd.is_empty() {
        updates.insert(
            "transcript_path".into(),
            Value::String(
                crate::transcript::grok::session_transcript_path(cwd, session)
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
        updates.insert("directory".into(), Value::String(cwd.to_string()));
    }
    crate::instances::update_instance_position(db, &name, &updates);
    if first {
        crate::instance_binding::capture_and_store_launch_context(db, &name);
        lifecycle::set_status(db, &name, ST_LISTENING, "start", Default::default());
        crate::relay::worker::ensure_worker(true);
    }
    log_info(
        "native",
        "grok.acp.bound",
        &format!("instance={name} session={session}"),
    );
    name
}

/// Report a pending launch as blocked. `drive_launch_outcome` clears it to
/// ready once the cause goes away.
fn block_launch(
    db: &HcomDb,
    state: &DeliveryState,
    name: &str,
    outcome: &mut LaunchOutcome,
    reason: &str,
    detail: &str,
) {
    if !outcome.is_pending() {
        return;
    }
    let _ = db.set_status(name, ST_BLOCKED, "launch_blocked");
    let _ = db.emit_launch_blocked_event(name, ST_BLOCKED, "launch_blocked", reason, detail);
    super::mark_launch_phase_complete(state, outcome, LaunchOutcome::Blocked);
}

fn apply_status(db: &HcomDb, name: &str, status: &Status) {
    match status {
        Status::Prompt => lifecycle::set_status(db, name, ST_ACTIVE, "prompt", Default::default()),
        Status::Tool { name: tool, input } => {
            common::update_tool_status(db, name, "grok", tool, input)
        }
        Status::Blocked(context) => {
            lifecycle::set_status(db, name, ST_BLOCKED, context, Default::default())
        }
        Status::Listening(context) => {
            lifecycle::set_status(db, name, ST_LISTENING, context, Default::default())
        }
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
    let mut tracker = Tracker::default();
    let mut loaded: HashSet<String> = HashSet::new();
    let mut pending: HashMap<u64, Pending> = HashMap::new();
    let mut roster_due: Option<Instant> = None;
    let mut roster_fast_until = Instant::now();
    let mut in_flight: Option<InFlight> = None;
    let mut load_retry_at = Instant::now();
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
        // Ready once the bound session is loaded: until then it is not
        // subscribed and nothing can be delivered.
        if client.is_some() && tracker.bound.as_ref().is_some_and(|s| loaded.contains(s)) {
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
        if client.is_none() {
            if Instant::now() < retry_at {
                notify.wait(POLL);
                continue;
            }
            match Client::connect(
                launch,
                &instance.directory,
                running,
                Instant::now() + SETUP_TIMEOUT,
            ) {
                Ok(connected) => {
                    client = Some(connected);
                    connect_failures = 0;
                    tracker.reconnected();
                    loaded.clear();
                    pending.clear();
                    roster_due = Some(Instant::now());
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
                    if launched_at.elapsed() >= SETUP_TIMEOUT {
                        block_launch(
                            db,
                            state,
                            current_name,
                            launch_outcome,
                            "grok_acp_connection",
                            &detail,
                        );
                    }
                    continue;
                }
            }
        }
        let Some(conn) = client.as_mut() else {
            continue;
        };

        let mut effects = Vec::new();
        let mut outcome = None;
        let mut closed = None;
        while let Ok(event) = conn.events.try_recv() {
            match event {
                Event::Closed(error) => {
                    closed = Some(error);
                    break;
                }
                Event::Response(value) => {
                    let id = value["id"].as_u64();
                    match id.and_then(|id| pending.remove(&id)) {
                        Some(Pending::Roster) => match value.get("error") {
                            Some(error) => {
                                log_warn("native", "grok.acp.roster_failed", &error.to_string())
                            }
                            None => {
                                effects.extend(tracker.on_roster(roster_sessions(&value["result"])))
                            }
                        },
                        Some(Pending::Load(session)) => match value.get("error") {
                            Some(error) => {
                                load_retry_at = Instant::now() + LOAD_RETRY_DELAY;
                                let detail = format!("Grok session/load failed: {error}");
                                log_warn(
                                    "native",
                                    "grok.acp.load_failed",
                                    &format!("session={session}: {detail}"),
                                );
                                // Readiness waits for the bound session to load,
                                // so one that never loads would stay pending.
                                if tracker.bound.as_deref() == Some(session.as_str())
                                    && launched_at.elapsed() >= SETUP_TIMEOUT
                                {
                                    block_launch(
                                        db,
                                        state,
                                        current_name,
                                        launch_outcome,
                                        "grok_session_load",
                                        &detail,
                                    );
                                }
                            }
                            None => {
                                loaded.insert(session);
                            }
                        },
                        None => {
                            if let Some(flight) = in_flight.as_mut()
                                && id.is_some()
                                && id == flight.request_id
                            {
                                outcome = response_outcome(&value);
                                if outcome.is_none() {
                                    flight.removed_at = Some(Instant::now());
                                }
                            }
                        }
                    }
                }
                Event::Notification { method, params } => {
                    if method == "x.ai/queue/changed"
                        && let Some(flight) = in_flight.as_ref()
                        && params["sessionId"].as_str() == Some(flight.session.as_str())
                    {
                        outcome = queue_outcome(&params, flight);
                    }
                    effects.extend(tracker.on_notification(&method, &params));
                }
                Event::Request { id, method, params } => {
                    let approve = (method == "session/request_permission"
                        && tracker.owns(&params)
                        && permission_command(&params).is_some_and(common::is_safe_hcom_command)
                        && crate::config::load_config_snapshot().core.auto_approve)
                        .then(|| permission_option(&params, "allow_once"))
                        .flatten();
                    match approve {
                        Some(option) => {
                            let answer =
                                json!({"outcome": {"outcome": "selected", "optionId": option}});
                            if let Err(error) = conn.respond(id, answer) {
                                closed = Some(format!("write failed: {error:#}"));
                                break;
                            }
                            log_info(
                                "native",
                                "grok.acp.auto_approved",
                                permission_command(&params).unwrap_or_default(),
                            );
                        }
                        None => effects.extend(tracker.on_request(&method, &params)),
                    }
                }
            }
            if outcome.is_some() {
                break;
            }
        }
        for effect in effects {
            match effect {
                Effect::Bind { session, cwd } => {
                    let cwd = if cwd.is_empty() {
                        instance.directory.clone()
                    } else {
                        cwd
                    };
                    *current_name = apply_bind(db, process_id, current_name, &session, &cwd);
                }
                Effect::Load { session, cwd } => {
                    if loaded.contains(&session)
                        || pending
                            .values()
                            .any(|p| matches!(p, Pending::Load(s) if *s == session))
                    {
                        continue;
                    }
                    let cwd = if cwd.is_empty() {
                        instance.directory.clone()
                    } else {
                        cwd
                    };
                    match conn.send(
                        "session/load",
                        json!({"sessionId": session, "cwd": cwd, "mcpServers": [],
                               "_meta": {"noReplay": true}}),
                    ) {
                        Ok(id) => {
                            pending.insert(id, Pending::Load(session));
                        }
                        Err(error) => closed = Some(format!("write failed: {error:#}")),
                    }
                }
                Effect::Status(status) => apply_status(db, current_name, &status),
                Effect::RefreshRoster => {
                    roster_fast_until = Instant::now() + ROSTER_FAST_WINDOW;
                    let at = Instant::now() + ROSTER_FAST_POLL;
                    roster_due = Some(roster_due.map_or(at, |due| due.min(at)));
                }
            }
        }
        let roster_pending = pending.values().any(|p| matches!(p, Pending::Roster));
        if !roster_pending && roster_due.is_none() {
            let fast = tracker.bound.is_none() || Instant::now() < roster_fast_until;
            let interval = if fast {
                ROSTER_FAST_POLL
            } else {
                ROSTER_SLOW_POLL
            };
            roster_due = Some(Instant::now() + interval);
        }
        if !roster_pending && roster_due.is_some_and(|due| Instant::now() >= due) {
            roster_due = None;
            match conn.send("_x.ai/sessions/list", json!({})) {
                Ok(id) => {
                    pending.insert(id, Pending::Roster);
                }
                Err(error) => closed = Some(format!("write failed: {error:#}")),
            }
        }
        if let Some(error) = closed {
            client = None;
            retry_at = Instant::now() + Duration::from_secs(1);
            if let Some(flight) = in_flight.as_mut() {
                flight.request_id = None;
                flight.detached_at.get_or_insert_with(Instant::now);
            }
            log_warn("native", "grok.acp.disconnected", &error);
        }
        if outcome.is_none()
            && in_flight
                .as_ref()
                .and_then(|flight| flight.removed_at)
                .is_some_and(|at| at.elapsed() >= MERGE_EVIDENCE_WAIT)
        {
            outcome = Some(Outcome::Dropped("removed from Grok's queue".into()));
        }
        if outcome.is_none()
            && in_flight
                .as_ref()
                .and_then(|flight| flight.detached_at)
                .is_some_and(|at| at.elapsed() >= DETACHED_WAIT)
        {
            outcome = Some(Outcome::Dropped(
                "no queue evidence after reconnecting".into(),
            ));
        }
        if let Some(outcome) = outcome
            && let Some(flight) = in_flight.take()
        {
            let outcome = match outcome {
                Outcome::Dropped(_) if ran_in_transcript(&flight.transcript, &flight.text) => {
                    Outcome::Delivered
                }
                other => other,
            };
            match outcome {
                Outcome::Delivered if flight.ack.instance_name != *current_name => {
                    // Queued under an identity a session switch retired: its
                    // messages were delivered, but status belongs to the
                    // current identity, so don't revive the old row.
                    if let Err(error) = db.ack_hook_delivery(
                        &flight.ack.instance_name,
                        flight.ack.last_event_id,
                        flight.ack.mark_announced,
                    ) {
                        log_warn("native", "grok.acp.ack_failed", &format!("{error}"));
                    }
                }
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
        // The bound session must be loaded, or its queue events never arrive.
        // Other sessions only matter once they are bound, so only this one
        // is retried.
        if let Some(session) = tracker.bound.clone()
            && !loaded.contains(&session)
            && !pending
                .values()
                .any(|p| matches!(p, Pending::Load(s) if *s == session))
            && Instant::now() >= load_retry_at
        {
            let cwd = tracker
                .resident
                .as_ref()
                .and_then(|resident| resident.get(&session))
                .filter(|cwd| !cwd.is_empty())
                .cloned()
                .unwrap_or_else(|| instance.directory.clone());
            load_retry_at = Instant::now() + LOAD_RETRY_DELAY;
            match conn.send(
                "session/load",
                json!({"sessionId": session, "cwd": cwd, "mcpServers": [],
                       "_meta": {"noReplay": true}}),
            ) {
                Ok(id) => {
                    pending.insert(id, Pending::Load(session));
                }
                Err(error) => {
                    log_warn("native", "grok.acp.write_failed", &format!("{error:#}"));
                    client = None;
                    retry_at = Instant::now() + Duration::from_secs(1);
                    notify.wait(POLL);
                    continue;
                }
            }
        }
        let target = tracker.bound.clone().filter(|s| loaded.contains(s));
        if let Some(session) = target
            && in_flight.is_none()
            && Instant::now() >= requeue_at
            && !matches!(current_status.as_str(), "stopped" | "inactive")
            && let Some(prepared) = common::prepare_pending_messages(db, current_name)
        {
            let prompt_id = format!("{HCOM_PROMPT_ID_PREFIX}{}", uuid::Uuid::new_v4());
            match conn.send(
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{"type": "text", "text": &prepared.formatted}],
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
                    let cwd = tracker
                        .resident
                        .as_ref()
                        .and_then(|resident| resident.get(&session))
                        .filter(|cwd| !cwd.is_empty())
                        .cloned()
                        .unwrap_or_else(|| instance.directory.clone());
                    in_flight = Some(InFlight {
                        request_id: Some(request_id),
                        transcript: crate::transcript::grok::session_transcript_path(
                            &cwd, &session,
                        ),
                        session,
                        prompt_id,
                        text: prepared.formatted,
                        ack: prepared.ack,
                        removed_at: None,
                        detached_at: None,
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
    // Dropping the ACP client does not close the TUI's sessions.
    drop(client);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BATCH: &str = "<hcom>[request #7] luna → nova: hi</hcom>";

    fn flight(removed: bool) -> InFlight {
        InFlight {
            request_id: Some(4),
            session: "s".into(),
            prompt_id: "hcom-1".into(),
            text: BATCH.into(),
            transcript: std::path::PathBuf::new(),
            ack: DeliveryAck {
                instance_name: "nova".into(),
                last_event_id: 7,
                status_context: "deliver:luna".into(),
                msg_ts: String::new(),
                mark_announced: false,
            },
            removed_at: removed.then(Instant::now),
            detached_at: None,
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
    fn absence_alone_proves_nothing() {
        for params in [
            json!({"sessionId": "s", "runningPromptId": "user-1", "entries": [{"id": "hcom-1"}]}),
            json!({"sessionId": "s", "runningPromptId": "user-1", "entries": []}),
            // Someone else's combined turn is running.
            json!({"sessionId": "s", "runningPromptId": "user-1",
                   "runningCombinedTexts": ["fix it", "and test it"], "entries": []}),
        ] {
            assert_eq!(queue_outcome(&params, &flight(false)), None, "{params}");
        }
    }

    #[test]
    fn removed_prompt_is_delivered_only_if_merged_into_the_running_turn() {
        let unrelated = json!({"sessionId": "s", "runningPromptId": "user-1",
                               "runningCombinedTexts": ["fix it", "and test it"], "entries": []});
        // Not yet: a later snapshot may still show the merge.
        assert_eq!(queue_outcome(&unrelated, &flight(true)), None);
        let merged = json!({"sessionId": "s", "runningPromptId": "user-1",
                            "runningCombinedTexts": ["fix it", BATCH], "entries": []});
        // The merge snapshot may arrive before or after the removal response.
        assert_eq!(
            queue_outcome(&merged, &flight(false)),
            Some(Outcome::Delivered)
        );
        assert_eq!(
            queue_outcome(&merged, &flight(true)),
            Some(Outcome::Delivered)
        );
    }

    #[test]
    fn detached_batch_follows_the_reconnected_queue() {
        let mut detached = flight(false);
        detached.detached_at = Some(Instant::now());
        let still_queued = json!({"sessionId": "s", "runningPromptId": "user-1",
                                  "entries": [{"id": "hcom-1"}]});
        assert_eq!(queue_outcome(&still_queued, &detached), None);
        let running = json!({"sessionId": "s", "runningPromptId": "hcom-1", "entries": []});
        assert_eq!(queue_outcome(&running, &detached), Some(Outcome::Delivered));
        let gone = json!({"sessionId": "s", "runningPromptId": "user-2", "entries": []});
        assert!(matches!(
            queue_outcome(&gone, &detached),
            Some(Outcome::Dropped(_))
        ));
    }

    #[test]
    fn transcript_proves_a_batch_ran() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.jsonl");
        let chunk = |text: &str| {
            json!({"method": "session/update", "params": {"update": {
                "sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": text}}}})
            .to_string()
        };
        std::fs::write(&path, format!("{}\n", chunk("fix it"))).unwrap();
        assert!(!ran_in_transcript(&path, BATCH));
        // Merged into a combined turn.
        std::fs::write(&path, format!("{}\n", chunk(&format!("fix it\n\n{BATCH}")))).unwrap();
        assert!(ran_in_transcript(&path, BATCH));
        assert!(!ran_in_transcript(&dir.path().join("missing"), BATCH));
    }

    #[test]
    fn response_outcomes() {
        for result in [
            json!({"stopReason": "end_turn"}),
            json!({"stopReason": "max_tokens"}),
            // An interrupted turn still ran the prompt.
            json!({"stopReason": "cancelled"}),
        ] {
            let value = json!({"id": 4, "result": result});
            assert_eq!(
                response_outcome(&value),
                Some(Outcome::Delivered),
                "{value}"
            );
        }
        let removed = json!({"id": 4, "result": {"stopReason": "cancelled",
                                                 "_meta": {"completionKind": "removedFromQueue"}}});
        assert_eq!(response_outcome(&removed), None);
        let error = json!({"id": 4, "error": {"code": -32000, "message": "failed"}});
        assert!(matches!(
            response_outcome(&error),
            Some(Outcome::Dropped(_))
        ));
    }

    #[test]
    fn classifies_notifications_requests_and_responses() {
        let queue = json!({"method": "_x.ai/queue/changed", "params": {"sessionId": "s"}});
        assert!(matches!(
            classify_event(queue),
            Some(Event::Notification { method, params }) if method == "x.ai/queue/changed" && params["sessionId"] == "s"
        ));
        let wrapped = json!({"method": "_x.ai/session_notification",
                             "params": {"method": "x.ai/session_notification", "params": {"sessionId": "s"}}});
        assert!(matches!(
            classify_event(wrapped),
            Some(Event::Notification { method, params }) if method == "x.ai/session_notification" && params["sessionId"] == "s"
        ));
        let permission = json!({"id": "ask-1", "method": "session/request_permission", "params": {"sessionId": "s"}});
        assert!(matches!(
            classify_event(permission),
            Some(Event::Request { id, method, .. }) if id == "ask-1" && method == "session/request_permission"
        ));
        let response = json!({"id": 3, "result": {}});
        assert!(matches!(classify_event(response), Some(Event::Response(_))));
    }

    fn entry(id: &str, activity: &str, last_change: i64) -> Value {
        json!({"sessionId": id, "cwd": "/w", "activity": activity, "resident": true,
               "lastChangeUnixMs": last_change})
    }

    fn binds(effects: &[Effect]) -> Vec<&str> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Bind { session, .. } => Some(session.as_str()),
                _ => None,
            })
            .collect()
    }

    fn loads(effects: &[Effect]) -> Vec<&str> {
        let mut ids: Vec<&str> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Load { session, .. } => Some(session.as_str()),
                _ => None,
            })
            .collect();
        ids.sort();
        ids
    }

    fn queue(session: &str, prompt: Option<&str>) -> Value {
        match prompt {
            Some(p) => {
                json!({"sessionId": session, "runningPromptId": p, "runningKind": "prompt", "entries": []})
            }
            None => json!({"sessionId": session, "entries": []}),
        }
    }

    fn note(session: &str, update: Value) -> Value {
        json!({"sessionId": session, "update": update})
    }

    fn ask(session: &str, call: &str) -> Value {
        json!({"sessionId": session, "toolCall": {"toolCallId": call, "kind": "execute",
               "rawInput": {"variant": "Bash", "command": "hcom list"}},
               "options": [{"optionId": "allow-once", "kind": "allow_once"},
                           {"optionId": "reject-once", "kind": "reject_once"}]})
    }

    #[test]
    fn permission_request_parts() {
        let request = ask("a", "t1");
        assert_eq!(permission_command(&request), Some("hcom list"));
        assert_eq!(
            permission_option(&request, "allow_once").as_deref(),
            Some("allow-once")
        );
        let edit = json!({"toolCall": {"kind": "edit", "rawInput": {"command": "hcom list"}}});
        assert_eq!(permission_command(&edit), None);
        let powershell = json!({"toolCall": {"kind": "execute",
            "rawInput": {"variant": "PowerShell", "command": "hcom list \\; Write-Output x"}}});
        assert_eq!(permission_command(&powershell), None);
    }

    #[test]
    fn binds_the_first_resident_session_and_loads_all() {
        let mut tracker = Tracker::default();
        assert!(tracker.on_roster(&[]).is_empty());
        let dormant =
            json!({"sessionId": "old", "cwd": "/w", "activity": "dormant", "resident": false});
        let effects = tracker.on_roster(&[dormant, entry("a", "idle", 5)]);
        assert_eq!(binds(&effects), ["a"]);
        assert_eq!(loads(&effects), ["a"]);
    }

    #[test]
    fn follows_new_idle_sessions_but_not_reconnects_or_working_ones() {
        let mut tracker = Tracker::default();
        tracker.on_roster(&[entry("a", "idle", 1)]);
        // /new: a fresh idle session.
        let effects = tracker.on_roster(&[entry("a", "idle", 1), entry("b", "idle", 2)]);
        assert_eq!(binds(&effects), ["b"]);
        assert_eq!(loads(&effects), ["b"]);
        // A new session that is already working waits for a user prompt.
        let effects = tracker.on_roster(&[
            entry("a", "idle", 1),
            entry("b", "idle", 2),
            entry("c", "working", 3),
        ]);
        assert!(binds(&effects).is_empty());
        assert_eq!(loads(&effects), ["c"]);
        // After a reconnect everything is reloaded, the binding kept.
        tracker.reconnected();
        let effects = tracker.on_roster(&[entry("a", "idle", 9), entry("b", "idle", 2)]);
        assert!(binds(&effects).is_empty());
        assert_eq!(loads(&effects), ["a", "b"]);
    }

    #[test]
    fn user_prompt_in_another_open_session_moves_the_binding() {
        let mut tracker = Tracker::default();
        tracker.on_roster(&[entry("a", "idle", 2), entry("b", "idle", 1)]);
        assert_eq!(tracker.bound.as_deref(), Some("a"));
        // hcom's own batch running in b (queued before a switch) is not a switch.
        assert!(
            tracker
                .on_notification("x.ai/queue/changed", &queue("b", Some("hcom-9")))
                .is_empty()
        );
        let effects = tracker.on_notification("x.ai/queue/changed", &queue("b", Some("u1")));
        assert_eq!(binds(&effects), ["b"]);
        assert_eq!(effects.last(), Some(&Effect::Status(Status::Prompt)));
        // The same running entry again is not a new turn.
        assert!(
            tracker
                .on_notification("x.ai/queue/changed", &queue("b", Some("u1")))
                .is_empty()
        );
    }

    #[test]
    fn subagent_sessions_never_bind_or_end_the_turn() {
        let mut tracker = Tracker::default();
        tracker.on_roster(&[entry("a", "idle", 1)]);
        tracker.on_notification(
            "x.ai/session_notification",
            &note(
                "a",
                json!({"sessionUpdate": "subagent_spawned", "child_session_id": "kid"}),
            ),
        );
        let effects = tracker.on_roster(&[entry("a", "working", 1), entry("kid", "idle", 2)]);
        assert!(binds(&effects).is_empty());
        assert!(loads(&effects).is_empty());
        assert!(
            tracker
                .on_notification("x.ai/queue/changed", &queue("kid", Some("u1")))
                .is_empty()
        );
        let done = json!({"sessionUpdate": "turn_completed", "prompt_id": "u1", "stop_reason": "end_turn"});
        assert!(
            tracker
                .on_notification("x.ai/session_notification", &note("kid", done))
                .is_empty()
        );
        // Its approval prompt does block the parent.
        assert_eq!(
            tracker.on_request("session/request_permission", &ask("kid", "t1")),
            [Effect::Status(Status::Blocked("approval".into()))]
        );
    }

    #[test]
    fn status_follows_the_bound_session() {
        let mut tracker = Tracker::default();
        tracker.on_roster(&[entry("a", "idle", 1)]);
        assert_eq!(
            tracker.on_notification("x.ai/queue/changed", &queue("a", Some("u1"))),
            [Effect::Status(Status::Prompt)]
        );
        let call = json!({"sessionId": "a", "update": {"sessionUpdate": "tool_call",
            "title": "run_terminal_command", "rawInput": {"command": "ls"},
            "_meta": {"x.ai/tool": {"name": "run_terminal_command"}}}});
        let tool = Status::Tool {
            name: "run_terminal_command".into(),
            input: json!({"command": "ls"}),
        };
        assert_eq!(
            tracker.on_notification("session/update", &call),
            [Effect::Status(tool.clone())]
        );
        // Grok decided it itself (rules, auto mode): never asked, not blocked.
        let auto = json!({"sessionUpdate": "pending_interaction", "kind": "permission", "tool_call_id": "t0"});
        assert!(
            tracker
                .on_notification("x.ai/session_notification", &note("a", auto))
                .is_empty()
        );
        let auto_done = json!({"sessionUpdate": "interaction_resolved", "tool_call_id": "t0"});
        assert!(
            tracker
                .on_notification("x.ai/session_notification", &note("a", auto_done))
                .is_empty()
        );
        assert_eq!(
            tracker.on_request("session/request_permission", &ask("a", "t1")),
            [Effect::Status(Status::Blocked("approval".into()))]
        );
        let resolved = json!({"sessionUpdate": "interaction_resolved", "tool_call_id": "t1"});
        assert_eq!(
            tracker.on_notification("x.ai/session_notification", &note("a", resolved)),
            [Effect::Status(tool)]
        );
        // A queued prompt already running: the old turn's end changes nothing.
        tracker.on_notification("x.ai/queue/changed", &queue("a", Some("u2")));
        let stale = json!({"sessionUpdate": "turn_completed", "prompt_id": "u1", "stop_reason": "end_turn"});
        assert!(
            tracker
                .on_notification("x.ai/session_notification", &note("a", stale))
                .is_empty()
        );
        tracker.on_notification("x.ai/queue/changed", &queue("a", None));
        let failed = json!({"sessionUpdate": "turn_completed", "prompt_id": "u2",
                            "stop_reason": "error", "error_kind": "overloaded"});
        assert_eq!(
            tracker.on_notification("x.ai/session_notification", &note("a", failed)),
            [Effect::Status(Status::Listening(
                "failure:overloaded".into()
            ))]
        );
    }

    #[test]
    fn subagents_of_a_session_left_behind_are_not_ours() {
        let mut tracker = Tracker::default();
        tracker.on_roster(&[entry("a", "idle", 1)]);
        tracker.on_notification(
            "x.ai/session_notification",
            &note(
                "a",
                json!({"sessionUpdate": "subagent_spawned", "child_session_id": "kid"}),
            ),
        );
        tracker.on_notification(
            "x.ai/session_notification",
            &note(
                "kid",
                json!({"sessionUpdate": "subagent_spawned", "child_session_id": "grandkid"}),
            ),
        );
        assert!(tracker.owns(&json!({"sessionId": "grandkid"})));
        // /new: b is bound, a's subagents keep running.
        tracker.on_roster(&[entry("a", "working", 1), entry("b", "idle", 2)]);
        assert_eq!(tracker.bound.as_deref(), Some("b"));
        assert!(!tracker.owns(&json!({"sessionId": "kid"})));
        assert!(
            tracker
                .on_request("session/request_permission", &ask("kid", "t1"))
                .is_empty()
        );
    }

    #[test]
    fn blocked_until_every_open_interaction_is_answered() {
        let mut tracker = Tracker::default();
        tracker.on_roster(&[entry("a", "idle", 1)]);
        tracker.on_notification(
            "x.ai/session_notification",
            &note(
                "a",
                json!({"sessionUpdate": "subagent_spawned", "child_session_id": "kid"}),
            ),
        );
        for (session, id) in [("a", "t1"), ("kid", "t2")] {
            tracker.on_request("session/request_permission", &ask(session, id));
        }
        // Another session's question is not ours.
        assert!(
            tracker
                .on_request(
                    "x.ai/ask_user_question",
                    &json!({"sessionId": "other", "toolCallId": "q"})
                )
                .is_empty()
        );
        let resolved =
            |id: &str| json!({"sessionUpdate": "interaction_resolved", "tool_call_id": id});
        assert!(
            tracker
                .on_notification("x.ai/session_notification", &note("a", resolved("t1")))
                .is_empty()
        );
        assert_eq!(
            tracker.on_notification("x.ai/session_notification", &note("kid", resolved("t2"))),
            [Effect::Status(Status::Prompt)]
        );
    }

    #[test]
    fn turn_end_contexts() {
        let ctx = |reason: &str| turn_end_context(&json!({"stop_reason": reason}));
        assert_eq!(ctx("end_turn"), "");
        assert_eq!(ctx("cancelled"), "cancelled");
        assert_eq!(ctx("error"), "failure:error");
        assert_eq!(ctx("rate_limit"), "failure:rate_limit");
    }

    #[test]
    fn roster_refresh_triggers() {
        let mut tracker = Tracker::default();
        tracker.on_roster(&[entry("a", "idle", 1)]);
        let known = json!({"upserted": [entry("a", "working", 2)], "removed": []});
        assert!(
            tracker
                .on_notification("x.ai/sessions/changed", &known)
                .is_empty()
        );
        let unknown = json!({"upserted": [entry("b", "working", 2)], "removed": []});
        assert_eq!(
            tracker.on_notification("x.ai/sessions/changed", &unknown),
            [Effect::RefreshRoster]
        );
        assert_eq!(
            tracker.on_notification("x.ai/models/update", &json!({})),
            [Effect::RefreshRoster]
        );
    }

    #[test]
    fn roster_result_may_be_wrapped() {
        let sessions = json!([entry("a", "idle", 1)]);
        assert_eq!(roster_sessions(&json!({"sessions": sessions})).len(), 1);
        assert_eq!(
            roster_sessions(&json!({"result": {"sessions": sessions}})).len(),
            1
        );
        assert!(roster_sessions(&json!({})).is_empty());
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
