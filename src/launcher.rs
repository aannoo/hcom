//! Unified launcher pipeline for released AI-tool integrations.
//!
//!
//! Provides a single entry point for launching all supported AI tools
//! with consistent batch tracking, environment setup, and error handling.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Result, bail};
use rand::RngExt;
use serde_json::json;

use crate::config::{self, HcomConfig};
use crate::db::HcomDb;
use crate::instance_binding;
use crate::instance_names;
use crate::instances;
use crate::paths;
use crate::shared::constants::HCOM_IDENTITY_VARS;
use crate::shared::tool_detection::tool_marker_vars;
use crate::terminal;
use crate::tools::launch_arg_validation::{
    ANTIGRAVITY_REJECTED_ARGS, GEMINI_REJECTED_ARGS, KILO_REJECTED_ARGS, KIMI_REJECTED_ARGS,
    OPENCODE_REJECTED_ARGS, PI_REJECTED_ARGS, validate_rejected_args,
};
use crate::tools::{
    codex_preprocessing, copilot_preprocessing, cursor_preprocessing, opencode_preprocessing,
};

/// Canonical tool types for launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchTool {
    Claude,
    ClaudePty,
    Gemini,
    Codex,
    OpenCode,
    Kilo,
    Pi,
    Antigravity,
    Cursor,
    Kimi,
    Copilot,
}

impl LaunchTool {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(LaunchTool::Claude),
            "claude-pty" => Ok(LaunchTool::ClaudePty),
            "gemini" => Ok(LaunchTool::Gemini),
            "codex" => Ok(LaunchTool::Codex),
            "opencode" => Ok(LaunchTool::OpenCode),
            "kilo" | "kilocode" => Ok(LaunchTool::Kilo),
            "pi" | "pi-agent" => Ok(LaunchTool::Pi),
            "antigravity" | "agy" => Ok(LaunchTool::Antigravity),
            "cursor" | "cursor-agent" => Ok(LaunchTool::Cursor),
            "kimi" => Ok(LaunchTool::Kimi),
            "copilot" => Ok(LaunchTool::Copilot),
            _ => bail!("Unknown tool: {}", s),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            LaunchTool::Claude => "claude",
            LaunchTool::ClaudePty => "claude-pty",
            LaunchTool::Gemini => "gemini",
            LaunchTool::Codex => "codex",
            LaunchTool::OpenCode => "opencode",
            LaunchTool::Kilo => "kilo",
            LaunchTool::Pi => "pi",
            LaunchTool::Antigravity => "antigravity",
            LaunchTool::Cursor => "cursor",
            LaunchTool::Kimi => "kimi",
            LaunchTool::Copilot => "copilot",
        }
    }

    /// Canonical [`Tool`] for this launch surface.
    ///
    /// `ClaudePty` is a launch-surface alias (PTY-wrapped Claude); it resolves
    /// to `Tool::Claude` so all per-tool data flows through one spec.
    pub fn tool(&self) -> crate::tool::Tool {
        match self {
            LaunchTool::Claude | LaunchTool::ClaudePty => crate::tool::Tool::Claude,
            LaunchTool::Gemini => crate::tool::Tool::Gemini,
            LaunchTool::Codex => crate::tool::Tool::Codex,
            LaunchTool::OpenCode => crate::tool::Tool::OpenCode,
            LaunchTool::Kilo => crate::tool::Tool::Kilo,
            LaunchTool::Pi => crate::tool::Tool::Pi,
            LaunchTool::Antigravity => crate::tool::Tool::Antigravity,
            LaunchTool::Cursor => crate::tool::Tool::Cursor,
            LaunchTool::Kimi => crate::tool::Tool::Kimi,
            LaunchTool::Copilot => crate::tool::Tool::Copilot,
        }
    }

    /// Integration spec for this launch surface (shared with the base `Tool`).
    pub fn spec(&self) -> &'static crate::integration_spec::IntegrationSpec {
        self.tool().spec()
    }

    /// Base tool name (without -pty suffix). Equivalent to `self.tool().as_str()`.
    pub fn base_tool(&self) -> &'static str {
        self.tool().as_str()
    }

    /// Whether this tool uses the PTY wrapper.
    pub fn uses_pty(&self) -> bool {
        // ClaudePty is a launch surface (alias), not a Tool variant — it always
        // takes the PTY path. Everything else defers to the canonical spec.
        if matches!(self, LaunchTool::ClaudePty) {
            return true;
        }
        self.spec().launch.uses_pty_default
    }

    /// Executable name on PATH for this tool.
    pub fn cli_binary(&self) -> &'static str {
        self.spec().cli_binary
    }
}

/// How the child process is hosted. Computed from (tool, background, pty) at
/// launch time so dispatch doesn't have to re-derive the combination.
///
/// - `InteractiveVisible`: foreground, user-visible terminal. All tools.
/// - `HeadlessPty`:       background, PTY wrapper in a detached runner. Default
///   for gemini/codex/opencode and for default claude `--headless`.
/// - `NativePrint`:       background, direct claude spawn in print mode
///   (`-p --output-format stream-json --verbose`). Claude only, opt-in via an
///   explicit `-p`/`--print`; kept alive across turns by hcom's stop-hook loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchBackend {
    InteractiveVisible,
    HeadlessPty,
    NativePrint,
}

impl LaunchBackend {
    /// Resolve from the already-prepared (tool, background) pair.
    ///
    /// The PTY-vs-print decision for Claude is encoded in the [`LaunchTool`]
    /// surface chosen by `launch()`: `Claude` is the `-p`/`--print` surface
    /// (→ `NativePrint`), `ClaudePty` is the live PTY surface (→ `HeadlessPty`).
    /// Background Claude defaults to `ClaudePty`; it only becomes
    /// `Claude`/`NativePrint` when the caller passes `-p`/`--print`.
    pub fn resolve(tool: &LaunchTool, background: bool) -> Self {
        if !background {
            return LaunchBackend::InteractiveVisible;
        }
        match tool {
            LaunchTool::Claude => LaunchBackend::NativePrint,
            LaunchTool::ClaudePty => LaunchBackend::HeadlessPty,
            LaunchTool::Gemini
            | LaunchTool::Codex
            | LaunchTool::OpenCode
            | LaunchTool::Kilo
            | LaunchTool::Pi
            | LaunchTool::Antigravity
            | LaunchTool::Cursor
            | LaunchTool::Kimi
            | LaunchTool::Copilot => LaunchBackend::HeadlessPty,
        }
    }
}

/// Launch parameters.
#[derive(Clone)]
pub struct LaunchParams {
    pub tool: String,
    pub count: usize,
    pub args: Vec<String>,
    /// Raw user/config args to persist for future resume, before hcom injections.
    pub persisted_args: Option<Vec<String>>,
    /// Session id being resumed, inherited by the recreated instance row so a
    /// kill before the tool's first turn (no hook re-bind yet) stays resumable.
    pub prior_session_id: Option<String>,
    pub tag: Option<String>,
    pub system_prompt: Option<String>,
    pub initial_prompt: Option<String>,
    pub background: bool,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub launcher: Option<String>,
    pub run_here: Option<bool>,
    pub batch_id: Option<String>,
    pub name: Option<String>,
    pub skip_validation: bool,
    pub terminal: Option<String>,
    pub append_reply_handoff: bool,
}

impl Default for LaunchParams {
    fn default() -> Self {
        Self {
            tool: "claude".to_string(),
            count: 1,
            args: Vec::new(),
            persisted_args: None,
            prior_session_id: None,
            tag: None,
            system_prompt: None,
            initial_prompt: None,
            background: false,
            cwd: None,
            env: None,
            launcher: None,
            run_here: None,
            batch_id: None,
            name: None,
            skip_validation: false,
            terminal: None,
            append_reply_handoff: true,
        }
    }
}

/// Launch result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LaunchResult {
    pub tool: String,
    pub batch_id: String,
    pub launched: usize,
    pub failed: usize,
    pub background: bool,
    pub log_files: Vec<String>,
    pub handles: Vec<serde_json::Value>,
    pub errors: Vec<serde_json::Value>,
}

/// Predict if launch will block current terminal (run in same window).
/// Find tool executable path with fallbacks.
/// Claude has special fallback locations; other tools just use PATH.
fn find_tool_path(tool: &str) -> Option<String> {
    crate::terminal::which_bin(tool)
}

/// Check if tool CLI is installed (PATH + fallbacks).
fn is_tool_installed(tool: &str) -> bool {
    find_tool_path(tool).is_some()
}

pub fn will_run_in_current_terminal(
    count: usize,
    background: bool,
    run_here: Option<bool>,
    terminal: Option<&str>,
    inside_ai_tool: bool,
) -> bool {
    if let Some(rh) = run_here {
        return rh;
    }
    // terminal=here forces current terminal
    if terminal == Some("here") {
        return true;
    }
    if inside_ai_tool {
        return false;
    }
    if background {
        return false;
    }
    count == 1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchEnvRegime {
    HumanShell,
    ContaminatedParent,
    RunHere,
}

pub fn launch_env_regime(run_here: bool, inside_ai_tool: bool) -> LaunchEnvRegime {
    if run_here {
        LaunchEnvRegime::RunHere
    } else if contaminated_parent_with_inside_ai_tool(inside_ai_tool) {
        LaunchEnvRegime::ContaminatedParent
    } else {
        LaunchEnvRegime::HumanShell
    }
}

pub fn contaminated_parent() -> bool {
    contaminated_parent_with_inside_ai_tool(crate::shared::platform::is_inside_ai_tool())
}

fn contaminated_parent_with_inside_ai_tool(inside_ai_tool: bool) -> bool {
    inside_ai_tool
        || std::env::var_os("CI").is_some()
        || std::env::var_os("GITHUB_ACTIONS").is_some()
        || std::env::vars_os().any(|(key, _)| key.to_string_lossy().starts_with("CARGO_"))
}

/// Build base environment, then overlay config.toml + ~/.hcom/env (these win).
///
/// This makes the new-window/runner-script path behave like the PTY path
/// (`Command::new` inherits parent env by default). The runner script
/// already `unset`s TOOL_MARKER_VARS + HCOM_IDENTITY_VARS before exec,
/// so those categories are safe to include here (they're cleared in-script).
pub fn build_launch_env(
    hcom_config: &HcomConfig,
    regime: LaunchEnvRegime,
) -> HashMap<String, String> {
    build_launch_env_with_resolver(hcom_config, regime, crate::shell_env::resolved_shell_env)
}

fn build_launch_env_with_resolver<F>(
    hcom_config: &HcomConfig,
    regime: LaunchEnvRegime,
    resolved_shell_env: F,
) -> HashMap<String, String>
where
    F: Fn() -> Option<HashMap<String, String>>,
{
    let base: HashMap<String, String> = match regime {
        LaunchEnvRegime::HumanShell | LaunchEnvRegime::RunHere => std::env::vars().collect(),
        LaunchEnvRegime::ContaminatedParent => {
            resolved_shell_env().unwrap_or_else(|| std::env::vars().collect())
        }
    };
    let strip = match regime {
        LaunchEnvRegime::RunHere => run_here_env_strip_set(),
        LaunchEnvRegime::HumanShell | LaunchEnvRegime::ContaminatedParent => env_strip_set(),
    };
    let mut env: HashMap<String, String> = base
        .into_iter()
        .filter(|(k, _)| !strip.contains(k.as_str()))
        .collect();

    // HCOM_* settings from config.toml
    for (key, value) in hcom_config.to_env_dict() {
        if !value.is_empty() {
            env.insert(key, value);
        }
    }

    // Passthrough vars from env file (these win over everything)
    let env_path = paths::hcom_path(&["env"]);
    for (key, value) in config::load_env_extras(&env_path) {
        if !value.is_empty() {
            env.insert(key, value);
        }
    }

    env
}

/// Build the set of env var names to strip from inherited env.
///
/// Three closed categories (owned by hcom):
/// 1. HCOM_IDENTITY_VARS
/// 2. TOOL_MARKER_VARS
/// 3. TERMINAL_CONTEXT_VARS
///
/// Per-tool instance-state vars are NOT in the initial strip — they are
/// stripped per-instance later via `strip_instance_state_vars` so that
/// cross-tool nesting doesn't strip vars the child tool doesn't own.
fn env_strip_set() -> std::collections::HashSet<String> {
    let mut strip: std::collections::HashSet<String> = std::collections::HashSet::new();

    strip.extend(run_here_env_strip_set());
    for v in crate::terminal::TERMINAL_CONTEXT_VARS {
        strip.insert((*v).to_string());
    }

    strip
}

fn run_here_env_strip_set() -> std::collections::HashSet<String> {
    let mut strip: std::collections::HashSet<String> = std::collections::HashSet::new();

    for v in crate::shared::constants::HCOM_IDENTITY_VARS {
        strip.insert((*v).to_string());
    }
    for v in tool_marker_vars() {
        strip.insert((*v).to_string());
    }
    strip.insert("HCOM_LAUNCHED_PRESET".to_string());

    strip
}

fn isolated_tool_config_dir(tool: &LaunchTool) -> Option<std::path::PathBuf> {
    let root = crate::runtime_env::tool_config_root();
    if dirs::home_dir().as_deref() == Some(root.as_path()) {
        return None;
    }
    let dirname = match tool.tool() {
        crate::tool::Tool::Claude => ".claude",
        crate::tool::Tool::Gemini | crate::tool::Tool::Antigravity => ".gemini",
        crate::tool::Tool::Codex => ".codex",
        crate::tool::Tool::Kilo => ".kilo",
        crate::tool::Tool::Pi => ".pi",
        crate::tool::Tool::Cursor => ".cursor",
        crate::tool::Tool::Kimi => ".kimi",
        crate::tool::Tool::Copilot => ".copilot",
        crate::tool::Tool::OpenCode | crate::tool::Tool::Adhoc => return None,
    };
    Some(root.join(dirname))
}

/// Get system prompt file path for Gemini/Codex.
fn get_system_prompt_path(tool: &str) -> std::path::PathBuf {
    let prompts_dir = paths::hcom_path(&["system-prompts"]);
    fs::create_dir_all(&prompts_dir).ok();
    prompts_dir.join(format!("{}.md", tool))
}

/// Write system prompt to file (only if content differs).
fn write_system_prompt_file(system_prompt: &str, tool: &str) -> String {
    let filepath = get_system_prompt_path(tool);

    // Only write if content differs
    if let Ok(existing) = fs::read_to_string(&filepath)
        && existing == system_prompt
    {
        return filepath.to_string_lossy().to_string();
    }

    if let Err(e) = fs::write(&filepath, system_prompt) {
        eprintln!(
            "[hcom] warn: failed to write system prompt to {}: {e}",
            filepath.display()
        );
    }
    filepath.to_string_lossy().to_string()
}

/// Generate a UUID v4-like process ID string.
fn generate_process_id() -> String {
    let mut rng = rand::rng();
    let a: u32 = rng.random();
    let b: u16 = rng.random();
    let c: u16 = (rng.random::<u16>() & 0x0FFF) | 0x4000; // version 4
    let d: u16 = (rng.random::<u16>() & 0x3FFF) | 0x8000; // variant 1
    let e: u64 = rng.random::<u64>() & 0xFFFFFFFFFFFF; // 48 bits
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a, b, c, d, e)
}

fn install_diag_context(tool: &LaunchTool, paths: &[(&str, std::path::PathBuf)]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "Diagnostic context:");
    for (label, p) in paths {
        let _ = writeln!(out, "  resolved {label}={}", p.display());
    }
    let _ = writeln!(
        out,
        "  HCOM_DIR={}",
        std::env::var("HCOM_DIR").unwrap_or_else(|_| "<unset>".into())
    );
    let tool_env_var = tool.spec().launch.config_dir_env;
    if let Some(env_var) = tool_env_var {
        let _ = writeln!(
            out,
            "  {env_var}={}",
            std::env::var(env_var).unwrap_or_else(|_| "<unset>".into())
        );
    }
    let _ = writeln!(
        out,
        "  cwd={}",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".into())
    );
    out
}

/// Verify hooks are installed for the target tool, auto-install if needed.
///
/// Uses verify-first pattern: read-only check first, only write if needed.
/// Strict gate: refuses to launch if hooks can't be installed.
fn ensure_hooks_installed(tool: &LaunchTool, include_permissions: bool) -> Result<()> {
    match tool {
        LaunchTool::Claude | LaunchTool::ClaudePty => {
            if crate::hooks::claude::verify_claude_hooks_installed(None, include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::claude::try_setup_claude_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[(
                        "settings_path",
                        crate::hooks::claude::get_claude_settings_path(),
                    )],
                );
                bail!(
                    "Failed to setup Claude hooks: {e}\n\
                     Run: hcom hooks add claude\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Gemini => {
            if !crate::hooks::gemini::is_gemini_version_supported() {
                if let Some(ver) = crate::hooks::gemini::get_gemini_version() {
                    bail!(
                        "Gemini CLI version {}.{}.{} is too old. Update: npm i -g @google/gemini-cli@latest",
                        ver.0,
                        ver.1,
                        ver.2
                    );
                } else {
                    eprintln!("Warning: Could not detect Gemini CLI version");
                }
            }
            if crate::hooks::gemini::verify_gemini_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::gemini::try_setup_gemini_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[(
                        "settings_path",
                        crate::hooks::gemini::get_gemini_settings_path(),
                    )],
                );
                bail!(
                    "Failed to setup Gemini hooks: {e}\n\
                     Run: hcom hooks add gemini\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Codex => {
            if crate::hooks::codex::verify_codex_hooks_installed(include_permissions)
                && crate::hooks::codex::codex_current_feature_enabled()
            {
                return Ok(());
            }
            if let Err(e) = crate::hooks::codex::try_setup_codex_hooks(include_permissions) {
                if matches!(e, crate::hooks::codex::SetupError::HookTrustFailed { .. }) {
                    crate::log::log_warn(
                        "codex",
                        "codex.hook_trust_setup_warn",
                        &format!(
                            "Codex hook setup could not write trust state; launch preprocessing may fall back to hook-trust bypass: {e}"
                        ),
                    );
                } else {
                    let diag = install_diag_context(
                        tool,
                        &[
                            ("config_path", crate::hooks::codex::get_codex_config_path()),
                            ("hooks_path", crate::hooks::codex::get_codex_hooks_path()),
                        ],
                    );
                    bail!(
                        "Failed to setup Codex hooks: {e}\n\
                         Run: hcom hooks add codex\n\
                         {diag}"
                    );
                }
            }
            Ok(())
        }
        LaunchTool::OpenCode => {
            if crate::hooks::opencode::ensure_plugin_installed("opencode") {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup OpenCode plugin. Run: hcom hooks add opencode\n{diag}");
        }
        LaunchTool::Kilo => {
            if crate::hooks::opencode::ensure_plugin_installed("kilo") {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup Kilo Code plugin. Run: hcom hooks add kilo\n{diag}");
        }
        LaunchTool::Pi => {
            if crate::hooks::pi::ensure_pi_plugin_installed() {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup Pi plugin. Run: hcom hooks add pi\n{diag}");
        }
        LaunchTool::Antigravity => {
            if crate::hooks::antigravity::verify_antigravity_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) =
                crate::hooks::antigravity::try_setup_antigravity_hooks(include_permissions)
            {
                let diag = install_diag_context(
                    tool,
                    &[(
                        "hooks_path",
                        crate::hooks::antigravity::get_antigravity_hooks_path(),
                    )],
                );
                bail!(
                    "Failed to setup Antigravity hooks: {e}\n\
                     Run: hcom hooks add antigravity\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Cursor => {
            if crate::hooks::cursor::verify_cursor_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::cursor::try_setup_cursor_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[("hooks_path", crate::hooks::cursor::get_cursor_hooks_path())],
                );
                bail!(
                    "Failed to setup Cursor hooks: {e}\n\
                     Run: hcom hooks add cursor\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Kimi => {
            if crate::hooks::kimi::verify_kimi_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::kimi::try_setup_kimi_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[("hooks_path", crate::hooks::kimi::get_kimi_settings_path())],
                );
                bail!(
                    "Failed to setup Kimi hooks: {e}\n\
                     Run: hcom hooks add kimi\n\
                     {diag}"
                );
            }
            Ok(())
        }
        LaunchTool::Copilot => {
            if crate::hooks::copilot::verify_copilot_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::copilot::try_setup_copilot_hooks(include_permissions) {
                let diag = install_diag_context(
                    tool,
                    &[(
                        "hooks_path",
                        crate::hooks::copilot::get_copilot_hooks_path(),
                    )],
                );
                bail!(
                    "Failed to setup Copilot hooks: {e}\n\
                     Run: hcom hooks add copilot\n\
                     {diag}"
                );
            }
            Ok(())
        }
    }
}

/// Build a command string for Claude (non-PTY mode).
fn build_claude_command(args: &[String]) -> String {
    let mut parts = vec!["claude".to_string()];
    for arg in args {
        parts.push(crate::tools::args_common::shell_quote(arg));
    }
    parts.join(" ")
}

/// Tool-specific extra environment variables for PTY mode.
fn tool_extra_env(tool: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if tool == "claude" {
        m.insert("HCOM_PTY_MODE".to_string(), "1".to_string());
    }
    if tool == "antigravity" {
        m.insert("ANTIGRAVITY_AGENT".to_string(), "1".to_string());
    }
    m
}

fn background_runner_env(
    tool: &str,
    env: &HashMap<String, String>,
    instance_name: &str,
) -> HashMap<String, String> {
    let mut runner_env = env.clone();
    runner_env.insert("HCOM_INSTANCE_NAME".to_string(), instance_name.to_string());
    // Default HCOM_TOOL when the caller didn't already set it (most callers
    // come from `launch()` which inserts it; this keeps the standalone PTY
    // and headless paths consistent so `{tool}` template substitution and
    // delivery-loop label formatting see the right value).
    runner_env
        .entry("HCOM_TOOL".to_string())
        .or_insert_with(|| tool.to_string());
    runner_env.extend(tool_extra_env(tool));
    runner_env
}

/// Create a bash script that runs a tool via the hcom native PTY wrapper.
///
/// The script sets up the environment and calls `hcom pty <tool> [args...]`.
pub fn create_runner_script(
    tool: &str,
    cwd: &str,
    instance_name: &str,
    env: &HashMap<String, String>,
    tool_args: &[String],
    run_here: bool,
) -> Result<String> {
    // Resolve the tool's IntegrationSpec for instance-state env stripping
    let tool_spec = tool.parse::<crate::tool::Tool>().map(|t| t.spec()).ok();
    let instance_state_env: &[&str] = tool_spec.map(|s| s.instance_state_env).unwrap_or(&[]);
    let native_bin = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("hcom"));
    let native_bin_str = native_bin.to_string_lossy();

    let launch_dir = paths::hcom_path(&[paths::LAUNCH_DIR]);
    fs::create_dir_all(&launch_dir).ok();

    let script_file = launch_dir.join(format!(
        "{}_{}_{}_{}.sh",
        tool,
        instance_name,
        std::process::id(),
        rand::random::<u16>() % 9000 + 1000
    ));

    // Route ALL forwarded non-HCOM env vars through the 0600 sidecar.
    // The visible .sh only exports HCOM_* vars + PATH + cwd (minimal).
    // This avoids the sensitivity-classification heuristic entirely — no
    // secret ever lands in the 0755 world-readable script.
    let hcom_env: HashMap<String, String> = env
        .iter()
        .filter(|(k, _)| k.starts_with("HCOM_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let sidecar_strip: std::collections::HashSet<&str> = tool_marker_vars()
        .iter()
        .chain(HCOM_IDENTITY_VARS.iter())
        .chain(instance_state_env.iter())
        .chain(crate::terminal::TERMINAL_COLOR_VARS.iter())
        .copied()
        .collect();
    let ambient_env: HashMap<String, String> = env
        .iter()
        .filter(|(k, _)| !k.starts_with("HCOM_") && !sidecar_strip.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let env_block = terminal::build_env_string(&hcom_env, "bash_export");
    let sensitive_env_source = if ambient_env.is_empty() {
        String::new()
    } else {
        let env_file = launch_dir.join(format!(
            "{}_{}_{}_{}.env",
            tool,
            instance_name,
            std::process::id(),
            rand::random::<u16>() % 9000 + 1000
        ));
        let mut file = crate::sys::fs::create_private_new(&env_file)?;
        writeln!(
            file,
            "{}",
            terminal::build_env_string(&ambient_env, "bash_export")
        )?;
        let quoted = crate::tools::args_common::shell_quote(&env_file.to_string_lossy());
        format!("if [ -f {quoted} ]; then\n  . {quoted}\n  rm -f {quoted}\nfi")
    };
    let tool_args_str: String = tool_args
        .iter()
        .map(|a| crate::tools::args_common::shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    // Resolve binary paths for minimal PATH environments
    let mut path_dirs: Vec<String> = Vec::new();

    // Dev mode: prepend the worktree's Cargo output dir
    if let Ok(dev_root) = std::env::var("HCOM_DEV_ROOT")
        && let Some(bin) = crate::shared::dev_root_binary(Path::new(&dev_root))
        && let Some(dir) = bin.parent()
    {
        path_dirs.push(dir.to_string_lossy().into_owned());
    }

    let tool_bin = tool
        .parse::<crate::tool::Tool>()
        .map(|t| t.spec().cli_binary)
        .unwrap_or(tool);
    for bin_name in &[tool_bin, "hcom", "python3", "node"] {
        if let Some(bin_path) = terminal::which_bin(bin_name)
            && let Some(dir) = Path::new(&bin_path).parent()
        {
            let d = dir.to_string_lossy().to_string();
            if !path_dirs.contains(&d) {
                path_dirs.push(d);
            }
        }
    }

    let path_export = if !path_dirs.is_empty() {
        format!("export PATH=\"{}:$PATH\"", path_dirs.join(":"))
    } else {
        String::new()
    };

    let use_exec = if run_here { "" } else { "exec " };

    let content = format!(
        "#!/bin/bash\n\
         # {} hcom native PTY runner ({})\n\
         # Using: {}\n\
         cd {}\n\
         \n\
         unset {}\n\
         unset {}\n\
         unset {}\n\
         {}\n\
         {}\n\
         {}\n\
         \n\
         {}{} pty {} {}\n",
        tool.chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .collect::<String>()
            + &tool[1..],
        instance_name,
        native_bin_str,
        crate::tools::args_common::shell_quote(cwd),
        tool_marker_vars().join(" "),
        HCOM_IDENTITY_VARS.join(" "),
        instance_state_env.join(" "),
        env_block,
        sensitive_env_source,
        path_export,
        use_exec,
        crate::tools::args_common::shell_quote(&native_bin_str),
        tool,
        tool_args_str,
    );

    fs::write(&script_file, &content)?;
    crate::sys::fs::set_executable(&script_file)?;

    crate::log::log_info(
        "pty",
        "native.script",
        &format!(
            "script={} tool={} instance={} forwarded_keys=[{}]",
            script_file.display(),
            tool,
            instance_name,
            ambient_env.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    );

    Ok(script_file.to_string_lossy().to_string())
}

/// Launch a tool via PTY wrapper in a terminal.
#[allow(clippy::too_many_arguments)]
pub fn launch_pty(
    tool: &str,
    cwd: &str,
    env: &HashMap<String, String>,
    instance_name: &str,
    tool_args: &[String],
    run_here: bool,
    terminal: Option<&str>,
    inside_ai_tool: bool,
) -> Result<bool> {
    if env.get("HCOM_PROCESS_ID").is_none_or(|v| v.is_empty()) {
        crate::log::log_error(
            "pty",
            "pty.exit",
            &format!("HCOM_PROCESS_ID not set in env for {}", instance_name),
        );
        return Ok(false);
    }

    let mut runner_env = env.clone();
    runner_env.insert("HCOM_INSTANCE_NAME".to_string(), instance_name.to_string());
    runner_env
        .entry("HCOM_TOOL".to_string())
        .or_insert_with(|| tool.to_string());
    runner_env.extend(tool_extra_env(tool));

    let script_file =
        create_runner_script(tool, cwd, instance_name, &runner_env, tool_args, run_here)?;

    let command = format!(
        "bash {}",
        crate::tools::args_common::shell_quote(&script_file)
    );
    let terminal_env: HashMap<String, String> = runner_env
        .iter()
        .filter(|(k, _)| k.starts_with("HCOM_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let (launch_result, effective_preset) = terminal::launch_terminal(
        &command,
        &terminal_env,
        Some(cwd),
        false, // not background
        run_here,
        terminal,
        inside_ai_tool,
    )?;

    instance_binding::persist_terminal_launch_context(
        &crate::db::HcomDb::open()?,
        instance_name,
        terminal,
        &effective_preset,
        env.get("HCOM_PROCESS_ID").map(|s| s.as_str()),
    );

    match launch_result {
        terminal::LaunchResult::Success => Ok(true),
        terminal::LaunchResult::Background(_, _) => Ok(true),
        terminal::LaunchResult::Failed(_) => Ok(false),
    }
}

/// Identity and tracking context for a background launch, shared across tool types.
struct BackgroundLaunchCtx<'a> {
    db: &'a HcomDb,
    tool: &'a str,
    instance_name: &'a str,
    process_id: &'a str,
    terminal_mode: Option<&'a str>,
    tag: &'a str,
    working_dir: &'a str,
    log_files: &'a mut Vec<String>,
    handles: &'a mut Vec<serde_json::Value>,
}

/// Shared bookkeeping after a successful background launch for gemini/codex/opencode.
/// Persists the launch context, updates position, records the PID, and appends
/// log_file / handle entries. Per-tool differences (args, prompt) stay in the caller.
fn finalize_background_launch(
    ctx: &mut BackgroundLaunchCtx<'_>,
    log_file: String,
    pid: u32,
    effective_preset: String,
) {
    instance_binding::persist_terminal_launch_context(
        ctx.db,
        ctx.instance_name,
        ctx.terminal_mode,
        &effective_preset,
        Some(ctx.process_id),
    );
    instances::update_instance_position(
        ctx.db,
        ctx.instance_name,
        &serde_json::Map::from_iter([
            ("pid".to_string(), json!(pid)),
            ("background_log_file".to_string(), json!(&log_file)),
        ]),
    );
    crate::pidtrack::record_pid(&crate::pidtrack::PidRecord {
        process_id: ctx.process_id,
        terminal_preset: &effective_preset,
        tag: ctx.tag,
        ..crate::pidtrack::PidRecord::new(
            &crate::paths::hcom_dir(),
            pid,
            ctx.tool,
            ctx.instance_name,
            ctx.working_dir,
        )
    });
    ctx.log_files.push(log_file.clone());
    ctx.handles.push(json!({
        "tool": ctx.tool,
        "instance_name": ctx.instance_name,
        "log_file": log_file,
        "pid": pid,
    }));
}

fn launch_background_runner(
    tool: &str,
    cwd: &str,
    instance_name: &str,
    instance_env: &mut HashMap<String, String>,
    tool_args: &[String],
    terminal_mode: Option<&str>,
    inside_ai_tool: bool,
) -> Result<(String, u32, String)> {
    let log_filename = format!(
        "background_{}_{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        rand::random::<u16>() % 9000 + 1000
    );
    let mut runner_env = background_runner_env(tool, instance_env, instance_name);
    runner_env.insert("HCOM_BACKGROUND".to_string(), log_filename);
    let script_file =
        create_runner_script(tool, cwd, instance_name, &runner_env, tool_args, false)?;
    let command = format!(
        "bash {}",
        crate::tools::args_common::shell_quote(&script_file)
    );
    let terminal_env: HashMap<String, String> = runner_env
        .iter()
        .filter(|(k, _)| k.starts_with("HCOM_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let (launch_result, effective_preset) = terminal::launch_terminal(
        &command,
        &terminal_env,
        Some(cwd),
        true,
        false,
        terminal_mode,
        inside_ai_tool,
    )?;
    match launch_result {
        terminal::LaunchResult::Background(log_file, pid) => Ok((log_file, pid, effective_preset)),
        _ => bail!("background launch failed"),
    }
}

/// Common launch path for gemini/codex/opencode: background or PTY foreground.
///
/// Handles `launch_background_runner` + `finalize_background_launch` for background,
/// and `will_run_in_current_terminal` + `launch_pty` for foreground.
fn launch_pty_or_background(
    ctx: &mut BackgroundLaunchCtx<'_>,
    instance_env: &mut HashMap<String, String>,
    tool_args: &[String],
    params: &LaunchParams,
    inside_ai_tool: bool,
) -> Result<bool> {
    if params.background {
        let (log_file, pid, effective_preset) = launch_background_runner(
            ctx.tool,
            ctx.working_dir,
            ctx.instance_name,
            instance_env,
            tool_args,
            ctx.terminal_mode,
            inside_ai_tool,
        )?;
        finalize_background_launch(ctx, log_file, pid, effective_preset);
        Ok(true)
    } else {
        let effective_run_here = will_run_in_current_terminal(
            params.count,
            false,
            params.run_here,
            ctx.terminal_mode,
            inside_ai_tool,
        );
        let ok = launch_pty(
            ctx.tool,
            ctx.working_dir,
            instance_env,
            ctx.instance_name,
            tool_args,
            effective_run_here,
            ctx.terminal_mode,
            inside_ai_tool,
        )?;
        if ok {
            ctx.handles
                .push(json!({"tool": ctx.tool, "instance_name": ctx.instance_name}));
        }
        Ok(ok)
    }
}

/// Resolve a naming conflict for an explicit instance name.
///
/// - Name is free → Ok(()).
/// - Name held by an inactive row → consume the row (delete) and return Ok(()).
///   An inactive row is a resume handle from agy soft-finalize; launch will
///   re-create a fresh row with the same name.
/// - Name held by a `pending` placeholder reservation → Ok(()) without
///   deleting. This is *our own* reservation: the fork/resume path calls
///   `reserve_generated_name` (under flock, against an unused name) before the
///   launch, then passes that name as `params.name`. The pre-register step
///   (`initialize_instance_in_position_file`) promotes the placeholder in
///   place, so it must survive — bailing here broke every tracked `hcom f`.
/// - Name held by anything else (listening/active/blocked) → Err.
fn resolve_explicit_name_conflict(db: &HcomDb, name: &str) -> Result<()> {
    let Some(row) = db.get_instance(name).ok().flatten() else {
        return Ok(());
    };
    let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if status == "inactive" {
        db.delete_instance(name).map_err(|e| {
            anyhow::anyhow!("Failed to clear inactive resume row '{}': {}", name, e)
        })?;
        return Ok(());
    }
    // A pending placeholder with no session yet is a reservation, not a live
    // agent — leave it for the launcher's pre-register promotion.
    let status_context = row
        .get("status_context")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let session_empty = row
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.is_empty())
        .unwrap_or(true);
    if status == instance_names::PLACEHOLDER_STATUS
        && status_context == instance_names::PLACEHOLDER_CONTEXT
        && session_empty
    {
        return Ok(());
    }
    bail!(
        "Instance '{}' already exists (stop it first or use a different name)",
        name
    );
}

/// Inject ephemeral workspace trust flags into args for gemini and codex.
///
/// - gemini: adds `--skip-trust` (session-scoped, no persisted state)
/// - codex: adds `-c` with a TOML inline-table value that sets trust for the
///   canonical CWD. Uses key `projects` (no dots in the key path) so codex's
///   naive `.`-splitting never touches the path itself, making it robust to
///   dotted directory components. Paths containing a literal `"` or backslash
///   would break the TOML quoting — a Windows-only edge case, not handled here.
///
/// When `auto_trust` is false, returns immediately without modifying args.
/// Idempotent: no-op if the relevant flag is already present.
/// Cursor is handled separately via `ensure_cursor_workspace_trusted`.
pub(crate) fn inject_workspace_trust_args(
    tool: &LaunchTool,
    canonical_dir: &std::path::Path,
    args: &mut Vec<String>,
    auto_trust: bool,
) {
    if !auto_trust {
        return;
    }
    match tool {
        LaunchTool::Gemini if !args.iter().any(|a| a == "--skip-trust") => {
            args.push("--skip-trust".to_string());
        }
        LaunchTool::Gemini => {}
        LaunchTool::Codex => {
            let already_set = args
                .windows(2)
                .any(|w| w[0] == "-c" && w[1].contains("trust_level"));
            if !already_set {
                let canonical_str = canonical_dir.to_string_lossy();
                // codex -c: key="projects" (no dots → no split issue), value is a TOML
                // inline table with the quoted path as key. apply_single_override replaces
                // the projects table for this session only (file stays untouched).
                let trust_override = format!(
                    "projects={{ \"{}\" = {{ trust_level = \"trusted\" }} }}",
                    canonical_str
                );
                args.push("-c".to_string());
                args.push(trust_override);
            }
        }
        _ => {}
    }
}

fn validate_launch_count(tool: &LaunchTool, count: usize) -> Result<()> {
    if count == 0 {
        bail!("Count must be positive");
    }
    let max = tool.spec().launch.max_launch_count;
    if count > max {
        bail!(
            "Too many {} instances requested (max {})",
            tool.as_str(),
            max
        );
    }
    Ok(())
}

fn append_initial_prompt_args(
    tool: &LaunchTool,
    args: &mut Vec<String>,
    prompt: String,
) -> Result<()> {
    match tool.spec().launch.initial_prompt {
        crate::integration_spec::InitialPromptShape::Unsupported { reason } => bail!("{reason}"),
        crate::integration_spec::InitialPromptShape::DashDashPositional => {
            args.push("--".to_string());
            args.push(prompt);
        }
        crate::integration_spec::InitialPromptShape::Positional => args.push(prompt),
        crate::integration_spec::InitialPromptShape::Flag(flag) => {
            args.push(flag.to_string());
            args.push(prompt);
        }
    }
    Ok(())
}

/// Launch one or more AI tool instances with consistent tracking.
///
/// This is the unified entry point for launching Claude, Gemini, Codex,
/// and OpenCode instances with batch tracking, environment setup, and
/// error handling.
pub fn launch(db: &HcomDb, mut params: LaunchParams) -> Result<LaunchResult> {
    // Claude background defaults to the live PTY surface (`ClaudePty`); it only
    // drops to the `-p`/`--print` surface (`Claude`/`NativePrint`) when the
    // caller explicitly passes `-p`/`--print` in the args. Both stay alive —
    // PTY hosts the live TUI, print mode loops via the stop hook. `-p` is gated
    // behind explicit opt-in because, from 2026-06-15, `claude -p` draws from a
    // separate Agent SDK credit pool on subscription plans.
    // Exact-token match is intentional: `--print` is a boolean flag, so an
    // equals form (`--print=x`) is malformed for claude and surfaces as a
    // launch failure rather than being silently mis-routed to the PTY surface.
    let claude_print = (params.tool == "claude" || params.tool == "claude-pty")
        && params
            .args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-p" | "--print"));

    // `claude-pty` is the PTY surface; `-p`/`--print` selects the NativePrint
    // surface. The two are mutually exclusive: passing both would route print
    // mode through the HeadlessPty runner, which is broken. Fail fast.
    if params.tool == "claude-pty" && claude_print {
        bail!(
            "The PTY surface does not support `-p`/`--print`.\n\
             Use one of:\n\
             • `hcom claude --headless`  — live PTY session\n\
             • `hcom claude -p ...`      — print/pipe mode"
        );
    }

    let normalized = if params.tool == "claude" && !claude_print {
        LaunchTool::ClaudePty
    } else {
        LaunchTool::from_str(&params.tool)?
    };
    let base_tool = normalized.base_tool();
    let backend = LaunchBackend::resolve(&normalized, params.background);

    // Validation
    validate_launch_count(&normalized, params.count)?;

    // HCOM_DIR placement: refuse if it sits under a tool-protected metadata
    // directory. codex hard-denies apply_patch into these via
    // FileSystemSandboxPolicy with no approval path; claude/gemini gate them
    // behind permission prompts on every hcom write. Either way the user gets
    // a broken session — fail fast at launch with a clear message instead.
    let hcom_dir_path = paths::hcom_dir();
    if let Some(protected) = paths::protected_hcom_dir_component(&hcom_dir_path) {
        bail!(
            "HCOM_DIR ({}) sits under a protected directory component '{}'.\n\
             AI tools (codex/claude/gemini) deny writes under .git/.codex/.claude/.agents,\n\
             which would block hcom DB writes from the launched agent.\n\
             Set HCOM_DIR to a path outside these directories.",
            hcom_dir_path.display(),
            protected
        );
    }

    let tool_binary = normalized.cli_binary();
    if !is_tool_installed(tool_binary) {
        bail!("'{}' is not installed or not in PATH", tool_binary);
    }

    if !params.skip_validation {
        let validation_errors = validate_tool_args(&normalized, &params.args);
        if !validation_errors.is_empty() {
            bail!("{}", validation_errors.join("\n"));
        }
    }

    // Load config before hook setup so auto_approve is authoritative for
    // wrapped launches as well as manual `hcom hooks add`.
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|e| {
        eprintln!("[hcom] warn: config load failed, using defaults: {e}");
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    // For Codex: probe CODEX_HOME writability synchronously. Sandboxed parent
    // codex would otherwise spawn a child that hangs on the readonly-state-DB
    // repair prompt. Failing here lets the parent's sandbox-escalation flow
    // surface the denial to the user.
    if matches!(normalized, LaunchTool::Codex) {
        crate::tools::codex_preprocessing::ensure_codex_home_writable()?;
    }

    let inside_ai_tool = crate::shared::context::HcomContext::from_os().is_inside_ai_tool();
    let terminal_mode = params
        .terminal
        .as_deref()
        .or(Some(hcom_config.terminal.as_str()).filter(|t| !t.is_empty()));
    let base_env_run_here = will_run_in_current_terminal(
        params.count,
        params.background,
        params.run_here,
        terminal_mode,
        inside_ai_tool,
    );

    // Ensure hooks are installed (strict: refuse to launch without hooks)
    ensure_hooks_installed(&normalized, hcom_config.auto_approve)?;

    // Build base environment for the current launch regime, then overlay
    // config.toml + ~/.hcom/env which win.
    let mut base_env = build_launch_env(
        &hcom_config,
        launch_env_regime(base_env_run_here, inside_ai_tool),
    );
    if let Some(ref caller_env) = params.env {
        base_env.extend(caller_env.clone());
    }
    base_env.remove("HCOM_TERMINAL");
    if let Some(env_var) = normalized.spec().launch.config_dir_env
        && !base_env.contains_key(env_var)
        && std::env::var(env_var)
            .ok()
            .filter(|v| !v.is_empty())
            .is_none()
        && let Some(config_dir) = isolated_tool_config_dir(&normalized)
    {
        base_env.insert(
            env_var.to_string(),
            config_dir.to_string_lossy().to_string(),
        );
    }

    // Tag resolution
    let effective_tag = if let Some(ref tag) = params.tag {
        base_env.insert("HCOM_TAG".to_string(), tag.clone());
        tag.clone()
    } else if let Some(tag) = base_env.get("HCOM_TAG").cloned() {
        tag
    } else {
        let default = hcom_config.tag.clone();
        if !default.is_empty() {
            base_env.insert("HCOM_TAG".to_string(), default.clone());
        }
        default
    };

    // Explicit name validation
    if let Some(ref name) = params.name {
        if params.count > 1 {
            bail!(
                "Cannot use explicit name with count > 1 (count={})",
                params.count
            );
        }
        resolve_explicit_name_conflict(db, name)?;
    }

    // System prompt file for Gemini/Codex
    if let Some(ref sp) = params.system_prompt
        && normalized == LaunchTool::Gemini
    {
        let path = write_system_prompt_file(sp, "gemini");
        base_env.insert("GEMINI_SYSTEM_MD".to_string(), path);
    }

    let working_dir = params.cwd.as_deref().unwrap_or(".");
    let canonical_dir = std::fs::canonicalize(working_dir)
        .unwrap_or_else(|_| std::path::PathBuf::from(working_dir));
    // Folder trust: on first run each tool shows a "do you trust this folder?"
    // prompt — the user accepts to continue or declines and it exits. When an
    // agent launches another agent via hcom, auto-approve the prompt for the
    // launch dir to smooth the process. Cursor's lever is a marker file (its
    // `--trust` flag is print-only, inert in our PTY), so it's seeded here;
    // gemini/codex use arg injection below.
    if hcom_config.auto_trust_workspace && matches!(normalized, LaunchTool::Cursor) {
        cursor_preprocessing::ensure_cursor_workspace_trusted(&canonical_dir)?;
    }
    if hcom_config.auto_trust_workspace && matches!(normalized, LaunchTool::Copilot) {
        copilot_preprocessing::ensure_copilot_workspace_trusted(&canonical_dir)?;
    }

    // Capture the persistable args BEFORE any hcom launch injection below.
    // Resume replays only user/config args; workspace-trust injection
    // (gemini `--skip-trust`, codex `-c projects=…trust_level`) and the
    // `--hcom-prompt` translation are session/path-specific and must not be
    // baked into launch_args, or they would replay stale state on resume/fork.
    let stored_launch_args = params
        .persisted_args
        .clone()
        .unwrap_or_else(|| params.args.clone());

    inject_workspace_trust_args(
        &normalized,
        &canonical_dir,
        &mut params.args,
        hcom_config.auto_trust_workspace,
    );
    let launcher_name: String = params.launcher.take().unwrap_or_else(|| {
        // Try to resolve caller identity from the live process binding.
        let process_id = std::env::var("HCOM_PROCESS_ID").ok();
        match crate::identity::resolve_identity(
            db,
            None,
            None,
            None,
            process_id.as_deref(),
            None,
            None,
        ) {
            Ok(id) => id.name,
            Err(_) => "api".to_string(),
        }
    });

    // Inject --hcom-prompt into tool args (translated per-tool).
    // When a real hcom participant launched us, append a reply instruction so
    // the spawned agent knows to send its result back.
    if let Some(ref prompt) = params.initial_prompt {
        let reply_suffix =
            if params.append_reply_handoff && launcher_name != "api" && launcher_name != "user" {
                format!("\n\nWhen done, send your result back to @{launcher_name} via hcom.")
            } else {
                String::new()
            };
        let full_prompt = format!("{prompt}{reply_suffix}");
        append_initial_prompt_args(&normalized, &mut params.args, full_prompt)?;
    }
    let batch_id = params
        .batch_id
        .take()
        .unwrap_or_else(|| format!("{:08x}", rand::rng().random::<u32>()));

    let mut launched = 0usize;
    let mut log_files: Vec<String> = Vec::new();
    let mut handles: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<serde_json::Value> = Vec::new();

    for _ in 0..params.count {
        let mut instance_env = base_env.clone();
        instance_env.insert("HCOM_LAUNCHED".to_string(), "1".to_string());
        instance_env.insert(
            "HCOM_LAUNCH_EVENT_ID".to_string(),
            db.get_last_event_id().to_string(),
        );
        instance_env.insert("HCOM_LAUNCHED_BY".to_string(), launcher_name.to_string());
        instance_env.insert("HCOM_LAUNCH_BATCH_ID".to_string(), batch_id.clone());
        instance_env.insert(
            "HCOM_DIR".to_string(),
            paths::hcom_dir().to_string_lossy().to_string(),
        );

        // Propagate dev root
        if let Ok(val) = std::env::var("HCOM_DEV_ROOT") {
            instance_env.insert("HCOM_DEV_ROOT".to_string(), val);
        }
        // Propagate HCOM_NOTES
        if let Ok(val) = std::env::var("HCOM_NOTES") {
            instance_env.insert("HCOM_NOTES".to_string(), val);
        }

        let process_id = generate_process_id();
        instance_env.insert("HCOM_PROCESS_ID".to_string(), process_id.clone());

        // Fork mode detection
        if matches!(normalized, LaunchTool::Claude | LaunchTool::ClaudePty)
            && params.args.iter().any(|a| a == "--fork-session")
        {
            instance_env.insert("HCOM_IS_FORK".to_string(), "1".to_string());
        }

        let instance_name = if let Some(ref name) = params.name {
            name.clone()
        } else {
            instance_names::generate_unique_name(db)?
        };
        instance_env.insert("HCOM_INSTANCE_NAME".to_string(), instance_name.clone());

        // Process ID export: allow custom env var name
        if let Ok(export_var) = std::env::var("HCOM_PROCESS_ID_EXPORT")
            && !export_var.is_empty()
        {
            instance_env.insert(export_var, process_id.clone());
        }

        // Name/process export vars
        if let Ok(export_var) = std::env::var("HCOM_NAME_EXPORT") {
            if !export_var.is_empty() {
                instance_env.insert(export_var, instance_name.clone());
            }
        } else if !hcom_config.name_export.is_empty() {
            instance_env.insert(hcom_config.name_export.clone(), instance_name.clone());
        }

        let tool_type = base_tool;
        instance_env.insert("HCOM_TOOL".to_string(), tool_type.to_string());

        // Pre-format the pane title for templates that substitute
        // `{pane_title}` (custom user templates only — the built-in herdr
        // preset passes `{instance_name}` and the delivery loop pushes the
        // styled label via `pane.rename`). `display_for_title` mirrors
        // `identity::get_display_name` for the about-to-be-created instance
        // row.
        let display_for_title = if effective_tag.is_empty() {
            instance_name.clone()
        } else {
            format!("{}-{}", effective_tag, instance_name)
        };
        instance_env.insert(
            "HCOM_PANE_TITLE".to_string(),
            crate::shared::format_pane_title(
                crate::shared::ST_LISTENING,
                &display_for_title,
                tool_type,
            ),
        );

        // Pre-register instance
        if let Err(e) = (|| -> Result<()> {
            instance_binding::initialize_instance_in_position_file(
                db,
                &instance_name,
                params.prior_session_id.as_deref(),
                None,            // parent_session_id
                None,            // parent_name
                None,            // agent_id
                None,            // transcript_path
                Some(tool_type), // tool
                params.background,
                if effective_tag.is_empty() {
                    None
                } else {
                    Some(effective_tag.as_str())
                },
                None,              // wait_timeout
                None,              // subagent_timeout
                None,              // hints
                Some(working_dir), // cwd_override: use launch params cwd, not current_dir()
            );
            db.set_process_binding(&process_id, "", &instance_name)?;
            Ok(())
        })() {
            errors.push(json!({"tool": base_tool, "error": e.to_string()}));
            continue;
        }

        // Dispatch to tool-specific launcher
        let launch_result = (|| -> Result<bool> {
            match normalized {
                LaunchTool::Claude => {
                    let claude_cmd = build_claude_command(&params.args);

                    // Store launch_args
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );

                    // LaunchTool::Claude only resolves to NativePrint (background,
                    // direct spawn in print mode) or InteractiveVisible — the
                    // PTY-backed variants live in LaunchTool::ClaudePty below.
                    if matches!(backend, LaunchBackend::NativePrint) {
                        let log_filename = format!(
                            "background_{}_{}.log",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0),
                            rand::random::<u16>() % 9000 + 1000
                        );
                        instance_env.insert("HCOM_BACKGROUND".to_string(), log_filename.clone());

                        let (launch_result, effective_preset) = terminal::launch_terminal(
                            &claude_cmd,
                            &instance_env,
                            Some(working_dir),
                            true, // background
                            false,
                            terminal_mode,
                            inside_ai_tool,
                        )?;
                        match launch_result {
                            terminal::LaunchResult::Background(log_file, pid) => {
                                finalize_background_launch(
                                    &mut BackgroundLaunchCtx {
                                        db,
                                        tool: "claude",
                                        instance_name: &instance_name,
                                        process_id: &process_id,
                                        terminal_mode,
                                        tag: params.tag.as_deref().unwrap_or(""),
                                        working_dir,
                                        log_files: &mut log_files,
                                        handles: &mut handles,
                                    },
                                    log_file,
                                    pid,
                                    effective_preset,
                                );
                                Ok(true)
                            }
                            _ => Ok(false),
                        }
                    } else {
                        let effective_run_here = will_run_in_current_terminal(
                            params.count,
                            false,
                            params.run_here,
                            terminal_mode,
                            inside_ai_tool,
                        );
                        let (launch_result, effective_preset) = terminal::launch_terminal(
                            &claude_cmd,
                            &instance_env,
                            Some(working_dir),
                            false,
                            effective_run_here,
                            terminal_mode,
                            inside_ai_tool,
                        )?;
                        instance_binding::persist_terminal_launch_context(
                            db,
                            &instance_name,
                            terminal_mode,
                            &effective_preset,
                            Some(&process_id),
                        );

                        match launch_result {
                            terminal::LaunchResult::Success => {
                                handles.push(
                                    json!({"tool": "claude", "instance_name": instance_name}),
                                );
                                Ok(true)
                            }
                            _ => Ok(false),
                        }
                    }
                }

                LaunchTool::ClaudePty => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    // Same background/foreground split as gemini/codex/opencode:
                    // foreground → visible PTY in a terminal; background → PTY
                    // wrapper in a detached runner. The wrapper handles the TUI
                    // the same way either way, which is what lets PTY-headless
                    // claude keep a live session that accepts hcom inject.
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "claude",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }

                LaunchTool::Gemini => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "gemini",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }

                LaunchTool::Codex => {
                    // Bootstrap delivered via developer_instructions at launch
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([("name_announced".to_string(), json!(true))]),
                    );

                    // Build effective args: system_prompt + preprocessing
                    let mut effective_args = params.args.clone();
                    if let Some(ref sp) = params.system_prompt {
                        let mut pre =
                            vec!["-c".to_string(), format!("developer_instructions={}", sp)];
                        pre.extend(effective_args);
                        effective_args = pre;
                    }

                    // Generate bootstrap text for preprocessing
                    let bootstrap = crate::bootstrap::get_bootstrap(
                        db,
                        &paths::hcom_dir(),
                        &instance_name,
                        "codex",
                        params.background,
                        true, // is_launched
                        "",
                        &effective_tag,
                        hcom_config.relay_enabled,
                        None,
                    );

                    let sandbox_mode = instance_env
                        .get("HCOM_CODEX_SANDBOX_MODE")
                        .cloned()
                        .unwrap_or_else(|| "workspace".to_string());

                    effective_args = codex_preprocessing::preprocess_codex_args(
                        &effective_args,
                        &bootstrap,
                        &sandbox_mode,
                    );

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );

                    instance_env.insert("HCOM_CODEX_SANDBOX_MODE".to_string(), sandbox_mode);

                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "codex",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &effective_args,
                        &params,
                        inside_ai_tool,
                    )
                }

                LaunchTool::OpenCode | LaunchTool::Kilo | LaunchTool::Pi => {
                    opencode_preprocessing::preprocess_opencode_env(
                        &mut instance_env,
                        base_tool,
                        &instance_name,
                        hcom_config.auto_approve,
                    );

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );

                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: base_tool,
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }
                LaunchTool::Antigravity => {
                    // Antigravity ignores GEMINI_SYSTEM_MD; bootstrap is delivered via the
                    // SessionStart hook's inject_bootstrap_once (name_announced stays 0 here).
                    instance_env.insert("ANTIGRAVITY_AGENT".to_string(), "1".to_string());

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "antigravity",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }
                LaunchTool::Cursor => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "cursor",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }

                LaunchTool::Kimi => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "kimi",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }
                LaunchTool::Copilot => {
                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(&stored_launch_args),
                        )]),
                    );
                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "copilot",
                            instance_name: &instance_name,
                            process_id: &process_id,
                            terminal_mode,
                            tag: params.tag.as_deref().unwrap_or(""),
                            working_dir,
                            log_files: &mut log_files,
                            handles: &mut handles,
                        },
                        &mut instance_env,
                        &params.args,
                        &params,
                        inside_ai_tool,
                    )
                }
            }
        })();

        match launch_result {
            Ok(true) => launched += 1,
            Ok(false) => {
                cleanup_instance(db, &instance_name, &process_id);
            }
            Err(e) => {
                cleanup_instance(db, &instance_name, &process_id);
                errors.push(json!({"tool": base_tool, "error": e.to_string()}));
            }
        }
    }

    let failed = params.count - launched;
    if launched == 0 {
        if !errors.is_empty() {
            let details: Vec<String> = errors
                .iter()
                .filter_map(|e| {
                    e.get("error")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect();
            bail!(
                "No instances launched (0/{}): {}",
                params.count,
                details.join("; ")
            );
        }
        bail!("No instances launched (0/{})", params.count);
    }

    // Log batch launch event
    db.log_event(
        "life",
        &launcher_name,
        &json!({
            "action": "batch_launched",
            "by": &launcher_name,
            "batch_id": batch_id,
            // User-facing tool identity (`claude`, not the `claude-pty` launch
            // surface) — consistent with per-instance events and LaunchResult.
            "tool": base_tool,
            "count_requested": params.count,
            "launched": launched,
            "failed": failed,
            "background": params.background,
            "tag": effective_tag,
            "instances": handles
                .iter()
                .filter_map(|h| h.get("instance_name").and_then(|v| v.as_str()))
                .collect::<Vec<_>>(),
        }),
    )
    .ok();

    // Push launch event to relay (best-effort)
    let prefix = crate::runtime_env::get_hcom_prefix();
    if let Some((cmd, prefix_args)) = prefix.split_first() {
        let _ = std::process::Command::new(cmd)
            .args(prefix_args)
            .args(["relay", "push"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    Ok(LaunchResult {
        // User-facing identity. The PTY-vs-print backend distinction lives in
        // `LaunchBackend`, not the tool string, so consumers see `claude`.
        tool: base_tool.to_string(),
        batch_id,
        launched,
        failed,
        background: params.background,
        log_files,
        handles,
        errors,
    })
}

/// Validate tool args (pure parsing, no mutation).
pub(crate) fn validate_tool_args(tool: &LaunchTool, args: &[String]) -> Vec<String> {
    match tool {
        LaunchTool::Claude | LaunchTool::ClaudePty | LaunchTool::Codex => Vec::new(),
        LaunchTool::Gemini => {
            validate_rejected_args("Gemini", "hcom gemini", args, GEMINI_REJECTED_ARGS)
        }
        LaunchTool::Cursor => crate::tools::cursor_preprocessing::validate_cursor_args(args),
        LaunchTool::Kimi => validate_rejected_args("Kimi", "hcom kimi", args, KIMI_REJECTED_ARGS),
        LaunchTool::OpenCode => {
            validate_rejected_args("OpenCode", "hcom opencode", args, OPENCODE_REJECTED_ARGS)
        }
        LaunchTool::Kilo => validate_rejected_args("Kilo", "hcom kilo", args, KILO_REJECTED_ARGS),
        LaunchTool::Pi => validate_rejected_args("Pi", "hcom pi", args, PI_REJECTED_ARGS),
        LaunchTool::Antigravity => validate_rejected_args(
            "Antigravity",
            "hcom antigravity",
            args,
            ANTIGRAVITY_REJECTED_ARGS,
        ),
        LaunchTool::Copilot => crate::tools::copilot_preprocessing::validate_copilot_args(args),
    }
}

/// Clean up instance and process binding on failure.
fn cleanup_instance(db: &HcomDb, name: &str, process_id: &str) {
    db.delete_instance(name).ok();
    db.delete_process_binding(process_id).ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::collections::BTreeMap;

    struct EnvVarGuard {
        saved: BTreeMap<String, Option<String>>,
    }

    impl EnvVarGuard {
        fn clean_detection_env() -> Self {
            let mut keys: Vec<String> = [
                "CLAUDECODE",
                "CLAUDE_ENV_FILE",
                "ANTIGRAVITY_AGENT",
                "GEMINI_CLI",
                "CODEX_SANDBOX",
                "CODEX_SANDBOX_NETWORK_DISABLED",
                "CODEX_MANAGED_BY_NPM",
                "CODEX_MANAGED_BY_BUN",
                "CODEX_THREAD_ID",
                "OPENCODE",
                "KILO",
                "CURSOR_AGENT",
                "CURSOR_PROJECT_DIR",
                "KIMI_CODE_CLI",
                "KIMI_SESSION_ID",
                "HCOM_TOOL",
                "HCOM_LAUNCHED",
                "HCOM_PI",
                "CI",
                "GITHUB_ACTIONS",
                "CARGO_TEST_PARENT",
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            keys.extend(
                std::env::vars()
                    .map(|(key, _)| key)
                    .filter(|key| key.starts_with("CARGO_")),
            );
            Self::remove(keys)
        }

        fn remove<I>(keys: I) -> Self
        where
            I: IntoIterator<Item = String>,
        {
            let mut saved = BTreeMap::new();
            for key in keys {
                saved
                    .entry(key.clone())
                    .or_insert_with(|| std::env::var(&key).ok());
                unsafe { std::env::remove_var(key) };
            }
            Self { saved }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    fn test_launch_tool_from_str() {
        assert_eq!(LaunchTool::from_str("claude").unwrap(), LaunchTool::Claude);
        assert_eq!(
            LaunchTool::from_str("claude-pty").unwrap(),
            LaunchTool::ClaudePty
        );
        assert_eq!(LaunchTool::from_str("gemini").unwrap(), LaunchTool::Gemini);
        assert_eq!(LaunchTool::from_str("codex").unwrap(), LaunchTool::Codex);
        assert_eq!(
            LaunchTool::from_str("opencode").unwrap(),
            LaunchTool::OpenCode
        );
        assert_eq!(LaunchTool::from_str("kilo").unwrap(), LaunchTool::Kilo);
        assert_eq!(LaunchTool::from_str("kilocode").unwrap(), LaunchTool::Kilo);
        assert_eq!(
            LaunchTool::from_str("antigravity").unwrap(),
            LaunchTool::Antigravity
        );
        assert_eq!(
            LaunchTool::from_str("agy").unwrap(),
            LaunchTool::Antigravity
        );
        assert_eq!(
            LaunchTool::from_str("copilot").unwrap(),
            LaunchTool::Copilot
        );
        assert!(LaunchTool::from_str("unknown").is_err());
    }

    #[test]
    fn launch_count_uses_per_tool_spec_limit() {
        assert!(validate_launch_count(&LaunchTool::Kimi, 10).is_ok());
        let err = validate_launch_count(&LaunchTool::Kimi, 11).unwrap_err();
        assert!(err.to_string().contains("max 10"));

        assert!(validate_launch_count(&LaunchTool::Claude, 100).is_ok());
        let err = validate_launch_count(&LaunchTool::Claude, 101).unwrap_err();
        assert!(err.to_string().contains("max 100"));
    }

    #[test]
    fn unsupported_initial_prompt_is_spec_driven() {
        let mut args = Vec::new();
        let err =
            append_initial_prompt_args(&LaunchTool::Kimi, &mut args, "task".into()).unwrap_err();
        assert!(
            err.to_string()
                .contains("kimi does not support an initial prompt")
        );
        assert!(args.is_empty());

        append_initial_prompt_args(&LaunchTool::Gemini, &mut args, "task".into()).unwrap();
        assert_eq!(args, vec!["task"]);
    }

    #[test]
    fn initial_prompt_flag_shape_appends_after_native_prompt() {
        let mut args = vec!["--prompt".to_string(), "native prompt".to_string()];
        append_initial_prompt_args(&LaunchTool::OpenCode, &mut args, "hcom prompt".into()).unwrap();
        assert_eq!(
            args,
            vec!["--prompt", "native prompt", "--prompt", "hcom prompt"]
        );
    }

    #[test]
    fn initial_prompt_positional_shape_appends_after_native_prompt() {
        let mut args = vec!["native prompt".to_string()];
        append_initial_prompt_args(&LaunchTool::Gemini, &mut args, "hcom prompt".into()).unwrap();
        assert_eq!(args, vec!["native prompt", "hcom prompt"]);
    }

    #[test]
    fn initial_prompt_dash_dash_shape_appends_after_native_prompt() {
        let mut args = vec!["--".to_string(), "native prompt".to_string()];
        append_initial_prompt_args(&LaunchTool::Claude, &mut args, "hcom prompt".into()).unwrap();
        assert_eq!(args, vec!["--", "native prompt", "--", "hcom prompt"]);
    }

    #[test]
    fn hcom_prompt_alone_is_injected_for_every_shape() {
        for (tool, expected) in [
            (
                LaunchTool::OpenCode,
                vec!["--prompt".to_string(), "hcom".to_string()],
            ),
            (LaunchTool::Gemini, vec!["hcom".to_string()]),
            (
                LaunchTool::Claude,
                vec!["--".to_string(), "hcom".to_string()],
            ),
        ] {
            let mut args = Vec::new();
            append_initial_prompt_args(&tool, &mut args, "hcom".into()).unwrap();
            assert_eq!(args, expected);
        }
    }

    #[test]
    fn positional_shape_does_not_treat_model_value_as_prompt() {
        let mut args = vec!["--model".to_string(), "safe-model".to_string()];
        append_initial_prompt_args(&LaunchTool::Gemini, &mut args, "hcom".into()).unwrap();
        assert_eq!(args.last().map(String::as_str), Some("hcom"));
    }

    #[test]
    fn validate_cursor_print_mode_fails_fast() {
        let errors = validate_tool_args(&LaunchTool::Cursor, &["--print".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("not supported"));
    }

    #[test]
    fn validate_kimi_rejects_prompt_mode_but_allows_resume() {
        let errors = validate_tool_args(&LaunchTool::Kimi, &["--prompt".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("--prompt"));
        assert!(validate_tool_args(&LaunchTool::Kimi, &["--session".to_string()]).is_empty());
    }

    #[test]
    fn validate_opencode_rejects_run_but_allows_session() {
        let errors = validate_tool_args(&LaunchTool::OpenCode, &["run".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("run"));
        assert!(validate_tool_args(&LaunchTool::OpenCode, &["--session".to_string()]).is_empty());
        assert!(validate_tool_args(&LaunchTool::OpenCode, &["--prompt".to_string()]).is_empty());
    }

    #[test]
    fn validate_kilo_rejects_serve_but_allows_continue() {
        let errors = validate_tool_args(&LaunchTool::Kilo, &["serve".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("serve"));
        assert!(validate_tool_args(&LaunchTool::Kilo, &["--continue".to_string()]).is_empty());
    }

    #[test]
    fn validate_pi_rejects_print_but_allows_fork() {
        let errors = validate_tool_args(&LaunchTool::Pi, &["--print".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("--print"));
        assert!(validate_tool_args(&LaunchTool::Pi, &["--fork".to_string()]).is_empty());
    }

    #[test]
    fn validate_antigravity_rejects_print_alias_but_allows_conversation() {
        let errors = validate_tool_args(&LaunchTool::Antigravity, &["--prompt".to_string()]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("--prompt"));
        assert!(
            validate_tool_args(&LaunchTool::Antigravity, &["--conversation".to_string()])
                .is_empty()
        );
        assert!(
            validate_tool_args(
                &LaunchTool::Antigravity,
                &["--prompt-interactive".to_string()]
            )
            .is_empty()
        );
    }

    #[test]
    fn test_launch_tool_as_str() {
        assert_eq!(LaunchTool::Claude.as_str(), "claude");
        assert_eq!(LaunchTool::ClaudePty.as_str(), "claude-pty");
        assert_eq!(LaunchTool::Gemini.as_str(), "gemini");
        assert_eq!(LaunchTool::Antigravity.as_str(), "antigravity");
        assert_eq!(LaunchTool::Copilot.as_str(), "copilot");
    }

    #[test]
    fn test_launch_tool_base_tool() {
        assert_eq!(LaunchTool::Claude.base_tool(), "claude");
        assert_eq!(LaunchTool::ClaudePty.base_tool(), "claude");
        assert_eq!(LaunchTool::Codex.base_tool(), "codex");
        assert_eq!(LaunchTool::Antigravity.base_tool(), "antigravity");
        assert_eq!(LaunchTool::Copilot.base_tool(), "copilot");
    }

    #[test]
    fn test_launch_tool_uses_pty() {
        assert!(!LaunchTool::Claude.uses_pty());
        assert!(LaunchTool::ClaudePty.uses_pty());
        assert!(LaunchTool::Gemini.uses_pty());
        assert!(LaunchTool::Codex.uses_pty());
        assert!(LaunchTool::OpenCode.uses_pty());
        assert!(LaunchTool::Kilo.uses_pty());
        assert!(LaunchTool::Copilot.uses_pty());
    }

    #[test]
    fn test_launch_backend_resolve_interactive() {
        // Any tool, !background → InteractiveVisible (visible terminal).
        for tool in [
            LaunchTool::Claude,
            LaunchTool::ClaudePty,
            LaunchTool::Gemini,
            LaunchTool::Codex,
            LaunchTool::OpenCode,
            LaunchTool::Kilo,
            LaunchTool::Antigravity,
            LaunchTool::Copilot,
        ] {
            assert_eq!(
                LaunchBackend::resolve(&tool, false),
                LaunchBackend::InteractiveVisible,
                "{:?} should resolve to InteractiveVisible without background",
                tool
            );
        }
    }

    #[test]
    fn test_launch_backend_resolve_claude_native_print() {
        // claude `-p`/`--print` surface (`Claude`) + background → NativePrint
        // (detached -p stream-json). Only chosen when the caller passes -p.
        assert_eq!(
            LaunchBackend::resolve(&LaunchTool::Claude, true),
            LaunchBackend::NativePrint
        );
    }

    #[test]
    fn test_launch_backend_resolve_claude_pty_headless() {
        // claude --headless default surface (`ClaudePty`) → HeadlessPty
        // (PTY wrapper, live TUI).
        assert_eq!(
            LaunchBackend::resolve(&LaunchTool::ClaudePty, true),
            LaunchBackend::HeadlessPty
        );
    }

    #[test]
    fn test_launch_backend_resolve_other_tools_headless() {
        // gemini/codex/opencode + --headless → HeadlessPty (unchanged from today).
        for tool in [
            LaunchTool::Gemini,
            LaunchTool::Codex,
            LaunchTool::OpenCode,
            LaunchTool::Kilo,
            LaunchTool::Antigravity,
            LaunchTool::Copilot,
        ] {
            assert_eq!(
                LaunchBackend::resolve(&tool, true),
                LaunchBackend::HeadlessPty,
                "{:?} --headless should be HeadlessPty",
                tool
            );
        }
    }

    #[test]
    fn test_will_run_in_current_terminal() {
        // Explicit override
        assert!(will_run_in_current_terminal(
            5,
            false,
            Some(true),
            None,
            false
        ));
        assert!(!will_run_in_current_terminal(
            1,
            false,
            Some(false),
            None,
            false
        ));

        // terminal=here
        assert!(will_run_in_current_terminal(
            5,
            false,
            None,
            Some("here"),
            false
        ));

        // Inside AI tool → always new window
        assert!(!will_run_in_current_terminal(1, false, None, None, true));

        // Background → never run here
        assert!(!will_run_in_current_terminal(1, true, None, None, false));

        // Single → run here, multiple → new window
        assert!(will_run_in_current_terminal(1, false, None, None, false));
        assert!(!will_run_in_current_terminal(2, false, None, None, false));
    }

    #[test]
    fn test_build_claude_command() {
        let args = vec!["--model".to_string(), "sonnet".to_string()];
        let cmd = build_claude_command(&args);
        assert_eq!(cmd, "claude --model sonnet");
    }

    #[test]
    fn test_build_claude_command_with_spaces() {
        let args = vec!["--prompt".to_string(), "fix all tests".to_string()];
        let cmd = build_claude_command(&args);
        assert!(cmd.contains("'fix all tests'"));
    }

    #[test]
    fn test_background_runner_env_includes_instance_name() {
        let mut env = HashMap::new();
        env.insert("HCOM_PROCESS_ID".to_string(), "pid-123".to_string());

        let runner_env = background_runner_env("codex", &env, "nita");

        assert_eq!(
            runner_env.get("HCOM_INSTANCE_NAME").map(String::as_str),
            Some("nita")
        );
        assert_eq!(
            runner_env.get("HCOM_PROCESS_ID").map(String::as_str),
            Some("pid-123")
        );
        assert!(!runner_env.contains_key("HCOM_PTY_MODE"));
    }

    #[test]
    fn test_background_runner_env_includes_claude_pty_mode() {
        let env = HashMap::new();

        let runner_env = background_runner_env("claude", &env, "hone");

        assert_eq!(
            runner_env.get("HCOM_INSTANCE_NAME").map(String::as_str),
            Some("hone")
        );
        assert_eq!(
            runner_env.get("HCOM_PTY_MODE").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn test_background_runner_env_antigravity_sets_agent() {
        let runner_env = background_runner_env("antigravity", &HashMap::new(), "nabe");
        assert_eq!(
            runner_env.get("ANTIGRAVITY_AGENT").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn test_env_strip_set_strips_closed_categories() {
        let strip = env_strip_set();
        // HCOM identity
        assert!(strip.contains("HCOM_PROCESS_ID"));
        assert!(strip.contains("HCOM_LAUNCHED"));
        // Tool markers
        assert!(strip.contains("CLAUDECODE"));
        assert!(strip.contains("CLAUDE_ENV_FILE"));
        assert!(strip.contains("CODEX_SANDBOX"));
        assert!(strip.contains("CODEX_THREAD_ID"));
        assert!(strip.contains("GEMINI_SYSTEM_MD"));
        assert!(strip.contains("HCOM_TOOL"));
        assert!(strip.contains("HCOM_PI"));
        assert!(!strip.contains("PI_CODING_AGENT_DIR"));
        // Terminal context
        assert!(strip.contains("KITTY_WINDOW_ID"));
        assert!(strip.contains("TMUX_PANE"));
        assert!(!strip.contains("COLORTERM"));
        assert!(!strip.contains("TERM"));
    }

    #[test]
    #[serial]
    fn test_contaminated_parent_detection() {
        let _guard = EnvVarGuard::clean_detection_env();
        assert!(!contaminated_parent());

        unsafe { std::env::set_var("CLAUDECODE", "1") };
        assert!(contaminated_parent());
        unsafe { std::env::remove_var("CLAUDECODE") };

        unsafe { std::env::set_var("CODEX_THREAD_ID", "thread") };
        assert!(contaminated_parent());
        unsafe { std::env::remove_var("CODEX_THREAD_ID") };

        unsafe { std::env::set_var("CI", "1") };
        assert!(contaminated_parent());
        unsafe { std::env::remove_var("CI") };

        unsafe { std::env::set_var("CARGO_TEST_PARENT", "1") };
        assert!(contaminated_parent());
    }

    #[test]
    #[serial]
    fn test_build_launch_env_inherits_parent_env() {
        unsafe { std::env::set_var("RORI_TEST_MY_VAR", "hello") }
        unsafe { std::env::set_var("RORI_TEST_OPENROUTER_API_KEY", "sk-test-123") }
        unsafe { std::env::set_var("RORI_TEST_PI_OFFLINE", "1") }

        let config = crate::config::HcomConfig::default();
        let env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        assert_eq!(
            env.get("RORI_TEST_MY_VAR").map(String::as_str),
            Some("hello")
        );
        assert_eq!(
            env.get("RORI_TEST_OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-test-123")
        );
        assert_eq!(
            env.get("RORI_TEST_PI_OFFLINE").map(String::as_str),
            Some("1")
        );

        unsafe { std::env::remove_var("RORI_TEST_MY_VAR") }
        unsafe { std::env::remove_var("RORI_TEST_OPENROUTER_API_KEY") }
        unsafe { std::env::remove_var("RORI_TEST_PI_OFFLINE") }
    }

    #[test]
    #[serial]
    fn test_build_launch_env_agent_regime_uses_resolved_shell_base() {
        let _guard = EnvVarGuard::remove(vec![
            "RORI_PARENT_CONTAMINATION".to_string(),
            "RORI_RESOLVED_AUTH".to_string(),
        ]);
        unsafe { std::env::set_var("RORI_PARENT_CONTAMINATION", "leak") };

        let config = crate::config::HcomConfig::default();
        let env =
            build_launch_env_with_resolver(&config, LaunchEnvRegime::ContaminatedParent, || {
                Some(HashMap::from([(
                    "RORI_RESOLVED_AUTH".to_string(),
                    "auth-token".to_string(),
                )]))
            });

        assert!(!env.contains_key("RORI_PARENT_CONTAMINATION"));
        assert_eq!(
            env.get("RORI_RESOLVED_AUTH").map(String::as_str),
            Some("auth-token")
        );
    }

    #[test]
    #[serial]
    fn test_build_launch_env_strips_closed_categories() {
        unsafe { std::env::set_var("HCOM_PROCESS_ID", "pid-stale") }
        unsafe { std::env::set_var("CLAUDECODE", "1") }
        unsafe { std::env::set_var("CODEX_THREAD_ID", "thread-stale") }
        unsafe { std::env::set_var("PI_CODING_AGENT_DIR", "/tmp/pi-config") }
        unsafe { std::env::set_var("KITTY_WINDOW_ID", "1337") }

        let config = crate::config::HcomConfig::default();
        let env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        assert!(!env.contains_key("HCOM_PROCESS_ID"));
        assert!(!env.contains_key("CLAUDECODE"));
        assert!(!env.contains_key("CODEX_THREAD_ID"));
        assert_eq!(
            env.get("PI_CODING_AGENT_DIR").map(String::as_str),
            Some("/tmp/pi-config")
        );
        assert!(!env.contains_key("KITTY_WINDOW_ID"));

        unsafe { std::env::remove_var("HCOM_PROCESS_ID") }
        unsafe { std::env::remove_var("CLAUDECODE") }
        unsafe { std::env::remove_var("CODEX_THREAD_ID") }
        unsafe { std::env::remove_var("PI_CODING_AGENT_DIR") }
        unsafe { std::env::remove_var("KITTY_WINDOW_ID") }
    }

    #[test]
    #[serial]
    fn test_build_launch_env_run_here_inherits_terminal_vars() {
        let _guard =
            EnvVarGuard::remove(vec!["NO_COLOR".to_string(), "HCOM_PROCESS_ID".to_string()]);
        unsafe { std::env::set_var("NO_COLOR", "1") };
        unsafe { std::env::set_var("HCOM_PROCESS_ID", "pid-stale") };

        let config = crate::config::HcomConfig::default();
        let env = build_launch_env_with_resolver(&config, LaunchEnvRegime::RunHere, || {
            panic!("run_here must not resolve shell env")
        });

        assert_eq!(env.get("NO_COLOR").map(String::as_str), Some("1"));
        assert!(!env.contains_key("HCOM_PROCESS_ID"));
    }

    #[test]
    #[serial]
    fn test_build_launch_env_fail_open_to_parent_env() {
        let _guard = EnvVarGuard::remove(vec!["RORI_FAIL_OPEN_PARENT".to_string()]);
        unsafe { std::env::set_var("RORI_FAIL_OPEN_PARENT", "present") };

        let config = crate::config::HcomConfig::default();
        let env =
            build_launch_env_with_resolver(&config, LaunchEnvRegime::ContaminatedParent, || None);

        assert_eq!(
            env.get("RORI_FAIL_OPEN_PARENT").map(String::as_str),
            Some("present")
        );
    }

    #[test]
    #[serial]
    fn test_build_launch_env_config_overrides_ambient() {
        unsafe { std::env::set_var("HCOM_TAG", "ambient-tag") }

        let config = crate::config::HcomConfig {
            tag: "config-tag".to_string(),
            ..Default::default()
        };
        let env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        assert_eq!(env.get("HCOM_TAG").map(String::as_str), Some("config-tag"));

        unsafe { std::env::remove_var("HCOM_TAG") }
    }

    #[test]
    #[serial]
    fn test_background_runner_env_uses_upstream_resolved_base() {
        let _guard = EnvVarGuard::remove(vec!["RORI_BACKGROUND_CONTAMINATION".to_string()]);
        unsafe { std::env::set_var("RORI_BACKGROUND_CONTAMINATION", "leak") };

        let config = crate::config::HcomConfig::default();
        let env =
            build_launch_env_with_resolver(&config, LaunchEnvRegime::ContaminatedParent, || {
                Some(HashMap::from([(
                    "RORI_BACKGROUND_AUTH".to_string(),
                    "auth-token".to_string(),
                )]))
            });
        let runner_env = background_runner_env("codex", &env, "nita");

        assert!(!runner_env.contains_key("RORI_BACKGROUND_CONTAMINATION"));
        assert_eq!(
            runner_env.get("RORI_BACKGROUND_AUTH").map(String::as_str),
            Some("auth-token")
        );
    }

    #[test]
    fn test_runner_script_strips_instance_state_vars() {
        let env = HashMap::from([
            ("GEMINI_PTY_INFO".to_string(), "child_process".to_string()),
            ("GEMINI_API_KEY".to_string(), "gem-key".to_string()),
            ("GEMINI_CLI".to_string(), "1".to_string()),
            ("TERM".to_string(), "dumb".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
            ("NO_COLOR".to_string(), "1".to_string()),
            ("FORCE_COLOR".to_string(), "1".to_string()),
            ("RORI_MY_VAR".to_string(), "myval".to_string()),
        ]);

        let script = create_runner_script("gemini", "/tmp", "test", &env, &[], false).unwrap();

        let content = std::fs::read_to_string(&script).unwrap();
        // Instance-state stripped from unset block
        assert!(
            content.contains("GEMINI_PTY_INFO"),
            "GEMINI_PTY_INFO should appear in unset"
        );
        let env_file = content
            .lines()
            .find_map(|line| line.trim().strip_prefix(". "))
            .map(|path| path.trim_matches('\'').to_string())
            .expect("runner script should source a sidecar env file");
        let sidecar = std::fs::read_to_string(&env_file).unwrap();
        assert!(!sidecar.contains("GEMINI_PTY_INFO"));
        assert!(!sidecar.contains("GEMINI_CLI"));
        assert!(!sidecar.contains("TERM="));
        assert!(!sidecar.contains("COLORTERM="));
        assert!(!sidecar.contains("NO_COLOR="));
        assert!(!sidecar.contains("FORCE_COLOR="));
        assert!(sidecar.contains("GEMINI_API_KEY"));
        assert!(sidecar.contains("RORI_MY_VAR"));

        std::fs::remove_file(&script).ok();
        std::fs::remove_file(env_file).ok();
    }

    #[test]
    #[serial]
    fn test_same_tool_nesting_strips_instance_state() {
        unsafe { std::env::set_var("GEMINI_PTY_INFO", "child_process") }
        unsafe { std::env::set_var("GEMINI_API_KEY", "parent-key") }

        let config = crate::config::HcomConfig::default();
        let mut env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        let gemini_spec: &'static crate::integration_spec::IntegrationSpec =
            crate::tool::Tool::Gemini.spec();
        for var in gemini_spec.instance_state_env {
            env.remove(*var);
        }

        assert!(!env.contains_key("GEMINI_PTY_INFO"));
        assert_eq!(
            env.get("GEMINI_API_KEY").map(String::as_str),
            Some("parent-key")
        );

        unsafe { std::env::remove_var("GEMINI_PTY_INFO") }
        unsafe { std::env::remove_var("GEMINI_API_KEY") }
    }

    #[test]
    #[serial]
    fn test_cross_tool_nesting_forwards_auth() {
        unsafe { std::env::set_var("OPENROUTER_API_KEY", "sk-parent") }

        let config = crate::config::HcomConfig::default();
        let env = build_launch_env(&config, LaunchEnvRegime::HumanShell);

        assert_eq!(
            env.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-parent")
        );

        unsafe { std::env::remove_var("OPENROUTER_API_KEY") }
    }

    fn launcher_test_db() -> crate::db::HcomDb {
        let db = crate::db::HcomDb::open_raw(std::path::Path::new(":memory:")).unwrap();
        db.init_db().unwrap();
        db
    }

    fn insert_test_instance(db: &crate::db::HcomDb, name: &str, status: &str) {
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, created_at, tool) VALUES (?1, ?2, ?3, 'antigravity')",
                rusqlite::params![name, status, now],
            )
            .unwrap();
    }

    #[test]
    fn resolve_explicit_name_conflict_allows_free_name() {
        let db = launcher_test_db();
        assert!(resolve_explicit_name_conflict(&db, "luna").is_ok());
    }

    #[test]
    fn resolve_explicit_name_conflict_consumes_inactive_resume_row() {
        let db = launcher_test_db();
        insert_test_instance(&db, "zeno", "inactive");
        assert!(resolve_explicit_name_conflict(&db, "zeno").is_ok());
        // Row must be gone so the launcher can create a fresh row with the same name.
        assert!(db.get_instance("zeno").unwrap().is_none());
    }

    #[test]
    fn resolve_explicit_name_conflict_allows_pending_placeholder() {
        // A pending placeholder is the fork/resume path's own reservation
        // (reserve_generated_name). It must pass through so the launcher's
        // pre-register step can promote it — bailing here broke `hcom f`.
        let db = launcher_test_db();
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, status_context, created_at, tool) \
                 VALUES (?1, ?2, ?3, ?4, 'claude')",
                rusqlite::params![
                    "milo",
                    instance_names::PLACEHOLDER_STATUS,
                    instance_names::PLACEHOLDER_CONTEXT,
                    now
                ],
            )
            .unwrap();
        assert!(resolve_explicit_name_conflict(&db, "milo").is_ok());
        // Row must survive — the launcher promotes it in place.
        assert!(db.get_instance("milo").unwrap().is_some());
    }

    #[test]
    fn resolve_explicit_name_conflict_rejects_active_row() {
        let db = launcher_test_db();
        insert_test_instance(&db, "rune", "listening");
        let err = resolve_explicit_name_conflict(&db, "rune")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "unexpected: {err}");
        // Row should still be present — no deletion on conflict.
        assert!(db.get_instance("rune").unwrap().is_some());
    }

    // ── inject_workspace_trust_args ──────────────────────────────────────────

    #[test]
    fn test_auto_trust_workspace_default_true() {
        assert!(crate::config::HcomConfig::default().auto_trust_workspace);
    }

    #[test]
    fn test_gemini_flag_on_injects_skip_trust() {
        let dir = std::path::Path::new("/some/workspace");
        let mut args = vec!["--model".to_string(), "gemini-2.5-flash".to_string()];
        inject_workspace_trust_args(&LaunchTool::Gemini, dir, &mut args, true);
        assert!(args.contains(&"--skip-trust".to_string()));
    }

    #[test]
    fn test_gemini_flag_off_no_injection() {
        let dir = std::path::Path::new("/some/workspace");
        let mut args = vec!["--model".to_string(), "gemini-2.5-flash".to_string()];
        inject_workspace_trust_args(&LaunchTool::Gemini, dir, &mut args, false);
        assert!(!args.contains(&"--skip-trust".to_string()));
    }

    #[test]
    fn test_gemini_inject_idempotent_when_present() {
        let dir = std::path::Path::new("/some/workspace");
        let mut args = vec![
            "--skip-trust".to_string(),
            "--model".to_string(),
            "x".to_string(),
        ];
        inject_workspace_trust_args(&LaunchTool::Gemini, dir, &mut args, true);
        assert_eq!(
            args.iter().filter(|a| a.as_str() == "--skip-trust").count(),
            1
        );
    }

    #[test]
    fn test_codex_flag_on_injects_trust_level() {
        let dir = std::path::Path::new("/my/project");
        let mut args = vec!["--model".to_string(), "o4-mini".to_string()];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, true);
        let c_idx = args
            .iter()
            .position(|a| a == "-c")
            .expect("-c not injected");
        let val = &args[c_idx + 1];
        assert!(val.contains("/my/project"), "path missing: {val}");
        assert!(val.contains("trust_level"), "trust_level missing: {val}");
        assert!(val.contains("\"trusted\""), "trusted value missing: {val}");
    }

    #[test]
    fn test_codex_flag_off_no_injection() {
        let dir = std::path::Path::new("/my/project");
        let mut args = vec!["--model".to_string(), "o4-mini".to_string()];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, false);
        assert!(!args.iter().any(|a| a == "-c"));
    }

    #[test]
    fn test_codex_inject_idempotent_when_present() {
        let dir = std::path::Path::new("/my/project");
        // Any -c value containing "trust_level" suppresses re-injection.
        let existing = r#"projects={ "/my/project" = { trust_level = "trusted" } }"#.to_string();
        let mut args = vec!["-c".to_string(), existing];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, true);
        assert_eq!(args.iter().filter(|a| a.as_str() == "-c").count(), 1);
    }

    #[test]
    fn test_codex_dotted_path_encoded_as_single_key() {
        // A path like /Users/x/proj.v2 has a dot in a component. The inline-table
        // format keeps the whole path as one TOML quoted key — no dot-splitting.
        let dir = std::path::Path::new("/Users/x/proj.v2");
        let mut args: Vec<String> = vec![];
        inject_workspace_trust_args(&LaunchTool::Codex, dir, &mut args, true);
        let c_idx = args
            .iter()
            .position(|a| a == "-c")
            .expect("-c not injected");
        let val = &args[c_idx + 1];
        // The full dotted path must survive intact as one quoted string.
        assert!(
            val.contains("\"/Users/x/proj.v2\""),
            "full dotted path must be a single quoted key: {val}"
        );
        assert!(val.contains("trust_level"), "trust_level missing: {val}");
    }

    #[test]
    fn test_non_trust_tools_unaffected() {
        let dir = std::path::Path::new("/some/workspace");
        for tool in &[
            LaunchTool::Claude,
            LaunchTool::ClaudePty,
            LaunchTool::OpenCode,
        ] {
            let mut args = vec!["--model".to_string(), "x".to_string()];
            inject_workspace_trust_args(tool, dir, &mut args, true);
            assert_eq!(
                args,
                vec!["--model".to_string(), "x".to_string()],
                "{tool:?} args should be unchanged"
            );
        }
    }
}
