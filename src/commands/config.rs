//! `hcom config` command — view and edit configuration.
//!
//!
//! Supports: show all, get/set single key, --json, --edit, --reset,
//! per-instance config (-i), terminal preset management.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::db::DEV_ROOT_KV_KEY;
use crate::db::HcomDb;
use crate::instance_lifecycle;
use crate::instances;
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
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Open config in editor
    #[arg(long)]
    pub edit: bool,
    /// Unset value (supported for dev_root)
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

// ── Config Key Registry ──────────────────────────────────────────────────

/// Known config keys with descriptions and types.
pub const CONFIG_KEYS: &[(&str, &str, &str)] = &[
    ("HCOM_TAG", "Group tag for launched instances", "string"),
    ("HCOM_HINTS", "Text injected with all messages", "string"),
    (
        "HCOM_NOTES",
        "One-time notes appended at bootstrap",
        "string",
    ),
    (
        "HCOM_TIMEOUT",
        "Idle timeout in seconds (default: 86400)",
        "integer",
    ),
    (
        "HCOM_SUBAGENT_TIMEOUT",
        "Timeout for Claude subagents in seconds (default: 30)",
        "integer",
    ),
    (
        "HCOM_CLAUDE_ARGS",
        "Default args for claude on launch",
        "string",
    ),
    (
        "HCOM_GEMINI_ARGS",
        "Default args for gemini on launch",
        "string",
    ),
    (
        "HCOM_CODEX_ARGS",
        "Default args for codex on launch",
        "string",
    ),
    (
        "HCOM_OPENCODE_ARGS",
        "Default args for opencode on launch",
        "string",
    ),
    (
        "HCOM_CODEX_SANDBOX_MODE",
        "Codex permission profile (workspace | untrusted | danger-full-access | none)",
        "string",
    ),
    (
        "HCOM_GEMINI_SYSTEM_PROMPT",
        "System prompt for gemini on launch",
        "string",
    ),
    (
        "HCOM_CODEX_SYSTEM_PROMPT",
        "System prompt for codex on launch",
        "string",
    ),
    (
        "HCOM_TERMINAL",
        "Terminal preset for spawning agent panes",
        "string",
    ),
    (
        "HCOM_AUTO_APPROVE",
        "Auto-approve safe hcom commands (true/false)",
        "boolean",
    ),
    (
        "HCOM_AUTO_SUBSCRIBE",
        "Auto-subscribe event presets (comma-separated)",
        "string",
    ),
    (
        "HCOM_NAME_EXPORT",
        "Export instance name to custom env var",
        "string",
    ),
    (
        "HCOM_RELAY",
        "Relay MQTT broker URL (config file only)",
        "string",
    ),
    (
        "HCOM_RELAY_ID",
        "Relay group identifier (config file only)",
        "string",
    ),
    (
        "HCOM_RELAY_TOKEN",
        "Relay authentication token (config file only)",
        "string",
    ),
    (
        "HCOM_RELAY_ENABLED",
        "Enable relay sync (config file only)",
        "boolean",
    ),
];

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

// ── Flag Parsing ─────────────────────────────────────────────────────────

// ── TOML Key Mapping ────────────────────

/// Maps HCOM_ field name (lowercase, no prefix) to nested TOML dotted path.
fn toml_path_for_key(field_name: &str) -> Option<&'static str> {
    match field_name {
        "terminal" => Some("terminal.active"),
        "tag" => Some("launch.tag"),
        "hints" => Some("launch.hints"),
        "notes" => Some("launch.notes"),
        "subagent_timeout" => Some("launch.subagent_timeout"),
        "auto_subscribe" => Some("launch.auto_subscribe"),
        "claude_args" => Some("launch.claude.args"),
        "gemini_args" => Some("launch.gemini.args"),
        "gemini_system_prompt" => Some("launch.gemini.system_prompt"),
        "codex_args" => Some("launch.codex.args"),
        "codex_sandbox_mode" => Some("launch.codex.sandbox_mode"),
        "codex_system_prompt" => Some("launch.codex.system_prompt"),
        "opencode_args" => Some("launch.opencode.args"),
        "relay" => Some("relay.url"),
        "relay_id" => Some("relay.id"),
        "relay_token" => Some("relay.token"),
        "relay_enabled" => Some("relay.enabled"),
        "timeout" => Some("preferences.timeout"),
        "auto_approve" => Some("preferences.auto_approve"),
        "name_export" => Some("preferences.name_export"),
        _ => None,
    }
}

// ── Config File Operations ───────────────────────────────────────────────

fn config_path() -> PathBuf {
    crate::paths::hcom_dir().join("config.toml")
}

/// Load raw TOML content from config file.
fn load_config_content() -> String {
    std::fs::read_to_string(config_path()).unwrap_or_default()
}

/// Set a value at a nested TOML dotted path (e.g. "preferences.timeout") using toml_edit.
fn set_nested_toml(doc: &mut toml_edit::DocumentMut, dotted_path: &str, value: &str) {
    let parts: Vec<&str> = dotted_path.split('.').collect();

    // Ensure intermediate tables exist
    for part in &parts[..parts.len() - 1] {
        if doc.get(part).is_none() || !doc[part].is_table_like() {
            doc[part] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        // For deeper nesting we need to navigate into the table via the Item API
    }

    // For 2-level paths (e.g. "preferences.timeout")
    if parts.len() == 2 {
        if value.is_empty() {
            if let Some(tbl) = doc[parts[0]].as_table_like_mut() {
                tbl.remove(parts[1]);
            }
        } else if let Ok(n) = value.parse::<i64>() {
            doc[parts[0]][parts[1]] = toml_edit::value(n);
        } else if value == "true" || value == "false" {
            doc[parts[0]][parts[1]] = toml_edit::value(value == "true");
        } else {
            doc[parts[0]][parts[1]] = toml_edit::value(value);
        }
    } else if parts.len() == 3 {
        // For 3-level paths (e.g. "launch.claude.args")
        // Ensure second-level table exists
        if doc[parts[0]].get(parts[1]).is_none() || !doc[parts[0]][parts[1]].is_table_like() {
            doc[parts[0]][parts[1]] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        if value.is_empty() {
            if let Some(tbl) = doc[parts[0]][parts[1]].as_table_like_mut() {
                tbl.remove(parts[2]);
            }
        } else if let Ok(n) = value.parse::<i64>() {
            doc[parts[0]][parts[1]][parts[2]] = toml_edit::value(n);
        } else if value == "true" || value == "false" {
            doc[parts[0]][parts[1]][parts[2]] = toml_edit::value(value == "true");
        } else {
            doc[parts[0]][parts[1]][parts[2]] = toml_edit::value(value);
        }
    }
}

/// Get a value from a nested TOML dotted path.
fn get_nested_toml(table: &toml::Table, dotted_path: &str) -> Option<toml::Value> {
    let parts: Vec<&str> = dotted_path.split('.').collect();
    let mut current: &toml::Value = &toml::Value::Table(table.clone());

    for part in &parts {
        match current {
            toml::Value::Table(t) => {
                current = t.get(*part)?;
            }
            _ => return None,
        }
    }
    Some(current.clone())
}

/// Set a config key in the TOML file (preserving comments via toml_edit).
/// Uses nested TOML paths
pub fn config_set(key: &str, value: &str) -> Result<(), String> {
    let path = config_path();
    let content = std::fs::read_to_string(&path).unwrap_or_default();

    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("Failed to parse config.toml: {e}"))?;

    // Map HCOM_KEY to field name, then to nested TOML path
    let field_name = key.strip_prefix("HCOM_").unwrap_or(key).to_lowercase();

    if field_name == "codex_sandbox_mode" && !value.is_empty() {
        let normalized = if value == "full-auto" {
            "danger-full-access"
        } else {
            value
        };
        if !crate::config::VALID_SANDBOX_MODES.contains(&normalized) {
            return Err(format!(
                "codex_sandbox_mode must be one of: {}. Got '{value}'",
                crate::config::VALID_SANDBOX_MODES.join(", ")
            ));
        }
    }

    if let Some(dotted_path) = toml_path_for_key(&field_name) {
        set_nested_toml(&mut doc, dotted_path, value);
    } else {
        // Unknown key — fall back to flat key for forward compat
        if value.is_empty() {
            doc.remove(&field_name);
        } else if let Ok(n) = value.parse::<i64>() {
            doc[&field_name] = toml_edit::value(n);
        } else if value == "true" || value == "false" {
            doc[&field_name] = toml_edit::value(value == "true");
        } else {
            doc[&field_name] = toml_edit::value(value);
        }
    }

    crate::config::write_config_toml_path(&path, &doc.to_string())
        .map_err(|e| format!("Failed to write config.toml: {e}"))?;

    Ok(())
}

/// Get a config value (checks env var, then TOML nested path, then default).
pub fn config_get(key: &str) -> (String, &'static str) {
    // Check env var first
    if let Ok(val) = std::env::var(key) {
        return (val, "env");
    }

    // Map to field name and nested TOML path
    let field_name = key.strip_prefix("HCOM_").unwrap_or(key).to_lowercase();

    let content = load_config_content();
    if let Ok(table) = content.parse::<toml::Table>() {
        // Try nested path first
        if let Some(dotted_path) = toml_path_for_key(&field_name) {
            if let Some(val) = get_nested_toml(&table, dotted_path) {
                let val_str = match &val {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Integer(n) => n.to_string(),
                    toml::Value::Boolean(b) => b.to_string(),
                    toml::Value::Float(f) => f.to_string(),
                    other => other.to_string(),
                };
                return (val_str, "toml");
            }
        }
        // Fallback: try flat key (for legacy configs) — skip non-scalar values
        // (e.g. table.get("terminal") returns the whole [terminal] section when
        // terminal.active is not set; returning that would be a display bug)
        if let Some(val) = table.get(&field_name) {
            let val_str = match val {
                toml::Value::String(s) => s.clone(),
                toml::Value::Integer(n) => n.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                toml::Value::Float(f) => f.to_string(),
                _ => return ("".to_string(), "toml"), // table/array — treat as unset
            };
            return (val_str, "toml");
        }
    }

    // Default
    let default = match key {
        "HCOM_TIMEOUT" => "86400",
        "HCOM_SUBAGENT_TIMEOUT" => "30",
        "HCOM_AUTO_APPROVE" => "true",
        _ => "",
    };
    (default.to_string(), "default")
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
        instances::resolve_display_name(db, instance_arg)
            .unwrap_or_else(|| instance_arg.to_string())
    };

    // Verify instance exists
    let instance = match db.get_instance_full(&name) {
        Ok(Some(inst)) => inst,
        Ok(None) => {
            // Try prefix match
            match db.conn().query_row(
                "SELECT name FROM instances WHERE name LIKE ? LIMIT 1",
                rusqlite::params![format!("{name}%")],
                |row| row.get::<_, String>(0),
            ) {
                Ok(matched) => match db.get_instance_full(&matched) {
                    Ok(Some(inst)) => inst,
                    _ => {
                        eprintln!("Error: Agent '{name}' not found");
                        return 1;
                    }
                },
                Err(_) => {
                    eprintln!("Error: Agent '{name}' not found");
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
        let full_name = crate::instances::get_full_name(&instance);
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
        let current = match key {
            "tag" => serde_json::json!({"value": instance.tag.as_deref().unwrap_or("")}),
            "timeout" => serde_json::json!({"value": instance.wait_timeout.unwrap_or(86400)}),
            "hints" => serde_json::json!({"value": instance.hints.as_deref().unwrap_or("")}),
            "subagent_timeout" => match instance.subagent_timeout {
                Some(value) => serde_json::json!({"value": value}),
                None => serde_json::json!({"value": serde_json::Value::Null}),
            },
            _ => serde_json::json!({"value": ""}),
        };
        println!(
            "{}",
            render_config_instance_get(&current, Some(key), json_mode)
        );
        return 0;
    };

    // Set value
    match key {
        "tag" => {
            let tag = if value.is_empty() { "" } else { value };
            // Validate: alphanumeric, hyphens, underscores only
            if !tag.is_empty()
                && !tag
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
            {
                eprintln!("Error: Tag must be alphanumeric (hyphens and underscores allowed)");
                return 1;
            }
            let _ = db.conn().execute(
                "UPDATE instances SET tag = ? WHERE name = ?",
                rusqlite::params![tag, inst_name],
            );
            println!(
                "{}",
                render_config_instance_set_feedback(inst_name, "tag", tag)
            );
            // Notify for display update
            instance_lifecycle::notify_all_instances(db);
        }
        "timeout" => {
            if value.is_empty() || value.eq_ignore_ascii_case("default") {
                let _ = db.conn().execute(
                    "UPDATE instances SET wait_timeout = 86400 WHERE name = ?",
                    rusqlite::params![inst_name],
                );
                println!(
                    "{}",
                    render_config_instance_set_feedback(inst_name, "timeout", value)
                );
            } else {
                match value.parse::<i64>() {
                    Ok(secs) if secs > 0 => {
                        let _ = db.conn().execute(
                            "UPDATE instances SET wait_timeout = ? WHERE name = ?",
                            rusqlite::params![secs, inst_name],
                        );
                        println!(
                            "{}",
                            render_config_instance_set_feedback(
                                inst_name,
                                "timeout",
                                &secs.to_string()
                            )
                        );
                    }
                    _ => {
                        eprintln!("Error: timeout must be a positive integer (seconds)");
                        return 1;
                    }
                }
            }
        }
        "hints" => {
            let mut updates = serde_json::Map::new();
            if value.is_empty() {
                updates.insert("hints".into(), serde_json::Value::Null);
                instances::update_instance_position(db, inst_name, &updates);
            } else {
                updates.insert("hints".into(), serde_json::json!(value));
                instances::update_instance_position(db, inst_name, &updates);
            }
            println!(
                "{}",
                render_config_instance_set_feedback(inst_name, "hints", value)
            );
        }
        "subagent_timeout" => {
            let mut updates = serde_json::Map::new();
            if value.is_empty() || value.eq_ignore_ascii_case("default") {
                updates.insert("subagent_timeout".into(), serde_json::Value::Null);
                instances::update_instance_position(db, inst_name, &updates);
                println!(
                    "{}",
                    render_config_instance_set_feedback(inst_name, "subagent_timeout", value)
                );
            } else {
                match value.parse::<i64>() {
                    Ok(secs) if secs > 0 => {
                        updates.insert("subagent_timeout".into(), serde_json::json!(secs));
                        instances::update_instance_position(db, inst_name, &updates);
                        println!(
                            "{}",
                            render_config_instance_set_feedback(
                                inst_name,
                                "subagent_timeout",
                                &secs.to_string()
                            )
                        );
                    }
                    _ => {
                        eprintln!("Error: subagent_timeout must be a positive integer (seconds)");
                        return 1;
                    }
                }
            }
        }
        _ => {
            eprintln!("Error: Unknown key '{key}'");
            return 1;
        }
    }

    // C4 fix: push config changes to relay
    trigger_relay_push();

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
    let name = instances::resolve_display_name(db, instance_arg)
        .unwrap_or_else(|| instance_arg.to_string());
    let instance = db
        .get_instance_full(&name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Agent '{}' not found", name))?;

    Ok(match key {
        None => {
            let full_name = crate::instances::get_full_name(&instance);
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
    let name = instances::resolve_display_name(db, instance_arg)
        .unwrap_or_else(|| instance_arg.to_string());
    let instance = db
        .get_instance_full(&name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Agent '{}' not found", name))?;
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
            if value.is_empty() || value.eq_ignore_ascii_case("default") {
                db.conn()
                    .execute(
                        "UPDATE instances SET wait_timeout = 86400 WHERE name = ?",
                        rusqlite::params![inst_name],
                    )
                    .map_err(|e| e.to_string())?;
            } else {
                let secs = value
                    .parse::<i64>()
                    .map_err(|_| "timeout must be a positive integer (seconds)".to_string())?;
                if secs <= 0 {
                    return Err("timeout must be a positive integer (seconds)".to_string());
                }
                db.conn()
                    .execute(
                        "UPDATE instances SET wait_timeout = ? WHERE name = ?",
                        rusqlite::params![secs, inst_name],
                    )
                    .map_err(|e| e.to_string())?;
            }
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

    instance_lifecycle::notify_all_instances(db);
    Ok(serde_json::json!({"instance": inst_name, "field": key, "value": value}))
}

// ── Main Entry Point ─────────────────────────────────────────────────────

/// Main entry point for `hcom config` command.
pub fn cmd_config(db: &HcomDb, args: &ConfigArgs, ctx: Option<&CommandContext>) -> i32 {
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
        let resolved =
            instances::resolve_display_name(db, inst).unwrap_or_else(|| inst.to_string());
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
        if !path.exists() {
            let _ = std::fs::write(&path, "# hcom configuration\n");
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

    let key_arg = &argv[0];

    if key_arg == "dev_root" {
        return config_dev_root(
            db,
            argv.get(1).map(|s| s.as_str()),
            unset_mode,
            json_mode,
            info_mode,
        );
    }

    if unset_mode {
        eprintln!("Error: --unset is only supported with 'hcom config dev_root'");
        return 1;
    }

    // Terminal subcommand
    if key_arg == "terminal" {
        if info_mode {
            print_terminal_info();
            return 0;
        }
        return config_terminal(&argv[1..], setup_mode);
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

    // Set mode: config KEY VALUE
    if argv.len() >= 2 && !wants_info {
        // Join all remaining args with spaces
        let value = argv[1..].join(" ");
        match config_set(&key, &value) {
            Ok(()) => {
                println!("Set {key} = {value}");

                // Side effect: auto_approve changes must update tool permissions
                if key == "HCOM_AUTO_APPROVE" {
                    update_auto_approve_permissions(&value);
                }

                return 0;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        }
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

    0
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

    if json_mode {
        let mut result = serde_json::Map::new();
        for (key, _, _) in CONFIG_KEYS {
            let (value, _source) = config_get(key);
            // {KEY: value} — mask relay token
            let display = if *key == "HCOM_RELAY_TOKEN" && value.len() > 4 {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(result)).unwrap_or_default()
        );
    } else {
        println!("hcom configuration ({})\n", config_path().display());
        println!("hcom Settings:");
        for (key, _desc, _) in CONFIG_KEYS {
            // Check runtime override first
            let (display, source) = if let Some(val) = runtime_overrides.get(key) {
                (val.clone(), "runtime")
            } else {
                let (value, source) = config_get(key);
                let display = if value.is_empty() {
                    "(not set)".to_string()
                } else if *key == "HCOM_RELAY_TOKEN" && !value.is_empty() {
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
        println!(
            "\n[env] = environment, [toml] = config.toml, [file] = env file, [runtime] = agent override, [kv] = hcom.db key-value, (blank) = default"
        );
        println!("\nEdit: hcom config --edit");
    }
    0
}

/// Show detailed info for a config key.
/// Rich per-key help text.
pub fn config_help(key: &str) -> Option<&'static str> {
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

        "HCOM_TIMEOUT" => Some(
            "\
HCOM_TIMEOUT - Advanced: idle timeout for headless/vanilla Claude (seconds)

Default: 86400 (24 hours)

This setting only applies to:
  - Headless Claude: hcom N claude -p
  - Vanilla Claude: claude + hcom start

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

        "HCOM_CLAUDE_ARGS" => Some(
            "\
HCOM_CLAUDE_ARGS - Default args passed to claude on launch

Example: hcom config claude_args \"--model opus\"
Clear:   hcom config claude_args \"\"

Merged with launch-time cli args (launch args win on conflict).",
        ),

        "HCOM_GEMINI_ARGS" => Some(
            "\
HCOM_GEMINI_ARGS - Default args passed to gemini on launch

Example: hcom config gemini_args \"--model gemini-2.5-flash\"
Clear:   hcom config gemini_args \"\"

Merged with launch-time cli args (launch args win on conflict).",
        ),

        "HCOM_CODEX_ARGS" => Some(
            "\
HCOM_CODEX_ARGS - Default args passed to codex on launch

Example: hcom config codex_args \"--search\"
Clear:   hcom config codex_args \"\"

Merged with launch-time cli args (launch args win on conflict).",
        ),

        "HCOM_CODEX_SANDBOX_MODE" => Some(
            "\
HCOM_CODEX_SANDBOX_MODE - Permission flags hcom injects when launching codex

Default: workspace

Codex's default sandbox blocks the writes and Unix sockets hcom needs,
so hcom injects flags on every codex launch to reshape it. This knob
picks which set.

Values:
  workspace          Codex auto-runs; asks only when the model judges
                     necessary.
  untrusted          Codex prompts before every command that isn't a
                     known-safe read. Effectively read-only unless you
                     approve writes case-by-case.
  danger-full-access No sandbox, no approvals.
  none               Inject nothing. Codex uses your own config; DB
                     writes fail unless your config allows ~/.hcom.

Usage:
  hcom config codex_sandbox_mode untrusted
  hcom config codex_sandbox_mode \"\"        # Reset to default",
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

Optional. Set via 'hcom relay new --password <secret>' or directly here.
Stored in [relay] in config.toml. Environment overrides are ignored for relay fields.
Only needed if your broker requires authentication.",
        ),

        "HCOM_AUTO_APPROVE" => Some(
            "\
HCOM_AUTO_APPROVE - Auto-approve safe hcom commands

Purpose:
  When enabled, Claude/Gemini/Codex auto-approve \"safe\" hcom commands
  without requiring user confirmation.

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
  - Only affects hcom-launched instances (hcom N claude/gemini/codex)
  - Variable name must be a valid shell identifier
  - Works alongside HCOM_PROCESS_ID (always set) for identity",
        ),

        "HCOM_OPENCODE_ARGS" => Some(
            "\
HCOM_OPENCODE_ARGS - Default args passed to opencode on launch

Example: hcom config opencode_args \"--model o3\"
Clear:   hcom config opencode_args \"\"

Merged with launch-time cli args (launch args win on conflict).",
        ),

        "HCOM_RELAY_ENABLED" => Some(
            "\
HCOM_RELAY_ENABLED - Enable or disable relay sync

Default: true (when relay is configured)
Stored in [relay] in config.toml. Environment overrides are ignored for relay fields.

Usage:
  hcom config relay_enabled false    Disable relay sync
  hcom config relay_enabled true     Re-enable relay sync

Temporarily disables MQTT sync without removing relay configuration.",
        ),

        _ => None,
    }
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

    // Managed parents and their variants
    const MANAGED_PARENTS: &[(&str, &str)] = &[
        ("kitty", "auto split/tab/window"),
        ("wezterm", "auto tab/split/window"),
        ("tmux", "detached sessions"),
        ("cmux", "workspaces"),
        ("zellij", "panes"),
        ("waveterm", "blocks"),
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
        #[allow(unused_variables)]
        if let Some((name, p)) = preset {
            if let Some(bin) = p.binary {
                if crate::terminal::which_bin(bin).is_some() {
                    return true;
                }
            }
            #[cfg(target_os = "macos")]
            {
                let app = p.app_name.unwrap_or(name);
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
        }
        false
    };

    let mut lines = Vec::new();
    lines.push("HCOM_TERMINAL — where hcom opens new agent windows".to_string());
    lines.push(String::new());

    if show_current {
        let (current, source) = config_get("HCOM_TERMINAL");
        if current.is_empty() {
            lines.push("Current: default (auto-detect)".to_string());
        } else {
            let kind =
                if crate::config::get_merged_preset(&current).is_some_and(|p| p.close.is_some()) {
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

    // Other (open-only, platform-filtered)
    lines.push(String::new());
    lines.push("Other (opens window only):".to_string());
    for (name, preset) in TERMINAL_PRESETS.iter() {
        if all_managed.contains(name) {
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
                        let has_close = val
                            .get("close")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty());
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
    lines.push(String::new());
    lines.push("Set:    hcom config terminal kitty".to_string());
    lines.push("Reset:  hcom config terminal default".to_string());

    lines.join("\n")
}

fn print_terminal_info() {
    println!("{}", terminal_help_text(true));
}

fn show_key_info(key: &str) -> i32 {
    if CONFIG_KEYS.iter().any(|(k, _, _)| *k == key) {
        // HCOM_TERMINAL: dynamic help from TERMINAL_PRESETS
        if key == "HCOM_TERMINAL" {
            print_terminal_info();
            return 0;
        }
        // If rich help text exists, show it with current value appended
        if let Some(help) = config_help(key) {
            let (value, source) = config_get(key);
            println!("{help}");
            println!();
            if value.is_empty() {
                println!("Current value: (not set)");
            } else {
                println!("Current value: {value} [{source}]");
            }
            return 0;
        }
        // Fallback: basic info from CONFIG_KEYS
        let (_, desc, typ) = CONFIG_KEYS.iter().find(|(k, _, _)| *k == key).unwrap();
        let (value, source) = config_get(key);
        println!("{key}");
        println!("  Description: {desc}");
        println!("  Type: {typ}");
        if value.is_empty() {
            println!("  Value: (not set)");
        } else {
            println!("  Value: {value} [{source}]");
        }
        println!("  Set via: hcom config {key} <value>");
        println!("  Or env: export {key}=<value>");
        0
    } else {
        eprintln!("Unknown config key: {key}");
        eprintln!(
            "Valid keys: {}",
            CONFIG_KEYS
                .iter()
                .map(|(k, _, _)| *k)
                .collect::<Vec<_>>()
                .join(", ")
        );
        1
    }
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
        for (name, _preset) in TERMINAL_PRESETS.iter() {
            let marker = if *name == current { " ← current" } else { "" };
            println!("  {}{}", name, marker);
        }
        // Include TOML-defined presets not in built-ins
        let toml_path = crate::paths::config_toml_path();
        if let Some(toml_presets) = crate::config::load_toml_presets(&toml_path) {
            if let Some(table) = toml_presets.as_table() {
                for name in table.keys() {
                    if !TERMINAL_PRESETS.iter().any(|(n, _)| *n == name.as_str()) {
                        let marker = if *name == current { " ← current" } else { "" };
                        println!("  {}{}", name, marker);
                    }
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
        match config_set("HCOM_TERMINAL", "") {
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

    // Validate preset exists (built-in or user-defined in config.toml)
    let valid = TERMINAL_PRESETS
        .iter()
        .any(|(name, _)| *name == preset_name.as_str())
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
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            if k == key {
                return Some(v.trim().to_string());
            }
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

    if let Some(ref rc_val) = has_rc {
        if rc_val != "yes" && rc_val != "socket" {
            eprintln!(
                "Error: allow_remote_control is '{rc_val}' in {}",
                conf.display()
            );
            eprintln!("  Change to 'yes' or 'socket', then restart kitty");
            return 1;
        }
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
/// Delegates to `hcom hooks setup` for Claude/Gemini/Codex.
fn update_auto_approve_permissions(value: &str) {
    let enabled = !matches!(value, "0" | "false" | "False" | "no" | "off" | "");
    // Re-run hooks setup to update tool permission files
    let prefix = crate::runtime_env::get_hcom_prefix();
    if let Some((cmd, prefix_args)) = prefix.split_first() {
        let _ = std::process::Command::new(cmd)
            .args(prefix_args)
            .args(["hooks", "setup"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .and_then(|mut c| c.wait());
    }

    if enabled {
        println!("Auto-approve enabled for safe hcom commands in Claude/Gemini/Codex");
    } else {
        println!("Auto-approve disabled - safe hcom commands will require approval");
    }
}

/// Trigger relay push (best-effort, silent failure). C4 fix.
fn trigger_relay_push() {
    // Trigger relay push (best-effort)
    let prefix = crate::runtime_env::get_hcom_prefix();
    if let Some((cmd, prefix_args)) = prefix.split_first() {
        let _ = std::process::Command::new(cmd)
            .args(prefix_args)
            .args(["relay", "push"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        for (key, desc, typ) in CONFIG_KEYS {
            assert!(!key.is_empty());
            assert!(!desc.is_empty());
            assert!(!typ.is_empty());
        }
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
    fn test_terminal_help_text_lists_close_capable_zellij_as_managed() {
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

        assert!(managed.contains("zellij"));
        assert!(!other.contains("zellij"));
    }

    #[test]
    fn test_terminal_help_text_lists_close_capable_waveterm_as_managed() {
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

        assert!(managed.contains("waveterm"));
        assert!(!other.contains("waveterm"));
    }

    #[test]
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
        assert_eq!(timeout, 86400);
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
        //   - timeout: number when set (schema default 86400); null when the
        //     row has an explicit NULL (legacy/migrated rows)
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
        // Schema default kicks in on INSERT without an explicit value.
        assert_eq!(config["timeout"], 86400);
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
