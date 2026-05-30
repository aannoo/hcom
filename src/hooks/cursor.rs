//! Cursor Agent native hook handlers and hooks.json management.

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
const CURSOR_HOOK_COMMANDS: &[(&str, &str)] = &[
    ("sessionStart", "cursor-sessionstart"),
    ("beforeSubmitPrompt", "cursor-beforesubmitprompt"),
    ("preToolUse", "cursor-pretooluse"),
    ("postToolUse", "cursor-posttooluse"),
    ("stop", "cursor-stop"),
    ("sessionEnd", "cursor-sessionend"),
];

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("existing Cursor config at {} could not be read: {source}", path.display())]
    ExistingReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing Cursor config at {} is not valid JSON: {source}", path.display())]
    ExistingParseFailed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("existing Cursor config at {} must be a JSON object", path.display())]
    ExistingRootNotObject { path: PathBuf },
    #[error("failed to create Cursor config directory {}: {source}", path.display())]
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
    #[error("post-write Cursor hook verification failed for {}", .0.display())]
    PostWriteVerifyFailed(PathBuf),
    #[error("Cursor permissions setup failed for {}", .0.display())]
    PermissionsSetupFailed(PathBuf),
}

fn cursor_config_dir() -> PathBuf {
    crate::runtime_env::tool_config_root().join(".cursor")
}

pub fn get_cursor_hooks_path() -> PathBuf {
    cursor_config_dir().join("hooks.json")
}

pub fn get_cursor_permissions_path() -> PathBuf {
    let root = crate::runtime_env::tool_config_root();
    let filename = if dirs::home_dir().as_deref() == Some(root.as_path()) {
        "cli-config.json"
    } else {
        "cli.json"
    };
    root.join(".cursor").join(filename)
}

fn build_cursor_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn is_hcom_cursor_command(command: &str) -> bool {
    CURSOR_HOOK_COMMANDS
        .iter()
        .any(|(_, suffix)| command == build_cursor_hook_command(suffix))
}

fn expected_hook(event: &str, command: &str) -> Value {
    let mut obj = serde_json::Map::from_iter([
        (
            "command".to_string(),
            Value::String(build_cursor_hook_command(command)),
        ),
        ("timeout".to_string(), json!(HOOK_TIMEOUT_SECS)),
    ]);
    if event == "stop" {
        obj.insert("loop_limit".to_string(), Value::Null);
    }
    Value::Object(obj)
}

fn merge_hcom_hooks(root: &mut Value) {
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

    for (event, command) in CURSOR_HOOK_COMMANDS {
        let entries = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        if !entries.is_array() {
            *entries = json!([]);
        }
        let entries = entries.as_array_mut().unwrap();
        entries.retain(|entry| {
            !entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(is_hcom_cursor_command)
        });
        entries.push(expected_hook(event, command));
    }
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
                .is_some_and(is_hcom_cursor_command)
        });
    }
    hooks.retain(|_, entries| {
        entries
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
    });
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

fn verify_hooks_at(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    CURSOR_HOOK_COMMANDS.iter().all(|(event, command)| {
        hooks
            .get(*event)
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.get("command").and_then(Value::as_str)
                        == Some(build_cursor_hook_command(command).as_str())
                        && entry.get("timeout").and_then(Value::as_u64).is_some()
                        && (*event != "stop" || entry.get("loop_limit").is_some_and(Value::is_null))
                })
            })
    })
}

fn cursor_permission_rule() -> String {
    format!("Shell({})", crate::runtime_env::build_hcom_command())
}

fn update_cursor_permissions(add: bool) -> Result<(), SetupError> {
    let path = get_cursor_permissions_path();
    if !add && !path.exists() {
        return Ok(());
    }
    let mut root = read_json_object(&path)?;
    if !add {
        if let Some(permissions) = root.get_mut("permissions").and_then(Value::as_object_mut)
            && let Some(allow) = permissions.get_mut("allow").and_then(Value::as_array_mut)
        {
            let rule = cursor_permission_rule();
            allow.retain(|entry| entry.as_str() != Some(rule.as_str()));
            if allow.is_empty() {
                permissions.remove("allow");
            }
        }
        if root
            .get("permissions")
            .and_then(Value::as_object)
            .is_some_and(|permissions| permissions.is_empty())
        {
            root.remove("permissions");
        }
        return write_json(&path, &Value::Object(root));
    }
    if path.file_name().and_then(|name| name.to_str()) == Some("cli-config.json") {
        root.entry("version".to_string())
            .or_insert_with(|| json!(1));
        root.entry("editor".to_string())
            .or_insert_with(|| json!({ "vimMode": false }));
    }
    let permissions = root
        .entry("permissions".to_string())
        .or_insert_with(|| json!({ "allow": [], "deny": [] }));
    if !permissions.is_object() {
        *permissions = json!({ "allow": [], "deny": [] });
    }
    let permissions = permissions.as_object_mut().unwrap();
    let allow = permissions
        .entry("allow".to_string())
        .or_insert_with(|| json!([]));
    if !allow.is_array() {
        *allow = json!([]);
    }
    let allow = allow.as_array_mut().unwrap();
    let rule = cursor_permission_rule();
    allow.retain(|entry| entry.as_str() != Some(rule.as_str()));
    allow.push(Value::String(rule));
    permissions
        .entry("deny".to_string())
        .or_insert_with(|| json!([]));
    write_json(&path, &Value::Object(root))
}

fn verify_cursor_permissions() -> bool {
    let Ok(content) = std::fs::read_to_string(get_cursor_permissions_path()) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    let rule = cursor_permission_rule();
    root.pointer("/permissions/allow")
        .and_then(Value::as_array)
        .is_some_and(|allow| {
            allow
                .iter()
                .any(|entry| entry.as_str() == Some(rule.as_str()))
        })
}

pub fn try_setup_cursor_hooks(include_permissions: bool) -> Result<(), SetupError> {
    let hooks_path = get_cursor_hooks_path();
    let mut hooks = Value::Object(read_json_object(&hooks_path)?);
    merge_hcom_hooks(&mut hooks);
    write_json(&hooks_path, &hooks)?;
    if !verify_hooks_at(&hooks_path) {
        return Err(SetupError::PostWriteVerifyFailed(hooks_path));
    }
    if include_permissions {
        update_cursor_permissions(true)?;
        if !verify_cursor_permissions() {
            return Err(SetupError::PermissionsSetupFailed(
                get_cursor_permissions_path(),
            ));
        }
    } else {
        update_cursor_permissions(false)?;
    }
    Ok(())
}

pub fn verify_cursor_hooks_installed(check_permissions: bool) -> bool {
    verify_hooks_at(&get_cursor_hooks_path()) && (!check_permissions || verify_cursor_permissions())
}

pub fn remove_cursor_hooks() -> bool {
    let hooks_path = get_cursor_hooks_path();
    let hooks_ok = if hooks_path.exists() {
        match read_json_object(&hooks_path) {
            Ok(root) => {
                let mut value = Value::Object(root);
                remove_hcom_hooks(&mut value);
                write_json(&hooks_path, &value).is_ok()
            }
            Err(_) => false,
        }
    } else {
        true
    };
    hooks_ok && update_cursor_permissions(false).is_ok()
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
        .or_else(|| {
            payload
                .raw
                .get("workspace_roots")
                .and_then(Value::as_array)
                .and_then(|roots| roots.first())
                .and_then(Value::as_str)
        })
        .unwrap_or_else(|| ctx.cwd.to_str().unwrap_or(""));
    if !cwd.is_empty() {
        updates.insert("directory".into(), Value::String(cwd.to_string()));
    }
    instances::update_instance_position(db, instance_name, &updates);
}

fn cursor_session_env(ctx: &HcomContext) -> Value {
    const KEYS: &[&str] = &[
        "HCOM_PROCESS_ID",
        "HCOM_INSTANCE_NAME",
        "HCOM_TOOL",
        "HCOM_DIR",
        "HCOM_LAUNCHED",
        "HCOM_PTY_MODE",
        "HCOM_BACKGROUND",
        "HCOM_LAUNCHED_BY",
        "HCOM_LAUNCH_BATCH_ID",
        "HCOM_LAUNCH_EVENT_ID",
    ];
    Value::Object(
        KEYS.iter()
            .filter_map(|key| {
                ctx.raw_env
                    .get(*key)
                    .map(|value| ((*key).to_string(), Value::String(value.clone())))
            })
            .collect(),
    )
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(session_id) = payload.session_id.as_deref().filter(|sid| !sid.is_empty()) else {
        return json!({ "env": cursor_session_env(ctx) });
    };
    let instance_name = ctx
        .process_id
        .as_deref()
        .and_then(|pid| instance_binding::bind_session_to_process(db, session_id, Some(pid)))
        .or_else(|| resolve_instance(db, ctx, payload).map(|instance| instance.name));
    let Some(instance_name) = instance_name else {
        return json!({ "env": cursor_session_env(ctx) });
    };
    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    let Some(instance) = db.get_instance_full(&instance_name).ok().flatten() else {
        return json!({ "env": cursor_session_env(ctx) });
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
    let mut output = serde_json::Map::from_iter([("env".into(), cursor_session_env(ctx))]);
    if let Some(bootstrap) =
        common::inject_bootstrap_once(db, ctx, &instance_name, &instance, "cursor")
    {
        output.insert("additional_context".into(), Value::String(bootstrap));
    }
    Value::Object(output)
}

fn resolved_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    let instance = resolve_instance(db, ctx, payload)?;
    update_position(db, ctx, payload, &instance.name);
    Some(instance)
}

fn handle_beforesubmitprompt(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        let prompt = payload
            .raw
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("");
        let context = if prompt.trim() == HCOM_TRIGGER {
            "trigger"
        } else {
            "prompt"
        };
        lifecycle::set_status(db, &instance.name, ST_ACTIVE, context, Default::default());
    }
    json!({ "continue": true })
}

fn handle_pretooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        common::update_tool_status(
            db,
            &instance.name,
            "cursor",
            &payload.tool_name,
            &payload.tool_input,
        );
    }
    json!({})
}

fn handle_posttooluse(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> (Value, Option<DeliveryAck>) {
    let Some(instance) = resolved_instance(db, ctx, payload) else {
        return (json!({}), None);
    };
    match common::prepare_pending_messages(db, &instance.name) {
        Some(prepared) => (
            json!({ "additional_context": prepared.formatted }),
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
    lifecycle::set_status(db, &instance.name, ST_LISTENING, "", Default::default());
    common::notify_hook_instance_with_db(db, &instance.name);
    if payload.raw.get("status").and_then(Value::as_str) != Some("completed") {
        return (json!({}), None);
    }
    match common::prepare_pending_messages(db, &instance.name) {
        Some(prepared) => (
            json!({ "followup_message": prepared.formatted }),
            Some(prepared.ack),
        ),
        None => (json!({}), None),
    }
}

fn handle_sessionend(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        let reason = payload
            .raw
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        common::finalize_session(db, &instance.name, reason, None);
    }
    json!({})
}

/// Dispatch one Cursor JSON-on-stdin hook.
pub fn dispatch_cursor_hook_native(hook_name: &str) -> i32 {
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(value) => value,
        Err(err) => {
            log::log_warn(
                "hooks",
                "cursor.parse_error",
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
                "cursor.db_error",
                &format!("hook={hook_name} err={err}"),
            );
            return 0;
        }
    };
    let ctx = HcomContext::from_os();
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }
    let payload = HookPayload::from_cursor_native(hook_name, raw);
    let (output, delivery_ack) = common::dispatch_with_panic_guard(
        "cursor",
        hook_name,
        (json!({}), None),
        || match hook_name {
            "cursor-sessionstart" => (handle_sessionstart(&db, &ctx, &payload), None),
            "cursor-beforesubmitprompt" => (handle_beforesubmitprompt(&db, &ctx, &payload), None),
            "cursor-pretooluse" => (handle_pretooluse(&db, &ctx, &payload), None),
            "cursor-posttooluse" => handle_posttooluse(&db, &ctx, &payload),
            "cursor-stop" => handle_stop(&db, &ctx, &payload),
            "cursor-sessionend" => (handle_sessionend(&db, &ctx, &payload), None),
            _ => (json!({}), None),
        },
    );
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

    fn cursor_test_env() -> (tempfile::TempDir, PathBuf, EnvGuard) {
        let guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("HCOM_DIR", workspace.join(".hcom"));
        }
        (dir, workspace, guard)
    }

    #[test]
    #[serial]
    fn setup_is_idempotent_and_preserves_existing_hooks() {
        let (_dir, workspace, _guard) = cursor_test_env();
        let hooks_path = workspace.join(".cursor/hooks.json");
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "hooks": {
                    "sessionStart": [{ "command": "./custom-start.sh" }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        try_setup_cursor_hooks(false).unwrap();
        let first = std::fs::read_to_string(&hooks_path).unwrap();
        try_setup_cursor_hooks(false).unwrap();
        let second = std::fs::read_to_string(&hooks_path).unwrap();

        assert_eq!(first, second);
        assert!(verify_cursor_hooks_installed(false));
        let root: Value = serde_json::from_str(&second).unwrap();
        assert!(
            root["hooks"]["sessionStart"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hook| hook["command"] == "./custom-start.sh")
        );
        assert!(!workspace.join(".cursor/cli.json").exists());
    }

    #[test]
    #[serial]
    fn permissions_are_project_local_and_cleanup_preserves_other_rules() {
        let (_dir, workspace, _guard) = cursor_test_env();
        let permissions_path = workspace.join(".cursor/cli.json");
        std::fs::create_dir_all(permissions_path.parent().unwrap()).unwrap();
        std::fs::write(
            &permissions_path,
            serde_json::to_string_pretty(&json!({
                "permissions": {
                    "allow": ["Shell(custom)"]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        try_setup_cursor_hooks(true).unwrap();
        assert!(verify_cursor_hooks_installed(true));
        try_setup_cursor_hooks(false).unwrap();

        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(permissions_path).unwrap()).unwrap();
        assert_eq!(root["permissions"]["allow"], json!(["Shell(custom)"]));
        assert!(root.get("version").is_none());
        assert!(root.get("editor").is_none());
    }

    #[test]
    #[serial]
    fn remove_preserves_unrelated_hooks() {
        let (_dir, workspace, _guard) = cursor_test_env();
        let hooks_path = workspace.join(".cursor/hooks.json");
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "hooks": {
                    "sessionEnd": [{ "command": "./custom-end.sh" }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        try_setup_cursor_hooks(false).unwrap();
        assert!(remove_cursor_hooks());

        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(hooks_path).unwrap()).unwrap();
        assert_eq!(
            root["hooks"]["sessionEnd"],
            json!([{ "command": "./custom-end.sh" }])
        );
        assert!(
            root["hooks"]
                .as_object()
                .unwrap()
                .get("sessionStart")
                .is_none()
        );
    }
}
