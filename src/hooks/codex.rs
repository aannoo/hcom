//! Codex native hook handlers and settings management.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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
const HCOM_TOOL_NAMES: &[&str] = &["claude", "gemini", "codex", "opencode"];
const CODEX_HOOKS_FEATURE_RENAME_VERSION: (u64, u64, u64) = (0, 129, 0);
type CodexHookHandler = fn(&HcomDb, &HcomContext, &HookPayload) -> HookResult;

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

    let instance_name = common::find_last_bind_marker(effective_path)?;

    family::bind_vanilla_instance(
        db,
        &instance_name,
        Some(session_id).filter(|s| !s.is_empty()),
        Some(effective_path),
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
        updates.insert("transcript_path".into(), Value::String(tp));
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
                if serde_json::to_writer(&mut stdout, &json).is_ok() && stdout.flush().is_ok() {
                    if let Some(ack) = delivery_ack.as_ref() {
                        common::commit_delivery_ack(db, ack);
                    }
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
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
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
        std::process::Command::new("codex")
            .arg("--version")
            .output()
            .ok()
            .and_then(|output| {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                parse_codex_cli_version(&text)
            })
            .map(codex_hooks_feature_key_for_version)
            .unwrap_or(CodexHooksFeatureKey::Hooks)
    })
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
    if let Some(features) = doc.get_mut("features") {
        if let Some(table) = features.as_table_like_mut() {
            table.remove("codex_hooks");
        }
    }
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

fn codex_feature_enabled(config_path: &Path, feature_key: CodexHooksFeatureKey) -> bool {
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
        .and_then(|item| {
            item.get(feature_key.as_str())
                .or_else(|| item.get(feature_key.alternate()))
        })
        .and_then(|item| item.as_bool())
        .unwrap_or(false)
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
        let any_matches = groups.iter().any(|group| {
            let matcher_ok = match matcher {
                Some(expected) => group.get("matcher").and_then(|v| v.as_str()) == Some(*expected),
                None => {
                    group.get("matcher").is_none()
                        || group.get("matcher").and_then(|v| v.as_str()) == Some("")
                }
            };
            matcher_ok
                && group
                    .get("hooks")
                    .and_then(|v| v.as_array())
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("type").and_then(|v| v.as_str()) == Some("command")
                                && hook.get("command").and_then(|v| v.as_str())
                                    == Some(expected_command.as_str())
                        })
                    })
        });
        if !any_matches {
            return Err(VerifyFailReason::HookCommandMissing {
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
                            None => group_matcher.is_none() || group_matcher == Some(""),
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
    #[error("stale hcom hook entry in event '{event}' under unexpected matcher: {matcher:?}")]
    StaleHcomHookEntry {
        event: String,
        matcher: Option<String>,
    },
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
    let feature_key = detect_codex_hooks_feature_key();

    if !config_path.exists() {
        return Err(VerifyFailReason::ConfigPathMissing(config_path));
    }
    if !codex_feature_enabled(&config_path, feature_key) {
        return Err(VerifyFailReason::CodexFeatureDisabled(config_path));
    }
    // No exists() pre-check: verify_hooks_json_at converts NotFound to
    // HooksPathMissing, avoiding a stat-then-open race.
    verify_hooks_json_at(&hooks_path)?;
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

    // Strip deprecated codex_hooks from config.toml. Leave hooks alone
    // (shared flag for all Codex hooks, not just hcom's).
    let config_path = base.join("config.toml");
    if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if let Ok(mut doc) = content.parse::<DocumentMut>() {
                let had_codex_hooks = doc
                    .get("features")
                    .and_then(|f| f.get("codex_hooks"))
                    .is_some();
                if had_codex_hooks {
                    if let Some(features) = doc.get_mut("features") {
                        if let Some(table) = features.as_table_like_mut() {
                            table.remove("codex_hooks");
                        }
                    }
                    ok &= paths::atomic_write(&config_path, &doc.to_string());
                }
            }
        }
    }

    if rules_file.exists() {
        ok &= std::fs::remove_file(&rules_file).is_ok();
    }

    ok
}

/// Remove hcom hooks from Codex config.
///
/// Cleans both the default (~/.codex) and env-var (CODEX_HOME) paths.
pub fn remove_codex_hooks() -> bool {
    let default_dir = dirs::home_dir()
        .map(|h| h.join(".codex"))
        .unwrap_or_default();
    let env_dir = std::env::var("CODEX_HOME")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);

    let default_ok = remove_codex_hooks_from_dir(&default_dir);
    let env_ok = match env_dir {
        Some(ref d) if *d != default_dir => remove_codex_hooks_from_dir(d),
        _ => true,
    };

    default_ok && env_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::isolated_test_env;
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
}
