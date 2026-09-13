//! GitHub Copilot CLI native hook handlers and hooks/hcom.json management.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::{DeliveryAck, HookPayload, common};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::paths;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_LISTENING};

const HCOM_TRIGGER: &str = "<hcom>";
const HOOK_TIMEOUT_SECS: u64 = 15;
// PascalCase selects Copilot's VS Code-compatible payload format; changing to
// camelCase also changes payload fields, so it requires checking
// HookPayload::from_copilot_native, not just renaming entries in this table.
// `command` is the documented cross-platform fallback; explicit `bash` and
// `powershell` fields take precedence on their respective platforms.
// https://docs.github.com/en/copilot/reference/hooks-reference
//
// Keep Notification, PermissionRequest and PostToolUseFailure: their handlers
// support idle delivery, approval detection and tool-failure status. These CLI
// events are documented; the cloud agent supports a smaller event set.
const COPILOT_HOOK_COMMANDS: &[(&str, &str, bool, Option<&str>)] = &[
    ("SessionStart", "copilot-sessionstart", false, None),
    ("UserPromptSubmit", "copilot-userpromptsubmit", false, None),
    ("PreToolUse", "copilot-pretooluse", false, None),
    ("PermissionRequest", "copilot-permissionrequest", true, None),
    ("PostToolUse", "copilot-posttooluse", false, None),
    (
        "PostToolUseFailure",
        "copilot-posttoolusefailure",
        false,
        None,
    ),
    (
        "Notification",
        "copilot-notification",
        false,
        Some("agent_idle|permission_prompt"),
    ),
    ("Stop", "copilot-agentstop", false, None),
    ("SubagentStart", "copilot-subagentstart", false, None),
    ("SubagentStop", "copilot-subagentstop", false, None),
    ("SessionEnd", "copilot-sessionend", false, None),
];

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("existing Copilot hook file at {} could not be read: {source}", path.display())]
    ExistingReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing Copilot hook file at {} is not valid JSON: {source}", path.display())]
    ExistingParseFailed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("existing Copilot hook file at {} must be a JSON object", path.display())]
    ExistingRootNotObject { path: PathBuf },
    #[error("failed to create Copilot hook directory {}: {source}", path.display())]
    DirCreateFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON serialization failed: {0}")]
    SerializationFailed(#[from] serde_json::Error),
    #[error("atomic write to {} failed: {source}", path.display())]
    AtomicWriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("post-write Copilot hook verification failed for {}", .0.display())]
    PostWriteVerifyFailed(PathBuf),
}

fn copilot_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("COPILOT_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    crate::runtime_env::tool_config_root().join(".copilot")
}

pub fn get_copilot_hooks_path() -> PathBuf {
    copilot_config_dir().join("hooks").join("hcom.json")
}

pub fn get_copilot_settings_path() -> PathBuf {
    copilot_config_dir().join("settings.json")
}

fn build_copilot_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn is_hcom_copilot_command(command: &str) -> bool {
    COPILOT_HOOK_COMMANDS.iter().any(|(_, suffix, _, _)| {
        command == build_copilot_hook_command(suffix) || command.ends_with(suffix)
    })
}

fn expected_hook(command: &str, matcher: Option<&str>) -> Value {
    let mut obj = serde_json::Map::from_iter([
        ("type".to_string(), Value::String("command".to_string())),
        (
            "command".to_string(),
            Value::String(build_copilot_hook_command(command)),
        ),
        ("timeoutSec".to_string(), json!(HOOK_TIMEOUT_SECS)),
    ]);
    if let Some(matcher) = matcher {
        obj.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    Value::Object(obj)
}

fn read_json_object(path: &Path) -> Result<serde_json::Map<String, Value>, SetupError> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let content =
        std::fs::read_to_string(path).map_err(|source| SetupError::ExistingReadFailed {
            path: path.to_path_buf(),
            source,
        })?;
    let value = serde_json::from_str::<Value>(&content).map_err(|source| {
        SetupError::ExistingParseFailed {
            path: path.to_path_buf(),
            source,
        }
    })?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| SetupError::ExistingRootNotObject {
            path: path.to_path_buf(),
        })
}

fn write_json(path: &Path, value: &Value) -> Result<(), SetupError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SetupError::DirCreateFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let content = serde_json::to_string_pretty(value)?;
    paths::atomic_write_io(path, &content).map_err(|source| SetupError::AtomicWriteFailed {
        path: path.to_path_buf(),
        source,
    })
}

fn merge_hcom_hooks(root: &mut Value, include_permissions: bool) {
    if !root.is_object() {
        *root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    obj.entry("version".to_string()).or_insert_with(|| json!(1));
    let hooks = obj.entry("hooks".to_string()).or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();

    for entries in hooks.values_mut() {
        if let Some(entries) = entries.as_array_mut() {
            entries.retain(|entry| {
                !entry
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_hcom_copilot_command)
            });
        }
    }

    for (event, command, permissions_only, matcher) in COPILOT_HOOK_COMMANDS {
        if *permissions_only && !include_permissions {
            continue;
        }
        let entries = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        if !entries.is_array() {
            *entries = json!([]);
        }
        entries
            .as_array_mut()
            .unwrap()
            .push(expected_hook(command, *matcher));
    }
    hooks.retain(|_, entries| {
        entries
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
    });
}

fn remove_hcom_hooks(root: &mut Value) {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for entries in hooks.values_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        entries.retain(|entry| {
            !entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(is_hcom_copilot_command)
        });
    }
    hooks.retain(|_, entries| {
        entries
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
    });
}

fn verify_hooks_at(path: &Path, include_permissions: bool) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    COPILOT_HOOK_COMMANDS
        .iter()
        .filter(|(_, _, permissions_only, _)| include_permissions || !*permissions_only)
        .all(|(event, command, _, _)| {
            hooks
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry.get("command").and_then(Value::as_str)
                            == Some(build_copilot_hook_command(command).as_str())
                            && entry.get("timeoutSec").and_then(Value::as_u64).is_some()
                    })
                })
        })
}

pub fn remove_copilot_hooks() -> bool {
    let path = get_copilot_hooks_path();
    if !path.exists() {
        return true;
    }
    match read_json_object(&path) {
        Ok(root) => {
            let mut value = Value::Object(root);
            remove_hcom_hooks(&mut value);
            write_json(&path, &value).is_ok()
        }
        Err(_) => false,
    }
}

pub fn try_setup_copilot_hooks(include_permissions: bool) -> Result<(), SetupError> {
    let hooks_path = get_copilot_hooks_path();
    let mut hooks = Value::Object(read_json_object(&hooks_path)?);
    merge_hcom_hooks(&mut hooks, include_permissions);
    write_json(&hooks_path, &hooks)?;
    if !verify_hooks_at(&hooks_path, include_permissions) {
        return Err(SetupError::PostWriteVerifyFailed(hooks_path));
    }
    Ok(())
}

pub fn verify_copilot_hooks_installed(include_permissions: bool) -> bool {
    verify_hooks_at(&get_copilot_hooks_path(), include_permissions)
}

fn resolve_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        payload.session_id.as_deref(),
        ctx.process_id.as_deref(),
    )
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
    instances::update_instance_position(db, instance_name, &updates);
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(session_id) = payload.session_id.as_deref().filter(|sid| !sid.is_empty()) else {
        return json!({});
    };
    let instance_name = ctx
        .process_id
        .as_deref()
        .and_then(|pid| instance_binding::bind_session_to_process(db, session_id, Some(pid)))
        .or_else(|| resolve_instance(db, ctx, payload).map(|instance| instance.name));
    let Some(instance_name) = instance_name else {
        return json!({});
    };
    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    let Some(instance) = db.get_instance_full(&instance_name).ok().flatten() else {
        return json!({});
    };
    update_position(db, ctx, payload, &instance_name);
    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );
    crate::runtime_env::set_terminal_title(&instance_name);
    crate::relay::worker::ensure_worker(true);
    common::notify_hook_instance_with_db(db, &instance_name);
    if let Some(bootstrap) =
        common::inject_bootstrap_once(db, ctx, &instance_name, &instance, "copilot")
    {
        json!({ "additionalContext": bootstrap })
    } else {
        json!({})
    }
}

fn resolved_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    let instance = resolve_instance(db, ctx, payload)?;
    update_position(db, ctx, payload, &instance.name);
    Some(instance)
}

fn handle_userpromptsubmit(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        let prompt = payload
            .raw
            .get("prompt")
            .or_else(|| payload.raw.get("initial_prompt"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let context = if prompt.trim() == HCOM_TRIGGER {
            "trigger"
        } else {
            "prompt"
        };
        lifecycle::set_status(db, &instance.name, ST_ACTIVE, context, Default::default());
    }
    json!({})
}

fn handle_pretooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        common::update_tool_status(
            db,
            &instance.name,
            "copilot",
            &payload.tool_name,
            &payload.tool_input,
        );
    }
    json!({})
}

fn pending_additional_context(db: &HcomDb, instance_name: &str) -> (Value, Option<DeliveryAck>) {
    match common::prepare_pending_messages(db, instance_name) {
        Some(prepared) => (
            json!({ "additionalContext": prepared.formatted }),
            Some(prepared.ack),
        ),
        None => (json!({}), None),
    }
}

fn handle_posttooluse(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return (json!({}), None);
    };
    pending_additional_context(db, &instance.name)
}

fn handle_agentstop(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return (json!({ "decision": "allow" }), None);
    };
    lifecycle::set_status(db, &instance.name, ST_LISTENING, "", Default::default());
    common::notify_hook_instance_with_db(db, &instance.name);
    match common::prepare_pending_messages(db, &instance.name) {
        Some(prepared) => (
            json!({ "decision": "block", "reason": prepared.formatted }),
            Some(prepared.ack),
        ),
        None => (json!({ "decision": "allow" }), None),
    }
}

fn handle_notification(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return json!({});
    };
    match payload.notification_type.as_deref() {
        Some("permission_prompt") => {
            lifecycle::set_status(
                db,
                &instance.name,
                "blocked",
                "approval",
                Default::default(),
            );
            json!({})
        }
        _ => json!({}),
    }
}

fn command_looks_safe_hcom(command: &str) -> bool {
    let trimmed = command.trim();
    for prefix in ["hcom", "uvx hcom"] {
        if trimmed == prefix {
            return true;
        }
        for safe in common::SAFE_HCOM_COMMANDS {
            let expected = format!("{prefix} {safe}");
            if trimmed == expected || trimmed.starts_with(&format!("{expected} ")) {
                return true;
            }
        }
    }
    false
}

fn handle_permissionrequest(_db: &HcomDb, _ctx: &HcomContext, payload: &HookPayload) -> Value {
    let command = payload
        .tool_input
        .get("command")
        .or_else(|| payload.tool_input.get("cmd"))
        .or_else(|| payload.tool_input.get("script"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(payload.tool_name.as_str(), "bash" | "powershell" | "shell")
        && command_looks_safe_hcom(command)
    {
        json!({ "behavior": "allow", "message": "hcom coordination command" })
    } else {
        json!({})
    }
}

fn handle_subagentstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return json!({});
    };
    let Some(instance_full) = db.get_instance_full(&instance.name).ok().flatten() else {
        return json!({});
    };
    if let Some(bootstrap) =
        common::inject_bootstrap_once(db, ctx, &instance.name, &instance_full, "copilot")
    {
        json!({ "additionalContext": bootstrap })
    } else {
        json!({})
    }
}

fn handle_sessionend(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        let reason = payload
            .raw
            .get("reason")
            .or_else(|| payload.raw.get("stop_reason"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        common::finalize_session(db, &instance.name, reason, None);
    }
    json!({})
}

fn hook_type_for_command(hook_name: &str) -> &'static str {
    COPILOT_HOOK_COMMANDS
        .iter()
        .find(|(_, command, _, _)| *command == hook_name)
        .map(|(event, _, _, _)| *event)
        .unwrap_or("Unknown")
}

pub fn dispatch_copilot_hook_native(hook_name: &str) -> i32 {
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(value) => value,
        Err(err) => {
            log::log_warn(
                "hooks",
                "copilot.parse_error",
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
                "copilot.db_error",
                &format!("hook={hook_name} err={err}"),
            );
            return 0;
        }
    };
    let ctx = HcomContext::from_os();
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }
    let payload = HookPayload::from_copilot_native(hook_type_for_command(hook_name), raw);
    let (output, delivery_ack) =
        common::dispatch_with_panic_guard("copilot", hook_name, (json!({}), None), || {
            match hook_name {
                "copilot-sessionstart" => (handle_sessionstart(&db, &ctx, &payload), None),
                "copilot-userpromptsubmit" => (handle_userpromptsubmit(&db, &ctx, &payload), None),
                "copilot-pretooluse" => (handle_pretooluse(&db, &ctx, &payload), None),
                "copilot-permissionrequest" => {
                    (handle_permissionrequest(&db, &ctx, &payload), None)
                }
                "copilot-posttooluse" | "copilot-posttoolusefailure" => {
                    handle_posttooluse(&db, &ctx, &payload)
                }
                "copilot-agentstop" | "copilot-subagentstop" => {
                    handle_agentstop(&db, &ctx, &payload)
                }
                "copilot-notification" => (handle_notification(&db, &ctx, &payload), None),
                "copilot-subagentstart" => (handle_subagentstart(&db, &ctx, &payload), None),
                "copilot-sessionend" => (handle_sessionend(&db, &ctx, &payload), None),
                _ => (json!({}), None),
            }
        });
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

    fn copilot_test_env() -> (tempfile::TempDir, PathBuf, EnvGuard) {
        let guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("HCOM_DIR", workspace.join(".hcom"));
            std::env::remove_var("COPILOT_HOME");
        }
        (dir, workspace, guard)
    }

    #[test]
    #[serial]
    fn setup_is_idempotent_and_preserves_other_hooks() {
        let (_dir, workspace, _guard) = copilot_test_env();
        let hooks_path = workspace.join(".copilot/hooks/hcom.json");
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "hooks": {
                    "SessionStart": [{ "type": "command", "command": "./custom-start.sh" }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        try_setup_copilot_hooks(true).unwrap();
        let first = std::fs::read_to_string(&hooks_path).unwrap();
        try_setup_copilot_hooks(true).unwrap();
        let second = std::fs::read_to_string(&hooks_path).unwrap();

        assert_eq!(first, second);
        assert!(verify_copilot_hooks_installed(true));
        let root: Value = serde_json::from_str(&second).unwrap();
        assert!(
            root["hooks"]["SessionStart"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hook| hook["command"] == "./custom-start.sh")
        );
        assert!(
            root["hooks"]["PermissionRequest"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hook| hook["command"]
                    == build_copilot_hook_command("copilot-permissionrequest"))
        );
    }

    #[test]
    fn safe_hcom_command_detection() {
        assert!(command_looks_safe_hcom("hcom send @luna -- hi"));
        assert!(command_looks_safe_hcom("uvx hcom list --json"));
        assert!(!command_looks_safe_hcom("hcom kill luna"));
        assert!(!command_looks_safe_hcom("echo hcom send @luna"));
    }
}
