//! Opt-in Codex idle delivery (`codex_native_delivery`) through a private
//! app-server. hcom starts `codex app-server` on a private Unix socket, attaches
//! the TUI with `--remote`, and wakes an idle agent by queueing `<hcom>` on the
//! thread the TUI is showing. Nothing is typed into the composer, so drafts
//! survive. Hooks still own the inbox and its cursor; accepting a wake into
//! Codex's queue is never an hcom ack.
//!
//! The connection only sends queue operations; the TUI owns turns, approvals
//! and session selection. It does follow the TUI's selection from server
//! notifications, because `/new` and `/resume` switch threads without a hook
//! until the next prompt, and a wake on the old thread would run invisibly.
//!
//! Launches this cannot serve (Windows, flags that conflict with `--remote`,
//! a server that fails to start) fall back to terminal delivery.

use std::process::Child;

use super::log_warn;

#[cfg(unix)]
pub(crate) use unix::Launch;
#[cfg(unix)]
pub(super) use unix::run;

/// Own the server until the PTY and delivery thread are gone. Drop also covers
/// setup failures, so a failed TUI spawn cannot leave a server behind.
pub(crate) struct Server {
    child: Child,
    _directory: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        // npm shims spawn the native binary as a child; own the whole group.
        let _ = crate::sys::process::kill_group(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Why a launch can't attach through hcom's private server, if it can't.
fn incompatible(args: &[&str]) -> Option<String> {
    let flags = || args.iter().copied().take_while(|arg| *arg != "--");
    // A remote TUI resumes or forks with the thread's saved permissions:
    // resume drops explicit overrides and fork rejects them.
    let resumes = flags().any(|arg| matches!(arg, "resume" | "fork"));
    let mut config_value = false;
    for arg in flags() {
        let flag = arg.split('=').next().unwrap_or(arg);
        let raw = if std::mem::take(&mut config_value) {
            Some(arg)
        } else {
            config_value = matches!(arg, "-c" | "--config");
            arg.strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c").filter(|raw| !raw.is_empty()))
        };
        let conflict = match raw {
            Some(raw) => resumes && permission_key(raw),
            None => match flag {
                // hcom owns the connection.
                "--remote" | "--remote-auth-token-env" | "--no-daemon" => true,
                // app-server can't load a v2 profile; the TUI's would be ignored.
                "--profile" => true,
                // The remote TUI rejects these; they configure the server.
                "--add-dir" | "--worktree" => true,
                "-a"
                | "--ask-for-approval"
                | "-s"
                | "--sandbox"
                | "--yolo"
                | "--dangerously-bypass-approvals-and-sandbox"
                | "--approve-for-me"
                | "--not-so-yolo" => resumes,
                _ => flag.starts_with("-p") && !flag.starts_with("--"),
            },
        };
        if conflict {
            return Some(raw.unwrap_or(flag).to_string());
        }
    }
    None
}

/// Config keys Codex treats as explicit permission choices on resume.
/// `sandbox_workspace_write` is excluded: it goes to the server only.
fn permission_key(raw: &str) -> bool {
    let key = raw
        .trim_start_matches('=')
        .split('=')
        .next()
        .unwrap_or("")
        .trim();
    matches!(
        key.split('.').next().unwrap_or(key),
        "approval_policy"
            | "approvals_reviewer"
            | "sandbox_mode"
            | "default_permissions"
            | "permissions"
    )
}

/// Config overrides must reach the server (notably hooks and writable roots).
/// Keep ordinary TUI arguments intact, but move the writable-root overrides:
/// Codex rejects those on a remote client.
#[cfg_attr(not(unix), allow(dead_code))]
fn split_args(args: &[&str]) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    use anyhow::Context;
    let mut server = Vec::new();
    let mut tui = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            tui.extend(args[i..].iter().map(|s| s.to_string()));
            break;
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

/// Start native delivery if the user opted in and this launch supports it.
/// Returns the delivery handle, the server guard and the TUI's arguments.
pub(crate) fn start(
    command: &str,
    prefix: &[String],
    args: &[&str],
    env: &[(String, String)],
) -> Option<(Launch, Server, Vec<String>)> {
    let enabled = crate::config::HcomConfig::load(None)
        .map(|config| config.codex_native_delivery)
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    if !cfg!(unix) {
        log_warn(
            "native",
            "codex.native.fallback",
            "unsupported on this platform",
        );
        return None;
    }
    if let Some(flag) = incompatible(args) {
        log_warn(
            "native",
            "codex.native.fallback",
            &format!("{flag} is incompatible with native delivery; using terminal delivery"),
        );
        return None;
    }
    #[cfg(unix)]
    match unix::Launch::start(command, prefix, args, env) {
        Ok(started) => return Some(started),
        Err(error) => log_warn(
            "native",
            "codex.native.fallback",
            &format!("private app-server failed; using terminal delivery: {error:#}"),
        ),
    }
    #[cfg(not(unix))]
    let _ = (command, prefix, env);
    None
}

/// Uninhabited off Unix: native delivery never starts there.
#[cfg(not(unix))]
#[derive(Clone, Debug)]
pub(crate) enum Launch {}

#[cfg(not(unix))]
#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    launch: &Launch,
    _running: &std::sync::atomic::AtomicBool,
    _db: &mut crate::db::HcomDb,
    _notify: &crate::notify::NotifyServer,
    _state: &super::DeliveryState,
    _process_id: &str,
    _current_name: &mut String,
    _config: &super::ToolConfig,
    _shared_name: &Option<std::sync::Arc<std::sync::RwLock<String>>>,
    _shared_status: &Option<std::sync::Arc<std::sync::RwLock<String>>>,
    _title_wake: &Option<super::TitleWake>,
    _host_label: &mut super::host_label::HostLabel,
    _launch_outcome: &mut super::LaunchOutcome,
) {
    match *launch {}
}

#[cfg(unix)]
mod unix {
    use std::collections::HashSet;
    use std::os::unix::{net::UnixStream, process::CommandExt};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use serde_json::{Value, json};
    use tungstenite::{Message, WebSocket};

    use super::super::{DeliveryState, LaunchOutcome, TitleWake, ToolConfig, log_info, log_warn};
    use super::{Server, split_args};
    use crate::db::HcomDb;
    use crate::notify::NotifyServer;
    use crate::shared::ST_LISTENING;

    const POLL: Duration = Duration::from_secs(1);
    const RPC_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    const WAKE_PREFIX: &str = "hcom-wake-";
    const DIR_PREFIX: &str = "hcom-codex-";
    /// `<wrapper pid> <server pid>`, for sweeping servers whose wrapper was
    /// killed before it could drop its [`Server`].
    const OWNER_FILE: &str = "owner";

    /// The native binary an npm-style `codex.js` launcher would spawn, so the
    /// private server runs without a Node process in front of it. The launcher
    /// only adds `CODEX_MANAGED_BY_*`, which just picks the TUI's update hint.
    /// Wrappers and unrecognized layouts keep the original command.
    fn native_server_binary(command: &str, prefix: &[String]) -> Option<std::path::PathBuf> {
        if !prefix.is_empty() {
            return None;
        }
        let launcher = std::fs::canonicalize(command).ok()?;
        if !launcher.ends_with("node_modules/@openai/codex/bin/codex.js") {
            return None;
        }
        let root = launcher.parent()?.parent()?;
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(root.join("package.json")).ok()?).ok()?;
        let os = match std::env::consts::OS {
            "macos" => "darwin",
            "linux" => "linux",
            _ => return None,
        };
        // Take whichever platform package the installer picked rather than
        // hcom's own arch: Node may run under Rosetta. Resolve it like Node
        // (nested, then hoisted), falling back to the package's own vendor.
        let platform = format!("@openai/codex-{os}-");
        let mut installed = manifest["optionalDependencies"]
            .as_object()?
            .keys()
            .filter(|name| name.starts_with(&platform))
            .filter_map(|name| {
                root.ancestors()
                    .map(|dir| dir.join("node_modules").join(name))
                    .find(|dir| dir.join("package.json").is_file())
            });
        let vendor = match (installed.next(), installed.next()) {
            (Some(package), None) => package.join("vendor"),
            (None, _) => root.join("vendor"),
            (Some(_), Some(_)) => return None,
        };
        // vendor/<target triple>/bin/codex; one target, or it's ambiguous.
        let mut binaries = std::fs::read_dir(vendor)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("bin/codex"))
            .filter(|path| {
                use std::os::unix::fs::PermissionsExt;
                path.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            });
        match (binaries.next(), binaries.next()) {
            (Some(binary), None) => Some(binary),
            _ => None,
        }
    }

    #[derive(Clone)]
    pub(crate) struct Launch {
        endpoint: String,
        /// The readiness connection, opened before the TUI starts so it sees
        /// the TUI's first thread. The delivery loop takes it over.
        early: Arc<Mutex<Option<Client>>>,
    }

    impl std::fmt::Debug for Launch {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Launch")
                .field("endpoint", &self.endpoint)
                .finish()
        }
    }

    impl Launch {
        fn socket(&self) -> &str {
            self.endpoint
                .strip_prefix("unix://")
                .unwrap_or(&self.endpoint)
        }

        pub(super) fn start(
            command: &str,
            prefix: &[String],
            args: &[&str],
            env: &[(String, String)],
        ) -> Result<(Self, Server, Vec<String>)> {
            sweep_orphans();
            let (server_args, mut tui_args) = split_args(args)?;
            let directory = tempfile::Builder::new().prefix(DIR_PREFIX).tempdir()?;
            let launch = Self {
                endpoint: format!("unix://{}", directory.path().join("server.sock").display()),
                early: Arc::default(),
            };
            let log_path = directory.path().join("server.log");
            let native = native_server_binary(command, prefix);
            if let Some(binary) = &native {
                log_info(
                    "native",
                    "codex.native.binary",
                    &binary.display().to_string(),
                );
            }
            let mut server_command = Command::new(
                native
                    .as_deref()
                    .map_or(std::ffi::OsStr::new(command), std::path::Path::as_os_str),
            );
            server_command
                .args(prefix)
                .args(&server_args)
                .args(["app-server", "--listen", &launch.endpoint])
                .envs(env.iter().map(|(k, v)| (k, v)))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log_path)?)
                .process_group(0);
            let child = server_command
                .spawn()
                .context("starting private Codex app-server")?;
            let _ = std::fs::write(
                directory.path().join(OWNER_FILE),
                format!("{} {}", std::process::id(), child.id()),
            );
            let mut server = Server {
                child,
                _directory: directory,
            };
            let deadline = Instant::now() + Duration::from_secs(15);
            let client = loop {
                if let Some(status) = server.child.try_wait()? {
                    let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
                    bail!("Codex app-server exited ({status}): {detail}");
                }
                if let Ok(client) = Client::connect(launch.socket(), None) {
                    break client;
                }
                if Instant::now() >= deadline {
                    bail!(
                        "Timed out starting private Codex app-server: {}",
                        std::fs::read_to_string(&log_path).unwrap_or_default()
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            *launch.early.lock().unwrap_or_else(|e| e.into_inner()) = Some(client);
            log_info("native", "codex.native.started", &launch.endpoint);
            // Insert before the user's prompt marker and any resume/fork subcommand.
            tui_args.splice(0..0, ["--remote".into(), launch.endpoint.clone()]);
            Ok((launch, server, tui_args))
        }
    }

    /// Stop servers left by wrappers that died without cleanup (SIGKILL, OOM).
    /// Only a socket that still accepts proves the recorded group is ours.
    fn sweep_orphans() {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_name().to_string_lossy().starts_with(DIR_PREFIX) {
                continue;
            }
            let Some((owner, server)) = std::fs::read_to_string(path.join(OWNER_FILE))
                .ok()
                .and_then(|text| {
                    let mut pids = text.split_whitespace().map(str::parse::<u32>);
                    Some((pids.next()?.ok()?, pids.next()?.ok()?))
                })
            else {
                continue;
            };
            if crate::sys::process::is_alive(owner) {
                continue;
            }
            if UnixStream::connect(path.join("server.sock")).is_ok() {
                let _ = crate::sys::process::kill_group(server);
                log_info(
                    "native",
                    "codex.native.orphan_stopped",
                    &format!("pid={server}"),
                );
            }
            let _ = std::fs::remove_dir_all(&path);
        }
    }

    pub(super) struct Client {
        websocket: WebSocket<UnixStream>,
        next_id: u64,
        /// Root thread the TUI last started, forked or resumed.
        root: Option<String>,
        loaded: HashSet<String>,
        /// Newly loaded threads that may be a TUI `/resume`; checked lazily.
        unchecked: Vec<String>,
    }

    impl Client {
        /// `hint` is the hook-bound thread, used only to pick among loaded
        /// threads after a reconnect has lost the notification history.
        fn connect(socket: &str, hint: Option<&str>) -> Result<Self> {
            let socket = UnixStream::connect(socket)?;
            socket.set_read_timeout(Some(RPC_TIMEOUT))?;
            socket.set_write_timeout(Some(RPC_TIMEOUT))?;
            let (websocket, _) = tungstenite::client("ws://localhost/", socket)
                .map_err(|error| anyhow::anyhow!("Codex socket handshake: {error}"))?;
            let mut client = Self {
                websocket,
                next_id: 0,
                root: None,
                loaded: HashSet::new(),
                unchecked: Vec::new(),
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
            let response = client.request("thread/loaded/list", json!({}))?;
            client.loaded = response["data"]
                .as_array()
                .context("Codex loaded thread list missing data")?
                .iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect();
            client.root = match hint.filter(|hint| client.loaded.contains(*hint)) {
                Some(hint) => Some(hint.to_string()),
                None => {
                    let mut roots = Vec::new();
                    for id in client.loaded.clone() {
                        if client.is_root(&id)? {
                            roots.push(id);
                        }
                    }
                    // Never guess between several root threads.
                    (roots.len() == 1).then(|| roots.remove(0))
                }
            };
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
                let Some(response) = self.receive(message)? else {
                    continue;
                };
                if response.get("id").and_then(Value::as_u64) != Some(id) {
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

        /// Track thread selection from a notification; return responses.
        fn receive(&mut self, message: Message) -> Result<Option<Value>> {
            let text = match message {
                Message::Text(text) => text,
                Message::Close(_) => bail!("Codex socket closed"),
                _ => return Ok(None),
            };
            let message: Value = serde_json::from_str(&text)?;
            let Some(method) = message.get("method").and_then(Value::as_str) else {
                return Ok(Some(message));
            };
            let params = &message["params"];
            match method {
                "thread/started" => {
                    let thread = &params["thread"];
                    if let Some(id) = thread["id"].as_str() {
                        self.loaded.insert(id.to_string());
                        if is_root(thread) {
                            self.root = Some(id.to_string());
                        }
                    }
                }
                "thread/status/changed" => {
                    if let Some(id) = params["threadId"].as_str() {
                        if params["status"]["type"] == "notLoaded" {
                            self.forget(id);
                        } else if self.loaded.insert(id.to_string()) {
                            self.unchecked.push(id.to_string());
                        }
                    }
                }
                "thread/closed" => {
                    if let Some(id) = params["threadId"].as_str() {
                        self.forget(id);
                    }
                }
                _ => {}
            }
            Ok(None)
        }

        fn forget(&mut self, id: &str) {
            self.loaded.remove(id);
            self.unchecked.retain(|unchecked| unchecked != id);
            if self.root.as_deref() == Some(id) {
                self.root = None;
            }
        }

        fn is_root(&mut self, id: &str) -> Result<bool> {
            let response = self.request("thread/read", json!({"threadId": id}))?;
            Ok(is_root(&response["thread"]))
        }

        /// Read pending notifications without blocking, then classify threads
        /// that were loaded without a `thread/started` (a TUI `/resume`).
        fn pump(&mut self) -> Result<()> {
            self.websocket
                .get_mut()
                .set_read_timeout(Some(Duration::from_millis(1)))?;
            loop {
                match self.websocket.read() {
                    Ok(message) => {
                        self.receive(message)?;
                    }
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            while let Some(id) = self.unchecked.pop() {
                if self.is_root(&id)? {
                    self.root = Some(id);
                }
            }
            Ok(())
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

        /// Remove every hcom wake from `thread`, leaving the human's queue.
        fn clear(&mut self, thread: &str) -> Result<()> {
            for id in self.wakes(thread)? {
                self.delete(thread, &id)?;
            }
            Ok(())
        }

        /// Ensure exactly one wake is queued on `thread` and nudge it to start.
        fn wake(&mut self, thread: &str) -> Result<()> {
            let wakes = self.wakes(thread)?;
            if let Some(id) = wakes.first() {
                for duplicate in wakes.iter().skip(1) {
                    self.delete(thread, duplicate)?;
                }
                // Interrupted turns don't auto-dispatch existing queue entries.
                // start is conditional on server-side idle; a race with an
                // active turn preserves the entry. Busy is a normal outcome.
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
            Ok(())
        }
    }

    /// A thread the TUI can show as its session: not a subagent, and not one
    /// of the ephemeral helper threads the TUI starts for side requests.
    fn is_root(thread: &Value) -> bool {
        thread["parentThreadId"].is_null() && thread["ephemeral"] != true
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
    pub(in crate::delivery) fn run(
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
        host_label: &mut super::super::host_label::HostLabel,
        launch_outcome: &mut LaunchOutcome,
    ) {
        let mut client = launch
            .early
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let mut current_status = ST_LISTENING.to_string();
        let mut heartbeat = Instant::now();
        let mut retry_at = Instant::now();
        let mut backoff = POLL;
        // Thread holding our wake. Remembered before the add: a lost response
        // may still have queued it, and cleanup inspects the durable queue.
        let mut queued: Option<String> = None;
        while running.load(Ordering::Acquire) {
            super::super::refresh_title_state(super::super::TitleRefresh {
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
            super::super::drive_launch_outcome(
                db,
                state,
                current_name,
                &current_status,
                config,
                launch_outcome,
            );
            if heartbeat.elapsed() >= Duration::from_secs(5) {
                db.reconnect_if_stale();
                super::super::refresh_liveness(db, current_name);
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
                let result = (|| -> Result<()> {
                    if client.is_none() {
                        client = Some(Client::connect(
                            launch.socket(),
                            instance.session_id.as_deref(),
                        )?);
                    }
                    let client = client.as_mut().unwrap();
                    client.pump()?;
                    let pending = db.has_pending(current_name);
                    let status = instance.status.as_str();
                    let eligible = matches!(status, "listening" | "active" | "blocked");
                    let target = client.root.clone().filter(|_| pending && eligible);
                    // Best effort: a stale wake on a thread the TUI left must
                    // not block waking the thread it shows now. The hook
                    // rejects an empty wake if one ever runs.
                    if let Some(old) = queued.take_if(|old| target.as_ref() != Some(old))
                        && let Err(error) = client.clear(&old)
                    {
                        log_warn(
                            "native",
                            "codex.queue.clear_failed",
                            &format!("name={current_name}, thread={old}: {error:#}"),
                        );
                    }
                    let Some(target) = target else {
                        return Ok(());
                    };
                    // A queued wake dispatches when the running turn ends;
                    // only an idle thread needs another nudge.
                    if queued.as_deref() == Some(target.as_str()) && status != ST_LISTENING {
                        return Ok(());
                    }
                    if queued.is_none() {
                        log_info(
                            "native",
                            "codex.queue.wake",
                            &format!("name={current_name}, thread={target}"),
                        );
                    }
                    queued = Some(target.clone());
                    client.wake(&target)?;
                    retry_at = Instant::now() + Duration::from_secs(2);
                    Ok(())
                })();
                match result {
                    Ok(()) => backoff = POLL,
                    Err(error) => {
                        log_warn(
                            "native",
                            "codex.queue.retry",
                            &format!("name={current_name}: {error:#}"),
                        );
                        client = None;
                        retry_at = Instant::now() + backoff;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                }
            }
            notify.wait(POLL);
        }
        // A queued sentinel is only a wake; remove it when its TUI goes away.
        if let (Some(client), Some(thread)) = (client.as_mut(), queued.as_deref()) {
            let _ = client.clear(thread);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn native_server_binary_follows_npm_layouts() {
            let os = match std::env::consts::OS {
                "macos" => "darwin",
                "linux" => "linux",
                _ => return,
            };
            let temp = tempfile::tempdir().unwrap();
            let modules = temp.path().join("node_modules");
            let root = modules.join("@openai/codex");
            std::fs::create_dir_all(root.join("bin")).unwrap();
            let launcher = root.join("bin/codex.js");
            std::fs::write(&launcher, "#!/usr/bin/env node\n").unwrap();
            let launcher = launcher.to_str().unwrap();
            // Deliberately not hcom's arch: Node may run under Rosetta.
            let package = format!("@openai/codex-{os}-other");
            let manifest = json!({"name": "@openai/codex", "optionalDependencies": {
                format!("@openai/codex-{os}-x64"): "", format!("@openai/codex-{os}-arm64"): "",
                package.clone(): "", "@openai/codex-plan9-x64": "",
            }});
            std::fs::write(root.join("package.json"), manifest.to_string()).unwrap();
            let install = |dir: &std::path::Path, target: &str| {
                let binary = dir.join("vendor").join(target).join("bin/codex");
                std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
                std::fs::write(&binary, "").unwrap();
                crate::sys::fs::set_executable(&binary).unwrap();
                if !dir.join("package.json").exists() {
                    std::fs::write(dir.join("package.json"), "{}").unwrap();
                }
                binary.canonicalize().unwrap()
            };

            // Neither platform package nor bundled vendor: keep the launcher.
            assert_eq!(native_server_binary(launcher, &[]), None);
            // Bundled vendor in the main package.
            let bundled = install(&root, "bundled-triple");
            assert_eq!(native_server_binary(launcher, &[]), Some(bundled));
            // Hoisted platform package wins over the bundled fallback.
            let hoisted = install(&modules.join(&package), "triple");
            assert_eq!(native_server_binary(launcher, &[]), Some(hoisted));
            // A custom wrapper owns its own launch.
            assert_eq!(native_server_binary(launcher, &["wrapper".into()]), None);
            // Two installed platform packages: Node would pick by its arch.
            install(
                &root
                    .join("node_modules")
                    .join(format!("@openai/codex-{os}-x64")),
                "x",
            );
            assert_eq!(native_server_binary(launcher, &[]), None);
        }

        /// Scripted peer: answers each expected request in order, sending
        /// `notes` notifications just before the response.
        fn mock_client(
            steps: Vec<(&'static str, Value, Vec<Value>)>,
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
                let mut all_steps = vec![
                    ("initialize", json!({"result":{}}), vec![]),
                    ("thread/loaded/list", json!({"result":{"data":[]}}), vec![]),
                ];
                all_steps.extend(steps);
                for (method, mut response, notes) in all_steps {
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
                    for note in notes {
                        websocket
                            .send(Message::Text(note.to_string().into()))
                            .unwrap();
                    }
                    websocket
                        .send(Message::Text(response.to_string().into()))
                        .unwrap();
                    requests.push(request);
                }
                // Keep the peer open until the client consumes the final reply.
                let _ = websocket.read();
                requests
            });
            let mut client = Client::connect(socket.to_str().unwrap(), None).unwrap();
            action(&mut client);
            drop(client);
            server.join().unwrap()
        }

        fn started(id: &str, parent: Option<&str>) -> Value {
            json!({"method":"thread/started","params":{"thread":{"id":id,"parentThreadId":parent}}})
        }

        #[test]
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
                        vec![json!({"method":"thread/queue/changed", "params":{}})],
                    ),
                    (
                        "thread/queue/delete",
                        json!({"result":{"deleted":true}}),
                        vec![],
                    ),
                    (
                        "thread/queue/start",
                        json!({"error":{"code":-32000, "message":"thread busy"}}),
                        vec![],
                    ),
                ],
                |client| client.wake("thread").unwrap(),
            );
            assert_eq!(requests[3]["params"]["queuedSubmissionId"], "duplicate");
            assert_eq!(requests[4]["params"]["queuedSubmissionId"], "wake");
        }

        #[test]
        fn fresh_wake_queues_only_sentinel_and_clear_removes_only_wakes() {
            let requests = mock_client(
                vec![
                    (
                        "thread/queue/list",
                        json!({"result":{"data":[], "nextCursor":null}}),
                        vec![],
                    ),
                    (
                        "thread/queue/add",
                        json!({"result":{"queuedSubmission":{"id":"wake"}}}),
                        vec![],
                    ),
                    (
                        "thread/queue/list",
                        json!({"result":{"data":[
                            {"id":"human", "clientUserMessageId":"user"},
                            {"id":"wake", "clientUserMessageId":"hcom-wake-1"}
                        ], "nextCursor":null}}),
                        vec![],
                    ),
                    (
                        "thread/queue/delete",
                        json!({"result":{"deleted":true}}),
                        vec![],
                    ),
                ],
                |client| {
                    client.wake("thread").unwrap();
                    client.clear("thread").unwrap();
                },
            );
            assert_eq!(requests[3]["params"]["input"][0]["text"], "<hcom>");
            assert_eq!(requests[5]["params"]["queuedSubmissionId"], "wake");
        }

        #[test]
        fn selection_follows_new_and_resumed_root_threads_but_not_subagents() {
            mock_client(
                vec![
                    (
                        "thread/queue/list",
                        json!({"result":{"data":[], "nextCursor":null}}),
                        vec![
                            started("first", None),
                            // /new
                            started("second", None),
                            started("child", Some("second")),
                            json!({"method":"thread/started","params":{"thread":{"id":"helper","ephemeral":true}}}),
                            // /resume: loaded without thread/started
                            json!({"method":"thread/status/changed","params":{"threadId":"resumed","status":{"type":"idle"}}}),
                        ],
                    ),
                    (
                        "thread/read",
                        json!({"result":{"thread":{"id":"resumed","parentThreadId":null}}}),
                        vec![],
                    ),
                ],
                |client| {
                    client.wakes("any").unwrap();
                    assert_eq!(client.root.as_deref(), Some("second"));
                    client.pump().unwrap();
                    assert_eq!(client.root.as_deref(), Some("resumed"));
                    client.forget("resumed");
                    assert_eq!(client.root, None);
                },
            );
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_receives_hooks_roots_and_features_without_remote_root_rejection() {
        let (server, tui) = split_args(&[
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
    }

    #[test]
    fn launches_the_private_server_cannot_serve_fall_back() {
        for args in [
            &["--remote=unix:///other"][..],
            &["--no-daemon"],
            &["--profile", "work"],
            &["-pwork"],
            &["--add-dir", "/tmp"],
            &["--worktree"],
            &["--yolo", "fork", "thread"],
            &["resume", "thread", "-s", "read-only"],
            &["fork", "thread", "-c", "approval_policy=never"],
            &["resume", "thread", "-capproval_policy=never"],
        ] {
            assert!(incompatible(args).is_some(), "{args:?}");
        }
        for args in [
            &["-m", "gpt", "--yolo", "--", "--profile"][..],
            &["-a", "never", "-c", "approval_policy=never"],
            // hcom's own writable roots go to the server.
            &[
                "fork",
                "thread",
                "-c",
                "sandbox_workspace_write.writable_roots=[]",
            ],
        ] {
            assert_eq!(incompatible(args), None, "{args:?}");
        }
    }
}
