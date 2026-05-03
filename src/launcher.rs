//! Unified launcher for Claude, Gemini, Codex, and OpenCode.
//!
//!
//! Provides a single entry point for launching all supported AI tools
//! with consistent batch tracking, environment setup, and error handling.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
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
use crate::shared::constants::{HCOM_IDENTITY_VARS, TOOL_MARKER_VARS};
use crate::terminal;
use crate::tools::{codex_preprocessing, cline_preprocessing, kilo_preprocessing, opencode_preprocessing};

/// Canonical tool types for launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchTool {
    Claude,
    ClaudePty,
    Gemini,
    Codex,
    OpenCode,
    Kilo,
    Cline,
}

impl LaunchTool {
    pub fn from_str(s: &str, pty: bool) -> Result<Self> {
        match s {
            "claude" if pty => Ok(LaunchTool::ClaudePty),
            "claude" => Ok(LaunchTool::Claude),
            "claude-pty" => Ok(LaunchTool::ClaudePty),
            "gemini" => Ok(LaunchTool::Gemini),
            "codex" => Ok(LaunchTool::Codex),
            "opencode" => Ok(LaunchTool::OpenCode),
            "kilocode" | "kilo" => Ok(LaunchTool::Kilo),
            "cline" => Ok(LaunchTool::Cline),
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
            LaunchTool::Cline => "cline",
        }
    }

    /// Base tool name (without -pty suffix).
    pub fn base_tool(&self) -> &'static str {
        match self {
            LaunchTool::Claude | LaunchTool::ClaudePty => "claude",
            LaunchTool::Gemini => "gemini",
            LaunchTool::Codex => "codex",
            LaunchTool::OpenCode => "opencode",
            LaunchTool::Kilo => "kilo",
            LaunchTool::Cline => "cline",
        }
    }

    /// Whether this tool uses the PTY wrapper.
    pub fn uses_pty(&self) -> bool {
        !matches!(self, LaunchTool::Claude)
    }
}

/// How the child process is hosted. Computed from (tool, background, pty) at
/// launch time so dispatch doesn't have to re-derive the combination.
///
/// - `InteractiveVisible`: foreground, user-visible terminal. All tools.
/// - `HeadlessPty`:       background, PTY wrapper in a detached runner. Default
///   for gemini/codex/opencode; claude with `--pty`.
/// - `NativePrint`:       background, direct claude spawn in print mode
///   (`-p --output-format stream-json --verbose`). Claude
///   only; one-shot, exits after the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchBackend {
    InteractiveVisible,
    HeadlessPty,
    NativePrint,
}

impl LaunchBackend {
    /// Resolve from the already-prepared (tool, background, pty) triple.
    ///
    /// `pty` here is the effective use-pty decision coming out of
    /// `prepare_launch_execution` — for claude, that tracks the user's `--pty`
    /// opt-in (plus the existing interactive default). For other tools it is
    /// always true.
    pub fn resolve(tool: &LaunchTool, background: bool, pty: bool) -> Self {
        if !background {
            return LaunchBackend::InteractiveVisible;
        }
        match tool {
            LaunchTool::Claude if !pty => LaunchBackend::NativePrint,
            LaunchTool::Claude | LaunchTool::ClaudePty => LaunchBackend::HeadlessPty,
            LaunchTool::Gemini | LaunchTool::Codex | LaunchTool::OpenCode | LaunchTool::Kilo | LaunchTool::Cline => {
                LaunchBackend::HeadlessPty
            }
        }
    }
}

/// Launch parameters.
#[derive(Clone)]
pub struct LaunchParams {
    pub tool: String,
    pub count: usize,
    pub args: Vec<String>,
    pub tag: Option<String>,
    pub project: Option<String>,
    pub system_prompt: Option<String>,
    pub initial_prompt: Option<String>,
    pub pty: bool,
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
            tag: None,
            project: None,
            system_prompt: None,
            initial_prompt: None,
            pty: false,
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

/// Build base environment from config.toml + env extras.
pub fn build_launch_env(hcom_config: &HcomConfig) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();

    // HCOM_* settings from config.toml
    for (key, value) in hcom_config.to_env_dict() {
        if !value.is_empty() {
            env.insert(key, value);
        }
    }

    // Passthrough vars from env file
    let env_path = paths::hcom_path(&["env"]);
    for (key, value) in config::load_env_extras(&env_path) {
        if !value.is_empty() {
            env.insert(key, value);
        }
    }

    env
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
    if let Ok(existing) = fs::read_to_string(&filepath) {
        if existing == system_prompt {
            return filepath.to_string_lossy().to_string();
        }
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
    let tool_env_var = match tool {
        LaunchTool::Claude | LaunchTool::ClaudePty => Some("CLAUDE_CONFIG_DIR"),
        LaunchTool::Gemini => Some("GEMINI_CLI_HOME"),
        LaunchTool::Codex => Some("CODEX_HOME"),
        LaunchTool::OpenCode => None,
        LaunchTool::Kilo => None,
        LaunchTool::Cline => None,
    };
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
fn ensure_hooks_installed(tool: &LaunchTool) -> Result<()> {
    let include_permissions = true;
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
            if crate::hooks::codex::verify_codex_hooks_installed(include_permissions) {
                return Ok(());
            }
            if let Err(e) = crate::hooks::codex::try_setup_codex_hooks(include_permissions) {
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
            Ok(())
        }
        LaunchTool::OpenCode => {
            if crate::hooks::opencode::ensure_plugin_installed() {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup OpenCode plugin. Run: hcom hooks add opencode\n{diag}");
        }
        LaunchTool::Kilo => {
            if crate::hooks::kilo::ensure_plugin_installed() {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup Kilo plugin. Run: hcom hooks add kilo\n{diag}");
        }
        LaunchTool::Cline => {
            if crate::hooks::cline::ensure_plugin_installed() {
                return Ok(());
            }
            let diag = install_diag_context(tool, &[]);
            bail!("Failed to setup Cline plugin. Run: hcom hooks add cline\n{diag}");
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
    m
}

fn background_runner_env(
    tool: &str,
    env: &HashMap<String, String>,
    instance_name: &str,
) -> HashMap<String, String> {
    let mut runner_env = env.clone();
    runner_env.insert("HCOM_INSTANCE_NAME".to_string(), instance_name.to_string());
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

    let env_block = terminal::build_env_string(env, "bash_export");
    let tool_args_str: String = tool_args
        .iter()
        .map(|a| crate::tools::args_common::shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");

    // Resolve binary paths for minimal PATH environments
    let mut path_dirs: Vec<String> = Vec::new();

    // Dev mode: prepend the worktree's Cargo output dir
    if let Ok(dev_root) = std::env::var("HCOM_DEV_ROOT") {
        if let Some(bin) = crate::shared::dev_root_binary(Path::new(&dev_root)) {
            if let Some(dir) = bin.parent() {
                path_dirs.push(dir.to_string_lossy().into_owned());
            }
        }
    }

    for bin_name in &[tool, "hcom", "python3", "node"] {
        if let Some(bin_path) = terminal::which_bin(bin_name) {
            if let Some(dir) = Path::new(&bin_path).parent() {
                let d = dir.to_string_lossy().to_string();
                if !path_dirs.contains(&d) {
                    path_dirs.push(d);
                }
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
        TOOL_MARKER_VARS.join(" "),
        HCOM_IDENTITY_VARS.join(" "),
        env_block,
        path_export,
        use_exec,
        crate::tools::args_common::shell_quote(&native_bin_str),
        tool,
        tool_args_str,
    );

    fs::write(&script_file, &content)?;
    fs::set_permissions(&script_file, fs::Permissions::from_mode(0o755))?;

    crate::log::log_info(
        "pty",
        "native.script",
        &format!(
            "script={} tool={} instance={}",
            script_file.display(),
            tool,
            instance_name
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
    runner_env.extend(tool_extra_env(tool));

    let script_file =
        create_runner_script(tool, cwd, instance_name, &runner_env, tool_args, run_here)?;

    let command = format!(
        "bash {}",
        crate::tools::args_common::shell_quote(&script_file)
    );

    let (launch_result, effective_preset) = terminal::launch_terminal(
        &command,
        env,
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
        &serde_json::Map::from_iter([("pid".to_string(), json!(pid))]),
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
    let (launch_result, effective_preset) = terminal::launch_terminal(
        &command,
        &runner_env,
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

/// Launch one or more AI tool instances with consistent tracking.
///
/// This is the unified entry point for launching Claude, Gemini, Codex,
/// and OpenCode instances with batch tracking, environment setup, and
/// error handling.
pub fn launch(db: &HcomDb, mut params: LaunchParams) -> Result<LaunchResult> {
    let normalized = LaunchTool::from_str(&params.tool, params.pty)?;
    let base_tool = normalized.base_tool();
    let backend = LaunchBackend::resolve(
        &normalized,
        params.background,
        normalized.uses_pty() || params.pty,
    );

    // Validation
    if params.count == 0 {
        bail!("Count must be positive");
    }
    if params.count > 100 {
        bail!(
            "Too many {} instances requested (max 100)",
            normalized.as_str()
        );
    }

    // Ensure agents dir exists for per-agent prompts
    crate::agent_prompts::ensure_agents_dir().ok();

    // Ensure hooks are installed (strict: refuse to launch without hooks)
    ensure_hooks_installed(&normalized)?;

    // Load config
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|e| {
        eprintln!("[hcom] warn: config load failed, using defaults: {e}");
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    // Build base environment
    let mut base_env = build_launch_env(&hcom_config);
    if let Some(ref caller_env) = params.env {
        base_env.extend(caller_env.clone());
    }
    base_env.remove("HCOM_TERMINAL");

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

    // Project env var
    if let Some(ref project) = params.project {
        base_env.insert("HCOM_PROJECT".to_string(), project.clone());
    }

    // Explicit name validation
    if let Some(ref name) = params.name {
        if params.count > 1 {
            bail!(
                "Cannot use explicit name with count > 1 (count={})",
                params.count
            );
        }
        // Check if name is already in use by an active instance
        if let Ok(Some(_)) = db.get_instance(name) {
            bail!(
                "Instance '{}' already exists (stop it first or use a different name)",
                name
            );
        }
    }

    // Tool args validation
    if !params.skip_validation {
        let validation_errors = validate_tool_args(&normalized, &params.args);
        if !validation_errors.is_empty() {
            bail!("{}", validation_errors.join("\n"));
        }
    }

    // System prompt file for Gemini/Codex
    if let Some(ref sp) = params.system_prompt {
        if normalized == LaunchTool::Gemini {
            let path = write_system_prompt_file(sp, "gemini");
            base_env.insert("GEMINI_SYSTEM_MD".to_string(), path);
        }
    }

    let working_dir = params.cwd.as_deref().unwrap_or(".");
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
        match normalized {
            LaunchTool::Claude | LaunchTool::ClaudePty => {
                // Claude: positional argument (after --)
                params.args.push("--".to_string());
                params.args.push(full_prompt);
            }
            LaunchTool::Gemini => {
                // Gemini: positional arg = interactive mode (--prompt would make it headless)
                params.args.push(full_prompt);
            }
            LaunchTool::Codex => {
                // Codex: positional argument
                params.args.push(full_prompt);
            }
            LaunchTool::OpenCode => {
                // OpenCode: --prompt flag
                params.args.push("--prompt".to_string());
                params.args.push(full_prompt);
            }
            LaunchTool::Kilo => {
                // Kilo: --prompt flag (same as opencode)
                params.args.push("--prompt".to_string());
                params.args.push(full_prompt);
            }
            LaunchTool::Cline => {
                // Cline: prompt as positional arg after --tui
                if !full_prompt.is_empty() {
                    params.args.push(full_prompt);
                }
            }
        }
    }

    // Cline always needs --tui flag regardless of prompt
    if normalized == LaunchTool::Cline && !params.args.contains(&"--tui".to_string()) {
        params.args.push("--tui".to_string());
    }
    let batch_id = params
        .batch_id
        .take()
        .unwrap_or_else(|| format!("{:08x}", rand::rng().random::<u32>()));

    let inside_ai_tool = crate::shared::context::HcomContext::from_os().is_inside_ai_tool();
    let terminal_mode = params
        .terminal
        .as_deref()
        .or(Some(hcom_config.terminal.as_str()).filter(|t| !t.is_empty()));

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

        // Process ID export: allow custom env var name
        if let Ok(export_var) = std::env::var("HCOM_PROCESS_ID_EXPORT") {
            if !export_var.is_empty() {
                instance_env.insert(export_var, process_id.clone());
            }
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

        // Pre-register instance
        if let Err(e) = (|| -> Result<()> {
            instance_binding::initialize_instance_in_position_file(
                db,
                &instance_name,
                None,            // session_id
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
                params.project.as_deref(),
                None,              // wait_timeout
                None,              // subagent_timeout
                None,              // hints
                Some(working_dir), // cwd_override: use launch params cwd, not current_dir()
            );
            db.set_process_binding(&process_id, "", &instance_name)?;
            Ok(())
        })() {
            errors.push(json!({"tool": normalized.as_str(), "error": e.to_string()}));
            continue;
        }

        // Check tool binary exists before launching
        let tool_binary = match normalized {
            LaunchTool::Claude | LaunchTool::ClaudePty => "claude",
            LaunchTool::Gemini => "gemini",
            LaunchTool::Codex => "codex",
            LaunchTool::OpenCode => "opencode",
            LaunchTool::Kilo => "kilo",
            LaunchTool::Cline => "cline",
        };
        if !is_tool_installed(tool_binary) {
            eprintln!("Error: '{}' is not installed or not in PATH", tool_binary);
            errors.push(
                json!({"tool": normalized.as_str(), "error": format!("{} not found", tool_binary)}),
            );
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
                            json!(params.args),
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
                            json!(params.args),
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
                            json!(params.args),
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
                            json!(effective_args),
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

                LaunchTool::OpenCode => {
                    opencode_preprocessing::preprocess_opencode_env(
                        &mut instance_env,
                        &instance_name,
                    );

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(params.args),
                        )]),
                    );

                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "opencode",
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

                LaunchTool::Kilo => {
                    kilo_preprocessing::preprocess_kilo_env(
                        &mut instance_env,
                        &instance_name,
                    );

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(params.args),
                        )]),
                    );

                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "kilo",
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

                LaunchTool::Cline => {
                    cline_preprocessing::preprocess_cline_env(
                        &mut instance_env,
                        &instance_name,
                    );

                    instances::update_instance_position(
                        db,
                        &instance_name,
                        &serde_json::Map::from_iter([(
                            "launch_args".to_string(),
                            json!(params.args),
                        )]),
                    );

                    launch_pty_or_background(
                        &mut BackgroundLaunchCtx {
                            db,
                            tool: "cline",
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
                errors.push(json!({"tool": normalized.as_str(), "error": e.to_string()}));
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
            "tool": normalized.as_str(),
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
        tool: normalized.as_str().to_string(),
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
fn validate_tool_args(tool: &LaunchTool, args: &[String]) -> Vec<String> {
    match tool {
        LaunchTool::Claude | LaunchTool::ClaudePty => {
            let spec = crate::hooks::claude_args::resolve_claude_args(Some(args), None);
            spec.errors.clone()
        }
        LaunchTool::Gemini => {
            let spec = crate::tools::gemini_args::resolve_gemini_args(Some(args), None);
            let mut errs = spec.errors.clone();
            errs.extend(crate::tools::gemini_args::validate_conflicts(&spec));
            errs
        }
        LaunchTool::Codex => {
            let spec = crate::tools::codex_args::resolve_codex_args(Some(args), None);
            let mut errs = spec.errors.clone();
            errs.extend(crate::tools::codex_args::validate_conflicts(&spec));
            errs
        }
        LaunchTool::OpenCode => Vec::new(),
        LaunchTool::Kilo => Vec::new(),
        LaunchTool::Cline => Vec::new(),
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

    #[test]
    fn test_launch_tool_from_str() {
        assert_eq!(
            LaunchTool::from_str("claude", false).unwrap(),
            LaunchTool::Claude
        );
        assert_eq!(
            LaunchTool::from_str("claude", true).unwrap(),
            LaunchTool::ClaudePty
        );
        assert_eq!(
            LaunchTool::from_str("gemini", false).unwrap(),
            LaunchTool::Gemini
        );
        assert_eq!(
            LaunchTool::from_str("codex", false).unwrap(),
            LaunchTool::Codex
        );
        assert_eq!(
            LaunchTool::from_str("opencode", false).unwrap(),
            LaunchTool::OpenCode
        );
        assert_eq!(
            LaunchTool::from_str("kilo", false).unwrap(),
            LaunchTool::Kilo
        );
        assert_eq!(
            LaunchTool::from_str("cline", false).unwrap(),
            LaunchTool::Cline
        );
        assert!(LaunchTool::from_str("unknown", false).is_err());
    }

    #[test]
    fn test_launch_tool_as_str() {
        assert_eq!(LaunchTool::Claude.as_str(), "claude");
        assert_eq!(LaunchTool::ClaudePty.as_str(), "claude-pty");
        assert_eq!(LaunchTool::Gemini.as_str(), "gemini");
    }

    #[test]
    fn test_launch_tool_base_tool() {
        assert_eq!(LaunchTool::Claude.base_tool(), "claude");
        assert_eq!(LaunchTool::ClaudePty.base_tool(), "claude");
        assert_eq!(LaunchTool::Codex.base_tool(), "codex");
    }

    #[test]
    fn test_launch_tool_uses_pty() {
        assert!(!LaunchTool::Claude.uses_pty());
        assert!(LaunchTool::ClaudePty.uses_pty());
        assert!(LaunchTool::Gemini.uses_pty());
        assert!(LaunchTool::Codex.uses_pty());
        assert!(LaunchTool::OpenCode.uses_pty());
        assert!(LaunchTool::Kilo.uses_pty());
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
        ] {
            let pty = tool.uses_pty();
            assert_eq!(
                LaunchBackend::resolve(&tool, false, pty),
                LaunchBackend::InteractiveVisible,
                "{:?} should resolve to InteractiveVisible without background",
                tool
            );
        }
    }

    #[test]
    fn test_launch_backend_resolve_claude_native_print() {
        // claude + background + NO pty → NativePrint (detached -p stream-json).
        assert_eq!(
            LaunchBackend::resolve(&LaunchTool::Claude, true, false),
            LaunchBackend::NativePrint
        );
    }

    #[test]
    fn test_launch_backend_resolve_claude_pty_headless() {
        // claude --pty --headless → HeadlessPty (PTY wrapper, live TUI).
        assert_eq!(
            LaunchBackend::resolve(&LaunchTool::ClaudePty, true, true),
            LaunchBackend::HeadlessPty
        );
    }

    #[test]
    fn test_launch_backend_resolve_other_tools_headless() {
        // gemini/codex/opencode/kilo + --headless → HeadlessPty (unchanged from today).
        for tool in [LaunchTool::Gemini, LaunchTool::Codex, LaunchTool::OpenCode, LaunchTool::Kilo] {
            assert_eq!(
                LaunchBackend::resolve(&tool, true, true),
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
}
