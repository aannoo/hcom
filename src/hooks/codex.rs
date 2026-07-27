//! Codex native hook handlers and settings management.

use std::collections::{HashMap, HashSet};
use std::io::Write;
#[cfg(not(test))]
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::process::Stdio;
use std::sync::OnceLock;
#[cfg(not(test))]
use std::sync::mpsc;
#[cfg(not(test))]
use std::sync::{Arc, Mutex};
#[cfg(not(test))]
use std::time::Duration;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use toml_edit::{DocumentMut, Item, value};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::{HookPayload, HookResult, common, family};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::paths;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_LISTENING};

use super::common::SAFE_HCOM_COMMANDS;

const HCOM_TRIGGER: &str = "<hcom>";
const CODEX_HOOK_COMMANDS: &[(&str, &str, Option<&str>)] = &[
    (
        "SessionStart",
        "codex-sessionstart",
        Some("startup|resume|clear"),
    ),
    ("UserPromptSubmit", "codex-userpromptsubmit", None),
    ("PreToolUse", "codex-pretooluse", Some("Bash")),
    ("PostToolUse", "codex-posttooluse", Some("Bash")),
    ("Stop", "codex-stop", None),
];
const HCOM_TOOL_NAMES: &[&str] = &[
    "claude",
    "gemini",
    "codex",
    "opencode",
    "antigravity",
    "agy",
];
const CODEX_HOOKS_FEATURE_RENAME_VERSION: (u64, u64, u64) = (0, 129, 0);
const CODEX_HOOK_TRUST_MIN_VERSION: (u64, u64, u64) = (0, 131, 0);
const HCOM_CODEX_CLI_VERSION_KEY: &str = "hcom_codex_cli_version";
const HCOM_HOOK_DEFINITION_HASH_KEY: &str = "hcom_hook_definition_hash";
#[cfg(not(test))]
const CODEX_APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const CODEX_APP_SERVER_STDERR_LIMIT: usize = 8192;
type CodexHookHandler = fn(&HcomDb, &HcomContext, &HookPayload) -> HookResult;

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexHookTrustEntry {
    key: String,
    command: String,
    current_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexHookLocalEntry {
    key: String,
    command: String,
    definition_hash: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexHooksFeatureKey {
    CodexHooks,
    Hooks,
}

impl CodexHooksFeatureKey {
    fn as_str(self) -> &'static str {
        match self {
            Self::CodexHooks => "codex_hooks",
            Self::Hooks => "hooks",
        }
    }

    fn alternate(self) -> &'static str {
        match self {
            Self::CodexHooks => "hooks",
            Self::Hooks => "codex_hooks",
        }
    }
}

fn hook_noop() -> HookResult {
    HookResult::Allow {
        additional_context: None,
        system_message: None,
        delivery_ack: None,
    }
}

fn hcom_available_hint() -> HookResult {
    HookResult::Allow {
        additional_context: Some(format!(
            "[hcom available - run '{} start' to participate]",
            crate::runtime_env::build_hcom_command()
        )),
        system_message: None,
        delivery_ack: None,
    }
}

fn codex_event_name(hook_name: &str) -> &'static str {
    CODEX_HOOK_COMMANDS
        .iter()
        .find(|(_, cmd, _)| *cmd == hook_name)
        .map(|(event, _, _)| *event)
        .unwrap_or("Unknown")
}

/// Derive Codex transcript path from session_id.
pub fn derive_codex_transcript_path(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }

    let codex_base = std::env::var("CODEX_HOME").ok().unwrap_or_else(|| {
        dirs::home_dir()
            .map(|h| h.join(".codex").to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let sessions_dir = PathBuf::from(&codex_base).join("sessions");
    let pattern = format!(
        "{}/**/rollout-*-{}.jsonl",
        sessions_dir.display(),
        session_id
    );

    match glob::glob(&pattern) {
        Ok(entries) => {
            let mut matches: Vec<PathBuf> = entries.filter_map(|e| e.ok()).collect();
            if matches.is_empty() {
                return None;
            }
            matches.sort_by(|a, b| {
                let ta = a
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                let tb = b
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                tb.cmp(&ta)
            });
            matches.first().map(|p| p.to_string_lossy().to_string())
        }
        Err(_) => None,
    }
}

/// Normalize Windows verbatim paths before storing them in the instance row.
///
/// Codex can report the same transcript as `C:\...` on the initial hook and
/// `\\?\C:\...` after a resume. Keep the database representation stable so
/// resume and transcript lookup continue to refer to the same file.
fn normalize_codex_transcript_path(path: &str) -> String {
    const VERBATIM_PREFIX: &str = "\\\\?\\";
    const VERBATIM_UNC_PREFIX: &str = "\\\\?\\UNC\\";

    if let Some(unc_path) = path.strip_prefix(VERBATIM_UNC_PREFIX) {
        format!("\\\\{unc_path}")
    } else {
        path.strip_prefix(VERBATIM_PREFIX)
            .unwrap_or(path)
            .to_string()
    }
}

fn resolve_instance_codex(db: &HcomDb, ctx: &HcomContext, session_id: &str) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        Some(session_id).filter(|s| !s.is_empty()),
        ctx.process_id.as_deref(),
    )
}

fn bind_vanilla_instance_codex(
    db: &HcomDb,
    session_id: &str,
    transcript_path: Option<&str>,
) -> Option<String> {
    let pending = common::get_pending_instances(db);
    if pending.is_empty() {
        return None;
    }

    let derived_path = if transcript_path.is_none() || transcript_path == Some("") {
        derive_codex_transcript_path(session_id)
    } else {
        None
    };
    let effective_path = transcript_path
        .filter(|s| !s.is_empty())
        .or(derived_path.as_deref())?;
    let effective_path = normalize_codex_transcript_path(effective_path);

    let instance_name = common::find_last_bind_marker(&effective_path)?;

    family::bind_vanilla_instance(
        db,
        &instance_name,
        Some(session_id).filter(|s| !s.is_empty()),
        Some(&effective_path),
        "codex",
        "codex-sessionstart",
    )
}

fn resolve_codex_instance(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> Option<InstanceRow> {
    let session_id = payload.session_id.as_deref().unwrap_or("");
    if let Some(instance) = resolve_instance_codex(db, ctx, session_id) {
        return Some(instance);
    }

    let bound_name =
        bind_vanilla_instance_codex(db, session_id, payload.transcript_path.as_deref())?;
    db.get_instance_full(&bound_name).ok().flatten()
}

fn update_codex_position(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
    instance_name: &str,
) {
    let mut updates = serde_json::Map::new();
    let cwd = payload
        .raw
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ctx.cwd.to_string_lossy().to_string());
    if !cwd.is_empty() {
        updates.insert("directory".into(), Value::String(cwd));
    }
    if let Some(session_id) = payload.session_id.as_ref().filter(|s| !s.is_empty()) {
        updates.insert("session_id".into(), Value::String(session_id.clone()));
    }
    let transcript_path = payload.transcript_path.clone().or_else(|| {
        payload
            .session_id
            .as_deref()
            .and_then(derive_codex_transcript_path)
    });
    if let Some(tp) = transcript_path {
        updates.insert(
            "transcript_path".into(),
            Value::String(normalize_codex_transcript_path(&tp)),
        );
    }
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, &updates);
    }
}

/// Prepare pending messages for a Codex instance.
///
/// Only additionalContext — no systemMessage. Codex TUI renders both
/// as separate visible lines ("warning:" + "hook context:"), causing
/// double output for every delivered message.
fn prepare_codex_delivery(db: &HcomDb, instance_name: &str) -> Option<HookResult> {
    common::prepare_pending_messages(db, instance_name).map(|prepared| HookResult::Allow {
        additional_context: Some(prepared.formatted),
        system_message: None,
        delivery_ack: Some(prepared.ack),
    })
}

fn resolve_and_update_codex_instance(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> Option<InstanceRow> {
    let instance = resolve_codex_instance(db, ctx, payload)?;
    update_codex_position(db, ctx, payload, &instance.name);
    Some(instance)
}

fn set_prompt_active(db: &HcomDb, instance_name: &str) {
    lifecycle::set_status(db, instance_name, ST_ACTIVE, "prompt", Default::default());
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let session_id = match payload.session_id.as_deref() {
        Some(sid) if !sid.is_empty() => sid,
        _ => return hook_noop(),
    };

    let mut instance_name = if let Some(pid) = ctx.process_id.as_deref() {
        instance_binding::bind_session_to_process(db, session_id, Some(pid))
    } else {
        None
    };

    if instance_name.is_none() {
        instance_name = resolve_codex_instance(db, ctx, payload).map(|i| i.name);
    }

    let instance_name = match instance_name {
        Some(name) => name,
        None => return hcom_available_hint(),
    };

    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    update_codex_position(db, ctx, payload, &instance_name);
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

    // Bootstrap is injected at launch time via developer_instructions flag,
    // not here — Codex TUI renders hook output visibly ("hook context:").
    hook_noop()
}

fn handle_userpromptsubmit(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    let prompt = payload
        .raw
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if prompt.trim() != HCOM_TRIGGER {
        set_prompt_active(db, &instance.name);
        return hook_noop();
    }

    if let Some(result) = prepare_codex_delivery(db, &instance.name) {
        result
    } else {
        set_prompt_active(db, &instance.name);
        hook_noop()
    }
}

fn handle_pretooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    common::update_tool_status(
        db,
        &instance.name,
        "codex",
        &payload.tool_name,
        &payload.tool_input,
    );
    hook_noop()
}

fn handle_posttooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    prepare_codex_delivery(db, &instance.name).unwrap_or_else(hook_noop)
}

fn handle_stop(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    lifecycle::set_status(db, &instance.name, ST_LISTENING, "", Default::default());
    common::notify_hook_instance_with_db(db, &instance.name);
    hook_noop()
}

fn get_codex_handler(hook_name: &str) -> Option<CodexHookHandler> {
    match hook_name {
        "codex-sessionstart" => Some(handle_sessionstart),
        "codex-userpromptsubmit" => Some(handle_userpromptsubmit),
        "codex-pretooluse" => Some(handle_pretooluse),
        "codex-posttooluse" => Some(handle_posttooluse),
        "codex-stop" => Some(handle_stop),
        _ => None,
    }
}

fn dispatch_result_to_stdout(db: &HcomDb, hook_name: &str, result: HookResult) -> i32 {
    match result {
        HookResult::Allow {
            additional_context,
            system_message,
            delivery_ack,
        } => {
            let output = match (hook_name, additional_context, system_message) {
                ("codex-stop", None, None) => Some(serde_json::json!({})),
                (_, Some(ctx), sys) => {
                    let mut obj = serde_json::Map::new();
                    if let Some(msg) = sys {
                        obj.insert("systemMessage".into(), Value::String(msg));
                    }
                    obj.insert(
                        "hookSpecificOutput".into(),
                        serde_json::json!({
                            "hookEventName": codex_event_name(hook_name),
                            "additionalContext": ctx,
                        }),
                    );
                    Some(Value::Object(obj))
                }
                (_, None, Some(msg)) => Some(serde_json::json!({ "systemMessage": msg })),
                _ => None,
            };
            if let Some(json) = output {
                let mut stdout = std::io::stdout().lock();
                if serde_json::to_writer(&mut stdout, &json).is_ok()
                    && stdout.flush().is_ok()
                    && let Some(ack) = delivery_ack.as_ref()
                {
                    common::commit_delivery_ack(db, ack);
                }
            }
            0
        }
        HookResult::Block { reason } => {
            // Codex hooks on exit 2 read the reason from stderr, not stdout.
            let _ = std::io::stderr().lock().write_all(reason.as_bytes());
            2
        }
        HookResult::UpdateInput { updated_input } => {
            let _ = serde_json::to_writer(
                std::io::stdout().lock(),
                &serde_json::json!({ "updatedInput": updated_input }),
            );
            0
        }
    }
}

/// Main entry point for native Codex hooks.
pub fn dispatch_codex_hook_native(hook_name: &str) -> i32 {
    let start = std::time::Instant::now();
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(v) => v,
        Err(e) => {
            log::log_error(
                "hooks",
                "codex.parse_error",
                &format!("hook={hook_name} err={e}"),
            );
            return 0;
        }
    };

    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log::log_warn(
                "hooks",
                "codex.db_error",
                &format!("hook={hook_name} err={e}"),
            );
            return 0;
        }
    };

    let ctx = HcomContext::from_os();
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }

    let payload = HookPayload::from_codex_native(codex_event_name(hook_name), raw);
    let result = common::dispatch_with_panic_guard("codex", hook_name, hook_noop(), || {
        get_codex_handler(hook_name)
            .map(|handler| handler(&db, &ctx, &payload))
            .unwrap_or_else(hook_noop)
    });

    let exit_code = dispatch_result_to_stdout(&db, hook_name, result);
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    log::log_info(
        "hooks",
        "codex.dispatch.timing",
        &format!(
            "hook={} total_ms={:.2} exit_code={}",
            hook_name, total_ms, exit_code
        ),
    );
    exit_code
}

// ---------------------------------------------------------------------------
// Settings management — hooks.json, config.toml, execpolicy
// ---------------------------------------------------------------------------

/// Resolve the Codex config directory.
///
/// Priority: CODEX_HOME env var → tool_config_root()/.codex
fn codex_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    crate::runtime_env::tool_config_root().join(".codex")
}

/// Get path to Codex config.toml.
pub fn get_codex_config_path() -> PathBuf {
    codex_config_dir().join("config.toml")
}

/// Get path to Codex hooks.json.
pub fn get_codex_hooks_path() -> PathBuf {
    codex_config_dir().join("hooks.json")
}

/// Get path to Codex execpolicy rules directory.
pub fn get_codex_rules_path() -> PathBuf {
    codex_config_dir().join("rules")
}

fn build_codex_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn build_expected_hook_json() -> Value {
    let mut hooks = serde_json::Map::new();
    for (event, command, matcher) in CODEX_HOOK_COMMANDS {
        let mut group = serde_json::Map::new();
        if let Some(matcher) = matcher {
            group.insert("matcher".into(), Value::String((*matcher).to_string()));
        }
        group.insert(
            "hooks".into(),
            Value::Array(vec![serde_json::json!({
                "type": "command",
                "command": build_codex_hook_command(command),
            })]),
        );
        hooks.insert(
            (*event).to_string(),
            Value::Array(vec![Value::Object(group)]),
        );
    }
    Value::Object(serde_json::Map::from_iter([(
        "hooks".into(),
        Value::Object(hooks),
    )]))
}

fn is_hcom_codex_command(command: &str) -> bool {
    CODEX_HOOK_COMMANDS.iter().any(|(_, suffix, _)| {
        command == build_codex_hook_command(suffix) || command.ends_with(suffix)
    })
}

fn is_hcom_legacy_notify(item: &Item) -> bool {
    match item {
        Item::Value(v) => {
            if let Some(s) = v.as_str() {
                return s.contains("hcom") && s.contains("codex-notify");
            }
            if let Some(arr) = v.as_array() {
                let values: Vec<&str> = arr.iter().filter_map(|entry| entry.as_str()).collect();
                return values.iter().any(|s| s.contains("hcom"))
                    && values.iter().any(|s| s.contains("codex-notify"));
            }
            false
        }
        _ => false,
    }
}

fn merge_hcom_hooks(existing: &mut Value) {
    if !existing.is_object() {
        *existing = serde_json::json!({ "hooks": {} });
    }

    // Strip existing hcom hooks first so stale matchers don't accumulate.
    remove_hcom_hooks_from_json(existing);

    let hooks_obj = existing
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks_obj.is_object() {
        *hooks_obj = serde_json::json!({});
    }

    let current_hooks = hooks_obj.as_object_mut().unwrap();
    let expected = build_expected_hook_json();
    let expected_hooks = expected["hooks"].as_object().unwrap();

    for (event, expected_groups) in expected_hooks {
        let entry = current_hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !entry.is_array() {
            *entry = Value::Array(Vec::new());
        }
        let groups = entry.as_array_mut().unwrap();

        for expected_group in expected_groups.as_array().unwrap() {
            let expected_matcher = expected_group.get("matcher").and_then(|v| v.as_str());
            let new_hooks = expected_group["hooks"].as_array().unwrap();

            let matched = groups
                .iter_mut()
                .find(|g| g.get("matcher").and_then(|v| v.as_str()) == expected_matcher);

            if let Some(group) = matched {
                if !group.get("hooks").is_some_and(|v| v.is_array()) {
                    group
                        .as_object_mut()
                        .unwrap()
                        .insert("hooks".into(), Value::Array(Vec::new()));
                }
                let hooks_arr = group
                    .get_mut("hooks")
                    .and_then(|v| v.as_array_mut())
                    .unwrap();
                hooks_arr.retain(|h| {
                    !h.get("command")
                        .and_then(|v| v.as_str())
                        .is_some_and(is_hcom_codex_command)
                });
                hooks_arr.extend(new_hooks.iter().cloned());
            } else {
                groups.push(expected_group.clone());
            }
        }
    }
}

fn remove_hcom_hooks_from_json(existing: &mut Value) {
    let Some(hooks_obj) = existing.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return;
    };

    for (_, groups) in hooks_obj.iter_mut() {
        let Some(groups_arr) = groups.as_array_mut() else {
            continue;
        };
        for group in groups_arr.iter_mut() {
            if let Some(hooks_arr) = group.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                hooks_arr.retain(|h| {
                    !h.get("command")
                        .and_then(|v| v.as_str())
                        .is_some_and(is_hcom_codex_command)
                });
            }
        }
        groups_arr.retain(|group| {
            group
                .get("hooks")
                .and_then(|v| v.as_array())
                .is_some_and(|arr| !arr.is_empty())
        });
    }

    hooks_obj.retain(|_, groups| groups.as_array().is_some_and(|arr| !arr.is_empty()));
    if hooks_obj.is_empty() {
        existing.as_object_mut().unwrap().remove("hooks");
    }
}

/// Returns true if `hook` is a legacy hcom Codex entry written in the old
/// `"type":"cmd"` / `"cmd"` format used before Codex 0.129.
fn is_legacy_hcom_codex_cmd_entry(hook: &Value) -> bool {
    hook.get("type").and_then(|v| v.as_str()) == Some("cmd")
        && hook.get("cmd").and_then(|v| v.as_str()).is_some_and(|cmd| {
            CODEX_HOOK_COMMANDS
                .iter()
                .any(|(_, suffix, _)| cmd.ends_with(suffix))
        })
}

/// Remove recognized legacy `"cmd"`-keyed hcom hook entries.
/// Only called when Codex >= CODEX_HOOKS_FEATURE_RENAME_VERSION, which is when
/// the current `"command"`-keyed format is known to be supported.
fn remove_legacy_hcom_cmd_hooks_from_json(existing: &mut Value) {
    let Some(hooks_obj) = existing.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return;
    };
    for (_, groups) in hooks_obj.iter_mut() {
        let Some(groups_arr) = groups.as_array_mut() else {
            continue;
        };
        for group in groups_arr.iter_mut() {
            if let Some(hooks_arr) = group.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                hooks_arr.retain(|h| !is_legacy_hcom_codex_cmd_entry(h));
            }
        }
        groups_arr.retain(|group| {
            group
                .get("hooks")
                .and_then(|v| v.as_array())
                .is_some_and(|arr| !arr.is_empty())
        });
    }
    hooks_obj.retain(|_, groups| groups.as_array().is_some_and(|arr| !arr.is_empty()));
    if hooks_obj.is_empty() {
        existing.as_object_mut().unwrap().remove("hooks");
    }
}

fn codex_hook_event_state_label(event: &str) -> &'static str {
    match event {
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "Stop" => "stop",
        _ => "unknown",
    }
}

fn hcom_hook_definition_hash(event: &str, group: &Value, hook: &Value) -> String {
    use sha2::{Digest, Sha256};

    let definition = serde_json::json!({
        "event": event,
        "matcher": group.get("matcher").cloned().unwrap_or(Value::Null),
        "hook": hook,
    });
    let encoded = serde_json::to_vec(&definition).unwrap_or_default();
    let digest = Sha256::digest(&encoded);
    let hex = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(&mut acc, "{b:02x}");
        acc
    });
    format!("sha256:{hex}")
}

fn hcom_hook_local_entries_from_hooks_json(
    json: &Value,
    hooks_path: &Path,
) -> Vec<CodexHookLocalEntry> {
    let source = hooks_path.to_path_buf();
    let Some(hooks_obj) = json.get("hooks").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    for (event, _, _) in CODEX_HOOK_COMMANDS {
        let Some(groups) = hooks_obj.get(*event).and_then(|v| v.as_array()) else {
            continue;
        };
        for (group_index, group) in groups.iter().enumerate() {
            let Some(hooks) = group.get("hooks").and_then(|v| v.as_array()) else {
                continue;
            };
            for (handler_index, hook) in hooks.iter().enumerate() {
                let Some(command) = hook.get("command").and_then(|v| v.as_str()) else {
                    continue;
                };
                if is_hcom_codex_command(command) {
                    entries.push(CodexHookLocalEntry {
                        key: format!(
                            "{}:{}:{}:{}",
                            source.display(),
                            codex_hook_event_state_label(event),
                            group_index,
                            handler_index
                        ),
                        command: command.to_string(),
                        definition_hash: hcom_hook_definition_hash(event, group, hook),
                    });
                }
            }
        }
    }
    entries
}

fn hcom_hook_state_keys_from_hooks_json(json: &Value, hooks_path: &Path) -> HashSet<String> {
    hcom_hook_local_entries_from_hooks_json(json, hooks_path)
        .into_iter()
        .map(|entry| entry.key)
        .collect()
}

fn hcom_hook_definition_hashes_from_hooks_json(
    json: &Value,
    hooks_path: &Path,
) -> HashMap<String, String> {
    hcom_hook_local_entries_from_hooks_json(json, hooks_path)
        .into_iter()
        .map(|entry| (entry.key, entry.definition_hash))
        .collect()
}

fn hcom_hook_definition_hashes_from_hooks_path(
    hooks_path: &Path,
) -> Result<HashMap<String, String>, VerifyFailReason> {
    let hooks_content = std::fs::read_to_string(hooks_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            VerifyFailReason::HooksPathMissing(hooks_path.to_path_buf())
        }
        _ => VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()),
    })?;
    let hooks_json: Value = serde_json::from_str(&hooks_content)
        .map_err(|_| VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()))?;
    Ok(hcom_hook_definition_hashes_from_hooks_json(
        &hooks_json,
        hooks_path,
    ))
}

fn hcom_hook_local_entries_from_hooks_path(
    hooks_path: &Path,
) -> Result<Vec<CodexHookLocalEntry>, VerifyFailReason> {
    let hooks_content = std::fs::read_to_string(hooks_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            VerifyFailReason::HooksPathMissing(hooks_path.to_path_buf())
        }
        _ => VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()),
    })?;
    let hooks_json: Value = serde_json::from_str(&hooks_content)
        .map_err(|_| VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()))?;
    Ok(hcom_hook_local_entries_from_hooks_json(
        &hooks_json,
        hooks_path,
    ))
}

fn expected_hcom_hook_commands() -> HashSet<String> {
    CODEX_HOOK_COMMANDS
        .iter()
        .map(|(_, command, _)| build_codex_hook_command(command))
        .collect()
}

fn parse_hcom_hook_entries_from_hooks_list(
    value: &Value,
) -> Result<Vec<CodexHookTrustEntry>, String> {
    let hooks = value
        .pointer("/result/data/0/hooks")
        .or_else(|| value.pointer("/data/0/hooks"))
        .or_else(|| value.get("hooks"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| "codex hooks/list response did not contain hooks".to_string())?;

    let expected = expected_hcom_hook_commands();
    let mut entries = Vec::new();
    for hook in hooks {
        let Some(command) = hook.get("command").and_then(|v| v.as_str()) else {
            continue;
        };
        if !expected.contains(command) {
            continue;
        }
        let key = hook
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("hcom hook {command} missing key"))?;
        let current_hash = hook
            .get("currentHash")
            .or_else(|| hook.get("current_hash"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("hcom hook {command} missing currentHash"))?;
        entries.push(CodexHookTrustEntry {
            key: key.to_string(),
            command: command.to_string(),
            current_hash: current_hash.to_string(),
        });
    }

    let found: HashSet<&str> = entries.iter().map(|entry| entry.command.as_str()).collect();
    let missing: Vec<String> = expected
        .iter()
        .filter(|command| !found.contains(command.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "codex hooks/list missing hcom hooks: {}",
            missing.join(", ")
        ));
    }

    Ok(entries)
}

#[cfg(test)]
fn test_hcom_hook_entries_from_hooks_json(
    hooks_path: &Path,
) -> Result<Vec<CodexHookTrustEntry>, String> {
    let content = std::fs::read_to_string(hooks_path).map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    let keys = hcom_hook_state_keys_from_hooks_json(&json, hooks_path);
    let commands = expected_hcom_hook_commands();
    if keys.len() != commands.len() {
        return Err(format!(
            "test hooks.json contained {} hcom hook keys, expected {}",
            keys.len(),
            commands.len()
        ));
    }
    let mut keys: Vec<String> = keys.into_iter().collect();
    keys.sort();
    Ok(keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| CodexHookTrustEntry {
            key,
            command: format!("test-hcom-hook-{index}"),
            current_hash: format!("sha256:test-{index}"),
        })
        .collect())
}

fn fetch_codex_hcom_hook_entries() -> Result<Vec<CodexHookTrustEntry>, String> {
    #[cfg(test)]
    {
        if let Ok(value) = std::env::var("HCOM_TEST_CODEX_HOOKS_LIST_JSON") {
            if value == "__fail__" {
                return Err("test hook list failure".to_string());
            }
            let json: Value = serde_json::from_str(&value).map_err(|e| e.to_string())?;
            return parse_hcom_hook_entries_from_hooks_list(&json);
        }
        test_hcom_hook_entries_from_hooks_json(&get_codex_hooks_path())
    }

    #[cfg(not(test))]
    {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        // TODO: If Codex changes hooks/list discovery to depend on each launch
        // cwd, pass the target launch cwd through instead of using hcom's cwd.
        let mut child = crate::terminal::executable_command("codex")
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start codex app-server: {e}"))?;

        let stderr_buf = child
            .stderr
            .take()
            .map(spawn_bounded_stderr_reader)
            .unwrap_or_else(|| Arc::new(Mutex::new(String::new())));

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture codex app-server stdout".to_string())?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to capture codex app-server stdin".to_string())?;
        let initialize = serde_json::json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "hcom",
                    "title": "hcom",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": { "experimentalApi": true }
            }
        });
        writeln!(stdin, "{initialize}").map_err(|e| e.to_string())?;
        read_jsonrpc_response(&rx, 1).map_err(|e| with_app_server_stderr(e, &stderr_buf))?;

        writeln!(
            stdin,
            "{}",
            serde_json::json!({"method":"initialized","params":{}})
        )
        .map_err(|e| e.to_string())?;
        let request = serde_json::json!({
            "method": "hooks/list",
            "id": 2,
            "params": { "cwds": [cwd] }
        });
        writeln!(stdin, "{request}").map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())?;

        let response =
            read_jsonrpc_response(&rx, 2).map_err(|e| with_app_server_stderr(e, &stderr_buf));
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        parse_hcom_hook_entries_from_hooks_list(&response?)
    }
}

#[cfg(not(test))]
fn spawn_bounded_stderr_reader<R>(mut stderr: R) -> Arc<Mutex<String>>
where
    R: Read + Send + 'static,
{
    let buf = Arc::new(Mutex::new(String::new()));
    let thread_buf = Arc::clone(&buf);
    std::thread::spawn(move || {
        let mut chunk = [0_u8; 1024];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    let Ok(mut current) = thread_buf.lock() else {
                        break;
                    };
                    let remaining = CODEX_APP_SERVER_STDERR_LIMIT.saturating_sub(current.len());
                    if remaining == 0 {
                        continue;
                    }
                    for ch in text.chars() {
                        if current.len() + ch.len_utf8() > CODEX_APP_SERVER_STDERR_LIMIT {
                            break;
                        }
                        current.push(ch);
                    }
                }
                Err(_) => break,
            }
        }
    });
    buf
}

#[cfg(not(test))]
fn with_app_server_stderr(mut error: String, stderr_buf: &Arc<Mutex<String>>) -> String {
    let stderr = stderr_buf
        .lock()
        .ok()
        .map(|buf| buf.trim().to_string())
        .unwrap_or_default();
    if !stderr.is_empty() {
        error.push_str("; stderr: ");
        error.push_str(&stderr);
    }
    error
}

#[cfg(not(test))]
fn read_jsonrpc_response(rx: &mpsc::Receiver<String>, id: i64) -> Result<Value, String> {
    let deadline = std::time::Instant::now() + CODEX_APP_SERVER_TIMEOUT;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(format!(
                "timed out waiting for codex app-server response id {id}"
            ));
        }
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(now))
            .map_err(|e| format!("codex app-server closed before response id {id}: {e}"))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(|v| v.as_i64()) == Some(id) {
            if let Some(error) = value.get("error") {
                return Err(format!(
                    "codex app-server returned error for id {id}: {error}"
                ));
            }
            return Ok(value);
        }
    }
}

fn parse_codex_cli_version(output: &str) -> Option<(u64, u64, u64)> {
    output
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|token| {
            let mut parts = token.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            Some((major, minor, patch))
        })
}

fn codex_cli_version_output_for_hook_trust() -> Result<String, String> {
    #[cfg(test)]
    if let Ok(version) = std::env::var("HCOM_TEST_CODEX_CLI_VERSION") {
        return Ok(version);
    }

    #[cfg(not(test))]
    {
        static CACHE: OnceLock<Result<String, String>> = OnceLock::new();
        CACHE
            .get_or_init(|| {
                let output = crate::terminal::executable_command("codex")
                    .arg("--version")
                    .output()
                    .map_err(|e| {
                        format!("could not run codex --version for hook trust check: {e}")
                    })?;
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                Ok(text.trim().to_string())
            })
            .clone()
    }

    #[cfg(test)]
    {
        Err("HCOM_TEST_CODEX_CLI_VERSION not set".to_string())
    }
}

fn codex_hook_trust_version() -> Result<Option<String>, String> {
    let output = codex_cli_version_output_for_hook_trust()?;
    let version = parse_codex_cli_version(&output).ok_or_else(|| {
        format!("could not parse version from codex --version output: {output:?}")
    })?;
    if version >= CODEX_HOOK_TRUST_MIN_VERSION {
        Ok(Some(format!("{}.{}.{}", version.0, version.1, version.2)))
    } else {
        Ok(None)
    }
}

fn codex_hooks_feature_key_for_version(version: (u64, u64, u64)) -> CodexHooksFeatureKey {
    if version >= CODEX_HOOKS_FEATURE_RENAME_VERSION {
        CodexHooksFeatureKey::Hooks
    } else {
        CodexHooksFeatureKey::CodexHooks
    }
}

/// Cached result of `detect_codex_hooks_feature_key`.  Tests bypass the
/// cache when `HCOM_TEST_CODEX_CLI_VERSION` is set so that changing the
/// env var mid-process produces the expected value.
static CODEX_HOOKS_FEATURE_KEY_CACHE: OnceLock<CodexHooksFeatureKey> = OnceLock::new();

fn detect_codex_hooks_feature_key() -> CodexHooksFeatureKey {
    #[cfg(test)]
    if let Ok(version) = std::env::var("HCOM_TEST_CODEX_CLI_VERSION") {
        return parse_codex_cli_version(&version)
            .map(codex_hooks_feature_key_for_version)
            .unwrap_or(CodexHooksFeatureKey::Hooks);
    }

    *CODEX_HOOKS_FEATURE_KEY_CACHE.get_or_init(|| {
        let output = match crate::terminal::executable_command("codex")
            .arg("--version")
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                crate::log::log_warn(
                    "hooks",
                    "codex.version_failed",
                    &format!("could not run codex --version: {e}"),
                );
                return CodexHooksFeatureKey::Hooks;
            }
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        match parse_codex_cli_version(&text) {
            Some(version) => codex_hooks_feature_key_for_version(version),
            None => {
                crate::log::log_warn(
                    "hooks",
                    "codex.version_unparseable",
                    "could not parse version from codex --version output",
                );
                CodexHooksFeatureKey::Hooks
            }
        }
    })
}

fn write_hcom_hook_trust_state(
    config_path: &Path,
    entries: &[CodexHookTrustEntry],
    stale_keys: &HashSet<String>,
    codex_cli_version: &str,
    definition_hashes: &HashMap<String, String>,
) -> Result<(), String> {
    let mut doc: DocumentMut = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .map_err(|e| e.to_string())?
            .parse::<DocumentMut>()
            .unwrap_or_default()
    } else {
        DocumentMut::new()
    };

    if !doc.contains_table("hooks") {
        doc["hooks"] = Item::Table(toml_edit::Table::new());
    }
    if doc["hooks"]
        .get("state")
        .is_none_or(|item| !item.is_table_like())
    {
        doc["hooks"]["state"] = Item::Table(toml_edit::Table::new());
    }
    let state = doc["hooks"]["state"]
        .as_table_like_mut()
        .ok_or_else(|| "hooks.state config section is not a table".to_string())?;

    for key in stale_keys {
        state.remove(key);
    }

    for entry in entries {
        if state
            .get(&entry.key)
            .is_none_or(|item| !item.is_table_like())
        {
            state.insert(&entry.key, Item::Table(toml_edit::Table::new()));
        }
        let Some(item) = state.get_mut(&entry.key) else {
            continue;
        };
        item["trusted_hash"] = value(entry.current_hash.clone());
        item["enabled"] = value(true);
        item[HCOM_CODEX_CLI_VERSION_KEY] = value(codex_cli_version.to_string());
        if let Some(definition_hash) = definition_hashes.get(&entry.key) {
            item[HCOM_HOOK_DEFINITION_HASH_KEY] = value(definition_hash.clone());
        }
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    paths::atomic_write_io(config_path, &doc.to_string()).map_err(|e| e.to_string())
}

pub(crate) fn ensure_codex_hcom_hooks_trusted() -> Result<(), String> {
    let Some(codex_cli_version) = codex_hook_trust_version()? else {
        return Ok(());
    };

    let entries = fetch_codex_hcom_hook_entries()?;
    let definition_hashes = hcom_hook_definition_hashes_from_hooks_path(&get_codex_hooks_path())
        .map_err(|e| e.to_string())?;
    let config_path = get_codex_config_path();
    write_hcom_hook_trust_state(
        &config_path,
        &entries,
        &HashSet::new(),
        &codex_cli_version,
        &definition_hashes,
    )
}

pub(crate) fn codex_hcom_hooks_trusted_locally() -> bool {
    let codex_cli_version = match codex_hook_trust_version() {
        Ok(Some(version)) => version,
        Ok(None) => {
            return true;
        }
        Err(_) => {
            return false;
        }
    };

    codex_hcom_hooks_trusted_locally_for_version(&codex_cli_version)
}

fn codex_hcom_hooks_trusted_locally_for_version(codex_cli_version: &str) -> bool {
    let hooks_path = get_codex_hooks_path();
    let hooks_content = match std::fs::read_to_string(&hooks_path) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let hooks_json: Value = match serde_json::from_str(&hooks_content) {
        Ok(json) => json,
        Err(_) => return false,
    };
    if verify_hooks_json_value(&hooks_json).is_err() {
        return false;
    }
    let entries = hcom_hook_local_entries_from_hooks_json(&hooks_json, &hooks_path);
    if entries.len() != CODEX_HOOK_COMMANDS.len() {
        return false;
    }
    let definition_hashes: HashMap<String, String> = entries
        .iter()
        .map(|entry| (entry.key.clone(), entry.definition_hash.clone()))
        .collect();
    let keys: HashSet<String> = entries.into_iter().map(|entry| entry.key).collect();

    codex_hcom_hook_keys_trusted_for_version(
        &get_codex_config_path(),
        &keys,
        codex_cli_version,
        &definition_hashes,
    )
}

fn codex_hcom_hook_keys_trusted_for_version(
    config_path: &Path,
    keys: &HashSet<String>,
    codex_cli_version: &str,
    definition_hashes: &HashMap<String, String>,
) -> bool {
    let config_content = match std::fs::read_to_string(config_path) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let doc = match config_content.parse::<DocumentMut>() {
        Ok(doc) => doc,
        Err(_) => return false,
    };
    let Some(state) = doc
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(|state| state.as_table_like())
    else {
        return false;
    };

    keys.iter().all(|key| {
        let Some(entry) = state.get(key) else {
            return false;
        };
        let Some(trusted_hash) = entry.get("trusted_hash").and_then(|v| v.as_str()) else {
            return false;
        };
        !trusted_hash.is_empty()
            && entry.get("enabled").and_then(|v| v.as_bool()) != Some(false)
            && entry
                .get(HCOM_CODEX_CLI_VERSION_KEY)
                .and_then(|v| v.as_str())
                == Some(codex_cli_version)
            && entry
                .get(HCOM_HOOK_DEFINITION_HASH_KEY)
                .and_then(|v| v.as_str())
                == definition_hashes.get(key).map(String::as_str)
    })
}

#[cfg(test)]
fn hcom_command_for_hook_state_key(key: &str) -> String {
    let mut parts = key.rsplitn(4, ':');
    let _handler_index = parts.next();
    let _group_index = parts.next();
    let event_label = parts.next();
    if let Some(event_label) = event_label {
        for (event, command, _) in CODEX_HOOK_COMMANDS {
            if codex_hook_event_state_label(event) == event_label {
                return build_codex_hook_command(command);
            }
        }
    }
    key.to_string()
}

fn verify_hcom_hook_keys_trusted_for_version(
    config_path: &Path,
    entries: &[CodexHookLocalEntry],
    codex_cli_version: &str,
) -> Result<(), VerifyFailReason> {
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| VerifyFailReason::HookTrustUnavailable(e.to_string()))?;
    let doc = content
        .parse::<DocumentMut>()
        .map_err(|e| VerifyFailReason::HookTrustUnavailable(e.to_string()))?;
    let state = doc
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(|state| state.as_table_like())
        .ok_or_else(|| VerifyFailReason::HookTrustUnavailable("hooks.state missing".to_string()))?;

    for entry in entries {
        let command = entry.command.clone();
        let Some(state_entry) = state.get(&entry.key) else {
            return Err(VerifyFailReason::HookTrustMissing { command });
        };
        if state_entry.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            return Err(VerifyFailReason::HookDisabled { command });
        }
        let trusted_hash = state_entry
            .get("trusted_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| VerifyFailReason::HookTrustMissing {
                command: command.clone(),
            })?;
        if trusted_hash.is_empty() {
            return Err(VerifyFailReason::HookTrustMissing { command });
        }
        if state_entry
            .get(HCOM_CODEX_CLI_VERSION_KEY)
            .and_then(|v| v.as_str())
            != Some(codex_cli_version)
        {
            return Err(VerifyFailReason::HookTrustStale { command });
        }
        if state_entry
            .get(HCOM_HOOK_DEFINITION_HASH_KEY)
            .and_then(|v| v.as_str())
            != Some(entry.definition_hash.as_str())
        {
            return Err(VerifyFailReason::HookTrustStale { command });
        }
    }

    Ok(())
}

fn verify_hcom_hook_trust_state(
    config_path: &Path,
    hooks_path: &Path,
) -> Result<(), VerifyFailReason> {
    let Some(codex_cli_version) =
        codex_hook_trust_version().map_err(VerifyFailReason::CodexUnavailable)?
    else {
        return Ok(());
    };
    let entries = hcom_hook_local_entries_from_hooks_path(hooks_path)?;
    if entries.len() != CODEX_HOOK_COMMANDS.len() {
        return Err(VerifyFailReason::HookTrustUnavailable(format!(
            "could not derive all hcom hook trust keys from {}",
            hooks_path.display()
        )));
    }

    verify_hcom_hook_keys_trusted_for_version(config_path, &entries, &codex_cli_version)
}

fn ensure_codex_feature_enabled(
    config_path: &Path,
    feature_key: CodexHooksFeatureKey,
) -> Result<(), String> {
    let mut doc: DocumentMut = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .map_err(|e| e.to_string())?
            .parse::<DocumentMut>()
            .unwrap_or_default()
    } else {
        DocumentMut::new()
    };

    if !doc.contains_table("features") {
        doc["features"] = Item::Table(toml_edit::Table::new());
    }
    // Codex renamed the feature flag from codex_hooks to hooks in 0.129.0.
    // Always clean the deprecated codex_hooks key if present; never remove
    // hooks — it's the shared flag for all Codex hooks, not just hcom's.
    remove_codex_hooks_aliases(&mut doc, feature_key);
    doc["features"][feature_key.as_str()] = value(true);
    // Remove the old hcom-owned codex-notify form only; leave unrelated notify untouched.
    let is_hcom_notify = doc.get("notify").is_some_and(is_hcom_legacy_notify);
    if is_hcom_notify {
        doc.remove("notify");
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if paths::atomic_write(config_path, &doc.to_string()) {
        Ok(())
    } else {
        Err("atomic_write failed".to_string())
    }
}

fn remove_codex_hooks_aliases(doc: &mut DocumentMut, feature_key: CodexHooksFeatureKey) {
    if let Some(features) = doc.get_mut("features")
        && let Some(table) = features.as_table_like_mut()
    {
        table.remove("codex_hooks");
    }

    if feature_key != CodexHooksFeatureKey::Hooks {
        return;
    }

    let Some(profiles) = doc
        .get_mut("profiles")
        .and_then(|item| item.as_table_like_mut())
    else {
        return;
    };
    for (_, profile) in profiles.iter_mut() {
        let Some(features) = profile
            .as_table_like_mut()
            .and_then(|profile| profile.get_mut("features"))
        else {
            continue;
        };
        if let Some(table) = features.as_table_like_mut() {
            table.remove("codex_hooks");
        }
    }
}

fn codex_selected_feature_enabled(config_path: &Path, feature_key: CodexHooksFeatureKey) -> bool {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<DocumentMut>() else {
        return false;
    };
    doc.get("features")
        .and_then(|item| item.get(feature_key.as_str()))
        .and_then(|item| item.as_bool())
        .unwrap_or(false)
}

fn codex_deprecated_feature_present(config_path: &Path, feature_key: CodexHooksFeatureKey) -> bool {
    if feature_key != CodexHooksFeatureKey::Hooks {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<DocumentMut>() else {
        return false;
    };
    if doc
        .get("features")
        .and_then(|item| item.get("codex_hooks"))
        .is_some()
    {
        return true;
    }

    let Some(active_profile) = doc.get("profile").and_then(|item| item.as_str()) else {
        return false;
    };
    doc.get("profiles")
        .and_then(|item| item.as_table_like())
        .and_then(|profiles| profiles.get(active_profile))
        .and_then(|profile| profile.get("features"))
        .and_then(|features| features.get("codex_hooks"))
        .is_some()
}

fn codex_feature_enabled(config_path: &Path, feature_key: CodexHooksFeatureKey) -> bool {
    if codex_selected_feature_enabled(config_path, feature_key) {
        return true;
    }

    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<DocumentMut>() else {
        return false;
    };
    // Check the version-selected key first, fall back to the alternate
    // so that a config written by an older (or newer) hcom still passes
    // verification until the next setup call canonicalizes it.
    doc.get("features")
        .and_then(|item| item.get(feature_key.alternate()))
        .and_then(|item| item.as_bool())
        .unwrap_or(false)
}

/// Whether Codex config already uses the feature flag key expected by the
/// installed Codex CLI. Verification accepts either key for compatibility, but
/// launch setup uses this to self-heal stale `codex_hooks` configs. Modern
/// Codex warns if the deprecated key is present at all, even when `hooks` is
/// also enabled, so treat that mixed state as not current.
pub(crate) fn codex_current_feature_enabled() -> bool {
    let config_path = get_codex_config_path();
    let feature_key = detect_codex_hooks_feature_key();
    codex_selected_feature_enabled(&config_path, feature_key)
        && !codex_deprecated_feature_present(&config_path, feature_key)
}

fn verify_hooks_json_at(hooks_path: &Path) -> Result<(), VerifyFailReason> {
    let content = std::fs::read_to_string(hooks_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            VerifyFailReason::HooksPathMissing(hooks_path.to_path_buf())
        }
        _ => VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()),
    })?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|_| VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()))?;
    verify_hooks_json_value(&json)
}

fn verify_hooks_json_value(json: &Value) -> Result<(), VerifyFailReason> {
    let hooks_obj = json
        .get("hooks")
        .and_then(|v| v.as_object())
        .ok_or(VerifyFailReason::HooksKeyMissing)?;

    // Check all expected hooks are present with correct matchers.
    for (event, command, matcher) in CODEX_HOOK_COMMANDS {
        let groups = match hooks_obj.get(*event).and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr,
            _ => {
                return Err(VerifyFailReason::HookEventMissing {
                    event: (*event).to_string(),
                });
            }
        };
        let expected_command = build_codex_hook_command(command);
        let expected_hook = serde_json::json!({
            "type": "command",
            "command": expected_command,
        });
        // Mirror merge_hcom_hooks: for None-matcher events only match groups
        // with no "matcher" key, not groups with "matcher":"" (which may belong
        // to other tools such as context-mode).
        let matching_group = groups.iter().find(|group| match matcher {
            Some(expected) => group.get("matcher").and_then(|v| v.as_str()) == Some(*expected),
            None => group.get("matcher").and_then(|v| v.as_str()).is_none(),
        });
        let Some(group) = matching_group else {
            return Err(VerifyFailReason::HookCommandMissing {
                event: (*event).to_string(),
                expected_command,
            });
        };
        let hooks = group
            .get("hooks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| VerifyFailReason::HookCommandMissing {
                event: (*event).to_string(),
                expected_command: expected_command.clone(),
            })?;
        let hcom_hooks: Vec<&Value> = hooks
            .iter()
            .filter(|hook| {
                hook.get("command")
                    .and_then(|v| v.as_str())
                    .is_some_and(is_hcom_codex_command)
            })
            .collect();
        if !hcom_hooks.iter().any(|hook| **hook == expected_hook) {
            return Err(VerifyFailReason::HookCommandMissing {
                event: (*event).to_string(),
                expected_command,
            });
        }
        if hcom_hooks.iter().any(|hook| **hook != expected_hook) {
            return Err(VerifyFailReason::HookDefinitionChanged {
                event: (*event).to_string(),
                expected_command,
            });
        }
    }

    // Check no stale hcom hooks exist in groups with non-matching matchers.
    for (event, groups) in hooks_obj {
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for group in groups {
            let has_hcom_command =
                group
                    .get("hooks")
                    .and_then(|v| v.as_array())
                    .is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|v| v.as_str())
                                .is_some_and(is_hcom_codex_command)
                        })
                    });
            if !has_hcom_command {
                continue;
            }
            // This group has an hcom command — it must match an expected entry.
            let group_matcher = group.get("matcher").and_then(|v| v.as_str());
            let is_expected = CODEX_HOOK_COMMANDS
                .iter()
                .any(|(exp_event, _, exp_matcher)| {
                    *exp_event == event.as_str()
                        && match exp_matcher {
                            Some(m) => group_matcher == Some(*m),
                            None => group_matcher.is_none(),
                        }
                });
            if !is_expected {
                return Err(VerifyFailReason::StaleHcomHookEntry {
                    event: event.clone(),
                    matcher: group_matcher.map(|s| s.to_string()),
                });
            }
        }
    }

    Ok(())
}

fn build_codex_rules() -> String {
    let prefix = crate::runtime_env::get_hcom_prefix();
    let prefix_parts: String = prefix
        .iter()
        .map(|p| format!("\"{}\"", p))
        .collect::<Vec<_>>()
        .join(", ");

    let mut rules = vec!["# hcom integration - auto-approve safe commands".to_string()];
    for cmd in SAFE_HCOM_COMMANDS {
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\"], decision=\"allow\")",
            prefix_parts, cmd
        ));
    }
    for tool in HCOM_TOOL_NAMES {
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\", \"--help\"], decision=\"allow\")",
            prefix_parts, tool
        ));
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\", \"-h\"], decision=\"allow\")",
            prefix_parts, tool
        ));
    }
    rules.join("\n") + "\n"
}

/// Set up Codex execpolicy rules for auto-approval.
pub fn setup_codex_execpolicy() -> bool {
    let rules_dir = get_codex_rules_path();
    let rules_file = rules_dir.join("hcom.rules");
    let rule_content = build_codex_rules();

    if rules_file.exists()
        && std::fs::read_to_string(&rules_file).ok().as_deref() == Some(rule_content.as_str())
    {
        return true;
    }

    let _ = std::fs::create_dir_all(&rules_dir);
    paths::atomic_write(&rules_file, &rule_content)
}

/// Remove hcom execpolicy rule.
pub fn remove_codex_execpolicy() -> bool {
    let rules_file = get_codex_rules_path().join("hcom.rules");
    if rules_file.exists() {
        std::fs::remove_file(&rules_file).is_ok()
    } else {
        true
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum VerifyFailReason {
    #[error("Codex config.toml missing: {}", .0.display())]
    ConfigPathMissing(PathBuf),
    #[error("Codex hooks.json missing: {}", .0.display())]
    HooksPathMissing(PathBuf),
    #[error("Codex experimental hooks feature not enabled in {}", .0.display())]
    CodexFeatureDisabled(PathBuf),
    #[error("Codex hooks.json missing or not parseable as JSON: {}", .0.display())]
    HooksUnreadable(PathBuf),
    #[error("'hooks' key missing or not an object")]
    HooksKeyMissing,
    #[error("hook event '{event}' missing or empty")]
    HookEventMissing { event: String },
    #[error("hcom hook command not found under event '{event}' (expected: {expected_command})")]
    HookCommandMissing {
        event: String,
        expected_command: String,
    },
    #[error("hcom hook definition changed under event '{event}' (expected: {expected_command})")]
    HookDefinitionChanged {
        event: String,
        expected_command: String,
    },
    #[error("stale hcom hook entry in event '{event}' under unexpected matcher: {matcher:?}")]
    StaleHcomHookEntry {
        event: String,
        matcher: Option<String>,
    },
    #[error("Codex CLI unavailable for hook trust check: {0}")]
    CodexUnavailable(String),
    #[error("hcom Codex hook trust state unavailable: {0}")]
    HookTrustUnavailable(String),
    #[error("hcom Codex hook '{command}' has no trusted_hash in hooks.state")]
    HookTrustMissing { command: String },
    #[error("hcom Codex hook '{command}' trusted_hash is stale")]
    HookTrustStale { command: String },
    #[error("hcom Codex hook '{command}' is disabled in hooks.state")]
    HookDisabled { command: String },
    #[error("hcom.rules file missing: {}", .0.display())]
    PermissionsRulesMissing(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("failed to enable Codex experimental hooks feature in {}: {reason}", path.display())]
    EnsureFeatureFailed { path: PathBuf, reason: String },
    #[error("failed to read existing {}: {source}", path.display())]
    HooksReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON serialization failed: {0}")]
    SerializationFailed(#[from] serde_json::Error),
    #[error("failed to create parent dir {}: {source}", path.display())]
    DirCreateFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
    #[error(
        "failed to trust Codex hooks: {reason}. hcom-wrapped Codex launches may fall back to --dangerously-bypass-hook-trust, but vanilla Codex will not run hcom hooks until trust succeeds"
    )]
    HookTrustFailed { reason: String },
}

pub fn try_setup_codex_hooks(include_permissions: bool) -> Result<(), SetupError> {
    let config_path = get_codex_config_path();
    let hooks_path = get_codex_hooks_path();
    let feature_key = detect_codex_hooks_feature_key();

    ensure_codex_feature_enabled(&config_path, feature_key).map_err(|e| {
        SetupError::EnsureFeatureFailed {
            path: config_path.clone(),
            reason: e,
        }
    })?;

    let mut hooks_json = if hooks_path.exists() {
        let content =
            std::fs::read_to_string(&hooks_path).map_err(|source| SetupError::HooksReadFailed {
                path: hooks_path.clone(),
                source,
            })?;
        serde_json::from_str::<Value>(&content)
            .unwrap_or_else(|_| serde_json::json!({ "hooks": {} }))
    } else {
        serde_json::json!({ "hooks": {} })
    };
    // Strip legacy "cmd"-keyed hcom entries written by pre-0.129 installs.
    // Only safe once Codex supports the current "command"-keyed format.
    if feature_key == CodexHooksFeatureKey::Hooks {
        remove_legacy_hcom_cmd_hooks_from_json(&mut hooks_json);
    }
    let old_hcom_hook_keys = hcom_hook_state_keys_from_hooks_json(&hooks_json, &hooks_path);
    merge_hcom_hooks(&mut hooks_json);

    if let Some(parent) = hooks_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SetupError::DirCreateFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let content =
        serde_json::to_string_pretty(&hooks_json).map_err(SetupError::SerializationFailed)?;
    paths::atomic_write_io(&hooks_path, &content).map_err(|source| {
        SetupError::AtomicWriteFailed {
            path: hooks_path.clone(),
            source,
        }
    })?;

    verify_hooks_json_at(&hooks_path).map_err(|reason| SetupError::PostWriteVerifyFailed {
        path: hooks_path.clone(),
        reason,
    })?;

    match codex_hook_trust_version() {
        Ok(Some(codex_cli_version)) => {
            let definition_hashes =
                hcom_hook_definition_hashes_from_hooks_json(&hooks_json, &hooks_path);
            match fetch_codex_hcom_hook_entries().and_then(|entries| {
                let current_keys: HashSet<String> =
                    entries.iter().map(|entry| entry.key.clone()).collect();
                let stale_keys: HashSet<String> = old_hcom_hook_keys
                    .difference(&current_keys)
                    .cloned()
                    .collect();
                write_hcom_hook_trust_state(
                    &config_path,
                    &entries,
                    &stale_keys,
                    &codex_cli_version,
                    &definition_hashes,
                )
            }) {
                Ok(()) => {}
                Err(e) => return Err(SetupError::HookTrustFailed { reason: e }),
            }
        }
        Ok(None) => {}
        Err(e) => log::log_warn(
            "hooks",
            "codex.hook_trust_version_warn",
            &format!(
                "hooks installed but Codex version check failed; launch may fall back to Codex hook-trust bypass: {e}"
            ),
        ),
    }

    let ep_ok = if include_permissions {
        setup_codex_execpolicy()
    } else {
        remove_codex_execpolicy()
    };
    if !ep_ok {
        log::log_warn(
            "hooks",
            "codex.execpolicy_warn",
            "hooks installed but execpolicy write failed; auto-approval will not work",
        );
    }
    Ok(())
}

pub fn setup_codex_hooks(include_permissions: bool) -> bool {
    try_setup_codex_hooks(include_permissions).is_ok()
}

pub fn verify_codex_hooks_installed(check_permissions: bool) -> bool {
    verify_codex_hooks_inner(check_permissions).is_ok()
}

pub(crate) fn verify_codex_hooks_inner(check_permissions: bool) -> Result<(), VerifyFailReason> {
    let config_path = get_codex_config_path();
    let hooks_path = get_codex_hooks_path();

    if !config_path.exists() {
        return Err(VerifyFailReason::ConfigPathMissing(config_path));
    }
    let feature_key = detect_codex_hooks_feature_key();
    if !codex_feature_enabled(&config_path, feature_key) {
        return Err(VerifyFailReason::CodexFeatureDisabled(config_path));
    }
    // No exists() pre-check: verify_hooks_json_at converts NotFound to
    // HooksPathMissing, avoiding a stat-then-open race.
    verify_hooks_json_at(&hooks_path)?;
    verify_hcom_hook_trust_state(&config_path, &hooks_path)?;
    if check_permissions {
        let rules_file = get_codex_rules_path().join("hcom.rules");
        if !rules_file.exists() {
            return Err(VerifyFailReason::PermissionsRulesMissing(rules_file));
        }
    }
    Ok(())
}

/// Remove hcom hooks from a single Codex hooks.json + execpolicy at the given base dir.
fn remove_codex_hooks_from_dir(base: &std::path::Path) -> bool {
    let hooks_path = base.join("hooks.json");
    let rules_file = base.join("rules").join("hcom.rules");
    let mut ok = true;

    if hooks_path.exists() {
        match std::fs::read_to_string(&hooks_path) {
            Ok(content) => {
                let mut json = serde_json::from_str::<Value>(&content)
                    .unwrap_or_else(|_| serde_json::json!({ "hooks": {} }));
                remove_hcom_hooks_from_json(&mut json);
                if json.get("hooks").is_none() && json.as_object().is_some_and(|o| o.is_empty()) {
                    ok &= std::fs::remove_file(&hooks_path).is_ok();
                } else {
                    let content =
                        serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".into());
                    ok &= paths::atomic_write(&hooks_path, &content);
                }
            }
            Err(_) => ok = false,
        }
    }

    if rules_file.exists() {
        ok &= std::fs::remove_file(&rules_file).is_ok();
    }

    ok
}

/// Remove hcom hooks from Codex config.
///
/// Cleans the default (~/.codex), env-var (CODEX_HOME), and active HCOM_DIR-local paths.
pub fn remove_codex_hooks() -> bool {
    let default_dir = dirs::home_dir()
        .map(|h| h.join(".codex"))
        .unwrap_or_default();
    let env_dir = std::env::var("CODEX_HOME")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    let local_dir = codex_config_dir();

    let default_ok = remove_codex_hooks_from_dir(&default_dir);
    let env_ok = match env_dir {
        Some(ref d) if *d != default_dir => remove_codex_hooks_from_dir(d),
        _ => true,
    };
    let local_ok = if local_dir != default_dir && Some(&local_dir) != env_dir.as_ref() {
        remove_codex_hooks_from_dir(&local_dir)
    } else {
        true
    };

    default_ok && env_ok && local_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::{EnvGuard, isolated_test_env};
    use serial_test::serial;

    #[test]
    fn test_hook_payload_factory_uses_native_fields() {
        let payload = HookPayload::from_codex_native(
            "UserPromptSubmit",
            serde_json::json!({
                "session_id": "sess-1",
                "prompt": "<hcom>",
            }),
        );
        assert_eq!(payload.session_id.as_deref(), Some("sess-1"));
        assert_eq!(payload.hook_name, "UserPromptSubmit");
    }

    #[test]
    fn test_derive_transcript_empty_thread_id() {
        assert!(derive_codex_transcript_path("").is_none());
    }

    #[test]
    fn test_derive_transcript_no_match() {
        assert!(derive_codex_transcript_path("nonexistent-thread-12345").is_none());
    }

    #[test]
    fn test_normalize_transcript_path() {
        assert_eq!(
            normalize_codex_transcript_path("C:\\Users\\runner\\session.jsonl"),
            "C:\\Users\\runner\\session.jsonl"
        );
        assert_eq!(
            normalize_codex_transcript_path("\\\\?\\C:\\Users\\runner\\session.jsonl"),
            "C:\\Users\\runner\\session.jsonl"
        );
        assert_eq!(
            normalize_codex_transcript_path("\\\\?\\UNC\\server\\share\\session.jsonl"),
            "\\\\server\\share\\session.jsonl"
        );
    }

    #[test]
    #[serial]
    fn test_derive_transcript_finds_file() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions").join("project");
        std::fs::create_dir_all(&sessions).unwrap();

        let transcript = sessions.join("rollout-1-abc-123-def.jsonl");
        std::fs::File::create(&transcript).unwrap();

        let saved = std::env::var("CODEX_HOME").ok();
        unsafe { std::env::set_var("CODEX_HOME", dir.path()) };

        let result = derive_codex_transcript_path("abc-123-def");
        assert!(result.is_some(), "should find transcript file");
        assert!(result.unwrap().contains("rollout-1-abc-123-def.jsonl"));

        if let Some(v) = saved {
            unsafe { std::env::set_var("CODEX_HOME", v) };
        } else {
            unsafe { std::env::remove_var("CODEX_HOME") };
        }
    }

    // -- build_codex_rules --

    #[test]
    fn test_build_codex_rules_contains_send() {
        let rules = build_codex_rules();
        assert!(rules.contains("\"send\""));
        assert!(rules.contains("\"list\""));
        assert!(rules.contains("decision=\"allow\""));
    }

    #[test]
    fn test_build_codex_rules_contains_tool_help() {
        let rules = build_codex_rules();
        assert!(rules.contains("\"claude\", \"--help\""));
        assert!(rules.contains("\"gemini\", \"-h\""));
    }

    // -- settings setup/remove/verify --

    #[test]
    #[serial]
    fn test_setup_and_remove_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.130.0") };
        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        let hooks_path = get_codex_hooks_path();
        let config_path = get_codex_config_path();
        let hooks_content = std::fs::read_to_string(hooks_path).unwrap();
        let config_content = std::fs::read_to_string(config_path).unwrap();

        assert!(hooks_content.contains("codex-sessionstart"));
        assert!(config_content.contains("hooks = true"));
        assert!(!config_content.contains("codex-notify"));

        assert!(remove_codex_hooks());
        assert!(!verify_codex_hooks_installed(false));
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_trusts_hcom_hooks_for_modern_codex() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        let config_content = std::fs::read_to_string(get_codex_config_path()).unwrap();
        assert!(config_content.contains("trusted_hash"));
        assert!(config_content.contains("enabled = true"));
        assert!(config_content.contains("hcom_codex_cli_version = \"0.131.0\""));
        assert!(config_content.contains("hcom_hook_definition_hash"));
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_repairs_disabled_hcom_hook_state() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));

        let config_path = get_codex_config_path();
        let content = std::fs::read_to_string(&config_path).unwrap();
        let mut doc = content.parse::<DocumentMut>().unwrap();
        let state = doc["hooks"]["state"].as_table_like_mut().unwrap();
        let first_key = state.iter().next().unwrap().0.to_string();
        state.get_mut(&first_key).unwrap()["enabled"] = value(false);
        paths::atomic_write_io(&config_path, &doc.to_string()).unwrap();

        assert!(!verify_codex_hooks_installed(false));

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));
        let repaired = std::fs::read_to_string(&config_path).unwrap();
        let repaired_doc = repaired.parse::<DocumentMut>().unwrap();
        assert_eq!(
            repaired_doc["hooks"]["state"][&first_key]["enabled"].as_bool(),
            Some(true)
        );
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_repairs_stale_trusted_hash() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        let config_path = get_codex_config_path();
        let content = std::fs::read_to_string(&config_path).unwrap();
        let mut doc = content.parse::<DocumentMut>().unwrap();
        let state = doc["hooks"]["state"].as_table_like_mut().unwrap();
        let first_key = state.iter().next().unwrap().0.to_string();
        state.get_mut(&first_key).unwrap()["trusted_hash"] = value("sha256:stale");
        paths::atomic_write_io(&config_path, &doc.to_string()).unwrap();

        // Cheap verify does not spawn Codex app-server to compare currentHash.
        assert!(verify_codex_hooks_installed(false));

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));
        let repaired = std::fs::read_to_string(&config_path).unwrap();
        let repaired_doc = repaired.parse::<DocumentMut>().unwrap();
        assert_ne!(
            repaired_doc["hooks"]["state"][&first_key]["trusted_hash"].as_str(),
            Some("sha256:stale")
        );
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_repairs_version_stamped_trust_state() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.132.0") };
        assert!(!verify_codex_hooks_installed(false));

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));
        let repaired = std::fs::read_to_string(get_codex_config_path()).unwrap();
        assert!(repaired.contains("hcom_codex_cli_version = \"0.132.0\""));
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_repairs_drifted_hcom_hook_definition() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        let hooks_path = get_codex_hooks_path();
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let mut json: Value = serde_json::from_str(&content).unwrap();
        json["hooks"]["PreToolUse"][0]["hooks"][0]["statusMessage"] =
            Value::String("running".to_string());
        paths::atomic_write_io(&hooks_path, &serde_json::to_string_pretty(&json).unwrap()).unwrap();

        assert!(!verify_codex_hooks_installed(false));

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));
        let repaired: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
        assert!(
            repaired["hooks"]["PreToolUse"][0]["hooks"][0]
                .get("statusMessage")
                .is_none()
        );
    }

    #[test]
    fn test_hcom_command_for_hook_state_key() {
        assert_eq!(
            hcom_command_for_hook_state_key("/tmp/codex/hooks.json:pre_tool_use:0:0"),
            build_codex_hook_command("codex-pretooluse")
        );
    }

    #[test]
    #[serial]
    fn test_setup_preserves_unrelated_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "other-hook"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(hooks_path).unwrap();
        assert!(content.contains("other-hook"));
        assert!(content.contains("codex-posttooluse"));
    }

    #[test]
    #[serial]
    fn test_mixed_group_merge_preserves_user_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Bash",
                        "hooks": [
                            {"type": "command", "command": "user-mixed-hook"},
                            {"type": "command", "command": "old-path codex-posttooluse"}
                        ]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(content.contains("user-mixed-hook"), "user hook was dropped");
        assert!(content.contains("codex-posttooluse"), "hcom hook missing");
        let json: Value = serde_json::from_str(&content).unwrap();
        let posttool_groups = json["hooks"]["PostToolUse"].as_array().unwrap();
        let bash_group = posttool_groups
            .iter()
            .find(|g| g.get("matcher").and_then(|v| v.as_str()) == Some("Bash"))
            .expect("Bash group missing");
        let hook_cmds: Vec<&str> = bash_group["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h.get("command").and_then(|v| v.as_str()))
            .collect();
        let hcom_count = hook_cmds
            .iter()
            .filter(|c| c.contains("codex-posttooluse"))
            .count();
        assert_eq!(
            hcom_count, 1,
            "expected exactly one hcom hook, got {hcom_count}"
        );
    }

    #[test]
    #[serial]
    fn test_mixed_group_remove_preserves_user_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\nhooks = true\n").unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Bash",
                        "hooks": [
                            {"type": "command", "command": "user-remove-hook"},
                            {"type": "command", "command": "old-path codex-posttooluse"}
                        ]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(remove_codex_hooks());
        assert!(
            hooks_path.exists(),
            "hooks.json was deleted but user hook was present"
        );
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(
            content.contains("user-remove-hook"),
            "user hook was dropped"
        );
        assert!(
            !content.contains("codex-posttooluse"),
            "hcom hook was not removed"
        );
    }

    #[test]
    #[serial]
    fn test_ensure_feature_enabled_preserves_unrelated_notify() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "notify = \"some-other-notify-tool\"\n").unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("some-other-notify-tool"),
            "unrelated notify was removed"
        );
        assert!(content.contains("hooks = true"), "feature flag not set");
    }

    #[test]
    #[serial]
    fn test_ensure_feature_enabled_preserves_notify_with_codex_notify_but_no_hcom_owner() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "notify = \"other-tool codex-notify\"\n").unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("other-tool codex-notify"),
            "non-hcom notify mentioning codex-notify was removed"
        );
        assert!(content.contains("hooks = true"), "feature flag not set");
    }

    #[test]
    #[serial]
    fn test_ensure_feature_enabled_removes_hcom_notify() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "notify = \"hcom internal codex-notify --name luna\"\n",
        )
        .unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !content.contains("notify"),
            "hcom notify key was not removed"
        );
        assert!(content.contains("hooks = true"), "feature flag not set");
    }

    #[test]
    #[serial]
    fn test_remove_codex_hooks_preserves_feature_flag() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(setup_codex_hooks(false));

        let config_path = get_codex_config_path();
        let before = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            before.contains("hooks = true"),
            "setup did not enable feature flag"
        );

        assert!(remove_codex_hooks());
        let after = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            after.contains("hooks = true"),
            "feature flag should be preserved"
        );
    }

    #[test]
    #[serial]
    fn test_setup_codex_creates_execpolicy() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(setup_codex_hooks(true));

        let rules_file = get_codex_rules_path().join("hcom.rules");
        assert!(rules_file.exists(), "execpolicy rules should be created");
        let content = std::fs::read_to_string(&rules_file).unwrap();
        assert!(content.contains("hcom"));
    }

    #[test]
    #[serial]
    fn test_remove_codex_removes_execpolicy() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(setup_codex_hooks(true));
        let rules_file = get_codex_rules_path().join("hcom.rules");
        assert!(rules_file.exists());

        assert!(remove_codex_hooks());
        assert!(!rules_file.exists(), "execpolicy rules should be removed");
    }

    #[test]
    #[serial]
    fn test_remove_codex_noop_when_no_hooks_json() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(remove_codex_hooks());
    }

    #[test]
    #[serial]
    fn test_codex_feature_enabled_with_fallback() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\ncodex_hooks = true\n").unwrap();

        // Both keys resolve because the selected key is checked first,
        // then the alternate acts as a fallback.
        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::CodexHooks
        ));
        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));

        // Reverse: only hooks key present.
        std::fs::write(&config_path, "[features]\nhooks = true\n").unwrap();
        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::CodexHooks
        ));
    }

    #[test]
    #[serial]
    fn test_ensure_feature_upgrade_cleans_stale_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        // Seed config with the deprecated key, simulating an old hcom install.
        std::fs::write(&config_path, "[features]\ncodex_hooks = true\n").unwrap();

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("hooks = true"), "upgrade should set hooks");
        assert!(
            !content.contains("codex_hooks"),
            "upgrade should remove stale codex_hooks"
        );
    }

    #[test]
    #[serial]
    fn test_ensure_feature_upgrade_cleans_profile_stale_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "profile = \"work\"\n\n[features]\nhooks = true\n\n[profiles.work.features]\ncodex_hooks = true\n",
        )
        .unwrap();

        assert!(!codex_current_feature_enabled());

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("hooks = true"), "upgrade should set hooks");
        assert!(
            !content.contains("codex_hooks"),
            "upgrade should remove stale profile codex_hooks"
        );
        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_ensure_feature_upgrade_cleans_inline_profile_stale_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "profile = \"work\"\nprofiles = { work = { features = { codex_hooks = true } } }\n\n[features]\nhooks = true\n",
        )
        .unwrap();

        assert!(!codex_current_feature_enabled());

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("hooks = true"), "upgrade should set hooks");
        assert!(
            !content.contains("codex_hooks"),
            "upgrade should remove stale inline profile codex_hooks"
        );
        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_requires_selected_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\ncodex_hooks = true\n").unwrap();

        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(!codex_current_feature_enabled());

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();
        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_rejects_mixed_deprecated_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "[features]\nhooks = true\ncodex_hooks = true\n",
        )
        .unwrap();

        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(!codex_current_feature_enabled());

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();
        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_rejects_profile_deprecated_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "profile = \"work\"\n\n[features]\nhooks = true\n\n[profiles.work.features]\ncodex_hooks = true\n",
        )
        .unwrap();

        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(!codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_ignores_inactive_profile_deprecated_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "[features]\nhooks = true\n\n[profiles.work.features]\ncodex_hooks = true\n",
        )
        .unwrap();

        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_rejects_inline_profile_deprecated_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "profile = \"work\"\nprofiles = { work = { features = { codex_hooks = true } } }\n\n[features]\nhooks = true\n",
        )
        .unwrap();

        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(!codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_ensure_feature_downgrade_uses_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\nhooks = true\n").unwrap();

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::CodexHooks).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let doc = content.parse::<DocumentMut>().unwrap();
        let features = doc.get("features").unwrap();
        assert!(
            features.get("codex_hooks").and_then(|v| v.as_bool()) == Some(true),
            "old Codex should use codex_hooks"
        );
        // hooks is the shared flag for all Codex hooks — not just hcom's.
        // hcom must not delete it even when writing for an older Codex.
        assert!(
            features.get("hooks").and_then(|v| v.as_bool()) == Some(true),
            "shared hooks flag should be preserved"
        );
    }

    #[test]
    fn test_codex_hooks_feature_key_version_gate() {
        assert_eq!(
            codex_hooks_feature_key_for_version((0, 128, 0)),
            CodexHooksFeatureKey::CodexHooks
        );
        assert_eq!(
            codex_hooks_feature_key_for_version((0, 129, 0)),
            CodexHooksFeatureKey::Hooks
        );
        assert_eq!(
            parse_codex_cli_version("codex-cli 0.129.0"),
            Some((0, 129, 0))
        );
    }

    // ── regression: legacy "cmd"-format cleanup ─────────────────────────────

    /// Old hcom versions wrote hooks as {"type":"cmd","cmd":"hcom codex-..."}.
    /// On Codex >= 0.129 (CODEX_HOOKS_FEATURE_RENAME_VERSION), try_setup_codex_hooks
    /// must remove those stale entries and replace them with the current format.
    ///
    /// FAILS before the fix: remove_legacy_hcom_cmd_hooks_from_json is defined
    /// but not yet called from try_setup_codex_hooks.
    #[test]
    #[serial]
    fn test_legacy_cmd_hooks_cleaned_on_new_codex() {
        // isolated_test_env sets HCOM_TEST_CODEX_CLI_VERSION = "codex-cli 0.129.0"
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [{
                        "hooks": [{"type": "cmd", "cmd": "hcom codex-userpromptsubmit"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(try_setup_codex_hooks(false).is_ok());
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let hooks_json: Value = serde_json::from_str(&content).unwrap();
        let user_prompt_hooks = hooks_json["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap();
        assert!(
            !user_prompt_hooks
                .iter()
                .any(|hook| hook["type"] == "cmd" && hook.get("cmd").is_some()),
            "legacy cmd-keyed entry must be removed on Codex >= 0.129"
        );
        assert!(
            user_prompt_hooks.iter().any(|hook| {
                hook["type"] == "command"
                    && hook["command"] == build_codex_hook_command("codex-userpromptsubmit")
            }),
            "current command-keyed entry must be present after cleanup"
        );
    }

    // ── regression: context-mode "matcher":"" groups must not block verify ──

    /// context-mode writes groups with "matcher":"" for None-matcher events
    /// (UserPromptSubmit, Stop).  Those groups appear before hcom's no-matcher
    /// groups in the file.  verify_hooks_json_value must not pick the wrong
    /// group and report HookCommandMissing.
    #[test]
    #[serial]
    fn test_context_mode_empty_matcher_does_not_block_verify() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        // Seed with context-mode "matcher":"" groups appearing FIRST for both
        // None-matcher events, matching the live ~/.codex/hooks.json layout.
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "context-mode hook codex userpromptsubmit"}]
                    }],
                    "Stop": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "context-mode hook codex stop"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(
            try_setup_codex_hooks(false).is_ok(),
            "setup must succeed even when another tool owns a \"matcher\":\"\" group for the same event"
        );
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(
            content.contains("context-mode hook codex userpromptsubmit"),
            "third-party hook must be preserved"
        );
        assert!(
            content.contains("codex-userpromptsubmit"),
            "hcom hook must be present"
        );
    }

    #[test]
    #[serial]
    fn remove_codex_hooks_cleans_active_hcom_dir_local_path() {
        let _guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = dir.path().join("workspace");
        let local_dir = workspace.join(".codex");
        std::fs::create_dir_all(local_dir.join("rules")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("HCOM_DIR", workspace.join(".hcom"));
            std::env::remove_var("CODEX_HOME");
        }
        std::fs::write(
            local_dir.join("hooks.json"),
            serde_json::to_string_pretty(&build_expected_hook_json()).unwrap(),
        )
        .unwrap();
        std::fs::write(local_dir.join("rules/hcom.rules"), "allow").unwrap();

        assert!(remove_codex_hooks());
        assert!(!local_dir.join("rules/hcom.rules").exists());
        if local_dir.join("hooks.json").exists() {
            let content = std::fs::read_to_string(local_dir.join("hooks.json")).unwrap();
            assert!(!content.contains("codex-"));
        }
    }
}
