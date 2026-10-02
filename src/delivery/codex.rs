//! Codex idle delivery through a private app-server. Hooks still own the inbox
//! and its cursor; accepting a wake into Codex's queue is never an hcom ack.
//!
//! Only queue operations are sent here: the TUI owns turns, approvals and
//! session selection. Unix uses a private control socket; Windows uses a
//! loopback WebSocket endpoint.

#[cfg(windows)]
use std::net::TcpStream as Socket;
#[cfg(unix)]
use std::os::unix::{net::UnixStream as Socket, process::CommandExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{DeliveryState, LaunchOutcome, TitleWake, ToolConfig, log_info, log_warn};
use crate::db::HcomDb;
use crate::notify::NotifyServer;
use crate::shared::ST_LISTENING;

const POLL: Duration = Duration::from_secs(1);
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
const WAKE_PREFIX: &str = "hcom-wake-";

#[derive(Clone, Debug)]
pub(crate) struct Launch {
    endpoint: String,
}

/// Own the server until the PTY and delivery thread are gone. Drop also covers
/// setup failures, so a failed TUI spawn cannot leave a server behind.
pub(crate) struct Server {
    child: Child,
    _directory: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = crate::sys::process::kill_group(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Config overrides must reach the server (notably hooks and writable roots).
/// Keep ordinary TUI arguments intact, but move the writable-root overrides:
/// Codex rejects those on a remote client.
fn server_args(args: &[&str]) -> Result<(Vec<String>, Vec<String>)> {
    let mut server = Vec::new();
    let mut tui = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            tui.extend(args[i..].iter().map(|s| s.to_string()));
            break;
        }
        if matches!(
            arg.split('=').next().unwrap_or(arg),
            "--remote" | "--remote-auth-token-env" | "--no-daemon"
        ) {
            bail!("hcom manages Codex's private app-server; remove {arg}");
        }
        if arg.split('=').next() == Some("--profile") || arg.starts_with("-p") {
            bail!("Codex app-server cannot select --profile; use -c overrides for hcom launches");
        }
        let raw = if matches!(arg, "-c" | "--config") {
            i += 1;
            Some(*args.get(i).context("Codex config flag requires a value")?)
        } else {
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c="))
                .or_else(|| arg.strip_prefix("-c"))
        };
        if let Some(raw) = raw {
            server.extend(["-c".to_string(), raw.to_string()]);
            let key = raw.split('=').next().unwrap_or(raw).trim();
            if key != "sandbox_workspace_write" && !key.starts_with("sandbox_workspace_write.") {
                tui.extend(["-c".to_string(), raw.to_string()]);
            }
        } else if matches!(arg, "--enable" | "--disable") {
            i += 1;
            let value = *args.get(i).context("Codex flag requires a value")?;
            tui.extend([arg.to_string(), value.to_string()]);
            server.extend([arg.to_string(), value.to_string()]);
        } else {
            if arg.starts_with("--enable=") || arg.starts_with("--disable=") {
                server.push(arg.to_string());
            }
            tui.push(arg.to_string());
        }
        i += 1;
    }
    Ok((server, tui))
}

impl Launch {
    pub(crate) fn start(
        command: &str,
        prefix: &[String],
        args: &[&str],
        env: &[(String, String)],
    ) -> Result<(Self, Server, Vec<String>)> {
        let (server_args, mut tui_args) = server_args(args)?;
        let directory = tempfile::Builder::new().prefix("hcom-codex-").tempdir()?;
        #[cfg(unix)]
        let endpoint = format!("unix://{}", directory.path().join("server.sock").display());
        #[cfg(windows)]
        let endpoint = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            format!("ws://{}", listener.local_addr()?)
        };
        let launch = Self { endpoint };
        let log_path = directory.path().join("server.log");
        let mut command = Command::new(command);
        command
            .args(prefix)
            .args(&server_args)
            .args(["app-server", "--listen", &launch.endpoint])
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log_path)?);
        // npm shims spawn a native Codex child. Own their entire process tree.
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .context("starting private Codex app-server")?;
        let mut server = Server {
            child,
            _directory: directory,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = server.child.try_wait()? {
                let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
                bail!("Codex app-server exited ({status}): {detail}");
            }
            if Client::connect(&launch).is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                bail!(
                    "Timed out starting private Codex app-server: {}",
                    std::fs::read_to_string(&log_path).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Insert before the user's prompt marker and any resume/fork subcommand.
        tui_args.splice(0..0, ["--remote".into(), launch.endpoint.clone()]);
        Ok((launch, server, tui_args))
    }
}

struct Client {
    websocket: WebSocket<Socket>,
    next_id: u64,
}

impl Client {
    fn connect(launch: &Launch) -> Result<Self> {
        #[cfg(unix)]
        let socket = Socket::connect(
            launch
                .endpoint
                .strip_prefix("unix://")
                .context("Codex socket endpoint")?,
        )?;
        #[cfg(windows)]
        let socket = Socket::connect_timeout(
            &launch
                .endpoint
                .strip_prefix("ws://")
                .context("Codex loopback endpoint")?
                .parse()?,
            RPC_TIMEOUT,
        )?;
        socket.set_read_timeout(Some(RPC_TIMEOUT))?;
        socket.set_write_timeout(Some(RPC_TIMEOUT))?;
        #[cfg(unix)]
        let handshake_url = "ws://localhost/";
        #[cfg(windows)]
        let handshake_url = launch.endpoint.as_str();
        let (websocket, _) = tungstenite::client(handshake_url, socket)
            .map_err(|error| anyhow::anyhow!("Codex socket handshake: {error}"))?;
        let mut client = Self {
            websocket,
            next_id: 0,
        };
        client.request(
            "initialize",
            json!({
                "clientInfo": {"name": "hcom", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        )?;
        client.websocket.send(Message::Text(
            json!({"method":"initialized"}).to_string().into(),
        ))?;
        Ok(client)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.websocket.send(Message::Text(
            json!({"id": id, "method": method, "params": params})
                .to_string()
                .into(),
        ))?;
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("Codex {method} response timed out");
            }
            self.websocket.get_mut().set_read_timeout(Some(remaining))?;
            let message = self
                .websocket
                .read()
                .with_context(|| format!("Codex {method} response"))?;
            let Message::Text(text) = message else {
                if matches!(message, Message::Close(_)) {
                    bail!("Codex socket closed");
                }
                continue;
            };
            let response: Value = serde_json::from_str(&text)?;
            if response.get("id").and_then(Value::as_u64) != Some(id)
                || response.get("method").is_some()
            {
                continue;
            }
            if let Some(error) = response.get("error") {
                bail!("Codex {method}: {error}");
            }
            return response
                .get("result")
                .cloned()
                .context("Codex response missing result");
        }
    }

    fn initial_thread(&mut self) -> Result<Option<String>> {
        let response = self.request("thread/loaded/list", json!({}))?;
        let ids = response["data"]
            .as_array()
            .context("Codex loaded thread list missing data")?;
        // Before the first prompt, SessionStart hasn't run yet. This private
        // server has only the TUI's root thread; never guess if it has more.
        Ok(if ids.len() == 1 {
            ids[0].as_str().map(str::to_owned)
        } else {
            None
        })
    }

    fn wakes(&mut self, thread: &str) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let response = self.request(
                "thread/queue/list",
                json!({"threadId": thread, "limit":100, "cursor":cursor}),
            )?;
            let data = response
                .get("data")
                .and_then(Value::as_array)
                .context("Codex queue list missing data")?;
            ids.extend(data.iter().filter_map(wake_id));
            cursor = response.get("nextCursor").cloned().unwrap_or(Value::Null);
            if cursor.is_null() {
                return Ok(ids);
            }
        }
    }

    fn delete(&mut self, thread: &str, id: &str) -> Result<()> {
        self.request(
            "thread/queue/delete",
            json!({"threadId":thread,"queuedSubmissionId":id}),
        )?;
        Ok(())
    }

    fn wake(&mut self, thread: &str, pending: bool) -> Result<bool> {
        let wakes = self.wakes(thread)?;
        if !pending {
            for id in wakes {
                self.delete(thread, &id)?;
            }
            return Ok(false);
        }
        if let Some(id) = wakes.first() {
            for duplicate in wakes.iter().skip(1) {
                self.delete(thread, duplicate)?;
            }
            // Interrupted turns don't auto-dispatch existing queue entries.
            // start is conditional on server-side idle; a race with an active
            // turn preserves the entry. Busy is a normal outcome here.
            let _ = self.request(
                "thread/queue/start",
                json!({"threadId":thread,"queuedSubmissionId":id}),
            );
        } else {
            self.request(
                "thread/queue/add",
                json!({
                    "threadId": thread,
                    "clientUserMessageId": format!("{WAKE_PREFIX}{}", uuid::Uuid::new_v4()),
                    "input": [{"type":"text", "text":"<hcom>", "textElements":[]}]
                }),
            )?;
        }
        Ok(true)
    }
}

fn wake_id(entry: &Value) -> Option<String> {
    entry
        .get("clientUserMessageId")?
        .as_str()?
        .starts_with(WAKE_PREFIX)
        .then(|| entry.get("id")?.as_str().map(str::to_owned))
        .flatten()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    launch: &Launch,
    running: &AtomicBool,
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
    let mut current_status = ST_LISTENING.to_string();
    let mut heartbeat = Instant::now();
    let mut retry_at = Instant::now();
    let mut queued_thread: Option<String> = None;
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
        super::drive_launch_outcome(
            db,
            state,
            current_name,
            &current_status,
            config,
            launch_outcome,
        );
        if heartbeat.elapsed() >= Duration::from_secs(5) {
            db.reconnect_if_stale();
            super::refresh_liveness(db, current_name);
            let _ = db.refresh_pty_endpoints(current_name, notify.port(), state.inject_port);
            heartbeat = Instant::now();
        }
        let instance = match db.get_instance_full(current_name) {
            Ok(Some(instance)) => instance,
            Ok(None) => break,
            Err(error) => {
                log_warn("native", "codex.queue.instance_error", &format!("{error}"));
                notify.wait(POLL);
                continue;
            }
        };
        if Instant::now() >= retry_at {
            let pending = db.has_pending(current_name);
            let eligible = matches!(instance.status.as_str(), "listening" | "active" | "blocked");
            if (pending && eligible) || queued_thread.is_some() {
                let result = (|| -> Result<()> {
                    if client.is_none() {
                        client = Some(Client::connect(launch)?);
                    }
                    let client = client.as_mut().unwrap();
                    if let Some(old) = queued_thread.as_ref()
                        && (!pending || instance.session_id.as_ref() != Some(old))
                    {
                        client.wake(old, false)?;
                        queued_thread = None;
                    }
                    if pending && eligible {
                        let thread = match instance.session_id.clone() {
                            Some(thread) => thread,
                            None => {
                                let Some(thread) = client.initial_thread()? else {
                                    return Ok(());
                                };
                                crate::instance_binding::bind_session_to_process(
                                    db,
                                    &thread,
                                    Some(process_id),
                                )
                                .context("binding initial Codex thread")?;
                                thread
                            }
                        };
                        // Remember before the add: a lost response may still
                        // have queued it. Reconnect inspects the durable queue.
                        queued_thread = Some(thread.to_string());
                        client.wake(&thread, true)?;
                        log_info(
                            "native",
                            "codex.queue.wake",
                            &format!("name={current_name}, thread={thread}"),
                        );
                        retry_at = Instant::now() + Duration::from_secs(2);
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    log_warn(
                        "native",
                        "codex.queue.retry",
                        &format!("name={current_name}: {error:#}"),
                    );
                    client = None;
                    retry_at = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        notify.wait(POLL);
    }
    // A queued sentinel is only a wake; remove it when its TUI goes away.
    if let (Some(client), Some(thread)) = (client.as_mut(), queued_thread.as_deref()) {
        let _ = client.wake(thread, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_receives_hooks_roots_and_features_without_remote_root_rejection() {
        let (server, tui) = server_args(&[
            "-c",
            "hooks.UserPromptSubmit=[]",
            "-csandbox_workspace_write.writable_roots=['/state']",
            "--enable=hooks",
            "resume",
            "thread",
            "--",
            "--remote",
        ])
        .unwrap();
        assert!(server.contains(&"hooks.UserPromptSubmit=[]".into()));
        assert!(server.contains(&"sandbox_workspace_write.writable_roots=['/state']".into()));
        assert!(server.contains(&"--enable=hooks".into()));
        assert!(!tui.iter().any(|arg| arg.contains("writable_roots")));
        assert_eq!(
            &tui[tui.len() - 4..],
            &["resume", "thread", "--", "--remote"]
        );
        assert!(server_args(&["--remote=unix:///other"]).is_err());
        assert!(server_args(&["--no-daemon"]).is_err());
        assert!(server_args(&["--profile", "work"]).is_err());
    }

    #[cfg(unix)]
    fn mock_client(
        steps: Vec<(&'static str, Value)>,
        action: impl FnOnce(&mut Client),
    ) -> Vec<Value> {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(RPC_TIMEOUT)).unwrap();
            let mut websocket = tungstenite::accept(stream).unwrap();
            let mut requests = Vec::new();
            let mut all_steps = vec![("initialize", json!({"result":{}}))];
            all_steps.extend(steps);
            for (method, mut response) in all_steps {
                let request = loop {
                    let Message::Text(text) = websocket.read().unwrap() else {
                        continue;
                    };
                    let request: Value = serde_json::from_str(&text).unwrap();
                    if request.get("id").is_some() {
                        break request;
                    }
                    assert_eq!(request["method"], "initialized");
                };
                assert_eq!(request["method"], method);
                response["id"] = request["id"].clone();
                // Notifications can interleave with any queue response.
                websocket
                    .send(Message::Text(
                        json!({"method":"thread/queue/changed", "params":{}})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                websocket
                    .send(Message::Text(response.to_string().into()))
                    .unwrap();
                requests.push(request);
            }
            // Keep the peer open until the client consumes the final reply.
            let _ = websocket.read();
            requests
        });
        let launch = Launch {
            endpoint: format!("unix://{}", socket.display()),
        };
        let mut client = Client::connect(&launch).unwrap();
        action(&mut client);
        drop(client);
        server.join().unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn interrupted_wake_is_started_and_coalesced_without_touching_human_queue() {
        let requests = mock_client(
            vec![
                (
                    "thread/queue/list",
                    json!({"result":{"data":[
                {"id":"human", "clientUserMessageId":"user"},
                {"id":"wake", "clientUserMessageId":"hcom-wake-1"},
                {"id":"duplicate", "clientUserMessageId":"hcom-wake-2"}
            ], "nextCursor":null}}),
                ),
                ("thread/queue/delete", json!({"result":{"deleted":true}})),
                (
                    "thread/queue/start",
                    json!({"error":{"code":-32000, "message":"thread busy"}}),
                ),
            ],
            |client| {
                assert!(client.wake("thread", true).unwrap());
            },
        );
        assert_eq!(requests[2]["params"]["queuedSubmissionId"], "duplicate");
        assert_eq!(requests[3]["params"]["queuedSubmissionId"], "wake");
    }

    #[test]
    #[cfg(unix)]
    fn fresh_wake_queues_only_sentinel_and_stale_wake_is_removed() {
        let requests = mock_client(
            vec![
                (
                    "thread/queue/list",
                    json!({"result":{"data":[], "nextCursor":null}}),
                ),
                (
                    "thread/queue/add",
                    json!({"result":{"queuedSubmission":{"id":"wake"}}}),
                ),
                (
                    "thread/queue/list",
                    json!({"result":{"data":[
                {"id":"human", "clientUserMessageId":"user"},
                {"id":"wake", "clientUserMessageId":"hcom-wake-1"}
            ], "nextCursor":null}}),
                ),
                ("thread/queue/delete", json!({"result":{"deleted":true}})),
            ],
            |client| {
                assert!(client.wake("thread", true).unwrap());
                assert!(!client.wake("thread", false).unwrap());
            },
        );
        assert_eq!(requests[2]["params"]["input"][0]["text"], "<hcom>");
        assert_eq!(requests[4]["params"]["queuedSubmissionId"], "wake");
    }

    #[test]
    fn queue_cleanup_recognizes_only_hcom_wakes() {
        assert_eq!(
            wake_id(&json!({"id":"wake", "clientUserMessageId":"hcom-wake-123"})),
            Some("wake".into())
        );
        assert_eq!(
            wake_id(&json!({"id":"human", "clientUserMessageId":"user-123"})),
            None
        );
        assert_eq!(wake_id(&json!({"id":"human"})), None);
    }
}
