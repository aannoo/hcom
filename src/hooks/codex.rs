//! Codex native hook handlers and settings management.

use std::collections::{HashMap, HashSet};
use std::io::Write;
#[cfg(not(test))]
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::process::Stdio;
#[cfg(not(test))]
use std::sync::OnceLock;
#[cfg(not(test))]
use std::sync::mpsc;
#[cfg(not(test))]
use std::sync::{Arc, Mutex};
#[cfg(not(test))]
use std::time::Duration;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use toml_edit::{DocumentMut, Item};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::{HookPayload, HookResult, common, family};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::paths;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_LISTENING};

use super::common::SAFE_HCOM_COMMANDS;
use super::runtime::{self, LaunchCtx, LegacyFile, PerRunAdapter, RuntimeInjection};
use anyhow::{Context as _, Result as AnyResult, bail};

const HCOM_TRIGGER: &str = "<hcom>";
// `fork` is its own SessionStart source since Codex 0.155 (earlier releases
// reported forks as `startup`); without it a forked session never binds hooks.
//
// PermissionRequest fires for every tool that asks for approval, so PostToolUse
// must match every tool too, or an approved apply_patch/MCP call would leave the
// row blocked until the next shell call. PreToolUse only feeds status detail.
// A denied or aborted approval skips PostToolUse; Stop, Interrupt, and the next
// PreToolUse clear it instead.
const CODEX_HOOK_COMMANDS: &[(&str, &str, Option<&str>)] = &[
    (
        "SessionStart",
        "codex-sessionstart",
        Some("startup|resume|clear|fork"),
    ),
    ("UserPromptSubmit", "codex-userpromptsubmit", None),
    (
        "PreToolUse",
        "codex-pretooluse",
        Some("Bash|apply_patch|spawn_agent"),
    ),
    ("PermissionRequest", "codex-permissionrequest", None),
    ("PostToolUse", "codex-posttooluse", None),
    ("Stop", "codex-stop", None),
    ("Interrupt", "codex-interrupt", None),
];

pub static PER_RUN: PerRunAdapter = PerRunAdapter {
    prepare: prepare_per_run,
    cleanup_legacy: cleanup_legacy_per_run,
    ensure_permissions: Some(ensure_per_run_permissions),
    managed_value_flags: &[],
    strip_legacy_args: None,
};

fn per_run_home(ctx: &LaunchCtx) -> PathBuf {
    ctx.path_var("CODEX_HOME")
        .unwrap_or_else(|| crate::runtime_env::tool_config_root().join(".codex"))
}

fn parse_override(raw: &str) -> AnyResult<(String, toml::Value)> {
    let (key, value) = raw
        .split_once('=')
        .context("Codex -c override must be key=value")?;
    let key = key.trim();
    let doc: toml::Value = toml::from_str(&format!("value = {}", value.trim()))
        .with_context(|| format!("Invalid Codex -c {key} TOML value"))?;
    Ok((key.to_string(), doc["value"].clone()))
}

fn override_key_path(key: &str) -> AnyResult<Vec<String>> {
    let parsed: toml::Value = toml::from_str(&format!("{key} = 0"))
        .with_context(|| format!("Invalid Codex -c key {key}"))?;
    let mut path = Vec::new();
    let mut current = &parsed;
    while let Some(table) = current.as_table() {
        if table.len() != 1 {
            bail!("Invalid Codex -c key {key}");
        }
        let (segment, next) = table.iter().next().unwrap();
        path.push(segment.clone());
        current = next;
    }
    Ok(path)
}

fn toml_literal(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => serde_json::to_string(s).unwrap(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(n) => n.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(a) => format!(
            "[{}]",
            a.iter().map(toml_literal).collect::<Vec<_>>().join(", ")
        ),
        toml::Value::Table(t) => format!(
            "{{ {} }}",
            t.iter()
                .map(|(k, v)| format!(
                    "{} = {}",
                    serde_json::to_string(k).unwrap(),
                    toml_literal(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn set_override_path(root: &mut toml::Value, path: &[&str], value: toml::Value) -> AnyResult<()> {
    if path.is_empty() {
        *root = value;
        return Ok(());
    }
    let Some(table) = root.as_table_mut() else {
        bail!("Codex -c hooks parent is not a table");
    };
    if path.len() == 1 {
        table.insert(path[0].to_string(), value);
        return Ok(());
    }
    let child = table
        .entry(path[0].to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    set_override_path(child, &path[1..], value)
}

fn merged_per_run_hooks(ctx: &LaunchCtx) -> AnyResult<(Vec<String>, toml::Value)> {
    let mut kept = Vec::new();
    let mut hooks = toml::Value::Table(Default::default());
    let end = ctx
        .args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(ctx.args.len());
    let mut i = 0;
    while i < ctx.args.len() {
        if i >= end {
            kept.extend_from_slice(&ctx.args[i..]);
            break;
        }
        let (raw, consumed) = if ctx.args[i] == "-c" || ctx.args[i] == "--config" {
            if i + 1 >= end {
                bail!("Codex -c is missing its value");
            }
            (Some(ctx.args[i + 1].as_str()), 2)
        } else if let Some(raw) = ctx.args[i]
            .strip_prefix("-c=")
            .or_else(|| ctx.args[i].strip_prefix("--config="))
        {
            (Some(raw), 1)
        } else {
            (None, 1)
        };
        if let Some(raw) = raw {
            let key = raw.split_once('=').map(|(key, _)| key.trim());
            if key == Some("hooks") || key.is_some_and(|key| key.starts_with("hooks.")) {
                let (key, value) = parse_override(raw)?;
                let path = override_key_path(&key)?;
                if path.first().map(String::as_str) != Some("hooks") {
                    bail!("Invalid Codex -c hooks key {key}");
                }
                set_override_path(
                    &mut hooks,
                    &path[1..].iter().map(String::as_str).collect::<Vec<_>>(),
                    value,
                )?;
            } else {
                kept.extend_from_slice(&ctx.args[i..i + consumed]);
            }
        } else {
            kept.push(ctx.args[i].clone());
        }
        i += consumed;
    }
    let expected = build_expected_hook_json();
    let table = hooks
        .as_table_mut()
        .context("Codex -c hooks must be a TOML table")?;
    for (event, group) in expected["hooks"].as_object().unwrap() {
        let entry = table
            .entry(event.clone())
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        let Some(array) = entry.as_array_mut() else {
            bail!("Codex -c hooks.{event} must be an array");
        };
        let hcom_group: toml::Value =
            toml::from_str(&format!("value = {}", json_to_toml_literal(group)))?;
        array.extend(hcom_group["value"].as_array().unwrap().iter().cloned());
    }
    Ok((kept, hooks))
}

fn json_to_toml_literal(value: &Value) -> String {
    match value {
        Value::String(s) => serde_json::to_string(s).unwrap(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(json_to_toml_literal)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(t) => format!(
            "{{ {} }}",
            t.iter()
                .map(|(k, v)| format!(
                    "{} = {}",
                    serde_json::to_string(k).unwrap(),
                    json_to_toml_literal(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Null => "\"\"".to_string(),
    }
}

fn prepare_per_run(ctx: &LaunchCtx) -> AnyResult<RuntimeInjection> {
    let (mut args, mut hooks) = merged_per_run_hooks(ctx)?;
    // Codex merges hooks.state per key across the user and session-flag layers
    // (codex-rs/hooks/src/config_rules.rs), so only hcom's entries and the
    // caller's own -c state belong here. Copying config.toml's state would pin
    // a launch-time snapshot above any trust the user grants mid-session.
    let mut state = match hooks.as_table_mut().unwrap().remove("state") {
        Some(toml::Value::Table(caller_state)) => caller_state,
        Some(_) => bail!("Codex -c hooks.state must be a table"),
        None => Default::default(),
    };
    let declarations: Vec<String> = hooks
        .as_table()
        .unwrap()
        .iter()
        .map(|(event, value)| format!("hooks.{event}={}", toml_literal(value)))
        .collect();
    let declaration_bytes = serde_json::to_vec(&declarations)?;
    let version = codex_cli_version_output_for_hook_trust()
        .map_err(anyhow::Error::msg)
        .context("Cannot check Codex version for hook trust")?;
    let cache_key = runtime::content_digest(&[
        ("version", version.as_bytes()),
        ("declarations", &declaration_bytes),
    ]);
    let cache = runtime::integrations_dir()
        .join("codex")
        .join("cache")
        .join(format!("{cache_key}.json"));
    let entries: Vec<CodexHookTrustEntry> = if cache.exists() {
        serde_json::from_slice(&std::fs::read(&cache)?)
            .with_context(|| format!("Invalid Codex trust cache {}", cache.display()))?
    } else {
        let mut preflight = vec!["-c".to_string(), "features.hooks=true".to_string()];
        for declaration in &declarations {
            preflight.extend(["-c".to_string(), declaration.clone()]);
        }
        let entries =
            fetch_codex_hook_list_with_overrides(&ctx.cwd, &per_run_home(ctx), &preflight)
                .map_err(anyhow::Error::msg)
                .context("Codex hooks/list preflight failed")?;
        let expected: HashSet<String> = CODEX_HOOK_COMMANDS
            .iter()
            .map(|(_, suffix, _)| build_codex_hook_command(suffix))
            .collect();
        let owned: Vec<CodexHookTrustEntry> = entries
            .into_iter()
            .filter(|entry| {
                entry.source.as_deref() == Some("sessionFlags")
                    && entry
                        .command
                        .as_ref()
                        .is_some_and(|command| expected.contains(command))
            })
            .map(|entry| {
                Ok(CodexHookTrustEntry {
                    key: entry.key.context("Codex hcom session hook lacks key")?,
                    command: entry.command.unwrap(),
                    current_hash: entry
                        .current_hash
                        .context("Codex hcom session hook lacks currentHash")?,
                })
            })
            .collect::<AnyResult<_>>()?;
        if owned.len() != CODEX_HOOK_COMMANDS.len() {
            bail!(
                "Codex hooks/list found {} of {} hcom session hooks",
                owned.len(),
                CODEX_HOOK_COMMANDS.len()
            );
        }
        std::fs::create_dir_all(cache.parent().unwrap())?;
        paths::atomic_write_io(&cache, &serde_json::to_string(&owned)?)?;
        owned
    };
    for entry in entries {
        let mut trust = toml::map::Map::new();
        trust.insert(
            "trusted_hash".to_string(),
            toml::Value::String(entry.current_hash),
        );
        state.insert(entry.key, toml::Value::Table(trust));
    }
    let mut injection = vec!["-c".to_string(), "features.hooks=true".to_string()];
    for declaration in declarations {
        injection.extend(["-c".to_string(), declaration]);
    }
    injection.extend([
        "-c".to_string(),
        format!("hooks.state={}", toml_literal(&toml::Value::Table(state))),
    ]);
    runtime::insert_before_separator(&mut args, injection);
    Ok(RuntimeInjection {
        args,
        env: Vec::new(),
    })
}

fn ensure_per_run_permissions(ctx: &LaunchCtx) -> AnyResult<()> {
    let home = per_run_home(ctx);
    let ok = if ctx.auto_approve {
        setup_codex_execpolicy_at(&home)
    } else {
        remove_codex_execpolicy_at(&home)
    };
    if !ok {
        bail!(
            "Cannot sync {}",
            codex_rules_path_at(&home).join("hcom.rules").display()
        );
    }
    Ok(())
}

fn cleanup_legacy_per_run(ctx: &LaunchCtx) -> AnyResult<()> {
    let home = per_run_home(ctx);
    cleanup_codex_hooks_in_dir(&home)
}

/// Remove a legacy install: hcom's handlers in hooks.json, the config.toml
/// `hooks.state` entries that trusted them, a pre-hooks `codex-notify`
/// callback, and the trust metadata file.
/// hcom never declared hooks in config.toml, so its hook tables are left alone.
/// State keys are removed only when they named an hcom handler position or are
/// recorded in the metadata; a user's own hooks in the same file keep their trust.
fn cleanup_codex_hooks_in_dir(home: &Path) -> AnyResult<()> {
    let hooks_path = codex_hooks_path_at(home);
    let config_path = codex_config_path_at(home);
    let metadata_path = home.join(HCOM_HOOK_TRUST_METADATA_FILE);
    let fix_hooks = runtime::FIX_REMOVE_HCOM_HOOKS;
    // A config.toml failure stops cleanup before hooks.json is rewritten (user
    // trust keys must move first), so the fix covers hooks.json too.
    let fix_config = format!(
        "make it valid, writable TOML, then {fix_hooks} in {}",
        hooks_path.display()
    );

    let mut hcom_positions = HashSet::new();
    // User handlers after an hcom one shift left when it is removed; their
    // trust keys follow them so the user's hooks stay trusted.
    let mut moved: HashMap<HandlerPosition, HandlerPosition> = HashMap::new();
    let mut cleaned_hooks = None;
    match std::fs::read_to_string(&hooks_path) {
        Ok(source) => {
            let mut hooks: Value = serde_json::from_str(&source)
                .with_context(|| LegacyFile::read(&hooks_path, fix_hooks))?;
            hcom_positions = handler_positions(&hooks, is_hcom_handler)
                .into_iter()
                .collect();
            if !hcom_positions.is_empty() {
                let user_before = handler_positions(&hooks, |h| !is_hcom_handler(h));
                remove_hcom_hooks_from_json(&mut hooks);
                remove_legacy_hcom_cmd_hooks_from_json(&mut hooks);
                // Removal keeps the remaining handlers in order.
                let user_after = handler_positions(&hooks, |_| true);
                if user_before.len() == user_after.len() {
                    moved = user_before
                        .into_iter()
                        .zip(user_after)
                        .filter(|(old, new)| old != new)
                        .collect();
                }
                cleaned_hooks = Some(hooks);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| LegacyFile::read(&hooks_path, fix_hooks));
        }
    }

    // The metadata only names extra stale keys, and hcom deletes it below, so
    // an unparseable one is treated as empty rather than blocking the launch.
    let mut recorded_keys = HashSet::new();
    match std::fs::read_to_string(&metadata_path) {
        Ok(source) => {
            if let Ok(metadata) = source.parse::<DocumentMut>()
                && let Some(state) = metadata.get("state").and_then(Item::as_table_like)
            {
                recorded_keys.extend(state.iter().map(|(key, _)| key.to_string()));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| LegacyFile::read(&metadata_path, runtime::FIX_DELETE));
        }
    }

    match std::fs::read_to_string(&config_path) {
        Ok(source) => {
            let mut config: DocumentMut = source
                .parse()
                .with_context(|| LegacyFile::read(&config_path, fix_config.clone()))?;
            let mut changed = false;
            if let Some(state) = config
                .get_mut("hooks")
                .and_then(|hooks| hooks.get_mut("state"))
                .and_then(Item::as_table_like_mut)
            {
                let mut stale = Vec::new();
                let mut renames = Vec::new();
                for (key, _) in state.iter() {
                    let Some(position) = hcom_hooks_json_state_position(key, &hooks_path) else {
                        continue;
                    };
                    if hcom_positions.contains(&position) {
                        stale.push(key.to_string());
                    } else if let Some(new) = moved.get(&position) {
                        renames.push((key.to_string(), rekey_state_position(key, new)));
                    } else if recorded_keys.contains(key) {
                        stale.push(key.to_string());
                    }
                }
                for key in &stale {
                    state.remove(key);
                }
                // Take every moved entry out before reinserting: a new key can
                // be another handler's old one.
                let taken: Vec<(String, Item)> = renames
                    .iter()
                    .filter_map(|(old, new)| state.remove(old).map(|item| (new.clone(), item)))
                    .collect();
                for (key, item) in taken {
                    state.insert(&key, item);
                }
                changed = !stale.is_empty() || !renames.is_empty();
            }
            // Pre-hooks hcom installed a `codex-notify` callback; leave unrelated notify alone.
            if config.get("notify").is_some_and(is_hcom_legacy_notify) {
                config.remove("notify");
                changed = true;
            }
            if changed {
                paths::atomic_write_io(&config_path, &config.to_string())
                    .with_context(|| LegacyFile::write(&config_path, fix_config.clone()))?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| LegacyFile::read(&config_path, fix_config.clone()));
        }
    }

    // hooks.json and the metadata go last so a failure above retries next launch.
    if let Some(hooks) = cleaned_hooks {
        if hooks.as_object().is_some_and(|o| o.is_empty()) {
            std::fs::remove_file(&hooks_path)
        } else {
            paths::atomic_write_io(&hooks_path, &serde_json::to_string_pretty(&hooks)?)
        }
        .with_context(|| LegacyFile::write(&hooks_path, fix_hooks))?;
    }
    match std::fs::remove_file(&metadata_path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            // Trust metadata never loads hooks.
            Err(error).with_context(|| LegacyFile {
                still_loads: false,
                ..LegacyFile::write(&metadata_path, runtime::FIX_DELETE)
            })
        }
        _ => Ok(()),
    }
}

/// hcom wrote `notify` as its command prefix (`hcom`, `uvx hcom`, or an hcom
/// executable path) followed by a separate `codex-notify` argument, as an
/// array or one string. Match that shape, not substrings of a user's path.
fn is_hcom_legacy_notify(item: &Item) -> bool {
    let Some(value) = item.as_value() else {
        return false;
    };
    let tokens: Vec<&str> = if let Some(s) = value.as_str() {
        s.split_whitespace().collect()
    } else if let Some(arr) = value.as_array() {
        arr.iter().filter_map(|entry| entry.as_str()).collect()
    } else {
        return false;
    };
    let Some(notify_at) = tokens.iter().position(|t| *t == "codex-notify") else {
        return false;
    };
    tokens[..notify_at].iter().any(|token| {
        let name = token.rsplit(['/', '\\']).next().unwrap_or(token);
        let stem = name
            .strip_suffix(".exe")
            .or_else(|| name.strip_suffix(".py"))
            .unwrap_or(name);
        stem.eq_ignore_ascii_case("hcom")
    })
}

/// `(event_label, group, handler)`: a handler's place in hooks.json, in the
/// form Codex uses for `hooks.state` keys.
type HandlerPosition = (String, usize, usize);

fn is_hcom_handler(handler: &Value) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(is_hcom_codex_command)
        || is_legacy_hcom_codex_cmd_entry(handler)
}

/// Positions of the handlers `select` accepts, in document order.
fn handler_positions(hooks: &Value, select: impl Fn(&Value) -> bool) -> Vec<HandlerPosition> {
    let mut positions = Vec::new();
    let Some(events) = hooks.get("hooks").and_then(Value::as_object) else {
        return positions;
    };
    for (event, groups) in events {
        let label = codex_hook_event_state_label(event);
        for (group_index, group) in groups.as_array().into_iter().flatten().enumerate() {
            let handlers = group.get("hooks").and_then(Value::as_array);
            for (handler_index, handler) in handlers.into_iter().flatten().enumerate() {
                if select(handler) {
                    positions.push((label.to_string(), group_index, handler_index));
                }
            }
        }
    }
    positions
}

/// `<source>:<label>:<group>:<handler>` with the position replaced.
fn rekey_state_position(key: &str, (label, group, handler): &HandlerPosition) -> String {
    let source = key.rsplitn(4, ':').nth(3).unwrap_or_default();
    format!("{source}:{label}:{group}:{handler}")
}

const HCOM_TOOL_NAMES: &[&str] = &[
    "claude",
    "gemini",
    "codex",
    "opencode",
    "antigravity",
    "agy",
];
const HCOM_HOOK_TRUST_METADATA_FILE: &str = "hcom-hook-trust.toml";
#[cfg(not(test))]
const CODEX_APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const CODEX_APP_SERVER_STDERR_LIMIT: usize = 8192;
type CodexHookHandler = fn(&HcomDb, &HcomContext, &HookPayload) -> HookResult;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
struct CodexHookTrustEntry {
    key: String,
    command: String,
    current_hash: String,
}

/// One hook from a `codex app-server hooks/list` response.
///
/// Field names on the wire are camelCase (`HookMetadata` in
/// codex-rs/app-server-protocol/src/protocol/v2/plugin.rs:513-542); the
/// snake_case spellings of the core protocol are accepted too.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexHookListEntry {
    key: Option<String>,
    command: Option<String>,
    source: Option<String>,
    current_hash: Option<String>,
}

fn hook_noop() -> HookResult {
    HookResult::Allow {
        additional_context: None,
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

fn resolve_codex_instance(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> Option<InstanceRow> {
    let session_id = payload.session_id.as_deref().unwrap_or("");
    resolve_instance_codex(db, ctx, session_id)
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
/// Show the delivery in the TUI while also giving it to the model.
/// Codex renders systemMessage as hook output and keeps additionalContext
/// out of the TUI; only additionalContext enters model context.
fn prepare_codex_delivery(db: &HcomDb, instance_name: &str) -> Option<HookResult> {
    common::prepare_pending_messages(db, instance_name).map(|prepared| HookResult::Allow {
        system_message: Some(prepared.formatted.clone()),
        additional_context: Some(prepared.formatted),
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
        None => return hook_noop(),
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

    // Bootstrap is injected at launch time via developer_instructions flag.
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
    // The row holds one detail (the first file); log the rest of a multi-file
    // patch too so collision detection sees every file it writes.
    if payload.tool_name == "apply_patch" {
        for file in family::patch_files(&payload.tool_input).iter().skip(1) {
            let data = serde_json::json!({
                "status": ST_ACTIVE,
                "context": "tool:apply_patch",
                "detail": file,
            });
            if let Err(e) = db.log_event("status", &instance.name, &data) {
                log::log_warn("hooks", "codex.patch_file_event", &format!("{e}"));
            }
        }
    }
    hook_noop()
}

/// Approval blocks set by PermissionRequest or by the PTY's approval scrape.
fn is_approval_block(instance: &InstanceRow) -> bool {
    instance.status == ST_BLOCKED
        && matches!(
            instance.status_context.as_str(),
            "approval" | "pty:approval"
        )
}

/// Observe an approval prompt without deciding it: no output means Codex falls
/// through to its reviewer or the user.
fn handle_permissionrequest(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    // Keep the PTY's context when it saw the prompt first, so its falling edge
    // still owns the release.
    let context = if instance.status == ST_BLOCKED && instance.status_context == "pty:approval" {
        "pty:approval"
    } else {
        "approval"
    };
    let detail = family::extract_tool_detail("codex", &payload.tool_name, &payload.tool_input);
    lifecycle::set_status(
        db,
        &instance.name,
        ST_BLOCKED,
        context,
        lifecycle::StatusUpdate {
            detail: &detail,
            tool_name: &payload.tool_name,
            ..Default::default()
        },
    );
    hook_noop()
}

fn handle_posttooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    if is_approval_block(&instance) {
        lifecycle::set_status(
            db,
            &instance.name,
            ST_ACTIVE,
            &format!("approved:{}", payload.tool_name),
            lifecycle::StatusUpdate {
                tool_name: &payload.tool_name,
                ..Default::default()
            },
        );
    }

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

/// Esc (or an aborted approval) ends the turn without Stop.
fn handle_interrupt(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    lifecycle::set_status(
        db,
        &instance.name,
        ST_LISTENING,
        "interrupted",
        Default::default(),
    );
    common::notify_hook_instance_with_db(db, &instance.name);
    hook_noop()
}

fn get_codex_handler(hook_name: &str) -> Option<CodexHookHandler> {
    match hook_name {
        "codex-sessionstart" => Some(handle_sessionstart),
        "codex-userpromptsubmit" => Some(handle_userpromptsubmit),
        "codex-pretooluse" => Some(handle_pretooluse),
        "codex-permissionrequest" => Some(handle_permissionrequest),
        "codex-posttooluse" => Some(handle_posttooluse),
        "codex-stop" => Some(handle_stop),
        "codex-interrupt" => Some(handle_interrupt),
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
        HookResult::Block { reason, .. } => {
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
#[cfg(test)]
fn get_codex_config_path() -> PathBuf {
    codex_config_path_at(&codex_config_dir())
}

/// Get path to Codex hooks.json.
#[cfg(test)]
fn get_codex_hooks_path() -> PathBuf {
    codex_hooks_path_at(&codex_config_dir())
}

fn codex_config_path_at(codex_home: &Path) -> PathBuf {
    codex_home.join("config.toml")
}

fn codex_hooks_path_at(codex_home: &Path) -> PathBuf {
    codex_home.join("hooks.json")
}

fn codex_rules_path_at(codex_home: &Path) -> PathBuf {
    codex_home.join("rules")
}

/// Strip a Windows verbatim prefix and collapse `.`/`..` components.
///
/// Purely lexical, so it works on paths that do not exist.
fn lexically_normalized(path: &Path) -> PathBuf {
    use std::path::Component;

    let text = path.to_string_lossy();
    let plain = text
        .strip_prefix(r"\\?\UNC\")
        .map(|unc| format!(r"\\{unc}"))
        .or_else(|| text.strip_prefix(r"\\?\").map(str::to_string));
    let plain = plain.map(PathBuf::from);
    let path = plain.as_deref().unwrap_or(path);

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Whether two paths name the same file, without requiring either to exist.
///
/// Codex passes hook source paths through `AbsolutePathBuf::from_absolute_path`
/// (codex-rs/utils/absolute-path/src/lib.rs:58), which absolutizes lexically but
/// does not resolve symlinks, so a `sourcePath` from Codex can differ from
/// hcom's own `codex_hooks_path_at()` by a `.`/`..` component, a verbatim
/// Windows prefix, or by one side having been canonicalized. Compare lexically
/// first and only then pay for canonicalization.
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if a == b || lexically_normalized(a) == lexically_normalized(b) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The `(event_label, group, handler)` a `hooks.state` key names inside hcom's
/// hooks.json, or None when the key names another source or event.
fn hcom_hooks_json_state_position(key: &str, hooks_path: &Path) -> Option<(String, usize, usize)> {
    let mut parts = key.rsplitn(4, ':');
    let (Some(handler_index), Some(group_index), Some(event_label), Some(key_source)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let handler_index = handler_index.parse::<usize>().ok()?;
    let group_index = group_index.parse::<usize>().ok()?;
    if !CODEX_HOOK_COMMANDS
        .iter()
        .any(|(event, _, _)| codex_hook_event_state_label(event) == event_label)
    {
        return None;
    }
    paths_equivalent(Path::new(key_source), hooks_path)
        .then(|| (event_label.to_string(), group_index, handler_index))
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

/// hcom's hook commands are `<hcom prefix> codex-<event>`. Matching the last
/// word alone would also claim a user's own `/usr/local/bin/codex-stop`.
fn is_hcom_codex_command(command: &str) -> bool {
    let mut words = command.split_whitespace().rev();
    let Some(last) = words.next() else {
        return false;
    };
    CODEX_HOOK_COMMANDS
        .iter()
        .any(|(_, suffix, _)| last == *suffix)
        && words.any(|word| word.contains("hcom"))
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
        && hook
            .get("cmd")
            .and_then(|v| v.as_str())
            .is_some_and(is_hcom_codex_command)
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
        "Interrupt" => "interrupt",
        _ => "unknown",
    }
}

/// Read one string field, accepting the camelCase wire spelling and the
/// snake_case spelling of the core protocol.
fn hook_list_str_field<'a>(hook: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    hook.get(camel)
        .or_else(|| hook.get(snake))
        .and_then(|v| v.as_str())
}

fn parse_codex_hook_list_entries(value: &Value) -> Result<Vec<CodexHookListEntry>, String> {
    let hooks = value
        .pointer("/result/data/0/hooks")
        .or_else(|| value.pointer("/data/0/hooks"))
        .or_else(|| value.get("hooks"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| "codex hooks/list response did not contain hooks".to_string())?;

    Ok(hooks
        .iter()
        .map(|hook| CodexHookListEntry {
            key: hook_list_str_field(hook, "key", "key").map(str::to_string),
            command: hook_list_str_field(hook, "command", "command").map(str::to_string),
            source: hook_list_str_field(hook, "source", "source").map(str::to_string),
            current_hash: hook_list_str_field(hook, "currentHash", "current_hash")
                .map(str::to_string),
        })
        .collect())
}

/// `codex` for the preflight. On Windows the npm `codex.cmd` shim runs through
/// cmd.exe, which would split the `startup|resume|…` matcher at each `|`, so
/// call the Node entrypoint directly like interactive launches do.
#[cfg(not(test))]
fn codex_app_server_command() -> std::process::Command {
    #[cfg(windows)]
    if let Some(resolved) = crate::terminal::which_bin("codex")
        && let Some((node, prefix)) =
            crate::terminal::resolve_windows_tool_launcher("codex", &resolved)
    {
        let mut command = std::process::Command::new(node);
        command.args(prefix);
        return command;
    }
    crate::terminal::executable_command("codex")
}

fn fetch_codex_hook_list_with_overrides(
    cwd: &Path,
    codex_home: &Path,
    overrides: &[String],
) -> Result<Vec<CodexHookListEntry>, String> {
    #[cfg(test)]
    {
        let _ = (cwd, codex_home, overrides);
        if let Ok(value) = std::env::var("HCOM_TEST_CODEX_HOOKS_LIST_JSON") {
            if value == "__fail__" {
                return Err("test hook list failure".to_string());
            }
            let json: Value = serde_json::from_str(&value).map_err(|e| e.to_string())?;
            return parse_codex_hook_list_entries(&json);
        }
        Err("test hook list fixture missing".to_string())
    }

    #[cfg(not(test))]
    {
        let mut child = codex_app_server_command()
            .args(overrides)
            .args(["app-server", "--listen", "stdio://"])
            .env("CODEX_HOME", codex_home)
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
        parse_codex_hook_list_entries(&response?)
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

fn setup_codex_execpolicy_at(codex_home: &Path) -> bool {
    let rules_dir = codex_rules_path_at(codex_home);
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

fn remove_codex_execpolicy_at(codex_home: &Path) -> bool {
    let rules_file = codex_rules_path_at(codex_home).join("hcom.rules");
    if rules_file.exists() {
        std::fs::remove_file(&rules_file).is_ok()
    } else {
        true
    }
}

fn remove_codex_hooks_from_dir(base: &std::path::Path) -> bool {
    let rules_file = base.join("rules").join("hcom.rules");
    // A file that doesn't parse is the user's to fix; cleanup leaves it
    // untouched and the failure is reported.
    let mut ok = match cleanup_codex_hooks_in_dir(base) {
        Ok(()) => true,
        Err(error) => {
            crate::log::log_warn("codex", "codex.hooks_cleanup_failed", &format!("{error:#}"));
            false
        }
    };
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
    use std::collections::HashMap;

    fn per_run_ctx(args: &[&str], home: &Path) -> LaunchCtx {
        LaunchCtx {
            tool: crate::tool::Tool::Codex,
            env: HashMap::from([(
                "CODEX_HOME".to_string(),
                home.to_string_lossy().into_owned(),
            )]),
            cwd: home.to_path_buf(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            auto_approve: false,
        }
    }

    #[test]
    fn per_run_merges_caller_hooks_in_flag_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = per_run_ctx(
            &[
                "-c",
                "hooks={SessionStart=[{hooks=[{type='command',command='first'}]}]}",
                "-c",
                "hooks.SessionStart=[{hooks=[{type='command',command='second'}]}]",
            ],
            dir.path(),
        );
        let (_, hooks) = merged_per_run_hooks(&first).unwrap();
        let groups = hooks["SessionStart"].as_array().unwrap();
        assert_eq!(groups[0]["hooks"][0]["command"].as_str(), Some("second"));
        assert_eq!(
            groups[1]["hooks"][0]["command"].as_str(),
            Some(build_codex_hook_command("codex-sessionstart").as_str())
        );

        let reversed = per_run_ctx(
            &[
                "-c",
                "hooks.SessionStart=[{hooks=[{type='command',command='first'}]}]",
                "-c",
                "hooks={SessionStart=[{hooks=[{type='command',command='second'}]}]}",
            ],
            dir.path(),
        );
        let (_, hooks) = merged_per_run_hooks(&reversed).unwrap();
        assert_eq!(
            hooks["SessionStart"][0]["hooks"][0]["command"].as_str(),
            Some("second")
        );
    }

    #[test]
    fn per_run_rejects_malformed_caller_override() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = per_run_ctx(&["-c", "hooks.SessionStart=[oops"], dir.path());
        assert!(merged_per_run_hooks(&ctx).is_err());
    }

    #[test]
    #[serial]
    fn failed_per_run_preparation_leaves_legacy_hooks_untouched() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let codex_home = tempfile::tempdir().unwrap();
        let hooks_path = codex_home.path().join("hooks.json");
        let legacy = serde_json::to_vec(&build_expected_hook_json()).unwrap();
        std::fs::write(&hooks_path, &legacy).unwrap();

        let ctx = per_run_ctx(&["-c", "hooks.SessionStart=[oops"], codex_home.path());
        assert!(crate::hooks::runtime::plan(&PER_RUN, &ctx).is_err());
        assert_eq!(std::fs::read(&hooks_path).unwrap(), legacy);
    }

    #[test]
    fn per_run_keeps_other_overrides_and_quoted_state_keys() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = per_run_ctx(
            &[
                "-c",
                "model=gpt-5",
                "-c",
                "hooks.state.\"config.toml:stop:0:0\"={trusted_hash='old'}",
            ],
            dir.path(),
        );
        let (kept, hooks) = merged_per_run_hooks(&ctx).unwrap();
        assert_eq!(kept, vec!["-c", "model=gpt-5"]);
        assert_eq!(
            hooks["state"]["config.toml:stop:0:0"]["trusted_hash"].as_str(),
            Some("old")
        );
    }

    #[test]
    fn per_run_cleanup_preserves_foreign_hooks_and_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "{broken").unwrap();
        assert!(cleanup_codex_hooks_in_dir(dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        let mut hooks = build_expected_hook_json();
        hooks["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "hooks": [{"type":"command","command":"user-stop"}]
            }));
        std::fs::write(&path, serde_json::to_string(&hooks).unwrap()).unwrap();
        cleanup_codex_hooks_in_dir(dir.path()).unwrap();
        let cleaned: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            cleaned["hooks"]["Stop"][0]["hooks"][0]["command"],
            "user-stop"
        );
    }

    #[test]
    fn per_run_cleanup_removes_only_hcom_legacy_notify() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        for (notify, removed) in [
            (
                "notify = \"hcom internal codex-notify --name luna\"\n",
                true,
            ),
            (
                "notify = [\"hcom\", \"internal\", \"codex-notify\"]\n",
                true,
            ),
            ("notify = \"some-other-notify-tool\"\n", false),
            ("notify = \"other-tool codex-notify\"\n", false),
            ("notify = [\"uvx\", \"hcom\", \"codex-notify\"]\n", true),
            ("notify = [\"C:/dev/hcom.exe\", \"codex-notify\"]\n", true),
            (
                "notify = [\"/home/alice/hcom-tools/codex-notify.sh\"]\n",
                false,
            ),
            ("notify = [\"my-hcom-wrapper\", \"codex-notify\"]\n", false),
        ] {
            std::fs::write(&config_path, format!("model = 'gpt-5'\n{notify}")).unwrap();
            cleanup_codex_hooks_in_dir(dir.path()).unwrap();
            let source = std::fs::read_to_string(&config_path).unwrap();
            assert_eq!(!source.contains("notify"), removed, "{notify}: {source}");
            assert!(source.contains("model = 'gpt-5'"), "{source}");
        }
    }

    #[test]
    fn per_run_cleanup_discards_malformed_trust_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let metadata_path = dir.path().join(HCOM_HOOK_TRUST_METADATA_FILE);
        std::fs::write(&metadata_path, "[broken").unwrap();
        cleanup_codex_hooks_in_dir(dir.path()).unwrap();
        assert!(!metadata_path.exists());
    }

    #[test]
    fn per_run_cleanup_removes_only_hcom_trust_state() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &hooks_path,
            serde_json::json!({"hooks": {"Stop": [
                {"hooks": [{"type": "command", "command": "user-stop"}]},
                {"hooks": [{"type": "command", "command": "hcom codex-stop"}]},
            ]}})
            .to_string(),
        )
        .unwrap();
        let key = |group: usize| format!("{}:stop:{group}:0", hooks_path.display());
        std::fs::write(
            &config_path,
            format!(
                "# keep me\nmodel = 'gpt-5'\n\n[hooks.state]\n'foreign:stop:0:0' = {{ trusted_hash = 'keep' }}\n'{}' = {{ trusted_hash = 'user' }}\n'{}' = {{ trusted_hash = 'hcom' }}\n",
                key(0),
                key(1)
            ),
        )
        .unwrap();

        cleanup_codex_hooks_in_dir(dir.path()).unwrap();
        let source = std::fs::read_to_string(&config_path).unwrap();
        assert!(source.starts_with("# keep me\n"), "{source}");
        let config: toml::Value = toml::from_str(&source).unwrap();
        let state = &config["hooks"]["state"];
        assert_eq!(
            state["foreign:stop:0:0"]["trusted_hash"].as_str(),
            Some("keep")
        );
        assert_eq!(state[&key(0)]["trusted_hash"].as_str(), Some("user"));
        assert!(state.get(key(1)).is_none());
        let hooks: Value = serde_json::from_slice(&std::fs::read(&hooks_path).unwrap()).unwrap();
        assert_eq!(hooks["hooks"]["Stop"].as_array().unwrap().len(), 1);

        // Once migrated, later launches leave the user's trust alone.
        cleanup_codex_hooks_in_dir(dir.path()).unwrap();
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            config["hooks"]["state"][&key(0)]["trusted_hash"].as_str(),
            Some("user")
        );
    }

    #[test]
    fn per_run_cleanup_moves_trust_of_user_hooks_that_shift() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &hooks_path,
            serde_json::json!({"hooks": {"Stop": [
                {"hooks": [{"type": "command", "command": "hcom codex-stop"}]},
                {"hooks": [
                    {"type": "command", "command": "user-a"},
                    {"type": "command", "command": "hcom codex-stop"},
                    {"type": "command", "command": "user-b"},
                ]},
            ]}})
            .to_string(),
        )
        .unwrap();
        let key = |group: usize, handler: usize| {
            format!("{}:stop:{group}:{handler}", hooks_path.display())
        };
        std::fs::write(
            &config_path,
            format!(
                "[hooks.state]\n'{}' = {{ trusted_hash = 'hcom0' }}\n'{}' = {{ trusted_hash = 'a' }}\n'{}' = {{ trusted_hash = 'hcom1' }}\n'{}' = {{ trusted_hash = 'b' }}\n",
                key(0, 0),
                key(1, 0),
                key(1, 1),
                key(1, 2)
            ),
        )
        .unwrap();

        cleanup_codex_hooks_in_dir(dir.path()).unwrap();
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let state = config["hooks"]["state"].as_table().unwrap();
        assert_eq!(state.len(), 2, "{state:?}");
        assert_eq!(state[&key(0, 0)]["trusted_hash"].as_str(), Some("a"));
        assert_eq!(state[&key(0, 1)]["trusted_hash"].as_str(), Some("b"));
    }

    #[test]
    fn hcom_command_match_requires_hcom_prefix() {
        assert!(is_hcom_codex_command("hcom codex-stop"));
        assert!(is_hcom_codex_command("/old/bin/hcom codex-stop"));
        assert!(is_hcom_codex_command("uvx hcom codex-stop"));
        assert!(!is_hcom_codex_command("/usr/local/bin/codex-stop"));
        assert!(!is_hcom_codex_command("hcom codex-stop --extra"));
    }

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
    fn test_paths_equivalent_handles_dot_components() {
        assert!(paths_equivalent(
            Path::new("/home/u/.codex/./hooks.json"),
            Path::new("/home/u/.codex/hooks.json")
        ));
        assert!(paths_equivalent(
            Path::new("/home/u/other/../.codex/hooks.json"),
            Path::new("/home/u/.codex/hooks.json")
        ));
        assert!(!paths_equivalent(
            Path::new("/home/u/.codex/hooks.json"),
            Path::new("/repo/.codex/hooks.json")
        ));
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
                            {"type": "command", "command": "/old/bin/hcom codex-posttooluse"}
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
    fn test_remove_codex_noop_when_no_hooks_json() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(remove_codex_hooks());
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
