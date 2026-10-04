//! Qoder CLI per-run hook loading and native hook handlers.
//!
//! Qoder mirrors Claude Code's `--settings` flag, hook schema and permission
//! model, so hcom loads its hooks for one run by merging them into a runtime
//! settings file passed with `--settings`. No hooks are written to the user's
//! `~/.qoder/settings.json`; the launcher's only write there is the workspace
//! trust entry (`tools::qoder_preprocessing`).
//!
//! Qoder runs `UserPromptSubmit`/`PostToolUse`/`SessionStart` hook output
//! (`hookSpecificOutput.additionalContext`) into the model's context and
//! continues a finished turn when `Stop` returns `{"decision":"block"}`; those
//! are the delivery channels. The PTY wake path in `delivery.rs` types a bare
//! `<hcom>` prompt when the agent is idle.

use std::io::Write;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::claude::{caller_settings, object_field};
use crate::hooks::{DeliveryAck, HookPayload, common};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_LISTENING};

use super::runtime::{self, LaunchCtx, PerRunAdapter, RuntimeInjection};

const HCOM_TRIGGER: &str = "<hcom>";
const HOOK_TIMEOUT_SECS: u64 = 15;

/// Hook configuration: (event, matcher, hook command name, permissions only).
///
/// `PreToolUse` is limited to the tools whose input feeds the status detail
/// (see `IntegrationSpec::status_detail`) so read-only tool calls don't pay for
/// a process spawn. `PermissionRequest` is only registered when auto-approve is
/// on. `Notification` is limited to the permission prompt, the one
/// notification the handler acts on.
const QODER_HOOK_CONFIGS: &[(&str, &str, &str, bool)] = &[
    ("SessionStart", "", "qoder-sessionstart", false),
    ("UserPromptSubmit", "", "qoder-userpromptsubmit", false),
    (
        "PreToolUse",
        "Bash|Write|Edit|Agent",
        "qoder-pretooluse",
        false,
    ),
    ("PermissionRequest", "Bash", "qoder-permissionrequest", true),
    ("PostToolUse", "", "qoder-posttooluse", false),
    ("PostToolUseFailure", "", "qoder-posttoolusefailure", false),
    (
        "Notification",
        "permission_prompt",
        "qoder-notification",
        false,
    ),
    ("Stop", "", "qoder-stop", false),
    ("SessionEnd", "", "qoder-sessionend", false),
];

pub static PER_RUN: PerRunAdapter = PerRunAdapter {
    prepare: prepare_per_run,
    cleanup_legacy: cleanup_legacy_per_run,
    ensure_permissions: None,
    managed_value_flags: &["--settings"],
    strip_legacy_args: None,
};

fn build_qoder_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn merge_per_run_settings(settings: &mut Value, auto_approve: bool) -> Result<()> {
    let hooks = object_field(settings, "hooks")?;
    for &(event, matcher, command, permissions_only) in QODER_HOOK_CONFIGS {
        if permissions_only && !auto_approve {
            continue;
        }
        let entries = hooks.entry(event).or_insert_with(|| json!([]));
        let entries = entries
            .as_array_mut()
            .with_context(|| format!("Caller --settings hooks.{event} must be an array"))?;
        let mut group = json!({
            "hooks": [{
                "type": "command",
                "command": build_qoder_hook_command(command),
                "timeout": HOOK_TIMEOUT_SECS,
            }]
        });
        if !matcher.is_empty() {
            group["matcher"] = Value::String(matcher.to_string());
        }
        entries.push(group);
    }
    Ok(())
}

/// The caller's last `--settings` value (a path or inline JSON) is merged with
/// hcom's hooks and passed as the only `--settings`: Qoder merges hooks across
/// settings sources (user, project, flag), but given several `--settings`
/// flags it loads hooks from just one of them (seen on 1.1.65), so exactly one
/// is passed. Earlier caller values are dropped.
/// Qoder gates `--settings` hooks behind workspace trust like file hooks, which
/// is why the launcher trusts the workspace (`tools::qoder_preprocessing`).
fn prepare_per_run(ctx: &LaunchCtx) -> Result<RuntimeInjection> {
    let mut args = ctx.args.clone();
    let values = runtime::take_flag_values(&mut args, &["--settings"]);
    let mut settings = match values.last() {
        Some(value) => caller_settings(value, &ctx.cwd)?,
        None => json!({}),
    };
    merge_per_run_settings(&mut settings, ctx.auto_approve)?;
    let json = serde_json::to_vec(&settings)?;
    let path = runtime::publish_file("qoder", "settings.json", &json)
        .context("Cannot publish Qoder runtime settings")?;
    runtime::insert_before_separator(
        &mut args,
        [
            "--settings".to_string(),
            path.to_string_lossy().into_owned(),
        ],
    );
    Ok(RuntimeInjection {
        args,
        env: Vec::new(),
    })
}

/// Qoder support was added as per-run only; no older hcom wrote a persistent
/// Qoder install, so there is nothing to clean up.
fn cleanup_legacy_per_run(_ctx: &LaunchCtx) -> Result<()> {
    Ok(())
}

/// `hcom hooks remove`: no persistent install exists.
pub fn remove_qoder_hooks() -> bool {
    true
}

// ── Handlers ────────────────────────────────────────────────────────────

fn resolve_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    if ctx
        .raw_env
        .get("HCOM_TOOL")
        .is_some_and(|tool| tool != "qoder")
    {
        return None;
    }
    // Validate both owners: process resolution takes precedence over session
    // resolution and must not hide a session belonging to another tool.
    if instance_binding::resolve_instance_from_binding(db, payload.session_id.as_deref(), None)
        .is_some_and(|instance| instance.tool != "qoder")
    {
        return None;
    }
    instance_binding::resolve_instance_from_binding(
        db,
        payload.session_id.as_deref(),
        ctx.process_id.as_deref(),
    )
    .filter(|instance| instance.tool == "qoder")
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

fn context_output(event: &str, text: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": text,
        }
    })
}

/// Bind the hook's session to the launching process and record its position.
/// Returns the instance name. Refuses a process that belongs to another tool.
fn bind_session(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<String> {
    let session_id = payload
        .session_id
        .as_deref()
        .filter(|sid| !sid.is_empty())?;
    let process_id = ctx.process_id.as_deref()?;
    if let Some(env_tool) = ctx.raw_env.get("HCOM_TOOL")
        && env_tool != "qoder"
    {
        log::log_warn(
            "hooks",
            "qoder.bind.tool_env_refused",
            &format!("session_id={session_id} process_id={process_id} env_tool={env_tool}"),
        );
        return None;
    }
    let instance_name = match instance_binding::bind_session_to_process_for_tool(
        db,
        session_id,
        Some(process_id),
        "qoder",
        true,
    ) {
        instance_binding::ToolCheckedBind::Bound(name) => name,
        instance_binding::ToolCheckedBind::Rejected
        | instance_binding::ToolCheckedBind::Unbound => return None,
    };
    // The session id changes in a live process (`/clear`, `/resume`, `new`);
    // keep exactly one session bound to the instance.
    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    update_position(db, ctx, payload, &instance_name);
    crate::runtime_env::set_terminal_title(&instance_name);
    crate::relay::worker::ensure_worker(true);
    Some(instance_name)
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(instance_name) = bind_session(db, ctx, payload) else {
        return json!({});
    };
    let Some(instance) = db.get_instance_full(&instance_name).ok().flatten() else {
        return json!({});
    };
    let source = payload
        .raw
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("startup");
    // After compaction the agent is mid-turn; every other source is idle.
    if source != "compact" {
        lifecycle::set_status(
            db,
            &instance_name,
            ST_LISTENING,
            "start",
            Default::default(),
        );
    }
    common::notify_hook_instance_with_db(db, &instance_name);
    // `compact` and `clear` drop the conversation, including the bootstrap
    // injected earlier, so it is re-sent; otherwise it is injected once.
    let bootstrap = if matches!(source, "compact" | "clear") && instance.name_announced != 0 {
        Some(crate::bootstrap::get_bootstrap(
            db,
            ctx,
            &instance_name,
            "qoder",
        ))
    } else {
        common::inject_bootstrap_once(db, ctx, &instance_name, &instance, "qoder")
    };
    match bootstrap {
        Some(text) => context_output("SessionStart", &text),
        None => json!({}),
    }
}

/// The instance for a post-start hook. When SessionStart never fired (Qoder
/// skips it in a folder that is not trusted) or the session changed without
/// one, bind lazily from the process binding.
fn resolved_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    let incoming = payload.session_id.as_deref().filter(|sid| !sid.is_empty());
    let mut instance = resolve_instance(db, ctx, payload);
    let stale = match (&instance, incoming) {
        (Some(inst), Some(sid)) => inst.session_id.as_deref() != Some(sid),
        (None, _) => true,
        _ => false,
    };
    if stale {
        let name = bind_session(db, ctx, payload)?;
        lifecycle::set_status(db, &name, ST_LISTENING, "start", Default::default());
        instance = db.get_instance_full(&name).ok().flatten();
    }
    let instance = instance?;
    update_position(db, ctx, payload, &instance.name);
    Some(instance)
}

fn handle_userpromptsubmit(
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
    // Bootstrap goes out with the first prompt when SessionStart did not
    // deliver it, ahead of any pending messages.
    let bootstrap = common::inject_bootstrap_once(db, ctx, &instance.name, &instance, "qoder");
    let pending = common::prepare_pending_messages(db, &instance.name);
    let text = match (&bootstrap, &pending) {
        (Some(boot), Some(p)) => Some(format!("{boot}\n\n{}", p.formatted)),
        (Some(boot), None) => Some(boot.clone()),
        (None, Some(p)) => Some(p.formatted.clone()),
        (None, None) => None,
    };
    match text {
        Some(text) => (
            context_output("UserPromptSubmit", &text),
            pending.map(|p| p.ack),
        ),
        None => (json!({}), None),
    }
}

fn handle_pretooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        common::update_tool_status(
            db,
            &instance.name,
            "qoder",
            &payload.tool_name,
            &payload.tool_input,
        );
    }
    json!({})
}

/// PostToolUse / PostToolUseFailure: a tool call just finished, so deliver
/// pending messages mid-turn. A finished call also means a pending approval
/// was resolved, so leave `blocked`.
fn handle_posttooluse(
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
    // Shared by PostToolUse and PostToolUseFailure: answer under the event
    // that fired.
    match common::prepare_pending_messages(db, &instance.name) {
        Some(prepared) => (
            context_output(&payload.hook_name, &prepared.formatted),
            Some(prepared.ack),
        ),
        None => (json!({}), None),
    }
}

fn handle_stop(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return (json!({}), None);
    };
    if let Some(prepared) = common::prepare_pending_messages(db, &instance.name) {
        // Status stays active; it goes `listening` on an empty Stop.
        return (
            json!({ "decision": "block", "reason": prepared.formatted }),
            Some(prepared.ack),
        );
    }
    lifecycle::set_status(db, &instance.name, ST_LISTENING, "", Default::default());
    common::notify_hook_instance_with_db(db, &instance.name);
    (json!({}), None)
}

/// Whether the PermissionRequest hook would auto-allow this call.
fn auto_allows(tool_name: &str, tool_input: &Value) -> bool {
    let command = tool_input
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
    // POSIX shells only: the check parses POSIX quoting.
    tool_name == "Bash" && !cfg!(windows) && common::is_safe_hcom_command(command)
}

/// `HCOM_AUTO_APPROVE` is set in the launched agent's env from the same config
/// value that decided whether the PermissionRequest hook was registered.
fn auto_approve_enabled(ctx: &HcomContext) -> bool {
    ctx.raw_env
        .get("HCOM_AUTO_APPROVE")
        .is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn handle_notification(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if payload.notification_type.as_deref() != Some("permission_prompt") {
        return json!({});
    }
    // Qoder still emits `permission_prompt` after a PermissionRequest hook has
    // allowed the call (verified), so an auto-allowed hcom command is not a
    // stall on a human.
    let details = payload.raw.get("details");
    let tool_name = details
        .and_then(|d| d.get("toolName"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if auto_approve_enabled(ctx)
        && let Some(input) = details.and_then(|d| d.get("input"))
        && auto_allows(tool_name, input)
    {
        return json!({});
    }
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return json!({});
    };
    lifecycle::set_status(
        db,
        &instance.name,
        ST_BLOCKED,
        "approval",
        Default::default(),
    );
    json!({})
}

/// Auto-approve hcom's own coordination commands. Only registered when
/// `auto_approve` is on. Anything else falls through to Qoder's prompt.
fn handle_permissionrequest(payload: &HookPayload) -> Value {
    if auto_allows(&payload.tool_name, &payload.tool_input) {
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow" },
            }
        })
    } else {
        json!({})
    }
}

/// Only the instance's own session ends it: `/clear` and `/resume` fire
/// SessionEnd for the old session while the process lives on with a new one,
/// and a stale session's SessionEnd must not stop a rebound instance.
fn handle_sessionend(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(instance) = resolve_instance(db, ctx, payload) else {
        return json!({});
    };
    let reason = payload
        .raw
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if matches!(reason, "clear" | "resume") {
        log::log_info(
            "hooks",
            "qoder.sessionend_process_continues",
            &format!("instance={} reason={reason}", instance.name),
        );
        return json!({});
    }
    let incoming = payload.session_id.as_deref().filter(|sid| !sid.is_empty());
    if incoming.is_some() && incoming != instance.session_id.as_deref() {
        log::log_warn(
            "hooks",
            "qoder.sessionend_ignored",
            &format!(
                "instance={} incoming_session_id={} bound_session_id={} reason={reason}",
                instance.name,
                incoming.unwrap_or(""),
                instance.session_id.as_deref().unwrap_or(""),
            ),
        );
        return json!({});
    }
    update_position(db, ctx, payload, &instance.name);
    common::finalize_session(db, &instance.name, reason, None);
    json!({})
}

fn hook_type_for_command(hook_name: &str) -> &'static str {
    QODER_HOOK_CONFIGS
        .iter()
        .find(|(_, _, command, _)| *command == hook_name)
        .map(|(event, _, _, _)| *event)
        .unwrap_or("Unknown")
}

fn route(
    db: &HcomDb,
    ctx: &HcomContext,
    hook_name: &str,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    match hook_name {
        "qoder-sessionstart" => (handle_sessionstart(db, ctx, payload), None),
        "qoder-userpromptsubmit" => handle_userpromptsubmit(db, ctx, payload),
        "qoder-pretooluse" => (handle_pretooluse(db, ctx, payload), None),
        "qoder-permissionrequest" => (handle_permissionrequest(payload), None),
        "qoder-posttooluse" | "qoder-posttoolusefailure" => handle_posttooluse(db, ctx, payload),
        "qoder-notification" => (handle_notification(db, ctx, payload), None),
        "qoder-stop" => handle_stop(db, ctx, payload),
        "qoder-sessionend" => (handle_sessionend(db, ctx, payload), None),
        _ => (json!({}), None),
    }
}

pub fn dispatch_qoder_hook_native(hook_name: &str) -> i32 {
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(value) => value,
        Err(err) => {
            log::log_warn(
                "hooks",
                "qoder.parse_error",
                &format!("hook={hook_name} err={err}"),
            );
            return 0;
        }
    };
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(err) => {
            log::log_warn(
                "hooks",
                "qoder.db_error",
                &format!("hook={hook_name} err={err}"),
            );
            return 0;
        }
    };
    let ctx = HcomContext::from_os();
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }
    let payload = HookPayload::from_qoder(hook_type_for_command(hook_name), raw);
    let (output, delivery_ack) =
        common::dispatch_with_panic_guard("qoder", hook_name, (json!({}), None), || {
            route(&db, &ctx, hook_name, &payload)
        });
    // Qoder injects plain stdout into the conversation, so a no-op writes nothing.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::EnvGuard;
    use serial_test::serial;
    use std::path::PathBuf;

    fn qoder_test_env() -> (tempfile::TempDir, PathBuf, EnvGuard) {
        let guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        unsafe {
            std::env::set_var("HOME", dir.path().join("home"));
            std::env::set_var("HCOM_DIR", workspace.join(".hcom"));
            std::env::remove_var("QODER_CONFIG_DIR");
        }
        (dir, workspace, guard)
    }

    fn prepared(ctx_args: &[&str], auto_approve: bool, workspace: PathBuf) -> (Vec<String>, Value) {
        let mut ctx = LaunchCtx::ambient(crate::tool::Tool::Qoder, auto_approve);
        ctx.cwd = workspace;
        ctx.args = ctx_args.iter().map(|s| s.to_string()).collect();
        let injection = prepare_per_run(&ctx).unwrap();
        let at = injection
            .args
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings injected");
        let settings: Value =
            serde_json::from_slice(&std::fs::read(&injection.args[at + 1]).unwrap()).unwrap();
        (injection.args, settings)
    }

    fn commands(settings: &Value, event: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                    .map(|h| h["command"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    #[serial]
    fn per_run_settings_hold_hcom_hooks_and_flag_precedes_separator() {
        let (_dir, workspace, _guard) = qoder_test_env();
        let (args, settings) =
            prepared(&["--model", "Qwen3.8-Flash", "--", "hi"], false, workspace);
        assert_eq!(&args[..2], ["--model", "Qwen3.8-Flash"]);
        assert_eq!(args[2], "--settings");
        assert_eq!(&args[4..], ["--", "hi"]);
        for (event, _, command, permissions_only) in QODER_HOOK_CONFIGS {
            if *permissions_only {
                continue;
            }
            assert!(
                commands(&settings, event).contains(&build_qoder_hook_command(command)),
                "{event}"
            );
        }
        let group = &settings["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(group["type"], "command");
        assert_eq!(group["timeout"], HOOK_TIMEOUT_SECS);
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["matcher"],
            "Bash|Write|Edit|Agent"
        );
    }

    #[test]
    #[serial]
    fn per_run_settings_preserve_caller_inline_hooks_and_settings() {
        let (_dir, workspace, _guard) = qoder_test_env();
        let caller = json!({
            "model": {"name": "m"},
            "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "./mine.sh"}]}]}
        })
        .to_string();
        let (args, settings) = prepared(&["--settings", &caller, "--model", "x"], false, workspace);
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        assert_eq!(settings["model"]["name"], "m");
        let starts = commands(&settings, "SessionStart");
        assert_eq!(starts[0], "./mine.sh");
        assert_eq!(starts[1], build_qoder_hook_command("qoder-sessionstart"));
    }

    #[test]
    #[serial]
    fn per_run_settings_read_caller_settings_file_and_equals_form() {
        let (_dir, workspace, _guard) = qoder_test_env();
        std::fs::write(
            workspace.join("mine.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"./stop.sh"}]}]}}"#,
        )
        .unwrap();
        let (args, settings) = prepared(&["--settings=mine.json"], false, workspace);
        assert_eq!(args.iter().filter(|a| a.contains("settings")).count(), 2);
        let stops = commands(&settings, "Stop");
        assert_eq!(stops[0], "./stop.sh");
        assert_eq!(stops[1], build_qoder_hook_command("qoder-stop"));
    }

    #[test]
    #[serial]
    fn per_run_rejects_malformed_caller_settings() {
        let (_dir, workspace, _guard) = qoder_test_env();
        let mut ctx = LaunchCtx::ambient(crate::tool::Tool::Qoder, false);
        ctx.cwd = workspace;
        ctx.args = vec!["--settings".into(), "{not json".into()];
        assert!(prepare_per_run(&ctx).is_err());
        ctx.args = vec!["--settings".into(), r#"{"hooks":{"Stop":"nope"}}"#.into()];
        assert!(prepare_per_run(&ctx).is_err());
    }

    #[test]
    #[serial]
    fn permission_request_hook_only_registered_with_auto_approve() {
        let (_dir, workspace, _guard) = qoder_test_env();
        let (_, off) = prepared(&[], false, workspace.clone());
        assert!(off["hooks"].get("PermissionRequest").is_none());
        let (_, on) = prepared(&[], true, workspace);
        assert_eq!(
            commands(&on, "PermissionRequest"),
            [build_qoder_hook_command("qoder-permissionrequest")]
        );
        assert_eq!(on["hooks"]["PermissionRequest"][0]["matcher"], "Bash");
        // The settings never carry permission rules: approval goes through the hook.
        assert!(on.get("permissions").is_none());
    }

    #[test]
    #[serial]
    fn per_run_publishes_identical_content_to_one_path() {
        let (_dir, workspace, _guard) = qoder_test_env();
        let (a, _) = prepared(&["--model", "x"], true, workspace.clone());
        let (b, _) = prepared(&["--model", "y"], true, workspace);
        assert_eq!(a[a.len() - 1], b[b.len() - 1]);
    }

    #[test]
    fn permission_request_allows_only_safe_hcom_bash() {
        let allow = |tool: &str, input: Value| {
            let payload = HookPayload::from_qoder(
                "PermissionRequest",
                json!({"tool_name": tool, "tool_input": input}),
            );
            handle_permissionrequest(&payload)
        };
        if cfg!(windows) {
            // The safety check parses POSIX quoting, so nothing is auto-allowed.
            assert_eq!(
                allow("Bash", json!({"command": "hcom send @luna -- hi"})),
                json!({})
            );
            return;
        }
        let allowed = allow("Bash", json!({"command": "hcom send @luna -- hi"}));
        assert_eq!(
            allowed["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        assert_eq!(
            allowed["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        for (tool, command) in [
            ("Bash", "hcom send @luna -- hi; rm -rf ~"),
            ("Bash", "hcom kill luna"),
            ("Bash", "echo hcom send @luna"),
            ("Write", "hcom send @luna -- hi"),
        ] {
            assert_eq!(
                allow(tool, json!({"command": command})),
                json!({}),
                "{tool} {command}"
            );
        }
    }

    fn db_with_instance(session: &str) -> (tempfile::TempDir, HcomDb) {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, session_id)
                 VALUES ('memo', 'qoder', 'listening', 'ready', 0, 0, ?1)",
                [session],
            )
            .unwrap();
        db.set_process_binding("proc-1", session, "memo").unwrap();
        (dir, db)
    }

    fn hook_ctx(dir: &std::path::Path) -> HcomContext {
        let env = [("HCOM_PROCESS_ID".to_string(), "proc-1".to_string())]
            .into_iter()
            .collect();
        HcomContext::from_env(&env, dir.to_path_buf())
    }

    #[test]
    fn hooks_refuse_foreign_owners_and_rejected_rebinding() {
        for incoming in ["own-sid", "new-sid"] {
            let (dir, db) = db_with_instance("own-sid");
            db.conn()
                .execute(
                    "UPDATE instances SET tool = 'claude' WHERE name = 'memo'",
                    [],
                )
                .unwrap();
            let ctx = hook_ctx(dir.path());
            for event in [
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "Stop",
                "SessionEnd",
            ] {
                let payload = HookPayload::from_qoder(
                    event,
                    json!({
                        "session_id": incoming, "cwd": "/wrong", "reason": "exit",
                        "tool_name": "Bash", "tool_input": {"command": "echo wrong"}
                    }),
                );
                let command = format!("qoder-{}", event.to_lowercase());
                let (output, ack) = route(&db, &ctx, &command, &payload);
                assert_eq!(output, json!({}), "{event}");
                assert!(ack.is_none());
                let row = db.get_instance_full("memo").unwrap().unwrap();
                assert_eq!(row.session_id.as_deref(), Some("own-sid"));
                assert_eq!(row.status, ST_LISTENING);
                assert_ne!(row.directory, "/wrong");
            }
        }
    }

    #[test]
    fn foreign_session_owner_cannot_replace_qoder_process_session() {
        let (dir, db) = db_with_instance("own-sid");
        db.conn().execute(
            "INSERT INTO instances (name, tool, status, status_time, created_at, session_id) VALUES ('other', 'claude', 'listening', 0, 0, 'foreign-sid')", []
        ).unwrap();
        db.set_process_binding("foreign-proc", "foreign-sid", "other")
            .unwrap();
        db.rebind_instance_session("other", "foreign-sid").unwrap();
        let payload = HookPayload::from_qoder(
            "UserPromptSubmit",
            json!({
                "session_id": "foreign-sid", "cwd": "/wrong"
            }),
        );
        assert!(resolved_instance(&db, &hook_ctx(dir.path()), &payload).is_none());
        let row = db.get_instance_full("memo").unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("own-sid"));
        assert_eq!(
            db.get_session_binding("foreign-sid").unwrap().as_deref(),
            Some("other")
        );
    }

    #[test]
    fn lazy_binding_rejection_preserves_existing_qoder_instance() {
        let (dir, db) = db_with_instance("own-sid");
        let mut ctx = hook_ctx(dir.path());
        ctx.raw_env.insert("HCOM_TOOL".into(), "claude".into());
        let payload = HookPayload::from_qoder(
            "UserPromptSubmit",
            json!({
                "session_id": "new-sid", "cwd": "/wrong"
            }),
        );
        assert!(resolved_instance(&db, &ctx, &payload).is_none());
        let row = db.get_instance_full("memo").unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("own-sid"));
        assert_eq!(row.status, ST_LISTENING);
    }

    #[test]
    fn sessionend_only_stops_the_instances_own_session() {
        let (dir, db) = db_with_instance("own-sid");
        let ctx = hook_ctx(dir.path());
        let end = |sid: &str, reason: &str| {
            handle_sessionend(
                &db,
                &ctx,
                &HookPayload::from_qoder(
                    "SessionEnd",
                    json!({"session_id": sid, "reason": reason}),
                ),
            )
        };

        end("some-old-sid", "other");
        assert!(db.get_instance_full("memo").unwrap().is_some());

        end("own-sid", "other");
        assert!(db.get_instance_full("memo").unwrap().is_none());
    }

    #[test]
    fn sessionend_clear_and_resume_keep_the_instance() {
        let (dir, db) = db_with_instance("own-sid");
        let ctx = hook_ctx(dir.path());
        for reason in ["clear", "resume"] {
            handle_sessionend(
                &db,
                &ctx,
                &HookPayload::from_qoder(
                    "SessionEnd",
                    json!({"session_id": "own-sid", "reason": reason}),
                ),
            );
            let inst = db.get_instance_full("memo").unwrap().unwrap();
            assert_eq!(inst.status, "listening", "{reason}");
        }
    }

    #[test]
    fn stop_blocks_with_pending_messages_and_listens_otherwise() {
        let (dir, db) = db_with_instance("own-sid");
        let ctx = hook_ctx(dir.path());
        let payload = HookPayload::from_qoder(
            "Stop",
            json!({"session_id": "own-sid", "stop_hook_active": false}),
        );
        let (out, ack) = handle_stop(&db, &ctx, &payload);
        assert_eq!(out, json!({}));
        assert!(ack.is_none());
        assert_eq!(
            db.get_instance_full("memo").unwrap().unwrap().status,
            "listening"
        );
    }

    #[test]
    fn notification_permission_prompt_blocks_and_tool_end_unblocks() {
        let (dir, db) = db_with_instance("own-sid");
        let ctx = hook_ctx(dir.path());
        let note = HookPayload::from_qoder(
            "Notification",
            json!({"session_id": "own-sid", "notification_type": "permission_prompt"}),
        );
        handle_notification(&db, &ctx, &note);
        let inst = db.get_instance_full("memo").unwrap().unwrap();
        assert_eq!(
            (inst.status.as_str(), inst.status_context.as_str()),
            ("blocked", "approval")
        );

        let idle = HookPayload::from_qoder(
            "Notification",
            json!({"session_id": "own-sid", "notification_type": "idle_prompt"}),
        );
        handle_notification(&db, &ctx, &idle);
        assert_eq!(
            db.get_instance_full("memo").unwrap().unwrap().status,
            "blocked"
        );

        let post = HookPayload::from_qoder(
            "PostToolUse",
            json!({"session_id": "own-sid", "tool_name": "Bash"}),
        );
        handle_posttooluse(&db, &ctx, &post);
        let inst = db.get_instance_full("memo").unwrap().unwrap();
        assert_eq!(inst.status, "active");
        assert_eq!(inst.status_context, "approved:Bash");
    }

    // Auto-allow is POSIX-only (see `auto_allows`); on Windows every prompt blocks.
    #[cfg(not(windows))]
    #[test]
    fn auto_allowed_hcom_command_does_not_mark_the_agent_blocked() {
        let (dir, db) = db_with_instance("own-sid");
        let env = [
            ("HCOM_PROCESS_ID".to_string(), "proc-1".to_string()),
            ("HCOM_AUTO_APPROVE".to_string(), "1".to_string()),
        ]
        .into_iter()
        .collect();
        let ctx = HcomContext::from_env(&env, dir.path().to_path_buf());
        let note = |command: &str| {
            HookPayload::from_qoder(
                "Notification",
                json!({
                    "session_id": "own-sid",
                    "notification_type": "permission_prompt",
                    "message": "Tool Bash requires confirmation",
                    "details": {"toolName": "Bash", "input": {"command": command}},
                }),
            )
        };
        handle_notification(&db, &ctx, &note("hcom listen 20"));
        assert_eq!(
            db.get_instance_full("memo").unwrap().unwrap().status,
            "listening"
        );
        // Anything the hook would not allow is a real prompt.
        handle_notification(&db, &ctx, &note("rm -rf ~"));
        assert_eq!(
            db.get_instance_full("memo").unwrap().unwrap().status,
            "blocked"
        );
    }

    #[test]
    fn userpromptsubmit_binds_lazily_and_bootstraps_once() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        // Launcher placeholder: process-bound, no session yet (SessionStart
        // never fired, as in an untrusted folder).
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at)
                 VALUES ('memo', 'qoder', 'pending', 'new', 0, 0)",
                [],
            )
            .unwrap();
        db.set_process_binding("proc-1", "", "memo").ok();
        let ctx = hook_ctx(dir.path());
        let payload = HookPayload::from_qoder(
            "UserPromptSubmit",
            json!({"session_id": "late-sid", "prompt": "<hcom>", "cwd": "/w",
                   "transcript_path": "/h/.qoder/projects/p/late-sid.jsonl"}),
        );
        let (out, ack) = handle_userpromptsubmit(&db, &ctx, &payload);
        assert!(ack.is_none());
        assert_eq!(
            out["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert!(
            out["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("memo")
        );
        let inst = db.get_instance_full("memo").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("late-sid"));
        assert_eq!(inst.transcript_path, "/h/.qoder/projects/p/late-sid.jsonl");
        assert_eq!(inst.status_context, "trigger");
        assert_ne!(inst.name_announced, 0);

        let (again, _) = handle_userpromptsubmit(&db, &ctx, &payload);
        assert_eq!(again, json!({}));
    }

    #[test]
    fn hook_names_in_config_match_the_spec() {
        let spec_names: std::collections::HashSet<_> =
            crate::tool::Tool::Qoder.hooks().iter().copied().collect();
        let config_names: std::collections::HashSet<_> = QODER_HOOK_CONFIGS
            .iter()
            .map(|(_, _, command, _)| *command)
            .collect();
        // PostToolUseFailure shares a handler but has its own entry.
        assert_eq!(spec_names, config_names);
        assert_eq!(hook_type_for_command("qoder-stop"), "Stop");
        assert_eq!(hook_type_for_command("nope"), "Unknown");
    }
}
