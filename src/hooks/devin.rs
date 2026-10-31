//! Devin CLI per-run hook loading and native hook handlers.
//!
//! Devin runs Claude Code-style command hooks (JSON on stdin, JSON on stdout)
//! from its user config file, and `--config <path>` replaces that file for one
//! run. hcom loads its hooks for one run by copying the effective config (the
//! caller's `--config`, else `~/.config/devin/config.json`), merging its hooks
//! into the copy, and passing the copy with `--config`. Nothing is written to
//! the user's config, and the copy is read-only (see `prepare_per_run`). Devin runs no hooks until its workspace trust prompt is
//! answered, so the launcher passes `--respect-workspace-trust false`
//! (`launcher::inject_workspace_trust_args`).
//!
//! Devin injects `UserPromptSubmit`/`SessionStart`/`PostToolUse` hook output
//! (`hookSpecificOutput.additionalContext`) into the model's context and
//! continues a finished turn when `Stop` returns `{"decision":"block"}`; those
//! are the delivery channels. The PTY wake path in `delivery.rs` types a bare
//! `<hcom>` prompt when the agent is idle.
//!
//! Devin's payloads carry no `transcript_path` or `cwd`: the transcript path is
//! derived from the session id and the directory from the hook's cwd.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::claude::object_field;
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
const CONFIG_FILE: &str = "config.json";

/// Hook configuration: (event, matcher, hook command name).
///
/// Matchers are regexes over `tool_name`. `PreToolUse` is limited to the tools
/// whose input feeds the status detail (see `IntegrationSpec::status_detail`)
/// so read-only tool calls don't pay for a process spawn. `PermissionRequest`
/// fires only when Devin is about to ask the user, so it is registered for
/// every tool: it marks the agent blocked, and with auto-approve on it also
/// allows safe hcom commands.
const DEVIN_HOOK_CONFIGS: &[(&str, &str, &str)] = &[
    ("SessionStart", "", "devin-sessionstart"),
    ("UserPromptSubmit", "", "devin-userpromptsubmit"),
    (
        "PreToolUse",
        "^(exec|edit|write|run_subagent)$",
        "devin-pretooluse",
    ),
    ("PermissionRequest", "", "devin-permissionrequest"),
    ("PostToolUse", "", "devin-posttooluse"),
    ("Stop", "", "devin-stop"),
    ("SessionEnd", "", "devin-sessionend"),
];

pub static PER_RUN: PerRunAdapter = PerRunAdapter {
    prepare: prepare_per_run,
    // Devin support was added as per-run only; no older hcom wrote a
    // persistent Devin install, so there is nothing to clean up.
    cleanup_legacy: |_| Ok(()),
    ensure_permissions: None,
    managed_value_flags: &["--config"],
    strip_legacy_args: None,
};

fn build_devin_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn merge_per_run_config(config: &mut Value) -> Result<()> {
    let hooks = object_field(config, "hooks")?;
    for &(event, matcher, command) in DEVIN_HOOK_CONFIGS {
        let entries = hooks.entry(event).or_insert_with(|| json!([]));
        let entries = entries
            .as_array_mut()
            .with_context(|| format!("Devin config hooks.{event} must be an array"))?;
        let mut group = json!({
            "hooks": [{
                "type": "command",
                "command": build_devin_hook_command(command),
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

/// Devin's user config file for this launch's environment:
/// `$XDG_CONFIG_HOME/devin/config.json`, else `~/.config/devin/config.json`
/// (`%APPDATA%\devin\config.json` on Windows).
fn default_config_path(ctx: &LaunchCtx) -> PathBuf {
    #[cfg(windows)]
    if let Some(appdata) = ctx.var("APPDATA") {
        return PathBuf::from(appdata).join("devin").join(CONFIG_FILE);
    }
    ctx.path_var("XDG_CONFIG_HOME")
        .unwrap_or_else(|| ctx.home().join(".config"))
        .join("devin")
        .join(CONFIG_FILE)
}

/// The config Devin would load without hcom: the caller's `--config` file
/// (relative to the launch dir), else the user config. A missing user config is
/// an empty one; a missing caller file is an error, as it is for Devin.
fn base_config(ctx: &LaunchCtx, caller: Option<&str>) -> Result<Value> {
    let (path, required) = match caller {
        Some(value) => (ctx.cwd.join(value), true),
        None => (default_config_path(ctx), false),
    };
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) if !required && err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({}));
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Cannot read Devin config {}", path.display()));
        }
    };
    if source.trim().is_empty() {
        return Ok(json!({}));
    }
    let config: Value = serde_json::from_str(&source)
        .with_context(|| format!("Invalid JSON in Devin config {}", path.display()))?;
    if !config.is_object() {
        anyhow::bail!(
            "Devin config must contain a JSON object: {}",
            path.display()
        );
    }
    Ok(config)
}

/// The caller's last `--config` value is the base config; it is merged with
/// hcom's hooks and passed as the only `--config`. Earlier caller values are
/// dropped, as Devin itself only uses the last one.
fn prepare_per_run(ctx: &LaunchCtx) -> Result<RuntimeInjection> {
    let mut args = ctx.args.clone();
    let values = runtime::take_flag_values(&mut args, &["--config"]);
    let mut config = base_config(ctx, values.last().map(String::as_str))?;
    merge_per_run_config(&mut config)?;
    let text = serde_json::to_string_pretty(&config)?;
    let path = runtime::publish_file("devin", CONFIG_FILE, text.as_bytes())
        .context("Cannot publish Devin runtime config")?;
    // Devin saves settings into its `--config` file (on every interactive start
    // and when e.g. the model changes in a session), and launches with the same
    // base config share this copy. Sealed, Devin's save fails quietly and every
    // launch reads what hcom published; in-session changes last for the session.
    let dir = path
        .parent()
        .context("Devin runtime config has no parent dir")?;
    runtime::seal_dir(dir).context("Cannot make Devin runtime config read-only")?;
    runtime::insert_before_separator(
        &mut args,
        ["--config".to_string(), path.to_string_lossy().into_owned()],
    );
    Ok(RuntimeInjection {
        args,
        env: Vec::new(),
    })
}

// ── Handlers ────────────────────────────────────────────────────────────

fn resolve_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    if ctx
        .raw_env
        .get("HCOM_TOOL")
        .is_some_and(|tool| tool != "devin")
    {
        return None;
    }
    instance_binding::resolve_instance_from_binding(
        db,
        payload.session_id.as_deref(),
        ctx.process_id.as_deref(),
    )
    .filter(|instance| instance.tool == "devin")
}

fn update_position(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload, instance_name: &str) {
    let mut updates = serde_json::Map::new();
    if let Some(session_id) = payload.session_id.as_ref().filter(|s| !s.is_empty()) {
        updates.insert("session_id".into(), Value::String(session_id.clone()));
        let path = crate::transcript::devin_transcript_path(session_id);
        updates.insert(
            "transcript_path".into(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    // Devin runs hooks in the session's working directory.
    let cwd = ctx.cwd.to_str().unwrap_or("");
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
        && env_tool != "devin"
    {
        log::log_warn(
            "hooks",
            "devin.bind.tool_env_refused",
            &format!("session_id={session_id} process_id={process_id} env_tool={env_tool}"),
        );
        return None;
    }
    let instance_name = match instance_binding::bind_session_to_process_for_tool(
        db,
        session_id,
        Some(process_id),
        "devin",
        true,
    ) {
        instance_binding::ToolCheckedBind::Bound(name) => name,
        instance_binding::ToolCheckedBind::Rejected
        | instance_binding::ToolCheckedBind::Unbound => return None,
    };
    // The session id changes in a live process (`/clear`, `/resume`); keep
    // exactly one session bound to the instance.
    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    update_position(db, ctx, payload, &instance_name);
    crate::runtime_env::set_terminal_title(&instance_name);
    crate::relay::worker::try_spawn_worker();
    Some(instance_name)
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    // The session this process had before, if any. `/clear` starts a new
    // conversation in the same process with `source: "startup"`.
    let previous_session = resolve_instance(db, ctx, payload).and_then(|inst| inst.session_id);
    let Some(instance_name) = bind_session(db, ctx, payload) else {
        return json!({});
    };
    let Some(instance) = db.get_instance_full(&instance_name).ok().flatten() else {
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
    // A conversation that replaced another one in this process lacks the
    // bootstrap injected earlier, so it is re-sent; otherwise it is injected once.
    let new_conversation = previous_session
        .as_deref()
        .is_some_and(|previous| Some(previous) != payload.session_id.as_deref());
    let bootstrap = if new_conversation && instance.name_announced != 0 {
        Some(crate::bootstrap::get_bootstrap(
            db,
            ctx,
            &instance_name,
            "devin",
        ))
    } else {
        common::inject_bootstrap_once(db, ctx, &instance_name, &instance, "devin")
    };
    match bootstrap {
        Some(text) => context_output("SessionStart", &text),
        None => json!({}),
    }
}

/// The instance for a post-start hook. When SessionStart never fired or the
/// session changed without one, bind lazily from the process binding.
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
    let bootstrap = common::inject_bootstrap_once(db, ctx, &instance.name, &instance, "devin");
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
            "devin",
            &payload.tool_name,
            &payload.tool_input,
        );
    }
    json!({})
}

/// A tool call just finished, so deliver pending messages mid-turn. A finished
/// call also means a pending approval was resolved, so leave `blocked`.
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
    match common::prepare_pending_messages(db, &instance.name) {
        Some(prepared) => (
            context_output("PostToolUse", &prepared.formatted),
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

/// Whether the PermissionRequest hook auto-allows this call.
fn auto_allows(ctx: &HcomContext, tool_name: &str, tool_input: &Value) -> bool {
    let command = tool_input
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
    // POSIX shells only: the check parses POSIX quoting.
    common::auto_approve_enabled(ctx)
        && tool_name == "exec"
        && !cfg!(windows)
        && common::is_safe_hcom_command(command)
}

/// Devin is about to ask the user to approve a tool call. Allow hcom's own
/// coordination commands when auto-approve is on; anything else falls through
/// to Devin's prompt, and the agent is blocked on a human until the call ends.
fn handle_permissionrequest(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if auto_allows(ctx, &payload.tool_name, &payload.tool_input) {
        return json!({ "decision": "approve" });
    }
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        lifecycle::set_status(
            db,
            &instance.name,
            ST_BLOCKED,
            "approval",
            Default::default(),
        );
    }
    json!({})
}

/// Only the instance's own session ends it: `/clear` fires SessionEnd for the
/// old session while the process lives on with a new one, and a stale
/// session's SessionEnd must not stop a rebound instance.
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
            "devin.sessionend_process_continues",
            &format!("instance={} reason={reason}", instance.name),
        );
        return json!({});
    }
    let incoming = payload.session_id.as_deref().filter(|sid| !sid.is_empty());
    if incoming.is_some() && incoming != instance.session_id.as_deref() {
        log::log_warn(
            "hooks",
            "devin.sessionend_ignored",
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
    DEVIN_HOOK_CONFIGS
        .iter()
        .find(|(_, _, command)| *command == hook_name)
        .map(|(event, _, _)| *event)
        .unwrap_or("Unknown")
}

fn route(
    db: &HcomDb,
    ctx: &HcomContext,
    hook_name: &str,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    match hook_name {
        "devin-sessionstart" => (handle_sessionstart(db, ctx, payload), None),
        "devin-userpromptsubmit" => handle_userpromptsubmit(db, ctx, payload),
        "devin-pretooluse" => (handle_pretooluse(db, ctx, payload), None),
        "devin-permissionrequest" => (handle_permissionrequest(db, ctx, payload), None),
        "devin-posttooluse" => handle_posttooluse(db, ctx, payload),
        "devin-stop" => handle_stop(db, ctx, payload),
        "devin-sessionend" => (handle_sessionend(db, ctx, payload), None),
        _ => (json!({}), None),
    }
}

pub fn dispatch_devin_hook_native(hook_name: &str) -> i32 {
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(value) => value,
        Err(err) => {
            log::log_warn(
                "hooks",
                "devin.parse_error",
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
                "devin.db_error",
                &format!("hook={hook_name} err={err}"),
            );
            return 0;
        }
    };
    let ctx = HcomContext::from_os();
    if !common::hook_gate_check(&ctx) {
        return 0;
    }
    let payload = HookPayload::from_devin(hook_type_for_command(hook_name), raw);
    let (output, delivery_ack) =
        common::dispatch_with_panic_guard("devin", hook_name, (json!({}), None), || {
            route(&db, &ctx, hook_name, &payload)
        });
    // A no-op hook writes nothing.
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

    fn devin_test_env() -> (tempfile::TempDir, PathBuf, EnvGuard) {
        let guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        unsafe {
            std::env::set_var("HOME", dir.path().join("home"));
            std::env::set_var("HCOM_DIR", workspace.join(".hcom"));
        }
        (dir, workspace, guard)
    }

    fn user_config_path() -> PathBuf {
        default_config_path(&LaunchCtx::ambient(crate::tool::Tool::Devin, false))
    }

    fn prepared(args: &[&str], workspace: PathBuf) -> (Vec<String>, PathBuf, Value) {
        let mut ctx = LaunchCtx::ambient(crate::tool::Tool::Devin, false);
        ctx.cwd = workspace;
        ctx.args = args.iter().map(|s| s.to_string()).collect();
        let injection = prepare_per_run(&ctx).unwrap();
        let at = injection
            .args
            .iter()
            .position(|a| a == "--config")
            .expect("--config injected");
        let path = PathBuf::from(&injection.args[at + 1]);
        let config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        (injection.args, path, config)
    }

    fn commands(config: &Value, event: &str) -> Vec<String> {
        config["hooks"][event]
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
    fn per_run_config_copies_user_config_and_adds_hcom_hooks() {
        let (_dir, workspace, _guard) = devin_test_env();
        let user = user_config_path();
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(
            &user,
            r#"{"version":1,"agent":{"model":"m"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"./mine.sh"}]}]}}"#,
        )
        .unwrap();
        let (args, path, config) = prepared(&["--model", "opus", "--", "hi"], workspace);
        assert_eq!(&args[..2], ["--model", "opus"]);
        assert_eq!(args[2], "--config");
        assert_eq!(&args[4..], ["--", "hi"]);
        assert!(runtime::is_hcom_runtime_path(path.to_str().unwrap()));
        assert_eq!(config["agent"]["model"], "m");
        for (event, _, command) in DEVIN_HOOK_CONFIGS {
            assert!(
                commands(&config, event).contains(&build_devin_hook_command(command)),
                "{event}"
            );
        }
        assert_eq!(
            commands(&config, "Stop"),
            [
                "./mine.sh".to_string(),
                build_devin_hook_command("devin-stop")
            ]
        );
        assert_eq!(
            config["hooks"]["PreToolUse"][0]["matcher"],
            "^(exec|edit|write|run_subagent)$"
        );
        // The user's own config is never written.
        assert!(
            !std::fs::read_to_string(&user)
                .unwrap()
                .contains("devin-stop")
        );
    }

    #[test]
    #[serial]
    fn per_run_config_uses_the_callers_config_and_tolerates_no_user_config() {
        let (_dir, workspace, _guard) = devin_test_env();
        let (args, _, config) = prepared(&[], workspace.clone());
        assert_eq!(args.len(), 2);
        assert_eq!(
            commands(&config, "SessionStart"),
            [build_devin_hook_command("devin-sessionstart")]
        );

        std::fs::write(workspace.join("mine.json"), r#"{"theme_mode":"dark"}"#).unwrap();
        let (args, _, config) =
            prepared(&["--config", "other.json", "--config=mine.json"], workspace);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "--config");
        assert_eq!(config["theme_mode"], "dark");
    }

    #[test]
    #[serial]
    fn per_run_rejects_missing_or_malformed_caller_config() {
        let (_dir, workspace, _guard) = devin_test_env();
        std::fs::write(workspace.join("bad.json"), "{not json").unwrap();
        std::fs::write(workspace.join("hooks.json"), r#"{"hooks":{"Stop":"x"}}"#).unwrap();
        for value in ["missing.json", "bad.json", "hooks.json"] {
            let mut ctx = LaunchCtx::ambient(crate::tool::Tool::Devin, false);
            ctx.cwd = workspace.clone();
            ctx.args = vec!["--config".into(), value.into()];
            assert!(prepare_per_run(&ctx).is_err(), "{value}");
        }
    }

    #[test]
    #[serial]
    fn per_run_copy_is_shared_and_read_only() {
        let (_dir, workspace, _guard) = devin_test_env();
        let (_, path, original) = prepared(&["--model", "x"], workspace.clone());
        // A Devin session cannot save settings into the copy other launches use.
        assert!(std::fs::write(&path, "{}").is_err());
        #[cfg(unix)]
        assert!(std::fs::write(path.with_file_name("tmp.json"), "{}").is_err());
        let (_, again, config) = prepared(&["--model", "y"], workspace);
        assert_eq!(again, path);
        assert_eq!(config, original);
    }

    fn db_with_instance(session: &str) -> (tempfile::TempDir, HcomDb) {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at, session_id)
                 VALUES ('memo', 'devin', 'listening', 'ready', 0, 0, ?1)",
                [session],
            )
            .unwrap();
        db.set_process_binding("proc-1", session, "memo").unwrap();
        (dir, db)
    }

    fn hook_ctx(dir: &std::path::Path, auto_approve: bool) -> HcomContext {
        let mut env: std::collections::HashMap<String, String> =
            [("HCOM_PROCESS_ID".to_string(), "proc-1".to_string())]
                .into_iter()
                .collect();
        if auto_approve {
            env.insert("HCOM_AUTO_APPROVE".into(), "1".into());
        }
        HcomContext::from_env(&env, dir.to_path_buf())
    }

    #[test]
    fn permission_request_approves_safe_hcom_exec_and_blocks_otherwise() {
        let (dir, db) = db_with_instance("own-sid");
        let request = |ctx: &HcomContext, tool: &str, command: &str| {
            let payload = HookPayload::from_devin(
                "PermissionRequest",
                json!({"session_id": "own-sid", "tool_name": tool,
                       "tool_input": {"command": command}}),
            );
            handle_permissionrequest(&db, ctx, &payload)
        };
        let status = || {
            let row = db.get_instance_full("memo").unwrap().unwrap();
            (row.status, row.status_context)
        };

        let on = hook_ctx(dir.path(), true);
        if !cfg!(windows) {
            assert_eq!(
                request(&on, "exec", "hcom send @luna -- hi"),
                json!({"decision": "approve"})
            );
            assert_eq!(status().0, ST_LISTENING);
        }
        for (ctx, tool, command) in [
            (&on, "exec", "hcom send @luna -- hi; rm -rf ~"),
            (&on, "write", "hcom send @luna -- hi"),
            (
                &hook_ctx(dir.path(), false),
                "exec",
                "hcom send @luna -- hi",
            ),
        ] {
            assert_eq!(request(ctx, tool, command), json!({}), "{tool} {command}");
            assert_eq!(status(), (ST_BLOCKED.to_string(), "approval".to_string()));
        }

        let post = HookPayload::from_devin(
            "PostToolUse",
            json!({"session_id": "own-sid", "tool_name": "exec"}),
        );
        handle_posttooluse(&db, &on, &post);
        assert_eq!(
            status(),
            (ST_ACTIVE.to_string(), "approved:exec".to_string())
        );
    }

    #[test]
    fn stop_listens_when_nothing_is_pending() {
        let (dir, db) = db_with_instance("own-sid");
        let payload = HookPayload::from_devin(
            "Stop",
            json!({"session_id": "own-sid", "stop_hook_active": false}),
        );
        let (out, ack) = handle_stop(&db, &hook_ctx(dir.path(), false), &payload);
        assert_eq!(out, json!({}));
        assert!(ack.is_none());
        assert_eq!(
            db.get_instance_full("memo").unwrap().unwrap().status,
            ST_LISTENING
        );
    }

    #[test]
    fn sessionend_only_stops_the_instances_own_session() {
        let (dir, db) = db_with_instance("own-sid");
        let ctx = hook_ctx(dir.path(), false);
        let end = |sid: &str, reason: &str| {
            handle_sessionend(
                &db,
                &ctx,
                &HookPayload::from_devin(
                    "SessionEnd",
                    json!({"session_id": sid, "reason": reason}),
                ),
            )
        };
        end("own-sid", "clear");
        assert!(db.get_instance_full("memo").unwrap().is_some());
        end("some-old-sid", "other");
        assert!(db.get_instance_full("memo").unwrap().is_some());
        end("own-sid", "prompt_input_exit");
        assert!(db.get_instance_full("memo").unwrap().is_none());
    }

    #[test]
    fn userpromptsubmit_binds_lazily_and_bootstraps_once() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("test.db")).unwrap();
        db.init_db().unwrap();
        // Launcher placeholder: process-bound, no session yet.
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, status_time, created_at)
                 VALUES ('memo', 'devin', 'pending', 'new', 0, 0)",
                [],
            )
            .unwrap();
        db.set_process_binding("proc-1", "", "memo").ok();
        let ctx = hook_ctx(dir.path(), false);
        let payload = HookPayload::from_devin(
            "UserPromptSubmit",
            json!({"session_id": "late-sid", "prompt": "<hcom>\n"}),
        );
        let (out, ack) = handle_userpromptsubmit(&db, &ctx, &payload);
        assert!(ack.is_none());
        assert!(
            out["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("memo")
        );
        let inst = db.get_instance_full("memo").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("late-sid"));
        assert!(
            std::path::Path::new(&inst.transcript_path)
                .ends_with(std::path::Path::new("transcripts").join("late-sid.json"))
        );
        assert_eq!(inst.status_context, "trigger");

        let (again, _) = handle_userpromptsubmit(&db, &ctx, &payload);
        assert_eq!(again, json!({}));
    }

    #[test]
    fn hook_names_in_config_match_the_spec() {
        let spec_names: std::collections::HashSet<_> =
            crate::tool::Tool::Devin.hooks().iter().copied().collect();
        let config_names: std::collections::HashSet<_> = DEVIN_HOOK_CONFIGS
            .iter()
            .map(|(_, _, command)| *command)
            .collect();
        assert_eq!(spec_names, config_names);
        assert_eq!(hook_type_for_command("devin-stop"), "Stop");
        assert_eq!(hook_type_for_command("nope"), "Unknown");
    }
}
