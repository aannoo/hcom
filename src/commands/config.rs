//! `hcom config` command — view and edit configuration.
//!
//!
//! Supports: show all, get/set single key, --json, --edit, --reset,
//! per-instance config (-i), terminal preset management.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::db::DEV_ROOT_KV_KEY;
use crate::db::HcomDb;
use crate::identity;
use crate::instances;
use crate::launcher::{LaunchTool, validate_tool_args};
use crate::shared::CommandContext;

/// Parsed arguments for `hcom config`.
///
/// Note: multi-word or dash-prefixed values should be quoted:
///   `hcom config codex_args "--model o3"`
///   `hcom config tag "my tag"`
#[derive(clap::Parser, Debug)]
#[command(name = "config", about = "View and edit configuration")]
pub struct ConfigArgs {
    /// Config key (e.g., "tag", "terminal", "HCOM_TIMEOUT")
    pub key: Option<String>,
    /// Value to set (quote multi-word or dash-prefixed values)
    #[arg(allow_hyphen_values = true)]
    pub value: Option<String>,
    /// Additional arguments for `config bootstrap show/render`.
    pub extra: Vec<String>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Open config in editor
    #[arg(long)]
    pub edit: bool,
    /// Unset value (back to the default)
    #[arg(long)]
    pub unset: bool,
    /// Reset config
    #[arg(long)]
    pub reset: bool,
    /// Show key description
    #[arg(long)]
    pub info: bool,
    /// Terminal setup mode
    #[arg(long)]
    pub setup: bool,
    /// Per-instance config
    #[arg(short = 'i')]
    pub instance: Option<String>,
}

/// Instance-level config keys.
const INSTANCE_KEYS: &[(&str, &str)] = &[
    ("tag", "Instance-specific tag (changes display name)"),
    ("timeout", "Instance-specific idle timeout in seconds"),
    ("hints", "Instance-specific hints (injected with messages)"),
    (
        "subagent_timeout",
        "Instance-specific subagent timeout in seconds",
    ),
];

// ── Config File Operations ───────────────────────────────────────────────

fn config_path() -> PathBuf {
    crate::paths::hcom_dir().join("config.toml")
}

/// Set a config key in config.toml (preserving comments via toml_edit).
/// An empty value is a value: "" for text, false for a switch.
pub fn config_set(key: &str, value: &str) -> Result<(), String> {
    config_set_at_path(&config_path(), key, Some(value))
}

/// Remove a config key from config.toml, so it falls back to its default.
pub fn config_unset(key: &str) -> Result<(), String> {
    config_set_at_path(&config_path(), key, None)
}

fn launch_tool_for_args_field(field_name: &str) -> Option<LaunchTool> {
    // Derive the field → tool map from the spec's `args_env` (the single source
    // of truth, e.g. `HCOM_CLAUDE_ARGS` → `claude_args`) so a new tool's config
    // args are validated as soon as its spec declares `args_env` — no parallel
    // match to keep in sync. See `drift_released_tools_with_args_env_merge_...`.
    let spec = crate::integration_spec::ALL.iter().find(|s| {
        s.launch.args_env.is_some_and(|env| {
            env.strip_prefix("HCOM_")
                .map(str::to_ascii_lowercase)
                .as_deref()
                == Some(field_name)
        })
    })?;
    LaunchTool::from_str(spec.name).ok()
}

fn validate_config_args(field_name: &str, value: &str) -> Result<(), String> {
    let Some(tool) = launch_tool_for_args_field(field_name) else {
        return Ok(());
    };
    if value.is_empty() {
        return Ok(());
    }

    let tokens = crate::tools::args_common::shell_split(value, cfg!(windows))
        .map_err(|e| format!("invalid {field_name} from config: {e}"))?;
    let errors = validate_tool_args(&tool, &tokens);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "invalid {field_name} from config: {}",
            errors.join("; ")
        ))
    }
}

fn config_set_at_path(path: &Path, key: &str, value: Option<&str>) -> Result<(), String> {
    let field =
        crate::config::user_field(key).ok_or_else(|| format!("Unknown config key '{key}'"))?;
    if let Some(value) = value {
        validate_config_args(field.name, value)?;
    }

    let new_value = value
        .map(|v| crate::config::field_toml_value(field, v))
        .transpose()?;
    edit_config_at_path(path, |doc| {
        crate::config::set_doc_path(doc, field.toml, new_value);
        // Refuse what the loader would reject and replace with the default.
        match crate::config::file_field_error(&doc.to_string(), field.name) {
            Some(err) => Err(err),
            None => Ok(true),
        }
    })
}

/// Read-modify-write config.toml under its lock. `edit` returns whether to
/// write the document back.
fn edit_config_at_path(
    path: &Path,
    edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<bool, String>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config directory: {e}"))?;
    }
    // Lock a stable sibling: the config itself is replaced by atomic rename.
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("Failed to open config lock: {e}"))?;
    crate::sys::fs::lock_exclusive(&lock_file)
        .map_err(|e| format!("Failed to lock config: {e}"))?;

    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("Failed to parse config.toml: {e}"))?;
    if !edit(&mut doc)? {
        return Ok(());
    }
    crate::config::write_config_toml_path(path, &doc.to_string())
        .map_err(|e| format!("Failed to write config.toml: {e}"))
}

/// One-off cleanup of the 0.7.29-0.7.30 `launch.<tool>.delivery` keys (see
/// `UNPINNED_FIELDS` in config.rs). They are already ignored; this only drops
/// the dead lines. Run from `hcom config` and `hcom status`, which are rare,
/// not from every config load. Remove once those versions are long gone.
pub(crate) fn drop_old_delivery_keys() {
    drop_old_delivery_keys_at(&config_path());
}

fn drop_old_delivery_keys_at(path: &Path) {
    const OLD_KEYS: [&str; 2] = ["launch.claude.delivery", "launch.copilot.delivery"];
    // Unlocked check on the text: almost always nothing to do. Also skips a
    // missing file (never created) and `idle_delivery`/`rpc_delivery`.
    let stale = std::fs::read_to_string(path).is_ok_and(|content| {
        content
            .lines()
            .any(|line| line.trim_start().starts_with("delivery"))
    });
    if !stale {
        return;
    }
    let result = edit_config_at_path(path, |doc| {
        let before = doc.to_string();
        for key in OLD_KEYS {
            crate::config::set_doc_path(doc, key, None);
        }
        Ok(doc.to_string() != before)
    });
    if let Err(e) = result {
        crate::log::log_warn("config", "config.old_delivery_keys_failed", &e);
    }
}

/// Set a key the way `hcom config <key> <value>` does, including side effects.
/// Shared by the CLI and remote (relay) config set.
pub fn config_set_and_apply(key: &str, value: &str) -> Result<(), String> {
    config_set(key, value)?;
    if crate::config::user_field(key).is_some_and(|f| f.name == "auto_approve")
        && !update_auto_approve_permissions()
    {
        return Err("failed to update auto-approve permissions".into());
    }
    Ok(())
}

/// Get a config value and its source: env var, config.toml, or default.
/// Relay fields skip the env var: the loader reads them from config.toml only.
/// Keys that are not config fields (e.g. HCOM_DIR) are read from env only.
pub fn config_get(key: &str) -> (String, &'static str) {
    let Some(field) = crate::config::user_field(key) else {
        return match std::env::var(key) {
            Ok(val) => (val, "env"),
            Err(_) => (String::new(), "default"),
        };
    };
    if !field.is_relay()
        && let Ok(val) = std::env::var(field.env)
    {
        return (val, "env");
    }
    let (values, _) = crate::config::read_toml_values(&config_path());
    if let Some(val) = values.get(field.name) {
        return (val.as_string(), "toml");
    }
    let default = crate::config::HcomConfig::default()
        .get_field(field.name)
        .unwrap_or_default();
    let default = match field.kind {
        crate::config::FieldKind::Bool => (default == "1").to_string(),
        _ => default,
    };
    (default, "default")
}

fn config_dev_root(
    db: &HcomDb,
    value: Option<&str>,
    unset: bool,
    json_mode: bool,
    wants_info: bool,
) -> i32 {
    if wants_info {
        println!(
            "dev_root - Persistent dev-worktree fallback for binary re-exec\n\
             \n\
             Stored in kv key `{}` under the active HCOM_DIR.\n\
             Router precedence is:\n\
               1. HCOM_DEV_ROOT env\n\
               2. kv dev_root fallback\n\
             \n\
             Examples:\n\
               hcom config dev_root /path/to/worktree\n\
               hcom config dev_root\n\
               hcom config dev_root --unset",
            DEV_ROOT_KV_KEY
        );
        return 0;
    }

    if unset {
        match db.kv_set(DEV_ROOT_KV_KEY, None) {
            Ok(()) => {
                println!("Unset dev_root");
                return 0;
            }
            Err(e) => {
                eprintln!("Error: Failed to unset dev_root: {e}");
                return 1;
            }
        }
    }

    if let Some(value) = value {
        match db.kv_set(DEV_ROOT_KV_KEY, Some(value)) {
            Ok(()) => {
                println!("Set dev_root = {value}");
                return 0;
            }
            Err(e) => {
                eprintln!("Error: Failed to set dev_root: {e}");
                return 1;
            }
        }
    }

    match db.kv_get(DEV_ROOT_KV_KEY) {
        Ok(Some(value)) => {
            if json_mode {
                let mut m = serde_json::Map::new();
                m.insert("dev_root".into(), serde_json::Value::String(value));
                println!("{}", serde_json::Value::Object(m));
            } else {
                println!("{value}");
            }
            0
        }
        Ok(None) => {
            if json_mode {
                let mut m = serde_json::Map::new();
                m.insert("dev_root".into(), serde_json::Value::Null);
                println!("{}", serde_json::Value::Object(m));
            } else {
                println!("dev_root: (not set)");
            }
            0
        }
        Err(e) => {
            eprintln!("Error: Failed to read dev_root: {e}");
            1
        }
    }
}

// ── Instance Config ──────────────────────────────────────────────────────

fn unique_instance_prefix(db: &HcomDb, name: &str) -> Result<Option<String>, String> {
    let escaped = name
        .replace('!', "!!")
        .replace('%', "!%")
        .replace('_', "!_");
    let mut stmt = db
        .conn()
        .prepare("SELECT name FROM instances WHERE name LIKE ? ESCAPE '!' ORDER BY name")
        .map_err(|e| e.to_string())?;
    let candidates = stmt
        .query_map([format!("{escaped}%")], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    match candidates.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.clone())),
        _ => Err(format!(
            "Ambiguous agent prefix '{name}': {}",
            candidates.join(", ")
        )),
    }
}

/// Handle instance-level config: `hcom config -i <name> [key] [value]`
fn config_instance(
    db: &HcomDb,
    instance_arg: &str,
    args: &[String],
    ctx: Option<&CommandContext>,
    json_mode: bool,
) -> i32 {
    // Resolve "self" to current instance
    let name = if instance_arg == "self" {
        if let Some(id) = ctx.and_then(|c| c.identity.as_ref()) {
            id.name.clone()
        } else {
            eprintln!("Error: Cannot resolve 'self' — no active identity");
            return 1;
        }
    } else {
        identity::resolve_display_name(db, instance_arg).unwrap_or_else(|| instance_arg.to_string())
    };

    // Verify instance exists
    let instance = match db.get_instance_full(&name) {
        Ok(Some(inst)) => inst,
        Ok(None) => {
            // Try prefix match
            match unique_instance_prefix(db, &name) {
                Ok(Some(matched)) => match db.get_instance_full(&matched) {
                    Ok(Some(inst)) => inst,
                    _ => {
                        eprintln!("Error: {}", identity::describe_missing_agent(db, &name));
                        return 1;
                    }
                },
                Ok(None) => {
                    eprintln!("Error: {}", identity::describe_missing_agent(db, &name));
                    return 1;
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    return 1;
                }
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let inst_name = &instance.name;

    // No key: show instance settings
    if args.is_empty() {
        let full_name = crate::identity::get_full_name(&instance);
        let config = build_instance_config_json(&instance, &full_name);
        println!("{}", render_config_instance_get(&config, None, json_mode));
        return 0;
    }

    let key = args[0].as_str();
    // Join all remaining args with spaces
    let value_joined = if args.len() > 1 {
        Some(
            args[1..]
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        )
    } else {
        None
    };
    let value = value_joined.as_deref();

    // Validate key
    if !INSTANCE_KEYS.iter().any(|(k, _)| *k == key) {
        eprintln!("Error: Unknown instance config key '{key}'");
        eprintln!(
            "Valid keys: {}",
            INSTANCE_KEYS
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return 1;
    }

    // No value: show current
    let Some(value) = value else {
        return match config_instance_get(db, inst_name, Some(key)) {
            Ok(current) => {
                println!(
                    "{}",
                    render_config_instance_get(&current, Some(key), json_mode)
                );
                0
            }
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        };
    };

    if let Err(e) = config_instance_set(db, inst_name, key, value) {
        eprintln!("Error: {e}");
        return 1;
    }
    println!(
        "{}",
        render_config_instance_set_feedback(inst_name, key, value)
    );

    // C4 fix: push config changes to relay
    crate::relay::spawn_background_push();

    0
}

pub fn render_config_instance_get(value: &Value, key: Option<&str>, json_mode: bool) -> String {
    if json_mode {
        return serde_json::to_string(value).unwrap_or_default();
    }

    match key {
        None => {
            // Prefer full_name for the human header; fall back to name for
            // backwards compatibility with older payloads that only had `name`.
            let display = value
                .get("full_name")
                .and_then(|v| v.as_str())
                .or_else(|| value.get("name").and_then(|v| v.as_str()))
                .unwrap_or("");
            let tag = value.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            let timeout = value
                .get("timeout")
                .and_then(|v| v.as_i64())
                .unwrap_or(86400);
            let hints = value.get("hints").and_then(|v| v.as_str()).unwrap_or("");
            let subagent_timeout = value
                .get("subagent_timeout")
                .and_then(|v| v.as_i64())
                .map(|t| format!("{t}s"))
                .unwrap_or_else(|| "(default)".to_string());
            format!(
                "Agent: {display}\n  tag: {}\n  timeout: {timeout}s\n  hints: {}\n  subagent_timeout: {subagent_timeout}",
                if tag.is_empty() { "(none)" } else { tag },
                if hints.is_empty() { "(none)" } else { hints },
            )
        }
        Some(_) => value
            .get("value")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .unwrap_or_default(),
    }
}

pub fn render_config_instance_set_feedback(instance_name: &str, key: &str, value: &str) -> String {
    match key {
        "tag" => {
            if value.is_empty() {
                format!("Cleared tag for {instance_name}")
            } else {
                format!("Set tag for {instance_name}: {value}")
            }
        }
        "timeout" => {
            if value.is_empty() || value.eq_ignore_ascii_case("default") {
                format!("Reset timeout for {instance_name}")
            } else {
                format!("Set timeout for {instance_name}: {value}s")
            }
        }
        "hints" => {
            if value.is_empty() {
                format!("Cleared hints for {instance_name}")
            } else {
                format!("Set hints for {instance_name}")
            }
        }
        "subagent_timeout" => {
            if value.is_empty() || value.eq_ignore_ascii_case("default") {
                format!("Cleared subagent_timeout for {instance_name}")
            } else {
                format!("Set subagent_timeout for {instance_name}: {value}s")
            }
        }
        _ => format!("Updated {key} for {instance_name}"),
    }
}

/// Build the canonical JSON shape returned by both local and remote no-key
/// instance config reads. Stable contract for scripts consuming
/// `hcom config -i <name> --json`:
///   `name`            — base name
///   `full_name`       — tag-prefixed display name
///   `tag` / `hints`   — null when unset (not "")
///   `timeout`         — null when unset (server-side default applies elsewhere)
///   `subagent_timeout`— null when unset
fn build_instance_config_json(
    instance: &crate::db::InstanceRow,
    full_name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "name": instance.name,
        "full_name": full_name,
        "tag": instance.tag.as_deref().filter(|s| !s.is_empty()),
        "timeout": instance.wait_timeout,
        "hints": instance.hints.as_deref().filter(|s| !s.is_empty()),
        "subagent_timeout": instance.subagent_timeout,
    })
}

pub fn config_instance_get(
    db: &HcomDb,
    instance_arg: &str,
    key: Option<&str>,
) -> Result<Value, String> {
    let name = identity::resolve_display_name(db, instance_arg)
        .unwrap_or_else(|| instance_arg.to_string());
    let instance = db
        .get_instance_full(&name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| identity::describe_missing_agent(db, instance_arg))?;

    Ok(match key {
        None => {
            let full_name = crate::identity::get_full_name(&instance);
            build_instance_config_json(&instance, &full_name)
        }
        Some("tag") => serde_json::json!({"value": instance.tag.as_deref().unwrap_or("")}),
        Some("timeout") => serde_json::json!({"value": instance.wait_timeout.unwrap_or(86400)}),
        Some("hints") => serde_json::json!({"value": instance.hints.as_deref().unwrap_or("")}),
        Some("subagent_timeout") => match instance.subagent_timeout {
            Some(value) => serde_json::json!({"value": value}),
            None => serde_json::json!({"value": serde_json::Value::Null}),
        },
        Some(other) => return Err(format!("Unknown instance config key '{}'", other)),
    })
}

pub fn config_instance_set(
    db: &HcomDb,
    instance_arg: &str,
    key: &str,
    value: &str,
) -> Result<Value, String> {
    let name = identity::resolve_display_name(db, instance_arg)
        .unwrap_or_else(|| instance_arg.to_string());
    let instance = db
        .get_instance_full(&name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| identity::describe_missing_agent(db, instance_arg))?;
    let inst_name = &instance.name;

    match key {
        "tag" => {
            if !value.is_empty()
                && !value
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            {
                return Err("Tag must be alphanumeric (hyphens and underscores allowed)".into());
            }
            db.conn()
                .execute(
                    "UPDATE instances SET tag = ? WHERE name = ?",
                    rusqlite::params![value, inst_name],
                )
                .map_err(|e| e.to_string())?;
        }
        "timeout" => {
            let secs = if value.is_empty() || value.eq_ignore_ascii_case("default") {
                crate::config::HcomConfig::load(None).timeout
            } else {
                match value.parse::<i64>() {
                    Ok(secs) if secs > 0 => secs,
                    _ => return Err("timeout must be a positive integer (seconds)".to_string()),
                }
            };
            db.conn()
                .execute(
                    "UPDATE instances SET wait_timeout = ? WHERE name = ?",
                    rusqlite::params![secs, inst_name],
                )
                .map_err(|e| e.to_string())?;
        }
        "hints" => {
            let mut updates = serde_json::Map::new();
            if value.is_empty() {
                updates.insert("hints".into(), serde_json::Value::Null);
            } else {
                updates.insert("hints".into(), serde_json::json!(value));
            }
            instances::update_instance_position(db, inst_name, &updates);
        }
        "subagent_timeout" => {
            let mut updates = serde_json::Map::new();
            if value.is_empty() || value.eq_ignore_ascii_case("default") {
                updates.insert("subagent_timeout".into(), serde_json::Value::Null);
            } else {
                let secs = value.parse::<i64>().map_err(|_| {
                    "subagent_timeout must be a positive integer (seconds)".to_string()
                })?;
                if secs <= 0 {
                    return Err("subagent_timeout must be a positive integer (seconds)".to_string());
                }
                updates.insert("subagent_timeout".into(), serde_json::json!(secs));
            }
            instances::update_instance_position(db, inst_name, &updates);
        }
        other => return Err(format!("Unknown instance config key '{}'", other)),
    }

    crate::notify::wake_all(db);
    Ok(serde_json::json!({"instance": inst_name, "field": key, "value": value}))
}

// ── Main Entry Point ─────────────────────────────────────────────────────

fn note_bootstrap_exclusion_source() {
    let file_config = crate::config::HcomConfig::load(Some(&std::collections::HashMap::new()));
    let effective = crate::config::HcomConfig::load(None);
    if effective.bootstrap_exclude != file_config.bootstrap_exclude
        && std::env::var_os("HCOM_BOOTSTRAP_EXCLUDE").is_some()
    {
        eprintln!(
            "Note: bootstrap_exclude={} from the current environment overrides config.toml ({}).",
            effective.bootstrap_exclude, file_config.bootstrap_exclude
        );
    }
}

fn config_bootstrap(db: &HcomDb, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("show") => {
            if args.len() > 2 {
                return crate::commands::help::usage_error(
                    "config bootstrap",
                    &format!(
                        "Unexpected argument for hcom config bootstrap show: '{}'",
                        args[2]
                    ),
                );
            }
            note_bootstrap_exclusion_source();
            if let Some(part) = args.get(1) {
                match crate::bootstrap::part_source(part) {
                    Some(source) => println!("{source}"),
                    None => {
                        let hint = crate::shared::suggest::did_you_mean(
                            part,
                            crate::bootstrap::EXCLUDE_PARTS.iter().copied(),
                        );
                        return crate::commands::help::usage_error(
                            "config bootstrap",
                            &format!(
                                "Unknown bootstrap part '{part}'{hint}\nParts: {}",
                                crate::bootstrap::EXCLUDE_PARTS.join(", ")
                            ),
                        );
                    }
                }
            } else {
                let config = crate::config::HcomConfig::load(None);
                let excluded = crate::bootstrap::parse_excluded_parts(&config.bootstrap_exclude)
                    .unwrap_or_default();
                for part in crate::bootstrap::EXCLUDE_PARTS {
                    let state = if excluded.contains(part) {
                        "excluded"
                    } else {
                        "included"
                    };
                    println!(
                        "{part} ({state})\n{}\n",
                        crate::bootstrap::part_source(part).unwrap_or_default()
                    );
                }
            }
            0
        }
        Some("render") => {
            if !(3..=4).contains(&args.len()) || (args.len() == 4 && args[3] != "launched") {
                return crate::commands::help::usage_error(
                    "config bootstrap",
                    "hcom config bootstrap render needs <name> <tool> [launched]",
                );
            }
            if args[2].parse::<crate::tool::Tool>().is_err() {
                return crate::commands::help::usage_error(
                    "config bootstrap",
                    &format!("Unknown tool '{}'", args[2]),
                );
            }
            note_bootstrap_exclusion_source();
            let mut hcom_ctx = crate::shared::HcomContext::from_os();
            hcom_ctx.is_launched = args.len() == 4;
            println!(
                "{}",
                crate::bootstrap::get_bootstrap(db, &hcom_ctx, &args[1], &args[2])
            );
            0
        }
        None => crate::commands::help::usage_error(
            "config bootstrap",
            "hcom config bootstrap needs show or render",
        ),
        Some(other) => crate::commands::help::unknown_subcommand(
            "config bootstrap",
            other,
            &["show", "render"],
        ),
    }
}

/// Main entry point for `hcom config` command.
/// Keys only `hcom relay` sets. `HCOM_RELAY_SHARE_ACCOUNT` is not one: it
/// changes what the worker sends, not whether or where it runs.
const RELAY_MANAGED_KEYS: &[&str] = &[
    "HCOM_RELAY",
    "HCOM_RELAY_ID",
    "HCOM_RELAY_TOKEN",
    "HCOM_RELAY_ENABLED",
    "HCOM_RELAY_PSK",
];

pub fn cmd_config(db: &HcomDb, args: &ConfigArgs, ctx: Option<&CommandContext>) -> i32 {
    if args.key.as_deref() != Some("bootstrap") && !args.extra.is_empty() {
        eprintln!("Error: quote multi-word config values");
        return 1;
    }
    drop_old_delivery_keys();
    let json_mode = args.json;
    let edit_mode = args.edit;
    let unset_mode = args.unset;
    let reset_mode = args.reset;
    let info_mode = args.info;
    let setup_mode = args.setup;
    let instance_name = args.instance.clone();
    // Reconstruct argv from key + value for backward compat with existing handlers
    let argv: Vec<String> = args
        .key
        .iter()
        .cloned()
        .chain(args.value.iter().cloned())
        .chain(args.extra.iter().cloned())
        .collect();

    // --setup: only valid with 'terminal kitty' (C4 fix)
    if setup_mode {
        let positional: Vec<&String> = argv.iter().filter(|a| !a.starts_with('-')).collect();
        let is_kitty_terminal = positional.len() >= 2
            && positional[0] == "terminal"
            && positional[1].starts_with("kitty");
        if !is_kitty_terminal {
            eprintln!(
                "Error: --setup is only valid with kitty: hcom config terminal kitty --setup"
            );
            return 1;
        }
    }

    // Instance config mode
    if let Some(ref inst) = instance_name {
        let resolved = identity::resolve_display_name(db, inst).unwrap_or_else(|| inst.to_string());
        if let Some((base_name, device)) = crate::relay::control::split_device_suffix(&resolved) {
            let action = if argv.len() >= 2 {
                crate::relay::control::rpc_action::CONFIG_SET
            } else {
                crate::relay::control::rpc_action::CONFIG_GET
            };
            let params = if argv.len() >= 2 {
                json!({
                    "instance": base_name,
                    "field": argv[0],
                    "value": argv[1..].join(" "),
                })
            } else {
                json!({
                    "instance": base_name,
                    "field": argv.first(),
                })
            };
            return match crate::relay::control::dispatch_remote(
                db,
                device,
                Some(&resolved),
                action,
                &params,
                crate::relay::control::RPC_DEFAULT_TIMEOUT,
            ) {
                Ok(inner) => {
                    if action == crate::relay::control::rpc_action::CONFIG_GET {
                        println!(
                            "{}",
                            render_config_instance_get(
                                &inner,
                                argv.first().map(|s| s.as_str()),
                                json_mode
                            )
                        );
                    } else {
                        let field = argv.first().map(|s| s.as_str()).unwrap_or("");
                        let value = argv.get(1..).map(|v| v.join(" ")).unwrap_or_default();
                        let instance_name = inner["instance"].as_str().unwrap_or(base_name);
                        println!(
                            "{}",
                            render_config_instance_set_feedback(instance_name, field, &value)
                        );
                    }
                    0
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    1
                }
            };
        }
        return config_instance(db, inst, &argv, ctx, json_mode);
    }

    // Edit mode
    if edit_mode {
        let path = config_path();
        // Ensure file exists
        let prepared = if path.exists() {
            crate::config::lock_down_config_permissions(&path)
        } else {
            crate::config::write_config_toml_path(&path, "# hcom configuration\n")
        };
        if let Err(e) = prepared {
            eprintln!("Error: Failed to prepare config.toml: {e}");
            return 1;
        }
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| "vim".to_string());
        let status = std::process::Command::new(&editor).arg(&path).status();
        match status {
            Ok(s) if s.success() => return 0,
            Ok(s) => {
                eprintln!("Editor exited with {s}");
                return 1;
            }
            Err(e) => {
                eprintln!("Error: Failed to launch editor '{editor}': {e}");
                return 1;
            }
        }
    }

    // Reset mode — delegate to reset.rs for proper timestamped archiving + env file
    if reset_mode {
        return super::reset_ops::reset_config();
    }

    // No args: show all config
    if argv.is_empty() {
        return show_all_config(db, ctx, json_mode);
    }

    // File-level failures affect every get path, including special keys.
    if argv.len() == 1 && !unset_mode && !info_mode {
        let (_, errors) = crate::config::HcomConfig::load_with_errors(None);
        if let Some(msg) = errors.get("config.toml") {
            eprintln!("Invalid config.toml: {msg}");
        }
    }

    let key_arg = &argv[0];

    if key_arg == "bootstrap" {
        if instance_name.is_some()
            || edit_mode
            || unset_mode
            || reset_mode
            || setup_mode
            || info_mode
            || json_mode
        {
            eprintln!("Error: unsupported option for 'hcom config bootstrap'");
            return 1;
        }
        return config_bootstrap(db, &argv[1..]);
    }

    if normalize_key(key_arg) == "HCOM_DEV_ROOT" {
        return config_dev_root(
            db,
            argv.get(1).map(|s| s.as_str()),
            unset_mode,
            json_mode,
            info_mode,
        );
    }

    // Terminal subcommand
    if key_arg == "terminal" {
        if info_mode {
            print_terminal_info();
            return 0;
        }
        let rc = if unset_mode {
            config_terminal(&["default".to_string()], setup_mode)
        } else {
            config_terminal(&argv[1..], setup_mode)
        };
        if rc == 0 && (unset_mode || argv.len() > 1) {
            note_env_override("HCOM_TERMINAL");
        }
        return rc;
    }

    // Check for info request: `config key --info` or `config key info` or `config key ?`
    let wants_info = info_mode
        || argv.get(1).map(|s| s.as_str()) == Some("info")
        || argv.get(1).map(|s| s.as_str()) == Some("?");

    // Normalize key to HCOM_ prefix
    let key = normalize_key(key_arg);

    if wants_info {
        return show_key_info(&key);
    }

    // Unknown keys would silently read as "(not set)" or be written to
    // config.toml where nothing reads them. Reading an env-only setting that
    // is actually set (e.g. HCOM_DIR) stays allowed.
    let is_set_mode = argv.len() >= 2 || unset_mode;
    let env_readable = !is_set_mode && std::env::var(&key).is_ok();
    if crate::config::user_field(&key).is_none() && !env_readable {
        let typed = key_arg.to_lowercase();
        let short: Vec<String> = crate::config::user_fields()
            .map(|f| f.name.to_string())
            .chain(["dev_root".to_string()])
            .collect();
        eprintln!(
            "Error: Unknown config key '{key_arg}'{}\nValid keys: {}",
            crate::shared::suggest::did_you_mean(
                typed.trim_start_matches("hcom_"),
                short.iter().map(String::as_str)
            ),
            short.join(", ")
        );
        return 1;
    }

    // Set mode: config KEY VALUE, or config KEY --unset (back to the default)
    if is_set_mode {
        if unset_mode && argv.len() >= 2 {
            eprintln!("Error: pass a value or --unset, not both");
            return 1;
        }
        // Relay settings change what the relay worker does and must go through
        // `hcom relay`, which validates them and starts/stops the worker to match.
        if RELAY_MANAGED_KEYS.contains(&key.as_str()) {
            eprintln!(
                "Error: {key} is managed by 'hcom relay' (new, connect, on, off).\nRun: hcom relay --help"
            );
            return 1;
        }
        // Join all remaining args with spaces
        let value = argv[1..].join(" ");

        // Validate and write first; side effects run after the confirmation.
        let result = if unset_mode {
            config_unset(&key)
        } else {
            config_set(&key, &value)
        };
        if let Err(e) = result {
            eprintln!("Error: {e}");
            return 1;
        }
        if unset_mode {
            println!("Unset {key}");
        } else {
            println!("Set {key} = {value}");
        }
        note_env_override(&key);
        if key == "HCOM_AUTO_APPROVE" && !update_auto_approve_permissions() {
            return 1;
        }
        return 0;
    }

    // Get mode: config KEY
    let (value, _source) = config_get(&key);
    if json_mode {
        let mut m = serde_json::Map::new();
        m.insert(key.clone(), serde_json::Value::String(value.clone()));
        println!("{}", serde_json::Value::Object(m));
    } else if value.is_empty() {
        println!("{key}: (not set)");
    } else {
        println!("{value}");
    }
    // The raw value is shown as set; say when the loader replaces it.
    if let Some(field) = crate::config::user_field(&key)
        && let (config, errors) = crate::config::HcomConfig::load_with_errors(None)
        && let Some(msg) = errors.get(field.name)
    {
        let effective = config.get_field(field.name).unwrap_or_default();
        eprintln!("Invalid, using the default ({effective}): {msg}");
    }

    0
}

/// An env var wins over config.toml. Agents launched by hcom carry every
/// setting in env as of their launch, so this is common there.
fn note_env_override(key: &str) {
    if let Ok(env_val) = std::env::var(key) {
        eprintln!(
            "Note: {key}={env_val} in this environment overrides config.toml here and for agents launched from here"
        );
    }
}

/// Normalize a key argument to HCOM_* format.
fn normalize_key(input: &str) -> String {
    let upper = input.to_uppercase();
    if upper.starts_with("HCOM_") {
        upper
    } else {
        format!("HCOM_{upper}")
    }
}

/// Build runtime overrides map from instance DB data.
///
/// Reads per-instance DB values
/// (tag, wait_timeout, hints, subagent_timeout) and returns overrides
/// where the instance value differs from the global config value.
fn get_runtime_overrides(
    db: &HcomDb,
    ctx: Option<&CommandContext>,
) -> std::collections::HashMap<&'static str, String> {
    use std::collections::HashMap;

    let mut overrides: HashMap<&'static str, String> = HashMap::new();

    // DB column -> config key mapping
    const RUNTIME_KEYS: &[(&str, &str)] = &[
        ("tag", "HCOM_TAG"),
        ("wait_timeout", "HCOM_TIMEOUT"),
        ("hints", "HCOM_HINTS"),
        ("subagent_timeout", "HCOM_SUBAGENT_TIMEOUT"),
    ];

    // Use identity from command context (already resolved by CLI router)
    let instance_name = ctx
        .and_then(|c| c.identity.as_ref())
        .filter(|id| matches!(id.kind, crate::shared::SenderKind::Instance))
        .map(|id| id.name.as_str());

    let instance_name = match instance_name {
        Some(name) => name,
        None => return overrides,
    };

    let instance_data = match db.get_instance_full(instance_name) {
        Ok(Some(data)) => data,
        _ => return overrides,
    };

    for (db_col, config_key) in RUNTIME_KEYS {
        let val = match *db_col {
            "tag" => instance_data.tag.clone(),
            "wait_timeout" => instance_data.wait_timeout.map(|v| v.to_string()),
            "hints" => instance_data.hints.clone(),
            "subagent_timeout" => instance_data.subagent_timeout.map(|v| v.to_string()),
            _ => None,
        };
        if let Some(val) = val {
            let (global_val, _) = config_get(config_key);
            if val != global_val {
                overrides.insert(config_key, val);
            }
        }
    }

    overrides
}

/// Show all config keys with values and sources.
fn show_all_config(db: &HcomDb, ctx: Option<&CommandContext>, json_mode: bool) -> i32 {
    let runtime_overrides = get_runtime_overrides(db, ctx);
    let dev_root = crate::router::resolve_effective_dev_root(db.path());
    let (_, config_errors) = crate::config::HcomConfig::load_with_errors(None);
    if let Some(msg) = config_errors.get("config.toml") {
        eprintln!("Invalid config.toml: {msg}");
    }

    if json_mode {
        let mut result = serde_json::Map::new();
        for key in crate::config::user_fields().map(|f| f.env) {
            let (value, _source) = config_get(key);
            // {KEY: value} — mask relay token
            let display = if key == "HCOM_RELAY_TOKEN" && value.len() > 4 {
                format!("{}***", &value[..4])
            } else {
                value
            };
            result.insert(key.to_string(), serde_json::Value::String(display));
        }
        if let Some((path, source)) = dev_root {
            result.insert(
                "dev_root".to_string(),
                serde_json::Value::String(path.to_string_lossy().into_owned()),
            );
            result.insert(
                "dev_root_source".to_string(),
                serde_json::Value::String(source.to_string()),
            );
        }
        if !config_errors.is_empty() {
            result.insert("config_errors".to_string(), json!(config_errors));
        }
        println!(
            "{}",
            serde_json::to_string(&Value::Object(result)).unwrap_or_default()
        );
    } else {
        println!("hcom configuration ({})\n", config_path().display());
        println!("hcom Settings:");
        for key in crate::config::user_fields().map(|f| f.env) {
            // Check runtime override first
            let (display, source) = if let Some(val) = runtime_overrides.get(key) {
                (val.clone(), "runtime")
            } else {
                let (value, source) = config_get(key);
                let display = if value.is_empty() {
                    "(not set)".to_string()
                } else if key == "HCOM_RELAY_TOKEN" && !value.is_empty() {
                    let visible = value.len().min(4);
                    format!("{}...", &value[..visible])
                } else {
                    value
                };
                (display, source)
            };
            println!("  {key:<28} {display:<30} [{source}]");
        }
        if let Some((path, source)) = dev_root {
            println!("  {:<28} {:<30} [{}]", "dev_root", path.display(), source);
        }
        if !config_errors.is_empty() {
            println!("\nInvalid, using the default instead:");
            for msg in config_errors.values() {
                println!("  {msg}");
            }
        }
        println!(
            "\n[env] = environment, [toml] = config.toml, [runtime] = agent override, [kv] = hcom.db key-value, [default] = default"
        );
        println!("\nEdit: hcom config --edit");
    }
    0
}

/// Show detailed info for a config key.
/// Rich per-key help text.
pub fn config_help(key: &str) -> Option<String> {
    if let Some(spec) = crate::integration_spec::ALL
        .iter()
        .find(|s| s.launch.args_env == Some(key))
    {
        return Some(format!(
            "{key} - Default args passed to {} on launch\n\nSet:   hcom config {}_args \"<args>\"\nClear: hcom config {}_args \"\"\n\nExplicit CLI arguments replace known singleton defaults (including aliases). Other arguments pass through in order.",
            spec.cli_binary, spec.name, spec.name
        ));
    }
    match key {
        "HCOM_TAG" => Some(
            "\
HCOM_TAG - Group tag for launched instances

Purpose:
  Creates named groups of agents that can be addressed together.
  When set, launched instances get names like: <tag>-<name>

Usage:
  hcom config tag myteam        # Set tag
  hcom config tag \"\"            # Clear tag

  # Or via environment:
  HCOM_TAG=myteam hcom 3 claude

Effect:
  Without tag: launches create → luna, nova, kira
  With tag \"dev\": launches create → dev-luna, dev-nova, dev-kira

Addressing:
  @dev         → sends to all agents with tag \"dev\"
  @dev-luna    → sends to specific agent

Allowed characters: letters, numbers, hyphens (a-z, A-Z, 0-9, -)",
        ),

        "HCOM_HINTS" => Some(
            "\
HCOM_HINTS - Text injected with all messages

Purpose:
  Appends text to every message received by launched agents.
  Useful for persistent instructions or context.

Usage:
  hcom config hints \"Always respond in JSON format\"
  hcom config hints \"\"   # Clear hints

Example:
  hcom config hints \"You are part of team-alpha. Coordinate with @team-alpha members.\"

Notes:
  - Hints are appended to message content, not system prompt
  - Each agent can have different hints (set via hcom config -i <name> hints)
  - Global hints apply to all new launches",
        ),

        "HCOM_NOTES" => Some(
            "\
HCOM_NOTES - One-time notes appended to bootstrap

  Custom text added to agent system context at startup.
  Unlike HCOM_HINTS (per-message), this is injected once and does not repeat.

Usage:
  hcom config notes \"Always check hcom list before spawning new agents\"
  hcom config notes \"\"                            # Clear
  HCOM_NOTES=\"tips\" hcom 1 claude                 # Per-launch override

  Changing after launch has no effect (bootstrap already delivered).",
        ),

        "HCOM_BOOTSTRAP_EXCLUDE" => Some(
            "\
HCOM_BOOTSTRAP_EXCLUDE - Comma-separated bootstrap parts to leave out

Parts: identity, messages, capabilities, rules, handoff
Default: none
Use all to exclude every built-in part.
Excluded parts may remove instructions agents need to use hcom correctly.
Individual parts affect the main bootstrap; all also suppresses the Claude subagent bootstrap.

  hcom config bootstrap_exclude rules
  hcom config bootstrap_exclude rules,handoff
  hcom config bootstrap_exclude all
  hcom config bootstrap_exclude none
  HCOM_BOOTSTRAP_EXCLUDE=rules hcom 1 claude

Inspect the exact parts and rendered result:
  hcom config bootstrap show [part]
  hcom config bootstrap render <name> <tool> [launched]

Notes remain appended and take precedence over other hcom instructions.",
        ),

        "HCOM_TIMEOUT" => Some(
            "\
HCOM_TIMEOUT - Advanced: idle timeout for headless Claude (seconds)

Default: 86400 (24 hours)

This setting only applies to:
  - Headless Claude: hcom N claude -p

Does NOT apply to:
  - Interactive PTY mode: hcom N claude (main path)
  - Gemini or Codex

How it works:
  - Claude's Stop hook runs when Claude goes idle
  - Hook waits up to TIMEOUT seconds for a message
  - If no message within timeout, instance is unregistered

Usage (if needed):
  hcom config HCOM_TIMEOUT 3600   # 1 hour
  export HCOM_TIMEOUT=3600        # via environment",
        ),

        "HCOM_SUBAGENT_TIMEOUT" => Some(
            "\
HCOM_SUBAGENT_TIMEOUT - Timeout for Claude subagents (seconds)

Default: 30

Purpose:
  How long Claude waits for a subagent (Task tool) to complete.
  Shorter than main timeout since subagents should be quick.

Usage:
  hcom config subagent_timeout 60    # 1 minute
  hcom config subagent_timeout 30    # 30 seconds (default)

Notes:
  - Only applies to Claude Code's Task tool spawned agents
  - Parent agent blocks until subagent completes or times out
  - Increase for complex subagent tasks",
        ),

        "HCOM_CLAUDE_DELIVERY" => Some(
            "\
HCOM_CLAUDE_DELIVERY - How hcom wakes idle Claude agents

  hcom config claude_delivery inject     # type into the terminal
  hcom config claude_delivery --unset    # back to the default (native)
  HCOM_CLAUDE_DELIVERY=inject hcom claude  # one launch

native  bundled Claude mod (loaded with --plugin-dir) submits the wake (default)
inject  PTY wrapper types the wake into Claude's prompt

Native needs Claude Code 2.1.287+ (mod support). Managed disableSideloadFlags
rejects --plugin-dir and the launch fails; an older Claude, allowManagedModsOnly
or a disabled mod rollout leave the mod unloaded, so the launch never reports
ready and the agent shows as unavailable in list/TUI. Use inject in both cases.
Applies to new launches; kill and resume to change an agent.
Print (-p) sessions use hook delivery either way.",
        ),




        "HCOM_RELAY" => Some(
            "\
HCOM_RELAY - MQTT broker URL

Empty = use public brokers (broker.emqx.io, broker.hivemq.com, test.mosquitto.org).
Set automatically by 'hcom relay new' (pins first working broker).
Stored in [relay] in config.toml. Environment overrides are ignored for relay fields.

Private broker: hcom relay new --broker mqtts://host:port",
        ),

        "HCOM_RELAY_ID" => Some(
            "\
HCOM_RELAY_ID - Shared UUID for relay group

Generated by 'hcom relay new'. Other devices join with 'hcom relay connect <token>'.
Stored in [relay] in config.toml. Environment overrides are ignored for relay fields.
All devices with the same relay_id sync state via MQTT pub/sub.",
        ),

        "HCOM_RELAY_TOKEN" => Some(
            "\
HCOM_RELAY_TOKEN - Auth token for MQTT broker

Optional. Set via 'hcom relay new|connect --password <secret>'.
Stored in [relay] in config.toml. Environment overrides are ignored for relay fields.
Only needed if your broker requires authentication.",
        ),

        "HCOM_AUTO_APPROVE" => Some(
            "\
HCOM_AUTO_APPROVE - Auto-approve safe hcom commands

Purpose:
  When enabled, hcom agents auto-approve \"safe\" hcom commands without
  requiring user confirmation.

Usage:
  hcom config auto_approve 1    # Enable auto-approve
  hcom config auto_approve 0    # Disable (require approval)

Safe commands (auto-approved when enabled):
  send, start, list, events, listen, relay, config,
  transcript, archive, status, help, --help, --version

Always require approval:
  - hcom reset          (archives and clears database)
  - hcom stop           (stops instances)
  - hcom <N> claude     (launches new instances)

Values: 1, true, yes, on (enabled) | 0, false, no, off, \"\" (disabled)",
        ),

        "HCOM_AUTO_SUBSCRIBE" => Some(
            "\
HCOM_AUTO_SUBSCRIBE - Auto-subscribe event presets for new instances

Default: collision

Purpose:
  Comma-separated list of event subscriptions automatically added
  when an instance registers with 'hcom start'.

Usage:
  hcom config auto_subscribe \"collision,created\"
  hcom config auto_subscribe \"\"   # No auto-subscribe

Available presets:
  collision    - Alert when agents edit same file (within 30s window)
  created      - Notify when new instances join
  stopped      - Notify when instances leave
  blocked      - Notify when any instance is blocked (needs approval)

Notes:
  - Instances can add/remove subscriptions at runtime
  - See 'hcom events --help' for subscription management",
        ),

        "HCOM_TITLE_MODE" => Some(
            "\
HCOM_TITLE_MODE - What appears in the terminal/tab title for hcom-launched agents

Default: combined

Purpose:
  Controls whether hcom replaces, combines, or leaves alone the wrapped tool's
  terminal title. In combined mode, hcom keeps its live status and appends the
  tool's own live title (for example, a Codex spinner).

Values:
  combined - Show '{icon} name - {tool title}' and update it live.
  label    - Show hcom's status label only: '{icon} name [tool]'.
  off      - Pass the tool's own terminal title through unchanged; hcom writes none.

Usage:
  hcom config title_mode combined   # hcom status + live tool title (default)
  hcom config title_mode label      # hcom status label only
  hcom config title_mode off        # use the tool's title
  hcom config title_mode <empty>    # reset to the default

Notes:
  - This affects terminal/tab titles, not the visible PTY output.
  - Tools that do not emit terminal titles have no live child title to append.
  - The same setting can be provided with HCOM_TITLE_MODE in the environment.",
        ),

        "HCOM_NAME_EXPORT" => Some(
            "\
HCOM_NAME_EXPORT - Export instance name to custom env var

Purpose:
  When set, launched instances will have their name exported to
  the specified environment variable. Useful for scripts that need
  to reference the current instance name.

Usage:
  hcom config name_export \"MY_AGENT_NAME\"   # Export to MY_AGENT_NAME
  hcom config name_export \"\"                 # Disable export

Example:
  # Set export variable
  hcom config name_export \"HCOM_NAME\"

  # Now launched instances have:
  # HCOM_NAME=luna (or whatever name was generated)

  # Scripts can use it:
  # hcom send \"@$HCOM_NAME completed task\"

Notes:
  - Only affects hcom-launched instances (hcom N claude/gemini/codex/opencode/kilo/pi/omp/agy/cursor/kimi/copilot/qoder)
  - Variable name must be a valid shell identifier
  - Works alongside HCOM_PROCESS_ID (always set) for identity",
        ),



        "HCOM_COPILOT_DELIVERY" => Some(
            "\
HCOM_COPILOT_DELIVERY - How idle Copilot agents get messages

  hcom config copilot_delivery inject   # type into the terminal
  hcom config copilot_delivery --unset  # back to the default (native)
  HCOM_COPILOT_DELIVERY=inject hcom copilot   # per-launch override

native (default): `hcom copilot` ships an extension in its per-run plugin that hands
messages to an idle session with the Copilot SDK (`session.send`) instead of
typing into the terminal: no waiting for an empty prompt, no extra wake turn.
Copilot CLI extensions are experimental: if Copilot's settings.json leaves
them unset, launch turns on enabledFeatureFlags.EXTENSIONS and says so on
stderr; if they were turned off (flag false, /extensions mode disabled, hcom
disabled), launch fails. Mid-turn delivery uses Copilot hooks either way.
Applies to new launches.

Diagnose: `hcom list <name>` shows the extension's wake port once bound;
hcom.log has plugin.copilot_* (extension), copilot.ext_* (hcom side) and
delivery.copilot_extension_stalled when idle messages are not delivered.",
        ),







        "HCOM_RELAY_ENABLED" => Some(
            "\
HCOM_RELAY_ENABLED - Whether relay sync is on

Default: true (when relay is configured)
Stored in [relay] in config.toml. Environment overrides are ignored for relay fields.

Usage:
  hcom relay off    Disable relay sync (keeps relay configuration)
  hcom relay on     Re-enable relay sync",
        ),

        _ => None,
    } .map(str::to_string)
}

/// Build terminal help text (shared between config --info and run docs --config).
pub fn terminal_help_text(show_current: bool) -> String {
    use crate::shared::terminal_presets::TERMINAL_PRESETS;

    let platform = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    };

    // Managed parents and their variants. A preset is "managed" when it
    // defines `close` — hcom can shut its agent window down on kill. The
    // built-in list is hand-curated here for ordering and human-readable
    // descriptions, but `is_managed_preset` below is the source of truth for
    // section bucketing so a new managed preset never gets mis-listed under
    // "Other (opens window only)".
    const MANAGED_PARENTS: &[(&str, &str)] = &[
        ("kitty", "auto split/tab/window"),
        ("wezterm", "auto tab/split/window"),
        ("tmux", "detached sessions"),
        ("cmux", "workspaces"),
        ("zellij", "panes"),
        ("waveterm", "blocks"),
        ("herdr", "panes"),
    ];
    const MANAGED_VARIANTS: &[(&str, &[&str])] = &[
        ("kitty", &["kitty-window", "kitty-tab", "kitty-split"]),
        (
            "wezterm",
            &["wezterm-window", "wezterm-tab", "wezterm-split"],
        ),
        ("tmux", &["tmux-split"]),
    ];

    let all_managed: std::collections::HashSet<&str> = {
        let mut s = std::collections::HashSet::new();
        for (parent, _) in MANAGED_PARENTS {
            s.insert(*parent);
        }
        for (_, variants) in MANAGED_VARIANTS {
            for v in *variants {
                s.insert(v);
            }
        }
        s
    };

    // Check binary availability
    let is_available = |preset_name: &str| -> bool {
        let preset = TERMINAL_PRESETS.iter().find(|(n, _)| *n == preset_name);
        let Some((_name, p)) = preset else {
            return false;
        };
        if p.binary
            .is_some_and(|bin| crate::terminal::which_bin(bin).is_some())
        {
            return true;
        }
        #[cfg(target_os = "macos")]
        {
            let app = p.app_name.unwrap_or(_name);
            // Handle preset names that already end in .app
            let bundle = if app.ends_with(".app") {
                app.to_string()
            } else {
                format!("{app}.app")
            };
            for dir in [
                "/Applications",
                "/Applications/Utilities",
                "/System/Applications",
                "/System/Applications/Utilities",
            ] {
                let path = format!("{dir}/{bundle}");
                if std::path::Path::new(&path).exists() {
                    return true;
                }
            }
        }
        false
    };

    let mut lines = Vec::new();
    lines.push("HCOM_TERMINAL — where hcom opens new agent windows".to_string());
    lines.push(String::new());

    if show_current {
        let (current, source) = config_get("HCOM_TERMINAL");
        if current.is_empty() || current == "default" {
            // "default" is a sentinel meaning auto-detect; it isn't a preset,
            // so don't run it through close-presence inference.
            let suffix = if source == "default" {
                String::new()
            } else {
                format!(" [{source}]")
            };
            lines.push(format!("Current: default (auto-detect){suffix}"));
        } else {
            let kind = if crate::config::get_merged_preset(&current)
                .is_some_and(|p| p.has_close(cfg!(windows)))
            {
                "managed"
            } else {
                "open only"
            };
            lines.push(format!("Current: {current} ({kind}) [{source}]"));
        }
        lines.push(String::new());
    }

    // Managed section
    lines.push("Managed (open + close on kill):".to_string());
    for (parent, desc) in MANAGED_PARENTS {
        // Skip if not on this platform
        let on_platform = TERMINAL_PRESETS
            .iter()
            .find(|(n, _)| n == parent)
            .is_some_and(|(_n, p)| p.platforms.is_empty() || p.platforms.contains(&platform));
        if !on_platform {
            continue;
        }
        let mark = if is_available(parent) { "[+]" } else { "[-]" };
        lines.push(format!("  {mark} {:<14} {desc}", parent));
    }
    lines.push(String::new());
    lines.push("  Variants:".to_string());
    for (parent, variants) in MANAGED_VARIANTS {
        let on_platform = TERMINAL_PRESETS
            .iter()
            .find(|(n, _)| n == parent)
            .is_some_and(|(_n, p)| p.platforms.is_empty() || p.platforms.contains(&platform));
        if !on_platform {
            continue;
        }
        lines.push(format!("    {parent}: {}", variants.join(", ")));
    }

    // Other (open-only, platform-filtered). Bucket by `close` presence rather
    // than the hand-curated MANAGED_PARENTS list, so any built-in preset that
    // grows a close command later doesn't silently land in the wrong section.
    lines.push(String::new());
    lines.push("Other (opens window only):".to_string());
    for (name, preset) in TERMINAL_PRESETS.iter() {
        let has_close = preset.close.default.is_some() || preset.close.windows.is_some();
        if all_managed.contains(name) || has_close {
            continue;
        }
        if !preset.platforms.is_empty() && !preset.platforms.contains(&platform) {
            continue;
        }
        let mark = if is_available(name) { "[+]" } else { "[-]" };
        lines.push(format!("  {mark} {name}"));
    }

    // TOML-defined presets not in built-ins
    let toml_path = crate::paths::config_toml_path();
    let toml_presets: Vec<(String, bool)> = crate::config::load_toml_presets(&toml_path)
        .and_then(|p| {
            p.as_table().map(|t| {
                t.iter()
                    .filter(|(name, _)| !TERMINAL_PRESETS.iter().any(|(n, _)| *n == name.as_str()))
                    .map(|(name, val)| {
                        // close may be a legacy string or an argv array.
                        let has_close = val.get("close").is_some_and(|v| {
                            v.as_str().is_some_and(|s| !s.is_empty())
                                || v.as_array().is_some_and(|a| !a.is_empty())
                        });
                        (name.clone(), has_close)
                    })
                    .collect()
            })
        })
        .unwrap_or_default();
    if !toml_presets.is_empty() {
        lines.push(String::new());
        lines.push("Custom presets (config.toml):".to_string());
        for (name, has_close) in &toml_presets {
            let kind = if *has_close {
                "open + close"
            } else {
                "open only"
            };
            let mark = if crate::config::get_merged_preset(name)
                .and_then(|p| p.binary)
                .map(|b| crate::terminal::which_bin(&b).is_some())
                .unwrap_or(true)
            {
                "[+]"
            } else {
                "[-]"
            };
            lines.push(format!("  {mark} {:<14} ({kind})", name));
        }
    }

    lines.push(String::new());
    lines.push("Custom command (open only):".to_string());
    lines.push("  hcom config terminal \"my-terminal -e bash {script}\"".to_string());
    lines.push(String::new());
    lines.push("Custom preset with close (~/.hcom/config.toml):".to_string());
    lines.push("  [terminal.presets.myterm]".to_string());
    lines.push("  open = \"myterm spawn -- bash {script}\"".to_string());
    lines.push("  close = \"myterm kill --id {pane_id}\"".to_string());
    lines.push("  binary = \"myterm\"".to_string());
    lines.push("  pane_id_env = \"MYTERM_PANE_ID\"".to_string());
    lines.push(String::new());
    lines.push("Placeholders:".to_string());
    lines.push("  {script}     = hcom-generated launch wrapper script path".to_string());
    lines.push(
        "  {pane_id}    = pane/window/workspace ID from pane_id_env; falls back to {id}"
            .to_string(),
    );
    lines.push("  {process_id} = HCOM_PROCESS_ID for the launched agent".to_string());
    lines.push("  {pid}        = launched terminal process ID".to_string());
    lines.push("  {id}         = first line of stdout captured from the open command".to_string());
    lines.push(
        "                 (herdr `agent start` JSON is parsed for result.agent.pane_id)"
            .to_string(),
    );
    lines.push("  {cwd}        = working directory the agent will start in".to_string());
    lines.push("  {instance_name} = hcom instance name (e.g. \"luna\")".to_string());
    lines.push("  {tool}          = tool label (e.g. \"claude\", \"codex\")".to_string());
    lines.push(
        "  {pane_title}    = pre-formatted label (\"\u{25c9} luna [claude]\"); falls back to"
            .to_string(),
    );
    lines.push("                 {instance_name} when not set".to_string());
    lines.push(String::new());
    lines.push("Set:    hcom config terminal kitty".to_string());
    lines.push("Reset:  hcom config terminal default".to_string());

    lines.join("\n")
}

fn print_terminal_info() {
    println!("{}", terminal_help_text(true));
}

fn show_key_info(key: &str) -> i32 {
    let Some(field) = crate::config::user_field(key) else {
        eprintln!("Unknown config key: {key}");
        eprintln!(
            "Valid keys: {}",
            crate::config::user_fields()
                .map(|f| f.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return 1;
    };
    // HCOM_TERMINAL: dynamic help from TERMINAL_PRESETS
    if field.name == "terminal" {
        print_terminal_info();
        return 0;
    }
    let (value, source) = config_get(field.env);
    // If rich help text exists, show it with current value appended
    if let Some(help) = config_help(field.env) {
        println!("{help}");
        println!();
        if value.is_empty() {
            println!("Current value: (not set)");
        } else {
            println!("Current value: {value} [{source}]");
        }
        return 0;
    }
    println!("{}", field.env);
    println!("  Description: {}", field.desc);
    println!("  Type: {}", field.kind.as_str());
    if value.is_empty() {
        println!("  Value: (not set)");
    } else {
        println!("  Value: {value} [{source}]");
    }
    println!("  Set via: hcom config {} <value>", field.name);
    println!("  Or env: export {}=<value>", field.env);
    0
}

/// Handle terminal preset configuration.
fn config_terminal(argv: &[String], setup_mode: bool) -> i32 {
    use crate::shared::terminal_presets::TERMINAL_PRESETS;

    if argv.is_empty() {
        // Show terminal status
        let (current, source) = config_get("HCOM_TERMINAL");
        if current.is_empty() {
            println!("Terminal: (auto-detect)");
        } else {
            println!("Terminal: {current} [{source}]");
        }
        println!("\nAvailable presets:");
        let platform = crate::shared::platform::platform_name();
        for (name, preset) in TERMINAL_PRESETS.iter() {
            let marker = if *name == current { " ← current" } else { "" };
            let availability =
                if preset.platforms.is_empty() || preset.platforms.contains(&platform) {
                    ""
                } else {
                    " (unavailable on this platform)"
                };
            println!("  {name}{availability}{marker}");
        }
        // Include TOML-defined presets not in built-ins
        let toml_path = crate::paths::config_toml_path();
        if let Some(toml_presets) = crate::config::load_toml_presets(&toml_path)
            && let Some(table) = toml_presets.as_table()
        {
            for name in table.keys() {
                if !TERMINAL_PRESETS.iter().any(|(n, _)| *n == name.as_str()) {
                    let marker = if *name == current { " ← current" } else { "" };
                    println!("  {}{}", name, marker);
                }
            }
        }
        println!("\nSet: hcom config terminal <preset>");
        return 0;
    }

    let preset_name = &argv[0];

    if preset_name == "--info" || preset_name == "info" || preset_name == "?" {
        print_terminal_info();
        return 0;
    }

    if preset_name == "default" || preset_name == "auto" {
        match config_unset("HCOM_TERMINAL") {
            Ok(()) => {
                println!("Terminal reset to auto-detect");
                return 0;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
    }

    if argv.len() > 1 || preset_name.contains("{script}") {
        let command = argv.join(" ");
        if !command.contains("{script}") {
            eprintln!("Error: Custom terminal command must contain {{script}}");
            return 1;
        }
        return match config_set("HCOM_TERMINAL", &command) {
            Ok(()) => {
                println!("Terminal set to custom command");
                0
            }
            Err(e) => {
                eprintln!("Error: {e}");
                1
            }
        };
    }

    // Validate preset exists (built-in or user-defined in config.toml)
    let builtin = TERMINAL_PRESETS
        .iter()
        .find(|(name, _)| *name == preset_name.as_str());
    let valid = builtin.is_some()
        || matches!(preset_name.as_str(), "here" | "print")
        || crate::config::is_user_defined_preset(preset_name);

    if !valid {
        let mut available: Vec<&str> = TERMINAL_PRESETS.iter().map(|(name, _)| *name).collect();
        // Include user-defined presets
        let toml_path = crate::paths::config_toml_path();
        let user_names: Vec<String> = crate::config::load_toml_presets(&toml_path)
            .and_then(|p| p.as_table().map(|t| t.keys().cloned().collect()))
            .unwrap_or_default();
        eprintln!("Error: Unknown terminal preset '{preset_name}'");
        let user_refs: Vec<&str> = user_names.iter().map(|s| s.as_str()).collect();
        available.extend(user_refs);
        eprintln!("Available: {}", available.join(", "));
        return 1;
    }
    // config_set refuses a built-in preset for another platform, unless a
    // user preset of the same name overrides it.
    match config_set("HCOM_TERMINAL", preset_name) {
        Ok(()) => {
            println!("Terminal set to: {preset_name}");
            if preset_name.starts_with("kitty") {
                if setup_mode {
                    kitty_setup();
                } else {
                    show_kitty_status(preset_name);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

/// Show kitty remote control socket status after setting a kitty preset.
/// Diagnoses missing socket and hints at --setup if needed.
fn show_kitty_status(preset_name: &str) {
    if let Some(socket) = find_kitty_socket() {
        if preset_name == "kitty" {
            println!(
                "  Socket found ({}) — splits/tabs available",
                socket.display()
            );
        }
        return;
    }

    // No socket — diagnose why
    let conf = match find_kitty_conf() {
        Some(c) => c,
        None => {
            println!("  No kitty.conf found");
            println!("  Run: hcom config terminal kitty --setup");
            return;
        }
    };

    let has_rc = kitty_conf_has(&conf, "allow_remote_control");
    let has_listen = kitty_conf_has(&conf, "listen_on");

    match (has_rc.as_deref(), &has_listen) {
        (Some("yes" | "socket"), Some(_)) => {
            println!("  Config OK but no socket — restart kitty");
        }
        (Some(val), _) if val != "yes" && val != "socket" => {
            println!("  allow_remote_control is '{val}' — needs 'yes' or 'socket'");
            println!("  Edit {} manually, then restart kitty", conf.display());
        }
        _ => {
            println!("  Remote control not configured");
            println!("  Run: hcom config terminal kitty --setup");
        }
    }
}

// ── Kitty Setup (C4) ─────────────────────────────────────────────────────

/// Find kitty.conf path.
fn find_kitty_conf() -> Option<PathBuf> {
    let candidates = [
        dirs::config_dir().map(|d| d.join("kitty/kitty.conf")),
        dirs::home_dir().map(|h| h.join(".config/kitty/kitty.conf")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

/// Find kitty remote control socket.
fn find_kitty_socket() -> Option<PathBuf> {
    let candidates = [PathBuf::from("/tmp/kitty"), PathBuf::from("/tmp/mykitty")];
    candidates.into_iter().find(|p| p.exists())
}

/// Check if kitty.conf contains a specific key (exact match via whitespace split).
fn kitty_conf_has(path: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, |c: char| c.is_whitespace());
        if let (Some(k), Some(v)) = (parts.next(), parts.next())
            && k == key
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Configure kitty for remote control (splits/tabs).
fn kitty_setup() -> i32 {
    if let Some(socket) = find_kitty_socket() {
        println!(
            "Kitty remote control already working ({})",
            socket.display()
        );
        return 0;
    }

    let conf = match find_kitty_conf() {
        Some(c) => c,
        None => {
            eprintln!("Error: Could not find kitty.conf");
            return 1;
        }
    };

    let has_rc = kitty_conf_has(&conf, "allow_remote_control");
    let has_listen = kitty_conf_has(&conf, "listen_on");

    if matches!(has_rc.as_deref(), Some("yes" | "socket")) && has_listen.is_some() {
        println!(
            "Config OK ({}) but no socket — restart kitty",
            conf.display()
        );
        return 0;
    }

    if let Some(ref rc_val) = has_rc
        && rc_val != "yes"
        && rc_val != "socket"
    {
        eprintln!(
            "Error: allow_remote_control is '{rc_val}' in {}",
            conf.display()
        );
        eprintln!("  Change to 'yes' or 'socket', then restart kitty");
        return 1;
    }

    let mut lines_to_add = Vec::new();
    if has_rc.is_none() {
        lines_to_add.push("allow_remote_control yes");
    }
    if has_listen.is_none() {
        lines_to_add.push("listen_on unix:/tmp/kitty");
    }

    match std::fs::OpenOptions::new().append(true).open(&conf) {
        Ok(mut file) => {
            use std::io::Write;
            let _ = writeln!(file, "\n# Added by hcom for remote control (splits/tabs)");
            for line in &lines_to_add {
                let _ = writeln!(file, "{line}");
            }
        }
        Err(e) => {
            eprintln!("Error: Failed to write {}: {e}", conf.display());
            return 1;
        }
    }

    println!("Added to {}:", conf.display());
    for line in &lines_to_add {
        println!("  {line}");
    }
    println!("\nRestart kitty to apply changes");
    0
}

/// Update tool permissions when auto_approve changes.
/// Sync installed hook permissions with `auto_approve` in config.toml. Those
/// permission files are shared like config.toml, so env overrides don't apply.
fn update_auto_approve_permissions() -> bool {
    let enabled = crate::config::load_config_snapshot().core.auto_approve;
    let failures = super::hooks::refresh_installed_hook_permissions(enabled);

    if enabled {
        println!("Auto-approve enabled for safe hcom commands");
    } else {
        println!("Auto-approve disabled - safe hcom commands will require approval");
    }
    for (tool, error) in &failures {
        eprintln!("Failed to update {tool} auto-approve permissions: {error}");
    }
    failures.is_empty()
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unique_instance_prefix_requires_one_literal_match() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        for name in ["luna", "lune", "a_one", "axone", "a%one", "a!one"] {
            db.conn()
                .execute(
                    "INSERT INTO instances (name, created_at) VALUES (?, 1)",
                    [name],
                )
                .unwrap();
        }
        assert_eq!(
            unique_instance_prefix(&db, "luna").unwrap().as_deref(),
            Some("luna")
        );
        let error = unique_instance_prefix(&db, "lun").unwrap_err();
        assert!(error.contains("luna, lune"));
        assert_eq!(
            unique_instance_prefix(&db, "a_").unwrap().as_deref(),
            Some("a_one")
        );
        assert_eq!(
            unique_instance_prefix(&db, "a%").unwrap().as_deref(),
            Some("a%one")
        );
        assert_eq!(
            unique_instance_prefix(&db, "a!").unwrap().as_deref(),
            Some("a!one")
        );
        assert_eq!(unique_instance_prefix(&db, "absent").unwrap(), None);
        assert_eq!(
            config_instance(&db, "lun", &["tag".into(), "changed".into()], None, false),
            1
        );
        assert_eq!(
            db.get_instance_full("luna")
                .unwrap()
                .unwrap()
                .tag
                .as_deref(),
            None
        );
    }

    #[test]
    fn config_set_reads_after_acquiring_sibling_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config_set_at_path(&path, "tag", Some("before")).unwrap();
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open(path.with_extension("toml.lock"))
            .unwrap();
        crate::sys::fs::lock_exclusive(&lock).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let path = &path;
            scope.spawn(move || {
                tx.send(config_set_at_path(path, "timeout", Some("600")))
                    .unwrap();
            });
            let blocked = rx.recv_timeout(std::time::Duration::from_millis(100));
            // Simulate another writer committing while it owns the lock.
            crate::config::write_config_toml_path(path, "[launch]\ntag = 'under-lock'\n").unwrap();
            drop(lock);
            assert!(matches!(
                blocked,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
        });
        let content = std::fs::read_to_string(&path).unwrap();
        let doc: toml_edit::DocumentMut = content.parse().unwrap();
        assert_eq!(doc["launch"]["tag"].as_str(), Some("under-lock"));
        assert_eq!(doc["preferences"]["timeout"].as_integer(), Some(600));
    }

    #[test]
    fn test_normalize_key() {
        assert_eq!(normalize_key("tag"), "HCOM_TAG");
        assert_eq!(normalize_key("HCOM_TAG"), "HCOM_TAG");
        assert_eq!(normalize_key("terminal"), "HCOM_TERMINAL");
        assert_eq!(normalize_key("hcom_timeout"), "HCOM_TIMEOUT");
    }

    #[test]
    fn test_config_args_json_flag() {
        use clap::Parser;
        let args = ConfigArgs::try_parse_from(["config", "--json", "key"]).unwrap();
        assert!(args.json);
        assert_eq!(args.key, Some("key".to_string()));
    }

    #[test]
    fn test_config_args_info_not_swallowed() {
        use clap::Parser;
        // "hcom config tag --info" should set info=true, not treat --info as value
        let args = ConfigArgs::try_parse_from(["config", "tag", "--info"]).unwrap();
        assert!(args.info);
        assert_eq!(args.key, Some("tag".to_string()));
        assert!(args.value.is_none());
    }

    #[test]
    fn test_config_args_set_value() {
        use clap::Parser;
        let args = ConfigArgs::try_parse_from(["config", "tag", "myvalue"]).unwrap();
        assert_eq!(args.key, Some("tag".to_string()));
        assert_eq!(args.value.as_deref(), Some("myvalue"));
    }

    #[test]
    fn test_config_args_instance() {
        use clap::Parser;
        let args = ConfigArgs::try_parse_from(["config", "-i", "self", "tag", "mytag"]).unwrap();
        assert_eq!(args.instance, Some("self".to_string()));
        assert_eq!(args.key, Some("tag".to_string()));
        assert_eq!(args.value.as_deref(), Some("mytag"));
    }

    #[test]
    fn test_config_args_dev_root_unset() {
        use clap::Parser;
        let args = ConfigArgs::try_parse_from(["config", "dev_root", "--unset"]).unwrap();
        assert_eq!(args.key, Some("dev_root".to_string()));
        assert!(args.unset);
        assert!(args.value.is_none());
    }

    #[test]
    fn test_config_args_hyphen_value() {
        use clap::Parser;
        // "hcom config codex_args '--model o3'" — quoted so shell passes as one token
        let args = ConfigArgs::try_parse_from(["config", "codex_args", "--model o3"]).unwrap();
        assert_eq!(args.key, Some("codex_args".to_string()));
        assert_eq!(args.value.as_deref(), Some("--model o3"));
    }

    #[test]
    fn test_config_args_flags_after_value_not_swallowed() {
        use clap::Parser;
        // "hcom config tag myval --json" should NOT swallow --json as value
        let args = ConfigArgs::try_parse_from(["config", "tag", "myval", "--json"]).unwrap();
        assert_eq!(args.key, Some("tag".to_string()));
        assert_eq!(args.value.as_deref(), Some("myval"));
        assert!(args.json);
    }

    #[test]
    fn test_instance_key_validation() {
        assert!(INSTANCE_KEYS.iter().any(|(k, _)| *k == "tag"));
        assert!(INSTANCE_KEYS.iter().any(|(k, _)| *k == "timeout"));
        assert!(INSTANCE_KEYS.iter().any(|(k, _)| *k == "hints"));
        assert!(INSTANCE_KEYS.iter().any(|(k, _)| *k == "subagent_timeout"));
        assert!(!INSTANCE_KEYS.iter().any(|(k, _)| *k == "invalid"));
    }

    #[test]
    fn test_config_key_info_exists() {
        // All keys should have descriptions
        for field in crate::config::user_fields() {
            assert!(!field.desc.is_empty(), "{}", field.name);
        }
        assert!(crate::config::user_field("relay_psk").is_none());
        assert!(crate::config::user_field("HCOM_GROK_ARGS").is_some());
    }

    #[test]
    fn test_config_dev_root_set_get_unset() {
        use clap::Parser;

        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();

        let set_args = ConfigArgs::try_parse_from(["config", "dev_root", "/tmp/worktree"]).unwrap();
        assert_eq!(cmd_config(&db, &set_args, None), 0);
        assert_eq!(
            db.kv_get(DEV_ROOT_KV_KEY).unwrap(),
            Some("/tmp/worktree".to_string())
        );

        let get_args = ConfigArgs::try_parse_from(["config", "dev_root"]).unwrap();
        assert_eq!(cmd_config(&db, &get_args, None), 0);

        let unset_args = ConfigArgs::try_parse_from(["config", "dev_root", "--unset"]).unwrap();
        assert_eq!(cmd_config(&db, &unset_args, None), 0);
        assert_eq!(db.kv_get(DEV_ROOT_KV_KEY).unwrap(), None);
    }

    #[test]
    fn test_config_set_accepts_unknown_upstream_args() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[launch.claude]\nargs = \"--model keep\"\n").unwrap();

        config_set_at_path(&path, "HCOM_CLAUDE_ARGS", Some("--future-upstream-flag")).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("--future-upstream-flag")
        );
    }

    #[test]
    fn test_relay_share_account_is_set_through_config() {
        assert!(!RELAY_MANAGED_KEYS.contains(&normalize_key("relay_share_account").as_str()));
        for key in [
            "relay",
            "relay_id",
            "relay_token",
            "relay_enabled",
            "relay_psk",
        ] {
            assert!(
                RELAY_MANAGED_KEYS.contains(&normalize_key(key).as_str()),
                "{key}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config_set_at_path(&path, "HCOM_RELAY_SHARE_ACCOUNT", Some("1")).unwrap();
        let parsed: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(parsed["relay"]["share_account"].as_bool(), Some(true));
    }

    #[test]
    fn test_config_set_saves_valid_args() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        config_set_at_path(&path, "HCOM_PI_ARGS", Some("--model safe-model")).unwrap();

        let parsed: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
        assert_eq!(
            parsed["launch"]["pi"]["args"].as_str(),
            Some("--model safe-model")
        );
        assert!(parsed.get("pi").is_none(), "no stray top-level [pi]");
    }

    #[test]
    fn old_delivery_keys_are_dropped_and_the_rest_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let old = "# mine\n[launch.claude]\nargs = \"--x\"\ndelivery = \"inject\"\n\n\
                   [launch.copilot]\ndelivery = \"inject\"\nidle_delivery = \"inject\"\n";
        std::fs::write(&path, old).unwrap();
        drop_old_delivery_keys_at(&path);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("\ndelivery"), "{content}");
        assert!(
            content.contains("# mine") && content.contains("args = \"--x\""),
            "{content}"
        );
        assert!(content.contains("idle_delivery = \"inject\""), "{content}");

        let missing = dir.path().join("none.toml");
        drop_old_delivery_keys_at(&missing);
        assert!(!missing.exists());
    }

    #[test]
    fn test_config_set_copilot_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let read = || -> toml::Table { std::fs::read_to_string(&path).unwrap().parse().unwrap() };

        config_set_at_path(&path, "HCOM_COPILOT_DELIVERY", Some("inject")).unwrap();
        assert_eq!(
            read()["launch"]["copilot"]["idle_delivery"].as_str(),
            Some("inject")
        );
        assert!(config_set_at_path(&path, "HCOM_COPILOT_DELIVERY", Some("pty")).is_err());

        // --unset writes "", which removes the key (back to the default).
        config_set_at_path(&path, "HCOM_COPILOT_DELIVERY", None).unwrap();
        assert!(read()["launch"]["copilot"].get("idle_delivery").is_none());
    }

    #[test]
    fn config_set_refuses_what_the_loader_would_reject() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (key, value) in [
            ("HCOM_TIMEOUT", "999999"),
            ("HCOM_TIMEOUT", "soon"),
            ("HCOM_SUBAGENT_TIMEOUT", "0"),
            ("HCOM_TAG", "a b"),
            ("HCOM_AUTO_APPROVE", "maybe"),
            ("HCOM_AUTO_SUBSCRIBE", "a b"),
            ("HCOM_TITLE_MODE", "loud"),
            ("HCOM_TERMINAL", "kitty; rm -rf ~"),
            ("HCOM_RELAY_PSK", "secret"),
            ("HCOM_NOT_A_KEY", "1"),
        ] {
            assert!(
                config_set_at_path(&path, key, Some(value)).is_err(),
                "{key}={value} must be refused"
            );
        }
        assert!(!path.exists() || std::fs::read_to_string(&path).unwrap().trim().is_empty());
    }

    #[test]
    fn config_set_writes_the_field_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let read = || -> toml::Table { std::fs::read_to_string(&path).unwrap().parse().unwrap() };

        config_set_at_path(&path, "hints", Some("true")).unwrap();
        config_set_at_path(&path, "tag", Some("123")).unwrap();
        config_set_at_path(&path, "timeout", Some("600")).unwrap();
        config_set_at_path(&path, "auto_approve", Some("no")).unwrap();
        config_set_at_path(&path, "grok_args", Some("--model x")).unwrap();
        let t = read();
        assert_eq!(t["launch"]["hints"].as_str(), Some("true"));
        assert_eq!(t["launch"]["tag"].as_str(), Some("123"));
        assert_eq!(t["preferences"]["timeout"].as_integer(), Some(600));
        assert_eq!(t["preferences"]["auto_approve"].as_bool(), Some(false));
        assert_eq!(t["launch"]["grok"]["args"].as_str(), Some("--model x"));

        // Explicit "" is a value (no auto-subscribe presets); unset removes the key.
        config_set_at_path(&path, "auto_subscribe", Some("")).unwrap();
        assert_eq!(read()["launch"]["auto_subscribe"].as_str(), Some(""));
        config_set_at_path(&path, "auto_subscribe", None).unwrap();
        assert!(read()["launch"].get("auto_subscribe").is_none());
        assert!(config_set_at_path(&path, "timeout", Some("")).is_err());
    }

    #[test]
    #[serial_test::serial] // resets the process-wide Config other tests read
    #[cfg(target_os = "macos")]
    fn test_terminal_help_text_lists_cmux_as_managed() {
        crate::config::Config::reset();
        crate::config::Config::init();
        let help = terminal_help_text(false);
        let managed = help
            .split("Other (opens window only):")
            .next()
            .expect("managed section should exist");
        let other = help
            .split("Other (opens window only):")
            .nth(1)
            .expect("other section should exist");

        assert!(managed.contains("cmux"));
        assert!(!other.contains("cmux"));
    }

    #[test]
    fn test_terminal_help_text_documents_new_placeholders() {
        // If you add a placeholder to substitute_open_argv, document it.
        let help = terminal_help_text(false);
        for placeholder in ["{instance_name}", "{tool}", "{cwd}", "{pane_title}"] {
            assert!(
                help.contains(placeholder),
                "missing {placeholder} placeholder docs",
            );
        }
    }

    #[test]
    #[serial_test::serial] // "default" reads HCOM_TIMEOUT, which serial tests set
    fn test_config_instance_set_timeout_default_resets() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, wait_timeout) VALUES (?1, ?2, ?3)",
                rusqlite::params!["luna", crate::shared::time::now_epoch_f64(), 15],
            )
            .unwrap();

        config_instance_set(&db, "luna", "timeout", "default").unwrap();
        let timeout: i64 = db
            .conn()
            .query_row(
                "SELECT wait_timeout FROM instances WHERE name = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // The configured default, not a literal: a developer's shell may export HCOM_TIMEOUT.
        let expected = crate::config::HcomConfig::load(None).timeout;
        assert_ne!(expected, 15);
        assert_eq!(timeout, expected);
    }

    #[test]
    fn test_config_instance_set_empty_hints_clears() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, hints) VALUES (?1, ?2, ?3)",
                rusqlite::params!["luna", crate::shared::time::now_epoch_f64(), "keep me"],
            )
            .unwrap();

        config_instance_set(&db, "luna", "hints", "").unwrap();
        let hints: Option<String> = db
            .conn()
            .query_row(
                "SELECT hints FROM instances WHERE name = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hints, None);
    }

    #[test]
    fn test_config_instance_set_subagent_timeout_default_clears() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, subagent_timeout) VALUES (?1, ?2, ?3)",
                rusqlite::params!["luna", crate::shared::time::now_epoch_f64(), 20],
            )
            .unwrap();

        config_instance_set(&db, "luna", "subagent_timeout", "default").unwrap();
        let timeout: Option<i64> = db
            .conn()
            .query_row(
                "SELECT subagent_timeout FROM instances WHERE name = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(timeout, None);
    }

    #[test]
    fn test_render_config_instance_get_scalar_matches_local_style() {
        let rendered =
            render_config_instance_get(&serde_json::json!({"value": 42}), Some("timeout"), false);
        assert_eq!(rendered, "42");
    }

    #[test]
    fn test_render_config_instance_get_null_renders_empty_string() {
        let rendered = render_config_instance_get(
            &serde_json::json!({"value": serde_json::Value::Null}),
            Some("timeout"),
            false,
        );
        assert_eq!(rendered, "");
    }

    #[test]
    fn test_render_config_instance_get_full_output_contract() {
        let rendered = render_config_instance_get(
            &serde_json::json!({
                "name": "team-luna",
                "tag": "team",
                "timeout": 120,
                "hints": "ship it",
                "subagent_timeout": 45,
            }),
            None,
            false,
        );
        assert_eq!(
            rendered,
            "Agent: team-luna\n  tag: team\n  timeout: 120s\n  hints: ship it\n  subagent_timeout: 45s"
        );
    }

    #[test]
    fn test_render_config_instance_get_full_output_uses_empty_state_labels() {
        let rendered = render_config_instance_get(
            &serde_json::json!({
                "name": "luna",
                "tag": "",
                "timeout": 86400,
                "hints": "",
                "subagent_timeout": serde_json::Value::Null,
            }),
            None,
            false,
        );
        assert_eq!(
            rendered,
            "Agent: luna\n  tag: (none)\n  timeout: 86400s\n  hints: (none)\n  subagent_timeout: (default)"
        );
    }

    #[test]
    fn test_render_config_instance_get_json_mode_passthrough() {
        let rendered = render_config_instance_get(
            &serde_json::json!({"value": 42, "name": "luna"}),
            Some("timeout"),
            true,
        );
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["value"], 42);
        assert_eq!(parsed["name"], "luna");
    }

    #[test]
    fn test_config_instance_get_full_output_uses_display_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, tag) VALUES (?1, ?2, ?3)",
                rusqlite::params!["luna", crate::shared::time::now_epoch_f64(), "team"],
            )
            .unwrap();

        let config = config_instance_get(&db, "team-luna", None).unwrap();
        // Stable shape: name is the base name, full_name is tag-prefixed.
        assert_eq!(config["name"], "luna");
        assert_eq!(config["full_name"], "team-luna");
    }

    #[test]
    fn test_config_instance_get_timeout_uses_default_value() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at) VALUES (?1, ?2)",
                rusqlite::params!["luna", crate::shared::time::now_epoch_f64()],
            )
            .unwrap();

        let config = config_instance_get(&db, "luna", Some("timeout")).unwrap();
        assert_eq!(config["value"], 86400);
    }

    #[test]
    fn test_config_instance_get_full_output_schema() {
        // Stable JSON contract for `hcom config -i <name> --json`. Keep the
        // local and remote (RPC) paths emitting identical shapes:
        //   - name: base name (no tag prefix)
        //   - full_name: tag-prefixed display name
        //   - tag / hints: null when empty or unset (not "")
        //   - timeout: null when the row has no explicit value (registration
        //     paths write the resolved HCOM_TIMEOUT explicitly; a bare INSERT
        //     that skips the column, as here, has nothing to fall back on)
        //   - subagent_timeout: null when unset
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, tag) VALUES (?1, ?2, ?3)",
                rusqlite::params!["luna", crate::shared::time::now_epoch_f64(), "team"],
            )
            .unwrap();

        let config = config_instance_get(&db, "team-luna", None).unwrap();
        assert_eq!(config["name"], "luna");
        assert_eq!(config["full_name"], "team-luna");
        assert_eq!(config["tag"], "team");
        assert!(
            config["timeout"].is_null(),
            "unset timeout must serialize as null (no schema default; issue #71)"
        );
        assert!(
            config["hints"].is_null(),
            "unset hints must serialize as null"
        );
        assert!(
            config["subagent_timeout"].is_null(),
            "unset subagent_timeout must serialize as null"
        );
    }

    #[test]
    fn test_config_instance_get_full_output_nullifies_empty_strings_and_null_timeout() {
        // Scripts consume these as `tag in (null, "<value>")` so empty
        // strings must be normalized to null. Also covers the legacy case
        // where wait_timeout was explicitly NULL before the schema default
        // existed.
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, tag, hints, wait_timeout) \
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                rusqlite::params!["luna", crate::shared::time::now_epoch_f64(), "", ""],
            )
            .unwrap();

        let config = config_instance_get(&db, "luna", None).unwrap();
        assert!(config["tag"].is_null(), "empty tag must serialize as null");
        assert!(
            config["hints"].is_null(),
            "empty hints must serialize as null"
        );
        assert!(
            config["timeout"].is_null(),
            "explicit NULL wait_timeout must serialize as null"
        );
    }

    #[test]
    fn test_config_instance_get_subagent_timeout_unset_returns_null_value() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at) VALUES (?1, ?2)",
                rusqlite::params!["luna", crate::shared::time::now_epoch_f64()],
            )
            .unwrap();

        let config = config_instance_get(&db, "luna", Some("subagent_timeout")).unwrap();
        assert_eq!(config["value"], serde_json::Value::Null);
    }

    #[test]
    fn test_render_config_instance_set_feedback_timeout_reset() {
        let rendered = render_config_instance_set_feedback("luna", "timeout", "default");
        assert_eq!(rendered, "Reset timeout for luna");
    }

    #[test]
    fn test_render_config_instance_set_feedback_subagent_timeout_contract() {
        let rendered = render_config_instance_set_feedback("team-luna", "subagent_timeout", "30");
        assert_eq!(rendered, "Set subagent_timeout for team-luna: 30s");
    }
}
