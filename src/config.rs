//! Configuration management — central config system used by all modules.
//!
//! Two config layers:
//! - `Config`: Runtime env vars (HCOM_DIR, HCOM_INSTANCE_NAME, etc.) — startup-only, used by router/client
//! - `HcomConfig`: User config from TOML + env vars — all 20 user-facing settings with validation

use regex::Regex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

static RE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9-]+$").unwrap());
static RE_PRESET_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_]+$").unwrap());

use crate::paths;

/// Global configuration instance, lazily initialized and resettable for tests.
static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Configuration loaded from HCOM_* environment variables.
///
/// All environment variable access should go through this struct
/// rather than calling env::var directly.
#[derive(Clone, Debug)]
pub struct Config {
    /// HCOM directory (HCOM_DIR or ~/.hcom)
    pub hcom_dir: PathBuf,
    /// Instance name (HCOM_INSTANCE_NAME)
    pub instance_name: Option<String>,
    /// Process ID for daemon binding (HCOM_PROCESS_ID)
    pub process_id: Option<String>,
}

impl Config {
    /// Initialize global config from environment variables (call once at startup).
    /// Can be called multiple times - subsequent calls are no-ops.
    pub fn init() {
        let _ = Self::get();
    }

    /// Get global config, initializing it from the current environment if needed.
    pub fn get() -> Config {
        let mut config = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
        if config.is_none() {
            *config = Some(Self::from_env());
        }
        config
            .clone()
            .expect("Config should be initialized before returning")
    }

    /// Reset global config (test-only).
    /// Allows tests to reinitialize config with different env vars.
    #[cfg(test)]
    pub fn reset() {
        *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Load configuration from environment variables
    fn from_env() -> Self {
        use std::env;

        let env_map: HashMap<String, String> = env::vars().collect();
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let (hcom_dir, _) = paths::resolve_hcom_dir_from_env(&env_map, &cwd);

        let instance_name = env::var("HCOM_INSTANCE_NAME")
            .ok()
            .filter(|s| !s.is_empty());

        let process_id = env::var("HCOM_PROCESS_ID").ok().filter(|s| !s.is_empty());

        Self {
            hcom_dir,
            instance_name,
            process_id,
        }
    }
}

/// Bidirectional mapping: HcomConfig field name <-> TOML dotted path.
const TOML_KEY_MAP: &[(&str, &str)] = &[
    ("terminal", "terminal.active"),
    ("tag", "launch.tag"),
    ("project", "launch.project"),
    ("hints", "launch.hints"),
    ("notes", "launch.notes"),
    ("subagent_timeout", "launch.subagent_timeout"),
    ("auto_subscribe", "launch.auto_subscribe"),
    ("claude_args", "launch.claude.args"),
    ("gemini_args", "launch.gemini.args"),
    ("gemini_system_prompt", "launch.gemini.system_prompt"),
    ("codex_args", "launch.codex.args"),
    ("codex_sandbox_mode", "launch.codex.sandbox_mode"),
    ("codex_system_prompt", "launch.codex.system_prompt"),
    ("opencode_args", "launch.opencode.args"),
    ("kilo_args", "launch.kilo.args"),
    ("cline_args", "launch.cline.args"),
    ("relay", "relay.url"),
    ("relay_id", "relay.id"),
    ("relay_token", "relay.token"),
    ("relay_psk", "relay.psk"),
    ("relay_enabled", "relay.enabled"),
    ("timeout", "preferences.timeout"),
    ("auto_approve", "preferences.auto_approve"),
    ("name_export", "preferences.name_export"),
];

/// Mapping: HcomConfig field name -> HCOM_* env var key.
const FIELD_TO_ENV: &[(&str, &str)] = &[
    ("timeout", "HCOM_TIMEOUT"),
    ("subagent_timeout", "HCOM_SUBAGENT_TIMEOUT"),
    ("terminal", "HCOM_TERMINAL"),
    ("hints", "HCOM_HINTS"),
    ("notes", "HCOM_NOTES"),
    ("tag", "HCOM_TAG"),
    ("project", "HCOM_PROJECT"),
    ("claude_args", "HCOM_CLAUDE_ARGS"),
    ("gemini_args", "HCOM_GEMINI_ARGS"),
    ("codex_args", "HCOM_CODEX_ARGS"),
    ("codex_sandbox_mode", "HCOM_CODEX_SANDBOX_MODE"),
    ("gemini_system_prompt", "HCOM_GEMINI_SYSTEM_PROMPT"),
    ("codex_system_prompt", "HCOM_CODEX_SYSTEM_PROMPT"),
    ("opencode_args", "HCOM_OPENCODE_ARGS"),
    ("kilo_args", "HCOM_KILO_ARGS"),
    ("cline_args", "HCOM_CLINE_ARGS"),
    ("relay", "HCOM_RELAY"),
    ("relay_id", "HCOM_RELAY_ID"),
    ("relay_token", "HCOM_RELAY_TOKEN"),
    // NOTE: `relay_psk` is deliberately NOT in FIELD_TO_ENV. `to_env_dict` feeds
    // `build_launch_env`, which injects these vars into every spawned agent
    // child. The PSK is forge/decrypt authority for the entire relay group —
    // it must never cross a process boundary via environment. Relay fields are
    // file-only on load already (see `is_relay_field` in `load_from_sources`),
    // so env-var override was never the mechanism for configuring the PSK.
    ("relay_enabled", "HCOM_RELAY_ENABLED"),
    ("auto_approve", "HCOM_AUTO_APPROVE"),
    ("auto_subscribe", "HCOM_AUTO_SUBSCRIBE"),
    ("name_export", "HCOM_NAME_EXPORT"),
];

/// Relay fields — file-only, no env var override.
const RELAY_FIELDS: &[&str] = &[
    "relay",
    "relay_id",
    "relay_token",
    "relay_psk",
    "relay_enabled",
];

/// Characters that are dangerous in terminal preset values (injection risk).
const TERMINAL_DANGEROUS_CHARS: &[char] = &['`', '$', ';', '|', '&', '\n', '\r'];

use crate::shared::terminal_presets::TERMINAL_PRESETS;

/// Valid codex sandbox modes.
pub const VALID_SANDBOX_MODES: &[&str] = &["workspace", "untrusted", "danger-full-access", "none"];

/// TOML file header comment.
const TOML_HEADER: &str = "\
# hcom configuration
# Help: hcom config --help
# Docs: hcom run docs
";

/// Get value from nested TOML table using dotted path (e.g., "launch.claude.args").
fn get_nested(table: &toml::Value, dotted_path: &str) -> Option<toml::Value> {
    let mut current = table;
    for part in dotted_path.split('.') {
        current = current.as_table()?.get(part)?;
    }
    Some(current.clone())
}

/// Set value in nested TOML table using dotted path, creating intermediates.
fn set_nested(table: &mut toml::Value, dotted_path: &str, value: toml::Value) {
    let parts: Vec<&str> = dotted_path.split('.').collect();
    let mut current = table;
    for &part in &parts[..parts.len() - 1] {
        let Some(tbl) = current.as_table_mut() else {
            return;
        };
        if !tbl.contains_key(part) {
            tbl.insert(part.to_string(), toml::Value::Table(toml::map::Map::new()));
        }
        let Some(next) = tbl.get_mut(part) else {
            return;
        };
        current = next;
    }
    if let Some(t) = current.as_table_mut() {
        t.insert(parts[parts.len() - 1].to_string(), value);
    }
}

/// Validation errors from HcomConfig construction.
#[derive(Debug, Clone)]
pub struct HcomConfigError {
    pub errors: HashMap<String, String>,
}

impl std::fmt::Display for HcomConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.errors.is_empty() {
            write!(f, "Invalid config")
        } else {
            writeln!(f, "Invalid config:")?;
            for msg in self.errors.values() {
                writeln!(f, "  - {msg}")?;
            }
            Ok(())
        }
    }
}

impl std::error::Error for HcomConfigError {}

/// HCOM user configuration with validation.
/// Load priority: env var → config.toml → defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct HcomConfig {
    pub timeout: i64,
    pub subagent_timeout: i64,
    pub terminal: String,
    pub hints: String,
    pub notes: String,
    pub tag: String,
    pub project: String,
    pub claude_args: String,
    pub gemini_args: String,
    pub codex_args: String,
    pub opencode_args: String,
    pub kilo_args: String,
    pub cline_args: String,
    pub codex_sandbox_mode: String,
    pub gemini_system_prompt: String,
    pub codex_system_prompt: String,
    pub relay: String,
    pub relay_id: String,
    pub relay_token: String,
    pub relay_psk: String,
    pub relay_enabled: bool,
    pub auto_approve: bool,
    pub auto_subscribe: String,
    pub name_export: String,
}

impl Default for HcomConfig {
    fn default() -> Self {
        Self {
            timeout: 86400,
            subagent_timeout: 30,
            terminal: "default".to_string(),
            hints: String::new(),
            notes: String::new(),
            tag: String::new(),
            project: String::new(),
            claude_args: String::new(),
            gemini_args: String::new(),
            codex_args: String::new(),
            opencode_args: String::new(),
            kilo_args: String::new(),
            cline_args: String::new(),
            codex_sandbox_mode: "workspace".to_string(),
            gemini_system_prompt: String::new(),
            codex_system_prompt: String::new(),
            relay: String::new(),
            relay_id: String::new(),
            relay_token: String::new(),
            relay_psk: String::new(),
            relay_enabled: true,
            auto_approve: true,
            auto_subscribe: "collision".to_string(),
            name_export: String::new(),
        }
    }
}

impl HcomConfig {
    /// Normalize fields before validation (case normalization, legacy values).
    pub fn normalize(&mut self) {
        // Resolve old terminal casing (WezTerm→wezterm, Alacritty→alacritty)
        if self.terminal != "default" && self.terminal != "print" && self.terminal != "here" {
            self.terminal = normalize_terminal_case(&self.terminal);
        }
    }

    /// Validate all fields, returning map of field → error message.
    /// Also normalizes fields (terminal case, etc.).
    pub fn collect_errors(&mut self) -> HashMap<String, String> {
        self.normalize();
        let mut errors: HashMap<String, String> = HashMap::new();

        // Validate timeout
        if !(1..=86400).contains(&self.timeout) {
            errors.insert(
                "timeout".into(),
                format!(
                    "timeout must be 1-86400 seconds (24 hours), got {}",
                    self.timeout
                ),
            );
        }

        // Validate subagent_timeout
        if !(1..=86400).contains(&self.subagent_timeout) {
            errors.insert(
                "subagent_timeout".into(),
                format!(
                    "subagent_timeout must be 1-86400 seconds, got {}",
                    self.subagent_timeout
                ),
            );
        }

        // Validate terminal
        if self.terminal.is_empty() {
            errors.insert("terminal".into(), "terminal cannot be empty".into());
        } else if self.terminal != "default" && self.terminal != "print" && self.terminal != "here"
        {
            // Check against built-in presets + user-defined TOML presets
            let known =
                is_known_terminal_preset(&self.terminal) || is_user_defined_preset(&self.terminal);
            if !known {
                // Not a known preset — must be a custom command with {script}
                if !self.terminal.contains("{script}") {
                    errors.insert(
                        "terminal".into(),
                        format!(
                            "terminal must be 'default', preset name, or custom command with {{script}}, got '{}'",
                            self.terminal
                        ),
                    );
                }
            }
        }

        // Validate tag (alphanumeric + hyphens only)
        if !self.tag.is_empty() && !RE_TAG.is_match(&self.tag) {
            errors.insert(
                "tag".into(),
                "tag can only contain letters, numbers, and hyphens".into(),
            );
        }

        // Validate project (alphanumeric + hyphens only)
        if !self.project.is_empty() && !RE_TAG.is_match(&self.project) {
            errors.insert(
                "project".into(),
                "project can only contain letters, numbers, and hyphens".into(),
            );
        }

        // Validate shell-quoted args fields
        for (field, value) in [
            ("claude_args", &self.claude_args),
            ("gemini_args", &self.gemini_args),
            ("codex_args", &self.codex_args),
            ("opencode_args", &self.opencode_args),
            ("kilo_args", &self.kilo_args),
            ("cline_args", &self.cline_args),
        ] {
            if !value.is_empty() {
                if let Err(e) = shell_words::split(value) {
                    errors.insert(
                        field.into(),
                        format!("{field} contains invalid shell quoting: {e}"),
                    );
                }
            }
        }

        // Validate codex_sandbox_mode
        if !VALID_SANDBOX_MODES.contains(&self.codex_sandbox_mode.as_str()) {
            errors.insert(
                "codex_sandbox_mode".into(),
                format!(
                    "codex_sandbox_mode must be one of {:?}, got '{}'",
                    VALID_SANDBOX_MODES, self.codex_sandbox_mode
                ),
            );
        }

        // Validate auto_subscribe (comma-separated alphanumeric/underscore preset names)
        if !self.auto_subscribe.is_empty() {
            for preset in self.auto_subscribe.split(',') {
                let preset = preset.trim();
                if !preset.is_empty() && !RE_PRESET_NAME.is_match(preset) {
                    errors.insert(
                        "auto_subscribe".into(),
                        format!(
                            "auto_subscribe preset '{preset}' contains invalid characters (alphanumeric/underscore only)"
                        ),
                    );
                }
            }
        }

        errors
    }

    /// Validate and return list of error messages.
    pub fn validate(&mut self) -> Vec<String> {
        self.collect_errors().into_values().collect()
    }

    /// Get a field value by name (returns string representation).
    pub fn get_field(&self, field: &str) -> Option<String> {
        match field {
            "timeout" => Some(self.timeout.to_string()),
            "subagent_timeout" => Some(self.subagent_timeout.to_string()),
            "terminal" => Some(self.terminal.clone()),
            "hints" => Some(self.hints.clone()),
            "notes" => Some(self.notes.clone()),
            "tag" => Some(self.tag.clone()),
            "project" => Some(self.project.clone()),
            "claude_args" => Some(self.claude_args.clone()),
            "gemini_args" => Some(self.gemini_args.clone()),
            "codex_args" => Some(self.codex_args.clone()),
            "opencode_args" => Some(self.opencode_args.clone()),
            "kilo_args" => Some(self.kilo_args.clone()),
            "cline_args" => Some(self.cline_args.clone()),
            "codex_sandbox_mode" => Some(self.codex_sandbox_mode.clone()),
            "gemini_system_prompt" => Some(self.gemini_system_prompt.clone()),
            "codex_system_prompt" => Some(self.codex_system_prompt.clone()),
            "relay" => Some(self.relay.clone()),
            "relay_id" => Some(self.relay_id.clone()),
            "relay_token" => Some(self.relay_token.clone()),
            "relay_psk" => Some(self.relay_psk.clone()),
            "relay_enabled" => Some(if self.relay_enabled { "1" } else { "0" }.into()),
            "auto_approve" => Some(if self.auto_approve { "1" } else { "0" }.into()),
            "auto_subscribe" => Some(self.auto_subscribe.clone()),
            "name_export" => Some(self.name_export.clone()),
            _ => None,
        }
    }

    /// Set a field value by name. Returns Err if field unknown or value invalid type.
    pub fn set_field(&mut self, field: &str, value: &str) -> Result<(), String> {
        match field {
            "timeout" => {
                self.timeout = value
                    .parse()
                    .map_err(|_| format!("timeout must be an integer, got '{value}'"))?;
            }
            "subagent_timeout" => {
                self.subagent_timeout = value
                    .parse()
                    .map_err(|_| format!("subagent_timeout must be an integer, got '{value}'"))?;
            }
            "terminal" => self.terminal = value.to_string(),
            "hints" => self.hints = value.to_string(),
            "notes" => self.notes = value.to_string(),
            "tag" => self.tag = value.to_string(),
            "project" => self.project = value.to_string(),
            "claude_args" => self.claude_args = value.to_string(),
            "gemini_args" => self.gemini_args = value.to_string(),
            "codex_args" => self.codex_args = value.to_string(),
            "opencode_args" => self.opencode_args = value.to_string(),
            "kilo_args" => self.kilo_args = value.to_string(),
            "cline_args" => self.cline_args = value.to_string(),
            "codex_sandbox_mode" => {
                // Normalize legacy value
                self.codex_sandbox_mode = if value == "full-auto" {
                    "workspace".to_string()
                } else {
                    value.to_string()
                };
            }
            "gemini_system_prompt" => self.gemini_system_prompt = value.to_string(),
            "codex_system_prompt" => self.codex_system_prompt = value.to_string(),
            "relay" => self.relay = value.to_string(),
            "relay_id" => self.relay_id = value.to_string(),
            "relay_token" => self.relay_token = value.to_string(),
            "relay_psk" => self.relay_psk = value.to_string(),
            "relay_enabled" => self.relay_enabled = !is_falsy(value),
            "auto_approve" => self.auto_approve = !is_falsy(value),
            "auto_subscribe" => self.auto_subscribe = value.to_string(),
            "name_export" => self.name_export = value.to_string(),
            _ => return Err(format!("unknown field: {field}")),
        }
        Ok(())
    }

    /// Load config with precedence: env var → config.toml → defaults.
    ///
    /// `env_override`: If Some, use this map for env var lookups instead of std::env.
    /// Used in daemon mode where os.environ is stale.
    pub fn load(env_override: Option<&HashMap<String, String>>) -> Result<Self, HcomConfigError> {
        let toml_path = paths::config_toml_path();

        if !toml_path.exists() {
            let hcom_dir = &Config::get().hcom_dir;
            let config_env_path = hcom_dir.join("config.env");
            if config_env_path.exists() {
                // Legacy config.env exists — migration to config.toml not yet done.
                // Don't write default config.toml here or we'd silently lose the
                // user's settings. Load returns defaults for this invocation.
            } else {
                // No config at all — write defaults
                let _ = write_default_config();
            }
        }

        // Parse config.toml
        let file_config = if toml_path.exists() {
            load_toml_config(&toml_path)
        } else {
            HashMap::new()
        };

        Self::load_from_sources(&file_config, env_override)
    }

    /// Load from pre-parsed TOML values + env. Separated for testability.
    fn load_from_sources(
        file_config: &HashMap<String, TomlFieldValue>,
        env_override: Option<&HashMap<String, String>>,
    ) -> Result<Self, HcomConfigError> {
        let mut config = HcomConfig::default();

        let is_relay_field = |field: &str| -> bool { RELAY_FIELDS.contains(&field) };

        // Helper: get value with precedence env → file
        let get_var = |field: &str| -> Option<TomlFieldValue> {
            let env_key = FIELD_TO_ENV
                .iter()
                .find(|&&(f, _)| f == field)
                .map(|&(_, e)| e);

            // Relay fields are file-only (no env override)
            if let Some(env_key) = env_key {
                if !is_relay_field(field) {
                    let env_val = if let Some(overrides) = env_override {
                        overrides.get(env_key).cloned()
                    } else {
                        std::env::var(env_key).ok()
                    };
                    if let Some(val) = env_val {
                        return Some(TomlFieldValue::Str(val));
                    }
                }
            }

            file_config.get(field).cloned()
        };

        // Load integer fields
        for int_field in &["timeout", "subagent_timeout"] {
            if let Some(val) = get_var(int_field) {
                match val {
                    TomlFieldValue::Int(i) => {
                        let _ = config.set_field(int_field, &i.to_string());
                    }
                    TomlFieldValue::Str(s) if !s.is_empty() => {
                        if let Ok(i) = s.parse::<i64>() {
                            let _ = config.set_field(int_field, &i.to_string());
                        }
                        // Invalid int: silently use default
                    }
                    _ => {}
                }
            }
        }

        // Load string fields
        let str_fields = [
            "terminal",
            "hints",
            "notes",
            "tag",
            "project",
            "claude_args",
            "gemini_args",
            "codex_args",
            "opencode_args",
            "kilo_args",
            "cline_args",
            "codex_sandbox_mode",
            "gemini_system_prompt",
            "codex_system_prompt",
            "auto_subscribe",
            "name_export",
        ];
        for str_field in &str_fields {
            if let Some(val) = get_var(str_field) {
                let s = val.as_string();
                // terminal and codex_sandbox_mode: skip empty (use default)
                if (*str_field == "terminal" || *str_field == "codex_sandbox_mode") && s.is_empty()
                {
                    continue;
                }
                let _ = config.set_field(str_field, &s);
            }
        }

        // Load boolean fields
        for bool_field in &["relay_enabled", "auto_approve"] {
            if let Some(val) = get_var(bool_field) {
                match val {
                    TomlFieldValue::Bool(b) => {
                        let _ = config.set_field(bool_field, if b { "1" } else { "0" });
                    }
                    TomlFieldValue::Int(i) => {
                        let _ = config.set_field(bool_field, if i == 0 { "0" } else { "1" });
                    }
                    TomlFieldValue::Str(s) => {
                        let _ = config.set_field(bool_field, if is_falsy(&s) { "0" } else { "1" });
                    }
                }
            }
        }

        // Load relay string fields (file-only, already handled by get_var)
        for relay_field in &["relay", "relay_id", "relay_token", "relay_psk"] {
            if let Some(val) = get_var(relay_field) {
                let _ = config.set_field(relay_field, &val.as_string());
            }
        }

        // Validate
        let errors = config.collect_errors();
        if !errors.is_empty() {
            return Err(HcomConfigError { errors });
        }

        Ok(config)
    }

    /// Convert to HCOM_* env var dict (for persistence/display). Relay secret
    /// material (the PSK) is never emitted here — see `FIELD_TO_ENV` for why.
    pub fn to_env_dict(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for &(field, env_key) in FIELD_TO_ENV {
            if field == "relay_psk" {
                continue;
            }
            if let Some(val) = self.get_field(field) {
                map.insert(env_key.to_string(), val);
            }
        }
        map
    }

    /// Build from HCOM_* env var dict. Returns validated config.
    pub fn from_env_dict(data: &HashMap<String, String>) -> Result<Self, HcomConfigError> {
        let mut config = HcomConfig::default();
        let mut errors: HashMap<String, String> = HashMap::new();

        // Build reverse map: HCOM_* key -> field name
        let env_to_field: HashMap<&str, &str> = FIELD_TO_ENV.iter().map(|&(f, e)| (e, f)).collect();

        for (env_key, value) in data {
            if let Some(&field) = env_to_field.get(env_key.as_str()) {
                if let Err(e) = config.set_field(field, value) {
                    errors.insert(field.to_string(), e);
                }
            }
        }

        if !errors.is_empty() {
            return Err(HcomConfigError { errors });
        }

        // Run validation
        let validation_errors = config.collect_errors();
        if !validation_errors.is_empty() {
            return Err(HcomConfigError {
                errors: validation_errors,
            });
        }

        Ok(config)
    }

    /// Convert to nested TOML-ready table.
    pub fn to_toml_table(&self) -> toml::Value {
        let mut table = default_toml_structure();
        for &(field, toml_path) in TOML_KEY_MAP {
            if let Some(val) = self.get_field(field) {
                // Determine TOML value type from the default structure
                let default_val = get_nested(&table, toml_path);
                let toml_val = match default_val {
                    Some(toml::Value::Boolean(_)) => toml::Value::Boolean(!is_falsy(&val)),
                    Some(toml::Value::Integer(_)) => toml::Value::Integer(val.parse().unwrap_or(0)),
                    _ => toml::Value::String(val),
                };
                set_nested(&mut table, toml_path, toml_val);
            }
        }
        table
    }
}

/// Typed value from TOML parsing (preserves original type for coercion).
#[derive(Clone, Debug)]
pub(crate) enum TomlFieldValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl TomlFieldValue {
    fn as_string(&self) -> String {
        match self {
            TomlFieldValue::Str(s) => s.clone(),
            TomlFieldValue::Int(i) => i.to_string(),
            TomlFieldValue::Bool(b) => {
                if *b {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
        }
    }
}

/// Load config.toml and return flat map of field name → typed value.
/// Includes terminal dangerous-char validation.
pub fn load_toml_config(path: &std::path::Path) -> HashMap<String, TomlFieldValue> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let raw: toml::Value = match content.parse::<toml::Table>() {
        Ok(t) => toml::Value::Table(t),
        Err(e) => {
            eprintln!(
                "Warning: Failed to parse {}: {e} — using defaults",
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            );
            return HashMap::new();
        }
    };

    let mut result = HashMap::new();
    for &(field_name, toml_path) in TOML_KEY_MAP {
        if let Some(val) = get_nested(&raw, toml_path) {
            let typed = match &val {
                toml::Value::String(s) => TomlFieldValue::Str(s.clone()),
                toml::Value::Integer(i) => TomlFieldValue::Int(*i),
                toml::Value::Boolean(b) => TomlFieldValue::Bool(*b),
                _ => continue,
            };
            result.insert(field_name.to_string(), typed);
        }
    }

    // Terminal dangerous-char validation
    if let Some(TomlFieldValue::Str(terminal_val)) = result.get("terminal") {
        if terminal_val
            .chars()
            .any(|c| TERMINAL_DANGEROUS_CHARS.contains(&c))
        {
            let bad_chars: Vec<String> = TERMINAL_DANGEROUS_CHARS
                .iter()
                .filter(|&&c| terminal_val.contains(c))
                .map(|c| format!("{c:?}"))
                .collect();
            eprintln!(
                "Warning: Unsafe characters in terminal.active ({}), ignoring custom terminal command",
                bad_chars.join(", ")
            );
            result.remove("terminal");
        }
    }

    result
}

/// Write config.toml from HcomConfig using toml_edit to preserve comments and formatting.
/// If the file already exists, parses it and surgically updates only changed keys.
/// If the file doesn't exist, writes a fresh default with the header comment.
pub fn save_toml_config(config: &HcomConfig, presets: Option<&toml::Value>) -> std::io::Result<()> {
    use toml_edit::DocumentMut;

    let toml_path = paths::config_toml_path();

    // Ensure parent dir exists
    if let Some(parent) = toml_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Load existing document or create fresh one with header
    let mut doc: DocumentMut = if toml_path.exists() {
        let existing = std::fs::read_to_string(&toml_path)?;
        existing
            .parse::<DocumentMut>()
            .unwrap_or_else(|_| format!("{TOML_HEADER}\n").parse::<DocumentMut>().unwrap())
    } else {
        format!("{TOML_HEADER}\n").parse::<DocumentMut>().unwrap()
    };

    // Update each config key in the document
    for &(field, toml_path_str) in TOML_KEY_MAP {
        if let Some(val) = config.get_field(field) {
            set_nested_edit(&mut doc, toml_path_str, &val);
        }
    }

    // Merge terminal presets if provided
    if let Some(presets_val) = presets {
        // Convert toml::Value presets to toml_edit items
        if let Some(presets_table) = presets_val.as_table() {
            ensure_edit_table(&mut doc, "terminal");
            let Some(terminal) = doc["terminal"].as_table_mut() else {
                return Ok(());
            };
            // Parse via a wrapper doc so we get a proper nested table, not a document root
            let wrapper_str = format!(
                "[presets]\n{}",
                toml::to_string_pretty(&toml::Value::Table(presets_table.clone()))
                    .unwrap_or_default()
            );
            if let Ok(wrapper_doc) = wrapper_str.parse::<DocumentMut>() {
                if let Some(item) = wrapper_doc
                    .as_item()
                    .as_table()
                    .and_then(|t| t.get("presets"))
                {
                    terminal.insert("presets", item.clone());
                }
            }
        }
    }

    write_config_toml_path(&toml_path, &doc.to_string())
}

/// Set a value in a toml_edit document using a dotted path, creating intermediate tables.
fn set_nested_edit(doc: &mut toml_edit::DocumentMut, dotted_path: &str, value: &str) {
    let parts: Vec<&str> = dotted_path.split('.').collect();

    // Build the default structure to determine expected type
    let defaults = default_toml_structure();
    let default_val = get_nested(&defaults, dotted_path);

    // Convert string value to the correct toml_edit type
    let edit_value: toml_edit::Value = match default_val {
        Some(toml::Value::Boolean(_)) => toml_edit::value(!is_falsy(value)).into_value().unwrap(),
        Some(toml::Value::Integer(_)) => toml_edit::value(value.parse::<i64>().unwrap_or(0))
            .into_value()
            .unwrap(),
        _ => toml_edit::value(value).into_value().unwrap(),
    };

    // Navigate/create intermediate tables
    match parts.len() {
        1 => {
            doc[parts[0]] = toml_edit::Item::Value(edit_value);
        }
        2 => {
            ensure_edit_table(doc, parts[0]);
            doc[parts[0]][parts[1]] = toml_edit::Item::Value(edit_value);
        }
        3 => {
            ensure_edit_table(doc, parts[0]);
            let Some(t) = doc[parts[0]].as_table_mut() else {
                return;
            };
            if t.get(parts[1]).is_none() || !t[parts[1]].is_table() {
                t.insert(parts[1], toml_edit::Item::Table(toml_edit::Table::new()));
            }
            doc[parts[0]][parts[1]][parts[2]] = toml_edit::Item::Value(edit_value);
        }
        _ => {} // Deeper nesting not used in current config
    }
}

/// Ensure a top-level key exists as a table in the document.
fn ensure_edit_table(doc: &mut toml_edit::DocumentMut, key: &str) {
    if doc.get(key).is_none() || !doc[key].is_table() {
        doc[key] = toml_edit::Item::Table(toml_edit::Table::new());
    }
}

/// Load terminal presets from config.toml [terminal.presets.*] section.
pub fn load_toml_presets(path: &std::path::Path) -> Option<toml::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    let raw: toml::Value = toml::Value::Table(content.parse::<toml::Table>().ok()?);
    let terminal = raw.as_table()?.get("terminal")?.as_table()?;
    let presets = terminal.get("presets")?;
    if presets.is_table() {
        Some(presets.clone())
    } else {
        None
    }
}

/// Build the canonical default TOML structure
fn default_toml_structure() -> toml::Value {
    let toml_str = r#"[terminal]
active = "default"

[relay]
url = ""
id = ""
token = ""
psk = ""
enabled = true

[launch]
tag = ""
project = ""
hints = ""
notes = ""
subagent_timeout = 30
auto_subscribe = "collision"

[launch.claude]
args = ""

[launch.gemini]
args = ""
system_prompt = ""

[launch.codex]
args = ""
sandbox_mode = "workspace"
system_prompt = ""

[launch.opencode]
args = ""

[launch.kilo]
args = ""

[launch.cline]
args = ""

[preferences]
timeout = 86400
auto_approve = true
name_export = ""
"#;
    toml::Value::Table(toml_str.parse::<toml::Table>().unwrap())
}

/// Check if a string value is falsy
/// Check if a terminal name matches a known built-in preset (case-insensitive).
/// Public alias for use by status command.
pub fn is_known_terminal_preset_pub(name: &str) -> bool {
    is_known_terminal_preset(name)
}

/// Check if a terminal name matches a known built-in preset (case-insensitive).
fn is_known_terminal_preset(name: &str) -> bool {
    TERMINAL_PRESETS
        .iter()
        .any(|(p, _)| p.eq_ignore_ascii_case(name))
}

/// Resolve old casing to canonical preset name (e.g., "WezTerm" → "wezterm").
/// Returns the canonical name if matched, otherwise returns the input unchanged.
fn normalize_terminal_case(name: &str) -> String {
    for &(preset, _) in TERMINAL_PRESETS.iter() {
        if preset.eq_ignore_ascii_case(name) {
            return preset.to_string();
        }
    }
    name.to_string()
}

/// Check if a terminal name matches a user-defined preset in config.toml.
pub fn is_user_defined_preset(name: &str) -> bool {
    let toml_path = paths::config_toml_path();
    if let Some(presets_val) = load_toml_presets(&toml_path) {
        if let Some(table) = presets_val.as_table() {
            return table.keys().any(|k| k.eq_ignore_ascii_case(name));
        }
    }
    false
}

/// Get the pane_id_env for a preset, checking TOML overrides then built-in defaults.
pub fn get_merged_preset_pane_id_env(name: &str) -> Option<String> {
    get_merged_preset(name).and_then(|p| p.pane_id_env)
}

/// Get a fully merged terminal preset: TOML overrides on top of built-in defaults.
///
/// Returns None if the name matches neither a TOML preset nor a built-in preset.
pub fn get_merged_preset(name: &str) -> Option<MergedPreset> {
    let toml_path = paths::config_toml_path();
    let toml_preset = load_toml_presets(&toml_path).and_then(|presets| {
        let table = presets.as_table()?;
        let val = table
            .get(name)
            .or_else(|| {
                table
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })?
            .as_table()?;
        Some(TomlPresetFields {
            binary: val
                .get("binary")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            app_name: val
                .get("app_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            open: val
                .get("open")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            close: val
                .get("close")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            pane_id_env: val
                .get("pane_id_env")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    });

    let builtin = crate::shared::get_terminal_preset(name);

    match (&toml_preset, &builtin) {
        (None, None) => None,
        _ => {
            let b_open = builtin.map(|b| b.open).unwrap_or("");
            let b_close = builtin.and_then(|b| b.close);
            let b_binary = builtin.and_then(|b| b.binary);
            let b_app = builtin.and_then(|b| b.app_name);
            let b_pane_env = builtin.and_then(|b| b.pane_id_env);

            let t = toml_preset.as_ref();
            Some(MergedPreset {
                open: t
                    .and_then(|t| t.open.clone())
                    .unwrap_or_else(|| b_open.to_string()),
                close: t
                    .and_then(|t| t.close.clone())
                    .or_else(|| b_close.map(|s| s.to_string())),
                binary: t
                    .and_then(|t| t.binary.clone())
                    .or_else(|| b_binary.map(|s| s.to_string())),
                app_name: t
                    .and_then(|t| t.app_name.clone())
                    .or_else(|| b_app.map(|s| s.to_string())),
                pane_id_env: t
                    .and_then(|t| t.pane_id_env.clone())
                    .or_else(|| b_pane_env.map(|s| s.to_string())),
            })
        }
    }
}

/// Parsed TOML preset fields (all optional — overlay on built-in).
struct TomlPresetFields {
    binary: Option<String>,
    app_name: Option<String>,
    open: Option<String>,
    close: Option<String>,
    pane_id_env: Option<String>,
}

/// Fully merged terminal preset (TOML + built-in).
#[derive(Debug, Clone)]
pub struct MergedPreset {
    pub open: String,
    pub close: Option<String>,
    pub binary: Option<String>,
    pub app_name: Option<String>,
    pub pane_id_env: Option<String>,
}

fn is_falsy(s: &str) -> bool {
    matches!(s, "0" | "false" | "False" | "no" | "off" | "")
}

/// Structured snapshot of config state for load/save operations.
#[derive(Clone, Debug)]
pub struct ConfigSnapshot {
    pub core: HcomConfig,
}

/// Load config snapshot from files (no env overrides — file contents only).
pub fn load_config_snapshot() -> ConfigSnapshot {
    let toml_path = paths::config_toml_path();

    if !toml_path.exists() {
        // Check for legacy config.env before writing defaults — don't overwrite
        // user settings that haven't been migrated yet.
        let config_env_path = Config::get().hcom_dir.join("config.env");
        if !config_env_path.exists() {
            let _ = write_default_config();
        }
    }

    let file_config = if toml_path.exists() {
        load_toml_config(&toml_path)
    } else {
        HashMap::new()
    };

    // Build HcomConfig from file values only (no env)
    let core = match HcomConfig::load_from_sources(&file_config, Some(&HashMap::new())) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            HcomConfig::default()
        }
    };

    ConfigSnapshot { core }
}

/// Write default config.toml + env file.
pub fn write_default_config() -> std::io::Result<()> {
    let config = HcomConfig::default();
    save_toml_config(&config, None)?;
    save_env_file(&HashMap::new())
}

const ENV_HEADER: &str = "# Env vars passed through to agents (e.g. ANTHROPIC_MODEL=...)\n";
const DEFAULT_ENV_VARS: &[&str] = &[
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "GEMINI_MODEL",
];

/// Load non-HCOM env vars from env file.
pub fn load_env_extras(path: &std::path::Path) -> HashMap<String, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };

    let mut result = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() && !key.starts_with("HCOM_") {
                result.insert(key.to_string(), parse_env_value(value));
            }
        }
    }
    result
}

/// Write env passthrough file (non-HCOM vars only).
pub fn save_env_file(extras: &HashMap<String, String>) -> std::io::Result<()> {
    let env_path = Config::get().hcom_dir.join("env");

    if let Some(parent) = env_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut lines = vec![ENV_HEADER.to_string()];

    // Always include default placeholders
    let mut all_keys: Vec<String> = DEFAULT_ENV_VARS.iter().map(|s| s.to_string()).collect();
    for key in extras.keys() {
        if !all_keys.contains(key) && !key.starts_with("HCOM_") {
            all_keys.push(key.clone());
        }
    }

    for key in &all_keys {
        if key.starts_with("HCOM_") {
            continue;
        }
        let value = extras.get(key.as_str()).map(|s| s.as_str()).unwrap_or("");
        let formatted = format_env_value(value);
        if formatted.is_empty() {
            lines.push(format!("{key}="));
        } else {
            lines.push(format!("{key}={formatted}"));
        }
    }

    let content = lines.join("\n") + "\n";
    atomic_write(&env_path, &content)
}

/// Parse ENV file value with proper quote and escape handling.
fn parse_env_value(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }

    // Double-quoted: unescape
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let inner = &value[1..value.len() - 1];
        return inner
            .replace("\\\\", "\x00")
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
            .replace('\x00', "\\");
    }

    // Single-quoted: literal
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].to_string();
    }

    value.to_string()
}

/// Format value for ENV file with proper quoting (inverse of parse_env_value).
fn format_env_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }

    let needs_quoting = value.contains(['\n', '\t', '"', '\'', ' ', '\r']);

    if needs_quoting {
        let escaped = value
            .replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
            .replace('\r', "\\r")
            .replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

/// Atomic write: delegates to paths::atomic_write_io (preserves error detail).
fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    crate::paths::atomic_write_io(path, content)
}

pub fn write_config_toml_path(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    atomic_write(path, content)?;
    lock_down_config_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn lock_down_config_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn lock_down_config_permissions(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::isolated_test_env;
    use serial_test::serial;
    use std::env;

    /// Helper to set env var for test scope
    fn with_env<F>(key: &str, value: &str, f: F)
    where
        F: FnOnce(),
    {
        // SAFETY: Tests use serial_test to run single-threaded.
        unsafe {
            env::set_var(key, value);
        }
        f();
        unsafe {
            env::remove_var(key);
        }
    }

    /// Helper to clear multiple env vars for test scope
    fn without_env<F>(keys: &[&str], f: F)
    where
        F: FnOnce(),
    {
        let saved: Vec<_> = keys.iter().map(|k| (*k, env::var(k).ok())).collect();
        for key in keys {
            unsafe {
                env::remove_var(key);
            }
        }
        f();
        for (key, val) in saved {
            if let Some(v) = val {
                unsafe {
                    env::set_var(key, v);
                }
            }
        }
    }

    #[test]
    #[serial]
    fn test_default_config_uses_home_hcom() {
        Config::reset();
        without_env(&["HCOM_DIR"], || {
            Config::init();
            let config = Config::get();
            let expected = env::var("HOME")
                .map(|h| PathBuf::from(h).join(".hcom"))
                .unwrap();
            assert_eq!(config.hcom_dir, expected);
        });
    }

    #[test]
    #[serial]
    fn test_hcom_dir_overrides_home() {
        Config::reset();
        with_env("HCOM_DIR", "/custom/hcom", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.hcom_dir, PathBuf::from("/custom/hcom"));
        });
    }

    #[test]
    #[serial]
    fn test_instance_name_some_when_set() {
        Config::reset();
        with_env("HCOM_INSTANCE_NAME", "test-instance", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.instance_name, Some("test-instance".to_string()));
        });
    }

    #[test]
    #[serial]
    fn test_instance_name_none_when_unset() {
        Config::reset();
        without_env(&["HCOM_INSTANCE_NAME"], || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.instance_name, None);
        });
    }

    #[test]
    #[serial]
    fn test_process_id_some_when_set() {
        Config::reset();
        with_env("HCOM_PROCESS_ID", "pid-123", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.process_id, Some("pid-123".to_string()));
        });
    }

    #[test]
    #[serial]
    fn test_process_id_none_when_unset() {
        Config::reset();
        without_env(&["HCOM_PROCESS_ID"], || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.process_id, None);
        });
    }

    #[test]
    #[serial]
    fn test_reset_allows_reinit() {
        Config::reset();
        with_env("HCOM_INSTANCE_NAME", "first", || {
            Config::init();
            assert_eq!(Config::get().instance_name, Some("first".to_string()));
        });

        Config::reset();
        with_env("HCOM_INSTANCE_NAME", "second", || {
            Config::init();
            assert_eq!(Config::get().instance_name, Some("second".to_string()));
        });
    }

    #[test]
    #[serial]
    fn test_hcom_dir_tilde_expansion() {
        Config::reset();
        with_env("HCOM_DIR", "~/.hcom", || {
            Config::init();
            let config = Config::get();
            assert!(config.hcom_dir.is_absolute());
            assert!(config.hcom_dir.ends_with(".hcom"));
        });
    }

    #[test]
    #[serial]
    fn test_hcom_dir_relative_resolved_to_absolute() {
        Config::reset();
        with_env("HCOM_DIR", "relative/path", || {
            Config::init();
            let config = Config::get();
            // Should be resolved relative to CWD
            assert!(config.hcom_dir.is_absolute());
            assert!(config.hcom_dir.ends_with("relative/path"));
        });
    }

    #[test]
    #[serial]
    fn test_hcom_dir_absolute_stays_absolute() {
        Config::reset();
        with_env("HCOM_DIR", "/absolute/hcom", || {
            Config::init();
            let config = Config::get();
            assert_eq!(config.hcom_dir, PathBuf::from("/absolute/hcom"));
        });
    }

    #[test]
    fn test_hcom_config_defaults() {
        let mut config = HcomConfig::default();
        assert_eq!(config.timeout, 86400);
        assert_eq!(config.subagent_timeout, 30);
        assert_eq!(config.terminal, "default");
        assert_eq!(config.tag, "");
        assert_eq!(config.codex_sandbox_mode, "workspace");
        assert!(config.relay_enabled);
        assert!(config.auto_approve);
        assert_eq!(config.auto_subscribe, "collision");
        assert!(config.collect_errors().is_empty());
    }

    #[test]
    fn test_hcom_config_validation_timeout() {
        let mut config = HcomConfig {
            timeout: 0,
            ..HcomConfig::default()
        };
        let errors = config.collect_errors();
        assert!(errors.contains_key("timeout"));

        config.timeout = 86401;
        let errors = config.collect_errors();
        assert!(errors.contains_key("timeout"));

        config.timeout = 3600;
        let errors = config.collect_errors();
        assert!(!errors.contains_key("timeout"));
    }

    #[test]
    fn test_hcom_config_validation_tag() {
        let mut config = HcomConfig {
            tag: "valid-tag".to_string(),
            ..HcomConfig::default()
        };
        assert!(!config.collect_errors().contains_key("tag"));

        config.tag = "invalid tag!".to_string();
        assert!(config.collect_errors().contains_key("tag"));

        config.tag = "".to_string(); // empty is valid
        assert!(!config.collect_errors().contains_key("tag"));
    }

    #[test]
    fn test_hcom_config_validation_sandbox_mode() {
        let mut config = HcomConfig::default();

        for mode in VALID_SANDBOX_MODES {
            config.codex_sandbox_mode = mode.to_string();
            assert!(
                !config.collect_errors().contains_key("codex_sandbox_mode"),
                "mode '{mode}' should be valid"
            );
        }

        config.codex_sandbox_mode = "invalid".to_string();
        assert!(config.collect_errors().contains_key("codex_sandbox_mode"));
    }

    #[test]
    fn test_hcom_config_validation_shell_args() {
        let mut config = HcomConfig {
            claude_args: "--model opus".to_string(),
            ..HcomConfig::default()
        };
        assert!(!config.collect_errors().contains_key("claude_args"));

        config.claude_args = "unclosed 'quote".to_string();
        assert!(config.collect_errors().contains_key("claude_args"));
    }

    #[test]
    fn test_hcom_config_validation_auto_subscribe() {
        let mut config = HcomConfig {
            auto_subscribe: "collision,created".to_string(),
            ..HcomConfig::default()
        };
        assert!(!config.collect_errors().contains_key("auto_subscribe"));

        config.auto_subscribe = "bad preset!".to_string();
        assert!(config.collect_errors().contains_key("auto_subscribe"));
    }

    #[test]
    fn test_terminal_case_normalization() {
        let mut config = HcomConfig {
            terminal: "WezTerm".to_string(),
            ..HcomConfig::default()
        };
        let errors = config.collect_errors();
        assert!(!errors.contains_key("terminal"));
        assert_eq!(config.terminal, "wezterm"); // Normalized

        config.terminal = "Alacritty".to_string();
        let errors = config.collect_errors();
        assert!(!errors.contains_key("terminal"));
        assert_eq!(config.terminal, "alacritty");

        config.terminal = "KITTY".to_string();
        let errors = config.collect_errors();
        assert!(!errors.contains_key("terminal"));
        assert_eq!(config.terminal, "kitty");
    }

    #[test]
    fn test_terminal_custom_command_requires_script() {
        let mut config = HcomConfig {
            terminal: "my-terminal -e bash {script}".to_string(),
            ..HcomConfig::default()
        };
        assert!(!config.collect_errors().contains_key("terminal"));

        // Unknown name without {script} is rejected
        config.terminal = "not-a-preset".to_string();
        assert!(config.collect_errors().contains_key("terminal"));
    }

    #[test]
    fn test_terminal_known_presets_accepted() {
        let mut config = HcomConfig::default();
        for preset in &[
            "kitty",
            "wezterm",
            "tmux",
            "alacritty",
            "terminal.app",
            "iterm",
        ] {
            config.terminal = preset.to_string();
            assert!(
                !config.collect_errors().contains_key("terminal"),
                "preset '{preset}' should be valid"
            );
        }
    }

    #[test]
    fn test_set_field_full_auto_normalization() {
        let mut config = HcomConfig::default();
        config.set_field("codex_sandbox_mode", "full-auto").unwrap();
        assert_eq!(config.codex_sandbox_mode, "workspace");
    }

    #[test]
    fn test_set_field_bool_coercion() {
        let mut config = HcomConfig::default();

        config.set_field("auto_approve", "0").unwrap();
        assert!(!config.auto_approve);

        config.set_field("auto_approve", "1").unwrap();
        assert!(config.auto_approve);

        config.set_field("auto_approve", "false").unwrap();
        assert!(!config.auto_approve);

        config.set_field("auto_approve", "yes").unwrap();
        assert!(config.auto_approve);

        config.set_field("relay_enabled", "off").unwrap();
        assert!(!config.relay_enabled);

        config.set_field("relay_enabled", "on").unwrap();
        assert!(config.relay_enabled);
    }

    #[test]
    fn test_is_falsy() {
        assert!(is_falsy("0"));
        assert!(is_falsy("false"));
        assert!(is_falsy("False"));
        assert!(is_falsy("no"));
        assert!(is_falsy("off"));
        assert!(is_falsy(""));
        assert!(!is_falsy("1"));
        assert!(!is_falsy("true"));
        assert!(!is_falsy("yes"));
        assert!(!is_falsy("on"));
    }

    #[test]
    fn test_to_env_dict_roundtrip() {
        let config = HcomConfig::default();
        let dict = config.to_env_dict();

        assert_eq!(dict.get("HCOM_TIMEOUT"), Some(&"86400".to_string()));
        assert_eq!(dict.get("HCOM_TERMINAL"), Some(&"default".to_string()));
        assert_eq!(dict.get("HCOM_AUTO_APPROVE"), Some(&"1".to_string()));
        assert_eq!(dict.get("HCOM_RELAY_ENABLED"), Some(&"1".to_string()));

        let roundtrip = HcomConfig::from_env_dict(&dict).unwrap();
        assert_eq!(config, roundtrip);
    }

    #[test]
    fn test_to_env_dict_never_exposes_relay_psk() {
        // The PSK is the decrypt/forge authority for the whole relay group.
        // `build_launch_env` feeds `to_env_dict` into every spawned child
        // process's environment, so anything emitted here crosses a
        // process boundary. The PSK must stay file-only — verified by
        // checking that even a populated field is suppressed.
        let config = HcomConfig {
            relay_psk: "an-example-secret-value-xxxxxxxxxxxxxxxxxxxxxxxx".to_string(),
            ..Default::default()
        };
        let dict = config.to_env_dict();
        assert!(!dict.contains_key("HCOM_RELAY_PSK"));
        for v in dict.values() {
            assert!(
                !v.contains("an-example-secret-value"),
                "PSK leaked into launch env dict: {v}"
            );
        }
    }

    #[test]
    fn test_load_from_sources_empty() {
        let file_config = HashMap::new();
        let env = HashMap::new();
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).unwrap();
        assert_eq!(config, HcomConfig::default());
    }

    #[test]
    fn test_load_from_sources_toml_values() {
        let mut file_config = HashMap::new();
        file_config.insert("timeout".to_string(), TomlFieldValue::Int(3600));
        file_config.insert("tag".to_string(), TomlFieldValue::Str("test".to_string()));
        file_config.insert("relay_enabled".to_string(), TomlFieldValue::Bool(false));

        let env = HashMap::new();
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).unwrap();

        assert_eq!(config.timeout, 3600);
        assert_eq!(config.tag, "test");
        assert!(!config.relay_enabled);
    }

    #[test]
    fn test_load_from_sources_env_overrides_toml() {
        let mut file_config = HashMap::new();
        file_config.insert("timeout".to_string(), TomlFieldValue::Int(3600));
        file_config.insert(
            "tag".to_string(),
            TomlFieldValue::Str("file-tag".to_string()),
        );

        let mut env = HashMap::new();
        env.insert("HCOM_TAG".to_string(), "env-tag".to_string());

        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).unwrap();

        assert_eq!(config.timeout, 3600); // From file (no env override)
        assert_eq!(config.tag, "env-tag"); // Env wins over file
    }

    #[test]
    fn test_load_from_sources_relay_fields_file_only() {
        let mut file_config = HashMap::new();
        file_config.insert(
            "relay".to_string(),
            TomlFieldValue::Str("mqtt://file.example.com".to_string()),
        );

        let mut env = HashMap::new();
        env.insert(
            "HCOM_RELAY".to_string(),
            "mqtt://env.example.com".to_string(),
        );

        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).unwrap();

        // Relay fields should come from file, not env
        assert_eq!(config.relay, "mqtt://file.example.com");
    }

    #[test]
    fn test_load_from_sources_int_coercion() {
        let mut file_config = HashMap::new();
        file_config.insert(
            "timeout".to_string(),
            TomlFieldValue::Str("7200".to_string()),
        );

        let env = HashMap::new();
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).unwrap();
        assert_eq!(config.timeout, 7200);
    }

    #[test]
    fn test_load_from_sources_bool_string_coercion() {
        let mut file_config = HashMap::new();
        file_config.insert(
            "auto_approve".to_string(),
            TomlFieldValue::Str("0".to_string()),
        );

        let env = HashMap::new();
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).unwrap();
        assert!(!config.auto_approve);
    }

    #[test]
    fn test_load_from_sources_sandbox_mode_empty_uses_default() {
        let mut file_config = HashMap::new();
        file_config.insert(
            "codex_sandbox_mode".to_string(),
            TomlFieldValue::Str("".to_string()),
        );

        let env = HashMap::new();
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).unwrap();
        assert_eq!(config.codex_sandbox_mode, "workspace"); // Default, not empty
    }

    #[test]
    fn test_load_from_sources_terminal_empty_uses_default() {
        let mut file_config = HashMap::new();
        file_config.insert("terminal".to_string(), TomlFieldValue::Str("".to_string()));

        let env = HashMap::new();
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).unwrap();
        assert_eq!(config.terminal, "default");
    }

    #[test]
    fn test_toml_roundtrip() {
        let config = HcomConfig {
            timeout: 3600,
            tag: "dev".to_string(),
            auto_approve: false,
            relay: "mqtt://test.com".to_string(),
            ..HcomConfig::default()
        };

        let toml_table = config.to_toml_table();
        let toml_str = toml::to_string_pretty(&toml_table).unwrap();

        // Parse it back
        let parsed: toml::Value = toml::Value::Table(toml_str.parse::<toml::Table>().unwrap());
        let mut file_config = HashMap::new();
        for &(field_name, toml_path) in TOML_KEY_MAP {
            if let Some(val) = get_nested(&parsed, toml_path) {
                let typed = match &val {
                    toml::Value::String(s) => TomlFieldValue::Str(s.clone()),
                    toml::Value::Integer(i) => TomlFieldValue::Int(*i),
                    toml::Value::Boolean(b) => TomlFieldValue::Bool(*b),
                    _ => continue,
                };
                file_config.insert(field_name.to_string(), typed);
            }
        }

        let roundtrip = HcomConfig::load_from_sources(&file_config, Some(&HashMap::new())).unwrap();
        assert_eq!(config, roundtrip);
    }

    #[test]
    fn test_load_toml_config_with_dangerous_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[terminal]
active = "echo `whoami`"
[preferences]
timeout = 3600
"#,
        )
        .unwrap();

        let result = load_toml_config(&path);
        // Terminal with dangerous chars should be removed
        assert!(!result.contains_key("terminal"));
        // Other values should load fine
        assert!(result.contains_key("timeout"));
    }

    #[test]
    fn test_load_toml_config_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[terminal]
active = "kitty"

[launch]
tag = "myteam"
subagent_timeout = 60

[launch.claude]
args = "--model opus"

[preferences]
timeout = 7200
auto_approve = false
"#,
        )
        .unwrap();

        let result = load_toml_config(&path);
        assert_eq!(
            result.get("terminal").map(|v| v.as_string()),
            Some("kitty".to_string())
        );
        assert_eq!(
            result.get("tag").map(|v| v.as_string()),
            Some("myteam".to_string())
        );
        assert_eq!(
            result.get("claude_args").map(|v| v.as_string()),
            Some("--model opus".to_string())
        );
        assert_eq!(
            result.get("timeout").map(|v| v.as_string()),
            Some("7200".to_string())
        );
    }

    #[test]
    fn test_load_toml_config_missing_file() {
        let result = load_toml_config(std::path::Path::new("/nonexistent/config.toml"));
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_toml_config_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not valid toml [[[[").unwrap();

        let result = load_toml_config(&path);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_env_value_unquoted() {
        assert_eq!(parse_env_value("hello"), "hello");
        assert_eq!(parse_env_value("  hello  "), "hello");
    }

    #[test]
    fn test_parse_env_value_double_quoted() {
        assert_eq!(parse_env_value(r#""hello world""#), "hello world");
        assert_eq!(parse_env_value(r#""line1\nline2""#), "line1\nline2");
        assert_eq!(parse_env_value(r#""tab\there""#), "tab\there");
        assert_eq!(parse_env_value(r#""escaped\"quote""#), "escaped\"quote");
    }

    #[test]
    fn test_parse_env_value_single_quoted() {
        assert_eq!(parse_env_value("'literal'"), "literal");
        assert_eq!(parse_env_value(r"'no\nescaping'"), r"no\nescaping");
    }

    #[test]
    fn test_format_env_value_simple() {
        assert_eq!(format_env_value("hello"), "hello");
        assert_eq!(format_env_value(""), "");
    }

    #[test]
    fn test_format_env_value_needs_quoting() {
        assert_eq!(format_env_value("hello world"), "\"hello world\"");
        assert_eq!(format_env_value("line1\nline2"), "\"line1\\nline2\"");
    }

    #[test]
    fn test_get_field_all_fields() {
        let config = HcomConfig::default();
        // All 20 fields should be gettable
        for &(field, _) in FIELD_TO_ENV {
            assert!(
                config.get_field(field).is_some(),
                "get_field('{field}') should return Some"
            );
        }
        assert!(config.get_field("nonexistent").is_none());
    }

    #[test]
    fn test_hcom_config_from_env_dict_with_full_auto() {
        let mut data = HcomConfig::default().to_env_dict();
        data.insert(
            "HCOM_CODEX_SANDBOX_MODE".to_string(),
            "full-auto".to_string(),
        );
        let config = HcomConfig::from_env_dict(&data).unwrap();
        assert_eq!(config.codex_sandbox_mode, "workspace");
    }

    #[test]
    fn test_hcom_config_validation_error_display() {
        let errors = HashMap::from([
            ("timeout".to_string(), "timeout must be 1-86400".to_string()),
            ("tag".to_string(), "tag invalid chars".to_string()),
        ]);
        let err = HcomConfigError { errors };
        let display = format!("{err}");
        assert!(display.contains("Invalid config"));
        assert!(display.contains("timeout must be 1-86400"));
        assert!(display.contains("tag invalid chars"));
    }

    #[test]
    fn test_default_toml_structure() {
        let structure = default_toml_structure();
        // Verify key paths exist
        assert!(get_nested(&structure, "terminal.active").is_some());
        assert!(get_nested(&structure, "launch.tag").is_some());
        assert!(get_nested(&structure, "launch.claude.args").is_some());
        assert!(get_nested(&structure, "relay.url").is_some());
        assert!(get_nested(&structure, "preferences.timeout").is_some());
    }

    #[test]
    fn test_load_toml_presets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[terminal]
active = "default"

[terminal.presets.myterm]
open = "myterm spawn -- bash {script}"
close = "myterm kill --id {id}"
binary = "myterm"
"#,
        )
        .unwrap();

        let presets = load_toml_presets(&path);
        assert!(presets.is_some());
        let presets = presets.unwrap();
        assert!(presets.as_table().unwrap().contains_key("myterm"));
    }

    #[test]
    fn test_load_toml_presets_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[terminal]
active = "default"
"#,
        )
        .unwrap();

        let presets = load_toml_presets(&path);
        assert!(presets.is_none());
    }

    #[test]
    #[cfg(unix)]
    #[serial]
    fn test_save_toml_config_sets_mode_600_for_secret_bearing_config() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let config = HcomConfig {
            relay_psk: "super-secret-psk".to_string(),
            ..Default::default()
        };

        save_toml_config(&config, None).unwrap();

        let mode = std::fs::metadata(paths::config_toml_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
