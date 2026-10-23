//! Configuration management — central config system used by all modules.
//!
//! Two config layers:
//! - `Config`: Runtime env vars (HCOM_DIR, HCOM_INSTANCE_NAME, etc.) — startup-only, used by router/client
//! - `HcomConfig`: User config from TOML + env vars — the user-facing settings in [`FIELDS`]

use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

static RE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9-]+$").unwrap());
static RE_PRESET_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_]+$").unwrap());

use crate::{paths, tool::Tool};

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
    #[track_caller]
    pub fn reset() {
        crate::hooks::test_helpers::assert_serial();
        *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Load configuration from environment variables
    fn from_env() -> Self {
        use std::env;

        let env_map: HashMap<String, String> = env::vars().collect();
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let (hcom_dir, _) = paths::resolve_hcom_dir_from_env(&env_map, &cwd);

        // Unit tests must never inherit a real hcom data directory. Raw path
        // semantics are tested through `resolve_hcom_dir_from_env` directly, so
        // global Config accepts only roots a test fixture explicitly registered
        // as disposable — not merely "it lives under $TMPDIR", since a real DB
        // can sit under the temp tree too. Anything else redirects to a
        // process-local throwaway. Production builds do not compile this branch.
        //
        // Redirect rather than panic on an unregistered dir: countless tests
        // read Config with no HCOM_DIR set and no isolation installed, and must
        // land on a safe throwaway instead of aborting. Every explicit consumer
        // registers its root, so the fallback is a backstop, never the norm.
        #[cfg(test)]
        let hcom_dir = {
            if paths::test_roots::is_registered(&hcom_dir) {
                hcom_dir
            } else {
                test_default_hcom_dir()
            }
        };

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

/// Process-local fallback for unit tests that do not install an isolated
/// `HCOM_DIR`. Reusing one directory per test binary preserves Config's normal
/// process-wide semantics. Backed by a retained `TempDir` so it gets a unique,
/// uncontended name and is registered as a disposable root for the redirect.
#[cfg(test)]
fn test_default_hcom_dir() -> PathBuf {
    use std::sync::OnceLock;

    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::Builder::new()
            .prefix("hcom-test-default-")
            .tempdir()
            .expect("create test-default hcom dir");
        paths::test_roots::register(dir.path());
        dir
    })
    .path()
    .to_path_buf()
}

/// Value type of a config field, as stored in config.toml.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldKind {
    Str,
    Int,
    Bool,
}

impl FieldKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FieldKind::Str => "string",
            FieldKind::Int => "integer",
            FieldKind::Bool => "boolean",
        }
    }
}

/// One `HcomConfig` field. [`FIELDS`] is the only list of config fields:
/// loading, saving, env export, `hcom config` display/set and help all
/// derive from it.
#[derive(Debug)]
pub(crate) struct ConfigField {
    /// `HcomConfig` field name, also the `hcom config <key>` short name.
    pub name: &'static str,
    /// `HCOM_*` env var. Relay fields are never read from or exported to env.
    pub env: &'static str,
    /// Dotted path in config.toml.
    pub toml: &'static str,
    pub kind: FieldKind,
    /// Set only for generated launch args settings.
    pub tool: Option<Tool>,
    pub desc: &'static str,
}

impl ConfigField {
    /// Relay fields are file-only and managed by `hcom relay`.
    pub(crate) fn is_relay(&self) -> bool {
        self.name.starts_with("relay")
    }
}

const fn f(
    name: &'static str,
    env: &'static str,
    toml: &'static str,
    kind: FieldKind,
    desc: &'static str,
) -> ConfigField {
    ConfigField {
        name,
        env,
        toml,
        kind,
        tool: None,
        desc,
    }
}

use FieldKind::{Bool, Int, Str};

/// Metadata owned once; `FIELDS` borrows it for the same static field interface.
struct ToolArgsField {
    tool: Tool,
    name: String,
    env: &'static str,
    toml: String,
    desc: String,
}

static TOOL_ARGS_FIELDS: LazyLock<Vec<ToolArgsField>> = LazyLock::new(|| {
    crate::integration_spec::ALL
        .iter()
        .filter_map(|spec| {
            Some(ToolArgsField {
                tool: spec.tool,
                name: format!("{}_args", spec.name),
                env: spec.launch.args_env?,
                toml: format!("launch.{}.args", spec.name),
                desc: format!("Default args for {} on launch", spec.cli_binary),
            })
        })
        .collect()
});

/// All config fields, in display order.
#[rustfmt::skip]
pub(crate) static FIELDS: LazyLock<Vec<ConfigField>> = LazyLock::new(|| {
    let mut fields = vec![
    f("terminal", "HCOM_TERMINAL", "terminal.active", Str, "Terminal preset for spawning agent panes"),
    f("title_mode", "HCOM_TITLE_MODE", "terminal.title_mode", Str, "Terminal title mode (combined | label | off)"),
    f("tag", "HCOM_TAG", "launch.tag", Str, "Group tag for launched instances"),
    f("hints", "HCOM_HINTS", "launch.hints", Str, "Text injected with all messages"),
    f("notes", "HCOM_NOTES", "launch.notes", Str, "One-time notes appended at bootstrap"),
    f("bootstrap_exclude", "HCOM_BOOTSTRAP_EXCLUDE", "launch.bootstrap_exclude", Str, "Bootstrap parts to exclude (comma-separated, none, or all)"),
    f("subagent_timeout", "HCOM_SUBAGENT_TIMEOUT", "launch.subagent_timeout", Int, "Timeout for Claude subagents in seconds (default: 30)"),
    f("auto_subscribe", "HCOM_AUTO_SUBSCRIBE", "launch.auto_subscribe", Str, "Auto-subscribe event presets (comma-separated)"),
    f("auto_trust_workspace", "HCOM_AUTO_TRUST_WORKSPACE", "launch.auto_trust_workspace", Bool, "Auto-approve workspace trust at launch; may persist in tool config"),
    ];
    for field in TOOL_ARGS_FIELDS.iter() {
        fields.push(ConfigField {
            name: &field.name,
            env: field.env,
            toml: &field.toml,
            kind: Str,
            tool: Some(field.tool),
            desc: &field.desc,
        });
    }
    fields.extend([
    f("claude_delivery", "HCOM_CLAUDE_DELIVERY", "launch.claude.idle_delivery", Str, "Idle Claude delivery for new launches (native | inject)"),
    f("copilot_delivery", "HCOM_COPILOT_DELIVERY", "launch.copilot.idle_delivery", Str, "Idle Copilot delivery for new launches (native | inject)"),
    f("gemini_system_prompt", "HCOM_GEMINI_SYSTEM_PROMPT", "launch.gemini.system_prompt", Str, "System prompt for gemini on launch"),
    f("codex_system_prompt", "HCOM_CODEX_SYSTEM_PROMPT", "launch.codex.system_prompt", Str, "System prompt for codex on launch"),
    f("timeout", "HCOM_TIMEOUT", "preferences.timeout", Int, "Idle timeout in seconds (default: 86400)"),
    f("auto_approve", "HCOM_AUTO_APPROVE", "preferences.auto_approve", Bool, "Auto-approve safe hcom commands"),
    f("name_export", "HCOM_NAME_EXPORT", "preferences.name_export", Str, "Export instance name to custom env var"),
    f("relay", "HCOM_RELAY", "relay.url", Str, "Relay MQTT broker URL (managed by hcom relay)"),
    f("relay_id", "HCOM_RELAY_ID", "relay.id", Str, "Relay group identifier (managed by hcom relay)"),
    f("relay_token", "HCOM_RELAY_TOKEN", "relay.token", Str, "Relay authentication token (managed by hcom relay)"),
    f("relay_enabled", "HCOM_RELAY_ENABLED", "relay.enabled", Bool, "Enable relay sync (managed by hcom relay)"),
    f("relay_share_account", "HCOM_RELAY_SHARE_ACCOUNT", "relay.share_account", Bool, "Share agents' logged-in account e-mail with relay peers"),
    // Forge/decrypt authority for the whole relay group: never shown by
    // `hcom config`, never settable through it, never exported to env.
    f("relay_psk", "HCOM_RELAY_PSK", "relay.psk", Str, "Relay pre-shared key"),
    ]);
    fields
});

/// Fields `hcom config` shows and accepts (everything but the relay PSK).
pub(crate) fn user_fields() -> impl Iterator<Item = &'static ConfigField> {
    FIELDS.iter().filter(|f| f.name != "relay_psk")
}

/// Field for an `hcom config` key: `tag`, `HCOM_TAG` or `hcom_tag`.
pub(crate) fn user_field(key: &str) -> Option<&'static ConfigField> {
    let upper = key.to_ascii_uppercase();
    let name = upper
        .strip_prefix("HCOM_")
        .unwrap_or(&upper)
        .to_ascii_lowercase();
    user_fields().find(|f| f.name == name)
}

/// Field → error message. Each invalid field falls back to its default.
pub type ConfigErrors = BTreeMap<String, String>;

/// Characters that are dangerous in terminal preset values (injection risk).
const TERMINAL_DANGEROUS_CHARS: &[char] = &['`', '$', ';', '|', '&', '\n', '\r'];

use crate::shared::terminal_presets::TERMINAL_PRESETS;

/// `<tool>_delivery` values: `native` (the tool's own API: mod, extension,
/// [`DEFAULT_DELIVERY`]) or `inject` (typing into the PTY).
pub const DELIVERY_MODES: &[&str] = &["native", "inject"];
pub const DEFAULT_DELIVERY: &str = "native";

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

/// HCOM user configuration with validation.
/// Load priority: env var → config.toml → defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct HcomConfig {
    pub timeout: i64,
    pub subagent_timeout: i64,
    pub terminal: String,
    pub hints: String,
    pub notes: String,
    pub bootstrap_exclude: String,
    pub tag: String,
    pub(crate) tool_args: HashMap<Tool, String>,
    /// How idle Claude agents are woken: `"native"` (default) through hcom's
    /// bundled Claude mod, `"inject"` by typing into the PTY. Fixed per launch.
    /// See [`DELIVERY_MODES`].
    pub claude_delivery: String,
    pub gemini_system_prompt: String,
    pub codex_system_prompt: String,
    pub relay: String,
    pub relay_id: String,
    pub relay_token: String,
    pub relay_psk: String,
    pub relay_enabled: bool,
    pub relay_share_account: bool,
    pub auto_approve: bool,
    pub auto_subscribe: String,
    pub name_export: String,
    pub auto_trust_workspace: bool,
    /// How Copilot agents get messages while idle: `"native"` (default) through
    /// hcom's Copilot CLI extension, `"inject"` by typing into the PTY.
    /// See [`DELIVERY_MODES`].
    pub copilot_delivery: String,
    /// Terminal-title behavior: `"combined"` (default) shows
    /// `{icon} name - {tool's live title}`, `"label"` shows hcom's
    /// `{icon} name [tool]` only, `"off"` leaves the tool's own title untouched.
    /// See [`crate::shared::TitleMode`].
    pub title_mode: String,
}

impl Default for HcomConfig {
    fn default() -> Self {
        Self {
            timeout: 86400,
            subagent_timeout: 30,
            terminal: "default".to_string(),
            hints: String::new(),
            notes: String::new(),
            bootstrap_exclude: "none".to_string(),
            tag: String::new(),
            tool_args: HashMap::new(),
            claude_delivery: DEFAULT_DELIVERY.to_string(),
            gemini_system_prompt: String::new(),
            codex_system_prompt: String::new(),
            relay: String::new(),
            relay_id: String::new(),
            relay_token: String::new(),
            relay_psk: String::new(),
            relay_enabled: true,
            relay_share_account: false,
            auto_approve: true,
            auto_subscribe: "collision".to_string(),
            name_export: String::new(),
            auto_trust_workspace: true,
            copilot_delivery: DEFAULT_DELIVERY.to_string(),
            title_mode: "combined".to_string(),
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
    pub fn collect_errors(&mut self) -> ConfigErrors {
        self.normalize();
        let mut errors = ConfigErrors::new();

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
            let platform = crate::shared::platform::platform_name();
            if let Some(error) = user_defined_preset_error(&self.terminal) {
                errors.insert(
                    "terminal".into(),
                    format!("invalid terminal preset '{}': {error}", self.terminal),
                );
            } else if is_user_defined_preset(&self.terminal) {
                // User TOML presets declare no platform and override any built-in
                // of the same name — exempt from the built-in platform gate.
            } else if is_known_terminal_preset(&self.terminal) {
                if !terminal_preset_supported_on(&self.terminal, platform) {
                    errors.insert(
                        "terminal".into(),
                        format!(
                            "terminal preset '{}' is not available on {}",
                            self.terminal, platform
                        ),
                    );
                }
            } else if !self.terminal.contains("{script}") {
                errors.insert(
                    "terminal".into(),
                    format!(
                        "terminal must be 'default', preset name, or custom command with {{script}}, got '{}'",
                        self.terminal
                    ),
                );
            }
        }

        for (field, mode) in [
            ("claude_delivery", &self.claude_delivery),
            ("copilot_delivery", &self.copilot_delivery),
        ] {
            if !DELIVERY_MODES.contains(&mode.as_str()) {
                errors.insert(
                    field.into(),
                    format!(
                        "{field} must be one of: {}. Got '{mode}'",
                        DELIVERY_MODES.join(", ")
                    ),
                );
            }
        }

        // Validate tag (alphanumeric + hyphens only)
        if !self.tag.is_empty() && !RE_TAG.is_match(&self.tag) {
            errors.insert(
                "tag".into(),
                "tag can only contain letters, numbers, and hyphens".into(),
            );
        }

        if !crate::shared::VALID_TITLE_MODES.contains(&self.title_mode.as_str()) {
            errors.insert(
                "title_mode".into(),
                format!(
                    "title_mode must be one of: {}. Got '{}'",
                    crate::shared::VALID_TITLE_MODES.join(", "),
                    self.title_mode
                ),
            );
        }

        // Validate shell-quoted args fields
        for spec in crate::integration_spec::ALL
            .iter()
            .filter(|s| s.launch.args_env.is_some())
        {
            let field = format!("{}_args", spec.name);
            let value = self.tool_args(spec.tool);
            if !value.is_empty()
                && let Err(e) = shell_words::split(value)
            {
                errors.insert(
                    field.clone(),
                    format!("{field} contains invalid shell quoting: {e}"),
                );
            }
        }

        if let Err(error) = crate::bootstrap::parse_excluded_parts(&self.bootstrap_exclude) {
            errors.insert("bootstrap_exclude".into(), error);
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

    /// Default launch args; empty for tools without an args setting.
    pub fn tool_args(&self, tool: Tool) -> &str {
        self.tool_args.get(&tool).map(String::as_str).unwrap_or("")
    }

    /// Get a field value by name (returns string representation).
    pub fn get_field(&self, field: &str) -> Option<String> {
        if let Some(tool) = user_field(field).and_then(|f| f.tool) {
            return Some(self.tool_args(tool).to_string());
        }
        match field {
            "timeout" => Some(self.timeout.to_string()),
            "subagent_timeout" => Some(self.subagent_timeout.to_string()),
            "terminal" => Some(self.terminal.clone()),
            "hints" => Some(self.hints.clone()),
            "notes" => Some(self.notes.clone()),
            "bootstrap_exclude" => Some(self.bootstrap_exclude.clone()),
            "tag" => Some(self.tag.clone()),
            "claude_delivery" => Some(self.claude_delivery.clone()),
            "gemini_system_prompt" => Some(self.gemini_system_prompt.clone()),
            "codex_system_prompt" => Some(self.codex_system_prompt.clone()),
            "relay" => Some(self.relay.clone()),
            "relay_id" => Some(self.relay_id.clone()),
            "relay_token" => Some(self.relay_token.clone()),
            "relay_psk" => Some(self.relay_psk.clone()),
            "relay_enabled" => Some(if self.relay_enabled { "1" } else { "0" }.into()),
            "relay_share_account" => Some(if self.relay_share_account { "1" } else { "0" }.into()),
            "auto_approve" => Some(if self.auto_approve { "1" } else { "0" }.into()),
            "auto_subscribe" => Some(self.auto_subscribe.clone()),
            "name_export" => Some(self.name_export.clone()),
            "auto_trust_workspace" => {
                Some(if self.auto_trust_workspace { "1" } else { "0" }.into())
            }
            "copilot_delivery" => Some(self.copilot_delivery.clone()),
            "title_mode" => Some(self.title_mode.clone()),
            _ => None,
        }
    }

    /// Set a field value by name. Returns Err if field unknown or value invalid type.
    pub fn set_field(&mut self, field: &str, value: &str) -> Result<(), String> {
        if let Some(tool) = user_field(field).and_then(|f| f.tool) {
            if value.is_empty() {
                self.tool_args.remove(&tool);
            } else {
                self.tool_args.insert(tool, value.to_string());
            }
            return Ok(());
        }
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
            "bootstrap_exclude" => self.bootstrap_exclude = value.to_string(),
            "tag" => self.tag = value.to_string(),
            "claude_delivery" => self.claude_delivery = value.to_string(),
            "gemini_system_prompt" => self.gemini_system_prompt = value.to_string(),
            "codex_system_prompt" => self.codex_system_prompt = value.to_string(),
            "relay" => self.relay = value.to_string(),
            "relay_id" => self.relay_id = value.to_string(),
            "relay_token" => self.relay_token = value.to_string(),
            "relay_psk" => self.relay_psk = value.to_string(),
            "relay_enabled" => self.relay_enabled = !is_falsy(value),
            "relay_share_account" => self.relay_share_account = !is_falsy(value),
            "auto_approve" => self.auto_approve = !is_falsy(value),
            "auto_subscribe" => self.auto_subscribe = value.to_string(),
            "name_export" => self.name_export = value.to_string(),
            "auto_trust_workspace" => self.auto_trust_workspace = !is_falsy(value),
            "copilot_delivery" => self.copilot_delivery = value.to_string(),
            // Validated in `collect_errors`, like the other enum-like fields.
            "title_mode" => self.title_mode = value.to_string(),
            _ => return Err(format!("unknown field: {field}")),
        }
        Ok(())
    }

    /// Effective HCOM_TIMEOUT (idle poll timeout for non-PTY instances). Used
    /// both by the Stop-hook poll fallback and by registration so freshly
    /// created rows already carry the resolved value instead of relying on
    /// the (never-NULL) schema default.
    pub fn effective_timeout() -> i64 {
        Self::load(None).timeout
    }

    /// Load config with precedence: env var → config.toml → defaults. Invalid
    /// values fall back to their defaults (see [`Self::load_with_errors`]).
    ///
    /// `env_override`: If Some, use this map for env var lookups instead of std::env.
    /// Used in daemon mode where os.environ is stale.
    pub fn load(env_override: Option<&HashMap<String, String>>) -> Self {
        Self::load_with_errors(env_override).0
    }

    /// [`Self::load`], plus each value that was invalid and fell back to its default.
    pub fn load_with_errors(
        env_override: Option<&HashMap<String, String>>,
    ) -> (Self, ConfigErrors) {
        let (file_values, mut errors) = read_toml_values(&ensure_config_file());
        // A bad config.toml value doesn't matter while env overrides it.
        errors.retain(|name, _| {
            !FIELDS
                .iter()
                .any(|f| f.name == name && env_value(f, env_override).is_some())
        });
        let (config, field_errors) = Self::load_from_sources(&file_values, env_override);
        errors.extend(field_errors);
        (config, errors)
    }

    /// Build from parsed config.toml values + env. An invalid value is
    /// reported and falls back to its default, so one bad setting never
    /// discards the others.
    pub(crate) fn load_from_sources(
        file_values: &HashMap<String, TomlFieldValue>,
        env_override: Option<&HashMap<String, String>>,
    ) -> (Self, ConfigErrors) {
        let mut config = HcomConfig::default();
        let mut errors = ConfigErrors::new();
        let mut sources: HashMap<&str, String> = HashMap::new();

        for field in FIELDS.iter() {
            let (val, source) = match env_value(field, env_override) {
                Some(v) => (TomlFieldValue::Str(v), format!("env {}", field.env)),
                None => match file_values.get(field.name) {
                    Some(v) => (v.clone(), format!("config.toml {}", field.toml)),
                    None => continue,
                },
            };
            // Empty means unset, except where "" is itself a choice: no
            // auto_subscribe presets, or false for a switch (e.g. HCOM_AUTO_APPROVE=).
            if val.as_string().is_empty()
                && field.kind != FieldKind::Bool
                && field.name != "auto_subscribe"
            {
                continue;
            }
            if let Err(e) = coerce(field, &val).and_then(|v| config.set_field(field.name, &v)) {
                errors.insert(field.name.to_string(), format!("{e} [{source}]"));
            }
            sources.insert(field.name, source);
        }

        let defaults = HcomConfig::default();
        for (field, msg) in config.collect_errors() {
            if let Some(default) = defaults.get_field(&field) {
                let _ = config.set_field(&field, &default);
            }
            let msg = match sources.get(field.as_str()) {
                Some(source) => format!("{msg} [{source}]"),
                None => msg,
            };
            errors.entry(field).or_insert(msg);
        }
        (config, errors)
    }

    /// HCOM_* env for launched agents. Relay fields are file-only on load, and
    /// the PSK must never cross a process boundary, so none are exported.
    pub fn to_env_dict(&self) -> HashMap<String, String> {
        FIELDS
            .iter()
            .filter(|f| !f.is_relay())
            .filter_map(|f| Some((f.env.to_string(), self.get_field(f.name)?)))
            .collect()
    }
}

/// `field`'s env var, from `env_override` or the process env. Relay fields
/// are file-only (no env override).
fn env_value(
    field: &ConfigField,
    env_override: Option<&HashMap<String, String>>,
) -> Option<String> {
    if field.is_relay() {
        None
    } else if let Some(overrides) = env_override {
        overrides.get(field.env).cloned()
    } else {
        std::env::var(field.env).ok()
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
    pub(crate) fn as_string(&self) -> String {
        match self {
            TomlFieldValue::Str(s) => s.clone(),
            TomlFieldValue::Int(i) => i.to_string(),
            TomlFieldValue::Bool(b) => b.to_string(),
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

/// Normalize a raw value to the string `set_field` takes for `field`'s kind.
fn coerce(field: &ConfigField, val: &TomlFieldValue) -> Result<String, String> {
    let as_bool = |b: bool| if b { "1" } else { "0" }.to_string();
    let parsed = match (field.kind, val) {
        (FieldKind::Str, v) => Some(v.as_string()),
        (FieldKind::Int, TomlFieldValue::Int(i)) => Some(i.to_string()),
        (FieldKind::Int, TomlFieldValue::Str(s)) => {
            s.trim().parse::<i64>().ok().map(|i| i.to_string())
        }
        (FieldKind::Bool, TomlFieldValue::Bool(b)) => Some(as_bool(*b)),
        (FieldKind::Bool, TomlFieldValue::Int(i)) => Some(as_bool(*i != 0)),
        (FieldKind::Bool, TomlFieldValue::Str(s)) => parse_bool(s).map(as_bool),
        _ => None,
    };
    parsed.ok_or_else(|| match field.kind {
        FieldKind::Bool => format!(
            "{} must be true or false, got '{}'",
            field.name,
            val.as_string()
        ),
        _ => format!(
            "{} must be an integer, got '{}'",
            field.name,
            val.as_string()
        ),
    })
}

/// config.toml value for `field` from user input (`hcom config <key> <value>`).
pub(crate) fn field_toml_value(field: &ConfigField, raw: &str) -> Result<toml_edit::Value, String> {
    let v = coerce(field, &TomlFieldValue::Str(raw.to_string()))?;
    Ok(match field.kind {
        FieldKind::Str => v.into(),
        FieldKind::Int => v.parse::<i64>().map_err(|e| e.to_string())?.into(),
        FieldKind::Bool => (v == "1").into(),
    })
}

/// Path to config.toml, writing defaults first if there is no config at all.
/// A legacy config.env without config.toml is left alone: writing defaults
/// there would silently drop the user's settings before migration.
fn ensure_config_file() -> PathBuf {
    let toml_path = paths::config_toml_path();
    if !toml_path.exists() && !Config::get().hcom_dir.join("config.env").exists() {
        let _ = write_default_config();
    }
    toml_path
}

/// Read config.toml into field name → typed value. Unreadable or invalid
/// content is reported in the errors, never printed (hooks call this).
pub(crate) fn read_toml_values(
    path: &std::path::Path,
) -> (HashMap<String, TomlFieldValue>, ConfigErrors) {
    match std::fs::read_to_string(path) {
        Ok(content) => toml_values_from_str(&content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (HashMap::new(), ConfigErrors::new()),
        Err(e) => (
            HashMap::new(),
            ConfigErrors::from([(
                "config.toml".to_string(),
                format!("config.toml is unreadable, using defaults: {e}"),
            )]),
        ),
    }
}

pub(crate) fn toml_values_from_str(
    content: &str,
) -> (HashMap<String, TomlFieldValue>, ConfigErrors) {
    let mut values = HashMap::new();
    let mut errors = ConfigErrors::new();

    let raw = match content.parse::<toml::Table>() {
        Ok(t) => toml::Value::Table(t),
        Err(e) => {
            errors.insert(
                "config.toml".into(),
                format!("config.toml is not valid TOML, using defaults: {e}"),
            );
            return (values, errors);
        }
    };

    for field in FIELDS.iter() {
        let Some(val) = get_nested(&raw, field.toml) else {
            continue;
        };
        let typed = match val {
            toml::Value::String(s) => TomlFieldValue::Str(s),
            toml::Value::Integer(i) => TomlFieldValue::Int(i),
            toml::Value::Boolean(b) => TomlFieldValue::Bool(b),
            other => {
                errors.insert(
                    field.name.into(),
                    format!(
                        "{} must be a {}, got {} [config.toml {}]",
                        field.name,
                        field.kind.as_str(),
                        other.type_str(),
                        field.toml
                    ),
                );
                continue;
            }
        };
        values.insert(field.name.to_string(), typed);
    }

    if let Some(TomlFieldValue::Str(terminal)) = values.get("terminal") {
        let bad: Vec<String> = TERMINAL_DANGEROUS_CHARS
            .iter()
            .filter(|&&c| terminal.contains(c))
            .map(|c| format!("{c:?}"))
            .collect();
        if !bad.is_empty() {
            errors.insert(
                "terminal".into(),
                format!(
                    "terminal contains unsafe characters ({}) [config.toml terminal.active]",
                    bad.join(", ")
                ),
            );
            values.remove("terminal");
        }
    }

    (values, errors)
}

/// The error the loader would report for `field` if config.toml held
/// `content` (env ignored).
pub(crate) fn file_field_error(content: &str, field: &str) -> Option<String> {
    let (values, mut errors) = toml_values_from_str(content);
    let (_, field_errors) = HcomConfig::load_from_sources(&values, Some(&HashMap::new()));
    errors.extend(field_errors);
    errors.remove(field)
}

/// Write every field of `config` to config.toml. An existing file keeps its
/// comments and other keys (e.g. terminal presets).
#[cfg(test)]
pub fn save_toml_config(config: &HcomConfig) -> std::io::Result<()> {
    save_toml_fields(config, &[])
}

/// [`save_toml_config`] without the fields named in `skip`.
fn save_toml_fields(config: &HcomConfig, skip: &[&str]) -> std::io::Result<()> {
    let toml_path = paths::config_toml_path();
    if let Some(parent) = toml_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing = std::fs::read_to_string(&toml_path)
        .ok()
        .and_then(|c| c.parse::<toml_edit::DocumentMut>().ok());
    let is_new = existing.is_none();
    let mut doc = existing.unwrap_or_default();

    for field in FIELDS.iter().filter(|f| !skip.contains(&f.name)) {
        if let Some(val) = config.get_field(field.name) {
            let value = field_toml_value(field, &val).map_err(std::io::Error::other)?;
            set_doc_path(&mut doc, field.toml, Some(value));
        }
    }

    let content = if is_new {
        format!("{TOML_HEADER}\n{doc}")
    } else {
        doc.to_string()
    };
    write_config_toml_path(&toml_path, &content)
}

/// Set (or with `None`, remove) the value at a dotted path in a config.toml
/// document, creating parent tables as needed.
pub(crate) fn set_doc_path(
    doc: &mut toml_edit::DocumentMut,
    dotted_path: &str,
    value: Option<toml_edit::Value>,
) {
    let (parents, key) = dotted_path.rsplit_once('.').unwrap_or(("", dotted_path));
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for part in parents.split('.').filter(|p| !p.is_empty()) {
        if !table.get(part).is_some_and(|item| item.is_table_like()) {
            if value.is_none() {
                return;
            }
            table.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let Some(next) = table
            .get_mut(part)
            .and_then(|item| item.as_table_like_mut())
        else {
            return;
        };
        table = next;
    }
    match value {
        Some(v) => {
            table.insert(key, toml_edit::Item::Value(v));
        }
        None => {
            table.remove(key);
        }
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

/// True if `name` is a built-in preset supported on `platform`
/// ("Darwin"/"Linux"/"Windows", see `crate::shared::platform::platform_name`).
pub fn terminal_preset_supported_on(name: &str, platform: &str) -> bool {
    TERMINAL_PRESETS
        .iter()
        .any(|(p, preset)| p.eq_ignore_ascii_case(name) && preset.platforms.contains(&platform))
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

fn user_defined_preset_error(name: &str) -> Option<String> {
    let presets = load_toml_presets(&paths::config_toml_path())?;
    let (_, value) = presets
        .as_table()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))?;
    let preset = match value.as_table() {
        Some(preset) => preset,
        None => return Some("expected a table".to_string()),
    };
    for field in ["open", "close"] {
        if let Some(value) = preset.get(field)
            && let Err(error) = toml_val_to_argv(value)
        {
            return Some(format!("{field}: {error}"));
        }
    }
    None
}

/// Check if a terminal name matches a user-defined preset in config.toml.
pub fn is_user_defined_preset(name: &str) -> bool {
    let toml_path = paths::config_toml_path();
    if let Some(presets_val) = load_toml_presets(&toml_path)
        && let Some(table) = presets_val.as_table()
    {
        return table.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case(name)
                && value.as_table().is_some_and(|preset| {
                    preset.get("open").map(toml_val_to_argv).transpose().is_ok()
                        && preset
                            .get("close")
                            .map(toml_val_to_argv)
                            .transpose()
                            .is_ok()
                })
        });
    }
    false
}

/// Get the pane_id_env for a preset, checking TOML overrides then built-in defaults.
pub fn get_merged_preset_pane_id_env(name: &str) -> Option<String> {
    get_merged_preset(name).and_then(|p| p.pane_id_env)
}

/// Environment variables that identify a terminal-local pane/window/workspace.
///
/// New-window runner sidecars must not replay these values from the parent:
/// the terminal backend supplies fresh identity to the child. Include both
/// built-in detection variables and user-configured `pane_id_env` values.
pub fn pane_identity_env_vars() -> std::collections::HashSet<String> {
    pane_identity_env_vars_from_path(&paths::config_toml_path())
}

fn pane_identity_env_vars_from_path(
    toml_path: &std::path::Path,
) -> std::collections::HashSet<String> {
    let mut vars = crate::shared::terminal_presets::TERMINAL_ENV_MAP
        .iter()
        .map(|(env_var, _)| (*env_var).to_string())
        .collect::<std::collections::HashSet<_>>();
    // wezterm sets it in every pane; the launcher's would name another GUI
    // process when the new window starts its own.
    vars.insert("WEZTERM_UNIX_SOCKET".to_string());

    if let Some(presets) = load_toml_presets(toml_path).and_then(|value| value.as_table().cloned())
    {
        vars.extend(presets.values().filter_map(|value| {
            value
                .as_table()
                .and_then(|preset| preset.get("pane_id_env"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        }));
    }

    vars
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
        let open_result = val.get("open").map(toml_val_to_argv).transpose();
        let close_result = val.get("close").map(toml_val_to_argv).transpose();
        match (open_result, close_result) {
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("Warning: skipping custom terminal preset {name:?}: {e}");
                None
            }
            (Ok(open), Ok(close)) => Some(TomlPresetFields {
                binary: val
                    .get("binary")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                app_name: val
                    .get("app_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                open: open.flatten(),
                close: close.flatten(),
                pane_id_env: val
                    .get("pane_id_env")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            }),
        }
    });

    let builtin = crate::shared::get_terminal_preset(name);

    match (&toml_preset, &builtin) {
        (None, None) => None,
        _ => {
            // Built-in argv templates, lowered to owned Vec<String> per platform.
            let argv_vec = |sel: Option<crate::shared::terminal_presets::ArgvTemplate>| {
                sel.map(|t| t.iter().map(|s| s.to_string()).collect::<Vec<String>>())
            };
            let b_open = builtin.map(|b| b.open);
            let b_close = builtin.map(|b| b.close);
            let b_binary = builtin.and_then(|b| b.binary);
            let b_app = builtin.and_then(|b| b.app_name);
            let b_pane_env = builtin.and_then(|b| b.pane_id_env);

            let t = toml_preset.as_ref();

            // A TOML `open`/`close` override (string or array) replaces the
            // built-in on BOTH platforms — TOML custom presets have no separate
            // Windows slot, so the array form is the Windows escape hatch (it
            // can carry literal Windows paths without shell mangling).
            let toml_open = t.and_then(|t| t.open.clone());
            let toml_close = t.and_then(|t| t.close.clone());

            let (open, open_windows) = match toml_open {
                Some(o) => (o, None),
                None => (
                    argv_vec(b_open.and_then(|o| o.default)).unwrap_or_default(),
                    argv_vec(b_open.and_then(|o| o.windows)),
                ),
            };
            let (close, close_windows) = match toml_close {
                Some(c) => (Some(c), None),
                None => (
                    argv_vec(b_close.and_then(|c| c.default)),
                    argv_vec(b_close.and_then(|c| c.windows)),
                ),
            };

            Some(MergedPreset {
                open,
                open_windows,
                close,
                close_windows,
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

/// Convert a TOML preset `open`/`close` value into an argv vector.
///
/// Accepts BOTH forms:
/// - Array: each element must be a string; non-string elements are an error.
///   Literal Windows paths like `C:\Users\x\s.ps1` survive intact — this is
///   the recommended escape hatch for custom presets.
/// - String (legacy): tokenized once via the double-quote-aware
///   `args_common::shell_split`. Backslashes are consumed by that tokenizer, so
///   the array form is preferred for Windows paths. A `\` in the string triggers
///   a warning so users can migrate to the array form.
///
/// Returns `Ok(None)` for an empty string/array (treated as "unset").
/// Returns `Err` on non-string array elements or invalid shell quoting — the
/// caller should treat this as a configuration error rather than falling back
/// to a built-in preset.
fn toml_val_to_argv(v: &toml::Value) -> Result<Option<Vec<String>>, String> {
    match v {
        toml::Value::Array(items) => {
            if items.is_empty() {
                return Ok(None);
            }
            let mut argv = Vec::with_capacity(items.len());
            for (i, e) in items.iter().enumerate() {
                match e.as_str() {
                    Some(s) => argv.push(s.to_string()),
                    None => {
                        return Err(format!(
                            "element [{}] is not a string (got {}); use quoted strings",
                            i,
                            e.type_str()
                        ));
                    }
                }
            }
            Ok(Some(argv))
        }
        toml::Value::String(s) => {
            if s.is_empty() {
                return Ok(None);
            }
            if s.contains('\\') {
                eprintln!(
                    "Warning: custom terminal preset command contains backslashes; \
                     use the array form to avoid shell tokenization issues on Windows: {s:?}"
                );
            }
            match crate::tools::args_common::shell_split(s, cfg!(windows)) {
                Ok(argv) if !argv.is_empty() => Ok(Some(argv)),
                Ok(_) => Ok(None),
                Err(e) => Err(format!("invalid quoting in preset command: {e}")),
            }
        }
        _ => Err(format!(
            "expected a string or array of strings, got {}",
            v.type_str()
        )),
    }
}

/// Parsed TOML preset fields (all optional — overlay on built-in).
struct TomlPresetFields {
    binary: Option<String>,
    app_name: Option<String>,
    open: Option<Vec<String>>,
    close: Option<Vec<String>>,
    pane_id_env: Option<String>,
}

/// Fully merged terminal preset (TOML + built-in), as argument vectors.
#[derive(Debug, Clone)]
pub struct MergedPreset {
    /// Default (Unix / fallback) open argv.
    pub open: Vec<String>,
    /// Windows-specific open argv override (None ⇒ use `open`).
    pub open_windows: Option<Vec<String>>,
    /// Default (Unix / fallback) close argv (None ⇒ no close API).
    pub close: Option<Vec<String>>,
    /// Windows-specific close argv override (None ⇒ use `close`).
    pub close_windows: Option<Vec<String>>,
    pub binary: Option<String>,
    pub app_name: Option<String>,
    pub pane_id_env: Option<String>,
}

impl MergedPreset {
    /// Open argv for the given platform (Windows falls back to the default).
    pub fn open_argv(&self, is_windows: bool) -> Vec<String> {
        if is_windows {
            self.open_windows
                .clone()
                .unwrap_or_else(|| self.open.clone())
        } else {
            self.open.clone()
        }
    }

    /// Close argv for the given platform (Windows falls back to the default).
    pub fn close_argv(&self, is_windows: bool) -> Option<Vec<String>> {
        if is_windows {
            self.close_windows.clone().or_else(|| self.close.clone())
        } else {
            self.close.clone()
        }
    }

    /// Whether a close API exists for the given platform (no clone).
    pub fn has_close(&self, is_windows: bool) -> bool {
        if is_windows {
            self.close_windows.is_some() || self.close.is_some()
        } else {
            self.close.is_some()
        }
    }
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
    ConfigSnapshot {
        core: HcomConfig::load(Some(&HashMap::new())),
    }
}

/// Fields left out of a new config.toml so it follows their default if that
/// changes again, instead of pinning today's value.
///
/// 0.7.29-0.7.30 stored delivery as `launch.<tool>.delivery` and wrote the
/// then-default `"inject"` into every new config.toml. The key moved to
/// `idle_delivery` when native became the default, so those stale lines are
/// ignored (unknown keys are) and an `inject` set from now on sticks.
const UNPINNED_FIELDS: &[&str] = &["claude_delivery", "copilot_delivery"];

/// Write default config.toml + env file.
pub fn write_default_config() -> std::io::Result<()> {
    save_toml_fields(&HcomConfig::default(), UNPINNED_FIELDS)?;
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
pub(crate) fn lock_down_config_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
pub(crate) fn lock_down_config_permissions(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::{EnvGuard, isolated_test_env};
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
    fn claude_delivery_defaults_to_native_and_rejects_unknown_modes() {
        let mut config = HcomConfig::default();
        assert_eq!(config.to_env_dict()["HCOM_CLAUDE_DELIVERY"], "native");
        config.set_field("claude_delivery", "inject").unwrap();
        let (loaded, errors) =
            HcomConfig::load_from_sources(&HashMap::new(), Some(&config.to_env_dict()));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(loaded.claude_delivery, "inject");
        config.set_field("claude_delivery", "pty").unwrap();
        assert!(config.collect_errors().contains_key("claude_delivery"));
    }

    // Unix-only: asserts against $HOME and POSIX absolute paths; Windows
    // resolves the base dir from USERPROFILE and treats "/x" as drive-relative.
    #[test]
    #[serial]
    fn test_guard_redirects_non_temp_hcom_dir() {
        let _guard = EnvGuard::new();
        let unsafe_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".hcom-unsafe-test");
        unsafe {
            env::set_var("HCOM_DIR", &unsafe_dir);
        }
        Config::reset();
        Config::init();

        let actual = Config::get().hcom_dir;
        assert_ne!(actual, unsafe_dir);
        assert!(actual.starts_with(env::temp_dir()), "actual={actual:?}");
    }

    #[test]
    #[serial]
    fn test_guard_allows_registered_hcom_dir() {
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let expected = temp.path().join(".hcom");
        // A fixture must claim the root before Config will keep it.
        paths::test_roots::register(temp.path());
        unsafe {
            env::set_var("HCOM_DIR", &expected);
        }
        Config::reset();
        Config::init();

        assert_eq!(Config::get().hcom_dir, expected);
    }

    #[test]
    #[serial]
    fn test_guard_redirects_unregistered_temp_hcom_dir() {
        // Geography is not ownership: a temp path no fixture registered is not
        // trusted, even though it sits under $TMPDIR. This is the finding-3
        // guarantee that a real hcom DB happening to live under /tmp is not
        // waved through.
        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let unregistered = temp.path().join(".hcom");
        unsafe {
            env::set_var("HCOM_DIR", &unregistered);
        }
        Config::reset();
        Config::init();

        assert_ne!(Config::get().hcom_dir, unregistered);
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn test_guard_rejects_temp_symlink_to_non_temp_hcom_dir() {
        use std::os::unix::fs::symlink;

        let _guard = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("outside");
        symlink(env!("CARGO_MANIFEST_DIR"), &link).unwrap();
        let unsafe_dir = link.join(".hcom");
        unsafe {
            env::set_var("HCOM_DIR", &unsafe_dir);
        }
        Config::reset();
        Config::init();

        let actual = Config::get().hcom_dir;
        assert_ne!(actual, unsafe_dir);
        assert!(actual.starts_with(env::temp_dir()), "actual={actual:?}");
    }

    #[test]
    #[serial]
    fn test_raw_resolution_and_db_open_do_not_share_mutable_escape_state() {
        use std::sync::{Arc, Barrier};

        let (_dir, hcom_dir, _home, _guard) = isolated_test_env();
        let barrier = Arc::new(Barrier::new(2));
        let raw_barrier = Arc::clone(&barrier);
        let db_barrier = Arc::clone(&barrier);
        let raw_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".hcom-resolution-test");
        let expected_db = hcom_dir.join("hcom.db");

        let resolver = std::thread::spawn(move || {
            let env = HashMap::from([(
                "HCOM_DIR".to_string(),
                raw_path.to_string_lossy().into_owned(),
            )]);
            raw_barrier.wait();
            let (resolved, explicit) =
                paths::resolve_hcom_dir_from_env(&env, std::path::Path::new("/worktree"));
            assert_eq!(resolved, raw_path);
            assert!(explicit);
        });
        let db_open = std::thread::spawn(move || {
            db_barrier.wait();
            let db = crate::db::HcomDb::open().unwrap();
            assert_eq!(db.path(), expected_db);
        });

        resolver.join().unwrap();
        db_open.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_default_config_uses_home_hcom() {
        let env = HashMap::from([("HOME".to_string(), "/home/test".to_string())]);
        let (actual, explicit) =
            paths::resolve_hcom_dir_from_env(&env, std::path::Path::new("/worktree"));

        assert_eq!(actual, PathBuf::from("/home/test/.hcom"));
        assert!(!explicit);
    }

    #[cfg(unix)]
    #[test]
    fn test_hcom_dir_overrides_home() {
        let env = HashMap::from([("HCOM_DIR".to_string(), "/custom/hcom".to_string())]);
        let (actual, explicit) =
            paths::resolve_hcom_dir_from_env(&env, std::path::Path::new("/worktree"));

        assert_eq!(actual, PathBuf::from("/custom/hcom"));
        assert!(explicit);
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
    fn test_hcom_dir_tilde_expansion() {
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let env = HashMap::from([
            ("HOME".to_string(), home.to_string_lossy().into_owned()),
            ("HCOM_DIR".to_string(), "~/.hcom".to_string()),
        ]);
        let (actual, explicit) =
            paths::resolve_hcom_dir_from_env(&env, std::path::Path::new("/worktree"));

        assert_eq!(actual, home.join(".hcom"));
        assert!(explicit);
    }

    #[test]
    fn test_hcom_dir_relative_resolved_to_absolute() {
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let env = HashMap::from([("HCOM_DIR".to_string(), "relative/path".to_string())]);
        let (actual, explicit) = paths::resolve_hcom_dir_from_env(&env, &cwd);

        assert_eq!(actual, cwd.join("relative/path"));
        assert!(explicit);
    }

    #[cfg(unix)]
    #[test]
    fn test_hcom_dir_absolute_stays_absolute() {
        let env = HashMap::from([("HCOM_DIR".to_string(), "/absolute/hcom".to_string())]);
        let (actual, explicit) =
            paths::resolve_hcom_dir_from_env(&env, std::path::Path::new("/worktree"));

        assert_eq!(actual, PathBuf::from("/absolute/hcom"));
        assert!(explicit);
    }

    #[test]
    fn test_hcom_config_defaults() {
        let mut config = HcomConfig::default();
        assert_eq!(config.timeout, 86400);
        assert_eq!(config.subagent_timeout, 30);
        assert_eq!(config.terminal, "default");
        assert_eq!(config.tag, "");
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
    fn test_hcom_config_validation_shell_args() {
        let mut config = HcomConfig::default();
        config.set_field("claude_args", "--model opus").unwrap();
        assert!(!config.collect_errors().contains_key("claude_args"));

        config.set_field("claude_args", "unclosed 'quote").unwrap();
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
        assert_eq!(config.terminal, "kitty"); // Normalized regardless of platform
        // kitty is Darwin/Linux-only (DL); on Windows it's correctly rejected
        // by the platform-availability check added for finding #17.
        if crate::shared::platform::platform_name() == "Windows" {
            assert!(errors.contains_key("terminal"));
        } else {
            assert!(!errors.contains_key("terminal"));
        }
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
        // Finding 17: presets are now validated against the host platform, so
        // only assert presets that are actually supported here.
        let platform = crate::shared::platform::platform_name();
        let mut config = HcomConfig::default();
        for preset in &[
            "kitty",
            "wezterm",
            "tmux",
            "alacritty",
            "ptyxis",
            "terminal.app",
            "iterm",
        ] {
            if !terminal_preset_supported_on(preset, platform) {
                continue;
            }
            config.terminal = preset.to_string();
            assert!(
                !config.collect_errors().contains_key("terminal"),
                "preset '{preset}' should be valid on {platform}"
            );
        }
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn wrong_platform_builtin_preset_is_rejected() {
        // Finding 17: a built-in preset not available on the host platform
        // (here, "wttab" is Windows-only) must be rejected at validation time,
        // not just silently accepted and left to fail at launch.
        let mut config = HcomConfig {
            terminal: "wttab".to_string(),
            ..HcomConfig::default()
        };
        assert!(config.collect_errors().contains_key("terminal"));
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
        // Relay fields are file-only; exporting them would only leak them.
        assert!(dict.keys().all(|k| !k.starts_with("HCOM_RELAY")));

        let (roundtrip, errors) = HcomConfig::load_from_sources(&HashMap::new(), Some(&dict));
        assert!(errors.is_empty(), "{errors:?}");
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
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;
        assert_eq!(config, HcomConfig::default());
    }

    #[test]
    fn test_load_from_sources_toml_values() {
        let mut file_config = HashMap::new();
        file_config.insert("timeout".to_string(), TomlFieldValue::Int(3600));
        file_config.insert("tag".to_string(), TomlFieldValue::Str("test".to_string()));
        file_config.insert("relay_enabled".to_string(), TomlFieldValue::Bool(false));

        let env = HashMap::new();
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;

        assert_eq!(config.timeout, 3600);
        assert_eq!(config.tag, "test");
        assert!(!config.relay_enabled);
    }

    #[test]
    fn test_load_from_sources_title_mode_and_env_override() {
        let mut file_config = HashMap::new();
        file_config.insert(
            "title_mode".to_string(),
            TomlFieldValue::Str("label".to_string()),
        );
        let mut env = HashMap::new();
        env.insert("HCOM_TITLE_MODE".to_string(), "off".to_string());

        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;
        assert_eq!(config.title_mode, "off");
    }

    #[test]
    fn test_load_from_sources_copilot_delivery() {
        let none = HashMap::new();
        let config = HcomConfig::load_from_sources(&none, Some(&HashMap::new())).0;
        assert_eq!(config.copilot_delivery, "native");

        let mut file_config = HashMap::new();
        file_config.insert(
            "copilot_delivery".to_string(),
            TomlFieldValue::Str("inject".to_string()),
        );
        let config = HcomConfig::load_from_sources(&file_config, Some(&HashMap::new())).0;
        assert_eq!(config.copilot_delivery, "inject");

        let mut env = HashMap::new();
        env.insert("HCOM_COPILOT_DELIVERY".to_string(), "native".to_string());
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;
        assert_eq!(config.copilot_delivery, "native");

        env.insert("HCOM_COPILOT_DELIVERY".to_string(), "typo".to_string());
        let (config, errors) = HcomConfig::load_from_sources(&none, Some(&env));
        assert!(errors["copilot_delivery"].contains("HCOM_COPILOT_DELIVERY"));
        assert_eq!(config.copilot_delivery, DEFAULT_DELIVERY);
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

        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;

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

        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;

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
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;
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
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;
        assert!(!config.auto_approve);
    }

    #[test]
    fn test_load_from_sources_terminal_empty_uses_default() {
        let mut file_config = HashMap::new();
        file_config.insert("terminal".to_string(), TomlFieldValue::Str("".to_string()));

        let env = HashMap::new();
        let config = HcomConfig::load_from_sources(&file_config, Some(&env)).0;
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

        let mut doc = toml_edit::DocumentMut::new();
        for field in FIELDS.iter() {
            let val = config.get_field(field.name).unwrap();
            set_doc_path(
                &mut doc,
                field.toml,
                Some(field_toml_value(field, &val).unwrap()),
            );
        }
        let (file_config, errors) = toml_values_from_str(&doc.to_string());
        assert!(errors.is_empty(), "{errors:?}");

        let roundtrip = HcomConfig::load_from_sources(&file_config, Some(&HashMap::new())).0;
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

        let (result, errors) = read_toml_values(&path);
        // Terminal with dangerous chars should be removed and reported
        assert!(!result.contains_key("terminal"));
        assert!(errors["terminal"].contains("unsafe characters"));
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

        let (result, errors) = read_toml_values(&path);
        assert!(errors.is_empty(), "{errors:?}");
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
        let (result, errors) = read_toml_values(std::path::Path::new("/nonexistent/config.toml"));
        assert!(result.is_empty() && errors.is_empty());
    }

    #[test]
    fn test_load_toml_config_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not valid toml [[[[").unwrap();

        let (result, errors) = read_toml_values(&path);
        assert!(result.is_empty());
        assert!(errors.contains_key("config.toml"));
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
        let mut config = HcomConfig::default();
        for field in FIELDS.iter() {
            let val = config
                .get_field(field.name)
                .unwrap_or_else(|| panic!("get_field('{}') should return Some", field.name));
            config
                .set_field(field.name, &val)
                .unwrap_or_else(|e| panic!("set_field('{}'): {e}", field.name));
        }
        assert_eq!(config, HcomConfig::default());
        assert!(config.get_field("nonexistent").is_none());

        let unique = |key: fn(&ConfigField) -> &str| {
            FIELDS
                .iter()
                .map(key)
                .collect::<std::collections::HashSet<_>>()
                .len()
        };
        assert_eq!(unique(|f| f.name), FIELDS.len());
        assert_eq!(unique(|f| f.env), FIELDS.len());
        assert_eq!(unique(|f| f.toml), FIELDS.len());
    }

    #[test]
    fn args_env_keys_match_integration_specs() {
        let expected: std::collections::HashSet<&str> = crate::integration_spec::ALL
            .iter()
            .filter_map(|spec| spec.launch.args_env)
            .collect();
        let actual: std::collections::HashSet<&str> = FIELDS
            .iter()
            .filter_map(|f| f.name.ends_with("_args").then_some(f.env))
            .collect();

        assert_eq!(
            actual, expected,
            "HcomConfig *_args env vars must match IntegrationSpec.launch.args_env"
        );
    }

    /// A valid non-default value for each field, as `get_field` returns it.
    fn sample_value(field: &ConfigField) -> &'static str {
        match (field.kind, field.name) {
            (FieldKind::Int, _) => "123",
            (FieldKind::Bool, "relay_share_account") => "1",
            (FieldKind::Bool, _) => "0",
            (_, "terminal") => "print",
            (_, "title_mode") => "label",
            (_, name) if name.ends_with("_delivery") => "inject",
            (_, "auto_subscribe") => "sample",
            (_, "bootstrap_exclude") => "rules",
            _ => "sample-value",
        }
    }

    #[test]
    fn every_field_loads_from_config_toml_and_env() {
        for field in FIELDS.iter() {
            let expected = sample_value(field);
            assert_ne!(
                HcomConfig::default().get_field(field.name).unwrap(),
                expected
            );

            let mut doc = toml_edit::DocumentMut::new();
            let value = field_toml_value(field, expected).unwrap();
            set_doc_path(&mut doc, field.toml, Some(value));
            let (file_values, errors) = toml_values_from_str(&doc.to_string());
            assert!(errors.is_empty(), "{}: {errors:?}", field.name);
            let (config, errors) =
                HcomConfig::load_from_sources(&file_values, Some(&HashMap::new()));
            assert!(errors.is_empty(), "{}: {errors:?}", field.name);
            assert_eq!(
                config.get_field(field.name).unwrap(),
                expected,
                "{} must load from config.toml {}",
                field.name,
                field.toml
            );

            let env = HashMap::from([(field.env.to_string(), expected.to_string())]);
            let (config, _) = HcomConfig::load_from_sources(&HashMap::new(), Some(&env));
            let loaded = config.get_field(field.name).unwrap();
            if field.is_relay() {
                assert_ne!(loaded, expected, "{} must not load from env", field.name);
            } else {
                assert_eq!(
                    loaded, expected,
                    "{} must load from {}",
                    field.name, field.env
                );
            }
        }
    }

    #[test]
    fn invalid_value_falls_back_alone() {
        let file_config = HashMap::from([
            ("tag".to_string(), TomlFieldValue::Str("a b".to_string())),
            ("timeout".to_string(), TomlFieldValue::Int(999_999)),
            (
                "claude_args".to_string(),
                TomlFieldValue::Str("--model haiku".to_string()),
            ),
            (
                "relay_id".to_string(),
                TomlFieldValue::Str("group".to_string()),
            ),
        ]);
        let env = HashMap::from([
            ("HCOM_SUBAGENT_TIMEOUT".to_string(), "soon".to_string()),
            ("HCOM_AUTO_APPROVE".to_string(), "maybe".to_string()),
        ]);
        let (config, errors) = HcomConfig::load_from_sources(&file_config, Some(&env));

        let defaults = HcomConfig::default();
        assert_eq!(config.tag, defaults.tag);
        assert_eq!(config.timeout, defaults.timeout);
        assert_eq!(config.subagent_timeout, defaults.subagent_timeout);
        assert_eq!(config.auto_approve, defaults.auto_approve);
        assert_eq!(config.tool_args(Tool::Claude), "--model haiku");
        assert_eq!(config.relay_id, "group");
        assert_eq!(
            errors.keys().map(String::as_str).collect::<Vec<_>>(),
            ["auto_approve", "subagent_timeout", "tag", "timeout"]
        );
        assert!(errors["tag"].contains("[config.toml launch.tag]"));
        assert!(errors["subagent_timeout"].contains("[env HCOM_SUBAGENT_TIMEOUT]"));
    }

    #[test]
    fn empty_env_switch_means_off() {
        let env = HashMap::from([
            ("HCOM_AUTO_APPROVE".to_string(), String::new()),
            ("HCOM_AUTO_TRUST_WORKSPACE".to_string(), String::new()),
            ("HCOM_TIMEOUT".to_string(), String::new()),
        ]);
        let (config, errors) = HcomConfig::load_from_sources(&HashMap::new(), Some(&env));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!config.auto_approve);
        assert!(!config.auto_trust_workspace);
        assert_eq!(config.timeout, HcomConfig::default().timeout);
    }

    #[test]
    fn toml_bool_in_string_field_reads_as_text() {
        let file_config = HashMap::from([("hints".to_string(), TomlFieldValue::Bool(true))]);
        let (config, errors) = HcomConfig::load_from_sources(&file_config, Some(&HashMap::new()));
        assert!(errors.is_empty());
        assert_eq!(config.hints, "true");
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
    fn test_pane_identity_env_vars_include_builtin_and_custom_vars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[terminal.presets.myterm]
open = "myterm spawn -- bash {script}"
close = "myterm close {pane_id}"
pane_id_env = "MYTERM_PANE_ID"
"#,
        )
        .unwrap();

        let vars = pane_identity_env_vars_from_path(&path);
        assert!(vars.contains("HERDR_PANE_ID"));
        assert!(vars.contains("KITTY_WINDOW_ID"));
        assert!(vars.contains("MYTERM_PANE_ID"));
    }

    #[test]
    fn test_toml_val_to_argv_array_preserves_windows_path() {
        // Array form: elements collected verbatim, so a literal Windows path
        // (backslashes, drive letter) survives without tokenization.
        let v = toml::Value::Array(vec![
            toml::Value::String("myterm".into()),
            toml::Value::String("-e".into()),
            toml::Value::String(r"C:\Users\x\s.ps1".into()),
        ]);
        assert_eq!(
            toml_val_to_argv(&v),
            Ok(Some(vec![
                "myterm".to_string(),
                "-e".to_string(),
                r"C:\Users\x\s.ps1".to_string(),
            ]))
        );
    }

    #[test]
    fn test_toml_val_to_argv_array_rejects_non_string_element() {
        let v = toml::Value::Array(vec![
            toml::Value::String("myterm".into()),
            toml::Value::Integer(42),
            toml::Value::String("{script}".into()),
        ]);
        assert!(toml_val_to_argv(&v).is_err());
    }

    #[test]
    fn test_toml_val_to_argv_string_tokenizes_legacy() {
        let v = toml::Value::String("myterm -e bash {script}".into());
        assert_eq!(
            toml_val_to_argv(&v),
            Ok(Some(vec![
                "myterm".to_string(),
                "-e".to_string(),
                "bash".to_string(),
                "{script}".to_string(),
            ]))
        );
    }

    #[test]
    fn test_toml_val_to_argv_string_invalid_quoting_returns_err() {
        let v = toml::Value::String(r#"kitty -- bash "unterminated"#.into());
        assert!(toml_val_to_argv(&v).is_err());
    }

    #[test]
    fn test_toml_val_to_argv_empty_and_wrong_type() {
        assert!(toml_val_to_argv(&toml::Value::Integer(3)).is_err());
        assert_eq!(toml_val_to_argv(&toml::Value::Array(vec![])), Ok(None));
        assert_eq!(
            toml_val_to_argv(&toml::Value::String(String::new())),
            Ok(None)
        );
    }

    // B-1: a user-defined `[terminal.presets.<builtin>]` override declares no
    // platform and takes precedence over the built-in, so it must be accepted
    // even on a platform where the built-in itself is unavailable.
    #[test]
    #[serial]
    fn user_defined_override_exempt_from_builtin_platform_gate() {
        let (_dir, hcom_dir, _home, _guard) = isolated_test_env();
        let platform = crate::shared::platform::platform_name();
        // A built-in preset NOT available on the current host platform.
        let builtin = match platform {
            // windows-terminal is Windows-only.
            "Darwin" | "Linux" => "windows-terminal",
            // iterm is Darwin-only.
            _ => "iterm",
        };

        // Control: without any user override the wrong-platform built-in is
        // rejected at validate time.
        let mut cfg = HcomConfig {
            terminal: builtin.to_string(),
            ..Default::default()
        };
        assert!(
            cfg.collect_errors().contains_key("terminal"),
            "built-in {builtin} should be rejected on {platform} without a user override"
        );

        // Define a user preset with the SAME name — it must now be accepted.
        std::fs::write(
            hcom_dir.join("config.toml"),
            format!(
                "[terminal.presets.{builtin}]\nopen = \"{builtin} -- powershell -File {{script}}\"\n"
            ),
        )
        .unwrap();
        let mut cfg = HcomConfig {
            terminal: builtin.to_string(),
            ..Default::default()
        };
        assert!(
            !cfg.collect_errors().contains_key("terminal"),
            "user-defined override of {builtin} must be accepted on {platform}"
        );
    }

    #[test]
    #[serial]
    fn malformed_user_override_does_not_bypass_builtin_platform_gate() {
        let (_dir, hcom_dir, _home, _guard) = isolated_test_env();
        let builtin = match crate::shared::platform::platform_name() {
            "Darwin" | "Linux" => "windows-terminal",
            _ => "iterm",
        };
        std::fs::write(
            hcom_dir.join("config.toml"),
            format!("[terminal.presets.{builtin}]\nopen = \"powershell \\\"unterminated\"\n"),
        )
        .unwrap();

        assert!(!is_user_defined_preset(builtin));
        let mut cfg = HcomConfig {
            terminal: builtin.to_string(),
            ..Default::default()
        };
        assert!(
            cfg.collect_errors().contains_key("terminal"),
            "malformed override must not exempt {builtin} from the platform gate"
        );
    }

    #[test]
    #[serial]
    fn malformed_user_override_rejected_for_supported_builtin() {
        let (_dir, hcom_dir, _home, _guard) = isolated_test_env();
        let builtin = if cfg!(windows) { "cmd" } else { "tmux" };
        std::fs::write(
            hcom_dir.join("config.toml"),
            format!("[terminal.presets.{builtin}]\nclose = 42\n"),
        )
        .unwrap();
        let mut cfg = HcomConfig {
            terminal: builtin.to_string(),
            ..Default::default()
        };

        let error = cfg.collect_errors().remove("terminal").unwrap();
        assert!(error.contains("invalid terminal preset"));
        assert!(error.contains("close:"));
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

    /// 0.7.29-0.7.30 wrote `delivery = "inject"` into every new config.toml.
    /// Those lines are ignored without a warning, and `inject` set after the
    /// upgrade sticks.
    #[test]
    fn old_delivery_keys_are_ignored_for_the_native_default() {
        let old = "[launch.claude]\nargs = \"--model keep\"\ndelivery = \"inject\"\n\n\
                   [launch.copilot]\ndelivery = \"inject\"\n";
        let (values, errors) = toml_values_from_str(old);
        assert!(errors.is_empty(), "{errors:?}");
        let config = HcomConfig::load_from_sources(&values, Some(&HashMap::new())).0;
        assert_eq!(config.claude_delivery, "native");
        assert_eq!(config.copilot_delivery, "native");
        assert_eq!(config.tool_args(Tool::Claude), "--model keep");

        let mut doc: toml_edit::DocumentMut = old.parse().unwrap();
        let field = user_field("claude_delivery").unwrap();
        set_doc_path(
            &mut doc,
            field.toml,
            Some(field_toml_value(field, "inject").unwrap()),
        );
        let (values, _) = toml_values_from_str(&doc.to_string());
        let config = HcomConfig::load_from_sources(&values, Some(&HashMap::new())).0;
        assert_eq!(config.claude_delivery, "inject");
        assert_eq!(config.copilot_delivery, "native");
    }

    #[test]
    #[serial]
    fn default_config_file_does_not_pin_delivery() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        write_default_config().unwrap();
        let content = std::fs::read_to_string(paths::config_toml_path()).unwrap();
        assert!(!content.contains("delivery"), "{content}");
        assert!(content.contains("[launch.claude]"), "{content}");
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

        save_toml_config(&config).unwrap();

        let mode = std::fs::metadata(paths::config_toml_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        // Edit mode also tightens permissions on an existing config.
        let path = paths::config_toml_path();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        lock_down_config_permissions(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
