//! Claude Code hook handler, settings management, and subagent lifecycle.
//!
//! 10 hook types: sessionstart, userpromptsubmit, pre, post, poll,
//! notify, permission-request, subagent-start, subagent-stop, sessionend.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

use crate::bootstrap;
use crate::config::HcomConfig;
use crate::db::{HcomDb, InstanceRow};
use crate::hooks::common;
use crate::hooks::family;
use crate::hooks::{DeliveryAck, HookPayload};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instance_names;
use crate::instances;
use crate::log;
use crate::messages;
use crate::paths;
use crate::shared::constants::BIND_MARKER_RE;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_INACTIVE, ST_LISTENING};

const HOOK_SESSIONSTART: &str = "sessionstart";
const HOOK_USERPROMPTSUBMIT: &str = "userpromptsubmit";
const HOOK_PRE: &str = "pre";
const HOOK_POST: &str = "post";
const HOOK_NOTIFY: &str = "notify";
const HOOK_PERMISSION_REQUEST: &str = "permission-request";
const HOOK_SUBAGENT_START: &str = "subagent-start";
const HOOK_SUBAGENT_STOP: &str = "subagent-stop";
const HOOK_SESSIONEND: &str = "sessionend";
const HOOK_POLL: &str = "poll";

fn is_subagent_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Agent" | "Task")
}

/// Handle a Claude hook — entry point from router.
///
/// Reads JSON from stdin, builds context, dispatches to appropriate handler.
/// Returns exit code (0 = success/non-participant, 2 = message delivered).
///
pub fn dispatch_claude_hook(hook_type: &str) -> i32 {
    let start = Instant::now();

    // Read stdin JSON
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        log::log_error(
            "hooks",
            "claude.stdin_error",
            &format!("hook={} err={}", hook_type, e),
        );
        return 0;
    }

    let raw: Value = match serde_json::from_slice(&input) {
        Ok(v) => v,
        Err(e) => {
            log::log_error(
                "hooks",
                "claude.parse_error",
                &format!("hook={} err={}", hook_type, e),
            );
            return 0;
        }
    };

    // Open DB (includes schema migration/compat check)
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log::log_warn(
                "hooks",
                "claude.db_error",
                &format!("hook={} err={}", hook_type, e),
            );
            return 0;
        }
    };

    // Build context from environment
    let ctx = HcomContext::from_os();

    // Pre-gate: non-participants with empty DB → exit 0, no output
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }

    // Build payload
    let mut payload = HookPayload::from_claude(raw);

    let (exit_code, stdout, delivery_ack, timing) = common::dispatch_with_panic_guard(
        "claude",
        hook_type,
        (0, String::new(), None, DispatchTiming::default()),
        || route_claude_hook(&db, &ctx, hook_type, &mut payload),
    );

    // Output result
    if !stdout.is_empty() {
        let mut writer = std::io::stdout().lock();
        if let Err(e) = write_hook_output(&db, &mut writer, &stdout, delivery_ack.as_ref()) {
            log::log_error(
                "hooks",
                "claude.stdout_error",
                &format!("hook={} err={}", hook_type, e),
            );
        }
    }

    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    let tool_name = payload.tool_name.as_str();
    log::log_info(
        "hooks",
        "claude.dispatch.timing",
        &timing.format(hook_type, tool_name, exit_code, total_ms),
    );

    exit_code
}

fn write_hook_output(
    db: &HcomDb,
    writer: &mut impl std::io::Write,
    stdout: &str,
    delivery_ack: Option<&DeliveryAck>,
) -> std::io::Result<()> {
    writer.write_all(stdout.as_bytes())?;
    writer.flush()?;
    if let Some(ack) = delivery_ack {
        common::commit_delivery_ack(db, ack);
    }
    Ok(())
}

/// Per-stage timing collected during dispatch,
#[derive(Default)]
struct DispatchTiming {
    init_ms: Option<f64>,
    session_ms: Option<f64>,
    resolve_ms: Option<f64>,
    bind_ms: Option<f64>,
    handler_ms: Option<f64>,
    subagent_check_ms: Option<f64>,
    task_ms: Option<f64>,
    instance: Option<String>,
    context: Option<&'static str>,
    result: Option<&'static str>,
}

impl DispatchTiming {
    /// Format timing fields as key=value pairs for log line.
    fn format(&self, hook_type: &str, tool_name: &str, exit_code: i32, total_ms: f64) -> String {
        let instance = self.instance.as_deref();
        let mut parts = vec![format!("hook={}", hook_type)];
        if !tool_name.is_empty() {
            parts.push(format!("tool={}", tool_name));
        }
        if let Some(name) = instance {
            parts.push(format!("instance={}", name));
        }
        if let Some(v) = self.init_ms {
            parts.push(format!("init_ms={:.2}", v));
        }
        if let Some(v) = self.session_ms {
            parts.push(format!("session_ms={:.2}", v));
        }
        if let Some(v) = self.resolve_ms {
            parts.push(format!("resolve_ms={:.2}", v));
        }
        if let Some(v) = self.bind_ms {
            parts.push(format!("bind_ms={:.2}", v));
        }
        if let Some(v) = self.handler_ms {
            parts.push(format!("handler_ms={:.2}", v));
        }
        if let Some(v) = self.subagent_check_ms {
            parts.push(format!("subagent_check_ms={:.2}", v));
        }
        if let Some(v) = self.task_ms {
            parts.push(format!("task_ms={:.2}", v));
        }
        parts.push(format!("total_ms={:.2}", total_ms));
        parts.push(format!("exit_code={}", exit_code));
        if let Some(ctx) = self.context {
            parts.push(format!("context={}", ctx));
        }
        if let Some(r) = self.result {
            parts.push(format!("result={}", r));
        }
        parts.join(" ")
    }
}

/// Core dispatcher — routes to appropriate handler.
///
/// Returns (exit_code, stdout_string, deferred delivery ack, timing).
fn route_claude_hook(
    db: &HcomDb,
    ctx: &HcomContext,
    hook_type: &str,
    payload: &mut HookPayload,
) -> (i32, String, Option<DeliveryAck>, DispatchTiming) {
    let dispatch_start = Instant::now();
    let mut timing = DispatchTiming::default();

    // Ensure directories and init DB
    if !paths::ensure_hcom_directories() {
        return (0, String::new(), None, timing);
    }

    timing.init_ms = Some(dispatch_start.elapsed().as_secs_f64() * 1000.0);

    // Correct session_id (fork bug workaround)
    let session_start = Instant::now();
    let session_id = get_real_session_id(&payload.raw, ctx.claude_env_file.as_deref(), ctx.is_fork);

    // Update payload in place so downstream handlers don't need another raw clone.
    payload.session_id = Some(session_id.clone());
    if let Some(obj) = payload.raw.as_object_mut() {
        obj.insert("session_id".into(), Value::String(session_id.clone()));
    }
    timing.session_ms = Some(session_start.elapsed().as_secs_f64() * 1000.0);

    // SessionStart — no instance resolution needed
    if hook_type == HOOK_SESSIONSTART {
        let handler_start = Instant::now();
        let result = handle_sessionstart(db, ctx, &session_id, &payload.raw);
        timing.handler_ms = Some(handler_start.elapsed().as_secs_f64() * 1000.0);
        return (result.0, result.1, None, timing);
    }

    // Claude identifies the hook's actor with agent_id. All actors share the
    // root session_id, while running_tasks is lifecycle state rather than
    // identity, so actor routing must happen before task-state handling.
    let raw_agent_id = payload
        .raw
        .get("agent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(agent_id) = raw_agent_id {
        // Every Claude subagent carries agent_id on its hooks regardless of
        // whether its root ever ran `hcom start` — that's a property of
        // Claude's hook schema, not of hcom participation. Act only if the
        // shared session_id actually has an hcom root binding; otherwise this
        // whole branch (including the `hcom start --name ...` hint at
        // SubagentStart) must be a silent no-op, same as any other
        // nonparticipant hook.
        let Ok(Some(root_name)) = db.get_session_binding(&session_id) else {
            return (0, String::new(), None, timing);
        };
        let subagent_check_start = Instant::now();
        let (exit_code, stdout, delivery_ack) = route_subagent_actor_hook(
            db,
            hook_type,
            payload,
            &agent_id,
            &session_id,
            &root_name,
            &mut timing,
        );
        timing.subagent_check_ms = Some(subagent_check_start.elapsed().as_secs_f64() * 1000.0);
        return (exit_code, stdout, delivery_ack, timing);
    }

    // ---- Root/main-thread hook (no agent_id) from here on ----

    let tool_name = payload.tool_name.as_str();
    if hook_type == HOOK_PRE && is_subagent_tool(tool_name) {
        let task_start = Instant::now();
        let (exit_code, stdout) = match db.get_session_binding(&session_id).ok().flatten() {
            Some(instance_name) => start_task(db, &instance_name, &session_id, &payload.raw),
            None => (0, String::new()),
        };
        timing.task_ms = Some(task_start.elapsed().as_secs_f64() * 1000.0);
        return (exit_code, stdout, None, timing);
    }
    if hook_type == HOOK_POST && is_subagent_tool(tool_name) {
        let task_start = Instant::now();
        let stdout = match db.get_session_binding(&session_id).ok().flatten() {
            Some(instance_name) => end_task(db, &instance_name, &payload.raw).unwrap_or_default(),
            None => String::new(),
        };
        timing.task_ms = Some(task_start.elapsed().as_secs_f64() * 1000.0);
        return (0, stdout, None, timing);
    }

    if hook_type == HOOK_USERPROMPTSUBMIT {
        // Reap subagents that died without a clean SubagentStop before root
        // delivery. This is lifecycle cleanup, not actor identification.
        let transcript_path = payload.transcript_path.as_deref().unwrap_or("");
        cleanup_dead_subagents(db, &session_id, transcript_path);
        // Fall through to parent handler for PTY message delivery
    }

    // Resolve parent instance
    let resolve_start = Instant::now();
    let (instance_name, updates, _is_matched_resume) = common::init_hook_context(
        db,
        ctx,
        &session_id,
        payload.transcript_path.as_deref().unwrap_or(""),
    );
    timing.resolve_ms = Some(resolve_start.elapsed().as_secs_f64() * 1000.0);

    // Vanilla binding for Bash post hook
    let bind_start = Instant::now();
    let (instance_name, updates) = if hook_type == HOOK_POST && tool_name == "Bash" {
        let bound =
            bind_vanilla_from_marker(db, &payload.raw, &session_id, instance_name.as_deref());
        match bound {
            Some(name) => {
                let mut u = updates;
                u.entry("directory".to_string())
                    .or_insert_with(|| Value::String(ctx.cwd.to_string_lossy().to_string()));
                if let Some(ref tp) = payload.transcript_path {
                    u.entry("transcript_path".to_string())
                        .or_insert_with(|| Value::String(tp.clone()));
                }
                (Some(name), u)
            }
            None => (instance_name, updates),
        }
    } else {
        (instance_name, updates)
    };
    timing.bind_ms = Some(bind_start.elapsed().as_secs_f64() * 1000.0);

    let Some(ref instance_name) = instance_name else {
        timing.result = Some("no_instance");
        return (0, String::new(), None, timing);
    };

    let instance_data = match db.get_instance_full(instance_name) {
        Ok(Some(data)) => data,
        _ => {
            timing.result = Some("no_instance_data");
            return (0, String::new(), None, timing);
        }
    };

    // Dispatch to handler
    timing.instance = Some(instance_name.clone());
    let handler_start = Instant::now();
    let (exit_code, stdout, delivery_ack) = match hook_type {
        HOOK_PRE => {
            let (code, stdout) = handle_pretooluse(db, payload, instance_name);
            (code, stdout, None)
        }
        HOOK_POST => handle_posttooluse(db, ctx, payload, instance_name, &instance_data, &updates),
        HOOK_POLL => handle_poll(db, ctx, instance_name, &instance_data),
        HOOK_NOTIFY => {
            let (code, stdout) = handle_notify(db, payload, instance_name, &updates);
            (code, stdout, None)
        }
        HOOK_PERMISSION_REQUEST => {
            let (code, stdout) = handle_permission_request(db, payload, instance_name, &updates);
            (code, stdout, None)
        }
        HOOK_USERPROMPTSUBMIT => {
            handle_userpromptsubmit(db, ctx, payload, instance_name, &updates, &instance_data)
        }
        HOOK_SESSIONEND => {
            let (code, stdout) =
                handle_sessionend(db, instance_name, &session_id, &payload.raw, &updates);
            (code, stdout, None)
        }
        _ => (0, String::new(), None),
    };
    timing.handler_ms = Some(handler_start.elapsed().as_secs_f64() * 1000.0);

    (exit_code, stdout, delivery_ack, timing)
}

/// Route a hook whose payload carries a non-empty `agent_id` — i.e. one that
/// fired inside a subagent's own execution context. See the call site in
/// `route_claude_hook` for why `agent_id`, not `running_tasks.active`, is the
/// actor signal, and for the participation gate that must run before this
/// (root_name is already a proven hcom binding by the time we're called).
///
/// Returns (exit_code, stdout, delivery_ack).
fn route_subagent_actor_hook(
    db: &HcomDb,
    hook_type: &str,
    payload: &HookPayload,
    agent_id: &str,
    session_id: &str,
    root_name: &str,
    timing: &mut DispatchTiming,
) -> (i32, String, Option<DeliveryAck>) {
    timing.context = Some("subagent");

    if hook_type == HOOK_SUBAGENT_START {
        return handle_subagent_start(db, payload, agent_id, session_id, root_name, timing);
    }

    // SubagentStop resolves its own row and handles a missing row safely.
    if hook_type == HOOK_SUBAGENT_STOP {
        let (exit_code, stdout) = subagent_stop(db, session_id, &payload.raw);
        return (exit_code, stdout, None);
    }

    // Every other subagent hook requires a known actor row. A missing row is
    // not proof that the hook belongs to the root, so fail closed.
    let Ok(Some(subagent_instance)) = db.get_instance_by_agent_id(agent_id) else {
        timing.result = Some("unknown_subagent_actor");
        return (0, String::new(), None);
    };

    if hook_type == HOOK_NOTIFY {
        return (0, String::new(), None);
    }

    // Nested Agent/Task calls mutate the acting subagent, not the shared root.
    let tool_name = payload.tool_name.as_str();
    if hook_type == HOOK_PRE && is_subagent_tool(tool_name) {
        let (exit_code, stdout) = start_task(db, &subagent_instance, session_id, &payload.raw);
        return (exit_code, stdout, None);
    }
    if hook_type == HOOK_POST && is_subagent_tool(tool_name) {
        let stdout = end_task(db, &subagent_instance, &payload.raw).unwrap_or_default();
        return (0, stdout, None);
    }

    if (hook_type == HOOK_PRE || hook_type == HOOK_POST) && tool_name == "Bash" {
        if hook_type == HOOK_POST {
            let command = payload
                .tool_input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // A subagent may only route its own inbox: the --name it passed
            // to `hcom start`/hook delivery must match the agent_id Claude
            // itself stamped on this hook, or one subagent could spoof
            // another's --name and read its inbox (see subagent_posttooluse).
            if let Some(name_flag) = extract_name(command)
                && name_flag == agent_id
            {
                let (exit_code, stdout, delivery_ack) = subagent_posttooluse(db, &payload.raw);
                return (exit_code, stdout, delivery_ack);
            }
        }
        return (0, String::new(), None);
    }

    (0, String::new(), None)
}

fn handle_subagent_start(
    db: &HcomDb,
    payload: &HookPayload,
    agent_id: &str,
    session_id: &str,
    root_name: &str,
    timing: &mut DispatchTiming,
) -> (i32, String, Option<DeliveryAck>) {
    let agent_type = payload
        .raw
        .get("agent_type")
        .and_then(|value| value.as_str())
        .unwrap_or("");

    if !agent_type.is_empty() {
        let parent_instance =
            match resolve_spawn_owner(db, &payload.raw, session_id, agent_id, root_name) {
                SpawnOwner::LegacyRoot(name)
                | SpawnOwner::Resolved(name)
                | SpawnOwner::Resumed(name) => name,
                SpawnOwner::Unresolved => {
                    log::log_warn(
                        "hooks",
                        "subagent.spawn_owner.unresolved",
                        &format!(
                            "agent_id={} prompt_id={:?}",
                            agent_id,
                            payload
                                .raw
                                .get("prompt_id")
                                .and_then(|value| value.as_str())
                        ),
                    );
                    timing.result = Some("spawn_owner_unresolved");
                    return (0, String::new(), None);
                }
            };

        let _ = db.kv_set(
            &agent_owner_kv_key(session_id, agent_id),
            Some(&parent_instance),
        );

        // Allocate the row first, and only track the child once it has one, so
        // a tracked actor is always addressable. Tracking a child whose row
        // allocation failed would leave the parent pointing at a subagent every
        // one of whose later hooks fails closed as an unknown actor.
        if ensure_subagent_row(db, &parent_instance, session_id, agent_id, agent_type) {
            track_subagent(db, &parent_instance, agent_id, agent_type);
        }
    }

    let stdout = build_subagent_start_output(&payload.raw)
        .and_then(|output| serde_json::to_string(&output).ok())
        .unwrap_or_default();
    (0, stdout, None)
}

/// Get correct session_id, handling Claude Code's fork bug.
///
/// For --fork-session, hook_data.session_id reports the parent's old ID.
/// CLAUDE_ENV_FILE path has the correct one: ~/.claude/session-env/{session_id}/hook-N.sh
fn get_real_session_id(raw: &Value, env_file: Option<&str>, is_fork: bool) -> String {
    let hook_session_id = raw
        .get("session_id")
        .or_else(|| raw.get("sessionId"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Also check inside session object
    let hook_session_id = if hook_session_id.is_empty() {
        raw.get("session")
            .and_then(|s| {
                s.get("session_id")
                    .or_else(|| s.get("sessionId"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("")
    } else {
        hook_session_id
    };

    if let Some(env_file) = env_file
        && is_fork
    {
        let path = Path::new(env_file);
        let parts: Vec<&str> = path
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect();
        if let Some(idx) = parts.iter().position(|&p| p == "session-env")
            && idx + 1 < parts.len()
        {
            let candidate = parts[idx + 1];
            // Sanity: UUID format (36 chars, 4 hyphens)
            if candidate.len() == 36 && candidate.chars().filter(|&c| c == '-').count() == 4 {
                log::log_info(
                    "hooks",
                    "get_real_session_id.from_env_file",
                    &format!(
                        "candidate={} hook_session_id={}",
                        candidate, hook_session_id
                    ),
                );
                return candidate.to_string();
            }
        }
    }

    hook_session_id.to_string()
}

/// Handle SessionStart: bind session, inject bootstrap.
fn handle_sessionstart(
    db: &HcomDb,
    ctx: &HcomContext,
    session_id: &str,
    raw: &Value,
) -> (i32, String) {
    let source = raw.get("source").and_then(|v| v.as_str()).unwrap_or("");
    let process_id = ctx.process_id.as_deref();

    log::log_info(
        "hooks",
        "sessionstart.entry",
        &format!(
            "session_id={} source={} process_id={:?} transcript_path={}",
            session_id,
            source,
            process_id,
            raw.get("session")
                .and_then(|s| s.get("transcript_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
        ),
    );

    // Persist session_id to CLAUDE_ENV_FILE for bash commands
    if let Some(ref env_file) = ctx.claude_env_file
        && !session_id.is_empty()
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(env_file)
    {
        use std::io::Write;
        let _ = writeln!(f, "export HCOM_CLAUDE_UNIX_SESSION_ID={}", session_id);
    }

    // Compaction recovery: re-inject bootstrap
    if source == "compact"
        && !session_id.is_empty()
        && let Some(output) = handle_compact_recovery(db, ctx, session_id, process_id)
    {
        return (0, serde_json::to_string(&output).unwrap_or_default());
    }

    // Vanilla instance - show hint
    if process_id.is_none() || session_id.is_empty() {
        let hcom_cmd = crate::runtime_env::build_hcom_command();
        let output = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": format!("[hcom available - run '{} start' to participate]", hcom_cmd),
            }
        });
        return (0, serde_json::to_string(&output).unwrap_or_default());
    }

    // HCOM-launched: bind session and inject bootstrap
    let process_id = process_id.unwrap();
    let mut result_output: Option<Value> = None;

    match bind_and_bootstrap(db, ctx, session_id, process_id) {
        Ok(output) => result_output = output,
        Err(e) => {
            log::log_error(
                "hooks",
                "bind.fail",
                &format!("hook=sessionstart err={}", e),
            );
        }
    }

    // Auto-spawn relay-worker now that an instance is active
    crate::relay::worker::ensure_worker(true);

    let stdout = result_output
        .map(|v| serde_json::to_string(&v).unwrap_or_default())
        .unwrap_or_default();
    (0, stdout)
}

/// Handle compaction recovery (source=compact).
fn handle_compact_recovery(
    db: &HcomDb,
    ctx: &HcomContext,
    session_id: &str,
    process_id: Option<&str>,
) -> Option<Value> {
    let instance_name = db
        .get_session_binding(session_id)
        .ok()
        .flatten()
        .or_else(|| {
            process_id.and_then(|pid| instance_binding::resolve_process_binding(db, Some(pid)))
        })?;

    let bootstrap = if process_id.is_some() {
        // hcom-launched: inject full bootstrap
        let inst = db.get_instance_full(&instance_name).ok()??;
        let tag = inst.tag.as_deref().unwrap_or("");
        bootstrap::get_bootstrap(
            db,
            &ctx.hcom_dir,
            &instance_name,
            "claude",
            ctx.is_background,
            ctx.is_launched,
            &ctx.notes,
            tag,
            false,
            ctx.background_name.as_deref(),
        )
    } else {
        // Vanilla: need rebind
        let mut updates = serde_json::Map::new();
        updates.insert("name_announced".into(), serde_json::json!(false));
        instances::update_instance_position(db, &instance_name, &updates);
        format!(
            "[HCOM RECOVERY] You were participating in hcom as '{}'. \
             Run this command now to continue: hcom start --as {}",
            instance_name, instance_name
        )
    };

    Some(serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": bootstrap,
        }
    }))
}

/// Bind session to process and inject bootstrap for hcom-launched instances.
fn bind_and_bootstrap(
    db: &HcomDb,
    ctx: &HcomContext,
    session_id: &str,
    process_id: &str,
) -> Result<Option<Value>, String> {
    let mut instance_name =
        instance_binding::bind_session_to_process(db, session_id, Some(process_id));

    // Orphaned PTY: process_id exists but no binding (e.g., after /clear)
    if instance_name.is_none() {
        instance_name = instance_binding::create_orphaned_pty_identity(
            db,
            session_id,
            Some(process_id),
            "claude",
        );
        log::log_info(
            "hooks",
            "sessionstart.orphan_created",
            &format!("instance={:?} process_id={}", instance_name, process_id),
        );
    }

    let instance_name = instance_name.ok_or("no instance after bind")?;
    let instance = db
        .get_instance_full(&instance_name)
        .map_err(|e| e.to_string())?
        .ok_or("instance not found after bind")?;

    // Rebind session
    let _ = db.rebind_instance_session(&instance_name, session_id);

    // Capture launch context
    instance_binding::capture_and_store_launch_context(db, &instance_name);

    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );

    crate::runtime_env::set_terminal_title(&instance_name);

    let is_resume = instance.name_announced != 0;
    let tag = instance.tag.as_deref().unwrap_or("");
    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        &ctx.hcom_dir,
        &instance_name,
        "claude",
        ctx.is_background,
        ctx.is_launched,
        &ctx.notes,
        tag,
        false,
        ctx.background_name.as_deref(),
    );

    let result = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": bootstrap_text,
        }
    });

    if !is_resume {
        let mut updates = serde_json::Map::new();
        updates.insert("name_announced".into(), serde_json::json!(true));
        instances::update_instance_position(db, &instance_name, &updates);
        paths::increment_flag_counter("instance_count");
    }

    Ok(Some(result))
}

/// kv-table key correlating a Task/Agent tool call's `prompt_id` — scoped to
/// the root session it happened in — with the instance that made it. See
/// `resolve_spawn_owner`.
///
/// Session-scoped (not just `prompt_id`) because a session's mappings live
/// for the whole session (see `end_task` / `handle_sessionend` for why they
/// can't be deleted per-Task-completion), and prompt_id values are only
/// unique within one Claude conversation.
fn spawn_owner_kv_key(root_session_id: &str, prompt_id: &str) -> String {
    format!("spawn_owner:{root_session_id}:{prompt_id}")
}

/// kv-table key prefix covering every spawn-owner mapping for one root
/// session. See `handle_sessionend`.
fn spawn_owner_kv_prefix(root_session_id: &str) -> String {
    format!("spawn_owner:{root_session_id}:")
}

/// kv-table key remembering which instance owns a given `agent_id`, scoped
/// to the root session — independent of any one `prompt_id`. See
/// `resolve_spawn_owner`'s resume fallback.
fn agent_owner_kv_key(root_session_id: &str, agent_id: &str) -> String {
    format!("agent_owner:{root_session_id}:{agent_id}")
}

/// kv-table key prefix covering every agent-owner mapping for one root
/// session. See `handle_sessionend`.
fn agent_owner_kv_prefix(root_session_id: &str) -> String {
    format!("agent_owner:{root_session_id}:")
}

/// Outcome of resolving which instance spawned a SubagentStart's child.
enum SpawnOwner {
    /// `prompt_id` was absent from the payload entirely — Claude Code
    /// < 2.1.196, where this correlation doesn't exist on the wire at all.
    /// Root attribution is the only thing ever knowable without it; this is
    /// not nested-spawn support, just the honest pre-2.1.196 behavior.
    LegacyRoot(String),
    /// `prompt_id` was present and resolved to a live owner that's verified
    /// to belong to this root session.
    Resolved(String),
    /// `prompt_id` was present but didn't resolve, and this `agent_id` was
    /// already known (see `agent_owner_kv_key`) — Claude resuming a
    /// previously-spawned subagent under the same `agent_id` stamps a *new*
    /// `prompt_id` on the resumed SubagentStart with no corresponding
    /// PreToolUse to map it, so the fresh-`prompt_id` lookup always misses on
    /// resume. Falling back to this agent_id's own
    /// remembered owner is safe specifically because the agent_id is
    /// already known — see `Unresolved` for why an *unknown* agent_id must
    /// not get the same treatment.
    Resumed(String),
    /// `prompt_id` was present but no valid owner resolved by either path,
    /// and this `agent_id` has never been seen before: no mapping
    /// (correlation failed or hasn't landed yet), or the mapped instance is
    /// gone or belongs to a different session (stale/foreign). Root is
    /// deliberately not used as a fallback here — on a Claude Code version
    /// that *does* send `prompt_id`, a miss on a genuinely new agent_id
    /// means something is wrong, not that root is a safe guess.
    Unresolved,
}

/// Resolve which instance actually spawned a SubagentStart's child.
///
/// Claude stamps the same `prompt_id` on a Task/Agent tool's PreToolUse and
/// on the SubagentStart(s) it produces, including parallel siblings spawned
/// by the same call. `start_task` records
/// `(root_session_id, prompt_id) -> acting instance` for exactly this
/// lookup, so a SubagentStart spawned by a *nested* subagent resolves to
/// that subagent, not the session-bound root — parent and every nested
/// subagent share one Claude session_id, so session_id alone can never tell
/// them apart.
///
/// Falls back to `agent_owner_kv_key(root_session_id, agent_id)` — this
/// `agent_id`'s own remembered owner from when it was first spawned — when
/// the `prompt_id` lookup misses; see `SpawnOwner::Resumed`.
fn resolve_spawn_owner(
    db: &HcomDb,
    raw: &Value,
    root_session_id: &str,
    agent_id: &str,
    root_name: &str,
) -> SpawnOwner {
    let Some(prompt_id) = raw
        .get("prompt_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return SpawnOwner::LegacyRoot(root_name.to_string());
    };

    let by_prompt = db
        .kv_get(&spawn_owner_kv_key(root_session_id, prompt_id))
        .ok()
        .flatten();
    if let Some(owner) = by_prompt
        && validate_spawn_owner(db, &owner, root_session_id, root_name)
    {
        return SpawnOwner::Resolved(owner);
    }

    let by_agent = db
        .kv_get(&agent_owner_kv_key(root_session_id, agent_id))
        .ok()
        .flatten();
    if let Some(owner) = by_agent
        && validate_spawn_owner(db, &owner, root_session_id, root_name)
    {
        return SpawnOwner::Resumed(owner);
    }

    SpawnOwner::Unresolved
}

/// Verify a resolved spawn-owner instance actually belongs to this root
/// session's hierarchy, rather than trusting the kv mapping blindly — a
/// stale entry could name an instance that's since been deleted or (in
/// principle) reused for something else.
fn validate_spawn_owner(db: &HcomDb, owner: &str, root_session_id: &str, root_name: &str) -> bool {
    if owner == root_name {
        return true;
    }
    match db.get_instance_full(owner) {
        Ok(Some(data)) => data.parent_session_id.as_deref() == Some(root_session_id),
        _ => false,
    }
}

/// PreToolUse Task: enter subagent context for `instance_name` — the acting
/// instance's own row (root parent, or the spawning subagent for a nested
/// Agent/Task call; see the call sites in `route_claude_hook` /
/// `route_subagent_actor_hook`).
///
/// Returns (exit_code, stdout).
fn start_task(
    db: &HcomDb,
    instance_name: &str,
    root_session_id: &str,
    raw: &Value,
) -> (i32, String) {
    log::log_info(
        "hooks",
        "start_task.enter",
        &format!("instance={}", instance_name),
    );

    instances::mutate_running_tasks(db, instance_name, |rt| {
        rt.active = true;
    });

    if let Some(prompt_id) = raw
        .get("prompt_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        let _ = db.kv_set(
            &spawn_owner_kv_key(root_session_id, prompt_id),
            Some(instance_name),
        );
    }

    let tool_input = raw
        .get("tool_input")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let detail = family::extract_tool_detail("claude", "Task", &tool_input);
    lifecycle::set_status(
        db,
        instance_name,
        ST_ACTIVE,
        "tool:Task",
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    );

    // Append hcom instructions to Task prompt
    let original_prompt = tool_input
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !original_prompt.is_empty() {
        let hcom_cmd = crate::runtime_env::build_hcom_command();
        let hcom_hint =
            format!("\n\n---\nTo use hcom: run `{hcom_cmd} start --name <your-agent-id>` first.");
        let mut updated = tool_input.clone();
        if let Some(obj) = updated.as_object_mut() {
            obj.insert(
                "prompt".into(),
                Value::String(format!("{}{}", original_prompt, hcom_hint)),
            );
        }
        let output = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": updated,
            }
        });
        return (0, serde_json::to_string(&output).unwrap_or_default());
    }

    (0, String::new())
}

/// PostToolUse Task: deliver freeze-period messages for `instance_name` — the
/// acting instance's own row (see `start_task`).
///
/// Returns Option<String> — JSON stdout if messages were delivered.
/// Dispatcher writes this to stdout before returning exit code.
fn end_task(db: &HcomDb, instance_name: &str, raw: &Value) -> Option<String> {
    // Since Claude Code 2.1.198, Agent/Task calls background by default: this
    // PostToolUse fires immediately with tool_response.status="async_launched"
    // when the call is merely dispatched to the background, not when the
    // subagent actually finishes. Treating that as completion would emit a
    // false "Subagents have finished and are no longer active" summary and
    // advance last_event_id before the subagent's freeze window is over, so
    // skip delivery rather than assume anything about how/when completion is
    // reported — root-scoped messages still flow through the root's own
    // interleaved PostToolUse hooks regardless (see route_claude_hook).
    let is_async_launch = raw
        .get("tool_response")
        .and_then(|v| v.get("status"))
        .and_then(|v| v.as_str())
        == Some("async_launched");
    if is_async_launch {
        return None;
    }

    let instance_data = match db.get_instance_full(instance_name) {
        Ok(Some(data)) => data,
        _ => return None,
    };

    let freeze_event_id = instance_data.last_event_id;
    let (last_event_id, stdout) = deliver_freeze_messages(db, instance_name, freeze_event_id);

    let mut updates = serde_json::Map::new();
    updates.insert("last_event_id".into(), serde_json::json!(last_event_id));
    instances::update_instance_position(db, instance_name, &updates);

    stdout
}

/// Deliver messages from Task freeze period.
///
/// Returns (last_event_id, Option<stdout_json>). Caller writes stdout.
fn deliver_freeze_messages(
    db: &HcomDb,
    instance_name: &str,
    freeze_event_id: i64,
) -> (i64, Option<String>) {
    let events = match db.get_events_since(freeze_event_id, Some("message"), None) {
        Ok(events) => events,
        Err(_) => return (freeze_event_id, None),
    };

    if events.is_empty() {
        return (freeze_event_id, None);
    }

    let last_id = events
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_i64()))
        .max()
        .unwrap_or(freeze_event_id);

    // Get subagents for message filtering
    let subagent_rows: Vec<(String, Option<String>)> = db
        .conn()
        .prepare("SELECT name, agent_id FROM instances WHERE parent_name = ?")
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![instance_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let subagent_names: Vec<&str> = subagent_rows.iter().map(|(n, _)| n.as_str()).collect();

    // Filter messages
    let mut subagent_msgs: Vec<Value> = Vec::new();
    let mut parent_msgs: Vec<Value> = Vec::new();

    for event in &events {
        let event_data = match event.get("data") {
            Some(d) => d,
            None => continue,
        };
        let sender_name = event_data
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let timestamp = event
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let text = event_data
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let msg = serde_json::json!({
            "timestamp": timestamp,
            "from": sender_name,
            "message": text,
        });

        if subagent_names.contains(&sender_name) {
            subagent_msgs.push(msg);
        } else if !subagent_names.is_empty()
            && subagent_names.iter().any(|name| {
                match messages::should_deliver_message(event_data, name, sender_name) {
                    Ok(v) => v,
                    Err(e) => {
                        log::log_warn(
                            "hooks",
                            "claude.should_deliver_message",
                            &format!("target={} sender={} err={}", name, sender_name, e),
                        );
                        false
                    }
                }
            })
        {
            if !subagent_msgs.contains(&msg) {
                subagent_msgs.push(msg);
            }
        } else {
            match messages::should_deliver_message(event_data, instance_name, sender_name) {
                Ok(true) => parent_msgs.push(msg),
                Ok(false) => {}
                Err(e) => {
                    log::log_warn(
                        "hooks",
                        "claude.should_deliver_message",
                        &format!("target={} sender={} err={}", instance_name, sender_name, e),
                    );
                }
            }
        }
    }

    let mut all_relevant: Vec<Value> = Vec::new();
    all_relevant.extend(subagent_msgs);
    all_relevant.extend(parent_msgs);
    all_relevant.sort_by(|a, b| {
        let ta = a.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let tb = b.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        ta.cmp(tb)
    });

    if all_relevant.is_empty() {
        return (last_id, None);
    }

    let formatted: Vec<String> = all_relevant
        .iter()
        .map(|m| {
            format!(
                "{}: {}",
                m.get("from").and_then(|v| v.as_str()).unwrap_or("?"),
                m.get("message").and_then(|v| v.as_str()).unwrap_or("")
            )
        })
        .collect();

    let subagent_list = if subagent_rows.is_empty() {
        "none".to_string()
    } else {
        subagent_rows
            .iter()
            .map(|(name, agent_id)| {
                if let Some(aid) = agent_id {
                    format!("{} (agent_id: {})", name, aid)
                } else {
                    name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };

    let summary = format!(
        "[Task tool completed - Message history during Task tool]\n\
         Subagents: {}\n\
         The following {} message(s) occurred:\n\n\
         {}\n\n\
         [End of message history. Subagents have finished and are no longer active.]",
        subagent_list,
        all_relevant.len(),
        formatted.join("\n"),
    );

    let output = serde_json::json!({
        "systemMessage": "[Task subagent messages shown to instance]",
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": summary,
        },
    });

    (
        last_id,
        Some(serde_json::to_string(&output).unwrap_or_default()),
    )
}

/// Parent PreToolUse: status tracking with tool-specific detail.
fn handle_pretooluse(db: &HcomDb, payload: &HookPayload, instance_name: &str) -> (i32, String) {
    let tool_name = payload.tool_name.as_str();
    let tool_input = &payload.tool_input;

    // Skip status update for Claude's internal memory operations
    if tool_name == "Edit" || tool_name == "Write" {
        let detail = family::extract_tool_detail("claude", tool_name, tool_input);
        if detail.contains("session-memory/") {
            return (0, String::new());
        }
    }

    common::update_tool_status(db, instance_name, "claude", tool_name, tool_input);
    (0, String::new())
}

/// Parent PostToolUse: bootstrap, messages, vanilla binding.
fn handle_posttooluse(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
    instance_name: &str,
    instance_data: &InstanceRow,
    updates: &serde_json::Map<String, Value>,
) -> (i32, String, Option<DeliveryAck>) {
    let tool_name = payload.tool_name.as_str();
    let mut outputs: Vec<Value> = Vec::new();
    let mut delivery_ack = None;

    // Clear blocked status if tool completed
    if instance_data.status == ST_BLOCKED {
        lifecycle::set_status(
            db,
            instance_name,
            ST_ACTIVE,
            &format!("approved:{}", tool_name),
            Default::default(),
        );
    }

    // Bash-specific: persist updates and check bootstrap
    if tool_name == "Bash" {
        if !updates.is_empty() {
            instances::update_instance_position(db, instance_name, updates);
        }
        if let Some(output) = inject_bootstrap_if_needed(db, ctx, instance_name, instance_data) {
            outputs.push(output);
        }
    }

    // Message delivery for ALL tools (parent only)
    if let Some((output, ack)) = get_posttooluse_messages(db, instance_name) {
        outputs.push(output);
        delivery_ack = Some(ack);
    }

    if !outputs.is_empty() {
        let combined = combine_posttooluse_outputs(&outputs);
        return (
            0,
            serde_json::to_string(&combined).unwrap_or_default(),
            delivery_ack,
        );
    }

    (0, String::new(), None)
}

/// Defensive fallback bootstrap injection at PostToolUse.
fn inject_bootstrap_if_needed(
    db: &HcomDb,
    ctx: &HcomContext,
    instance_name: &str,
    instance_data: &InstanceRow,
) -> Option<Value> {
    let bootstrap = common::inject_bootstrap_once(db, ctx, instance_name, instance_data, "claude")?;

    paths::increment_flag_counter("instance_count");

    Some(serde_json::json!({
        "systemMessage": "[HCOM info shown to instance]",
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": bootstrap,
        },
    }))
}

/// Check for unread messages to deliver at PostToolUse.
fn get_posttooluse_messages(db: &HcomDb, instance_name: &str) -> Option<(Value, DeliveryAck)> {
    let prepared = common::prepare_pending_messages(db, instance_name)?;
    let model_context =
        common::format_messages_json_for_instance(db, &prepared.messages, instance_name);

    // Claude needs user-facing display in addition to model context
    let user_display =
        common::format_hook_messages_for_instance(db, &prepared.messages, instance_name);

    // Grok loads Claude-compat hooks but does not honor Claude's
    // hookSpecificOutput/additionalContext on PostToolUse the same way.
    // Emit followup_message (Grok binary accepts it) so mid-turn delivery works.
    if common::is_grok_host() {
        return Some((
            serde_json::json!({
                "followup_message": model_context,
                "additional_context": model_context,
                "systemMessage": user_display,
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": model_context,
                },
            }),
            prepared.ack,
        ));
    }

    Some((
        serde_json::json!({
            "systemMessage": user_display,
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": model_context,
            },
        }),
        prepared.ack,
    ))
}

/// Combine multiple PostToolUse outputs with \n\n---\n\n separator.
fn combine_posttooluse_outputs(outputs: &[Value]) -> Value {
    if outputs.len() == 1 {
        return outputs[0].clone();
    }

    let system_msgs: Vec<&str> = outputs
        .iter()
        .filter_map(|o| o.get("systemMessage").and_then(|v| v.as_str()))
        .collect();
    let combined_system = if system_msgs.is_empty() {
        None
    } else {
        Some(system_msgs.join(" + "))
    };

    let contexts: Vec<&str> = outputs
        .iter()
        .filter_map(|o| {
            o.get("hookSpecificOutput")
                .and_then(|h| h.get("additionalContext"))
                .and_then(|v| v.as_str())
        })
        .collect();
    let combined_context = contexts.join("\n\n---\n\n");

    let mut result = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": combined_context,
        }
    });

    if let Some(sys) = combined_system {
        result
            .as_object_mut()
            .unwrap()
            .insert("systemMessage".into(), Value::String(sys));
    }

    result
}

/// Poll hook: message delivery when Claude goes idle.
fn handle_poll(
    db: &HcomDb,
    ctx: &HcomContext,
    instance_name: &str,
    instance_data: &InstanceRow,
) -> (i32, String, Option<DeliveryAck>) {
    log::log_info(
        "hooks",
        "stop.enter",
        &format!(
            "instance={} is_headless={} pty_mode={} grok_host={}",
            instance_name,
            ctx.is_background,
            ctx.is_pty_mode,
            common::is_grok_host()
        ),
    );

    // Grok Build loads Claude-compat Stop hooks but treats Stop as passive:
    // exit-2 + decision:block does NOT re-prompt. Deliver via followup_message
    // (one-shot — never long-poll under Grok or Stop hangs for wait_timeout).
    if common::is_grok_host() {
        return handle_poll_for_grok(db, instance_name);
    }

    // PTY mode: exit immediately, PTY wrapper handles injection
    if ctx.is_pty_mode {
        lifecycle::set_status(db, instance_name, ST_LISTENING, "", Default::default());
        common::notify_hook_instance_with_db(db, instance_name);
        return (0, String::new(), None);
    }

    // Non-PTY: poll for messages
    let wait_timeout = instance_data.wait_timeout;
    let timeout = wait_timeout.unwrap_or_else(HcomConfig::effective_timeout);

    // Persist effective timeout
    let mut updates = serde_json::Map::new();
    updates.insert("wait_timeout".into(), serde_json::json!(timeout));
    instances::update_instance_position(db, instance_name, &updates);

    let (exit_code, output, timed_out) =
        common::poll_messages(db, instance_name, timeout as u64, ctx.is_background);

    if timed_out {
        lifecycle::set_status(
            db,
            instance_name,
            ST_INACTIVE,
            "exit:timeout",
            Default::default(),
        );
    }

    let stdout = output
        .map(|v| serde_json::to_string(&v).unwrap_or_default())
        .unwrap_or_default();
    // poll_messages already commits the delivery ack before returning.
    (exit_code, stdout, None)
}

/// One-shot Stop delivery for Grok Build (Claude-compat hook path).
///
/// Grok ignores Claude's exit-2 / `decision:block` Stop semantics. Use
/// `followup_message` (same field Cursor uses; present in the Grok binary) and
/// defer cursor advance until stdout is flushed by the dispatcher.
fn handle_poll_for_grok(db: &HcomDb, instance_name: &str) -> (i32, String, Option<DeliveryAck>) {
    lifecycle::set_status(db, instance_name, ST_LISTENING, "", Default::default());
    common::notify_hook_instance_with_db(db, instance_name);

    match common::prepare_pending_messages(db, instance_name) {
        Some(prepared) => {
            log::log_info(
                "hooks",
                "stop.grok_followup",
                &format!(
                    "instance={} bytes={}",
                    instance_name,
                    prepared.formatted.len()
                ),
            );
            let stdout = serde_json::json!({
                "followup_message": prepared.formatted,
            })
            .to_string();
            (0, stdout, Some(prepared.ack))
        }
        None => (0, String::new(), None),
    }
}

/// Parent UserPromptSubmit: fallback bootstrap, PTY mode message delivery.
fn handle_userpromptsubmit(
    db: &HcomDb,
    ctx: &HcomContext,
    _payload: &HookPayload,
    instance_name: &str,
    updates: &serde_json::Map<String, Value>,
    instance_data: &InstanceRow,
) -> (i32, String, Option<DeliveryAck>) {
    let name_announced = instance_data.name_announced != 0;

    // Persist updates
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, updates);
    }

    // Bootstrap fallback (rarely fires)
    if !name_announced
        && ctx.is_launched
        && let Some(bootstrap_text) =
            common::inject_bootstrap_once(db, ctx, instance_name, instance_data, "claude")
    {
        let output = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": bootstrap_text,
            }
        });
        paths::increment_flag_counter("instance_count");
        lifecycle::set_status(db, instance_name, ST_ACTIVE, "prompt", Default::default());
        return (0, serde_json::to_string(&output).unwrap_or_default(), None);
    }

    // PTY mode, or Grok host (Grok strips HCOM_PTY_MODE from hook env so
    // is_pty_mode is false even for hcom-launched sessions): deliver pending.
    if (ctx.is_pty_mode || common::is_grok_host())
        && let Some(prepared) = common::prepare_pending_messages(db, instance_name)
    {
        let user_display =
            common::format_hook_messages_for_instance(db, &prepared.messages, instance_name);
        let model_context =
            common::format_messages_json_for_instance(db, &prepared.messages, instance_name);

        if common::is_grok_host() {
            // Grok loads Claude-compat UPS *and* native grok-userpromptsubmit.
            // Emitting followup from both races into two queued turns. Native
            // grok owns delivery (full-body skip-ack or bare-wake followup).
            // Claude-compat only acks when the PTY full-body path already put
            // the payload in the user prompt (safe no-op if native acked first).
            let prompt = _payload
                .raw
                .get("prompt")
                .and_then(|v| v.as_str())
                .or_else(|| _payload.raw.get("userPrompt").and_then(|v| v.as_str()))
                .unwrap_or("");
            if common::prompt_already_carries_hcom_body(prompt, &model_context)
                || common::prompt_already_carries_hcom_body(prompt, &user_display)
            {
                log::log_info(
                    "hooks",
                    "userpromptsubmit.grok_skip_followup",
                    &format!(
                        "instance={} prompt already carries body (bytes={})",
                        instance_name,
                        prompt.len()
                    ),
                );
                return (0, "{}".to_string(), Some(prepared.ack));
            }
            log::log_info(
                "hooks",
                "userpromptsubmit.grok_defer_to_native",
                &format!(
                    "instance={} bare/wake prompt — native grok UPS owns followup",
                    instance_name
                ),
            );
            // Do not prepare-ack here: leave pending for native grok UPS.
            // We already called prepare_pending_messages above — that does NOT
            // advance cursor; only commit_delivery_ack does. Drop ack.
            return (0, "{}".to_string(), None);
        }

        let output = serde_json::json!({
            "systemMessage": user_display,
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": model_context,
            },
        });
        return (
            0,
            serde_json::to_string(&output).unwrap_or_default(),
            Some(prepared.ack),
        );
    }

    lifecycle::set_status(db, instance_name, ST_ACTIVE, "prompt", Default::default());
    (0, String::new(), None)
}

/// Parent PermissionRequest: mark instance blocked immediately on approval UI.
fn handle_permission_request(
    db: &HcomDb,
    payload: &HookPayload,
    instance_name: &str,
    updates: &serde_json::Map<String, Value>,
) -> (i32, String) {
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, updates);
    }

    let detail = family::extract_tool_detail("claude", &payload.tool_name, &payload.tool_input);
    lifecycle::set_status(
        db,
        instance_name,
        ST_BLOCKED,
        "approval",
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    );
    (0, String::new())
}

/// Parent Notification: map Claude notification types to hcom lifecycle state.
fn handle_notify(
    db: &HcomDb,
    payload: &HookPayload,
    instance_name: &str,
    updates: &serde_json::Map<String, Value>,
) -> (i32, String) {
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, updates);
    }

    let message = payload
        .raw
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match payload.notification_type.as_deref() {
        Some("idle_prompt") => {
            lifecycle::set_status(db, instance_name, ST_LISTENING, "", Default::default());
            common::notify_hook_instance_with_db(db, instance_name);
            return (0, String::new());
        }
        Some("permission_prompt") => {
            // PermissionRequest owns blocked state and carries tool detail.
            return (0, String::new());
        }
        Some("elicitation_dialog") => {
            lifecycle::set_status(
                db,
                instance_name,
                ST_BLOCKED,
                "approval",
                Default::default(),
            );
            return (0, String::new());
        }
        Some("auth_success") => return (0, String::new()),
        Some(other) => {
            log::log_warn(
                "hooks",
                "claude.notify.unknown_type",
                &format!("instance={} notification_type={}", instance_name, other),
            );
        }
        None => {}
    }

    // Back-compat fallback for older Claude payloads that only include free-form text.
    if message == "Claude is waiting for your input" {
        lifecycle::set_status(db, instance_name, ST_LISTENING, "", Default::default());
        common::notify_hook_instance_with_db(db, instance_name);
        return (0, String::new());
    }
    if message.starts_with("Claude needs your permission") {
        lifecycle::set_status(
            db,
            instance_name,
            ST_BLOCKED,
            "approval",
            Default::default(),
        );
        return (0, String::new());
    }
    (0, String::new())
}

/// Parent SessionEnd: finalize session and stop instance.
fn handle_sessionend(
    db: &HcomDb,
    instance_name: &str,
    session_id: &str,
    raw: &Value,
    updates: &serde_json::Map<String, Value>,
) -> (i32, String) {
    let reason = raw
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    common::finalize_session(
        db,
        instance_name,
        reason,
        if updates.is_empty() {
            None
        } else {
            Some(updates)
        },
    );

    // Root session is over: no more children can spawn under any of its
    // prompt_id mappings (see `spawn_owner_kv_key`), no further resumes can
    // arrive for its agent_ids (see `agent_owner_kv_key`), and no further
    // SubagentStop invocations can fire for it (see `subagent_stop_inflight_key`).
    // None of these can be cleaned up incrementally as they're used (parallel
    // siblings sharing one prompt_id would race a later SubagentStart against
    // an earlier sibling's cleanup — see `end_task`; a resumed agent_id needs
    // its mapping to *outlive* its own stop), so the whole session's worth of
    // each is swept here instead. Inflight stop records are transient and
    // self-recover when their owning process dies, but sweeping them avoids
    // retaining unreachable session data.
    for (label, prefix) in [
        ("spawn_owner", spawn_owner_kv_prefix(session_id)),
        ("agent_owner", agent_owner_kv_prefix(session_id)),
        (
            "subagent_stop_inflight",
            subagent_stop_inflight_prefix(session_id),
        ),
    ] {
        if let Err(e) = db.kv_delete_prefix(&prefix) {
            log::log_warn(
                "hooks",
                "sessionend.kv_cleanup_failed",
                &format!("kind={} session_id={} err={}", label, session_id, e),
            );
        }
    }

    (0, String::new())
}

/// Track subagent in `parent_instance`'s running_tasks. `parent_instance` is
/// the already-resolved spawning actor (root or a nested subagent — see
/// `resolve_spawn_owner`), not derived from session_id here.
fn track_subagent(db: &HcomDb, parent_instance: &str, agent_id: &str, agent_type: &str) {
    log::log_info(
        "hooks",
        "track_subagent.enter",
        &format!(
            "parent={} agent_id={} agent_type={}",
            parent_instance, agent_id, agent_type
        ),
    );

    // Dedup-check-and-insert happens inside the transaction (see
    // `mutate_running_tasks`), so concurrent SubagentStart hooks for sibling
    // subagents of the same parent (parallel Task calls) cannot race and
    // silently drop each other's entry.
    instances::mutate_running_tasks(db, parent_instance, |rt| {
        rt.track_subagent(agent_id, agent_type);
    });
}

/// Remove subagent from parent's running_tasks.
fn remove_subagent_from_parent(db: &HcomDb, parent_name: &str, agent_id: &str) {
    instances::mutate_running_tasks(db, parent_name, |rt| {
        rt.remove_subagent(agent_id);
    });
}

/// Maximum depth when searching for nested subagent transcripts.
///
/// Claude Code's `setAgentTranscriptSubdir` takes the subdir key unsanitized
/// (runAgent.ts:321-352), and the documented example is `workflows/<runId>`
/// which creates `subagents/workflows/<runId>/agent-*.jsonl`. Bound the
/// search at 4 levels so a pathological subdir key can't turn the scan
/// into an unbounded filesystem walk.
const MAX_SUBAGENT_TRANSCRIPT_DEPTH: u32 = 4;

/// Locate a subagent transcript when it isn't at the flat
/// `subagents/agent-{id}.jsonl`. Walks subdirectories up to
/// `MAX_SUBAGENT_TRANSCRIPT_DEPTH` levels looking for `agent-{id}.jsonl`.
fn find_subagent_transcript(subagent_dir: &Path, agent_id: &str) -> Option<PathBuf> {
    let target = format!("agent-{}.jsonl", agent_id);
    find_subagent_transcript_impl(subagent_dir, &target, 0)
}

fn find_subagent_transcript_impl(dir: &Path, target: &str, depth: u32) -> Option<PathBuf> {
    if depth >= MAX_SUBAGENT_TRANSCRIPT_DEPTH {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let sub = entry.path();
        let candidate = sub.join(target);
        if candidate.is_file() {
            return Some(candidate);
        }
        if let Some(found) = find_subagent_transcript_impl(&sub, target, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// Check for dead subagents by checking multiple death signals.
fn check_dead_subagents(
    db: &HcomDb,
    transcript_path: &str,
    running_tasks: &instances::RunningTasks,
    subagent_timeout: Option<i64>,
) -> Vec<String> {
    let timeout = subagent_timeout.unwrap_or_else(|| {
        HcomConfig::load(None)
            .ok()
            .map(|c| c.subagent_timeout)
            .unwrap_or(120)
    });
    let stale_threshold = (timeout * 2) as u64;
    // Claude Code stores subagent transcripts under
    // `{projectDir}/{sessionId}/subagents/agent-{agentId}.jsonl`
    // (claude-code/src/utils/sessionStorage.ts getAgentTranscriptPath).
    // Parent's transcript_path is `{projectDir}/{sessionId}.jsonl`; strip the
    // `.jsonl` suffix to get the directory that holds `subagents/`.
    let subagent_dir = if transcript_path.is_empty() {
        None
    } else {
        transcript_path
            .strip_suffix(".jsonl")
            .map(|stem| PathBuf::from(stem).join("subagents"))
    };

    let mut dead = Vec::new();
    let now = crate::shared::time::now_epoch_i64() as u64;

    for subagent in &running_tasks.subagents {
        let agent_id = match subagent.get("agent_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => continue,
        };

        // Check: instance deleted from DB
        let exists = db
            .conn()
            .query_row(
                "SELECT 1 FROM instances WHERE agent_id = ?",
                rusqlite::params![agent_id],
                |_| Ok(()),
            )
            .is_ok();
        if !exists {
            dead.push(agent_id.to_string());
            continue;
        }

        let Some(ref subagent_dir) = subagent_dir else {
            // No parent transcript path — skip rather than declare dead on
            // missing signal.
            continue;
        };

        // Primary path: flat `subagents/agent-{id}.jsonl`.
        // Fallback: workflow subagents live under `subagents/<subdir>/agent-{id}.jsonl`
        // (claude-code/src/utils/sessionStorage.ts:231-257 agentTranscriptSubdirs).
        // If the flat path misses, look one level deeper before giving up.
        let flat = subagent_dir.join(format!("agent-{}.jsonl", agent_id));
        let agent_transcript = if flat.is_file() {
            flat
        } else {
            find_subagent_transcript(subagent_dir, agent_id).unwrap_or(flat)
        };
        match std::fs::metadata(&agent_transcript) {
            Err(_) => {
                // Transcript not yet written (brand-new subagent) or layout
                // differs. A missing file is not evidence of death — skip
                // and rely on DB-row-missing / stale-mtime / marker checks
                // on the next tick.
                continue;
            }
            Ok(meta) => {
                // Stale check
                if let Ok(modified) = meta.modified() {
                    let mtime = modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if now.saturating_sub(mtime) > stale_threshold {
                        dead.push(agent_id.to_string());
                        continue;
                    }
                }

                // Check last 4KB for interrupt marker
                if let Ok(mut f) = std::fs::File::open(&agent_transcript) {
                    use std::io::{Read, Seek, SeekFrom};
                    let size = f.seek(SeekFrom::End(0)).unwrap_or(0);
                    let read_from = size.saturating_sub(4096);
                    let _ = f.seek(SeekFrom::Start(read_from));
                    let mut buf = Vec::new();
                    let _ = f.read_to_end(&mut buf);
                    let tail = String::from_utf8_lossy(&buf);
                    if tail.contains("[Request interrupted by user]") {
                        dead.push(agent_id.to_string());
                    }
                }
            }
        }
    }

    dead
}

/// Clean up dead subagents from parent's running_tasks.
fn cleanup_dead_subagents(db: &HcomDb, session_id: &str, transcript_path: &str) {
    let instance_name = match db.get_session_binding(session_id) {
        Ok(Some(name)) => name,
        _ => return,
    };

    let instance_data = match db.get_instance_full(&instance_name) {
        Ok(Some(data)) => data,
        _ => return,
    };

    let running_tasks = instances::parse_running_tasks(instance_data.running_tasks.as_deref());
    if running_tasks.subagents.is_empty() {
        return;
    }

    let dead_ids = check_dead_subagents(
        db,
        transcript_path,
        &running_tasks,
        instance_data.subagent_timeout,
    );
    if dead_ids.is_empty() {
        return;
    }

    for agent_id in &dead_ids {
        remove_subagent_from_parent(db, &instance_name, agent_id);

        // Stop the subagent instance if it exists
        if let Ok(Some(name)) = db.get_instance_by_agent_id(agent_id) {
            lifecycle::set_status(
                db,
                &name,
                ST_INACTIVE,
                "exit:interrupted",
                Default::default(),
            );
            common::stop_instance(db, &name, "system", "interrupted");
        }
    }
}

/// SubagentStart: surface agent_id to subagent.
fn build_subagent_start_output(raw: &Value) -> Option<Value> {
    let agent_id = raw.get("agent_id").and_then(|v| v.as_str())?;
    if agent_id.is_empty() {
        return None;
    }

    let hcom_cmd = crate::runtime_env::build_hcom_command();
    let additional_context = format!(
        "Your agent ID: {agent_id}\n\
         To use hcom, first run: {hcom_cmd} start --name {agent_id}"
    );

    Some(serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SubagentStart",
            "additionalContext": additional_context,
        }
    }))
}

/// Allocate (or adopt) an `instances` row for this subagent at SubagentStart,
/// so it's visible in the TUI and addressable by `hcom send` without any
/// change to the subagent's own context. The row stays dormant
/// (status_context=`subagent:dormant`, name_announced=0) until SubagentStop
/// activates it or the subagent runs `hcom start --name <agent_id>`.
///
/// Idempotent: calls into `allocate_subagent_instance`, which returns the
/// existing row's name if one already exists for this agent_id.
///
/// `parent_instance` (the row's `parent_name`) is the already-resolved
/// spawning actor, which may be a nested subagent — see
/// `resolve_spawn_owner`. `root_session_id` is always the real Claude
/// session_id (the root's own), never a subagent's, because subagent rows
/// never get their own `session_id` (see `allocate_subagent_instance`) — it
/// is the only value the `parent_session_id` FK column (`REFERENCES
/// instances(session_id)`) can ever validly point at, at any nesting depth.
/// Allocate this subagent's `instances` row. Returns `true` once a row exists
/// and the actor is addressable (allocation is idempotent, so an already-present
/// row also counts), `false` if the parent is unknown or allocation failed.
fn ensure_subagent_row(
    db: &HcomDb,
    parent_instance: &str,
    root_session_id: &str,
    agent_id: &str,
    agent_type: &str,
) -> bool {
    let parent_data = match db.get_instance_full(parent_instance) {
        Ok(Some(data)) => data,
        _ => return false,
    };
    let parent_tag = parent_data.tag.as_deref();

    let alloc = instance_names::SubagentAllocation {
        agent_id,
        agent_type,
        parent_name: parent_instance,
        parent_session_id: Some(root_session_id),
        parent_tag,
        status: ST_INACTIVE,
        status_context: Some("subagent:dormant"),
    };

    match instance_names::allocate_subagent_instance(db, &alloc) {
        Ok(name) => {
            log::log_info(
                "hooks",
                "subagent.row.ensured",
                &format!(
                    "name={} parent={} agent_id={} type={}",
                    name, parent_instance, agent_id, agent_type
                ),
            );
            true
        }
        Err(e) => {
            log::log_warn(
                "hooks",
                "subagent.row.alloc_failed",
                &format!("agent_id={} err={}", agent_id, e),
            );
            false
        }
    }
}

/// Deterministic, bounded fingerprint of a raw hook payload for use inside a
/// SQLite key. Payloads can carry unbounded fields (e.g. a transcript
/// excerpt), so the payload itself must never be embedded directly into a
/// key — hash it instead. Not for cryptographic use, just content-addressing
/// two invocations as "the same bytes" or not.
fn hash_raw_payload(raw: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(raw.to_string().as_bytes());
    let hash = hasher.finalize();
    hash.iter().map(|b| format!("{b:02x}")).take(16).collect()
}

/// kv-table key claiming exactly-once processing for *one specific*
/// SubagentStop invocation — not the agent_id alone, and not `prompt_id`
/// alone either. Claude legitimately re-fires SubagentStop for the same
/// agent_id (and, as far as we've established, possibly the same
/// `prompt_id` too — SubagentStop is explicitly designed to fire again after
/// an exit_code=2 delivery within the same agent prompt/continuation, and we
/// have not confirmed Claude changes `prompt_id` for that re-fire) across the
/// message-poll loop and on resume, so a claim keyed on either alone risks
/// wrongly suppressing a later, genuinely different stop. The full raw
/// payload is hashed instead: two hook registrations firing the *identical*
/// event (e.g. hcom installed in both global and repo Claude settings, seen
/// live) produce byte-identical stdin and collapse onto the same key, while
/// any real difference between invocations (delivered message content,
/// background/transcript state) changes the hash and gets its own key.
/// `prompt_id` is folded in only as a readable, non-load-bearing component.
fn subagent_stop_inflight_key(root_session_id: &str, agent_id: &str, raw: &Value) -> String {
    let prompt_id = raw.get("prompt_id").and_then(|v| v.as_str()).unwrap_or("");
    format!(
        "subagent_stop_inflight:{root_session_id}:{agent_id}:{prompt_id}:{}",
        hash_raw_payload(raw)
    )
}

/// kv-table key prefix covering every SubagentStop claim for one root
/// session. See `handle_sessionend`.
fn subagent_stop_inflight_prefix(root_session_id: &str) -> String {
    format!("subagent_stop_inflight:{root_session_id}:")
}

/// Transient claim that prevents concurrent duplicate SubagentStop hooks from
/// polling or tearing down the same actor at the same time. Persisting the PID
/// plus its start identity makes a claim recoverable after SIGKILL without
/// mistaking a reused PID for the original owner.
#[derive(Deserialize, Serialize)]
struct SubagentStopOwner {
    owner_token: String,
    pid: u32,
    process_start: String,
}

struct SubagentStopClaim<'a> {
    db: &'a HcomDb,
    key: String,
    value: String,
}

impl<'a> SubagentStopClaim<'a> {
    fn acquire(db: &'a HcomDb, root_session_id: &str, agent_id: &str, raw: &Value) -> Option<Self> {
        let key = subagent_stop_inflight_key(root_session_id, agent_id, raw);
        let pid = std::process::id();
        let process_start = match crate::sys::process::identity(pid) {
            Some(identity) => identity,
            None => {
                log::log_warn(
                    "hooks",
                    "subagent_stop.claim_identity_failed",
                    &format!("pid={pid}"),
                );
                return None;
            }
        };
        let owner = SubagentStopOwner {
            owner_token: uuid::Uuid::new_v4().to_string(),
            pid,
            process_start,
        };
        let value = serde_json::to_string(&owner).ok()?;

        match db.conn().execute(
            "INSERT OR IGNORE INTO kv (key, value) VALUES (?, ?)",
            rusqlite::params![key, value],
        ) {
            Ok(1) => return Some(Self { db, key, value }),
            Ok(_) => {}
            Err(e) => {
                log::log_warn(
                    "hooks",
                    "subagent_stop.claim_insert_failed",
                    &format!("key={key} err={e}"),
                );
                return None;
            }
        }

        let old_value = db.kv_get(&key).ok()??;
        if serde_json::from_str::<SubagentStopOwner>(&old_value)
            .is_ok_and(|old| crate::sys::process::has_identity(old.pid, &old.process_start))
        {
            return None;
        }

        // Replace a dead owner's record only if it is still the exact value we
        // inspected. Another contender may have recovered it first.
        match db.conn().execute(
            "UPDATE kv SET value = ? WHERE key = ? AND value = ?",
            rusqlite::params![value, key, old_value],
        ) {
            Ok(1) => Some(Self { db, key, value }),
            Ok(_) => None,
            Err(e) => {
                log::log_warn(
                    "hooks",
                    "subagent_stop.claim_replace_failed",
                    &format!("key={key} err={e}"),
                );
                None
            }
        }
    }
}

impl Drop for SubagentStopClaim<'_> {
    fn drop(&mut self) {
        let _ = self.db.conn().execute(
            "DELETE FROM kv WHERE key = ? AND value = ?",
            rusqlite::params![self.key, self.value],
        );
    }
}

/// SubagentStop: message polling using agent_id, cleanup on exit.
///
/// Returns (exit_code, stdout). exit_code=2 means message delivered
/// (SubagentStop fires again). exit_code=0 means cleanup and stop.
fn subagent_stop(db: &HcomDb, root_session_id: &str, raw: &Value) -> (i32, String) {
    let agent_id = match raw.get("agent_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return (0, String::new()),
    };

    // Claim before reading the row. Otherwise duplicate hook processes can all
    // observe the row, wait for the first claim to be released, and then each
    // perform a second teardown against their stale copy of that row.
    let Some(_stop_claim) = SubagentStopClaim::acquire(db, root_session_id, agent_id, raw) else {
        return (0, String::new());
    };

    // Query subagent instance by agent_id
    let row: Option<(String, String, Option<String>, i64)> = db
        .conn()
        .query_row(
            "SELECT name, transcript_path, parent_name, name_announced \
             FROM instances WHERE agent_id = ?",
            rusqlite::params![agent_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                ))
            },
        )
        .ok();

    let Some((subagent_name, existing_transcript, parent_name, name_announced)) = row else {
        // No instance = SubagentStart never allocated one (shouldn't happen for
        // in-ctx subagents, but kept as a defensive fallback). The tracking
        // entry could be on the session-bound root *or* a nested subagent
        // that spawned this one (see `resolve_spawn_owner`) — scan for
        // whichever instance actually tracks `agent_id` rather than assuming
        // root, or the true owner is left wedged active forever.
        instances::remove_tracked_subagent_by_agent_id(db, agent_id);
        return (0, String::new());
    };

    // Store transcript_path if not already set
    if existing_transcript.is_empty()
        && let Some(tp) = raw.get("agent_transcript_path").and_then(|v| v.as_str())
        && !tp.is_empty()
    {
        let mut updates = serde_json::Map::new();
        updates.insert("transcript_path".into(), Value::String(tp.to_string()));
        instances::update_instance_position(db, &subagent_name, &updates);
    }

    // Idle gate: a dormant subagent (never opted in via `hcom start`, never
    // had a message delivered) only wakes for *direct* mentions. Broadcasts
    // are visible to its row but are not enough to keep it alive — that
    // would break the "no message in → no keep-alive" contract, since
    // SubagentStart now puts every subagent in the broadcast recipient set.
    let dormant = name_announced == 0;
    let has_direct = dormant && db.has_direct_unread(&subagent_name);
    if dormant && !has_direct {
        lifecycle::set_status(
            db,
            &subagent_name,
            ST_INACTIVE,
            "exit:idle",
            Default::default(),
        );
        if let Some(ref pn) = parent_name {
            remove_subagent_from_parent(db, pn, agent_id);
        }
        common::stop_instance(db, &subagent_name, "subagent", "idle");
        return (0, String::new());
    }

    // Activation: dormant + a direct mention pending. Build the bootstrap
    // text now but DO NOT flip `name_announced` yet — only mark the row
    // announced once `poll_messages` actually returns a delivery. Otherwise
    // a transient poll failure (orphan stdin closed, row deleted mid-loop)
    // would burn the one-shot bootstrap with nothing to show for it.
    let activation_bootstrap = if dormant {
        let parent_display = match parent_name.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                log::log_warn(
                    "hooks",
                    "subagent.activation.parent_missing",
                    &format!("name={subagent_name} agent_id={agent_id}"),
                );
                ""
            }
        };
        let bs = bootstrap::get_subagent_bootstrap(&subagent_name, parent_display);
        if bs.is_empty() { None } else { Some(bs) }
    } else {
        None
    };

    // Resolve timeout: parent override > global config
    let timeout = parent_name
        .as_ref()
        .and_then(|pn| db.get_instance_full(pn).ok().flatten())
        .and_then(|pd| pd.subagent_timeout)
        .unwrap_or_else(|| {
            HcomConfig::load(None)
                .ok()
                .map(|c| c.subagent_timeout)
                .unwrap_or(120)
        }) as u64;

    let (exit_code, output, timed_out) = common::poll_messages(db, &subagent_name, timeout, false);

    // On first-activation delivery, prepend the subagent bootstrap to the
    // `reason` field so Claude injects both as a single user message on the
    // subagent's next turn. We only mark `name_announced=true` here, after
    // `poll_messages` confirmed a delivery — if it returned (0, None, _)
    // the bootstrap is preserved for the next SubagentStop.
    let stdout = match (&output, activation_bootstrap.as_deref()) {
        (Some(Value::Object(obj)), Some(bs)) => {
            let mut munged = obj.clone();
            let existing_reason = munged
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let combined = if existing_reason.is_empty() {
                bs.to_string()
            } else {
                format!("{bs}\n\n{existing_reason}")
            };
            munged.insert("reason".into(), Value::String(combined));
            let mut updates = serde_json::Map::new();
            updates.insert("name_announced".into(), serde_json::json!(true));
            instances::update_instance_position(db, &subagent_name, &updates);
            serde_json::to_string(&Value::Object(munged)).unwrap_or_default()
        }
        (Some(v), _) => serde_json::to_string(v).unwrap_or_default(),
        (None, _) => String::new(),
    };

    // exit_code=2: message delivered, subagent continues
    // exit_code=0: no message/timeout, cleanup
    if exit_code == 0 {
        let reason = if timed_out {
            "timeout"
        } else {
            "task_completed"
        };
        lifecycle::set_status(
            db,
            &subagent_name,
            ST_INACTIVE,
            &format!("exit:{}", reason),
            Default::default(),
        );
        if let Some(ref pn) = parent_name {
            remove_subagent_from_parent(db, pn, agent_id);
        }
        common::stop_instance(db, &subagent_name, "subagent", reason);
    }

    (exit_code, stdout)
}

/// Subagent PostToolUse: message delivery for subagents running hcom commands.
///
/// Returns (exit_code, stdout).
fn subagent_posttooluse(db: &HcomDb, raw: &Value) -> (i32, String, Option<DeliveryAck>) {
    let tool_input = raw.get("tool_input").unwrap_or(&Value::Null);
    let tool_name = raw.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");

    if tool_name != "Bash" {
        return (0, String::new(), None);
    }

    let command = tool_input
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !command.contains("--name") {
        return (0, String::new(), None);
    }

    let agent_id = match extract_name(command) {
        Some(name) => name,
        None => return (0, String::new(), None),
    };

    let subagent_name = match db.get_instance_by_agent_id(&agent_id) {
        Ok(Some(name)) => name,
        _ => return (0, String::new(), None),
    };

    // Check instance exists (row exists = participating)
    let _data = match db.get_instance_full(&subagent_name) {
        Ok(Some(data)) => data,
        _ => return (0, String::new(), None),
    };

    // Message delivery
    let Some(prepared) = common::prepare_pending_messages(db, &subagent_name) else {
        return (0, String::new(), None);
    };
    let formatted =
        common::format_messages_json_for_instance(db, &prepared.messages, &subagent_name);

    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": formatted,
        }
    });
    (
        0,
        serde_json::to_string(&output).unwrap_or_default(),
        Some(prepared.ack),
    )
}

/// Detect and process vanilla instance binding from `hcom start` output.
fn bind_vanilla_from_marker(
    db: &HcomDb,
    raw: &Value,
    session_id: &str,
    current_instance: Option<&str>,
) -> Option<String> {
    // Skip if no pending instances
    let pending = common::get_pending_instances(db);
    if pending.is_empty() {
        return None;
    }

    let tool_response = raw.get("tool_response")?;
    let response_text = if tool_response.is_string() {
        tool_response.as_str().unwrap_or("").to_string()
    } else if tool_response.is_object() {
        tool_response
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        return None;
    };

    if response_text.is_empty() {
        return None;
    }

    let caps = BIND_MARKER_RE.captures(&response_text)?;
    let instance_name = caps.get(1)?.as_str();

    // Don't rebind if already bound to a different instance
    if let Some(current) = current_instance
        && current != instance_name
    {
        return None;
    }

    if session_id.is_empty() {
        return current_instance
            .map(|s| s.to_string())
            .or_else(|| Some(instance_name.to_string()));
    }

    // Verify instance exists and is pending (session_id IS NULL)
    let inst = db.get_instance_full(instance_name).ok()??;
    if inst.session_id.is_some() {
        return None; // Already bound
    }

    if let Err(e) = db.rebind_instance_session(instance_name, session_id) {
        log::log_error(
            "hooks",
            "bind.fail",
            &format!("instance={} err={}", instance_name, e),
        );
        return None;
    }

    log::log_info(
        "hooks",
        "bind.session",
        &format!("instance={} session_id={}", instance_name, session_id),
    );

    let mut updates = serde_json::Map::new();
    updates.insert("session_id".into(), Value::String(session_id.to_string()));
    updates.insert("tool".into(), Value::String("claude".to_string()));
    instances::update_instance_position(db, instance_name, &updates);

    Some(instance_name.to_string())
}

/// Extract --name flag value from a bash command string.
fn extract_name(command: &str) -> Option<String> {
    RE_NAME_FLAG
        .captures(command)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

//
// Manages hook installation in ~/.claude/settings.json.

use super::common::SAFE_HCOM_COMMANDS;

/// Hook configuration: (hook_type, matcher, command_suffix, timeout_secs).
/// Single source of truth — all hook properties derived from this.
const CLAUDE_HOOK_CONFIGS: &[(&str, &str, &str, Option<u64>)] = &[
    ("SessionStart", "", "sessionstart", None),
    ("UserPromptSubmit", "", "userpromptsubmit", None),
    ("PreToolUse", "Bash|Task|Write|Edit", "pre", None),
    ("PostToolUse", "", "post", Some(86400)),
    ("Stop", "", "poll", Some(86400)),
    ("PermissionRequest", "", "permission-request", None),
    ("SubagentStart", "", "subagent-start", None),
    ("SubagentStop", "", "subagent-stop", Some(86400)),
    ("Notification", "", "notify", None),
    ("SessionEnd", "", "sessionend", None),
];

/// Hook command suffixes for pattern detection.
const CLAUDE_HOOK_COMMANDS: &[&str] = &[
    "sessionstart",
    "userpromptsubmit",
    "pre",
    "post",
    "poll",
    "permission-request",
    "subagent-start",
    "subagent-stop",
    "notify",
    "sessionend",
];

/// Claude hook types for cleanup iteration.
const CLAUDE_HOOK_TYPES: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "PermissionRequest",
    "SubagentStart",
    "SubagentStop",
    "Notification",
    "SessionEnd",
];

// Static regexes for hot-path hook command detection
static RE_NAME_FLAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"--name\s+(\S+)").unwrap());
static RE_HCOM_COMMANDS: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = CLAUDE_HOOK_COMMANDS.join("|");
    Regex::new(&format!(r"\bhcom\s+({})\b", pattern)).unwrap()
});
static RE_HCOM_CLAUDE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bhcom\s+claude-").unwrap());
static RE_UVX_HCOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\buvx\s+hcom\s+claude-").unwrap());
static RE_HCOM_ACTIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bHCOM_ACTIVE.*hcom\.py").unwrap());
static RE_HCOM_PY_COMMANDS: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = CLAUDE_HOOK_COMMANDS.join("|");
    Regex::new(&format!(r#"hcom\.py["']?\s+({})\b"#, pattern)).unwrap()
});
static RE_SH_HCOM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"sh\s+-c.*hcom").unwrap());

/// Resolve the Claude config directory.
///
/// Priority: CLAUDE_CONFIG_DIR env var → tool_config_root()/.claude
fn claude_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    paths::get_project_root().join(".claude")
}

/// Get path to Claude settings.json.
///
/// Respects CLAUDE_CONFIG_DIR env var, then falls back to:
/// - HCOM_DIR set → project_root is HCOM_DIR parent → {parent}/.claude/settings.json
/// - Otherwise → ~/.hcom parent = ~ → ~/.claude/settings.json
pub fn get_claude_settings_path() -> PathBuf {
    claude_config_dir().join("settings.json")
}

/// Load and parse Claude settings.json. Returns None on error or missing file.
pub fn load_claude_settings(settings_path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(settings_path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Build a hook command that silently exits 0 when hcom is not installed.
///
/// Claude already executes hook commands through a shell, so this command keeps
/// all shell logic inline instead of spawning another `sh -c`. It uses the
/// ${HCOM:-hcom} env var (set in settings.json env block) so it works for both
/// direct `hcom` and `uvx hcom` invocations. When the binary is absent (e.g.
/// after `brew uninstall hcom`), the hook exits 0 instead of emitting a "command
/// not found" error inside the tool.
fn build_hook_entry_command(cmd_suffix: &str) -> String {
    // Claude runs hook commands through a POSIX shell on every platform
    // (Git Bash on Windows), so the same command works everywhere. The
    // `${HCOM:-hcom}` default plus the `command -v` guard make it silently
    // exit 0 when hcom isn't on PATH.
    format!(
        "cmd=${{HCOM:-hcom}}; command -v \"${{cmd%% *}}\" >/dev/null 2>&1 && exec $cmd {} || exit 0",
        cmd_suffix
    )
}

/// Format a single Claude permission pattern: `Bash(prefix cmd:*)`.
fn format_claude_permission(prefix: &str, cmd: &str) -> String {
    let suffix = if cmd.starts_with('-') { "" } else { ":*" };
    format!("Bash({} {}{})", prefix, cmd, suffix)
}

/// Format a single Claude permission pattern for the PowerShell tool:
/// `PowerShell(prefix cmd:*)`.
///
/// Claude Code uses the Bash tool on Windows when Git for Windows is present,
/// and falls back to a separate PowerShell tool (same rule syntax as Bash)
/// otherwise, so both patterns are installed to cover either case.
fn format_claude_powershell_permission(prefix: &str, cmd: &str) -> String {
    let suffix = if cmd.starts_with('-') { "" } else { ":*" };
    format!("PowerShell({} {}{})", prefix, cmd, suffix)
}

/// Build permission patterns for installation using detected prefix.
fn build_claude_permissions() -> Vec<String> {
    let prefix = crate::runtime_env::build_hcom_command();
    SAFE_HCOM_COMMANDS
        .iter()
        .map(|cmd| format_claude_permission(&prefix, cmd))
        .chain(
            SAFE_HCOM_COMMANDS
                .iter()
                .map(|cmd| format_claude_powershell_permission(&prefix, cmd)),
        )
        .collect()
}

/// Legacy commands that were once in SAFE_HCOM_COMMANDS or auto-approved.
/// Kept here so removal cleans up permissions from older installs.
const LEGACY_HCOM_COMMANDS: &[&str] = &["daemon"];

/// Build ALL permission patterns (both "hcom" and "uvx hcom" prefixes) for removal.
fn build_all_claude_permission_patterns() -> Vec<String> {
    let mut patterns = Vec::new();
    for prefix in &["hcom", "uvx hcom"] {
        for cmd in SAFE_HCOM_COMMANDS.iter().chain(LEGACY_HCOM_COMMANDS.iter()) {
            patterns.push(format_claude_permission(prefix, cmd));
            patterns.push(format_claude_powershell_permission(prefix, cmd));
        }
    }
    patterns
}

/// Check if a hook command string matches any hcom hook pattern.
fn is_hcom_hook_command(command: &str) -> bool {
    // Env var patterns: ${HCOM} or %HCOM%
    if command.contains("${HCOM}")
        || command.contains("$HCOM")
        || command.contains("%HCOM%")
        || command.contains("${HCOM:-")
    {
        return true;
    }

    // Standard patterns: hcom <hook_command>
    if RE_HCOM_COMMANDS.is_match(command) {
        return true;
    }

    // Tool prefix pattern: hcom claude-
    if RE_HCOM_CLAUDE.is_match(command) {
        return true;
    }

    // uvx pattern: uvx hcom claude-
    if RE_UVX_HCOM.is_match(command) {
        return true;
    }

    // Legacy patterns
    if RE_HCOM_ACTIVE.is_match(command) {
        return true;
    }
    if command.contains(r#"IF "%HCOM_ACTIVE%""#) {
        return true;
    }
    if RE_HCOM_PY_COMMANDS.is_match(command) {
        return true;
    }
    if RE_SH_HCOM.is_match(command) {
        return true;
    }

    false
}

/// Remove all hcom hooks from a Claude settings dictionary (in-place).
///
/// Scans all hook types and removes hooks whose command matches hcom patterns.
/// Also removes HCOM from env and hcom permission patterns from permissions.allow.
/// Returns true if any hooks/env/permissions were removed.
fn remove_hcom_hooks_from_settings(settings: &mut Value) -> bool {
    let mut removed_any = false;

    let obj = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    // Process each hook type (if hooks section exists)
    if let Some(hooks) = obj.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        for event in CLAUDE_HOOK_TYPES {
            let event_matchers = match hooks.get_mut(*event) {
                Some(v) => v,
                None => continue,
            };

            let matchers = match event_matchers.as_array_mut() {
                Some(a) => a,
                None => {
                    // Malformed event value (not a list) — skip and preserve
                    continue;
                }
            };

            let mut updated_matchers = Vec::new();
            for matcher in matchers.iter() {
                let matcher_obj = match matcher.as_object() {
                    Some(o) => o,
                    None => {
                        // Malformed matcher — preserve as-is
                        updated_matchers.push(matcher.clone());
                        continue;
                    }
                };

                let hooks_field = match matcher_obj.get("hooks") {
                    Some(v) => match v.as_array() {
                        Some(a) => a,
                        None => {
                            // Malformed hooks field — preserve
                            updated_matchers.push(matcher.clone());
                            continue;
                        }
                    },
                    None => {
                        // No hooks field — preserve matcher
                        updated_matchers.push(matcher.clone());
                        continue;
                    }
                };

                // Filter out hcom hooks
                let non_hcom_hooks: Vec<&Value> = hooks_field
                    .iter()
                    .filter(|hook| {
                        let command = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        !is_hcom_hook_command(command)
                    })
                    .collect();

                if non_hcom_hooks.len() < hooks_field.len() {
                    removed_any = true;
                }

                // Only keep matcher if it has non-hcom hooks remaining
                if !non_hcom_hooks.is_empty() {
                    let mut matcher_copy = matcher.clone();
                    matcher_copy["hooks"] =
                        Value::Array(non_hcom_hooks.into_iter().cloned().collect());
                    updated_matchers.push(matcher_copy);
                }
                // If all hooks were hcom, drop the entire matcher
            }

            if updated_matchers.is_empty() {
                hooks.remove(*event);
            } else {
                hooks.insert(event.to_string(), Value::Array(updated_matchers));
            }
        }
    }

    // Remove HCOM from env section
    if let Some(env) = obj.get_mut("env").and_then(|v| v.as_object_mut()) {
        if env.remove("HCOM").is_some() {
            removed_any = true;
        }
        if env.is_empty() {
            obj.remove("env");
        }
    }

    // Remove hcom permission patterns
    if let Some(perms) = obj.get_mut("permissions").and_then(|v| v.as_object_mut()) {
        if let Some(allow) = perms.get_mut("allow").and_then(|v| v.as_array_mut()) {
            let all_patterns = build_all_claude_permission_patterns();
            let original_len = allow.len();
            allow.retain(|p| {
                let s = p.as_str().unwrap_or("");
                !all_patterns.iter().any(|pat| pat == s)
            });
            if allow.len() < original_len {
                removed_any = true;
            }
            if allow.is_empty() {
                perms.remove("allow");
            }
        }
        if perms.is_empty() {
            obj.remove("permissions");
        }
    }

    removed_any
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum VerifyFailReason {
    #[error("settings.json missing or not parseable as JSON")]
    SettingsUnreadable,
    #[error("'hooks' key missing or not an object")]
    HooksKeyMissing,
    #[error("hook type '{0}' missing or empty")]
    HookTypeMissing(String),
    #[error("hcom hook command '{cmd_suffix}' not found under hook type '{hook_type}'")]
    HookCommandMissing {
        hook_type: String,
        cmd_suffix: String,
    },
    #[error("hook type '{hook_type}' matcher mismatch: expected {expected:?}, got {actual:?}")]
    HookMatcherMismatch {
        hook_type: String,
        expected: String,
        actual: String,
    },
    #[error(
        "hook type '{hook_type}' has no numeric 'timeout' field (canonical): expected a numeric timeout for a canonically-bounded hook"
    )]
    HookTimeoutMissing { hook_type: String },
    #[error("duplicate hcom hook entry for hook type '{0}'")]
    HookDuplicated(String),
    #[error("HCOM env var not set in settings.json")]
    HcomEnvMissing,
    #[error("'permissions.allow' missing or not an array")]
    PermissionsAllowMissing,
    #[error("required permission pattern not present: {0}")]
    PermissionMissing(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("JSON serialization failed: {0}")]
    SerializationFailed(#[from] serde_json::Error),
    #[error("atomic write to {} failed: {source}", path.display())]
    AtomicWriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("post-write verify failed for {}: {reason}", path.display())]
    PostWriteVerifyFailed {
        path: PathBuf,
        #[source]
        reason: VerifyFailReason,
    },
}

/// Set up hcom hooks in Claude settings.json.
///
/// - Removes existing hcom hooks first (clean slate)
/// - Adds all hooks from CLAUDE_HOOK_CONFIGS
/// - Sets HCOM environment variable
/// - Optionally adds permission patterns
/// - Uses atomic write for concurrent safety
pub fn try_setup_claude_hooks(include_permissions: bool) -> Result<(), SetupError> {
    let settings_path = get_claude_settings_path();
    if let Some(parent) = settings_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut settings =
        load_claude_settings(&settings_path).unwrap_or_else(|| serde_json::json!({}));

    // Normalize hooks dict
    if !settings.get("hooks").is_some_and(|v| v.is_object()) {
        settings["hooks"] = serde_json::json!({});
    }

    // Remove existing hcom hooks
    remove_hcom_hooks_from_settings(&mut settings);

    for &(hook_type, matcher, cmd_suffix, timeout) in CLAUDE_HOOK_CONFIGS {
        // Initialize or normalize hook_type to array
        if !settings["hooks"]
            .get(hook_type)
            .is_some_and(|v| v.is_array())
        {
            settings["hooks"][hook_type] = serde_json::json!([]);
        }

        let mut hook_entry = serde_json::json!({
            "type": "command",
            "command": build_hook_entry_command(cmd_suffix),
        });

        if let Some(t) = timeout {
            hook_entry["timeout"] = serde_json::json!(t);
        }

        let mut hook_dict = serde_json::json!({
            "hooks": [hook_entry],
        });

        if !matcher.is_empty() {
            hook_dict["matcher"] = Value::String(matcher.to_string());
        }

        settings["hooks"][hook_type]
            .as_array_mut()
            .unwrap()
            .push(hook_dict);
    }

    // Set $HCOM environment variable
    if !settings.get("env").is_some_and(|v| v.is_object()) {
        settings["env"] = serde_json::json!({});
    }
    settings["env"]["HCOM"] = Value::String(crate::runtime_env::build_hcom_command());
    // Remove stale HCOM_DIR from settings
    if let Some(env) = settings["env"].as_object_mut() {
        env.remove("HCOM_DIR");
    }

    // Handle permission patterns
    if include_permissions {
        if !settings.get("permissions").is_some_and(|v| v.is_object()) {
            settings["permissions"] = serde_json::json!({});
        }
        if !settings["permissions"]
            .get("allow")
            .is_some_and(|v| v.is_array())
        {
            settings["permissions"]["allow"] = serde_json::json!([]);
        }
        if let Some(allow) = settings["permissions"]["allow"].as_array_mut() {
            for pattern in build_claude_permissions() {
                if !allow.iter().any(|p| p.as_str() == Some(&pattern)) {
                    allow.push(Value::String(pattern));
                }
            }
        }
    } else {
        // Remove hcom permissions if disabled
        if let Some(perms) = settings
            .get_mut("permissions")
            .and_then(|v| v.as_object_mut())
        {
            if let Some(allow) = perms.get_mut("allow").and_then(|v| v.as_array_mut()) {
                let hcom_perms = build_all_claude_permission_patterns();
                allow.retain(|p| {
                    let s = p.as_str().unwrap_or("");
                    !hcom_perms.iter().any(|pat| pat == s)
                });
                if allow.is_empty() {
                    perms.remove("allow");
                }
            }
            if perms.is_empty() {
                settings.as_object_mut().unwrap().remove("permissions");
            }
        }
    }

    let json_str =
        serde_json::to_string_pretty(&settings).map_err(SetupError::SerializationFailed)?;

    paths::atomic_write_io(&settings_path, &json_str).map_err(|e| {
        SetupError::AtomicWriteFailed {
            path: settings_path.clone(),
            source: e,
        }
    })?;

    // Re-read from disk: catches truncation, FS-layer corruption, and
    // concurrent overwrite by another process between rename and verify.
    verify_claude_hooks_inner(Some(&settings_path), include_permissions).map_err(|reason| {
        SetupError::PostWriteVerifyFailed {
            path: settings_path,
            reason,
        }
    })?;

    Ok(())
}

pub fn setup_claude_hooks(include_permissions: bool) -> bool {
    try_setup_claude_hooks(include_permissions).is_ok()
}

/// Verify hcom hooks are installed in Claude settings. Every hook that
/// carries a timeout in `CLAUDE_HOOK_CONFIGS` must have a numeric `timeout`
/// field — the value itself is not checked, so user edits still pass.
pub fn verify_claude_hooks_installed(
    settings_path: Option<&Path>,
    check_permissions: bool,
) -> bool {
    verify_claude_hooks_inner(settings_path, check_permissions).is_ok()
}

fn verify_claude_hooks_inner(
    settings_path: Option<&Path>,
    check_permissions: bool,
) -> Result<(), VerifyFailReason> {
    let default_path = get_claude_settings_path();
    let path = settings_path.unwrap_or(&default_path);

    let settings = load_claude_settings(path).ok_or(VerifyFailReason::SettingsUnreadable)?;

    let hooks = settings
        .get("hooks")
        .and_then(|v| v.as_object())
        .ok_or(VerifyFailReason::HooksKeyMissing)?;

    for &(hook_type, expected_matcher, cmd_suffix, expected_timeout) in CLAUDE_HOOK_CONFIGS {
        let hook_matchers = match hooks.get(hook_type).and_then(|v| v.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return Err(VerifyFailReason::HookTypeMissing(hook_type.to_string())),
        };

        let mut hcom_hook_found = false;
        for matcher_dict in hook_matchers {
            let matcher_obj = match matcher_dict.as_object() {
                Some(o) => o,
                None => continue,
            };
            let hooks_list = match matcher_obj.get("hooks").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => continue,
            };

            for hook in hooks_list {
                let command = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let has_hcom =
                    command.contains("${HCOM}") || command.to_lowercase().contains("hcom");
                if has_hcom && command.contains(cmd_suffix) {
                    if hcom_hook_found {
                        return Err(VerifyFailReason::HookDuplicated(hook_type.to_string()));
                    }

                    if expected_timeout.is_some()
                        && hook.get("timeout").and_then(|v| v.as_u64()).is_none()
                    {
                        return Err(VerifyFailReason::HookTimeoutMissing {
                            hook_type: hook_type.to_string(),
                        });
                    }

                    let actual_matcher = matcher_obj
                        .get("matcher")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if actual_matcher != expected_matcher {
                        return Err(VerifyFailReason::HookMatcherMismatch {
                            hook_type: hook_type.to_string(),
                            expected: expected_matcher.to_string(),
                            actual: actual_matcher.to_string(),
                        });
                    }

                    hcom_hook_found = true;
                }
            }
        }

        if !hcom_hook_found {
            return Err(VerifyFailReason::HookCommandMissing {
                hook_type: hook_type.to_string(),
                cmd_suffix: cmd_suffix.to_string(),
            });
        }
    }

    if settings.get("env").and_then(|v| v.get("HCOM")).is_none() {
        return Err(VerifyFailReason::HcomEnvMissing);
    }

    if check_permissions {
        let allow = settings
            .get("permissions")
            .and_then(|v| v.get("allow"))
            .and_then(|v| v.as_array())
            .ok_or(VerifyFailReason::PermissionsAllowMissing)?;
        for pattern in build_claude_permissions() {
            if !allow.iter().any(|p| p.as_str() == Some(&pattern)) {
                return Err(VerifyFailReason::PermissionMissing(pattern));
            }
        }
    }

    Ok(())
}

/// Remove hcom hooks from a specific settings path. Returns true on success.
fn remove_hooks_from_settings_path(settings_path: &Path) -> bool {
    if !settings_path.exists() {
        return true;
    }

    let mut settings = match load_claude_settings(settings_path) {
        Some(s) => s,
        None => return true, // Empty/missing is fine
    };

    if !settings.is_object() {
        return true;
    }

    remove_hcom_hooks_from_settings(&mut settings);

    let json_str = match serde_json::to_string_pretty(&settings) {
        Ok(s) => s,
        Err(_) => return false,
    };

    paths::atomic_write(settings_path, &json_str)
}

/// Remove hcom hooks from Claude settings.
///
/// Cleans both global (~/.claude/settings.json) and local (HCOM_DIR-based) paths.
/// Only removes hcom-specific hooks, not the whole file.
pub fn remove_claude_hooks() -> bool {
    let global_path = dirs::home_dir()
        .map(|h| h.join(".claude").join("settings.json"))
        .unwrap_or_default();
    let env_path = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("settings.json"));
    let local_path = get_claude_settings_path();

    let global_ok = remove_hooks_from_settings_path(&global_path);
    let env_ok = match env_path {
        Some(ref p) if *p != global_path => remove_hooks_from_settings_path(p),
        _ => true,
    };
    let local_ok = if local_path != global_path && Some(&local_path) != env_path.as_ref() {
        remove_hooks_from_settings_path(&local_path)
    } else {
        true
    };

    global_ok && env_ok && local_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_db() -> (tempfile::TempDir, HcomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (dir, db)
    }

    fn make_delivery_test_db() -> (tempfile::TempDir, HcomDb) {
        let (dir, db) = make_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO events (type, timestamp, instance, data)
                 VALUES ('message', '2026-01-01T00:00:00Z', 'luna', '{\"from\":\"luna\",\"text\":\"hello\",\"scope\":\"broadcast\"}')",
                [],
            )
            .unwrap();
        (dir, db)
    }

    fn delivery_cursor(db: &HcomDb) -> i64 {
        db.conn()
            .query_row(
                "SELECT last_event_id FROM instances WHERE name = 'nova'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test write failure",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_get_real_session_id_normal() {
        let raw = serde_json::json!({"session": {"session_id": "abc-123"}});
        assert_eq!(get_real_session_id(&raw, None, false), "abc-123");
    }

    #[test]
    fn test_get_real_session_id_fork() {
        crate::config::Config::init(); // log_info needs Config
        let raw = serde_json::json!({"session": {"session_id": "old-parent-id"}});
        let env_file =
            "/home/user/.claude/session-env/12345678-1234-1234-1234-123456789012/hook-1.sh";
        assert_eq!(
            get_real_session_id(&raw, Some(env_file), true),
            "12345678-1234-1234-1234-123456789012"
        );
    }

    #[test]
    fn test_get_real_session_id_non_fork_ignores_env() {
        let raw = serde_json::json!({"session": {"session_id": "correct-id"}});
        let env_file = "/home/user/.claude/session-env/wrong-id-from-env-file-path/hook-1.sh";
        // is_fork=false, so env_file should be ignored
        assert_eq!(
            get_real_session_id(&raw, Some(env_file), false),
            "correct-id"
        );
    }

    #[test]
    fn test_extract_name() {
        assert_eq!(
            extract_name("hcom send --name luna 'hello'"),
            Some("luna".to_string())
        );
        assert_eq!(extract_name("hcom list"), None);
        assert_eq!(
            extract_name("hcom send --name abc123 --intent request"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_subagent_start_with_agent_id() {
        let raw = serde_json::json!({"agent_id": "agent-uuid-123"});
        let result = build_subagent_start_output(&raw);
        assert!(result.is_some());
        let output = result.unwrap();
        let ctx = output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("agent-uuid-123"));
    }

    #[test]
    fn test_subagent_start_no_agent_id() {
        let raw = serde_json::json!({});
        assert!(build_subagent_start_output(&raw).is_none());
    }

    #[test]
    fn test_subagent_start_empty_agent_id() {
        let raw = serde_json::json!({"agent_id": ""});
        assert!(build_subagent_start_output(&raw).is_none());
    }

    #[test]
    fn test_posttooluse_delivery_commits_after_output_write() {
        let (_dir, db) = make_delivery_test_db();
        let (output, ack) = get_posttooluse_messages(&db, "nova").unwrap();
        let system_message = output["systemMessage"].as_str().unwrap();
        assert!(system_message.contains("luna"));
        assert!(system_message.contains("nova"));
        assert!(system_message.contains("hello"));
        let ctx = output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("luna"));
        assert!(ctx.contains("hello"));

        assert_eq!(delivery_cursor(&db), 0);
        let stdout = serde_json::to_string(&output).unwrap();
        let mut writer = Vec::new();
        write_hook_output(&db, &mut writer, &stdout, Some(&ack)).unwrap();

        assert_eq!(writer, stdout.as_bytes());
        assert_eq!(delivery_cursor(&db), ack.last_event_id);
    }

    #[test]
    fn test_posttooluse_delivery_write_failure_keeps_message_unread() {
        let (_dir, db) = make_delivery_test_db();
        let (output, ack) = get_posttooluse_messages(&db, "nova").unwrap();
        let stdout = serde_json::to_string(&output).unwrap();

        let error = write_hook_output(&db, &mut FailingWriter, &stdout, Some(&ack)).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(delivery_cursor(&db), 0);
        assert_eq!(db.get_unread_messages("nova").len(), 1);
    }

    #[test]
    fn test_combine_posttooluse_single() {
        let output = serde_json::json!({
            "systemMessage": "test msg",
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": "context1",
            }
        });
        let combined = combine_posttooluse_outputs(std::slice::from_ref(&output));
        assert_eq!(combined, output);
    }

    #[test]
    fn test_combine_posttooluse_multiple() {
        let o1 = serde_json::json!({
            "systemMessage": "msg1",
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": "ctx1",
            }
        });
        let o2 = serde_json::json!({
            "systemMessage": "msg2",
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": "ctx2",
            }
        });
        let combined = combine_posttooluse_outputs(&[o1, o2]);
        assert_eq!(
            combined["hookSpecificOutput"]["additionalContext"],
            "ctx1\n\n---\n\nctx2"
        );
        assert_eq!(combined["systemMessage"], "msg1 + msg2");
    }

    #[test]
    fn test_bind_vanilla_string_response() {
        // Test that tool_response as string works
        let _raw = serde_json::json!({
            "tool_response": "output [hcom:luna] done"
        });
        // Can't test full bind without DB, but can verify marker extraction
        let caps = BIND_MARKER_RE.captures("output [hcom:luna] done");
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "luna");
    }

    #[test]
    fn test_bind_vanilla_dict_response() {
        let _raw = serde_json::json!({
            "tool_response": {"stdout": "[hcom:nova]", "stderr": ""}
        });
        let response_text = "[hcom:nova]";
        let caps = BIND_MARKER_RE.captures(response_text);
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "nova");
    }

    #[test]
    fn test_is_hcom_hook_command() {
        assert!(is_hcom_hook_command("${HCOM} sessionstart"));
        assert!(is_hcom_hook_command("${HCOM} post"));
        assert!(is_hcom_hook_command("hcom sessionstart"));
        assert!(is_hcom_hook_command("hcom post"));
        assert!(is_hcom_hook_command("uvx hcom claude-notify"));
        assert!(!is_hcom_hook_command("echo hello"));
        assert!(!is_hcom_hook_command(""));
    }

    #[test]
    fn test_is_hcom_hook_command_legacy() {
        assert!(is_hcom_hook_command("HCOM_ACTIVE=1 hcom.py sessionstart"));
        assert!(is_hcom_hook_command("sh -c 'hcom something'"));
    }

    #[test]
    fn test_build_hook_entry_command_avoids_nested_shell() {
        let command = build_hook_entry_command("poll");
        assert_eq!(
            command,
            "cmd=${HCOM:-hcom}; command -v \"${cmd%% *}\" >/dev/null 2>&1 && exec $cmd poll || exit 0"
        );
        assert!(!command.starts_with("sh -c"));
    }

    #[test]
    fn test_remove_hcom_hooks_empty() {
        let mut settings = serde_json::json!({});
        assert!(!remove_hcom_hooks_from_settings(&mut settings));
    }

    #[test]
    fn test_remove_hcom_hooks_no_hooks_section() {
        let mut settings = serde_json::json!({"env": {"FOO": "bar"}});
        assert!(!remove_hcom_hooks_from_settings(&mut settings));
    }

    #[test]
    fn test_remove_hcom_hooks_with_hcom() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{
                        "type": "command",
                        "command": "${HCOM} sessionstart"
                    }]
                }]
            },
            "env": {"HCOM": "hcom"},
        });
        assert!(remove_hcom_hooks_from_settings(&mut settings));
        // SessionStart should be removed entirely
        assert!(settings["hooks"].get("SessionStart").is_none());
        // HCOM env should be removed
        assert!(settings.get("env").is_none());
    }

    #[test]
    fn test_remove_hcom_hooks_preserves_non_hcom() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PostToolUse": [{
                    "hooks": [
                        {"type": "command", "command": "${HCOM} post"},
                        {"type": "command", "command": "echo custom hook"},
                    ]
                }]
            }
        });
        assert!(remove_hcom_hooks_from_settings(&mut settings));
        // Matcher should be preserved with only the custom hook
        let matchers = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(matchers.len(), 1);
        let hooks = matchers[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["command"], "echo custom hook");
    }

    #[test]
    fn test_remove_hcom_permissions() {
        let mut settings = serde_json::json!({
            "hooks": {},
            "permissions": {
                "allow": [
                    "Bash(hcom send:*)",
                    "Bash(custom:*)",
                ]
            }
        });
        remove_hcom_hooks_from_settings(&mut settings);
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert_eq!(allow.len(), 1);
        assert_eq!(allow[0], "Bash(custom:*)");
    }

    #[test]
    fn test_claude_hook_configs_count() {
        assert_eq!(CLAUDE_HOOK_CONFIGS.len(), 10);
        assert_eq!(CLAUDE_HOOK_TYPES.len(), 10);
        assert_eq!(CLAUDE_HOOK_COMMANDS.len(), 10);
    }

    #[test]
    fn test_format_claude_permission() {
        assert_eq!(
            format_claude_permission("hcom", "send"),
            "Bash(hcom send:*)"
        );
        assert_eq!(
            format_claude_permission("hcom", "--help"),
            "Bash(hcom --help)"
        );
        assert_eq!(
            format_claude_permission("uvx hcom", "list"),
            "Bash(uvx hcom list:*)"
        );
    }

    #[test]
    fn test_format_claude_powershell_permission() {
        assert_eq!(
            format_claude_powershell_permission("hcom", "send"),
            "PowerShell(hcom send:*)"
        );
        assert_eq!(
            format_claude_powershell_permission("hcom", "--help"),
            "PowerShell(hcom --help)"
        );
        assert_eq!(
            format_claude_powershell_permission("uvx hcom", "list"),
            "PowerShell(uvx hcom list:*)"
        );
    }

    #[test]
    fn test_build_claude_permissions() {
        let perms = build_claude_permissions();
        assert!(!perms.is_empty());
        // Both Bash and PowerShell variants are installed for every safe command.
        assert_eq!(perms.len(), SAFE_HCOM_COMMANDS.len() * 2);
        assert_eq!(
            perms.iter().filter(|p| p.starts_with("Bash(")).count(),
            SAFE_HCOM_COMMANDS.len()
        );
        assert_eq!(
            perms
                .iter()
                .filter(|p| p.starts_with("PowerShell("))
                .count(),
            SAFE_HCOM_COMMANDS.len()
        );
        // All should start with "Bash(" or "PowerShell("
        for p in &perms {
            assert!(
                p.starts_with("Bash(") || p.starts_with("PowerShell("),
                "bad permission: {}",
                p
            );
        }
    }

    #[test]
    fn test_build_all_claude_permission_patterns() {
        let patterns = build_all_claude_permission_patterns();
        // Should have both hcom and uvx hcom variants, each with Bash and PowerShell rules
        let expected = (SAFE_HCOM_COMMANDS.len() + LEGACY_HCOM_COMMANDS.len()) * 2 * 2;
        assert_eq!(patterns.len(), expected);
        assert!(patterns.iter().any(|p| p.contains("hcom send")));
        assert!(patterns.iter().any(|p| p == "PowerShell(hcom send:*)"));
        assert!(patterns.iter().any(|p| p == "PowerShell(uvx hcom send:*)"));
        assert!(patterns.iter().any(|p| p.contains("uvx hcom send")));
        // Legacy commands included for removal
        assert!(patterns.iter().any(|p| p.contains("hcom daemon")));
    }

    #[test]
    fn test_setup_and_verify_claude_hooks() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

        // Write empty settings
        std::fs::write(&settings_path, "{}").unwrap();

        // Can't call setup_claude_hooks directly (uses get_claude_settings_path),
        // but we can test the verify path with a hand-built settings file.
        let hook_cmd = "${HCOM}";
        let mut settings = serde_json::json!({"hooks": {}, "env": {"HCOM": "hcom"}});

        for &(hook_type, matcher, cmd_suffix, timeout) in CLAUDE_HOOK_CONFIGS {
            let mut hook_entry = serde_json::json!({
                "type": "command",
                "command": format!("{} {}", hook_cmd, cmd_suffix),
            });
            if let Some(t) = timeout {
                hook_entry["timeout"] = serde_json::json!(t);
            }
            let mut hook_dict = serde_json::json!({"hooks": [hook_entry]});
            if !matcher.is_empty() {
                hook_dict["matcher"] = Value::String(matcher.to_string());
            }
            settings["hooks"][hook_type] = serde_json::json!([hook_dict]);
        }

        // Add permissions
        settings["permissions"] = serde_json::json!({"allow": build_claude_permissions()});

        let json_str = serde_json::to_string_pretty(&settings).unwrap();
        std::fs::write(&settings_path, &json_str).unwrap();

        // Verify should pass
        assert!(verify_claude_hooks_installed(Some(&settings_path), true,));

        // Verify without permissions check
        assert!(verify_claude_hooks_installed(Some(&settings_path), false,));
    }

    #[test]
    fn test_verify_missing_file() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("nonexistent.json");
        assert!(!verify_claude_hooks_installed(Some(&settings_path), false,));
    }

    #[test]
    fn test_verify_incomplete_hooks() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.json");

        // Only has SessionStart, missing others
        let settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{"type": "command", "command": "${HCOM} sessionstart"}]
                }]
            },
            "env": {"HCOM": "hcom"}
        });
        std::fs::write(&settings_path, serde_json::to_string(&settings).unwrap()).unwrap();

        assert!(!verify_claude_hooks_installed(Some(&settings_path), false,));
    }

    fn write_settings_with_mutated_timeout(
        settings_path: &Path,
        new_timeout: Option<u64>,
        include_permissions: bool,
    ) {
        let hook_cmd = "${HCOM}";
        let mut settings = serde_json::json!({"hooks": {}, "env": {"HCOM": "hcom"}});

        for &(hook_type, matcher, cmd_suffix, timeout) in CLAUDE_HOOK_CONFIGS {
            let mut hook_entry = serde_json::json!({
                "type": "command",
                "command": format!("{} {}", hook_cmd, cmd_suffix),
            });
            if timeout.is_some()
                && let Some(t) = new_timeout
            {
                hook_entry["timeout"] = serde_json::json!(t);
            }
            let mut hook_dict = serde_json::json!({"hooks": [hook_entry]});
            if !matcher.is_empty() {
                hook_dict["matcher"] = Value::String(matcher.to_string());
            }
            settings["hooks"][hook_type] = serde_json::json!([hook_dict]);
        }

        if include_permissions {
            settings["permissions"] = serde_json::json!({"allow": build_claude_permissions()});
        }

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let json_str = serde_json::to_string_pretty(&settings).unwrap();
        std::fs::write(settings_path, &json_str).unwrap();
    }

    #[test]
    fn test_verify_accepts_timeout_value_edit() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.json");

        // External edit: timeouts rewritten to 10 across all entries that
        // originally carried a timeout. Numeric value edits stay accepted —
        // only presence + numeric type are checked.
        write_settings_with_mutated_timeout(&settings_path, Some(10), false);
        assert!(verify_claude_hooks_installed(Some(&settings_path), false));
    }

    #[test]
    fn test_verify_catches_timeout_field_dropped() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.json");

        write_settings_with_mutated_timeout(&settings_path, None, false);
        assert!(!verify_claude_hooks_installed(Some(&settings_path), false));
    }

    #[test]
    fn test_verify_rejects_non_numeric_timeout() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.json");

        write_settings_with_mutated_timeout(&settings_path, Some(86400), false);
        let mut settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        for &(hook_type, _, _, expected_timeout) in CLAUDE_HOOK_CONFIGS {
            if expected_timeout.is_none() {
                continue;
            }
            if let Some(arr) = settings["hooks"][hook_type].as_array_mut() {
                for matcher_obj in arr {
                    if let Some(hooks) = matcher_obj["hooks"].as_array_mut() {
                        for hook in hooks {
                            if hook.get("timeout").is_some() {
                                hook["timeout"] = serde_json::json!("86400");
                            }
                        }
                    }
                }
            }
        }
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(!verify_claude_hooks_installed(Some(&settings_path), false));
    }

    #[test]
    fn test_verify_rejects_missing_env() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.json");

        write_settings_with_mutated_timeout(&settings_path, None, false);
        let mut settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        settings.as_object_mut().unwrap().remove("env");
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(!verify_claude_hooks_installed(Some(&settings_path), false));
    }

    #[test]
    fn test_verify_rejects_missing_command() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.json");

        write_settings_with_mutated_timeout(&settings_path, None, false);
        // Strip the hcom command from one required hook (PostToolUse) to
        // simulate a partial install / external removal.
        let mut settings: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        if let Some(post) = settings["hooks"]["PostToolUse"].as_array_mut() {
            post.clear();
        }
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(!verify_claude_hooks_installed(Some(&settings_path), false));
    }

    #[test]
    fn test_remove_hooks_from_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(remove_hooks_from_settings_path(&path));
    }

    #[test]
    fn test_remove_hooks_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        let settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{"type": "command", "command": "${HCOM} sessionstart"}]
                }]
            },
            "env": {"HCOM": "hcom"},
            "other_key": "preserved"
        });
        std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        assert!(remove_hooks_from_settings_path(&path));

        // Verify hooks are gone but other_key preserved
        let content = std::fs::read_to_string(&path).unwrap();
        let result: Value = serde_json::from_str(&content).unwrap();
        assert!(result["hooks"].get("SessionStart").is_none());
        assert_eq!(result["other_key"], "preserved");
    }

    use crate::hooks::test_helpers::{EnvGuard, isolated_test_env};
    use serial_test::serial;

    fn claude_test_env() -> (tempfile::TempDir, PathBuf, PathBuf, EnvGuard) {
        let (dir, _hcom_dir, test_home, guard) = isolated_test_env();
        let settings_path = test_home.join(".claude").join("settings.json");
        (dir, test_home, settings_path, guard)
    }

    fn read_json(path: &Path) -> Value {
        let content = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&content).unwrap()
    }

    /// Independent verification: no hcom hooks in Claude settings JSON.
    fn independently_verify_no_hcom_hooks_claude(settings: &Value) -> Vec<String> {
        let mut violations = Vec::new();
        let hooks = match settings.get("hooks").and_then(|v| v.as_object()) {
            Some(h) => h,
            None => return violations,
        };
        let hcom_patterns = ["hcom", "HCOM", "${HCOM}"];
        for (hook_type, matchers_val) in hooks {
            let matchers = match matchers_val.as_array() {
                Some(a) => a,
                None => continue,
            };
            for (i, matcher) in matchers.iter().enumerate() {
                let hooks_arr = match matcher.get("hooks").and_then(|v| v.as_array()) {
                    Some(a) => a,
                    None => continue,
                };
                for (j, hook) in hooks_arr.iter().enumerate() {
                    let command = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    if hcom_patterns.iter().any(|p| command.contains(p)) {
                        violations.push(format!("{hook_type}[{i}].hooks[{j}]: command={command}"));
                    }
                }
            }
        }
        violations
    }

    /// Independent verification: expected hcom hooks present.
    fn independently_verify_hcom_hooks_present_claude(
        settings: &Value,
        expected: &[(&str, &str)], // (hook_type, command_substring)
    ) -> Vec<String> {
        let mut missing = Vec::new();
        let hooks = match settings.get("hooks").and_then(|v| v.as_object()) {
            Some(h) => h,
            None => {
                return expected
                    .iter()
                    .map(|(ht, _)| format!("{ht}: hooks dict missing"))
                    .collect();
            }
        };
        for &(hook_type, cmd_suffix) in expected {
            let expected_full = build_hook_entry_command(cmd_suffix);
            let matchers = match hooks.get(hook_type).and_then(|v| v.as_array()) {
                Some(a) => a,
                None => {
                    missing.push(format!("{hook_type}: not present"));
                    continue;
                }
            };
            let mut found = false;
            for matcher in matchers {
                if let Some(hook_list) = matcher.get("hooks").and_then(|v| v.as_array()) {
                    for hook in hook_list {
                        if let Some(cmd) = hook.get("command").and_then(|v| v.as_str())
                            && cmd == expected_full
                        {
                            found = true;
                            break;
                        }
                    }
                }
                if found {
                    break;
                }
            }
            if !found {
                missing.push(format!(
                    "{hook_type}: expected exact command '{expected_full}', not found"
                ));
            }
        }
        missing
    }

    #[test]
    #[serial]
    fn test_setup_claude_hooks_from_scratch() {
        let (_dir, _test_home, settings_path, _guard) = claude_test_env();

        assert!(setup_claude_hooks(false));
        assert!(settings_path.exists());

        let settings = read_json(&settings_path);

        // All hook types should be present
        assert!(settings.get("hooks").unwrap().is_object());
        for &(hook_type, matcher, cmd_suffix, timeout) in CLAUDE_HOOK_CONFIGS {
            let arr = settings["hooks"]
                .get(hook_type)
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| panic!("{hook_type} missing or not array"));
            assert!(!arr.is_empty(), "{hook_type} should have entries");

            // Find the hcom hook entry with exact command match
            let expected_command = build_hook_entry_command(cmd_suffix);
            let mut found = false;
            for entry in arr {
                let hooks_list = entry.get("hooks").and_then(|v| v.as_array());
                if let Some(hooks) = hooks_list {
                    for hook in hooks {
                        let cmd = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        if cmd == expected_command {
                            found = true;
                            // Verify matcher if non-empty
                            if !matcher.is_empty() {
                                assert_eq!(
                                    entry.get("matcher").and_then(|v| v.as_str()).unwrap_or(""),
                                    matcher,
                                    "{hook_type} matcher mismatch"
                                );
                            }
                            // Verify timeout if set
                            if let Some(t) = timeout {
                                assert_eq!(
                                    hook.get("timeout").and_then(|v| v.as_u64()),
                                    Some(t),
                                    "{hook_type} timeout mismatch"
                                );
                            }
                        }
                    }
                }
            }
            assert!(
                found,
                "{hook_type}: expected exact command '{expected_command}', not found"
            );
        }

        // HCOM env var should be set
        assert!(
            settings.get("env").and_then(|v| v.get("HCOM")).is_some(),
            "HCOM env var should be set"
        );

        assert!(verify_claude_hooks_installed(Some(&settings_path), false));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_claude_preserves_user_data() {
        let (_dir, _test_home, settings_path, _guard) = claude_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let user_settings = serde_json::json!({
            "env": {"MY_VAR": "test", "OTHER": "value"},
            "permissions": {
                "deny": ["Bash(rm -rf:*)"],
            },
            "hooks": {
                "PostToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "echo user hook",
                        "name": "my-logger",
                    }]
                }]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&user_settings).unwrap(),
        )
        .unwrap();

        assert!(setup_claude_hooks(false));

        let updated = read_json(&settings_path);

        // User env keys preserved (HCOM is added by setup)
        assert_eq!(updated["env"]["MY_VAR"], "test");
        assert_eq!(updated["env"]["OTHER"], "value");
        assert!(updated["env"].get("HCOM").is_some());

        // permissions.deny preserved
        assert_eq!(
            updated["permissions"]["deny"],
            serde_json::json!(["Bash(rm -rf:*)"])
        );

        // User hook preserved
        let post_hooks = updated["hooks"]["PostToolUse"].as_array().unwrap();
        let mut found_user_hook = false;
        for entry in post_hooks {
            if let Some(hooks) = entry.get("hooks").and_then(|v| v.as_array()) {
                for hook in hooks {
                    if hook.get("command").and_then(|v| v.as_str()) == Some("echo user hook") {
                        found_user_hook = true;
                    }
                }
            }
        }
        assert!(found_user_hook, "user hook should be preserved");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_claude_idempotent() {
        let (_dir, _test_home, settings_path, _guard) = claude_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, r#"{"env": {"MY_VAR": "test"}}"#).unwrap();

        assert!(setup_claude_hooks(false));
        let first = std::fs::read_to_string(&settings_path).unwrap();

        assert!(setup_claude_hooks(false));
        let second = std::fs::read_to_string(&settings_path).unwrap();

        assert_eq!(first, second, "setup should be idempotent");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_remove_claude_only_removes_hcom() {
        let (_dir, _test_home, settings_path, _guard) = claude_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        // Mixed hcom + user hooks in same type
        let settings = serde_json::json!({
            "hooks": {
                "PostToolUse": [{
                    "hooks": [
                        {"type": "command", "command": "${HCOM} post"},
                        {"type": "command", "command": "echo user hook", "name": "my-logger"},
                    ]
                }]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(remove_hooks_from_settings_path(&settings_path));

        let updated = read_json(&settings_path);
        // User hook should remain
        let post_hooks = updated["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post_hooks.len(), 1);
        let hooks_list = post_hooks[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks_list.len(), 1);
        assert_eq!(hooks_list[0]["command"], "echo user hook");

        // No hcom hooks
        let violations = independently_verify_no_hcom_hooks_claude(&updated);
        assert!(violations.is_empty(), "hcom hooks remain: {violations:?}");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_claude_setup_remove_roundtrip() {
        let (_dir, _test_home, settings_path, _guard) = claude_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let user_settings = serde_json::json!({
            "env": {"MY_VAR": "test"},
            "permissions": {"deny": ["dangerous"]},
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&user_settings).unwrap(),
        )
        .unwrap();

        // Setup
        assert!(setup_claude_hooks(false));
        let after_setup = read_json(&settings_path);
        let expected = vec![
            ("PostToolUse", "post"),
            ("Stop", "poll"),
            ("PermissionRequest", "permission-request"),
            ("Notification", "notify"),
        ];
        let missing = independently_verify_hcom_hooks_present_claude(&after_setup, &expected);
        assert!(
            missing.is_empty(),
            "after setup, missing hooks: {missing:?}"
        );

        // Remove
        assert!(remove_hooks_from_settings_path(&settings_path));
        let after_remove = read_json(&settings_path);
        let violations = independently_verify_no_hcom_hooks_claude(&after_remove);
        assert!(
            violations.is_empty(),
            "after remove, hcom hooks still present: {violations:?}"
        );

        // User data preserved
        assert_eq!(after_remove["env"]["MY_VAR"], "test");
        assert_eq!(
            after_remove["permissions"]["deny"],
            serde_json::json!(["dangerous"])
        );

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_claude_handles_empty_file() {
        let (_dir, _test_home, settings_path, _guard) = claude_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, "{}").unwrap();

        assert!(setup_claude_hooks(false));

        let settings = read_json(&settings_path);
        assert!(settings.get("hooks").unwrap().is_object());
        assert!(settings["hooks"].get("PostToolUse").is_some());

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_claude_handles_no_file() {
        let (_dir, _test_home, settings_path, _guard) = claude_test_env();

        assert!(!settings_path.exists());
        assert!(setup_claude_hooks(false));
        assert!(settings_path.exists());

        let settings = read_json(&settings_path);
        assert!(settings.get("hooks").is_some());

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_claude_handles_malformed_hooks() {
        let corrupt_cases: Vec<Value> = vec![
            Value::Null,
            Value::String("string".into()),
            serde_json::json!([]),
            serde_json::json!({"PreToolUse": "not_a_list"}),
            serde_json::json!({"PreToolUse": [null, "string", 123]}),
            serde_json::json!({"PreToolUse": [{"matcher": "*", "hooks": "not_a_list"}]}),
        ];

        for corrupt_hooks in corrupt_cases {
            let (_dir, _test_home, settings_path, _guard) = claude_test_env();
            std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

            let settings = serde_json::json!({
                "hooks": corrupt_hooks,
                "env": {"MY_VAR": "test"},
            });
            std::fs::write(
                &settings_path,
                serde_json::to_string_pretty(&settings).unwrap(),
            )
            .unwrap();

            // Should not crash
            let _ = setup_claude_hooks(false);

            // User data should still be there
            let updated = read_json(&settings_path);
            assert_eq!(updated["env"]["MY_VAR"], "test");
        }
    }

    #[test]
    #[serial]
    fn test_setup_claude_with_permissions() {
        let (_dir, _test_home, settings_path, _guard) = claude_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let user_settings = serde_json::json!({
            "permissions": {
                "allow": ["Bash(custom:*)"],
                "deny": ["Bash(rm -rf:*)"],
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&user_settings).unwrap(),
        )
        .unwrap();

        assert!(setup_claude_hooks(true));

        let updated = read_json(&settings_path);
        let allow = updated["permissions"]["allow"].as_array().unwrap();

        // User's custom permission preserved
        assert!(
            allow.iter().any(|v| v.as_str() == Some("Bash(custom:*)")),
            "user permission should be preserved"
        );
        // hcom permissions added
        let perms = build_claude_permissions();
        for p in &perms {
            assert!(
                allow.iter().any(|v| v.as_str() == Some(p.as_str())),
                "hcom permission {p} should be added"
            );
        }
        // deny preserved
        assert_eq!(
            updated["permissions"]["deny"],
            serde_json::json!(["Bash(rm -rf:*)"])
        );

        assert!(verify_claude_hooks_installed(Some(&settings_path), true));

        drop(_guard);
    }

    // ---- Hook-actor routing (raw.agent_id, not running_tasks.active) ----
    //
    // These are dispatcher-level tests: they drive `route_claude_hook` itself
    // (the function `dispatch_claude_hook` calls after reading stdin), not the
    // individual handler functions, because the bug class here is a routing
    // bug — which branch a given hook payload falls into — not a bug inside
    // any one handler. They need `isolated_test_env()` because
    // `route_claude_hook` exercises real log::log_info call sites (start_task,
    // track_subagent, sessionstart, ...), which resolve the hcom log path
    // through the global `Config`; without isolation that would touch the
    // real `~/.hcom` of whatever machine runs the test.

    fn make_ctx() -> HcomContext {
        HcomContext::from_env(&std::collections::HashMap::new(), PathBuf::from("/tmp"))
    }

    fn make_isolated_test_db() -> (tempfile::TempDir, EnvGuard, HcomDb) {
        let (dir, hcom_dir, _test_home, guard) = isolated_test_env();
        let db = HcomDb::open_raw(&hcom_dir.join("test.db")).unwrap();
        db.init_db().unwrap();
        (dir, guard, db)
    }

    /// Same fixture as `make_delivery_test_db` (instance 'nova' + a pending
    /// broadcast message from 'luna'), but under `isolated_test_env()` so
    /// dispatcher-level tests that log are safe to run.
    fn make_isolated_delivery_test_db() -> (tempfile::TempDir, EnvGuard, HcomDb) {
        let (dir, guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO events (type, timestamp, instance, data)
                 VALUES ('message', '2026-01-01T00:00:00Z', 'luna', '{\"from\":\"luna\",\"text\":\"hello\",\"scope\":\"broadcast\"}')",
                [],
            )
            .unwrap();
        (dir, guard, db)
    }

    /// `root_session_id` matches what real `ensure_subagent_row`-created rows
    /// carry as `parent_session_id` (always the true root session_id, at any
    /// nesting depth — see its doc comment), so fixtures built with this
    /// helper pass `validate_spawn_owner`'s hierarchy check the same way a
    /// real row would.
    fn insert_subagent_row(
        db: &HcomDb,
        name: &str,
        agent_id: &str,
        parent_name: &str,
        root_session_id: &str,
    ) {
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, last_event_id, agent_id, parent_name, parent_session_id)
                 VALUES (?, 'claude', 'active', 'subagent', 0, 0, 0, ?, ?, ?)",
                rusqlite::params![name, agent_id, parent_name, root_session_id],
            )
            .unwrap();
    }

    /// Property: a subagent PostToolUse whose agent_id has no resolvable
    /// `instances` row (SubagentStart's row allocation is best-effort and may
    /// not have happened, or the row is gone) must never deliver anything —
    /// and, critically, must never fall through to root/parent delivery.
    /// Row absence is identity state, not proof that the actor is the root.
    #[test]
    #[serial]
    fn test_subagent_posttooluse_unknown_row_never_falls_through_to_parent() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_delivery_test_db();
        db.set_session_binding("sess-1", "nova").unwrap();
        // Parent tracks an agent_id that never got an instances row.
        db.conn()
            .execute(
                "UPDATE instances SET running_tasks = ? WHERE name = 'nova'",
                rusqlite::params![
                    r#"{"active":true,"subagents":[{"agent_id":"ghost-agent","type":"general"}]}"#
                ],
            )
            .unwrap();

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "ghost-agent",
            "tool_name": "Bash",
            "tool_input": {"command": "hcom send --name ghost-agent -- hi"},
        });
        let mut payload = HookPayload::from_claude(raw);
        let ctx = make_ctx();
        let (exit_code, stdout, ack, _timing) =
            route_claude_hook(&db, &ctx, HOOK_POST, &mut payload);

        assert_eq!(exit_code, 0);
        assert!(
            stdout.is_empty(),
            "unknown subagent actor must not deliver anything, got: {stdout}"
        );
        assert!(ack.is_none());
        // nova's own pending broadcast must remain untouched — no fallthrough.
        assert_eq!(delivery_cursor(&db), 0);
    }

    /// Property: a subagent-context hook whose row *does* resolve, but which
    /// doesn't match any actionable branch (e.g. an ordinary Edit tool call),
    /// stays a silent no-op rather than falling through to parent handling.
    #[test]
    #[serial]
    fn test_subagent_hook_unrelated_tool_stays_silent() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        insert_subagent_row(&db, "nova_task_1", "sub-agent-1", "nova", "sess-1");

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "sub-agent-1",
            "tool_name": "Edit",
            "tool_input": {"file_path": "/tmp/x"},
        });
        let mut payload = HookPayload::from_claude(raw);
        let ctx = make_ctx();
        let (exit_code, stdout, ack, _timing) =
            route_claude_hook(&db, &ctx, HOOK_POST, &mut payload);

        assert_eq!(exit_code, 0);
        assert!(stdout.is_empty());
        assert!(ack.is_none());
    }

    /// Property: stopping a nested native parent must recursively tear down its
    /// own children. Native subagent rows carry session_id=NULL and inherit the
    /// root session as parent_session_id, so the session-keyed teardown cascade
    /// never links a nested parent to its children — only parent_name does.
    /// Without the parent_name cascade, stopping parent A would delete A while
    /// its child B stayed alive, reparented to a reusable name.
    #[test]
    #[serial]
    fn test_stop_nested_parent_cascades_to_children() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        // A is a native subagent of root nova; B is a native subagent of A.
        // Both share the root session as parent_session_id and have no session
        // of their own (session_id=NULL, as insert_subagent_row leaves it).
        insert_subagent_row(&db, "nova_a_1", "agent-a", "nova", "sess-1");
        insert_subagent_row(&db, "nova_a_1_b_1", "agent-b", "nova_a_1", "sess-1");

        crate::hooks::stop_instance(&db, "nova_a_1", "test", "task_completed");

        assert!(
            db.get_instance_by_agent_id("agent-a").unwrap().is_none(),
            "nested parent A must be torn down"
        );
        assert!(
            db.get_instance_by_agent_id("agent-b").unwrap().is_none(),
            "child B must be cascaded, not orphaned with parent_name=A"
        );
        assert!(
            db.get_instance_full("nova").unwrap().is_some(),
            "the still-live root must not be touched"
        );
    }

    /// Property: the root's own PostToolUse must deliver even while a
    /// genuinely live background subagent is tracked active. Since Claude
    /// Code 2.1.198, Agent calls background by default, so the root can have
    /// its own interleaved tool calls while a subagent still runs — gating
    /// root delivery on `running_tasks.active` (the pre-existing design)
    /// would freeze the parent for that whole window.
    #[test]
    #[serial]
    fn test_root_posttooluse_delivers_while_subagent_active() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_delivery_test_db();
        db.set_session_binding("sess-1", "nova").unwrap();
        insert_subagent_row(&db, "nova_task_1", "live-agent-1", "nova", "sess-1");
        db.conn()
            .execute(
                "UPDATE instances SET running_tasks = ? WHERE name = 'nova'",
                rusqlite::params![
                    r#"{"active":true,"subagents":[{"agent_id":"live-agent-1","type":"general"}]}"#
                ],
            )
            .unwrap();

        // Root's own PostToolUse — no agent_id: this genuinely is nova's own
        // tool call, not a hook firing inside the live subagent.
        let raw = serde_json::json!({
            "session_id": "sess-1",
            "tool_name": "Read",
            "tool_input": {},
        });
        let mut payload = HookPayload::from_claude(raw);
        let ctx = make_ctx();
        let (_exit_code, stdout, ack, _timing) =
            route_claude_hook(&db, &ctx, HOOK_POST, &mut payload);

        assert!(
            !stdout.is_empty(),
            "root PostToolUse must deliver even while a background subagent is active"
        );
        assert!(stdout.contains("hello"));
        assert!(ack.is_some());
    }

    /// Property: a nested Agent/Task PreToolUse — one that fires *inside* a
    /// subagent's own execution context because that subagent itself called
    /// the Agent tool — must mark the spawning subagent's own running_tasks
    /// active, not the root parent's. Parent and every nested subagent share
    /// the same Claude session_id, so resolving the actor via
    /// `get_session_binding(session_id)` (the pre-existing design) always
    /// lands on the root regardless of which level actually spawned the call.
    #[test]
    #[serial]
    fn test_nested_task_pre_updates_subagent_not_root() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        insert_subagent_row(&db, "nova_task_1", "sub-agent-1", "nova", "sess-1");

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "sub-agent-1",
            "tool_name": "Task",
            "tool_input": {"prompt": "spawn a nested helper"},
        });
        let mut payload = HookPayload::from_claude(raw);
        let ctx = make_ctx();
        let _ = route_claude_hook(&db, &ctx, HOOK_PRE, &mut payload);

        let sub_rt = instances::parse_running_tasks(
            db.get_instance_full("nova_task_1")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert!(
            sub_rt.active,
            "the spawning subagent's own running_tasks must be marked active"
        );

        let root_rt = instances::parse_running_tasks(
            db.get_instance_full("nova")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert!(
            !root_rt.active,
            "the root parent's running_tasks must not be touched by a nested Task call"
        );
    }

    /// Property: PostToolUse for the Agent/Task tool fires with
    /// `tool_response.status == "async_launched"` when Claude merely
    /// dispatched the call to the background (default since Claude Code
    /// 2.1.198) — this is not completion, so it must not deliver a
    /// "Subagents have finished" summary and must not advance the delivery
    /// cursor. Foreground completion is covered separately by
    /// `test_task_posttooluse_foreground_completed_delivers`.
    #[test]
    #[serial]
    fn test_task_posttooluse_async_launch_skips_delivery() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_delivery_test_db();
        db.set_session_binding("sess-1", "nova").unwrap();
        let ctx = make_ctx();

        let raw_async = serde_json::json!({
            "session_id": "sess-1",
            "tool_name": "Agent",
            "tool_response": {"status": "async_launched"},
        });
        let mut payload_async = HookPayload::from_claude(raw_async);
        let (_exit_code, stdout_async, ack_async, _timing) =
            route_claude_hook(&db, &ctx, HOOK_POST, &mut payload_async);
        assert!(
            stdout_async.is_empty(),
            "async_launched must not be treated as Task completion, got: {stdout_async}"
        );
        assert!(ack_async.is_none());
        assert_eq!(
            delivery_cursor(&db),
            0,
            "async_launched must not advance the delivery cursor"
        );
    }

    /// Property: a foreground (synchronous, non-backgrounded) Agent/Task
    /// PostToolUse — no `async_launched` status — is a genuine completion and
    /// must deliver freeze-period messages normally. Independent of the
    /// async_launched test above: this is not "the same call, later", it's
    /// the separately-exercised foreground path.
    #[test]
    #[serial]
    fn test_task_posttooluse_foreground_completed_delivers() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_delivery_test_db();
        db.set_session_binding("sess-1", "nova").unwrap();
        let ctx = make_ctx();

        let raw_done = serde_json::json!({
            "session_id": "sess-1",
            "tool_name": "Agent",
            "tool_response": {"status": "completed"},
        });
        let mut payload_done = HookPayload::from_claude(raw_done);
        let (_exit_code, stdout_done, _ack, _timing) =
            route_claude_hook(&db, &ctx, HOOK_POST, &mut payload_done);
        assert!(
            stdout_done.contains("hello"),
            "a foreground Task completion must deliver freeze messages, got: {stdout_done}"
        );
        assert!(delivery_cursor(&db) > 0);
    }

    /// Property: the `--name` a subagent passes to a Bash hcom command must
    /// match the agent_id Claude itself stamped on this hook (raw.agent_id),
    /// or one subagent could spoof another's `--name` and read its inbox.
    #[test]
    #[serial]
    fn test_subagent_bash_name_mismatch_does_not_leak_other_inbox() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        insert_subagent_row(&db, "nova_task_1", "agent-a", "nova", "sess-1");
        insert_subagent_row(&db, "nova_task_2", "agent-b", "nova", "sess-1");
        // A direct mention pending for subagent B's inbox only.
        db.conn()
            .execute(
                "INSERT INTO events (type, timestamp, instance, data)
                 VALUES ('message', '2026-01-01T00:00:00Z', 'luna', '{\"from\":\"luna\",\"text\":\"secret for b\",\"scope\":\"mentions\",\"mentions\":[\"nova_task_2\"]}')",
                [],
            )
            .unwrap();
        let ctx = make_ctx();

        // Hook fires inside subagent A's own context (agent_id=agent-a), but
        // the Bash command spoofs --name agent-b.
        let raw_spoof = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-a",
            "tool_name": "Bash",
            "tool_input": {"command": "hcom send --name agent-b -- hi"},
        });
        let mut payload_spoof = HookPayload::from_claude(raw_spoof);
        let (exit_code, stdout, ack, _timing) =
            route_claude_hook(&db, &ctx, HOOK_POST, &mut payload_spoof);
        assert_eq!(exit_code, 0);
        assert!(
            stdout.is_empty(),
            "mismatched --name must not deliver another subagent's inbox, got: {stdout}"
        );
        assert!(ack.is_none());

        // The legitimate case still works: agent_id and --name match.
        let raw_ok = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-b",
            "tool_name": "Bash",
            "tool_input": {"command": "hcom send --name agent-b -- hi"},
        });
        let mut payload_ok = HookPayload::from_claude(raw_ok);
        let (_exit_code, stdout_ok, ack_ok, _timing) =
            route_claude_hook(&db, &ctx, HOOK_POST, &mut payload_ok);
        assert!(
            stdout_ok.contains("secret for b"),
            "matching --name must deliver its own inbox, got: {stdout_ok}"
        );
        assert!(ack_ok.is_some());
    }

    /// Property: `running_tasks` is a whole-JSON-blob column mutated by
    /// separate hook processes (each hook invocation opens its own DB
    /// connection). Concurrent SubagentStart hooks for sibling subagents of
    /// the same parent (parallel Task calls) must not race a plain
    /// read-then-write into a lost update.
    #[test]
    #[serial]
    fn test_mutate_running_tasks_concurrent_tracking_no_lost_updates() {
        crate::config::Config::init();
        let (_dir, hcom_dir, _test_home, _guard) = isolated_test_env();
        let db_path = hcom_dir.join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();

        let n: usize = 8;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let path = db_path.clone();
                std::thread::spawn(move || {
                    let db = HcomDb::open_raw(&path).unwrap();
                    track_subagent(&db, "nova", &format!("agent-{i}"), "general");
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let rt = instances::parse_running_tasks(
            db.get_instance_full("nova")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert_eq!(
            rt.subagents.len(),
            n,
            "concurrent SubagentStart tracking must not lose sibling entries to a read-modify-write race, got {:?}",
            rt.subagents
        );
        assert!(rt.active);
        for i in 0..n {
            let want = format!("agent-{i}");
            assert!(
                rt.subagents
                    .iter()
                    .any(|s| s.get("agent_id").and_then(|v| v.as_str()) == Some(want.as_str())),
                "missing {want}"
            );
        }
    }

    /// Property: concurrent SubagentStop-driven removals for sibling
    /// subagents must not race a plain read-then-write into a lost update
    /// either — an entry silently surviving a lost removal is exactly what
    /// leaves `running_tasks.active` stuck true (the original stale-freeze
    /// symptom).
    #[test]
    #[serial]
    fn test_mutate_running_tasks_concurrent_removal_no_lost_updates() {
        crate::config::Config::init();
        let (_dir, hcom_dir, _test_home, _guard) = isolated_test_env();
        let db_path = hcom_dir.join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();

        let n: usize = 8;
        let subagents: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({"agent_id": format!("agent-{i}"), "type": "general"}))
            .collect();
        let rt_json = serde_json::json!({"active": true, "subagents": subagents});
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, last_event_id, running_tasks)
                 VALUES ('nova', 'claude', 'listening', 'start', 0, 0, 0, ?)",
                rusqlite::params![rt_json.to_string()],
            )
            .unwrap();

        let handles: Vec<_> = (0..n)
            .map(|i| {
                let path = db_path.clone();
                std::thread::spawn(move || {
                    let db = HcomDb::open_raw(&path).unwrap();
                    remove_subagent_from_parent(&db, "nova", &format!("agent-{i}"));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let rt = instances::parse_running_tasks(
            db.get_instance_full("nova")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert!(
            rt.subagents.is_empty(),
            "concurrent removal must not lose sibling removals to a read-modify-write race, got {:?}",
            rt.subagents
        );
        assert!(!rt.active);
    }

    /// Property: every Claude subagent carries `agent_id` on its hooks
    /// regardless of whether its root ever ran `hcom start` — that's a
    /// property of Claude's hook schema, not of hcom participation.
    /// SubagentStart must stay a silent no-op (no `hcom start --name ...`
    /// hint, no allocated row, no running_tasks mutation) when the shared
    /// session_id has no hcom root binding at all.
    #[test]
    #[serial]
    fn test_subagent_start_nonparticipant_root_stays_silent() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        // No session binding: "sess-1" is not an hcom participant.

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "child-1",
            "agent_type": "general",
        });
        let mut payload = HookPayload::from_claude(raw);
        let ctx = make_ctx();
        let (exit_code, stdout, ack, _timing) =
            route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload);

        assert_eq!(exit_code, 0);
        assert!(
            stdout.is_empty(),
            "nonparticipant SubagentStart must not inject an hcom hint, got: {stdout}"
        );
        assert!(ack.is_none());
        assert!(
            db.get_instance_by_agent_id("child-1").unwrap().is_none(),
            "nonparticipant SubagentStart must not allocate an instances row"
        );
    }

    /// Property: a SubagentStart's own `agent_id` names the *new* child, not
    /// who spawned it, so a nested spawn (subagent A calling the Agent tool)
    /// can only be attributed correctly via the `prompt_id` Claude repeats
    /// across the spawning PreToolUse and the resulting SubagentStart(s) (see
    /// `resolve_spawn_owner`). Covers the full sequence: nested Pre on A,
    /// then SubagentStart for child B with a matching prompt_id, must
    /// attribute B to A (not root) — and a genuinely top-level spawn (root's
    /// own Pre, no agent_id) must still attribute to root.
    #[test]
    #[serial]
    fn test_nested_subagent_start_resolves_via_prompt_id_not_root() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id, running_tasks)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0, ?)",
                rusqlite::params![
                    r#"{"active":true,"subagents":[{"agent_id":"agent-a","type":"general"}]}"#
                ],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        insert_subagent_row(&db, "nova_task_1", "agent-a", "nova", "sess-1");
        let ctx = make_ctx();

        // Nested Task PreToolUse fires *inside* subagent A (agent_id=agent-a),
        // carrying the prompt_id Claude repeats on the SubagentStart for the
        // child it spawns.
        let raw_pre = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-a",
            "prompt_id": "p1",
            "tool_name": "Task",
            "tool_input": {"prompt": "spawn a nested helper"},
        });
        let mut payload_pre = HookPayload::from_claude(raw_pre);
        let _ = route_claude_hook(&db, &ctx, HOOK_PRE, &mut payload_pre);

        // SubagentStart for the new child B: its own agent_id names B, not A
        // — only the shared prompt_id says who spawned it.
        let raw_start = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-b",
            "agent_type": "general",
            "prompt_id": "p1",
        });
        let mut payload_start = HookPayload::from_claude(raw_start);
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload_start);

        let b_name = db.get_instance_by_agent_id("agent-b").unwrap().unwrap();
        let b_row = db.get_instance_full(&b_name).unwrap().unwrap();
        assert_eq!(
            b_row.parent_name.as_deref(),
            Some("nova_task_1"),
            "the nested child must be attributed to the spawning subagent, not root"
        );

        let a_rt = instances::parse_running_tasks(
            db.get_instance_full("nova_task_1")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert!(
            a_rt.subagents
                .iter()
                .any(|s| s.get("agent_id").and_then(|v| v.as_str()) == Some("agent-b")),
            "the spawning subagent must track its own child"
        );

        let root_rt = instances::parse_running_tasks(
            db.get_instance_full("nova")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert!(
            !root_rt
                .subagents
                .iter()
                .any(|s| s.get("agent_id").and_then(|v| v.as_str()) == Some("agent-b")),
            "root must not be credited with a grandchild it never spawned"
        );
        assert_eq!(
            root_rt.subagents.len(),
            1,
            "root's own directly-tracked subagent (A) must be untouched"
        );

        // A genuinely top-level spawn (root's own Pre, no agent_id) must
        // still resolve via the same correlation to root itself.
        let raw_pre2 = serde_json::json!({
            "session_id": "sess-1",
            "prompt_id": "p2",
            "tool_name": "Task",
            "tool_input": {"prompt": "spawn a top-level helper"},
        });
        let mut payload_pre2 = HookPayload::from_claude(raw_pre2);
        let _ = route_claude_hook(&db, &ctx, HOOK_PRE, &mut payload_pre2);

        let raw_start2 = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-c",
            "agent_type": "general",
            "prompt_id": "p2",
        });
        let mut payload_start2 = HookPayload::from_claude(raw_start2);
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload_start2);

        let c_name = db.get_instance_by_agent_id("agent-c").unwrap().unwrap();
        let c_row = db.get_instance_full(&c_name).unwrap().unwrap();
        assert_eq!(
            c_row.parent_name.as_deref(),
            Some("nova"),
            "a genuinely top-level spawn must still attribute to root"
        );
    }

    /// Property: a SubagentStart with no `prompt_id` field at all (Claude
    /// Code < 2.1.196, where this correlation doesn't exist on the wire)
    /// must still attach to root — the pre-2.1.196 legacy behavior, not
    /// nested-spawn support.
    #[test]
    #[serial]
    fn test_subagent_start_without_prompt_id_uses_legacy_root_attribution() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-legacy",
            "agent_type": "general",
        });
        let mut payload = HookPayload::from_claude(raw);
        let ctx = make_ctx();
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload);

        let name = db
            .get_instance_by_agent_id("agent-legacy")
            .unwrap()
            .unwrap();
        let row = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(row.parent_name.as_deref(), Some("nova"));
    }

    /// Property: `prompt_id` is present (Claude Code >= 2.1.196) but no
    /// mapping resolves — start_task's write hasn't landed yet, or never
    /// will. Correlation failure must fail closed: no allocated row, no
    /// running_tasks mutation anywhere (root included), no bootstrap hint.
    /// Root is not a safe fallback here — unlike the no-`prompt_id`-at-all
    /// case, a version that *does* send `prompt_id` and still misses means
    /// something is actually wrong.
    #[test]
    #[serial]
    fn test_subagent_start_with_prompt_id_but_no_mapping_fails_closed() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        let ctx = make_ctx();

        // No preceding Task/Agent PreToolUse ever recorded this prompt_id.
        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "orphan-1",
            "agent_type": "general",
            "prompt_id": "p-missing",
        });
        let mut payload = HookPayload::from_claude(raw);
        let (exit_code, stdout, ack, _timing) =
            route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload);

        assert_eq!(exit_code, 0);
        assert!(
            stdout.is_empty(),
            "an unresolved prompt_id must not inject an hcom hint, got: {stdout}"
        );
        assert!(ack.is_none());
        assert!(
            db.get_instance_by_agent_id("orphan-1").unwrap().is_none(),
            "an unresolved prompt_id must not allocate an instances row"
        );
        let root_rt = instances::parse_running_tasks(
            db.get_instance_full("nova")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert!(
            root_rt.subagents.is_empty(),
            "an unresolved prompt_id must not fall back to crediting root"
        );
    }

    /// Property: multiple children spawned in parallel by one actor within
    /// one turn share the same `prompt_id`. If an earlier sibling's own
    /// Task/Agent PostToolUse completes *before* a later sibling's
    /// SubagentStart arrives, the shared mapping must survive — completion
    /// of one sibling must not delete a mapping the others still need (the
    /// interleaving race `end_task` used to be vulnerable to when it deleted
    /// the mapping per-completion; see `spawn_owner_kv_key`).
    #[test]
    #[serial]
    fn test_parallel_siblings_survive_interleaved_sibling_completion() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        let ctx = make_ctx();

        // Root issues (what will become) two parallel Task calls in one
        // turn — Claude stamps both with the same prompt_id.
        let raw_pre = serde_json::json!({
            "session_id": "sess-1",
            "prompt_id": "p1",
            "tool_name": "Task",
            "tool_input": {"prompt": "spawn two parallel helpers"},
        });
        let mut payload_pre = HookPayload::from_claude(raw_pre);
        let _ = route_claude_hook(&db, &ctx, HOOK_PRE, &mut payload_pre);

        // First sibling starts.
        let raw_start1 = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-1",
            "agent_type": "general",
            "prompt_id": "p1",
        });
        let mut payload_start1 = HookPayload::from_claude(raw_start1);
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload_start1);

        // That Task tool_use's own PostToolUse completes — interleaved
        // *before* the second sibling's SubagentStart arrives.
        let raw_post = serde_json::json!({
            "session_id": "sess-1",
            "prompt_id": "p1",
            "tool_name": "Task",
            "tool_response": {"status": "completed"},
        });
        let mut payload_post = HookPayload::from_claude(raw_post);
        let _ = route_claude_hook(&db, &ctx, HOOK_POST, &mut payload_post);

        // Second sibling starts, same shared prompt_id.
        let raw_start2 = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-2",
            "agent_type": "general",
            "prompt_id": "p1",
        });
        let mut payload_start2 = HookPayload::from_claude(raw_start2);
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload_start2);

        let name1 = db.get_instance_by_agent_id("agent-1").unwrap().unwrap();
        let name2 = db.get_instance_by_agent_id("agent-2").unwrap();
        assert!(
            name2.is_some(),
            "the second sibling must still resolve its owner after the first sibling's PostToolUse completed"
        );
        let row1 = db.get_instance_full(&name1).unwrap().unwrap();
        let row2 = db.get_instance_full(&name2.unwrap()).unwrap().unwrap();
        assert_eq!(row1.parent_name.as_deref(), Some("nova"));
        assert_eq!(
            row2.parent_name.as_deref(),
            Some("nova"),
            "both siblings must attach to the same true owner despite the interleaved completion"
        );
    }

    /// Property: spawn-owner mappings are cleaned up per-session at
    /// SessionEnd (see `handle_sessionend`), not per-Task-completion — must
    /// remove only the ending session's own keys, never another session's.
    #[test]
    #[serial]
    fn test_sessionend_cleans_only_its_own_session_spawn_owner_keys() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        let ctx = make_ctx();

        db.kv_set(&spawn_owner_kv_key("sess-1", "p1"), Some("nova"))
            .unwrap();
        db.kv_set(&spawn_owner_kv_key("sess-other", "p1"), Some("other-root"))
            .unwrap();

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "reason": "clear",
        });
        let mut payload = HookPayload::from_claude(raw);
        let _ = route_claude_hook(&db, &ctx, HOOK_SESSIONEND, &mut payload);

        assert!(
            db.kv_get(&spawn_owner_kv_key("sess-1", "p1"))
                .unwrap()
                .is_none(),
            "SessionEnd must clean up its own session's spawn-owner mappings"
        );
        assert_eq!(
            db.kv_get(&spawn_owner_kv_key("sess-other", "p1")).unwrap(),
            Some("other-root".to_string()),
            "SessionEnd must not touch a different session's spawn-owner mappings"
        );
    }

    #[test]
    fn test_spawn_owner_kv_key_is_session_scoped() {
        assert_eq!(spawn_owner_kv_key("sess-1", "p1"), "spawn_owner:sess-1:p1");
        assert_ne!(
            spawn_owner_kv_key("sess-1", "p1"),
            spawn_owner_kv_key("sess-2", "p1")
        );
        assert!(spawn_owner_kv_key("sess-1", "p1").starts_with(&spawn_owner_kv_prefix("sess-1")));
    }

    /// Property: when SubagentStop's own `instances` row is missing, the
    /// stale running_tasks entry could be on the session-bound root *or* a
    /// nested subagent that actually spawned it — removal must find and
    /// clear it wherever it really is, not assume root.
    #[test]
    #[serial]
    fn test_subagent_stop_missing_row_removes_from_actual_nested_owner() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, last_event_id, running_tasks)
                 VALUES ('nova', 'claude', 'listening', 'start', 0, 0, 0, ?)",
                rusqlite::params![
                    r#"{"active":true,"subagents":[{"agent_id":"agent-a","type":"general"}]}"#
                ],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, last_event_id, agent_id, parent_name, running_tasks)
                 VALUES ('nova_task_1', 'claude', 'active', 'subagent', 0, 0, 0, 'agent-a', 'nova', ?)",
                rusqlite::params![
                    r#"{"active":true,"subagents":[{"agent_id":"agent-b","type":"general"}]}"#
                ],
            )
            .unwrap();
        // B (agent-b) has no instances row at all — allocation never
        // happened, or the row is gone.

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-b",
        });
        let mut payload = HookPayload::from_claude(raw);
        let ctx = make_ctx();
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_STOP, &mut payload);

        let a_rt = instances::parse_running_tasks(
            db.get_instance_full("nova_task_1")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert!(
            a_rt.subagents.is_empty(),
            "the actual nested owner (A) must have the dead child reaped"
        );
        assert!(!a_rt.active);

        let root_rt = instances::parse_running_tasks(
            db.get_instance_full("nova")
                .unwrap()
                .unwrap()
                .running_tasks
                .as_deref(),
        );
        assert_eq!(
            root_rt.subagents.len(),
            1,
            "root must be untouched — it never tracked the nested child directly"
        );
    }

    // ---- Resumed-agent correlation (agent_owner) ----

    /// Property: a resumed subagent — Claude re-firing SubagentStart for a
    /// previously-known `agent_id` under a *new* `prompt_id` with no
    /// corresponding PreToolUse to map it — must reattach to its
    /// original owner via the session-scoped agent_owner memory, not fail
    /// closed. Sequential: the original subagent fully spawns and stops
    /// (row deleted) before the resume fires.
    #[test]
    #[serial]
    fn test_resumed_agent_id_reattaches_via_agent_owner_after_original_stopped() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        let ctx = make_ctx();

        // Original spawn.
        let raw_pre = serde_json::json!({
            "session_id": "sess-1",
            "prompt_id": "p1",
            "tool_name": "Task",
            "tool_input": {"prompt": "do a thing"},
        });
        let mut payload_pre = HookPayload::from_claude(raw_pre);
        let _ = route_claude_hook(&db, &ctx, HOOK_PRE, &mut payload_pre);

        let raw_start = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-x",
            "agent_type": "general",
            "prompt_id": "p1",
        });
        let mut payload_start = HookPayload::from_claude(raw_start);
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload_start);
        let original_name = db.get_instance_by_agent_id("agent-x").unwrap().unwrap();
        assert_eq!(
            db.get_instance_full(&original_name)
                .unwrap()
                .unwrap()
                .parent_name
                .as_deref(),
            Some("nova")
        );

        // It stops (dormant + no direct message => immediate idle stop).
        let raw_stop = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-x",
            "prompt_id": "p1",
        });
        let mut payload_stop = HookPayload::from_claude(raw_stop);
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_STOP, &mut payload_stop);
        assert!(
            db.get_instance_by_agent_id("agent-x").unwrap().is_none(),
            "row must be gone after stop"
        );

        // Resume: same agent_id, new prompt_id, no PreToolUse for it.
        let raw_resume = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-x",
            "agent_type": "general",
            "prompt_id": "p2",
        });
        let mut payload_resume = HookPayload::from_claude(raw_resume);
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload_resume);

        let resumed_name = db.get_instance_by_agent_id("agent-x").unwrap();
        assert!(
            resumed_name.is_some(),
            "resume must not fail closed just because its new prompt_id has no mapping"
        );
        let resumed_row = db
            .get_instance_full(&resumed_name.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            resumed_row.parent_name.as_deref(),
            Some("nova"),
            "resume must reattach to the true original owner via agent_owner memory"
        );
    }

    /// Property: the resumed subagent's next PostToolUse must resolve its
    /// identity (not the reported `unknown_subagent_actor` fail-closed path)
    /// once resume has reattached it — otherwise `hcom list`/`send` can't see
    /// a subagent Claude's own TUI still shows as live.
    #[test]
    #[serial]
    fn test_resumed_agent_posttooluse_resolves_actor_not_unknown() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        // Pre-seed agent_owner as if this agent_id was spawned + stopped
        // once already (see the sequential test above for the full path).
        db.kv_set(&agent_owner_kv_key("sess-1", "agent-x"), Some("nova"))
            .unwrap();
        let ctx = make_ctx();

        let raw_resume = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-x",
            "agent_type": "general",
            "prompt_id": "p2",
        });
        let mut payload_resume = HookPayload::from_claude(raw_resume);
        let _ = route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload_resume);
        assert!(db.get_instance_by_agent_id("agent-x").unwrap().is_some());

        let raw_post = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-x",
            "tool_name": "Read",
            "tool_input": {},
        });
        let mut payload_post = HookPayload::from_claude(raw_post);
        let (exit_code, _stdout, _ack, timing) =
            route_claude_hook(&db, &ctx, HOOK_POST, &mut payload_post);
        assert_eq!(exit_code, 0);
        assert_ne!(
            timing.result,
            Some("unknown_subagent_actor"),
            "the resumed agent's own row must resolve, not fail closed as unknown"
        );
    }

    /// Property: concurrent duplicate hook delivery (the other live finding)
    /// combined with a resume must still converge on the one true owner —
    /// no split attribution, no lost row allocation.
    #[test]
    #[serial]
    fn test_concurrent_resumed_subagent_start_resolves_to_same_owner() {
        crate::config::Config::init();
        let (_dir, hcom_dir, _test_home, _guard) = isolated_test_env();
        let db_path = hcom_dir.join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        db.kv_set(&agent_owner_kv_key("sess-1", "agent-x"), Some("nova"))
            .unwrap();

        let n = 4;
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let path = db_path.clone();
                std::thread::spawn(move || {
                    let db = HcomDb::open_raw(&path).unwrap();
                    let ctx = make_ctx();
                    let raw = serde_json::json!({
                        "session_id": "sess-1",
                        "agent_id": "agent-x",
                        "agent_type": "general",
                        "prompt_id": "p-resume",
                    });
                    let mut payload = HookPayload::from_claude(raw);
                    route_claude_hook(&db, &ctx, HOOK_SUBAGENT_START, &mut payload)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let name = db.get_instance_by_agent_id("agent-x").unwrap();
        assert!(
            name.is_some(),
            "concurrent resumed SubagentStart must not fail closed"
        );
        let row = db.get_instance_full(&name.unwrap()).unwrap().unwrap();
        assert_eq!(row.parent_name.as_deref(), Some("nova"));
    }

    // ---- Duplicate hook delivery idempotency (SubagentStop) ----

    /// Property: the claim key must collapse a byte-identical duplicate
    /// invocation, but must NOT collapse a same-`prompt_id` invocation whose
    /// payload actually differs (SubagentStop legitimately re-fires within
    /// one prompt/continuation after an exit_code=2 delivery — we have not
    /// established Claude changes `prompt_id` for that re-fire), and must
    /// not collapse a genuinely different `prompt_id` either.
    #[test]
    fn test_subagent_stop_inflight_key_semantics() {
        let raw = serde_json::json!({"agent_id": "a1", "prompt_id": "p1", "x": 1});
        let raw_dup = serde_json::json!({"agent_id": "a1", "prompt_id": "p1", "x": 1});
        assert_eq!(
            subagent_stop_inflight_key("sess-1", "a1", &raw),
            subagent_stop_inflight_key("sess-1", "a1", &raw_dup),
            "byte-identical payloads must collapse to the same key"
        );

        let raw_same_prompt_diff_payload =
            serde_json::json!({"agent_id": "a1", "prompt_id": "p1", "x": 2});
        assert_ne!(
            subagent_stop_inflight_key("sess-1", "a1", &raw),
            subagent_stop_inflight_key("sess-1", "a1", &raw_same_prompt_diff_payload),
            "same prompt_id with different payload content must not collapse"
        );

        let raw_diff_prompt = serde_json::json!({"agent_id": "a1", "prompt_id": "p2", "x": 1});
        assert_ne!(
            subagent_stop_inflight_key("sess-1", "a1", &raw),
            subagent_stop_inflight_key("sess-1", "a1", &raw_diff_prompt),
            "different prompt_id must not collapse"
        );
    }

    /// Property: while a claim is held, an identical repeat loses (concurrency
    /// guard), and a same-prompt-but-different payload gets its own claim.
    #[test]
    #[serial]
    fn test_claim_subagent_stop_collapses_duplicate_invocation() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        let raw = serde_json::json!({"agent_id": "agent-x", "prompt_id": "p1"});
        let _first_claim = SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw)
            .expect("first claim must succeed");
        assert!(
            SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw).is_none(),
            "duplicate identical invocation must not re-claim while held"
        );

        let raw_different_payload =
            serde_json::json!({"agent_id": "agent-x", "prompt_id": "p1", "note": "different"});
        let _different_claim =
            SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw_different_payload)
                .expect("same prompt_id but different payload content must be its own claim");
    }

    /// The claim is a transient concurrency guard, not a session-long
    /// tombstone. Two distinct SubagentStop invocations can carry identical
    /// payloads. While one is
    /// in-flight the identical duplicate must lose (concurrency dedup), but
    /// once `SubagentStopClaim` releases it, a later identical stop must be able
    /// to re-claim and be processed — not suppressed forever.
    #[test]
    #[serial]
    fn test_stop_claim_released_lets_later_identical_stop_reclaim() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        let raw = serde_json::json!({
            "agent_id": "agent-x",
            "prompt_id": "p1",
            "last_assistant_message": "Done.",
        });
        let key = subagent_stop_inflight_key("sess-1", "agent-x", &raw);

        let first_claim = SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw)
            .expect("first invocation claims");
        assert!(
            SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw).is_none(),
            "a concurrent duplicate still in-flight must lose the claim"
        );
        drop(first_claim);
        assert!(
            db.kv_get(&key).unwrap().is_none(),
            "guard drop must release the claim key"
        );
        let _later_claim = SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw)
            .expect("a later, genuinely distinct stop with a byte-identical payload must re-claim");
    }

    #[test]
    #[serial]
    fn test_stop_claim_atomically_replaces_dead_owner() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        let raw = serde_json::json!({"agent_id": "agent-x", "prompt_id": "p1"});
        let key = subagent_stop_inflight_key("sess-1", "agent-x", &raw);
        let dead_owner = serde_json::to_string(&SubagentStopOwner {
            owner_token: "crashed-owner".to_string(),
            pid: u32::MAX,
            process_start: "dead-process".to_string(),
        })
        .unwrap();
        db.kv_set(&key, Some(&dead_owner)).unwrap();

        let claim = SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw)
            .expect("a dead process must not leave a permanent tombstone");
        let replacement = db.kv_get(&key).unwrap().unwrap();
        assert_ne!(replacement, dead_owner);
        assert_eq!(replacement, claim.value);
    }

    #[test]
    #[serial]
    fn test_stop_claim_drop_deletes_only_its_own_token() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        let raw = serde_json::json!({"agent_id": "agent-x", "prompt_id": "p1"});
        let key = subagent_stop_inflight_key("sess-1", "agent-x", &raw);
        let claim = SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw)
            .expect("first invocation claims");

        let replacement = r#"{"owner_token":"replacement","pid":1,"process_start":"other"}"#;
        db.kv_set(&key, Some(replacement)).unwrap();
        drop(claim);
        assert_eq!(
            db.kv_get(&key).unwrap().as_deref(),
            Some(replacement),
            "an old guard must not delete a newer owner's token"
        );
    }

    /// `subagent_stop` must release its claim so it cannot outlive the
    /// invocation. Uses
    /// a zero timeout so the poll returns immediately without blocking.
    #[test]
    #[serial]
    fn test_subagent_stop_releases_its_claim_on_completion() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id, subagent_timeout)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0, 0)",
                [],
            )
            .unwrap();
        // name_announced=1 -> skip the dormant idle gate and go straight to the
        // poll, which returns immediately (timeout 0) with no message.
        insert_subagent_row(&db, "nova_task_1", "agent-x", "nova", "sess-1");
        db.conn()
            .execute(
                "UPDATE instances SET name_announced = 1 WHERE name = 'nova_task_1'",
                [],
            )
            .unwrap();

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-x",
            "prompt_id": "p1",
            "last_assistant_message": "Done.",
        });
        let _ = subagent_stop(&db, "sess-1", &raw);

        assert!(
            db.kv_get(&subagent_stop_inflight_key("sess-1", "agent-x", &raw))
                .unwrap()
                .is_none(),
            "subagent_stop must not leave its claim behind as a permanent tombstone"
        );
    }

    /// Property: `subagent_stop` must check the claim *before* the idle gate
    /// and before `poll_messages` — a duplicate invocation must skip all
    /// processing, not just the final teardown. This is what the live
    /// teardown-window failure traced back to: with duplicate hook
    /// registrations, one invocation could receive a delivered message
    /// (exit_code=2) while the other, unclaimed, independently timed out and
    /// deleted the row out from under it.
    #[test]
    #[serial]
    fn test_subagent_stop_skips_all_processing_when_already_claimed() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        insert_subagent_row(&db, "nova_task_1", "agent-x", "nova", "sess-1");

        let raw = serde_json::json!({
            "session_id": "sess-1",
            "agent_id": "agent-x",
            "prompt_id": "p1",
        });
        // Simulate a concurrent duplicate having already claimed this exact
        // invocation (and still in-flight, so the claim is unreleased).
        let _existing_claim = SubagentStopClaim::acquire(&db, "sess-1", "agent-x", &raw)
            .expect("the simulated concurrent invocation must hold the claim");

        let (exit_code, stdout) = subagent_stop(&db, "sess-1", &raw);
        assert_eq!(exit_code, 0);
        assert!(stdout.is_empty());
        assert!(
            db.get_instance_by_agent_id("agent-x").unwrap().is_some(),
            "an already-claimed duplicate must not delete the row"
        );
        let stopped_events: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'life'
                 AND json_extract(data, '$.action') = 'stopped'
                 AND json_extract(data, '$.snapshot.agent_id') = 'agent-x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stopped_events, 0,
            "an already-claimed duplicate must not log a second life.stopped event"
        );
    }

    /// Property: genuinely concurrent duplicate SubagentStop hook delivery
    /// (separate DB connections, as separate hook processes would be) for
    /// the same dormant subagent must produce exactly one teardown — one
    /// life.stopped event, one row deletion — not one per invocation.
    #[test]
    #[serial]
    fn test_concurrent_duplicate_subagent_stop_produces_one_teardown() {
        crate::config::Config::init();
        let (_dir, hcom_dir, _test_home, _guard) = isolated_test_env();
        let db_path = hcom_dir.join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        insert_subagent_row(&db, "nova_task_1", "agent-x", "nova", "sess-1");

        let n = 4;
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let path = db_path.clone();
                std::thread::spawn(move || {
                    let db = HcomDb::open_raw(&path).unwrap();
                    // Identical payload from every "duplicate hook registration".
                    let raw = serde_json::json!({
                        "session_id": "sess-1",
                        "agent_id": "agent-x",
                        "prompt_id": "p1",
                    });
                    subagent_stop(&db, "sess-1", &raw)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert!(
            db.get_instance_by_agent_id("agent-x").unwrap().is_none(),
            "the subagent must end up stopped exactly like a single invocation would"
        );
        let stopped_events: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'life'
                 AND json_extract(data, '$.action') = 'stopped'
                 AND json_extract(data, '$.snapshot.agent_id') = 'agent-x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stopped_events, 1,
            "concurrent duplicate delivery must log exactly one life.stopped event, not {n}"
        );
    }

    /// Property: SessionEnd sweeps agent_owner and subagent_stop_inflight
    /// entries the same way it sweeps spawn_owner — only for its own
    /// session.
    #[test]
    #[serial]
    fn test_sessionend_cleans_agent_owner_and_stop_inflight_keys_too() {
        crate::config::Config::init();
        let (_dir, _guard, db) = make_isolated_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, status_time, created_at, last_event_id)
                 VALUES ('nova', 'sess-1', 'claude', 'listening', 'start', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        let ctx = make_ctx();

        db.kv_set(&agent_owner_kv_key("sess-1", "agent-x"), Some("nova"))
            .unwrap();
        db.kv_set(&agent_owner_kv_key("sess-other", "agent-x"), Some("other"))
            .unwrap();
        let stop_raw = serde_json::json!({"agent_id": "agent-x", "prompt_id": "p1"});
        db.kv_set(
            &subagent_stop_inflight_key("sess-1", "agent-x", &stop_raw),
            Some("1"),
        )
        .unwrap();
        db.kv_set(
            &subagent_stop_inflight_key("sess-other", "agent-x", &stop_raw),
            Some("1"),
        )
        .unwrap();

        let raw = serde_json::json!({"session_id": "sess-1", "reason": "clear"});
        let mut payload = HookPayload::from_claude(raw);
        let _ = route_claude_hook(&db, &ctx, HOOK_SESSIONEND, &mut payload);

        assert!(
            db.kv_get(&agent_owner_kv_key("sess-1", "agent-x"))
                .unwrap()
                .is_none()
        );
        assert!(
            db.kv_get(&subagent_stop_inflight_key("sess-1", "agent-x", &stop_raw))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.kv_get(&agent_owner_kv_key("sess-other", "agent-x"))
                .unwrap(),
            Some("other".to_string()),
            "a different session's agent_owner keys must survive"
        );
        assert_eq!(
            db.kv_get(&subagent_stop_inflight_key(
                "sess-other",
                "agent-x",
                &stop_raw
            ))
            .unwrap(),
            Some("1".to_string()),
            "a different session's stop-claim keys must survive"
        );
    }
}
