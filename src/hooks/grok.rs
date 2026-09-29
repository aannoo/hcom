//! Grok Build hooks: lifecycle and status only.
//!
//! Grok reads hooks from every `*.json` in `$GROK_HOME/hooks/` (default
//! `~/.grok/hooks/`); there is no per-run hook source, so hcom installs a
//! persistent `hcom.json` there. Messages never go through hooks: launched
//! Grok receives them through its native queue (`delivery/grok.rs`), and a
//! Grok session not launched by hcom has no binding for these hooks to act on.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::{HookPayload, common};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::paths;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_LISTENING};

const HOOK_TIMEOUT_SECS: u64 = 15;

/// Prompts hcom queues carry this `promptId` prefix (see `delivery/grok.rs`).
pub(crate) const HCOM_PROMPT_ID_PREFIX: &str = "hcom-";

/// (Grok event name, hcom subcommand)
const GROK_HOOK_COMMANDS: &[(&str, &str)] = &[
    ("SessionStart", "grok-sessionstart"),
    ("UserPromptSubmit", "grok-userpromptsubmit"),
    ("PreToolUse", "grok-pretooluse"),
    ("PostToolUse", "grok-posttooluse"),
    ("Stop", "grok-stop"),
    ("StopFailure", "grok-stopfailure"),
    ("StopCancelled", "grok-stopcancelled"),
    ("SessionEnd", "grok-sessionend"),
];

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("failed to write Grok hooks to {}: {source}", path.display())]
    WriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// `$GROK_HOME` if set, else `<tool_config_root>/.grok`.
pub fn grok_config_dir() -> PathBuf {
    std::env::var("GROK_HOME")
        .ok()
        .map(|home| home.trim().to_string())
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::runtime_env::tool_config_root().join(".grok"))
}

/// hcom owns this whole file; Grok loads every JSON file in the directory,
/// so user hooks live in their own files.
pub fn get_grok_hooks_path() -> PathBuf {
    grok_config_dir().join("hooks").join("hcom.json")
}

fn build_grok_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn expected_hooks() -> Value {
    let hooks: serde_json::Map<String, Value> = GROK_HOOK_COMMANDS
        .iter()
        .map(|(event, command)| {
            (
                (*event).to_string(),
                json!([{ "hooks": [{
                    "type": "command",
                    "command": build_grok_hook_command(command),
                    "timeout": HOOK_TIMEOUT_SECS,
                }] }]),
            )
        })
        .collect();
    json!({ "hooks": hooks })
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Every command in the file is one of hcom's (any prefix, any version).
fn is_hcom_owned(root: &Value) -> bool {
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    hooks
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .all(|entry| {
            entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| {
                    let last = command.split_whitespace().last().unwrap_or("");
                    last.starts_with("grok-")
                        && command.split_whitespace().any(|word| {
                            Path::new(word).file_stem().and_then(|s| s.to_str()) == Some("hcom")
                        })
                })
        })
}

pub fn try_setup_grok_hooks(_include_permissions: bool) -> Result<(), SetupError> {
    let path = get_grok_hooks_path();
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let content =
            serde_json::to_string_pretty(&expected_hooks()).map_err(std::io::Error::other)?;
        paths::atomic_write_io(&path, &content)
    };
    write().map_err(|source| SetupError::WriteFailed {
        path: path.clone(),
        source,
    })
}

/// Grok has no permission-rule file hcom can manage, and a PreToolUse
/// `allow` does not skip its approval prompt, so `auto_approve` has nothing
/// to install.
pub fn verify_grok_hooks_installed(_check_permissions: bool) -> bool {
    read_json(&get_grok_hooks_path()).is_some_and(|root| root == expected_hooks())
}

/// Remove hcom's hooks file from the configured and default Grok homes.
/// A file someone added their own hooks to is left alone.
pub fn remove_grok_hooks() -> bool {
    let mut paths = vec![get_grok_hooks_path()];
    if let Some(home) = dirs::home_dir() {
        let default = home.join(".grok").join("hooks").join("hcom.json");
        if !paths.contains(&default) {
            paths.push(default);
        }
    }
    paths.iter().all(|path| match read_json(path) {
        None if !path.exists() => true,
        Some(root) if is_hcom_owned(&root) => std::fs::remove_file(path).is_ok(),
        _ => {
            log::log_warn(
                "hooks",
                "grok.remove_skipped",
                &format!("{} has non-hcom content; left in place", path.display()),
            );
            false
        }
    })
}

// ── Runtime handlers ────────────────────────────────────────────────────

fn str_field<'a>(payload: &'a HookPayload, key: &str) -> Option<&'a str> {
    payload
        .raw
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Grok also fires lifecycle events for subagent sessions, with the
/// subagent's type attached; those must not act on the parent.
fn is_subagent_event(payload: &HookPayload) -> bool {
    str_field(payload, "subagentType").is_some() || str_field(payload, "agentType").is_some()
}

fn resolve_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        payload.session_id.as_deref(),
        ctx.process_id.as_deref(),
    )
}

/// `urlencoding::encode`, which Grok uses for the cwd component of session
/// directories: everything but RFC 3986 unreserved bytes is percent-encoded.
fn encode_path_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Grok stores a session at `$GROK_HOME/sessions/<url-encoded cwd>/<id>/`.
fn transcript_path(payload: &HookPayload, cwd: &str) -> Option<String> {
    if let Some(path) = payload.transcript_path.as_ref().filter(|p| !p.is_empty()) {
        return Some(path.clone());
    }
    let session_id = payload.session_id.as_deref()?;
    let path = grok_config_dir()
        .join("sessions")
        .join(encode_path_component(cwd))
        .join(session_id)
        .join("updates.jsonl");
    Some(path.to_string_lossy().into_owned())
}

fn update_position(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload, instance_name: &str) {
    let mut updates = serde_json::Map::new();
    let cwd = str_field(payload, "cwd")
        .map(str::to_string)
        .unwrap_or_else(|| ctx.cwd.to_string_lossy().into_owned());
    if let Some(path) = transcript_path(payload, &cwd) {
        updates.insert("transcript_path".into(), Value::String(path));
    }
    if !cwd.is_empty() {
        updates.insert("directory".into(), Value::String(cwd));
    }
    instances::update_instance_position(db, instance_name, &updates);
}

/// Only the session the instance is bound to may change its status. Subagent
/// events resolve to the parent through the inherited process binding, and a
/// background subagent can outlive the parent's turn.
fn primary_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    if is_subagent_event(payload) {
        return None;
    }
    let instance = resolve_instance(db, ctx, payload)?;
    (instance.session_id.is_none() || instance.session_id == payload.session_id).then_some(instance)
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) {
    if is_subagent_event(payload) {
        return;
    }
    let Some(session_id) = payload.session_id.as_deref().filter(|s| !s.is_empty()) else {
        return;
    };
    let instance_name = ctx
        .process_id
        .as_deref()
        .and_then(|pid| instance_binding::bind_session_to_process(db, session_id, Some(pid)))
        .or_else(|| resolve_instance(db, ctx, payload).map(|instance| instance.name));
    let Some(instance_name) = instance_name else {
        return;
    };
    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
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
    // Wakes the delivery thread, which attaches once it sees the session.
    common::notify_hook_instance_with_db(db, &instance_name);
}

fn handle_userpromptsubmit(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) {
    let Some(instance) = primary_instance(db, ctx, payload) else {
        return;
    };
    update_position(db, ctx, payload, &instance.name);
    // hcom's own prompt: the delivery ack sets `deliver:<sender>`.
    if str_field(payload, "promptId").is_some_and(|id| id.starts_with(HCOM_PROMPT_ID_PREFIX)) {
        return;
    }
    lifecycle::set_status(db, &instance.name, ST_ACTIVE, "prompt", Default::default());
}

/// Pre and post: post also ends a `blocked` set while Grok asked for approval.
fn handle_tool(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) {
    if let Some(instance) = primary_instance(db, ctx, payload) {
        common::update_tool_status(
            db,
            &instance.name,
            "grok",
            &payload.tool_name,
            &payload.tool_input,
        );
    }
}

/// Every turn ends in exactly one of Stop, StopFailure or StopCancelled.
fn handle_turn_end(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload, context: &str) {
    let Some(instance) = primary_instance(db, ctx, payload) else {
        return;
    };
    // Listening even after a failure: the process is alive and a queued
    // prompt can still run once the user clears whatever stopped the turn.
    lifecycle::set_status(
        db,
        &instance.name,
        ST_LISTENING,
        context,
        Default::default(),
    );
    common::notify_hook_instance_with_db(db, &instance.name);
}

fn handle_sessionend(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) {
    let Some(instance) = primary_instance(db, ctx, payload) else {
        return;
    };
    let reason = str_field(payload, "reason").unwrap_or("unknown");
    common::finalize_session(db, &instance.name, reason, None);
}

/// Dispatch one Grok JSON-on-stdin hook. Output is always `{}`: nothing here
/// gates Grok.
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
    common::dispatch_with_panic_guard("grok", hook_name, (), || match hook_name {
        "grok-sessionstart" => handle_sessionstart(&db, &ctx, &payload),
        "grok-userpromptsubmit" => handle_userpromptsubmit(&db, &ctx, &payload),
        "grok-pretooluse" | "grok-posttooluse" => handle_tool(&db, &ctx, &payload),
        "grok-stop" => handle_turn_end(&db, &ctx, &payload, ""),
        "grok-stopfailure" => {
            let error = str_field(&payload, "error").unwrap_or("unknown");
            handle_turn_end(&db, &ctx, &payload, &format!("failure:{error}"))
        }
        "grok-stopcancelled" => {
            let reason = str_field(&payload, "reason").unwrap_or("unknown");
            handle_turn_end(&db, &ctx, &payload, &format!("cancelled:{reason}"))
        }
        "grok-sessionend" => handle_sessionend(&db, &ctx, &payload),
        _ => {}
    });
    println!("{{}}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::EnvGuard;
    use serial_test::serial;

    fn grok_home() -> (tempfile::TempDir, EnvGuard) {
        let guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("HOME", dir.path().join("home"));
            std::env::set_var("GROK_HOME", dir.path().join("grok"));
        }
        (dir, guard)
    }

    #[test]
    #[serial]
    fn setup_writes_owned_file_and_verifies() {
        let (dir, _guard) = grok_home();
        assert!(!verify_grok_hooks_installed(false));
        try_setup_grok_hooks(false).unwrap();
        assert!(verify_grok_hooks_installed(false));
        let path = dir.path().join("grok/hooks/hcom.json");
        let root = read_json(&path).unwrap();
        assert_eq!(
            root["hooks"]["Stop"][0]["hooks"][0]["command"],
            build_grok_hook_command("grok-stop")
        );
        assert!(is_hcom_owned(&root));
    }

    #[test]
    #[serial]
    fn stale_hcom_file_is_rewritten() {
        let (dir, _guard) = grok_home();
        let path = dir.path().join("grok/hooks/hcom.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"hooks":{"SubagentStart":[{"hooks":[{"type":"command","command":"hcom grok-subagentstart"}]}]}}"#,
        )
        .unwrap();
        assert!(!verify_grok_hooks_installed(false));
        try_setup_grok_hooks(false).unwrap();
        assert!(verify_grok_hooks_installed(false));
        assert!(
            read_json(&path).unwrap()["hooks"]
                .get("SubagentStart")
                .is_none()
        );
    }

    #[test]
    #[serial]
    fn remove_deletes_hcom_file_but_not_edited_one() {
        let (dir, _guard) = grok_home();
        try_setup_grok_hooks(false).unwrap();
        assert!(remove_grok_hooks());
        let path = dir.path().join("grok/hooks/hcom.json");
        assert!(!path.exists());

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let foreign = r#"{"hooks":{"Stop":[{"hooks":[
            {"type":"command","command":"hcom grok-stop"},
            {"type":"command","command":"python custom.py grok-stop"}]}]}}"#;
        std::fs::write(&path, foreign).unwrap();
        assert!(!remove_grok_hooks());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), foreign);
    }

    #[test]
    fn ownership_accepts_prefixes_and_rejects_lookalikes() {
        let owned = |command: &str| {
            is_hcom_owned(&json!({"hooks": {"Stop": [{"hooks": [{"command": command}]}]}}))
        };
        assert!(owned("hcom grok-stop"));
        assert!(owned("uvx hcom grok-stop"));
        assert!(owned("/Users/x/.cargo/bin/hcom grok-stop"));
        assert!(!owned("python custom.py grok-stop"));
        assert!(!owned("hcom-wrapper grok-stop"));
    }

    #[test]
    fn payload_reads_grok_envelope() {
        let raw = json!({
            "hookEventName": "pre_tool_use",
            "sessionId": "sess-abc",
            "toolName": "run_terminal_command",
            "toolInput": { "command": "ls" },
            "cwd": "/tmp/proj",
        });
        let payload = HookPayload::from_grok("grok-pretooluse", raw);
        assert_eq!(payload.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(payload.tool_name, "run_terminal_command");
        assert_eq!(payload.tool_input["command"], "ls");
    }

    #[test]
    #[serial]
    fn transcript_path_falls_back_to_session_dir() {
        let (dir, _guard) = grok_home();
        let payload = HookPayload::from_grok("grok-sessionstart", json!({"sessionId": "s1"}));
        assert_eq!(
            transcript_path(&payload, "/a b/c").unwrap(),
            dir.path()
                .join("grok/sessions/%2Fa%20b%2Fc/s1/updates.jsonl")
                .to_string_lossy()
        );
        let given = HookPayload::from_grok(
            "grok-sessionstart",
            json!({"sessionId": "s1", "transcriptPath": "/x/updates.jsonl"}),
        );
        assert_eq!(transcript_path(&given, "/a").unwrap(), "/x/updates.jsonl");
    }

    #[test]
    #[serial]
    fn lifecycle_ignores_subagents_and_other_sessions() {
        let (_tmp, _dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tool, status, status_context, created_at, last_event_id)
                 VALUES ('nova', 'main', 'grok', 'active', 'prompt', 0, 0)",
                [],
            )
            .unwrap();
        db.set_process_binding("proc-nova", "main", "nova").unwrap();
        let env = std::collections::HashMap::from([
            ("HCOM_PROCESS_ID".to_string(), "proc-nova".to_string()),
            ("HCOM_LAUNCHED".to_string(), "1".to_string()),
        ]);
        let ctx = HcomContext::from_env(&env, PathBuf::from("."));
        let status = || db.get_status("nova").unwrap().unwrap();

        let subagent = HookPayload::from_grok(
            "grok-stopfailure",
            json!({"sessionId": "main", "error": "rate_limit", "subagentType": "explore"}),
        );
        handle_turn_end(&db, &ctx, &subagent, "failure:rate_limit");
        let other = HookPayload::from_grok("grok-stop", json!({"sessionId": "child"}));
        handle_turn_end(&db, &ctx, &other, "");
        handle_sessionend(&db, &ctx, &other);
        assert_eq!(status(), ("active".into(), "prompt".into()));

        // A background subagent after the parent's turn ended.
        lifecycle::set_status(&db, "nova", ST_LISTENING, "", Default::default());
        let tool = HookPayload::from_grok(
            "grok-pretooluse",
            json!({"sessionId": "main", "toolName": "run_terminal_command", "subagentType": "explore"}),
        );
        handle_tool(&db, &ctx, &tool);
        let prompt = HookPayload::from_grok(
            "grok-userpromptsubmit",
            json!({"sessionId": "main", "prompt": "look", "subagentType": "explore"}),
        );
        handle_userpromptsubmit(&db, &ctx, &prompt);
        assert_eq!(status(), ("listening".into(), "".into()));

        let main = HookPayload::from_grok(
            "grok-stopfailure",
            json!({"sessionId": "main", "error": "rate_limit"}),
        );
        handle_turn_end(&db, &ctx, &main, "failure:rate_limit");
        assert_eq!(status(), ("listening".into(), "failure:rate_limit".into()));
    }
}
