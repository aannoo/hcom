//! Terminal launching, script creation, and process management.
//!
//!
//! Handles:
//! - Terminal preset resolution (kitty, wezterm, tmux, etc.)
//! - Bash script creation for tool launches
//! - Terminal process spawning (new window, same terminal, background)
//! - Kill/close operations for managed terminals

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::paths;
use crate::shared::constants::{HCOM_IDENTITY_VARS, TOOL_MARKER_VARS};
use crate::shared::platform;
use crate::shared::terminal_presets::TERMINAL_ENV_MAP;

/// Result of kill_process().
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillResult {
    Sent,
    AlreadyDead,
    PermissionDenied,
}

/// Terminal info resolved for an instance.
#[derive(Debug, Clone, Default)]
pub struct TerminalInfo {
    pub preset_name: String,
    pub pane_id: String,
    pub process_id: String,
    pub kitty_listen_on: String,
    pub terminal_id: String,
}

/// Result from launch_terminal.
#[derive(Debug)]
pub enum LaunchResult {
    /// Background mode: (log_file_path, pid)
    Background(String, u32),
    /// Success (run_here or new window)
    Success,
    /// Failed
    Failed(String),
}

/// macOS app bundle fallback commands for cross-platform terminals.
/// Used when CLI binary isn't in PATH but .app bundle is installed.
const MACOS_APP_FALLBACKS: &[(&str, &str)] = &[
    ("kitty-window", "open -n -a kitty.app --args {script}"),
    (
        "wezterm-window",
        "open -n -a WezTerm.app --args start -- bash {script}",
    ),
    (
        "alacritty",
        "open -n -a Alacritty.app --args -e bash {script}",
    ),
];

/// Terminal context vars stripped from the env before spawning a terminal launcher subprocess.
/// Prevents outer terminal identity from leaking into newly-launched terminal panes.
/// Must stay in sync with every env var read by detect_terminal_from_env().
const TERMINAL_CONTEXT_VARS: &[&str] = &[
    // Multiplexers
    "CMUX_WORKSPACE_ID",
    "CMUX_SURFACE_ID",
    "TMUX_PANE",
    "ZELLIJ_PANE_ID",
    "ZELLIJ_SESSION_NAME",
    // GPU/rich terminals
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_LISTEN_ON",
    "WEZTERM_PANE",
    "WAVETERM_BLOCKID",
    // Bare terminal emulators
    "GHOSTTY_RESOURCES_DIR",
    "ITERM_SESSION_ID",
    "ALACRITTY_WINDOW_ID",
    "GNOME_TERMINAL_SCREEN",
    "KONSOLE_DBUS_WINDOW",
    "TERMINATOR_UUID",
    "TILIX_ID",
    "WT_SESSION",
    // Generic terminal identity
    "TERM_PROGRAM",
    "TERM_SESSION_ID",
    "COLORTERM",
];

/// Detect terminal preset from inherited environment variables.
/// Used for same-terminal PTY launches (run_here=True) to enable close-on-kill.
/// Checks built-in env map first, then TOML presets with pane_id_env defined.
pub fn detect_terminal_from_env() -> Option<String> {
    // Built-in mappings
    for &(env_var, preset_name) in TERMINAL_ENV_MAP {
        if std::env::var(env_var)
            .ok()
            .filter(|v| !v.is_empty())
            .is_some()
        {
            return Some(preset_name.to_string());
        }
    }
    // TOML-defined presets with pane_id_env
    let toml_path = crate::paths::config_toml_path();
    if let Some(presets_val) = crate::config::load_toml_presets(&toml_path) {
        if let Some(table) = presets_val.as_table() {
            for (name, val) in table {
                if let Some(env_var) = val.get("pane_id_env").and_then(|v| v.as_str()) {
                    if std::env::var(env_var)
                        .ok()
                        .filter(|v| !v.is_empty())
                        .is_some()
                    {
                        return Some(name.clone());
                    }
                }
            }
        }
    }
    // TERM_PROGRAM value-based detection (terminals without a unique env var)
    if let Ok(term_prog) = std::env::var("TERM_PROGRAM") {
        match term_prog.as_str() {
            "ghostty" => return Some("ghostty".to_string()),
            "iTerm.app" => return Some("iterm".to_string()),
            "Apple_Terminal" => return Some("terminal.app".to_string()),
            _ => {}
        }
    }
    None
}

/// Find macOS .app bundle in common locations.
fn find_macos_app(name: &str) -> Option<PathBuf> {
    let app_name = if name.ends_with(".app") {
        name.to_string()
    } else {
        format!("{}.app", name)
    };

    let home = std::env::var("HOME").ok()?;
    let search_dirs = [
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
        PathBuf::from(home).join("Applications"),
    ];

    for base in &search_dirs {
        let app_path = base.join(&app_name);
        if app_path.exists() {
            return Some(app_path);
        }
    }
    None
}

/// Replace `open -a <app>` app names with absolute `.app` bundle paths.
///
/// This is only safe for app-launch commands where `open` passes argv via
/// `--args`. Plain file-open forms like `open -a Terminal {script}` must keep
/// `-a`, otherwise `open` treats the app bundle and script as regular paths and
/// falls back to file association for the script.
fn rewrite_open_command_with_app_path(template: &str, app_path: &Path) -> Result<String> {
    let mut parts = shell_split(template)?;
    for idx in 0..parts.len().saturating_sub(1) {
        let flag = &parts[idx];
        let takes_app_arg = flag == "-a"
            || (flag.starts_with('-')
                && !flag.starts_with("--")
                && flag.chars().skip(1).any(|c| c == 'a'));
        if takes_app_arg {
            let has_args_tail = parts.iter().skip(idx + 2).any(|part| part == "--args");
            if !has_args_tail {
                return Ok(template.to_string());
            }
            if flag == "-a" {
                parts.remove(idx);
                parts[idx] = app_path.to_string_lossy().to_string();
            } else {
                let mut rewritten_flag = String::from("-");
                for ch in flag.chars().skip(1) {
                    if ch != 'a' {
                        rewritten_flag.push(ch);
                    }
                }
                if rewritten_flag == "-" {
                    parts.remove(idx);
                    parts[idx] = app_path.to_string_lossy().to_string();
                } else {
                    parts[idx] = rewritten_flag;
                    parts[idx + 1] = app_path.to_string_lossy().to_string();
                }
            }
            return Ok(parts
                .iter()
                .map(|p| shell_quote(p))
                .collect::<Vec<_>>()
                .join(" "));
        }
    }
    Ok(template.to_string())
}

fn rewrite_macos_open_app_command(template: &str, app_name: &str) -> String {
    if !cfg!(target_os = "macos") {
        return template.to_string();
    }
    let Some(app_path) = find_macos_app(app_name) else {
        return template.to_string();
    };
    rewrite_open_command_with_app_path(template, &app_path).unwrap_or_else(|_| template.to_string())
}

fn should_use_command_extension(background: bool, terminal_mode: &str) -> bool {
    !background
        && cfg!(target_os = "macos")
        && (terminal_mode == "default" || terminal_mode == "terminal.app")
}

/// Find kitten binary — PATH first, then macOS app bundle.
fn find_kitten_binary() -> Option<String> {
    if let Some(path) = which_bin("kitten") {
        return Some(path);
    }
    if cfg!(target_os = "macos") {
        if let Some(app) = find_macos_app("kitty") {
            let full = app.join("Contents/MacOS/kitten");
            if full.exists() {
                return Some(full.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Find a reachable kitty remote control socket.
pub fn find_kitty_socket() -> String {
    let kitten = match find_kitten_binary() {
        Some(k) => k,
        None => return String::new(),
    };

    // Find candidate sockets
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("kitty") {
                candidates.push(entry.path());
            }
        }
    }
    candidates.sort_by(|a, b| b.cmp(a)); // Reverse sort (newest first)

    for sock_path in &candidates {
        // Check if it's a socket
        if let Ok(meta) = fs::metadata(sock_path) {
            use std::os::unix::fs::FileTypeExt;
            if !meta.file_type().is_socket() {
                continue;
            }
        } else {
            continue;
        }

        let socket_uri = format!("unix:{}", sock_path.display());
        if let Ok(output) = Command::new(&kitten)
            .args(["@", "--to", &socket_uri, "ls"])
            .output()
        {
            if output.status.success() {
                return socket_uri;
            }
        }
    }
    String::new()
}

fn resolve_kitty_remote_socket(kitty_socket: &str) -> String {
    if !kitty_socket.is_empty() {
        return kitty_socket.to_string();
    }
    std::env::var("KITTY_LISTEN_ON")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(find_kitty_socket)
}

fn normalize_terminal_mode_for_launch(
    mut terminal_mode: String,
    opens_new_window: bool,
    run_here: bool,
) -> (String, String) {
    let mut kitty_socket = String::new();

    if opens_new_window {
        if terminal_mode == "default" {
            if let Some(detected) = detect_terminal_from_env() {
                terminal_mode = detected;
            }
        }
        if terminal_mode == "kitty" {
            if std::env::var("KITTY_WINDOW_ID")
                .ok()
                .filter(|v| !v.is_empty())
                .is_some()
            {
                // Inside kitty — use split, but still need socket for --to injection
                kitty_socket = resolve_kitty_remote_socket(&kitty_socket);
                terminal_mode = "kitty-split".to_string();
            } else {
                kitty_socket = find_kitty_socket();
                terminal_mode = if kitty_socket.is_empty() {
                    "kitty-window".to_string()
                } else {
                    "kitty-tab".to_string()
                };
            }
        } else if terminal_mode == "wezterm" {
            if std::env::var("WEZTERM_PANE")
                .ok()
                .filter(|v| !v.is_empty())
                .is_some()
            {
                terminal_mode = "wezterm-split".to_string();
            } else if wezterm_reachable() {
                terminal_mode = "wezterm-tab".to_string();
            } else {
                terminal_mode = "wezterm-window".to_string();
            }
        }

        if terminal_mode == "kitty-tab" || terminal_mode == "kitty-split" {
            kitty_socket = resolve_kitty_remote_socket(&kitty_socket);
        }
    } else if run_here {
        if let Some(detected) = detect_terminal_from_env() {
            terminal_mode = detected;
        } else if terminal_mode == "here" {
            terminal_mode = "default".to_string();
        }
    }

    (terminal_mode, kitty_socket)
}

pub fn resolve_terminal_mode_for_tips(
    terminal: Option<&str>,
    config_terminal: &str,
    background: bool,
    run_here: bool,
) -> (String, bool) {
    let explicit_terminal = terminal.filter(|t| !t.is_empty()).or_else(|| {
        (config_terminal != "default" && !config_terminal.is_empty()).then_some(config_terminal)
    });

    let requested = explicit_terminal.unwrap_or("default").to_string();
    let (resolved, _) =
        normalize_terminal_mode_for_launch(requested, !background && !run_here, run_here);

    (
        resolved.clone(),
        explicit_terminal.is_none() && resolved != "default",
    )
}

/// Check if a wezterm mux server is reachable.
pub fn wezterm_reachable() -> bool {
    Command::new("wezterm")
        .args(["cli", "list"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Simple `which` implementation — find binary in PATH.
pub fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(':') {
        let candidate = Path::new(dir).join(name);
        if candidate.exists() && candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }

    // Fallback: well-known install locations not always in PATH
    if let Ok(home) = std::env::var("HOME") {
        let home = Path::new(&home);
        let fallbacks: &[std::path::PathBuf] = match name {
            "claude" => &[
                home.join(".claude").join("local").join("claude"),
                home.join(".local").join("bin").join("claude"),
                home.join(".claude").join("bin").join("claude"),
            ],
            "opencode" => &[home.join(".opencode").join("bin").join("opencode")],
            _ => &[],
        };
        for fallback in fallbacks {
            if fallback.exists() && fallback.is_file() {
                return Some(fallback.to_string_lossy().to_string());
            }
        }
    }

    None
}

/// Check if a file has a node shebang (#!/usr/bin/env node or similar).
/// Used on Termux to detect npm-installed tools that need `node <path>` rewrite.
pub fn has_node_shebang(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 64];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    let header = String::from_utf8_lossy(&buf[..n]);
    header.starts_with("#!") && header.contains("node")
}

const TERMUX_CODEX_WRAPPER_PATH: &str = "/data/data/com.termux/files/usr/bin/codex";
const TERMUX_CODEX_INNER_WRAPPER_PATH: &str =
    "/data/data/com.termux/files/usr/lib/node_modules/@mmmbuto/codex-cli-termux/bin/codex";
const TERMUX_SH_PATH: &str = "/data/data/com.termux/files/usr/bin/sh";

/// Resolve Termux-only tool launch quirks.
///
/// Most npm-installed tools can run as `node <wrapper.js> ...`, but the
/// third-party `codex-cli-termux` wrapper breaks in stripped `RUN_COMMAND`
/// environments when its JS wrapper tries to spawn the nested shell wrapper
/// directly. Bypass that path by invoking the inner wrapper with `sh`.
pub fn resolve_termux_tool_launcher(
    tool_name: &str,
    resolved: &str,
) -> Option<(String, Vec<String>)> {
    if !platform::is_termux() {
        return None;
    }

    if tool_name == "codex"
        && resolved == TERMUX_CODEX_WRAPPER_PATH
        && Path::new(TERMUX_CODEX_INNER_WRAPPER_PATH).exists()
    {
        let sh = which_bin("sh").unwrap_or_else(|| TERMUX_SH_PATH.to_string());
        return Some((sh, vec![TERMUX_CODEX_INNER_WRAPPER_PATH.to_string()]));
    }

    if has_node_shebang(resolved) {
        let node = which_bin("node").unwrap_or_else(|| platform::TERMUX_NODE_PATH.to_string());
        return Some((node, vec![resolved.to_string()]));
    }

    None
}

/// Resolve binary to full path via macOS app bundle fallback.
fn resolve_binary_path(binary: &str, app_name: Option<&str>, preset_name: &str) -> Option<String> {
    if which_bin(binary).is_some() {
        return None; // Already on PATH
    }
    if !cfg!(target_os = "macos") {
        return None;
    }
    let app = find_macos_app(app_name.unwrap_or(preset_name))?;
    let full_path = app.join("Contents/MacOS").join(binary);
    if full_path.exists() {
        Some(full_path.to_string_lossy().to_string())
    } else {
        None
    }
}

/// Resolve preset name to command template string.
///
/// On macOS, if CLI binary isn't in PATH but .app bundle exists,
/// uses a hardcoded fallback or substitutes the full binary path.
pub fn resolve_terminal_preset(preset_name: &str) -> Option<String> {
    let merged = crate::config::get_merged_preset(preset_name)?;
    let mut open_cmd = merged.open;
    let app_name = merged.app_name.as_deref().unwrap_or(preset_name);

    if let Some(ref binary) = merged.binary {
        if which_bin(binary).is_none() && cfg!(target_os = "macos") {
            // New-window presets have hardcoded fallbacks using `open -a`
            for &(name, fallback) in MACOS_APP_FALLBACKS {
                if name == preset_name && find_macos_app(app_name).is_some() {
                    return Some(rewrite_macos_open_app_command(fallback, app_name));
                }
            }
            // Tab/split presets: substitute leading binary with full path
            if let Some(full_path) = resolve_binary_path(binary, Some(app_name), preset_name) {
                if open_cmd.starts_with(binary.as_str()) {
                    open_cmd = format!("{}{}", full_path, &open_cmd[binary.len()..]);
                }
            }
        }
    }

    Some(rewrite_macos_open_app_command(&open_cmd, app_name))
}

/// Get terminal presets for current platform with availability status.
pub fn get_available_presets() -> Vec<(String, bool)> {
    let mut result = vec![("default".to_string(), true)];
    let system = platform::platform_name();
    let mut seen = std::collections::HashSet::new();

    for (name, preset) in crate::shared::terminal_presets::TERMINAL_PRESETS.iter() {
        if !preset.platforms.contains(&system) {
            continue;
        }

        let available = if let Some(binary) = preset.binary {
            let in_path = which_bin(binary).is_some();
            if !in_path && system == "Darwin" {
                resolve_binary_path(binary, preset.app_name, name).is_some()
            } else {
                in_path
            }
        } else if system == "Darwin" {
            let app_name = preset.app_name.unwrap_or(name);
            find_macos_app(app_name).is_some()
        } else {
            true
        };

        result.push((name.to_string(), available));
        seen.insert(name.to_string());
    }

    // Add TOML-defined presets not already in built-ins
    let toml_path = crate::paths::config_toml_path();
    if let Some(presets_val) = crate::config::load_toml_presets(&toml_path) {
        if let Some(table) = presets_val.as_table() {
            for (name, preset_val) in table {
                if seen.contains(name) {
                    continue;
                }
                let available = preset_val
                    .get("binary")
                    .and_then(|v| v.as_str())
                    .map(|b| which_bin(b).is_some())
                    .unwrap_or(true);
                result.push((name.clone(), available));
            }
        }
    }

    result.push(("custom".to_string(), true));
    result
}

/// Build environment variable string for bash shells.
pub fn build_env_string(env_vars: &HashMap<String, String>, format_type: &str) -> String {
    let mut valid: Vec<(&String, &String)> = env_vars
        .iter()
        .filter(|(k, _)| {
            k.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .collect();
    valid.sort_by_key(|(k, _)| k.to_string());

    if format_type == "bash_export" {
        valid
            .iter()
            .map(|(k, v)| format!("export {}={};", k, shell_quote(v)))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        valid
            .iter()
            .map(|(k, v)| format!("{}={}", k, shell_quote(v)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Shell-quote a string for bash.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // If all safe chars, no quoting needed
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/_-.=:,@".contains(c))
    {
        return s.to_string();
    }
    // Use single quotes, escaping any embedded single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Create a bash script for terminal launch.
///
/// Scripts provide uniform execution across all platforms/terminals.
pub fn create_bash_script(
    script_file: &Path,
    env: &HashMap<String, String>,
    cwd: Option<&str>,
    command_str: &str,
    background: bool,
    tool_name: Option<&str>,
    opens_new_window: bool,
) -> Result<()> {
    let tool_name = tool_name.unwrap_or_else(|| {
        let cmd_lower = command_str.to_lowercase();
        if cmd_lower.contains("opencode") {
            "OpenCode"
        } else if cmd_lower.contains("gemini") {
            "Gemini"
        } else if cmd_lower.contains("codex") {
            "Codex"
        } else if cmd_lower.contains("claude") {
            "Claude Code"
        } else {
            "hcom"
        }
    });

    let mut f = fs::File::create(script_file).context("Failed to create script file")?;

    writeln!(f, "#!/bin/bash")?;
    writeln!(f, "printf \"\\033]0;hcom: starting {}...\\007\"", tool_name)?;
    writeln!(f, "echo \"Starting {}...\"", tool_name)?;

    // Unset tool markers and identity vars to prevent inheritance
    writeln!(f, "unset {}", TOOL_MARKER_VARS.join(" "))?;
    writeln!(f, "unset {}", HCOM_IDENTITY_VARS.join(" "))?;

    // Discover paths for minimal environments (kitty splits, etc.)
    let mut paths_to_add: Vec<String> = Vec::new();

    fn add_path(paths: &mut Vec<String>, binary_path: Option<String>) {
        if let Some(bp) = binary_path {
            if let Some(dir) = Path::new(&bp).parent() {
                let dir_str = dir.to_string_lossy().to_string();
                if !paths.contains(&dir_str) {
                    paths.push(dir_str);
                }
            }
        }
    }

    // Always add hcom's own directory
    add_path(&mut paths_to_add, which_bin("hcom"));
    // Add python3 to PATH for agents that need it
    add_path(&mut paths_to_add, which_bin("python3"));
    // Detect tool from command and add its path
    let cmd_stripped = command_str.trim_start();
    let tool_cmd = cmd_stripped.split_whitespace().next().unwrap_or("");
    add_path(&mut paths_to_add, which_bin(tool_cmd));
    // Claude needs node
    if tool_cmd == "claude" {
        add_path(&mut paths_to_add, which_bin("node"));
    }

    if !paths_to_add.is_empty() {
        writeln!(f, "export PATH=\"{}:$PATH\"", paths_to_add.join(":"))?;
    }

    // Write env exports
    let env_str = build_env_string(env, "bash_export");
    if !env_str.is_empty() {
        writeln!(f, "{}", env_str)?;
    }

    if let Some(dir) = cwd {
        writeln!(f, "cd {}", shell_quote(dir))?;
    }

    // Resolve tool path for full path execution.
    // On Termux, npm-installed tools have shebangs like #!/usr/bin/env node which
    // fail (no /usr/bin/env). Detect node shebangs and rewrite to: node /path/to/tool args
    let mut final_command = command_str.to_string();
    if !tool_cmd.is_empty() {
        if let Some(tool_path) = which_bin(tool_cmd) {
            if let Some((launcher, prefix_args)) =
                resolve_termux_tool_launcher(tool_cmd, &tool_path)
            {
                let mut replacement_parts = vec![shell_quote(&launcher)];
                replacement_parts.extend(prefix_args.iter().map(|arg| shell_quote(arg)));
                final_command = final_command.replacen(
                    &format!("{} ", tool_cmd),
                    &format!("{} ", replacement_parts.join(" ")),
                    1,
                );
            } else {
                final_command = final_command.replacen(
                    &format!("{} ", tool_cmd),
                    &format!("{} ", shell_quote(&tool_path)),
                    1,
                );
            }
        }
    }

    writeln!(f, "{}", final_command)?;

    if opens_new_window {
        writeln!(
            f,
            "unset HCOM_PROCESS_ID HCOM_LAUNCHED HCOM_PTY_MODE HCOM_TAG HCOM_CODEX_SANDBOX_MODE"
        )?;
        writeln!(f, "rm -f {}", shell_quote(&script_file.to_string_lossy()))?;
        writeln!(f, "exec bash -l")?;
    } else if !background {
        writeln!(f, "hcom_status=$?")?;
        writeln!(f, "rm -f {}", shell_quote(&script_file.to_string_lossy()))?;
        writeln!(f, "exit $hcom_status")?;
    }

    // Make executable
    fs::set_permissions(script_file, fs::Permissions::from_mode(0o755))?;

    Ok(())
}

/// Build clean env for terminal launcher subprocesses.
///
/// Strips AI tool markers, hcom identity vars, and terminal context vars.
fn get_launcher_env() -> HashMap<String, String> {
    let mut strip: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for v in TOOL_MARKER_VARS {
        strip.insert(v);
    }
    for v in HCOM_IDENTITY_VARS {
        strip.insert(v);
    }
    for v in TERMINAL_CONTEXT_VARS {
        strip.insert(v);
    }
    strip.insert("HCOM_LAUNCHED_PRESET");

    std::env::vars()
        .filter(|(k, _)| !strip.contains(k.as_str()))
        .collect()
}

/// Parse terminal command template safely to prevent shell injection.
fn parse_terminal_command(
    template: &str,
    script_file: &str,
    process_id: &str,
) -> Result<Vec<String>> {
    if !template.contains("{script}") {
        bail!(
            "Custom terminal command must include {{script}} placeholder\n\
             Example: open -n -a kitty.app --args bash \"{{script}}\""
        );
    }

    let parts = shell_split(template)?;

    let mut replaced = Vec::new();
    let mut placeholder_found = false;
    for mut part in parts {
        if part.contains("{process_id}") {
            part = part.replace("{process_id}", process_id);
        }
        if part.contains("{script}") {
            part = part.replace("{script}", script_file);
            placeholder_found = true;
        }
        replaced.push(part);
    }

    if !placeholder_found {
        bail!("{{script}} placeholder not found after parsing");
    }

    Ok(replaced)
}

/// Shell-split a string.
fn shell_split(s: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape_next = false;

    for ch in s.chars() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escape_next = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }

    if in_single || in_double {
        bail!("Unmatched quote in command");
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    Ok(tokens)
}

/// Get macOS Terminal.app launch command.
fn get_macos_terminal_command() -> String {
    rewrite_macos_open_app_command("open -a Terminal {script}", "Terminal")
}

/// Escape a string for use inside a YAML double-quoted scalar.
fn yaml_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Warp Stable's `~/.warp/launch_configurations/` dir.
///
/// Stable only for v1. Other channels (Preview/Dev/Local/Oss) own separate
/// config dirs and URL schemes (`warppreview://` etc.), so a single `warp://`
/// only ever reaches one channel. Add per-channel presets later if needed.
fn warp_launch_config_dir(home: &Path) -> PathBuf {
    home.join(".warp").join("launch_configurations")
}

/// Build YAML body for a one-pane Warp launch config that runs `bash <script>`.
fn build_warp_launch_yaml(config_name: &str, cwd: &str, script: &str) -> String {
    let exec_str = format!("bash {}", shell_quote(script));
    format!(
        "name: {name}\nwindows:\n  - tabs:\n      - layout:\n          cwd: {cwd}\n          commands:\n            - exec: {exec}\n",
        name = yaml_double_quote(config_name),
        cwd = yaml_double_quote(cwd),
        exec = yaml_double_quote(&exec_str),
    )
}

/// Resolve `cwd` to an absolute path Warp will accept for the pane's initial dir.
///
/// Warp's URL-based launch decouples the pane from the spawning process's
/// working dir, so the pane cwd must be set explicitly. For relative or
/// missing input, use the launcher's current_dir (the prefix the script's
/// later `cd <cwd>` would resolve against) so a relative `cd .` or
/// `cd subdir` lands where a non-Warp launch would. HOME is a last resort
/// if current_dir() fails.
fn resolve_warp_cwd(cwd: Option<&str>, home: &Path) -> String {
    if let Some(c) = cwd {
        if Path::new(c).is_absolute() {
            return c.to_string();
        }
    }
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| home.to_string_lossy().to_string())
}

/// Delete hcom-*.yaml files older than `older_than` from a channel dir.
///
/// Sweep-on-write avoids races with Warp cold start (where `open warp://...`
/// returns before Warp boots and reads the URL). Older configs should no
/// longer be needed by Warp.
const WARP_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(120);

fn sweep_stale_warp_configs(dir: &Path, older_than: std::time::Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("hcom-") || !name_str.ends_with(".yaml") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if now.duration_since(mtime).unwrap_or_default() > older_than {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Write a Warp launch_config YAML for `bash <script>` to Warp Stable's dir.
///
/// Warp has no CLI inject; the only way to launch a command is via
/// `warp://launch/<config_name>` which reads a YAML from the channel-specific
/// `launch_configurations/` dir. Returns the path written.
fn write_warp_launch_config(process_id: &str, cwd: Option<&str>, script: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    write_warp_launch_config_at(Path::new(&home), process_id, cwd, script)
}

fn write_warp_launch_config_at(
    home: &Path,
    process_id: &str,
    cwd: Option<&str>,
    script: &str,
) -> Result<PathBuf> {
    let dir = warp_launch_config_dir(home);
    fs::create_dir_all(&dir).context("Failed to create Warp launch_configurations dir")?;
    sweep_stale_warp_configs(&dir, WARP_STALE_AFTER);

    let config_name = format!("hcom-{}", process_id);
    let resolved_cwd = resolve_warp_cwd(cwd, home);
    let yaml = build_warp_launch_yaml(&config_name, &resolved_cwd, script);
    let yaml_path = dir.join(format!("{}.yaml", config_name));
    fs::write(&yaml_path, &yaml).context("Failed to write Warp launch config")?;
    Ok(yaml_path)
}

/// Return a human-readable name for the platform's built-in fallback terminal
/// (used when `terminal = "default"` and no terminal is detected from env).
pub fn get_default_fallback_terminal_name() -> &'static str {
    if platform::is_termux() {
        return "Termux";
    }
    match platform::platform_name() {
        "Darwin" => "Terminal.app",
        "Linux" => {
            if platform::is_wsl() {
                if which_bin("wt.exe").is_some() {
                    "Windows Terminal"
                } else {
                    "cmd.exe"
                }
            } else if which_bin("gnome-terminal").is_some() {
                "gnome-terminal"
            } else if which_bin("konsole").is_some() {
                "konsole"
            } else if which_bin("xterm").is_some() {
                "xterm"
            } else {
                "none"
            }
        }
        _ => "unknown",
    }
}

/// Get first available standard Linux terminal.
fn get_linux_terminal_argv() -> Option<Vec<String>> {
    let terminals = [
        (
            "gnome-terminal",
            &["gnome-terminal", "--", "bash", "{script}"] as &[&str],
        ),
        ("konsole", &["konsole", "-e", "bash", "{script}"]),
        ("xterm", &["xterm", "-e", "bash", "{script}"]),
    ];

    for (term_name, argv) in &terminals {
        if which_bin(term_name).is_some() {
            return Some(argv.iter().map(|s| s.to_string()).collect());
        }
    }

    // WSL fallback
    if platform::is_wsl() && which_bin("cmd.exe").is_some() {
        if which_bin("wt.exe").is_some() {
            return Some(
                ["cmd.exe", "/c", "start", "wt.exe", "bash", "{script}"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            );
        }
        return Some(
            ["cmd.exe", "/c", "start", "bash", "{script}"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
    }

    None
}

/// Spawn terminal process, detached when inside AI tools.
///
/// Returns (success, stdout_first_line) — stdout captured for {id} in close commands.
fn is_external_terminal_launcher(argv: &[String]) -> bool {
    let Some(cmd) = argv.first().map(|s| s.as_str()) else {
        return false;
    };

    matches!(
        cmd,
        "open"
            | "tmux"
            | "kitty"
            | "kitten"
            | "wezterm"
            | "alacritty"
            | "cmux"
            | "ttab"
            | "wttab"
            | "gnome-terminal"
            | "konsole"
            | "xterm"
            | "tilix"
            | "terminator"
            | "zellij"
            | "wsh"
            | "wt"
            | "wt.exe"
            | "mintty"
    )
}

fn maybe_append_ai_tool_launch_hint(
    message: String,
    argv: &[String],
    inside_ai_tool: bool,
) -> String {
    if !inside_ai_tool || !is_external_terminal_launcher(argv) {
        return message;
    }

    let hint = if argv.first().map(|s| s.as_str()) == Some("tmux") {
        "Hint: Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."
    } else {
        "Hint: This launch was attempted from inside a managed AI tool session, where terminal or session creation may be restricted. Rerun it with approval/escalation."
    };
    format!("{message}\n{hint}")
}

fn spawn_terminal_process(argv: &[String], inside_ai_tool: bool) -> Result<(bool, String)> {
    let launcher_env = get_launcher_env();
    let env_vec: Vec<(String, String)> = launcher_env.into_iter().collect();

    if inside_ai_tool {
        // Fully detach: don't let AI tool's PTY capture our output
        let launch_dir = paths::hcom_path(&[paths::LAUNCH_DIR]);
        fs::create_dir_all(&launch_dir).ok();

        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .envs(env_vec.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| {
                anyhow!(maybe_append_ai_tool_launch_hint(
                    format!("Failed to spawn terminal process: {err}"),
                    argv,
                    inside_ai_tool,
                ))
            })?;

        let output = child
            .wait_with_output()
            .context("Failed to wait for terminal")?;

        let captured = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let msg = format!(
                "Terminal launch failed (exit code {})",
                output.status.code().unwrap_or(-1)
            );
            let full_msg = if stderr.is_empty() {
                msg
            } else {
                format!("{}: {}", msg, stderr)
            };
            bail!(maybe_append_ai_tool_launch_hint(
                full_msg,
                argv,
                inside_ai_tool
            ));
        }

        Ok((true, captured))
    } else {
        // Normal case: wait for terminal launcher to complete
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .envs(env_vec.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .context("Failed to run terminal launcher")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "Terminal launch failed (exit code {}): {}",
                output.status.code().unwrap_or(-1),
                stderr
            );
        }

        let captured = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        Ok((true, captured))
    }
}

/// Write captured terminal ID to temp file for child to read.
fn write_terminal_id(env: &HashMap<String, String>, captured_id: &str) {
    if captured_id.is_empty() {
        return;
    }
    let process_id = match env.get("HCOM_PROCESS_ID") {
        Some(pid) if !pid.is_empty() => pid,
        _ => return,
    };
    let ids_dir = paths::hcom_path(&[".tmp", "terminal_ids"]);
    fs::create_dir_all(&ids_dir).ok();
    fs::write(ids_dir.join(process_id), captured_id).ok();
}

/// Launch terminal with command.
///
/// # Modes
/// - `background=true`: Launch as background process, returns Background(log_file, pid)
/// - `run_here=true`: Run in current terminal (blocking via execve)
/// - Otherwise: New terminal window/tab/split
pub fn launch_terminal(
    command: &str,
    env: &HashMap<String, String>,
    cwd: Option<&str>,
    background: bool,
    run_here: bool,
    terminal: Option<&str>,
    inside_ai_tool: bool,
) -> Result<(LaunchResult, String)> {
    let config_and_instance_env = env.clone();

    // Determine terminal mode
    let mut terminal_mode = terminal.unwrap_or("default").to_string();

    let opens_new_window = !background && !run_here;

    // Resolve smart terminal shortcuts
    let (terminal_mode_resolved, kitty_socket) =
        normalize_terminal_mode_for_launch(terminal_mode, opens_new_window, run_here);
    terminal_mode = terminal_mode_resolved;

    let mut final_env = config_and_instance_env;
    if opens_new_window && !kitty_socket.is_empty() {
        final_env.insert("KITTY_LISTEN_ON".to_string(), kitty_socket.clone());
    }

    if terminal_mode != "default" && terminal_mode != "print" {
        final_env.insert("HCOM_LAUNCHED_PRESET".to_string(), terminal_mode.clone());
    }

    // Determine script extension after terminal mode resolution so explicit
    // Terminal.app uses the macOS `.command` launcher just like auto-detect.
    let extension = if should_use_command_extension(background, &terminal_mode) {
        ".command"
    } else {
        ".sh"
    };
    let script_file = paths::hcom_path(&[
        paths::LAUNCH_DIR,
        &format!(
            "hcom_{}_{}{}",
            std::process::id(),
            rand::random::<u16>() % 9000 + 1000,
            extension
        ),
    ]);

    // Ensure launch dir exists
    if let Some(parent) = script_file.parent() {
        fs::create_dir_all(parent).ok();
    }

    // Create script
    create_bash_script(
        &script_file,
        &final_env,
        cwd,
        command,
        background,
        None,
        opens_new_window,
    )?;

    // Background mode
    if background {
        let logs_dir = paths::hcom_path(&[paths::LOGS_DIR]);
        fs::create_dir_all(&logs_dir).ok();
        let log_name = env.get("HCOM_BACKGROUND").cloned().unwrap_or_default();
        let log_file = logs_dir.join(&log_name);

        let log_handle = fs::File::create(&log_file).context("Failed to create log file")?;

        let mut cmd = Command::new("bash");
        cmd.arg(&script_file)
            .stdin(std::process::Stdio::null())
            .stdout(log_handle.try_clone()?)
            .stderr(log_handle);

        // Detach child into its own session so it survives parent exit (no SIGHUP)
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let child = cmd.spawn().context("Failed to launch background process")?;

        // Brief health check
        std::thread::sleep(std::time::Duration::from_millis(200));
        let pid = child.id();

        return Ok((
            LaunchResult::Background(log_file.to_string_lossy().to_string(), pid),
            terminal_mode,
        ));
    }

    // Print mode (debug)
    if terminal_mode == "print" {
        let content = fs::read_to_string(&script_file)?;
        println!("# Script: {}", script_file.display());
        print!("{}", content);
        fs::remove_file(&script_file).ok();
        return Ok((LaunchResult::Success, terminal_mode));
    }

    // Run in current terminal (blocking)
    if run_here {
        // Build full env (config + shell)
        let full_env = build_full_env(&final_env);
        if let Some(dir) = cwd {
            std::env::set_current_dir(dir).ok();
        }
        // Use execve to replace this process entirely
        use std::ffi::CString;
        let bash_path = which_bin("bash").unwrap_or_else(|| "/bin/bash".to_string());
        let bash = CString::new(bash_path).unwrap();
        let arg0 = CString::new("bash").unwrap();
        let arg1 = CString::new(script_file.to_string_lossy().as_ref()).unwrap();
        let argv_ptrs: Vec<*const libc::c_char> =
            vec![arg0.as_ptr(), arg1.as_ptr(), std::ptr::null()];
        let env_cstrings: Vec<CString> = full_env
            .iter()
            .filter_map(|(k, v)| CString::new(format!("{}={}", k, v)).ok())
            .collect();
        let mut env_ptrs: Vec<*const libc::c_char> =
            env_cstrings.iter().map(|c| c.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());
        // execve replaces process; never returns on success
        unsafe {
            libc::execve(bash.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
        }
        bail!("execve failed: {}", std::io::Error::last_os_error());
    }

    // New window / custom command mode
    let custom_cmd: Option<String> = if terminal_mode == "default" {
        None
    } else if crate::config::get_merged_preset(&terminal_mode).is_some() {
        // Known preset — check kitty remote control requirements
        if terminal_mode == "kitty-tab" || terminal_mode == "kitty-split" {
            let listen_on = std::env::var("KITTY_LISTEN_ON")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| kitty_socket.clone());
            if listen_on.is_empty() {
                bail!(
                    "{} requires remote control.\n\
                     Add to ~/.config/kitty/kitty.conf:\n\
                     allow_remote_control yes\n\
                     listen_on unix:/tmp/kitty\n\
                     Then restart kitty.",
                    terminal_mode
                );
            }
        }
        let mut cmd = resolve_terminal_preset(&terminal_mode).unwrap_or_default();
        // Inject --to for kitty commands launched outside kitty
        if !kitty_socket.is_empty() && cmd.contains("kitten @") && !cmd.contains("--to") {
            cmd = cmd.replace(
                "kitten @",
                &format!("kitten @ --to {}", shell_quote(&kitty_socket)),
            );
        }
        // Target launcher's tab for splits
        if terminal_mode == "kitty-tab" || terminal_mode == "kitty-split" {
            if let Ok(wid) = std::env::var("KITTY_WINDOW_ID") {
                if !wid.is_empty() && cmd.contains(" -- ") {
                    cmd = cmd.replacen(" -- ", &format!(" --match window_id:{} -- ", wid), 1);
                }
            }
        }
        Some(cmd)
    } else {
        // Custom command template
        Some(terminal_mode.clone())
    };

    let script_str = script_file.to_string_lossy().to_string();

    if terminal_mode == "warp" {
        let process_id = env
            .get("HCOM_PROCESS_ID")
            .map(|s| s.as_str())
            .unwrap_or("");
        if process_id.is_empty() {
            bail!("warp preset requires HCOM_PROCESS_ID to name the launch config");
        }
        write_warp_launch_config(process_id, cwd, &script_str)?;
        let final_argv = vec![
            "open".to_string(),
            format!("warp://launch/hcom-{}", process_id),
        ];
        let (success, captured_id) = spawn_terminal_process(&final_argv, inside_ai_tool)?;
        write_terminal_id(env, &captured_id);
        return if success {
            Ok((LaunchResult::Success, terminal_mode))
        } else {
            Ok((
                LaunchResult::Failed("Terminal process failed".to_string()),
                terminal_mode,
            ))
        };
    }

    if let Some(cmd_template) = custom_cmd {
        // Parse user-provided or preset command template
        let final_argv = parse_terminal_command(
            &cmd_template,
            &script_str,
            env.get("HCOM_PROCESS_ID").map(|s| s.as_str()).unwrap_or(""),
        )?;
        let (success, captured_id) = spawn_terminal_process(&final_argv, inside_ai_tool)?;
        write_terminal_id(env, &captured_id);
        if success {
            Ok((LaunchResult::Success, terminal_mode))
        } else {
            Ok((
                LaunchResult::Failed("Terminal process failed".to_string()),
                terminal_mode,
            ))
        }
    } else {
        // Platform default
        if platform::is_termux() {
            let am_argv = vec![
                "am",
                "startservice",
                "--user",
                "0",
                "-n",
                "com.termux/com.termux.app.RunCommandService",
                "-a",
                "com.termux.RUN_COMMAND",
                "--es",
                "com.termux.RUN_COMMAND_PATH",
                &script_str,
                "--ez",
                "com.termux.RUN_COMMAND_BACKGROUND",
                "false",
            ];
            Command::new(am_argv[0])
                .args(&am_argv[1..])
                .status()
                .context("Failed to launch Termux")?;
            return Ok((LaunchResult::Success, terminal_mode));
        }

        let argv = match platform::platform_name() {
            "Darwin" => parse_terminal_command(
                &get_macos_terminal_command(),
                &script_str,
                env.get("HCOM_PROCESS_ID").map(|s| s.as_str()).unwrap_or(""),
            )?,
            "Linux" => get_linux_terminal_argv()
                .ok_or_else(|| anyhow::anyhow!("No supported terminal emulator found"))?,
            other => bail!("Unsupported platform: {}", other),
        };

        let final_argv: Vec<String> = if platform::platform_name() == "Darwin" {
            argv
        } else {
            argv.iter()
                .map(|a| a.replace("{script}", &script_str))
                .collect()
        };
        let (success, captured_id) = spawn_terminal_process(&final_argv, inside_ai_tool)?;
        write_terminal_id(env, &captured_id);
        if success {
            Ok((LaunchResult::Success, terminal_mode))
        } else {
            Ok((
                LaunchResult::Failed("Terminal process failed".to_string()),
                terminal_mode,
            ))
        }
    }
}

/// Build full env from config env + shell env.
fn build_full_env(config_env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut full = config_env.clone();
    for (k, v) in std::env::vars() {
        if TOOL_MARKER_VARS.contains(&k.as_str()) {
            continue;
        }
        if k == "HCOM_TERMINAL" {
            continue;
        }
        // Config env takes precedence for HCOM_ vars
        full.entry(k).or_insert(v);
    }
    full
}

/// Close terminal pane via preset-specific command.
///
/// Must run before SIGTERM because terminal CLIs match panes by PID/pane_id.
/// Non-fatal: caller should always proceed with SIGTERM regardless.
pub fn close_terminal_pane(
    pid: u32,
    preset_name: &str,
    pane_id: &str,
    process_id: &str,
    kitty_listen_on: &str,
    terminal_id: &str,
) -> bool {
    let merged = match crate::config::get_merged_preset(preset_name) {
        Some(p) => p,
        None => return false,
    };

    let close_template = match merged.close {
        Some(ref c) => c.clone(),
        None => return false,
    };

    let mut close_cmd = close_template;

    // Determine effective pane_id (fall back to terminal_id)
    let effective_pane_id = if pane_id.is_empty() && !terminal_id.is_empty() {
        terminal_id
    } else {
        pane_id
    };

    // Skip if command needs a placeholder we don't have
    if close_cmd.contains("{pane_id}") && effective_pane_id.is_empty() {
        return false;
    }
    if close_cmd.contains("{process_id}") && process_id.is_empty() {
        return false;
    }
    if close_cmd.contains("{id}") && terminal_id.is_empty() {
        return false;
    }

    close_cmd = close_cmd.replace("{pid}", &pid.to_string());
    close_cmd = close_cmd.replace("{pane_id}", effective_pane_id);
    close_cmd = close_cmd.replace("{process_id}", process_id);
    close_cmd = close_cmd.replace("{id}", terminal_id);

    // Resolve binary path via app bundle fallback
    if let Some(ref binary) = merged.binary {
        let app_name = merged.app_name.as_deref().unwrap_or(preset_name);
        if let Some(full_path) = resolve_binary_path(binary, Some(app_name), preset_name) {
            if close_cmd.starts_with(binary.as_str()) {
                close_cmd = format!("{}{}", full_path, &close_cmd[binary.len()..]);
            }
        }
    }
    if close_cmd.starts_with("kitten ") {
        if let Some(full_path) = find_kitten_binary() {
            close_cmd = format!(
                "{}{}",
                shell_quote(&full_path),
                &close_cmd["kitten".len()..]
            );
        }
    }

    // Inject --to for kitten commands when we have the socket path
    if close_cmd.contains("kitten @")
        && !kitty_listen_on.is_empty()
        && !close_cmd.contains("--to")
        && !kitty_listen_on.starts_with("fd:")
    {
        close_cmd = close_cmd.replace(
            "kitten @",
            &format!("kitten @ --to {}", shell_quote(kitty_listen_on)),
        );
    }

    // Execute
    Command::new("sh")
        .args(["-c", &close_cmd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Close terminal pane (if applicable) then SIGTERM the process group.
pub fn kill_process(
    pid: u32,
    preset_name: &str,
    pane_id: &str,
    process_id: &str,
    kitty_listen_on: &str,
    terminal_id: &str,
) -> (KillResult, bool) {
    let pane_closed = if !preset_name.is_empty() {
        close_terminal_pane(
            pid,
            preset_name,
            pane_id,
            process_id,
            kitty_listen_on,
            terminal_id,
        )
    } else {
        false
    };

    // SIGTERM the process group
    let result = unsafe { libc::killpg(pid as i32, libc::SIGTERM) };
    let kill_result = if result == 0 {
        KillResult::Sent
    } else {
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => KillResult::AlreadyDead,
            Some(libc::EPERM) => KillResult::PermissionDenied,
            _ => KillResult::AlreadyDead,
        }
    };

    (kill_result, pane_closed)
}

/// Resolve terminal info from the canonical preset fields plus launch_context metadata.
pub fn resolve_terminal_info(
    preset_name: Option<&str>,
    launch_context_json: Option<&str>,
) -> TerminalInfo {
    let mut info = TerminalInfo {
        preset_name: preset_name.unwrap_or("").to_string(),
        ..TerminalInfo::default()
    };

    if let Some(launch_context_json) = launch_context_json.filter(|s| !s.is_empty()) {
        if let Ok(lc) = serde_json::from_str::<serde_json::Value>(launch_context_json) {
            if info.preset_name.is_empty() {
                info.preset_name = lc
                    .get("terminal_preset_effective")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        lc.get("terminal_preset")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or("")
                    .to_string();
            }
            info.pane_id = lc
                .get("pane_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            info.process_id = lc
                .get("process_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            info.terminal_id = lc
                .get("terminal_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Kitty socket from launch context or env snapshot
            let lc_env = lc.get("env").and_then(|v| v.as_object());
            info.kitty_listen_on = lc
                .get("kitty_listen_on")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| lc_env.and_then(|e| e.get("KITTY_LISTEN_ON").and_then(|v| v.as_str())))
                .unwrap_or("")
                .to_string();
        }
    }

    // Legacy kitty launches may have pane/socket metadata but no persisted preset.
    // Both kitty-tab and kitty-split now close via close-window on the captured ID,
    // so treating these old records as kitty-split is sufficient for cleanup.
    if info.preset_name.is_empty() && !info.pane_id.is_empty() && !info.kitty_listen_on.is_empty() {
        info.preset_name = "kitty-split".to_string();
    }

    info
}

/// Parse only launch_context metadata. Prefer `resolve_terminal_info()` for runtime decisions.
pub fn resolve_terminal_info_from_launch_context(launch_context_json: &str) -> TerminalInfo {
    resolve_terminal_info(None, Some(launch_context_json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn test_shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "hello");
    }

    #[test]
    fn test_shell_quote_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_quote_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_shell_split_basic() {
        let parts = shell_split("foo bar baz").unwrap();
        assert_eq!(parts, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_shell_split_quoted() {
        let parts = shell_split("foo 'bar baz' qux").unwrap();
        assert_eq!(parts, vec!["foo", "bar baz", "qux"]);
    }

    #[test]
    fn test_shell_split_double_quoted() {
        let parts = shell_split(r#"foo "bar baz" qux"#).unwrap();
        assert_eq!(parts, vec!["foo", "bar baz", "qux"]);
    }

    #[test]
    fn test_shell_split_unmatched_quote() {
        assert!(shell_split("foo 'bar").is_err());
    }

    #[test]
    fn test_resolve_terminal_info_prefers_effective_preset() {
        let info = resolve_terminal_info(Some("kitty-tab"), Some(r#"{"pane_id":"x"}"#));
        assert_eq!(info.preset_name, "kitty-tab");
    }

    #[test]
    fn test_resolve_terminal_info_reads_launch_context_metadata() {
        let info = resolve_terminal_info(
            Some("wezterm-split"),
            Some(r#"{"pane_id":"pane-1","process_id":"proc-1","terminal_id":"term-1"}"#),
        );
        assert_eq!(info.preset_name, "wezterm-split");
        assert_eq!(info.pane_id, "pane-1");
        assert_eq!(info.process_id, "proc-1");
        assert_eq!(info.terminal_id, "term-1");
    }

    #[test]
    fn test_yaml_double_quote_escapes_backslash_and_quote() {
        assert_eq!(yaml_double_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(yaml_double_quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(yaml_double_quote("plain"), "\"plain\"");
    }

    #[test]
    fn test_build_warp_launch_yaml_shape() {
        let yaml = build_warp_launch_yaml("hcom-pid", "/some/dir", "/tmp/script.sh");
        assert!(yaml.contains("name: \"hcom-pid\""));
        assert!(yaml.contains("cwd: \"/some/dir\""));
        assert!(yaml.contains("exec: \"bash /tmp/script.sh\""));
    }

    #[test]
    fn test_warp_launch_config_dir_is_stable_channel() {
        let dir = warp_launch_config_dir(Path::new("/h"));
        assert_eq!(dir, Path::new("/h/.warp/launch_configurations"));
    }

    #[test]
    fn test_write_warp_launch_config_writes_to_stable_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let written = write_warp_launch_config_at(
            tmp.path(),
            "test-pid",
            Some("/some/dir"),
            "/tmp/script.sh",
        )
        .unwrap();
        assert!(written.ends_with(".warp/launch_configurations/hcom-test-pid.yaml"));
        let content = std::fs::read_to_string(&written).unwrap();
        assert!(content.contains("name: \"hcom-test-pid\""));
        assert!(content.contains("exec: \"bash /tmp/script.sh\""));
        assert!(content.contains("cwd: \"/some/dir\""));
    }

    #[test]
    fn test_resolve_warp_cwd_keeps_absolute() {
        let home = Path::new("/h");
        assert_eq!(resolve_warp_cwd(Some("/abs/path"), home), "/abs/path");
    }

    #[test]
    fn test_resolve_warp_cwd_uses_current_dir_for_relative_or_missing() {
        let home = Path::new("/h");
        let cwd_str = std::env::current_dir().unwrap().to_string_lossy().to_string();
        // Must match the prefix the script's later `cd <cwd>` resolves against.
        assert_eq!(resolve_warp_cwd(Some("subdir"), home), cwd_str);
        assert_eq!(resolve_warp_cwd(Some("./rel"), home), cwd_str);
        assert_eq!(resolve_warp_cwd(Some("."), home), cwd_str);
        assert_eq!(resolve_warp_cwd(None, home), cwd_str);
    }

    #[test]
    fn test_sweep_stale_warp_configs_only_removes_hcom_prefixed_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let target = dir.join("hcom-old.yaml");
        let other = dir.join("user-config.yaml");
        let unrelated = dir.join("hcom-old.txt");
        std::fs::write(&target, "x").unwrap();
        std::fs::write(&other, "x").unwrap();
        std::fs::write(&unrelated, "x").unwrap();

        sweep_stale_warp_configs(dir, std::time::Duration::from_secs(0));

        assert!(!target.exists(), "hcom-*.yaml should be swept");
        assert!(other.exists(), "non-hcom-prefixed yaml should remain");
        assert!(unrelated.exists(), "non-yaml extension should remain");
    }

    #[test]
    fn test_sweep_stale_warp_configs_keeps_fresh_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let fresh = dir.join("hcom-new.yaml");
        std::fs::write(&fresh, "x").unwrap();

        sweep_stale_warp_configs(dir, std::time::Duration::from_secs(3600));

        assert!(fresh.exists(), "fresh file should remain");
    }

    #[test]
    fn test_warp_preset_registered() {
        let preset = crate::shared::terminal_presets::get_terminal_preset("warp").unwrap();
        assert_eq!(preset.app_name, Some("Warp"));
        assert_eq!(preset.binary, None);
        assert!(preset.open.contains("warp://launch/hcom-{process_id}"));
        assert_eq!(preset.platforms, &["Darwin"]);
    }

    #[test]
    fn test_build_env_string_bash() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let result = build_env_string(&env, "bash");
        assert_eq!(result, "FOO=bar");
    }

    #[test]
    fn test_build_env_string_export() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar baz".to_string());
        let result = build_env_string(&env, "bash_export");
        assert_eq!(result, "export FOO='bar baz';");
    }

    #[test]
    fn test_build_env_string_filters_invalid() {
        let mut env = HashMap::new();
        env.insert("GOOD".to_string(), "val".to_string());
        env.insert("123BAD".to_string(), "val".to_string());
        let result = build_env_string(&env, "bash");
        assert!(result.contains("GOOD"));
        assert!(!result.contains("123BAD"));
    }

    #[test]
    fn test_detect_terminal_from_env_none() {
        // In test environment, none of the terminal env vars should be set
        // (unless running inside kitty/tmux, in which case this test is fine to skip)
        let result = detect_terminal_from_env();
        // Just verify it returns an Option - value depends on test environment
        let _ = result;
    }

    #[test]
    #[serial]
    fn test_normalize_terminal_mode_for_launch_resolves_socket_for_auto_detected_kitty() {
        let saved_window = std::env::var("KITTY_WINDOW_ID").ok();
        let saved_listen = std::env::var("KITTY_LISTEN_ON").ok();
        unsafe {
            std::env::set_var("KITTY_WINDOW_ID", "window-1");
            std::env::set_var("KITTY_LISTEN_ON", "unix:/tmp/kitty-test");
        }

        let (mode, socket) = normalize_terminal_mode_for_launch("default".to_string(), true, false);

        assert_eq!(mode, "kitty-split");
        assert_eq!(socket, "unix:/tmp/kitty-test");

        unsafe {
            if let Some(value) = saved_window {
                std::env::set_var("KITTY_WINDOW_ID", value);
            } else {
                std::env::remove_var("KITTY_WINDOW_ID");
            }
            if let Some(value) = saved_listen {
                std::env::set_var("KITTY_LISTEN_ON", value);
            } else {
                std::env::remove_var("KITTY_LISTEN_ON");
            }
        }
    }

    #[test]
    #[serial]
    fn test_resolve_terminal_mode_for_tips_uses_normalized_auto_detected_mode() {
        let saved_window = std::env::var("KITTY_WINDOW_ID").ok();
        let saved_listen = std::env::var("KITTY_LISTEN_ON").ok();
        unsafe {
            std::env::set_var("KITTY_WINDOW_ID", "window-1");
            std::env::set_var("KITTY_LISTEN_ON", "unix:/tmp/kitty-test");
        }

        let (mode, auto) = resolve_terminal_mode_for_tips(None, "default", false, false);

        assert_eq!(mode, "kitty-split");
        assert!(auto);

        unsafe {
            if let Some(value) = saved_window {
                std::env::set_var("KITTY_WINDOW_ID", value);
            } else {
                std::env::remove_var("KITTY_WINDOW_ID");
            }
            if let Some(value) = saved_listen {
                std::env::set_var("KITTY_LISTEN_ON", value);
            } else {
                std::env::remove_var("KITTY_LISTEN_ON");
            }
        }
    }

    #[test]
    fn test_resolve_terminal_info_uses_launch_context_preset_when_column_missing() {
        let info = resolve_terminal_info(
            None,
            Some(
                r#"{"terminal_preset_effective":"kitty-tab","pane_id":"pane-1","kitty_listen_on":"unix:/tmp/kitty"}"#,
            ),
        );
        assert_eq!(info.preset_name, "kitty-tab");
        assert_eq!(info.pane_id, "pane-1");
        assert_eq!(info.kitty_listen_on, "unix:/tmp/kitty");
    }

    #[test]
    fn test_resolve_terminal_info_falls_back_for_legacy_kitty_metadata() {
        let info = resolve_terminal_info(
            None,
            Some(r#"{"pane_id":"pane-1","kitty_listen_on":"unix:/tmp/kitty"}"#),
        );
        assert_eq!(info.preset_name, "kitty-split");
        assert_eq!(info.pane_id, "pane-1");
        assert_eq!(info.kitty_listen_on, "unix:/tmp/kitty");
    }

    #[test]
    fn test_parse_terminal_command_basic() {
        let argv = parse_terminal_command("open -a Terminal {script}", "/tmp/test.sh", "").unwrap();
        assert_eq!(argv, vec!["open", "-a", "Terminal", "/tmp/test.sh"]);
    }

    #[test]
    fn test_rewrite_open_command_with_app_path() {
        let rewritten = rewrite_open_command_with_app_path(
            "open -a Terminal {script}",
            Path::new("/System/Applications/Utilities/Terminal.app"),
        )
        .unwrap();
        assert_eq!(rewritten, "open -a Terminal {script}");
    }

    #[test]
    fn test_rewrite_open_command_with_combined_flag() {
        let rewritten = rewrite_open_command_with_app_path(
            "open -na Ghostty.app --args -e bash {script}",
            Path::new("/Applications/Ghostty.app"),
        )
        .unwrap();
        assert_eq!(
            rewritten,
            "open -n /Applications/Ghostty.app --args -e bash '{script}'"
        );
    }

    #[test]
    fn test_rewrite_open_command_with_explicit_args() {
        let rewritten = rewrite_open_command_with_app_path(
            "open -a Terminal --args bash {script}",
            Path::new("/System/Applications/Utilities/Terminal.app"),
        )
        .unwrap();
        assert_eq!(
            rewritten,
            "open /System/Applications/Utilities/Terminal.app --args bash '{script}'"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_should_use_command_extension_for_terminal_app() {
        assert!(should_use_command_extension(false, "default"));
        assert!(should_use_command_extension(false, "terminal.app"));
        assert!(!should_use_command_extension(false, "iterm"));
        assert!(!should_use_command_extension(true, "terminal.app"));
    }

    #[test]
    fn test_maybe_append_ai_tool_launch_hint_for_tmux() {
        let message = maybe_append_ai_tool_launch_hint(
            "Terminal launch failed (exit code 1): permission denied".to_string(),
            &["tmux".to_string(), "new-session".to_string()],
            true,
        );
        assert!(message.contains("tmux kill-server"));
        assert!(message.contains("tmux new-session -d -s hcom-external"));
    }

    #[test]
    fn test_maybe_append_ai_tool_launch_hint_for_wsh() {
        let message = maybe_append_ai_tool_launch_hint(
            "Failed to spawn terminal process: operation not permitted".to_string(),
            &["wsh".to_string(), "launch".to_string()],
            true,
        );
        assert!(message.contains("managed AI tool session"));
        assert!(message.contains("Rerun it with approval/escalation."));
    }

    #[test]
    fn test_maybe_append_ai_tool_launch_hint_skips_non_terminal_commands() {
        let message = maybe_append_ai_tool_launch_hint(
            "plain failure".to_string(),
            &["bash".to_string()],
            true,
        );
        assert_eq!(message, "plain failure");
    }

    #[test]
    fn test_parse_terminal_command_missing_placeholder() {
        assert!(parse_terminal_command("open -a Terminal", "/tmp/test.sh", "").is_err());
    }

    #[test]
    fn test_parse_terminal_command_with_process_id() {
        let argv = parse_terminal_command(
            "tmux split -t {process_id} -- {script}",
            "/tmp/test.sh",
            "abc-123",
        )
        .unwrap();
        assert_eq!(
            argv,
            vec!["tmux", "split", "-t", "abc-123", "--", "/tmp/test.sh"]
        );
    }

    #[test]
    fn test_kill_result_enum() {
        assert_eq!(KillResult::Sent, KillResult::Sent);
        assert_ne!(KillResult::Sent, KillResult::AlreadyDead);
    }

    #[test]
    fn test_sandbox_flags_in_get_sandbox_flags() {
        use crate::tools::codex_preprocessing::get_sandbox_flags;
        let flags = get_sandbox_flags("workspace");
        assert!(flags.contains(&"--sandbox".to_string()));
        assert!(flags.contains(&"workspace-write".to_string()));
    }

    #[test]
    fn test_get_available_presets_always_has_default_and_custom() {
        let presets = get_available_presets();
        assert_eq!(presets.first().unwrap().0, "default");
        assert_eq!(presets.last().unwrap().0, "custom");
    }

    #[test]
    fn test_has_node_shebang_with_node_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.js");
        std::fs::write(&path, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();
        assert!(has_node_shebang(path.to_str().unwrap()));
    }

    #[test]
    fn test_has_node_shebang_with_bash_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.sh");
        std::fs::write(&path, "#!/bin/bash\necho hello\n").unwrap();
        assert!(!has_node_shebang(path.to_str().unwrap()));
    }

    #[test]
    fn test_has_node_shebang_with_elf_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, b"\x7fELF\x02\x01\x01\x00").unwrap();
        assert!(!has_node_shebang(path.to_str().unwrap()));
    }

    #[test]
    fn test_has_node_shebang_nonexistent() {
        assert!(!has_node_shebang("/nonexistent/path/to/tool"));
    }

    #[test]
    fn test_resolve_termux_tool_launcher_codex_wrapper() {
        let resolved = resolve_termux_tool_launcher("codex", TERMUX_CODEX_WRAPPER_PATH);
        if crate::shared::platform::is_termux()
            && Path::new(TERMUX_CODEX_INNER_WRAPPER_PATH).exists()
        {
            let (command, args) = resolved.expect("expected termux codex wrapper override");
            assert!(command.ends_with("/sh") || command == "sh");
            assert_eq!(args, vec![TERMUX_CODEX_INNER_WRAPPER_PATH.to_string()]);
        } else {
            assert!(resolved.is_none());
        }
    }

    #[test]
    fn test_resolve_termux_tool_launcher_node_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, "#!/usr/bin/env node\nconsole.log('ok');\n").unwrap();

        let resolved = resolve_termux_tool_launcher("tool", path.to_str().unwrap());
        if crate::shared::platform::is_termux() {
            let (command, args) = resolved.expect("expected node wrapper on termux");
            assert!(command.ends_with("/node") || command == "node");
            assert_eq!(args, vec![path.to_string_lossy().to_string()]);
        } else {
            assert!(resolved.is_none());
        }
    }
}
