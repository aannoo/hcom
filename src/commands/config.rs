//! `hcom config` command — view and edit configuration.
//!
//!
//! Supports: show all, get/set single key, --json, --edit, --reset,
//! per-instance config (-i), terminal preset management.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::db::HcomDb;
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
    ("HCOM_NOTES", "One-time notes appended at bootstrap", "string"),
    ("HCOM_TIMEOUT", "Idle timeout in seconds (default: 86400)", "integer"),
    ("HCOM_SUBAGENT_TIMEOUT", "Timeout for Claude subagents in seconds (default: 30)", "integer"),
    ("HCOM_CLAUDE_ARGS", "Default args for claude on launch", "string"),
    ("HCOM_GEMINI_ARGS", "Default args for gemini on launch", "string"),
    ("HCOM_CODEX_ARGS", "Default args for codex on launch", "string"),
    ("HCOM_OPENCODE_ARGS", "Default args for opencode on launch", "string"),
    ("HCOM_CODEX_SANDBOX_MODE", "Codex sandbox mode (e.g., off)", "string"),
    ("HCOM_GEMINI_SYSTEM_PROMPT", "System prompt for gemini on launch", "string"),
    ("HCOM_CODEX_SYSTEM_PROMPT", "System prompt for codex on launch", "string"),
    ("HCOM_TERMINAL", "Terminal preset for spawning agent panes", "string"),
    ("HCOM_AUTO_APPROVE", "Auto-approve safe hcom commands (true/false)", "boolean"),
    ("HCOM_AUTO_SUBSCRIBE", "Auto-subscribe event presets (comma-separated)", "string"),
    ("HCOM_NAME_EXPORT", "Export instance name to custom env var", "string"),
    ("HCOM_RELAY", "Relay MQTT broker URL", "string"),
    ("HCOM_RELAY_ID", "Relay group identifier", "string"),
    ("HCOM_RELAY_TOKEN", "Relay authentication token", "string"),
    ("HCOM_RELAY_ENABLED", "Enable relay sync (true/false)", "boolean"),
];

/// Instance-level config keys.
const INSTANCE_KEYS: &[(&str, &str)] = &[
    ("tag", "Instance-specific tag (changes display name)"),
    ("timeout", "Instance-specific idle timeout in seconds"),
    ("hints", "Instance-specific hints (injected with messages)"),
    ("subagent_timeout", "Instance-specific subagent timeout in seconds"),
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
fn config_set(key: &str, value: &str) -> Result<(), String> {
    let path = config_path();
    let content = std::fs::read_to_string(&path).unwrap_or_default();

    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("Failed to parse config.toml: {e}"))?;

    // Map HCOM_KEY to field name, then to nested TOML path
    let field_name = key
        .strip_prefix("HCOM_")
        .unwrap_or(key)
        .to_lowercase();

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

    std::fs::write(&path, doc.to_string())
        .map_err(|e| format!("Failed to write config.toml: {e}"))?;

    Ok(())
}

/// Get a config value (checks env var, then TOML nested path, then default).
fn config_get(key: &str) -> (String, &'static str) {
    // Check env var first
    if let Ok(val) = std::env::var(key) {
        return (val, "env");
    }

    // Map to field name and nested TOML path
    let field_name = key
        .strip_prefix("HCOM_")
        .unwrap_or(key)
        .to_lowercase();

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
        // Fallback: try flat key (for legacy configs)
        if let Some(val) = table.get(&field_name) {
            let val_str = match val {
                toml::Value::String(s) => s.clone(),
                toml::Value::Integer(n) => n.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                toml::Value::Float(f) => f.to_string(),
                other => other.to_string(),
            };
            return (val_str, "toml");
        }
    }

    // Default
    let default = match key {
        "HCOM_TIMEOUT" => "86400",
        "HCOM_SUBAGENT_TIMEOUT" => "30",
        "HCOM_AUTO_APPROVE" => "false",
        _ => "",
    };
    (default.to_string(), "default")
}

// ── Instance Config ──────────────────────────────────────────────────────

/// Handle instance-level config: `hcom config -i <name> [key] [value]`
fn config_instance(db: &HcomDb, instance_arg: &str, args: &[String], ctx: Option<&CommandContext>, json_mode: bool) -> i32 {
    // Resolve "self" to current instance
    let name = if instance_arg == "self" {
        if let Some(id) = ctx.and_then(|c| c.identity.as_ref()) {
            id.name.clone()
        } else {
            eprintln!("Error: Cannot resolve 'self' — no active identity");
            return 1;
        }
    } else {
        instance_arg.to_string()
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
                Ok(matched) => {
                    match db.get_instance_full(&matched) {
                        Ok(Some(inst)) => inst,
                        _ => {
                            eprintln!("Error: Agent '{name}' not found");
                            return 1;
                        }
                    }
                }
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
        if json_mode {
            let config = serde_json::json!({
                "name": inst_name,
                "full_name": full_name,
                "tag": instance.tag.as_deref().filter(|s| !s.is_empty()),
                "timeout": instance.wait_timeout,
                "hints": instance.hints.as_deref().filter(|s| !s.is_empty()),
                "subagent_timeout": instance.subagent_timeout,
            });
            println!("{}", serde_json::to_string(&config).unwrap_or_default());
        } else {
            println!("Agent: {full_name}");
            println!("  tag: {}", instance.tag.as_deref().unwrap_or("(none)"));
            println!("  timeout: {}s", instance.wait_timeout.map(|t| t.to_string()).unwrap_or_else(|| "(default)".into()));
            let hints = instance.hints.as_deref().unwrap_or("");
            println!("  hints: {}", if hints.is_empty() { "(none)" } else { hints });
            let sat = instance.subagent_timeout.map(|t| format!("{t}s"));
            println!("  subagent_timeout: {}", sat.as_deref().unwrap_or("(default)"));
        }
        return 0;
    }

    let key = args[0].as_str();
    // Join all remaining args with spaces
    let value_joined = if args.len() > 1 {
        Some(args[1..].iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "))
    } else {
        None
    };
    let value = value_joined.as_deref();

    // Validate key
    if !INSTANCE_KEYS.iter().any(|(k, _)| *k == key) {
        eprintln!("Error: Unknown instance config key '{key}'");
        eprintln!("Valid keys: {}", INSTANCE_KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", "));
        return 1;
    }

    // No value: show current
    let Some(value) = value else {
        match key {
            "tag" => println!("{}", instance.tag.as_deref().unwrap_or("")),
            "timeout" => println!("{}", instance.wait_timeout.map(|t| t.to_string()).unwrap_or_default()),
            "hints" => println!("{}", instance.hints.as_deref().unwrap_or("")),
            "subagent_timeout" => println!("{}", instance.subagent_timeout.map(|t| t.to_string()).unwrap_or_default()),
            _ => {}
        }
        return 0;
    };

    // Set value
    match key {
        "tag" => {
            let tag = if value.is_empty() { "" } else { value };
            // Validate: alphanumeric, hyphens, underscores only
            if !tag.is_empty() && !tag.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                eprintln!("Error: Tag must be alphanumeric (hyphens and underscores allowed)");
                return 1;
            }
            let _ = db.conn().execute(
                "UPDATE instances SET tag = ? WHERE name = ?",
                rusqlite::params![tag, inst_name],
            );
            if tag.is_empty() {
                println!("Cleared tag for {inst_name}");
            } else {
                println!("Set tag for {inst_name}: {tag}");
            }
            // Notify for display update
            crate::instances::notify_all_instances(db);
        }
        "timeout" => {
            if value.is_empty() || value.eq_ignore_ascii_case("default") {
                let _ = db.conn().execute(
                    "UPDATE instances SET wait_timeout = 86400 WHERE name = ?",
                    rusqlite::params![inst_name],
                );
                println!("Reset timeout for {inst_name}");
            } else {
                match value.parse::<i64>() {
                    Ok(secs) if secs > 0 => {
                        let _ = db.conn().execute(
                            "UPDATE instances SET wait_timeout = ? WHERE name = ?",
                            rusqlite::params![secs, inst_name],
                        );
                        println!("Set timeout for {inst_name}: {secs}s");
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
                println!("Cleared hints for {inst_name}");
            } else {
                updates.insert("hints".into(), serde_json::json!(value));
                instances::update_instance_position(db, inst_name, &updates);
                println!("Set hints for {inst_name}");
            }
        }
        "subagent_timeout" => {
            let mut updates = serde_json::Map::new();
            if value.is_empty() || value.eq_ignore_ascii_case("default") {
                updates.insert("subagent_timeout".into(), serde_json::Value::Null);
                instances::update_instance_position(db, inst_name, &updates);
                println!("Cleared subagent_timeout for {inst_name}");
            } else {
                match value.parse::<i64>() {
                    Ok(secs) if secs > 0 => {
                        updates.insert("subagent_timeout".into(), serde_json::json!(secs));
                        instances::update_instance_position(db, inst_name, &updates);
                        println!("Set subagent_timeout for {inst_name}: {value}s");
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

// ── Main Entry Point ─────────────────────────────────────────────────────

/// Main entry point for `hcom config` command.
pub fn cmd_config(db: &HcomDb, args: &ConfigArgs, ctx: Option<&CommandContext>) -> i32 {
    let json_mode = args.json;
    let edit_mode = args.edit;
    let reset_mode = args.reset;
    let info_mode = args.info;
    let setup_mode = args.setup;
    let instance_name = args.instance.clone();
    // Reconstruct argv from key + value for backward compat with existing handlers
    let argv: Vec<String> = args.key.iter().cloned()
        .chain(args.value.iter().cloned())
        .collect();

    // --setup: only valid with 'terminal kitty' (C4 fix)
    if setup_mode {
        let positional: Vec<&String> = argv.iter().filter(|a| !a.starts_with('-')).collect();
        let is_kitty_terminal = positional.len() >= 2
            && positional[0] == "terminal"
            && positional[1].starts_with("kitty");
        if !is_kitty_terminal {
            eprintln!("Error: --setup is only valid with kitty: hcom config terminal kitty --setup");
            return 1;
        }
    }

    // Instance config mode
    if let Some(ref inst) = instance_name {
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
        let status = std::process::Command::new(&editor)
            .arg(&path)
            .status();
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
        return super::reset::reset_config();
    }

    // No args: show all config
    if argv.is_empty() {
        return show_all_config(db, ctx, json_mode);
    }

    let key_arg = &argv[0];

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
fn get_runtime_overrides(db: &HcomDb, ctx: Option<&CommandContext>) -> std::collections::HashMap<&'static str, String> {
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
        println!("{}", serde_json::to_string_pretty(&Value::Object(result)).unwrap_or_default());
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
        println!("\n[env] = environment, [toml] = config.toml, [file] = env file, [runtime] = agent override, (blank) = default");
        println!("\nEdit: hcom config --edit");
    }
    0
}

/// Show detailed info for a config key.
/// Rich per-key help text.
pub fn config_help(key: &str) -> Option<&'static str> {
    match key {
        "HCOM_TAG" => Some("\
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

Allowed characters: letters, numbers, hyphens (a-z, A-Z, 0-9, -)"),

        "HCOM_HINTS" => Some("\
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
  - Global hints apply to all new launches"),

        "HCOM_NOTES" => Some("\
HCOM_NOTES - One-time notes appended to bootstrap

  Custom text added to agent system context at startup.
  Unlike HCOM_HINTS (per-message), this is injected once and does not repeat.

Usage:
  hcom config notes \"Always check hcom list before spawning new agents\"
  hcom config notes \"\"                            # Clear
  HCOM_NOTES=\"tips\" hcom 1 claude                 # Per-launch override

  Changing after launch has no effect (bootstrap already delivered)."),

        "HCOM_TIMEOUT" => Some("\
HCOM_TIMEOUT - Deprecated: no longer used (since v0.7.3)

Previously controlled the idle timeout for headless/vanilla Claude's
Stop hook blocking poll. The Stop hook is now non-blocking.

Related settings that ARE still active:
  - HCOM_SUBAGENT_TIMEOUT: timeout for Claude subagents (SubagentStop)
  - hcom listen --timeout N: timeout for explicit message waiting"),

        "HCOM_SUBAGENT_TIMEOUT" => Some("\
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
  - Increase for complex subagent tasks"),

        "HCOM_CLAUDE_ARGS" => Some("\
HCOM_CLAUDE_ARGS - Default args passed to claude on launch

Example: hcom config claude_args \"--model opus\"
Clear:   hcom config claude_args \"\"

Merged with launch-time cli args (launch args win on conflict)."),

        "HCOM_GEMINI_ARGS" => Some("\
HCOM_GEMINI_ARGS - Default args passed to gemini on launch

Example: hcom config gemini_args \"--model gemini-2.5-flash\"
Clear:   hcom config gemini_args \"\"

Merged with launch-time cli args (launch args win on conflict)."),

        "HCOM_CODEX_ARGS" => Some("\
HCOM_CODEX_ARGS - Default args passed to codex on launch

Example: hcom config codex_args \"--search\"
Clear:   hcom config codex_args \"\"

Merged with launch-time cli args (launch args win on conflict)."),

        "HCOM_RELAY" => Some("\
HCOM_RELAY - MQTT broker URL

Empty = use public brokers (broker.emqx.io, broker.hivemq.com, test.mosquitto.org).
Set automatically by 'hcom relay new' (pins first working broker).

Private broker: hcom relay new --broker mqtts://host:port"),

        "HCOM_RELAY_ID" => Some("\
HCOM_RELAY_ID - Shared UUID for relay group

Generated by 'hcom relay new'. Other devices join with 'hcom relay connect <token>'.
All devices with the same relay_id sync state via MQTT pub/sub."),

        "HCOM_RELAY_TOKEN" => Some("\
HCOM_RELAY_TOKEN - Auth token for MQTT broker

Optional. Set via 'hcom relay new --password <secret>' or directly here.
Only needed if your broker requires authentication."),

        "HCOM_AUTO_APPROVE" => Some("\
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

Values: 1, true, yes, on (enabled) | 0, false, no, off, \"\" (disabled)"),

        "HCOM_AUTO_SUBSCRIBE" => Some("\
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
  - See 'hcom events --help' for subscription management"),

        "HCOM_NAME_EXPORT" => Some("\
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
  - Works alongside HCOM_PROCESS_ID (always set) for identity"),

        "HCOM_OPENCODE_ARGS" => Some("\
HCOM_OPENCODE_ARGS - Default args passed to opencode on launch

Example: hcom config opencode_args \"--model o3\"
Clear:   hcom config opencode_args \"\"

Merged with launch-time cli args (launch args win on conflict)."),

        "HCOM_RELAY_ENABLED" => Some("\
HCOM_RELAY_ENABLED - Enable or disable relay sync

Default: true (when relay is configured)

Usage:
  hcom config relay_enabled false    Disable relay sync
  hcom config relay_enabled true     Re-enable relay sync

Temporarily disables MQTT sync without removing relay configuration."),

        _ => None,
    }
}

/// Build terminal help text (shared between config --info and run docs --config).
pub fn terminal_help_text(show_current: bool) -> String {
    use crate::shared::constants::TERMINAL_PRESETS;

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
    ];
    const MANAGED_VARIANTS: &[(&str, &[&str])] = &[
        ("kitty", &["kitty-window", "kitty-tab", "kitty-split"]),
        ("wezterm", &["wezterm-window", "wezterm-tab", "wezterm-split"]),
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
        let preset = TERMINAL_PRESETS
            .iter()
            .find(|(n, _)| *n == preset_name);
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
                for dir in ["/Applications", "/Applications/Utilities",
                            "/System/Applications", "/System/Applications/Utilities"] {
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
            let kind = if TERMINAL_PRESETS.iter().any(|(n, p)| *n == current && p.close.is_some()) {
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

    lines.push(String::new());
    lines.push("Custom command (open only):".to_string());
    lines.push("  hcom config terminal \"my-terminal -e bash {script}\"".to_string());
    lines.push(String::new());
    lines.push("Custom preset with close (~/.hcom/config.toml):".to_string());
    lines.push("  [terminal.presets.myterm]".to_string());
    lines.push("  open = \"myterm spawn -- bash {script}\"".to_string());
    lines.push("  close = \"myterm kill --id {id}\"".to_string());
    lines.push("  binary = \"myterm\"".to_string());
    lines.push(String::new());
    lines.push("  {id} = stdout from the open command.".to_string());
    lines.push("  {pid} and {process_id} also available.".to_string());
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
    use crate::shared::constants::TERMINAL_PRESETS;

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

    // Validate preset exists
    let valid = TERMINAL_PRESETS
        .iter()
        .any(|(name, _)| *name == preset_name.as_str());

    if !valid {
        eprintln!("Error: Unknown terminal preset '{preset_name}'");
        eprintln!(
            "Available: {}",
            TERMINAL_PRESETS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        );
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
            println!("  Socket found ({}) — splits/tabs available", socket.display());
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
    let candidates = [
        PathBuf::from("/tmp/kitty"),
        PathBuf::from("/tmp/mykitty"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Check if kitty.conf contains a specific key (exact match via whitespace split).
fn kitty_conf_has(path: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') { continue; }
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
        println!("Kitty remote control already working ({})", socket.display());
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
        println!("Config OK ({}) but no socket — restart kitty", conf.display());
        return 0;
    }

    if let Some(ref rc_val) = has_rc {
        if rc_val != "yes" && rc_val != "socket" {
            eprintln!("Error: allow_remote_control is '{rc_val}' in {}", conf.display());
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
    let _ = std::process::Command::new("hcom")
        .args(["hooks", "setup"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut c| c.wait());

    if enabled {
        println!("Auto-approve enabled for safe hcom commands in Claude/Gemini/Codex");
    } else {
        println!("Auto-approve disabled - safe hcom commands will require approval");
    }
}

/// Trigger relay push (best-effort, silent failure). C4 fix.
fn trigger_relay_push() {
    // Trigger relay push (best-effort)
    let _ = std::process::Command::new("hcom")
        .args(["relay", "push"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
}
