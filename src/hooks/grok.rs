//! Grok Build (xAI `grok` CLI) native hook handlers and `~/.grok/hooks` management.
//!
//! Grok loads lifecycle hooks from `~/.grok/hooks/*.json` (always trusted) using
//! the nested Claude-compatible event format. Message delivery mirrors Cursor:
//! `additional_context` on PostToolUse and `followup_message` on Stop.

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

/// (Grok event name, hcom subcommand suffix)
const GROK_HOOK_COMMANDS: &[(&str, &str)] = &[
    ("SessionStart", "grok-sessionstart"),
    ("UserPromptSubmit", "grok-userpromptsubmit"),
    ("PreToolUse", "grok-pretooluse"),
    ("PostToolUse", "grok-posttooluse"),
    ("Stop", "grok-stop"),
    ("SessionEnd", "grok-sessionend"),
];

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("existing Grok config at {} could not be read: {source}", path.display())]
    ExistingReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing Grok config at {} is not valid JSON: {source}", path.display())]
    ExistingParseFailed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("existing Grok config at {} must be a JSON object", path.display())]
    ExistingRootNotObject { path: PathBuf },
    #[error("failed to create Grok config directory {}: {source}", path.display())]
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
    #[error("post-write Grok hook verification failed for {}", .0.display())]
    PostWriteVerifyFailed(PathBuf),
}

fn grok_config_dir() -> PathBuf {
    crate::runtime_env::tool_config_root().join(".grok")
}

fn default_grok_config_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".grok")
}

pub fn get_grok_hooks_path() -> PathBuf {
    grok_config_dir().join("hooks").join("hcom.json")
}

fn build_grok_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn is_hcom_grok_command(command: &str) -> bool {
    let trimmed = command.trim();
    GROK_HOOK_COMMANDS
        .iter()
        .any(|(_, suffix)| trimmed == build_grok_hook_command(suffix))
}

fn expected_command_hook(command: &str) -> Value {
    json!({
        "type": "command",
        "command": build_grok_hook_command(command),
        "timeout": HOOK_TIMEOUT_SECS,
    })
}

/// Grok native format:
/// ```json
/// { "hooks": { "SessionStart": [ { "hooks": [ { "type":"command", "command":"..." } ] } ] } }
/// ```
fn merge_hcom_hooks(root: &mut Value) {
    if !root.is_object() {
        *root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let hooks = obj.entry("hooks".to_string()).or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();

    for (event, command) in GROK_HOOK_COMMANDS {
        let groups = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        let groups = groups.as_array_mut().unwrap();

        // Drop any matcher group that only contained our hcom commands, and
        // strip hcom commands from mixed groups.
        groups.retain_mut(|group| {
            let Some(group_obj) = group.as_object_mut() else {
                return true;
            };
            let Some(entries) = group_obj.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            entries.retain(|entry| {
                !entry
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_hcom_grok_command)
            });
            !entries.is_empty() || group_obj.keys().any(|k| k != "hooks" && k != "matcher")
        });

        groups.push(json!({
            "hooks": [expected_command_hook(command)]
        }));
    }
}

fn remove_hcom_hooks(root: &mut Value) {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        groups.retain_mut(|group| {
            let Some(group_obj) = group.as_object_mut() else {
                return true;
            };
            let Some(entries) = group_obj.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            entries.retain(|entry| {
                !entry
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_hcom_grok_command)
            });
            !entries.is_empty()
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_some_and(|groups| !groups.is_empty()));
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
    GROK_HOOK_COMMANDS.iter().all(|(event, command)| {
        let expected = build_grok_hook_command(command);
        hooks
            .get(*event)
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups.iter().any(|group| {
                    group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|entries| {
                            entries.iter().any(|entry| {
                                entry.get("command").and_then(Value::as_str)
                                    == Some(expected.as_str())
                                    && entry.get("type").and_then(Value::as_str) == Some("command")
                            })
                        })
                })
            })
    })
}

fn remove_grok_hooks_at(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    match read_json_object(path) {
        Ok(root) => {
            let mut value = Value::Object(root);
            remove_hcom_hooks(&mut value);
            // Drop empty file content cleanup: keep {} if everything removed
            if value
                .get("hooks")
                .and_then(Value::as_object)
                .is_none_or(|h| h.is_empty())
            {
                // Leave an empty hooks object rather than delete foreign files.
                value = json!({ "hooks": {} });
            }
            write_json(path, &value).is_ok()
        }
        Err(_) => false,
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_absolute() && !paths.contains(&path) {
        paths.push(path);
    }
}

fn grok_hooks_cleanup_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        push_unique(
            &mut paths,
            home.join(".grok").join("hooks").join("hcom.json"),
        );
    }
    push_unique(&mut paths, get_grok_hooks_path());
    // Also clean a default-home path when tool_config_root is isolated.
    push_unique(
        &mut paths,
        default_grok_config_dir().join("hooks").join("hcom.json"),
    );
    paths
}

pub fn try_setup_grok_hooks(_include_permissions: bool) -> Result<(), SetupError> {
    let hooks_path = get_grok_hooks_path();
    let mut hooks = Value::Object(read_json_object(&hooks_path)?);
    merge_hcom_hooks(&mut hooks);
    write_json(&hooks_path, &hooks)?;
    if !verify_hooks_at(&hooks_path) {
        return Err(SetupError::PostWriteVerifyFailed(hooks_path));
    }
    // Grok has no separate CLI permissions file analogous to Cursor's
    // cli-config.json; auto_approve is handled at the hcom layer / user flags.
    Ok(())
}

pub fn verify_grok_hooks_installed(_check_permissions: bool) -> bool {
    verify_hooks_at(&get_grok_hooks_path())
}

pub fn remove_grok_hooks() -> bool {
    grok_hooks_cleanup_paths()
        .iter()
        .all(|path| remove_grok_hooks_at(path))
}

// ── Runtime handlers ────────────────────────────────────────────────────

fn resolve_session_id(payload: &HookPayload) -> Option<String> {
    payload
        .session_id
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("GROK_SESSION_ID")
                .ok()
                .filter(|s| !s.is_empty())
        })
}

fn resolve_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        resolve_session_id(payload).as_deref(),
        ctx.process_id.as_deref(),
    )
}

fn update_position(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload, instance_name: &str) {
    let mut updates = serde_json::Map::new();
    if let Some(session_id) = resolve_session_id(payload) {
        updates.insert("session_id".into(), Value::String(session_id));
    }
    if let Some(path) = payload.transcript_path.as_ref().filter(|s| !s.is_empty()) {
        updates.insert("transcript_path".into(), Value::String(path.clone()));
    }
    let cwd = payload
        .raw
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| payload.raw.get("workspaceRoot").and_then(Value::as_str))
        .or_else(|| payload.raw.get("workspace_root").and_then(Value::as_str))
        .unwrap_or_else(|| ctx.cwd.to_str().unwrap_or(""));
    if !cwd.is_empty() {
        updates.insert("directory".into(), Value::String(cwd.to_string()));
    }
    instances::update_instance_position(db, instance_name, &updates);
}

fn grok_session_env(ctx: &HcomContext) -> Value {
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

fn resolved_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    let instance = resolve_instance(db, ctx, payload)?;
    update_position(db, ctx, payload, &instance.name);
    Some(instance)
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    let Some(session_id) = resolve_session_id(payload) else {
        return json!({ "env": grok_session_env(ctx) });
    };
    let instance_name = ctx
        .process_id
        .as_deref()
        .and_then(|pid| instance_binding::bind_session_to_process(db, &session_id, Some(pid)))
        .or_else(|| resolve_instance(db, ctx, payload).map(|instance| instance.name));
    let Some(instance_name) = instance_name else {
        return json!({ "env": grok_session_env(ctx) });
    };
    let _ = db.rebind_instance_session(&instance_name, &session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    let Some(instance) = db.get_instance_full(&instance_name).ok().flatten() else {
        return json!({ "env": grok_session_env(ctx) });
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
    let mut output = serde_json::Map::from_iter([("env".into(), grok_session_env(ctx))]);
    if let Some(bootstrap) =
        common::inject_bootstrap_once(db, ctx, &instance_name, &instance, "grok")
    {
        // Emit both snake_case (Cursor-style) and camelCase (Claude-style) so
        // either Grok parser path can inject bootstrap context.
        output.insert(
            "additional_context".into(),
            Value::String(bootstrap.clone()),
        );
        output.insert(
            "hookSpecificOutput".into(),
            json!({
                "hookEventName": "SessionStart",
                "additionalContext": bootstrap,
            }),
        );
    }
    Value::Object(output)
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
        .or_else(|| payload.raw.get("userPrompt").and_then(Value::as_str))
        .unwrap_or("");
    let context = if prompt.trim() == HCOM_TRIGGER
        || prompt.trim().eq_ignore_ascii_case("hcom: wake")
        || prompt.contains("[hcom")
        || prompt.contains("hcom ")
    {
        "trigger"
    } else {
        "prompt"
    };
    lifecycle::set_status(db, &instance.name, ST_ACTIVE, context, Default::default());

    // Pending bus messages: only emit followup when the prompt does NOT already
    // carry the PTY full-body inject. Otherwise Grok queues a duplicate turn
    // (and dual Claude+native hooks would double that).
    match common::prepare_pending_messages(db, &instance.name) {
        Some(prepared) => {
            if common::prompt_already_carries_hcom_body(prompt, &prepared.formatted) {
                log::log_info(
                    "hooks",
                    "grok.userpromptsubmit.skip_followup",
                    &format!("instance={} prompt already carries body", instance.name),
                );
                (json!({}), Some(prepared.ack))
            } else {
                (
                    json!({ "followup_message": prepared.formatted }),
                    Some(prepared.ack),
                )
            }
        }
        None => (json!({}), None),
    }
}

fn handle_pretooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Value {
    if let Some(instance) = resolved_instance(db, ctx, payload) {
        common::update_tool_status(
            db,
            &instance.name,
            "grok",
            &payload.tool_name,
            &payload.tool_input,
        );
    }
    // PreToolUse is blocking on Grok; always allow.
    json!({ "decision": "allow" })
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
            json!({
                "additional_context": prepared.formatted,
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": prepared.formatted,
                }
            }),
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

    // Cursor only delivers on status=="completed". Grok may omit status or use
    // different values — deliver whenever there is a pending message unless the
    // turn was clearly cancelled/errored.
    let status = payload
        .raw
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    if matches!(
        status,
        "cancelled" | "canceled" | "error" | "failed" | "aborted"
    ) {
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

/// Dispatch one Grok JSON-on-stdin hook.
pub fn dispatch_grok_hook(hook_name: &str) -> i32 {
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(value) => value,
        Err(err) => {
            log::log_warn(
                "hooks",
                "grok.parse_error",
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
                "grok.db_error",
                &format!("hook={hook_name} err={err}"),
            );
            return 0;
        }
    };
    let ctx = HcomContext::from_os();
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }
    let payload = HookPayload::from_grok(hook_name, raw);
    let (output, delivery_ack) = common::dispatch_with_panic_guard(
        "grok",
        hook_name,
        (json!({ "decision": "allow" }), None),
        || match hook_name {
            "grok-sessionstart" => (handle_sessionstart(&db, &ctx, &payload), None),
            "grok-userpromptsubmit" => handle_userpromptsubmit(&db, &ctx, &payload),
            "grok-pretooluse" => (handle_pretooluse(&db, &ctx, &payload), None),
            "grok-posttooluse" => handle_posttooluse(&db, &ctx, &payload),
            "grok-stop" => handle_stop(&db, &ctx, &payload),
            "grok-sessionend" => (handle_sessionend(&db, &ctx, &payload), None),
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

    fn grok_test_env() -> (tempfile::TempDir, PathBuf, EnvGuard) {
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
        let (_dir, workspace, _guard) = grok_test_env();
        let hooks_path = workspace.join(".grok/hooks/hcom.json");
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "SessionStart": [{
                        "hooks": [{ "type": "command", "command": "./custom-start.sh" }]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        try_setup_grok_hooks(false).unwrap();
        let first = std::fs::read_to_string(&hooks_path).unwrap();
        try_setup_grok_hooks(false).unwrap();
        let second = std::fs::read_to_string(&hooks_path).unwrap();

        assert_eq!(first, second);
        assert!(verify_grok_hooks_installed(false));
        let root: Value = serde_json::from_str(&second).unwrap();
        let session_start = root["hooks"]["SessionStart"].as_array().unwrap();
        assert!(session_start.iter().any(|group| {
            group["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hook| hook["command"] == "./custom-start.sh")
        }));
        assert!(session_start.iter().any(|group| {
            group["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hook| hook["command"] == build_grok_hook_command("grok-sessionstart"))
        }));
    }

    #[test]
    #[serial]
    fn setup_replaces_stale_hcom_commands() {
        let (_dir, workspace, _guard) = grok_test_env();
        let hooks_path = workspace.join(".grok/hooks/hcom.json");
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "Stop": [
                        {
                            "hooks": [
                                { "type": "command", "command": "hcom grok-stop" },
                                { "type": "command", "command": "uvx hcom grok-stop" },
                                { "type": "command", "command": "./custom-stop.sh" }
                            ]
                        }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        try_setup_grok_hooks(false).unwrap();

        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(hooks_path).unwrap()).unwrap();
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        let all_commands: Vec<&str> = stop
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .filter_map(|h| h["command"].as_str())
            .collect();
        assert_eq!(
            all_commands
                .iter()
                .filter(|c| is_hcom_grok_command(c))
                .count(),
            1
        );
        assert!(all_commands.contains(&"./custom-stop.sh"));
    }

    #[test]
    #[serial]
    fn remove_preserves_unrelated_hooks() {
        let (_dir, workspace, _guard) = grok_test_env();
        let hooks_path = workspace.join(".grok/hooks/hcom.json");
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "SessionEnd": [{
                        "hooks": [{ "type": "command", "command": "./custom-end.sh" }]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        try_setup_grok_hooks(false).unwrap();
        assert!(remove_grok_hooks());

        let root: Value =
            serde_json::from_str(&std::fs::read_to_string(hooks_path).unwrap()).unwrap();
        assert_eq!(
            root["hooks"]["SessionEnd"],
            json!([{
                "hooks": [{ "type": "command", "command": "./custom-end.sh" }]
            }])
        );
        assert!(
            root["hooks"]
                .as_object()
                .unwrap()
                .get("SessionStart")
                .is_none()
        );
    }

    #[test]
    fn payload_from_grok_reads_camel_case() {
        let raw = json!({
            "sessionId": "sess-abc",
            "hookEventName": "pre_tool_use",
            "toolName": "run_terminal_command",
            "toolInput": { "command": "ls" },
            "cwd": "/tmp/proj",
            "workspaceRoot": "/tmp/proj"
        });
        let payload = HookPayload::from_grok("grok-pretooluse", raw);
        assert_eq!(payload.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(payload.tool, "grok");
        assert_eq!(payload.tool_name, "run_terminal_command");
        assert_eq!(payload.tool_input["command"], "ls");
        assert_eq!(payload.hook_name, "grok-pretooluse");
    }

    #[test]
    fn payload_from_grok_reads_snake_case() {
        let raw = json!({
            "session_id": "sess-xyz",
            "tool_name": "search_replace",
            "tool_input": { "file_path": "a.rs" }
        });
        let payload = HookPayload::from_grok("grok-posttooluse", raw);
        assert_eq!(payload.session_id.as_deref(), Some("sess-xyz"));
        assert_eq!(payload.tool_name, "search_replace");
    }
}
