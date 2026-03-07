//! Claude Code hook handler, settings management, and subagent lifecycle.
//!
//! `tools/claude/subagent.py`, and `tools/claude/settings.py`.
//!
//! Claude has 9 hook types: sessionstart, userpromptsubmit, pre, post, poll,
//! notify, subagent-start, subagent-stop, sessionend.
//!
//! Dispatcher Flow:
//!   1. Parse stdin JSON, build HcomContext + HookPayload
//!   2. Correct session_id (fork bug workaround)
//!   3. SessionStart → handle directly (no instance resolution)
//!   4. Task pre/post → start_task/end_task
//!   5. Subagent context check → route to subagent handlers
//!   6. Resolve parent instance → dispatch to parent handlers
//!
//! Settings:
//!   - ~/.claude/settings.json: hook commands, HCOM env var, permissions
//!   - setup_claude_hooks() / verify_claude_hooks_installed() / remove_claude_hooks()
//!
//! std::process::exit(). Exit codes are returned through the call chain.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

use crate::bootstrap;
use crate::config::HcomConfig;
use crate::db::{HcomDb, InstanceRow};
use crate::hooks::common;
use crate::hooks::family;
use crate::hooks::HookPayload;
use crate::instances;
use crate::log;
use crate::messages;
use crate::paths;
use crate::shared::constants::{BIND_MARKER_RE, MAX_MESSAGES_PER_DELIVERY};
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_INACTIVE, ST_LISTENING};

// ==================== Constants ====================

const HOOK_SESSIONSTART: &str = "sessionstart";
const HOOK_USERPROMPTSUBMIT: &str = "userpromptsubmit";
const HOOK_PRE: &str = "pre";
const HOOK_POST: &str = "post";
const HOOK_NOTIFY: &str = "notify";
const HOOK_SUBAGENT_START: &str = "subagent-start";
const HOOK_SUBAGENT_STOP: &str = "subagent-stop";
const HOOK_SESSIONEND: &str = "sessionend";
const HOOK_POLL: &str = "poll";

// ==================== Entry Point ====================

/// Handle a Claude hook — entry point from router.
///
/// Reads JSON from stdin, builds context, dispatches to appropriate handler.
/// Returns exit code (0 = success/non-participant, 2 = message delivered).
///
pub fn handle_claude_hook(hook_type: &str) -> i32 {
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

    // Open DB
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
    let payload = HookPayload::from_claude(&raw);

    // Dispatch with panic safety (matches Gemini's catch_unwind pattern)
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch_claude_hook(&db, &ctx, hook_type, &payload)
    }));

    let (exit_code, stdout, timing) = match result {
        Ok(r) => r,
        Err(_) => {
            log::log_error(
                "hooks",
                "claude.dispatch.panic",
                &format!("hook={}", hook_type),
            );
            return 0;
        }
    };

    // Output result
    if !stdout.is_empty() {
        print!("{}", stdout);
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

// ==================== Dispatch Timing ====================

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
        let mut parts = vec![
            format!("hook={}", hook_type),
        ];
        if !tool_name.is_empty() {
            parts.push(format!("tool={}", tool_name));
        }
        if let Some(name) = instance {
            parts.push(format!("instance={}", name));
        }
        if let Some(v) = self.init_ms { parts.push(format!("init_ms={:.2}", v)); }
        if let Some(v) = self.session_ms { parts.push(format!("session_ms={:.2}", v)); }
        if let Some(v) = self.resolve_ms { parts.push(format!("resolve_ms={:.2}", v)); }
        if let Some(v) = self.bind_ms { parts.push(format!("bind_ms={:.2}", v)); }
        if let Some(v) = self.handler_ms { parts.push(format!("handler_ms={:.2}", v)); }
        if let Some(v) = self.subagent_check_ms { parts.push(format!("subagent_check_ms={:.2}", v)); }
        if let Some(v) = self.task_ms { parts.push(format!("task_ms={:.2}", v)); }
        parts.push(format!("total_ms={:.2}", total_ms));
        parts.push(format!("exit_code={}", exit_code));
        if let Some(ctx) = self.context { parts.push(format!("context={}", ctx)); }
        if let Some(r) = self.result { parts.push(format!("result={}", r)); }
        parts.join(" ")
    }
}

// ==================== Dispatcher ====================

/// Core dispatcher — routes to appropriate handler.
///
/// Returns (exit_code, stdout_string, timing).
fn dispatch_claude_hook(
    db: &HcomDb,
    ctx: &HcomContext,
    hook_type: &str,
    payload: &HookPayload,
) -> (i32, String, DispatchTiming) {
    let dispatch_start = Instant::now();
    let mut timing = DispatchTiming::default();

    // Ensure directories and init DB
    if !paths::ensure_hcom_directories() {
        return (0, String::new(), timing);
    }

    timing.init_ms = Some(dispatch_start.elapsed().as_secs_f64() * 1000.0);

    // Correct session_id (fork bug workaround)
    let session_start = Instant::now();
    let session_id = get_real_session_id(
        &payload.raw,
        ctx.claude_env_file.as_deref(),
        ctx.is_fork,
    );

    // Update raw with corrected session_id for downstream use
    let mut raw = payload.raw.clone();
    if let Some(obj) = raw.as_object_mut() {
        obj.insert("session_id".into(), Value::String(session_id.clone()));
    }
    timing.session_ms = Some(session_start.elapsed().as_secs_f64() * 1000.0);

    // SessionStart — no instance resolution needed
    if hook_type == HOOK_SESSIONSTART {
        let handler_start = Instant::now();
        let result = handle_sessionstart(db, ctx, &session_id, &raw);
        timing.handler_ms = Some(handler_start.elapsed().as_secs_f64() * 1000.0);
        return (result.0, result.1, timing);
    }

    // Task transitions
    let tool_name = payload.tool_name.as_str();
    if hook_type == HOOK_PRE && tool_name == "Task" {
        let task_start = Instant::now();
        let (stdout, exit_code) = start_task(db, &session_id, &raw);
        timing.task_ms = Some(task_start.elapsed().as_secs_f64() * 1000.0);
        return (exit_code, stdout, timing);
    }
    if hook_type == HOOK_POST && tool_name == "Task" {
        let task_start = Instant::now();
        let stdout = end_task(db, &session_id, &raw, false).unwrap_or_default();
        timing.task_ms = Some(task_start.elapsed().as_secs_f64() * 1000.0);
        return (0, stdout, timing);
    }

    // Subagent context check
    let subagent_check_start = Instant::now();
    let is_in_subagent_ctx = in_subagent_context(db, &session_id);
    timing.subagent_check_ms = Some(subagent_check_start.elapsed().as_secs_f64() * 1000.0);

    if is_in_subagent_ctx {
        timing.context = Some("subagent");

        if hook_type == HOOK_USERPROMPTSUBMIT {
            let transcript_path = payload.transcript_path.as_str();
            cleanup_dead_subagents(db, &session_id, transcript_path);
            // Fall through to parent handler for PTY message delivery
        }

        if hook_type == HOOK_SUBAGENT_START {
            let agent_id = raw.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
            let agent_type = raw
                .get("agent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !agent_id.is_empty() && !agent_type.is_empty() {
                track_subagent(db, &session_id, agent_id, agent_type);
            }
            let output = subagent_start(&raw);
            if let Some(out) = output {
                return (0, serde_json::to_string(&out).unwrap_or_default(), timing);
            }
            return (0, String::new(), timing);
        }

        if hook_type == HOOK_SUBAGENT_STOP {
            let (exit_code, stdout) = subagent_stop(db, &raw, &session_id);
            return (exit_code, stdout, timing);
        }

        if hook_type == HOOK_NOTIFY {
            return (0, String::new(), timing);
        }

        if hook_type == HOOK_PRE || hook_type == HOOK_POST {
            if tool_name == "Bash" {
                let tool_input = &payload.tool_input;
                let command = tool_input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if hook_type == HOOK_POST && extract_name(command).is_some() {
                    let (exit_code, stdout) = subagent_posttooluse(db, &raw);
                    return (exit_code, stdout, timing);
                }
            }
            return (0, String::new(), timing);
        }
    }

    // Resolve parent instance
    let resolve_start = Instant::now();
    let (instance_name, updates, _is_matched_resume) =
        common::init_hook_context(db, ctx, &session_id, &payload.transcript_path);
    timing.resolve_ms = Some(resolve_start.elapsed().as_secs_f64() * 1000.0);

    // Vanilla binding for Bash post hook
    let bind_start = Instant::now();
    let (instance_name, updates) = if hook_type == HOOK_POST && tool_name == "Bash" {
        let bound = bind_vanilla_from_marker(db, &raw, &session_id, instance_name.as_deref());
        match bound {
            Some(name) => {
                let mut u = updates;
                u.entry("directory".to_string())
                    .or_insert_with(|| Value::String(ctx.cwd.to_string_lossy().to_string()));
                if !payload.transcript_path.is_empty() {
                    u.entry("transcript_path".to_string())
                        .or_insert_with(|| Value::String(payload.transcript_path.clone()));
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
        return (0, String::new(), timing);
    };

    let instance_data = match db.get_instance_full(instance_name) {
        Ok(Some(data)) => data,
        _ => {
            timing.result = Some("no_instance_data");
            return (0, String::new(), timing);
        }
    };

    // Dispatch to handler
    timing.instance = Some(instance_name.clone());
    let handler_start = Instant::now();
    let result = match hook_type {
        HOOK_PRE => handle_pretooluse(db, payload, instance_name),
        HOOK_POST => handle_posttooluse(db, ctx, payload, instance_name, &instance_data, &updates),
        HOOK_POLL => handle_stop(db, ctx, instance_name, &instance_data),
        HOOK_NOTIFY => handle_notify(db, payload, instance_name, &updates),
        HOOK_USERPROMPTSUBMIT => {
            handle_userpromptsubmit(db, ctx, payload, instance_name, &updates, &instance_data)
        }
        HOOK_SESSIONEND => handle_sessionend(db, instance_name, &raw, &updates),
        _ => (0, String::new()),
    };
    timing.handler_ms = Some(handler_start.elapsed().as_secs_f64() * 1000.0);

    (result.0, result.1, timing)
}

// ==================== Fork Bug Workaround (1E.12) ====================

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

    if let Some(env_file) = env_file {
        if is_fork {
            let path = Path::new(env_file);
            let parts: Vec<&str> = path
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect();
            if let Some(idx) = parts.iter().position(|&p| p == "session-env") {
                if idx + 1 < parts.len() {
                    let candidate = parts[idx + 1];
                    // Sanity: UUID format (36 chars, 4 hyphens)
                    if candidate.len() == 36 && candidate.chars().filter(|&c| c == '-').count() == 4
                    {
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
        }
    }

    hook_session_id.to_string()
}

// ==================== SessionStart (1E.2) ====================

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
    if let Some(ref env_file) = ctx.claude_env_file {
        if !session_id.is_empty() {
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(env_file) {
                use std::io::Write;
                let _ = writeln!(f, "export HCOM_CLAUDE_UNIX_SESSION_ID={}", session_id);
            }
        }
    }

    // Compaction recovery: re-inject bootstrap
    if source == "compact" && !session_id.is_empty() {
        if let Some(output) = handle_compact_recovery(db, ctx, session_id, process_id) {
            return (0, serde_json::to_string(&output).unwrap_or_default());
        }
    }

    // Vanilla instance - show hint
    if process_id.is_none() || session_id.is_empty() {
        let hcom_cmd = super::common::build_hcom_command();
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
    crate::relay::worker::maybe_auto_spawn();

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
            process_id.and_then(|pid| instances::resolve_process_binding(db, Some(pid)))
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
        instances::bind_session_to_process(db, session_id, Some(process_id));

    // Orphaned PTY: process_id exists but no binding (e.g., after /clear)
    if instance_name.is_none() {
        instance_name =
            instances::create_orphaned_pty_identity(db, session_id, Some(process_id), "claude");
        log::log_info(
            "hooks",
            "sessionstart.orphan_created",
            &format!(
                "instance={:?} process_id={}",
                instance_name, process_id
            ),
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
    instances::capture_and_store_launch_context(db, &instance_name);

    // Set status
    instances::set_status(db, &instance_name, ST_LISTENING, "start", "", "", None, None);

    // Terminal title
    common::set_terminal_title(&instance_name);

    // Inject bootstrap
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

// ==================== Task Coordination ====================

/// PreToolUse Task: enter subagent context.
///
/// Returns (stdout, exit_code).
fn start_task(db: &HcomDb, session_id: &str, raw: &Value) -> (String, i32) {
    log::log_info("hooks", "start_task.enter", &format!("session_id={}", session_id));

    let instance_name = match db.get_session_binding(session_id) {
        Ok(Some(name)) => name,
        _ => return (String::new(), 0),
    };

    // Set running_tasks.active = true
    let instance_data = match db.get_instance_full(&instance_name) {
        Ok(Some(data)) => data,
        _ => return (String::new(), 0),
    };

    let mut running_tasks =
        instances::parse_running_tasks(instance_data.running_tasks.as_deref());
    running_tasks.active = true;
    let rt_json = serde_json::json!({
        "active": running_tasks.active,
        "subagents": running_tasks.subagents,
    });
    let mut updates = serde_json::Map::new();
    updates.insert("running_tasks".into(), Value::String(rt_json.to_string()));
    instances::update_instance_position(db, &instance_name, &updates);

    // Set status with task detail
    let tool_input = raw.get("tool_input").cloned().unwrap_or(Value::Object(Default::default()));
    let detail = family::extract_tool_detail("claude", "Task", &tool_input);
    instances::set_status(
        db,
        &instance_name,
        ST_ACTIVE,
        "tool:Task",
        &detail,
        "",
        None,
        None,
    );

    // Append hcom instructions to Task prompt
    let original_prompt = tool_input
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !original_prompt.is_empty() {
        let hcom_cmd = super::common::build_hcom_command();
        let hcom_hint = format!("\n\n---\nTo use hcom: run `{hcom_cmd} start --name <your-agent-id>` first.");
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
        return (serde_json::to_string(&output).unwrap_or_default(), 0);
    }

    (String::new(), 0)
}

/// PostToolUse Task: deliver freeze-period messages.
///
/// Returns Option<String> — JSON stdout if messages were delivered.
/// Dispatcher writes this to stdout before returning exit code.
fn end_task(db: &HcomDb, session_id: &str, _raw: &Value, interrupted: bool) -> Option<String> {
    let instance_name = match db.get_session_binding(session_id) {
        Ok(Some(name)) => name,
        _ => return None,
    };

    let instance_data = match db.get_instance_full(&instance_name) {
        Ok(Some(data)) => data,
        _ => return None,
    };

    if interrupted {
        return None;
    }

    let freeze_event_id = instance_data.last_event_id;
    let (last_event_id, stdout) = deliver_freeze_messages(db, &instance_name, freeze_event_id);

    let mut updates = serde_json::Map::new();
    updates.insert("last_event_id".into(), serde_json::json!(last_event_id));
    instances::update_instance_position(db, &instance_name, &updates);

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
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
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

    (last_id, Some(serde_json::to_string(&output).unwrap_or_default()))
}

// ==================== PreToolUse (1E.4) ====================

/// Parent PreToolUse: status tracking with tool-specific detail.
fn handle_pretooluse(
    db: &HcomDb,
    payload: &HookPayload,
    instance_name: &str,
) -> (i32, String) {
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

// ==================== PostToolUse (1E.5) ====================

/// Parent PostToolUse: bootstrap, messages, vanilla binding.
fn handle_posttooluse(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
    instance_name: &str,
    instance_data: &InstanceRow,
    updates: &serde_json::Map<String, Value>,
) -> (i32, String) {
    let tool_name = payload.tool_name.as_str();
    let mut outputs: Vec<Value> = Vec::new();

    // Clear blocked status if tool completed
    if instance_data.status == ST_BLOCKED {
        instances::set_status(
            db,
            instance_name,
            ST_ACTIVE,
            &format!("approved:{}", tool_name),
            "",
            "",
            None,
            None,
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
    if let Some(output) = get_posttooluse_messages(db, instance_name) {
        outputs.push(output);
    }

    if !outputs.is_empty() {
        let combined = combine_posttooluse_outputs(&outputs);
        return (
            0,
            serde_json::to_string(&combined).unwrap_or_default(),
        );
    }

    (0, String::new())
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
fn get_posttooluse_messages(db: &HcomDb, instance_name: &str) -> Option<Value> {
    let (deliver_messages, model_context) = common::deliver_pending_messages(db, instance_name);
    if deliver_messages.is_empty() {
        return None;
    }

    let model_context = model_context?;

    // Claude needs user-facing display in addition to model context
    let get_instance_data = common::make_instance_lookup(&db);
    let hints = common::load_config_hints();
    let get_config_hints = || hints.clone();
    let user_display = messages::format_hook_messages(
        &deliver_messages,
        instance_name,
        &get_instance_data,
        &get_config_hints,
        None,
    );

    Some(serde_json::json!({
        "systemMessage": user_display,
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": model_context,
        },
    }))
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

// ==================== Stop/Poll (1E.6) ====================

/// Parent Stop hook: message delivery when Claude goes idle.
fn handle_stop(
    db: &HcomDb,
    ctx: &HcomContext,
    instance_name: &str,
    _instance_data: &InstanceRow,
) -> (i32, String) {
    log::log_info("hooks", "stop.enter", &format!("instance={}", instance_name));

    // PTY mode: exit immediately, PTY wrapper handles injection (unchanged)
    if ctx.is_pty_mode {
        instances::set_status(db, instance_name, ST_LISTENING, "", "", "", None, None);
        common::notify_hook_instance_with_db(db, instance_name);
        return (0, String::new());
    }

    // Non-PTY: instant non-blocking check + deliver
    let (_delivered, formatted) = common::deliver_pending_messages(db, instance_name);
    if let Some(formatted) = formatted {
        // Messages found — deliver them (exit code 2 = "block" → triggers new turn)
        let output = serde_json::json!({
            "decision": "block",
            "reason": formatted,
        });
        return (2, serde_json::to_string(&output).unwrap_or_default());
    }

    // No messages — set status to LISTENING and exit immediately
    instances::set_status(db, instance_name, ST_LISTENING, "", "", "", None, None);
    (0, String::new())
}

// ==================== UserPromptSubmit (1E.3) ====================

/// Parent UserPromptSubmit: fallback bootstrap, PTY mode message delivery.
fn handle_userpromptsubmit(
    db: &HcomDb,
    ctx: &HcomContext,
    _payload: &HookPayload,
    instance_name: &str,
    updates: &serde_json::Map<String, Value>,
    instance_data: &InstanceRow,
) -> (i32, String) {
    let name_announced = instance_data.name_announced != 0;

    // Persist updates
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, updates);
    }

    // Bootstrap fallback (rarely fires)
    if !name_announced && ctx.is_launched {
        if let Some(bootstrap_text) =
            common::inject_bootstrap_once(db, ctx, instance_name, instance_data, "claude")
        {
            let output = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": bootstrap_text,
                }
            });
            paths::increment_flag_counter("instance_count");
            instances::set_status(db, instance_name, ST_ACTIVE, "prompt", "", "", None, None);
            return (
                0,
                serde_json::to_string(&output).unwrap_or_default(),
            );
        }
    }

    // PTY mode: deliver messages
    if ctx.is_pty_mode {
        let raw_messages = db.get_unread_messages(instance_name);
        if !raw_messages.is_empty() {
            let messages: Vec<Value> = raw_messages.iter().map(message_to_value).collect();

            let deliver = if messages.len() > MAX_MESSAGES_PER_DELIVERY {
                messages[..MAX_MESSAGES_PER_DELIVERY].to_vec()
            } else {
                messages
            };

            let last_id = deliver
                .last()
                .and_then(|m| m.get("event_id").and_then(|v| v.as_i64()))
                .unwrap_or(0);
            let mut pos_updates = serde_json::Map::new();
            pos_updates.insert("last_event_id".into(), serde_json::json!(last_id));
            instances::update_instance_position(db, instance_name, &pos_updates);

            // Format messages
            let get_instance_data = common::make_instance_lookup(db);
            let hints = common::load_config_hints();
            let get_config_hints = || hints.clone();

            let user_display = messages::format_hook_messages(
                &deliver,
                instance_name,
                &get_instance_data,
                &get_config_hints,
                None,
            );
            let model_context = messages::format_messages_json(
                &deliver,
                instance_name,
                &get_instance_data,
                &get_config_hints,
                None,
            );

            let sender = deliver
                .first()
                .and_then(|m| m.get("from").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            let display = instances::get_display_name(db, sender);
            let msg_ts = deliver
                .last()
                .and_then(|m| m.get("timestamp").and_then(|v| v.as_str()))
                .unwrap_or("");
            instances::set_status(
                db,
                instance_name,
                ST_ACTIVE,
                &format!("deliver:{}", display),
                "",
                msg_ts,
                None,
                None,
            );

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
            );
        }
    }

    // Non-PTY: deliver pending messages (closes gap from non-blocking Stop hook)
    let (_delivered, formatted) = common::deliver_pending_messages(db, instance_name);
    if let Some(formatted) = formatted {
        let output = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": formatted,
            }
        });
        return (0, serde_json::to_string(&output).unwrap_or_default());
    }

    // Set status to active
    instances::set_status(db, instance_name, ST_ACTIVE, "prompt", "", "", None, None);
    (0, String::new())
}

// ==================== Notification (1E.9) ====================

/// Parent Notification: update status to blocked.
fn handle_notify(
    db: &HcomDb,
    payload: &HookPayload,
    instance_name: &str,
    updates: &serde_json::Map<String, Value>,
) -> (i32, String) {
    let message = payload
        .raw
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Filter out generic "waiting for input"
    if message == "Claude is waiting for your input" {
        return (0, String::new());
    }

    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, updates);
    }
    instances::set_status(db, instance_name, ST_BLOCKED, message, "", "", None, None);
    (0, String::new())
}

// ==================== SessionEnd (1E.10) ====================

/// Parent SessionEnd: finalize session and stop instance.
fn handle_sessionend(
    db: &HcomDb,
    instance_name: &str,
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
    (0, String::new())
}

// ==================== Subagent Context (1E.11) ====================

/// Check if session is in subagent context (Task active).
pub fn in_subagent_context(db: &HcomDb, session_id: &str) -> bool {
    // Try as session_id first (fast path for hooks)
    let instance_name = match db.get_session_binding(session_id) {
        Ok(Some(name)) => name,
        _ => session_id.to_string(), // Try as instance name
    };

    let row: Option<Option<String>> = db
        .conn()
        .query_row(
            "SELECT running_tasks FROM instances WHERE name = ? LIMIT 1",
            rusqlite::params![instance_name],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok();

    match row {
        Some(Some(rt_json)) if !rt_json.is_empty() => {
            let rt = instances::parse_running_tasks(Some(&rt_json));
            rt.active
        }
        _ => false,
    }
}

/// Track subagent in parent's running_tasks.
fn track_subagent(db: &HcomDb, parent_session_id: &str, agent_id: &str, agent_type: &str) {
    log::log_info(
        "hooks",
        "track_subagent.enter",
        &format!(
            "session_id={} agent_id={} agent_type={}",
            parent_session_id, agent_id, agent_type
        ),
    );

    let instance_name = match db.get_session_binding(parent_session_id) {
        Ok(Some(name)) => name,
        _ => return,
    };

    let instance_data = match db.get_instance_full(&instance_name) {
        Ok(Some(data)) => data,
        _ => return,
    };

    let mut running_tasks =
        instances::parse_running_tasks(instance_data.running_tasks.as_deref());
    running_tasks.active = true;

    // Add subagent if not already tracked
    let already_tracked = running_tasks.subagents.iter().any(|s| {
        s.get("agent_id")
            .and_then(|v| v.as_str())
            == Some(agent_id)
    });

    if !already_tracked {
        running_tasks.subagents.push(serde_json::json!({
            "agent_id": agent_id,
            "type": agent_type,
        }));
        let rt_json = serde_json::json!({
            "active": running_tasks.active,
            "subagents": running_tasks.subagents,
        });
        let mut updates = serde_json::Map::new();
        updates.insert("running_tasks".into(), Value::String(rt_json.to_string()));
        instances::update_instance_position(db, &instance_name, &updates);
    }
}

/// Remove subagent from parent's running_tasks.
fn remove_subagent_from_parent(db: &HcomDb, parent_name: &str, agent_id: &str) {
    let parent_data = match db.get_instance_full(parent_name) {
        Ok(Some(data)) => data,
        _ => return,
    };

    let mut running_tasks =
        instances::parse_running_tasks(parent_data.running_tasks.as_deref());

    if running_tasks.subagents.is_empty() {
        return;
    }

    running_tasks.subagents.retain(|s| {
        s.get("agent_id").and_then(|v| v.as_str()) != Some(agent_id)
    });

    if running_tasks.subagents.is_empty() {
        running_tasks.active = false;
    }

    let rt_json = serde_json::json!({
        "active": running_tasks.active,
        "subagents": running_tasks.subagents,
    });
    let mut updates = serde_json::Map::new();
    updates.insert("running_tasks".into(), Value::String(rt_json.to_string()));
    instances::update_instance_position(db, parent_name, &updates);
}

/// Check for dead subagents by checking multiple death signals.
fn check_dead_subagents(
    db: &HcomDb,
    transcript_path: &str,
    running_tasks: &instances::RunningTasks,
    subagent_timeout: Option<i64>,
) -> Vec<String> {
    let timeout = subagent_timeout.unwrap_or_else(|| {
        HcomConfig::load(None).ok()
            .map(|c| c.subagent_timeout)
            .unwrap_or(120)
    });
    let stale_threshold = (timeout * 2) as u64;
    let transcript_dir = if transcript_path.is_empty() {
        None
    } else {
        Path::new(transcript_path).parent()
    };

    let mut dead = Vec::new();
    let now = crate::shared::constants::now_epoch_i64() as u64;

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

        let Some(transcript_dir) = transcript_dir else {
            dead.push(agent_id.to_string());
            continue;
        };

        let agent_transcript = transcript_dir.join(format!("agent-{}.jsonl", agent_id));
        match std::fs::metadata(&agent_transcript) {
            Err(_) => {
                dead.push(agent_id.to_string());
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

    let dead_ids =
        check_dead_subagents(db, transcript_path, &running_tasks, instance_data.subagent_timeout);
    if dead_ids.is_empty() {
        return;
    }

    for agent_id in &dead_ids {
        remove_subagent_from_parent(db, &instance_name, agent_id);

        // Stop the subagent instance if it exists
        if let Ok(Some(name)) = db.get_instance_by_agent_id(agent_id) {
            instances::set_status(
                db,
                &name,
                ST_INACTIVE,
                "exit:interrupted",
                "",
                "",
                None,
                None,
            );
            common::stop_instance(db, &name, "system", "interrupted");
        }
    }
}

// ==================== SubagentStart (1E.7) ====================

/// SubagentStart: surface agent_id to subagent.
fn subagent_start(raw: &Value) -> Option<Value> {
    let agent_id = raw.get("agent_id").and_then(|v| v.as_str())?;
    if agent_id.is_empty() {
        return None;
    }

    Some(serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SubagentStart",
            "additionalContext": format!("Your agent ID: {}", agent_id),
        }
    }))
}

// ==================== SubagentStop (1E.8) ====================

/// SubagentStop: message polling using agent_id, cleanup on exit.
///
/// Returns (exit_code, stdout). exit_code=2 means message delivered
/// (SubagentStop fires again). exit_code=0 means cleanup and stop.
fn subagent_stop(db: &HcomDb, raw: &Value, session_id: &str) -> (i32, String) {
    let agent_id = match raw.get("agent_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return (0, String::new()),
    };

    // Query subagent instance by agent_id
    let row: Option<(String, String, Option<String>)> = db
        .conn()
        .query_row(
            "SELECT name, transcript_path, parent_name FROM instances WHERE agent_id = ?",
            rusqlite::params![agent_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok();

    let Some((subagent_name, existing_transcript, parent_name)) = row else {
        // No instance = subagent never ran hcom start
        // Remove from parent's running_tasks to prevent stuck active state
        if let Ok(Some(parent)) = db.get_session_binding(session_id) {
            remove_subagent_from_parent(db, &parent, agent_id);
        }
        return (0, String::new());
    };

    // Store transcript_path if not already set
    if existing_transcript.is_empty() {
        if let Some(tp) = raw.get("agent_transcript_path").and_then(|v| v.as_str()) {
            if !tp.is_empty() {
                let mut updates = serde_json::Map::new();
                updates.insert("transcript_path".into(), Value::String(tp.to_string()));
                instances::update_instance_position(db, &subagent_name, &updates);
            }
        }
    }

    // Resolve timeout: parent override > global config
    let timeout = parent_name
        .as_ref()
        .and_then(|pn| db.get_instance_full(pn).ok().flatten())
        .and_then(|pd| pd.subagent_timeout)
        .unwrap_or_else(|| {
            HcomConfig::load(None).ok()
                .map(|c| c.subagent_timeout)
                .unwrap_or(120)
        }) as u64;

    let (exit_code, output, timed_out) = common::poll_messages(db, &subagent_name, timeout, false);

    let stdout = output
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();

    // exit_code=2: message delivered, subagent continues
    // exit_code=0: no message/timeout, cleanup
    if exit_code == 0 {
        let reason = if timed_out { "timeout" } else { "task_completed" };
        instances::set_status(
            db,
            &subagent_name,
            ST_INACTIVE,
            &format!("exit:{}", reason),
            "",
            "",
            None,
            None,
        );
        if let Some(ref pn) = parent_name {
            remove_subagent_from_parent(db, pn, agent_id);
        }
        common::stop_instance(db, &subagent_name, "subagent", reason);
    }

    (exit_code, stdout)
}

// ==================== Subagent PostToolUse ====================

/// Subagent PostToolUse: message delivery for subagents running hcom commands.
///
/// Returns (exit_code, stdout).
fn subagent_posttooluse(db: &HcomDb, raw: &Value) -> (i32, String) {
    let tool_input = raw
        .get("tool_input")
        .unwrap_or(&Value::Null);
    let tool_name = raw
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if tool_name != "Bash" {
        return (0, String::new());
    }

    let command = tool_input
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !command.contains("--name") {
        return (0, String::new());
    }

    let agent_id = match extract_name(command) {
        Some(name) => name,
        None => return (0, String::new()),
    };

    let subagent_name = match db.get_instance_by_agent_id(&agent_id) {
        Ok(Some(name)) => name,
        _ => return (0, String::new()),
    };

    // Check instance exists (row exists = participating)
    let _data = match db.get_instance_full(&subagent_name) {
        Ok(Some(data)) => data,
        _ => return (0, String::new()),
    };

    // Message delivery
    let raw_messages = db.get_unread_messages(&subagent_name);
    if raw_messages.is_empty() {
        return (0, String::new());
    }

    let messages: Vec<Value> = raw_messages.iter().map(message_to_value).collect();

    let deliver = if messages.len() > MAX_MESSAGES_PER_DELIVERY {
        messages[..MAX_MESSAGES_PER_DELIVERY].to_vec()
    } else {
        messages
    };

    // Advance cursor
    let last_id = deliver
        .last()
        .and_then(|m| m.get("event_id").and_then(|v| v.as_i64()))
        .unwrap_or(0);
    let mut updates = serde_json::Map::new();
    updates.insert("last_event_id".into(), serde_json::json!(last_id));
    instances::update_instance_position(db, &subagent_name, &updates);

    let get_instance_data = common::make_instance_lookup(db);
    let hints = common::load_config_hints();
    let get_config_hints = || hints.clone();
    let formatted = messages::format_messages_json(
        &deliver,
        &subagent_name,
        &get_instance_data,
        &get_config_hints,
        None,
    );

    let sender = deliver
        .first()
        .and_then(|m| m.get("from").and_then(|v| v.as_str()))
        .unwrap_or("unknown");
    let display = instances::get_display_name(db, sender);
    let msg_ts = deliver
        .last()
        .and_then(|m| m.get("timestamp").and_then(|v| v.as_str()))
        .unwrap_or("");
    instances::set_status(
        db,
        &subagent_name,
        ST_ACTIVE,
        &format!("deliver:{}", display),
        "",
        msg_ts,
        None,
        None,
    );

    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": formatted,
        }
    });
    (
        0,
        serde_json::to_string(&output).unwrap_or_default(),
    )
}

// ==================== Vanilla Binding ====================

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
    if let Some(current) = current_instance {
        if current != instance_name {
            return None;
        }
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
    updates.insert(
        "session_id".into(),
        Value::String(session_id.to_string()),
    );
    updates.insert("tool".into(), Value::String("claude".to_string()));
    instances::update_instance_position(db, instance_name, &updates);

    Some(instance_name.to_string())
}

// ==================== Helpers ====================

/// Convert a Message to a JSON Value for hook delivery.
///
/// Used by handle_userpromptsubmit and subagent_posttooluse for
/// message-to-JSON serialization.
// message_to_value is in common.rs (pub(crate))
use super::common::message_to_value;

/// Extract --name flag value from a bash command string.
fn extract_name(command: &str) -> Option<String> {
    RE_NAME_FLAG
        .captures(command)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

// ==================== Settings Management (1E.13) ====================
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
    ("Stop", "", "poll", None),
    ("SubagentStart", "", "subagent-start", None),
    ("SubagentStop", "", "subagent-stop", Some(86400)),
    ("Notification", "", "notify", None),
    ("SessionEnd", "", "sessionend", None),
];

/// Hook command suffixes for pattern detection.
const CLAUDE_HOOK_COMMANDS: &[&str] = &[
    "sessionstart", "userpromptsubmit", "pre", "post", "poll",
    "subagent-start", "subagent-stop", "notify", "sessionend",
];

/// Claude hook types for cleanup iteration.
const CLAUDE_HOOK_TYPES: &[&str] = &[
    "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop",
    "SubagentStart", "SubagentStop", "Notification", "SessionEnd",
];

// Static regexes for hot-path hook command detection
static RE_NAME_FLAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"--name\s+(\S+)").unwrap());
static RE_HCOM_COMMANDS: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = CLAUDE_HOOK_COMMANDS.join("|");
    Regex::new(&format!(r"\bhcom\s+({})\b", pattern)).unwrap()
});
static RE_HCOM_CLAUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bhcom\s+claude-").unwrap());
static RE_UVX_HCOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\buvx\s+hcom\s+claude-").unwrap());
static RE_HCOM_ACTIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bHCOM_ACTIVE.*hcom\.py").unwrap());
static RE_HCOM_PY_COMMANDS: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = CLAUDE_HOOK_COMMANDS.join("|");
    Regex::new(&format!(r#"hcom\.py["']?\s+({})\b"#, pattern)).unwrap()
});
static RE_SH_HCOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sh\s+-c.*hcom").unwrap());

/// Get path to Claude settings.json.
///
/// Uses `paths::get_project_root()` which respects HCOM_DIR:
/// - HCOM_DIR set → project_root is HCOM_DIR parent → {parent}/.claude/settings.json
/// - Otherwise → ~/.hcom parent = ~ → ~/.claude/settings.json
pub fn get_claude_settings_path() -> PathBuf {
    paths::get_project_root().join(".claude").join("settings.json")
}

/// Load and parse Claude settings.json. Returns None on error or missing file.
pub fn load_claude_settings(settings_path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(settings_path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Get hook command string. Uses ${HCOM} env var set in settings.json.
fn get_hook_command() -> String {
    "${HCOM}".to_string()
}

/// Format a single Claude permission pattern: `Bash(prefix cmd:*)`.
fn format_claude_permission(prefix: &str, cmd: &str) -> String {
    let suffix = if cmd.starts_with('-') { "" } else { ":*" };
    format!("Bash({} {}{})", prefix, cmd, suffix)
}

/// Build permission patterns for installation using detected prefix.
fn build_claude_permissions() -> Vec<String> {
    let prefix = super::common::build_hcom_command();
    SAFE_HCOM_COMMANDS
        .iter()
        .map(|cmd| format_claude_permission(&prefix, cmd))
        .collect()
}

/// Build ALL permission patterns (both "hcom" and "uvx hcom" prefixes) for removal.
fn build_all_claude_permission_patterns() -> Vec<String> {
    let mut patterns = Vec::new();
    for prefix in &["hcom", "uvx hcom"] {
        for cmd in SAFE_HCOM_COMMANDS {
            patterns.push(format_claude_permission(prefix, cmd));
        }
    }
    patterns
}

/// Check if a hook command string matches any hcom hook pattern.
fn is_hcom_hook_command(command: &str) -> bool {
    // Env var patterns: ${HCOM} or %HCOM%
    if command.contains("${HCOM}") || command.contains("$HCOM") || command.contains("%HCOM%") {
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

    let hooks = match obj.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        Some(h) => h,
        None => return false,
    };

    // Process each hook type
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
                    let command = hook
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    !is_hcom_hook_command(command)
                })
                .collect();

            if non_hcom_hooks.len() < hooks_field.len() {
                removed_any = true;
            }

            // Only keep matcher if it has non-hcom hooks remaining
            if !non_hcom_hooks.is_empty() {
                let mut matcher_copy = matcher.clone();
                matcher_copy["hooks"] = Value::Array(
                    non_hcom_hooks.into_iter().cloned().collect(),
                );
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

/// Set up hcom hooks in Claude settings.json.
///
/// - Removes existing hcom hooks first (clean slate)
/// - Adds all hooks from CLAUDE_HOOK_CONFIGS
/// - Sets HCOM environment variable
/// - Optionally adds permission patterns
/// - Uses atomic write for concurrent safety
///
/// Returns true on success.
pub fn setup_claude_hooks(include_permissions: bool) -> bool {
    let settings_path = get_claude_settings_path();
    if let Some(parent) = settings_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut settings = load_claude_settings(&settings_path).unwrap_or_else(|| {
        serde_json::json!({})
    });

    // Normalize hooks dict
    if !settings.get("hooks").is_some_and(|v| v.is_object()) {
        settings["hooks"] = serde_json::json!({});
    }

    // Remove existing hcom hooks
    remove_hcom_hooks_from_settings(&mut settings);

    // Build hook commands from CLAUDE_HOOK_CONFIGS
    let hook_cmd_base = get_hook_command();

    for &(hook_type, matcher, cmd_suffix, timeout) in CLAUDE_HOOK_CONFIGS {
        // Initialize or normalize hook_type to array
        if !settings["hooks"].get(hook_type).is_some_and(|v| v.is_array()) {
            settings["hooks"][hook_type] = serde_json::json!([]);
        }

        let mut hook_entry = serde_json::json!({
            "type": "command",
            "command": format!("{} {}", hook_cmd_base, cmd_suffix),
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
    settings["env"]["HCOM"] = Value::String(super::common::build_hcom_command());
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
        if let Some(perms) = settings.get_mut("permissions").and_then(|v| v.as_object_mut()) {
            if let Some(allow) = perms.get_mut("allow").and_then(|v| v.as_array_mut()) {
                let hcom_perms = build_claude_permissions();
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

    // Write settings atomically
    let json_str = match serde_json::to_string_pretty(&settings) {
        Ok(s) => s,
        Err(_) => return false,
    };

    if !paths::atomic_write(&settings_path, &json_str) {
        return false;
    }

    // Quick verification
    verify_claude_hooks_installed(Some(&settings_path), include_permissions)
}

/// Verify that hcom hooks are correctly installed in Claude settings.
///
/// Checks all hook types exist with correct commands, timeouts, and matchers.
/// Checks HCOM env var is set. Optionally checks permissions.
pub fn verify_claude_hooks_installed(
    settings_path: Option<&Path>,
    check_permissions: bool,
) -> bool {
    let default_path = get_claude_settings_path();
    let path = settings_path.unwrap_or(&default_path);

    let settings = match load_claude_settings(path) {
        Some(s) => s,
        None => return false,
    };

    let hooks = match settings.get("hooks").and_then(|v| v.as_object()) {
        Some(h) => h,
        None => return false,
    };

    for &(hook_type, expected_matcher, cmd_suffix, expected_timeout) in CLAUDE_HOOK_CONFIGS {
        let hook_matchers = match hooks.get(hook_type).and_then(|v| v.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return false,
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
                let has_hcom = command.contains("${HCOM}") || command.to_lowercase().contains("hcom");
                if has_hcom && command.contains(cmd_suffix) {
                    if hcom_hook_found {
                        // Duplicate hcom hook
                        return false;
                    }

                    // Verify timeout
                    let actual_timeout = hook.get("timeout").and_then(|v| v.as_u64());
                    if actual_timeout != expected_timeout {
                        return false;
                    }

                    // Verify matcher
                    let actual_matcher = matcher_obj
                        .get("matcher")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if actual_matcher != expected_matcher {
                        return false;
                    }

                    hcom_hook_found = true;
                }
            }
        }

        if !hcom_hook_found {
            return false;
        }
    }

    // Check HCOM env var
    if settings
        .get("env")
        .and_then(|v| v.get("HCOM"))
        .is_none()
    {
        return false;
    }

    // Check permissions
    if check_permissions {
        let allow = settings
            .get("permissions")
            .and_then(|v| v.get("allow"))
            .and_then(|v| v.as_array());
        let allow = match allow {
            Some(a) => a,
            None => return false,
        };
        for pattern in build_claude_permissions() {
            if !allow.iter().any(|p| p.as_str() == Some(&pattern)) {
                return false;
            }
        }
    }

    true
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
    let local_path = get_claude_settings_path();

    let global_ok = remove_hooks_from_settings_path(&global_path);
    let local_ok = if local_path != global_path {
        remove_hooks_from_settings_path(&local_path)
    } else {
        true
    };

    global_ok && local_ok
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_real_session_id_normal() {
        let raw = serde_json::json!({"session": {"session_id": "abc-123"}});
        assert_eq!(get_real_session_id(&raw, None, false), "abc-123");
    }

    #[test]
    fn test_get_real_session_id_fork() {
        crate::config::Config::init(); // log_info needs Config
        let raw = serde_json::json!({"session": {"session_id": "old-parent-id"}});
        let env_file = "/home/user/.claude/session-env/12345678-1234-1234-1234-123456789012/hook-1.sh";
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
        assert_eq!(get_real_session_id(&raw, Some(env_file), false), "correct-id");
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
        let result = subagent_start(&raw);
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
        assert!(subagent_start(&raw).is_none());
    }

    #[test]
    fn test_subagent_start_empty_agent_id() {
        let raw = serde_json::json!({"agent_id": ""});
        assert!(subagent_start(&raw).is_none());
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
        let combined = combine_posttooluse_outputs(&[output.clone()]);
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

    // ==================== Settings Tests ====================

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
        assert_eq!(CLAUDE_HOOK_CONFIGS.len(), 9);
        assert_eq!(CLAUDE_HOOK_TYPES.len(), 9);
        assert_eq!(CLAUDE_HOOK_COMMANDS.len(), 9);
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
    fn test_build_claude_permissions() {
        let perms = build_claude_permissions();
        assert!(!perms.is_empty());
        assert_eq!(perms.len(), SAFE_HCOM_COMMANDS.len());
        // All should start with "Bash("
        for p in &perms {
            assert!(p.starts_with("Bash("), "bad permission: {}", p);
        }
    }

    #[test]
    fn test_build_all_claude_permission_patterns() {
        let patterns = build_all_claude_permission_patterns();
        // Should have both hcom and uvx hcom variants
        assert_eq!(patterns.len(), SAFE_HCOM_COMMANDS.len() * 2);
        assert!(patterns.iter().any(|p| p.contains("hcom send")));
        assert!(patterns.iter().any(|p| p.contains("uvx hcom send")));
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
        assert!(verify_claude_hooks_installed(
            Some(&settings_path),
            true,
        ));

        // Verify without permissions check
        assert!(verify_claude_hooks_installed(
            Some(&settings_path),
            false,
        ));
    }

    #[test]
    fn test_verify_missing_file() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("nonexistent.json");
        assert!(!verify_claude_hooks_installed(
            Some(&settings_path),
            false,
        ));
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

        assert!(!verify_claude_hooks_installed(
            Some(&settings_path),
            false,
        ));
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

    // ==================== Claude Hook Property Tests ====================

    use serial_test::serial;
    use crate::hooks::test_helpers::{EnvGuard, isolated_test_env};

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
                        violations.push(format!(
                            "{hook_type}[{i}].hooks[{j}]: command={command}"
                        ));
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
        let hook_cmd_base = get_hook_command();
        for &(hook_type, cmd_suffix) in expected {
            let expected_full = format!("{} {}", hook_cmd_base, cmd_suffix);
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
                        if let Some(cmd) = hook.get("command").and_then(|v| v.as_str()) {
                            if cmd == expected_full {
                                found = true;
                                break;
                            }
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
            let hook_cmd_base = get_hook_command();
            let expected_command = format!("{} {}", hook_cmd_base, cmd_suffix);
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
            assert!(found, "{hook_type}: expected exact command '{expected_command}', not found");
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
            ("Notification", "notify"),
        ];
        let missing = independently_verify_hcom_hooks_present_claude(&after_setup, &expected);
        assert!(missing.is_empty(), "after setup, missing hooks: {missing:?}");

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
        assert_eq!(after_remove["permissions"]["deny"], serde_json::json!(["dangerous"]));

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
}
