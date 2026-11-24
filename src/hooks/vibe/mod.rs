//! Mistral Vibe hook handlers and `hooks.toml` management.
//!
//! Vibe runs command hooks from `$VIBE_HOME/hooks.toml` (see `config`). Output
//! `{"hook_specific_output":{"additional_context": …}}` reaches the model from
//! `pre_agent` (appended to the user message) and `post_tool` (appended to the
//! tool result); those are the delivery channels. The PTY wake path types a
//! bare `<hcom>` prompt when the agent is idle, and `pre_agent` attaches the
//! messages to it. `notification` (turn end, approval, question) and
//! `session_end` are observation-only.
//!
//! `post_agent` is not used: its deny makes Vibe discard the reply and ask
//! again, so messages arriving at turn end wait for the idle wake instead.

mod config;

#[cfg(test)]
mod tests;

pub(crate) use config::vibe_config_dir;
pub use config::{
    SetupError, ensure_installed, get_vibe_hooks_path, remove_vibe_hooks, try_setup_vibe_hooks,
    verify_vibe_hooks_installed,
};

use std::io::Write;

use serde_json::{Value, json};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::{DeliveryAck, HookPayload, common};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_LISTENING};

use config::VIBE_HOOK_COMMANDS;

const HCOM_TRIGGER: &str = "<hcom>";

fn resolve_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        payload.session_id.as_deref(),
        ctx.process_id.as_deref(),
    )
    .filter(|instance| instance.tool == "vibe")
}

fn update_position(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload, instance_name: &str) {
    let mut updates = serde_json::Map::new();
    if let Some(session_id) = payload.session_id.as_ref().filter(|s| !s.is_empty()) {
        updates.insert("session_id".into(), Value::String(session_id.clone()));
    }
    if let Some(path) = payload.transcript_path.as_ref().filter(|s| !s.is_empty()) {
        updates.insert("transcript_path".into(), Value::String(path.clone()));
    }
    let cwd = payload
        .raw
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_else(|| ctx.cwd.to_str().unwrap_or(""));
    if !cwd.is_empty() {
        updates.insert("directory".into(), Value::String(cwd.to_string()));
    }
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, &updates);
    }
}

/// Bind the hook's session to the launching process and record its position.
/// Returns the instance name. Refuses a process that belongs to another tool.
///
/// `create_if_unbound` lets a fresh session with no binding left start a new
/// identity, as Claude's SessionStart does. A live launch always keeps its
/// process binding, so only `session_start` passes it: after `hcom stop` the
/// running process must not rejoin from its next tool or prompt hook.
fn bind_session(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
    create_if_unbound: bool,
) -> Option<String> {
    let session_id = payload
        .session_id
        .as_deref()
        .filter(|sid| !sid.is_empty())?;
    let process_id = ctx.process_id.as_deref()?;
    let instance_name = match instance_binding::bind_session_to_process_for_tool(
        db,
        session_id,
        Some(process_id),
        "vibe",
        create_if_unbound,
    ) {
        instance_binding::ToolCheckedBind::Bound(name) => name,
        instance_binding::ToolCheckedBind::Rejected
        | instance_binding::ToolCheckedBind::Unbound => return None,
    };
    // `/clear`, `/resume` and `/branch` switch sessions in a live process;
    // keep exactly one session bound to the instance.
    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    update_position(db, ctx, payload, &instance_name);
    crate::runtime_env::set_terminal_title(&instance_name);
    crate::relay::worker::try_spawn_worker();
    Some(instance_name)
}

/// The instance for a post-start hook, binding lazily when `session_start`
/// was missed or the session changed without one. Never creates an identity,
/// and only binds a process that is still attached: `hcom stop` drops the
/// process binding, and binding again would hand the stopped process to
/// whoever owns its session now (e.g. a replacement started by `hcom r`).
fn resolved_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    let incoming = payload.session_id.as_deref().filter(|sid| !sid.is_empty());
    let mut instance = resolve_instance(db, ctx, payload);
    let stale = match (&instance, incoming) {
        (Some(inst), Some(sid)) => inst.session_id.as_deref() != Some(sid),
        (None, _) => true,
        _ => false,
    };
    if stale {
        instance_binding::resolve_process_binding(db, ctx.process_id.as_deref())?;
        let name = bind_session(db, ctx, payload, false)?;
        lifecycle::set_status(db, &name, ST_LISTENING, "start", Default::default());
        instance = db.get_instance_full(&name).ok().flatten();
    }
    let instance = instance?;
    update_position(db, ctx, payload, &instance.name);
    Some(instance)
}

fn context_output(text: &str) -> Value {
    json!({ "hook_specific_output": { "additional_context": text } })
}

/// Bind and go idle. Vibe adds `session_start` context to the next user
/// message after `pre_agent`'s, so the bootstrap normally goes out from
/// `pre_agent` (ahead of the messages). Only `/clear`, which drops the
/// conversation, re-sends it here.
fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(instance_name) = bind_session(db, ctx, payload, true) else {
        return json!({});
    };
    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );
    common::notify_hook_instance_with_db(db, &instance_name);
    let source = payload.raw.get("source").and_then(Value::as_str);
    let announced = db
        .get_instance_full(&instance_name)
        .ok()
        .flatten()
        .is_some_and(|instance| instance.name_announced != 0);
    if source == Some("clear") && announced {
        let bootstrap = crate::bootstrap::get_bootstrap(db, ctx, &instance_name, "vibe");
        return context_output(&bootstrap);
    }
    json!({})
}

/// Every user turn (typed, positional, queued, or the PTY wake): mark active
/// and attach the bootstrap (once) and pending messages.
fn handle_preagent(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return (json!({}), None);
    };
    let prompt = payload
        .raw
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("");
    let status_context = if prompt.trim() == HCOM_TRIGGER {
        "trigger"
    } else {
        "prompt"
    };
    lifecycle::set_status(
        db,
        &instance.name,
        ST_ACTIVE,
        status_context,
        Default::default(),
    );
    let bootstrap = common::inject_bootstrap_once(db, ctx, &instance.name, &instance, "vibe");
    let pending = common::prepare_pending_messages(db, &instance.name);
    let text = match (&bootstrap, &pending) {
        (Some(boot), Some(p)) => Some(format!("{boot}\n\n{}", p.formatted)),
        (Some(boot), None) => Some(boot.clone()),
        (None, Some(p)) => Some(p.formatted.clone()),
        (None, None) => None,
    };
    match text {
        Some(text) => (context_output(&text), pending.map(|p| p.ack)),
        None => (json!({}), None),
    }
}

fn handle_pretool(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        common::update_tool_status(
            db,
            &instance.name,
            "vibe",
            &payload.tool_name,
            &payload.tool_input,
        );
    }
    json!({})
}

/// A tool call finished: deliver pending messages mid-turn. A finished call
/// (denied ones included) also means a pending approval was answered, so
/// leave `blocked`.
///
/// Failed calls (`tool_status: failure`, e.g. a denied approval) get no
/// messages: Vibe keeps their error outside the result content, and context
/// appended to such a result replaces what the model sees, so the model would
/// lose the error (verified 3.1.4). Messages wait for the next successful call
/// or the idle wake after the turn.
fn handle_posttool(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return (json!({}), None);
    };
    if instance.status == ST_BLOCKED && instance.status_context == "approval" {
        lifecycle::set_status(
            db,
            &instance.name,
            ST_ACTIVE,
            &format!("approved:{}", payload.tool_name),
            Default::default(),
        );
    }
    if payload.raw.get("tool_status").and_then(Value::as_str) == Some("failure") {
        return (json!({}), None);
    }
    match common::prepare_pending_messages(db, &instance.name) {
        Some(prepared) => (context_output(&prepared.formatted), Some(prepared.ack)),
        None => (json!({}), None),
    }
}

/// Vibe sends `turn_completed` only once no queued turn, approval or question
/// is left, so it marks the agent idle; the PTY wake then delivers anything
/// that arrived after the last tool call.
fn handle_notification(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(instance) = resolve_instance(db, ctx, payload) else {
        return json!({});
    };
    match payload.notification_type.as_deref() {
        Some("turn_completed" | "turn_failed") => {
            lifecycle::set_status(db, &instance.name, ST_LISTENING, "", Default::default());
            common::notify_hook_instance_with_db(db, &instance.name);
        }
        Some("approval_requested" | "question_asked") => {
            let detail = payload
                .raw
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            lifecycle::set_status(
                db,
                &instance.name,
                ST_BLOCKED,
                "approval",
                lifecycle::StatusUpdate {
                    detail,
                    ..Default::default()
                },
            );
        }
        _ => {}
    }
    json!({})
}

/// Only the instance's own session ends it: `/clear` ends the old session
/// while the process lives on with a new one.
fn handle_sessionend(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(instance) = resolve_instance(db, ctx, payload) else {
        return json!({});
    };
    let reason = payload
        .raw
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if reason == "clear" {
        return json!({});
    }
    let incoming = payload.session_id.as_deref().filter(|sid| !sid.is_empty());
    if incoming.is_some() && incoming != instance.session_id.as_deref() {
        log::log_warn(
            "hooks",
            "vibe.sessionend_ignored",
            &format!(
                "instance={} incoming_session_id={} bound_session_id={} reason={reason}",
                instance.name,
                incoming.unwrap_or(""),
                instance.session_id.as_deref().unwrap_or(""),
            ),
        );
        return json!({});
    }
    common::finalize_session(db, &instance.name, reason, None);
    json!({})
}

fn hook_type_for_command(hook_name: &str) -> &'static str {
    VIBE_HOOK_COMMANDS
        .iter()
        .find(|(_, _, command)| *command == hook_name)
        .map(|(_, hook_type, _)| *hook_type)
        .unwrap_or("unknown")
}

fn route(
    db: &HcomDb,
    ctx: &HcomContext,
    hook_name: &str,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    match hook_name {
        "vibe-sessionstart" => (handle_sessionstart(db, ctx, payload), None),
        "vibe-preagent" => handle_preagent(db, ctx, payload),
        "vibe-pretool" => (handle_pretool(db, ctx, payload), None),
        "vibe-posttool" => handle_posttool(db, ctx, payload),
        "vibe-notification" => (handle_notification(db, ctx, payload), None),
        "vibe-sessionend" => (handle_sessionend(db, ctx, payload), None),
        _ => (json!({}), None),
    }
}

/// Whether this hook run belongs to an hcom-launched Vibe main agent. The
/// hooks are installed globally, so plain `vibe` runs reach here too.
fn is_hcom_main_agent(ctx: &HcomContext, raw: &Value) -> bool {
    // Subagents share the launcher's env; their sessions must not take over
    // the binding.
    let subagent = raw
        .get("parent_session_id")
        .and_then(Value::as_str)
        .is_some_and(|sid| !sid.is_empty());
    common::hook_gate_check(ctx)
        && ctx.raw_env.get("HCOM_TOOL").map(String::as_str) == Some("vibe")
        && !subagent
}

pub fn dispatch_vibe_hook(hook_name: &str) -> i32 {
    let ctx = HcomContext::from_os();
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(value) => value,
        Err(err) => {
            log::log_warn(
                "hooks",
                "vibe.parse_error",
                &format!("hook={hook_name} err={err}"),
            );
            return 0;
        }
    };
    // Checked before opening the DB, so plain `vibe` runs stay cheap.
    if !is_hcom_main_agent(&ctx, &raw) {
        return 0;
    }
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(err) => {
            log::log_warn(
                "hooks",
                "vibe.db_error",
                &format!("hook={hook_name} err={err}"),
            );
            return 0;
        }
    };
    let payload = HookPayload::from_vibe(hook_type_for_command(hook_name), raw);
    let (output, delivery_ack) =
        common::dispatch_with_panic_guard("vibe", hook_name, (json!({}), None), || {
            route(&db, &ctx, hook_name, &payload)
        });
    // Empty stdout is Vibe's pass-through.
    if output.as_object().is_some_and(|o| o.is_empty()) {
        return 0;
    }
    let mut stdout = std::io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &output).is_ok()
        && stdout.flush().is_ok()
        && let Some(ack) = delivery_ack.as_ref()
    {
        common::commit_delivery_ack(&db, ack);
    }
    0
}
