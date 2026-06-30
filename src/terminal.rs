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
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::paths;
use crate::shared::constants::HCOM_IDENTITY_VARS;
use crate::shared::platform;
use crate::shared::terminal_presets::{ArgvTemplate, TERMINAL_ENV_MAP};
use crate::shared::tool_detection::tool_marker_vars;

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
    pub zellij_session_name: String,
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
const MACOS_APP_FALLBACKS: &[(&str, ArgvTemplate)] = &[
    (
        "kitty-window",
        &["open", "-n", "-a", "kitty.app", "--args", "{script}"],
    ),
    (
        "wezterm-window",
        &[
            "open",
            "-n",
            "-a",
            "WezTerm.app",
            "--args",
            "start",
            "--",
            "bash",
            "{script}",
        ],
    ),
    (
        "alacritty",
        &[
            "open",
            "-n",
            "-a",
            "Alacritty.app",
            "--args",
            "-e",
            "bash",
            "{script}",
        ],
    ),
];

/// Terminal context vars stripped from the env before spawning a terminal launcher subprocess.
/// Prevents outer terminal identity from leaking into newly-launched terminal panes.
/// Must stay in sync with every env var read by detect_terminal_from_env().
pub(crate) const TERMINAL_CONTEXT_VARS: &[&str] = &[
    // Multiplexers
    "CMUX_WORKSPACE_ID",
    "CMUX_SURFACE_ID",
    "TMUX_PANE",
    "ZELLIJ_PANE_ID",
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
    // HERDR_* not stripped: the herdr preset's CLI (`herdr agent start ...`)
    // resolves its server socket from $HERDR_SOCKET_PATH; other presets pass
    // the socket via CLI flag (e.g. `kitty @ --to {kitty_listen}`) and don't
    // need their identity vars to survive in the launcher env.
    // Generic terminal identity
    "TERM_PROGRAM",
    "TERM_SESSION_ID",
];

/// Terminal color/capability vars should come from the terminal/PTY that hosts
/// the child. Forwarding a parent's `TERM=dumb`, test-harness `NO_COLOR`, or
/// stale truecolor state into a new kitty pane can make otherwise-capable tools
/// render in black and white.
pub(crate) const TERMINAL_COLOR_VARS: &[&str] = &[
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "FORCE_COLOR",
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
    if let Some(presets_val) = crate::config::load_toml_presets(&toml_path)
        && let Some(table) = presets_val.as_table()
    {
        for (name, val) in table {
            if let Some(env_var) = val.get("pane_id_env").and_then(|v| v.as_str())
                && std::env::var(env_var)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .is_some()
            {
                return Some(name.clone());
            }
        }
    }
    // TERM_PROGRAM value-based detection (terminals without a unique env var)
    if let Ok(term_prog) = std::env::var("TERM_PROGRAM") {
        match term_prog.as_str() {
            "ghostty" => return Some("ghostty".to_string()),
            "iTerm.app" => return Some("iterm".to_string()),
            "Apple_Terminal" => return Some("terminal.app".to_string()),
            "WarpTerminal" => return Some("warp".to_string()),
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

/// Replace `open -a <app>` app names with absolute `.app` bundle paths, in place
/// on an argv vector.
///
/// This is only safe for app-launch commands where `open` passes argv via
/// `--args`. Plain file-open forms like `open -a Terminal {script}` must keep
/// `-a`, otherwise `open` treats the app bundle and script as regular paths and
/// falls back to file association for the script. No-ops if no `--args` tail is
/// present or no app flag is found.
fn rewrite_open_argv_with_app_path(argv: &mut Vec<String>, app_path: &Path) {
    for idx in 0..argv.len().saturating_sub(1) {
        let flag = &argv[idx];
        let takes_app_arg = flag == "-a"
            || (flag.starts_with('-')
                && !flag.starts_with("--")
                && flag.chars().skip(1).any(|c| c == 'a'));
        if takes_app_arg {
            let has_args_tail = argv.iter().skip(idx + 2).any(|part| part == "--args");
            if !has_args_tail {
                return;
            }
            let app_path_str = app_path.to_string_lossy().to_string();
            if flag == "-a" {
                argv.remove(idx);
                argv[idx] = app_path_str;
            } else {
                let mut rewritten_flag = String::from("-");
                for ch in flag.chars().skip(1) {
                    if ch != 'a' {
                        rewritten_flag.push(ch);
                    }
                }
                if rewritten_flag == "-" {
                    argv.remove(idx);
                    argv[idx] = app_path_str;
                } else {
                    argv[idx] = rewritten_flag;
                    argv[idx + 1] = app_path_str;
                }
            }
            return;
        }
    }
}

fn rewrite_macos_open_app_argv(argv: &mut Vec<String>, app_name: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    if let Some(app_path) = find_macos_app(app_name) {
        rewrite_open_argv_with_app_path(argv, &app_path);
    }
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
    if cfg!(target_os = "macos")
        && let Some(app) = find_macos_app("kitty")
    {
        let full = app.join("Contents/MacOS/kitten");
        if full.exists() {
            return Some(full.to_string_lossy().to_string());
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
        // Skip anything that isn't a Unix-domain socket
        if !crate::sys::fs::is_socket(sock_path) {
            continue;
        }

        let socket_uri = format!("unix:{}", sock_path.display());
        if let Ok(output) = Command::new(&kitten)
            .args(["@", "--to", &socket_uri, "ls"])
            .output()
            && output.status.success()
        {
            return socket_uri;
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
        if terminal_mode == "default"
            && let Some(detected) = detect_terminal_from_env()
        {
            terminal_mode = detected;
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

/// Candidate file names to probe for `name` in a PATH directory. On Windows an
/// extension-less name is expanded with PATHEXT (`.exe`, `.cmd`, …); elsewhere
/// the name is used verbatim.
fn which_candidates(dir: &Path, name: &str) -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        if Path::new(name).extension().is_some() {
            return vec![dir.join(name)];
        }
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut out: Vec<std::path::PathBuf> = exts
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|ext| dir.join(format!("{name}{ext}")))
            .collect();
        out.push(dir.join(name));
        out
    }
    #[cfg(not(windows))]
    {
        vec![dir.join(name)]
    }
}

/// Simple `which` implementation — find binary in PATH.
pub fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    // `split_paths` uses the platform separator (`;` on Windows, `:` elsewhere),
    // which also avoids splitting Windows drive letters like `C:`.
    for dir in std::env::split_paths(&path_var) {
        for candidate in which_candidates(&dir, name) {
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
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
            "kilo" => &[home.join(".kilo").join("bin").join("kilo")],
            "cursor-agent" => &[home.join(".local").join("bin").join("cursor-agent")],
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
const TERMUX_PREFIX: &str = "/data/data/com.termux/files/usr";

/// Whether the host Termux installation is visible from this process.
///
/// This is intentionally broader than `is_native_termux_runtime()`: PRoot
/// exposes host Termux paths and environment variables inside the distro.
fn termux_host_visible() -> bool {
    platform::is_termux()
}

fn native_termux_prefix() -> bool {
    std::env::var("PREFIX")
        .ok()
        .is_some_and(|prefix| Path::new(&prefix) == Path::new(TERMUX_PREFIX))
}

fn proc_self_root_is_root() -> bool {
    fs::read_link("/proc/self/root").is_ok_and(|root| root == Path::new("/"))
}

fn is_native_termux_runtime_from(
    is_android_target: bool,
    host_visible: bool,
    has_native_prefix: bool,
    self_root_is_root: bool,
) -> bool {
    is_android_target && host_visible && has_native_prefix && self_root_is_root
}

/// Whether this process can safely dispatch scripts into native Termux.
fn is_native_termux_runtime() -> bool {
    is_native_termux_runtime_from(
        cfg!(target_os = "android"),
        termux_host_visible(),
        native_termux_prefix(),
        proc_self_root_is_root(),
    )
}

fn proot_termux_launch_error() -> &'static str {
    "Cannot open a native Termux window from this PRoot runtime:\n\
     the generated command uses PRoot paths and Linux/glibc binaries that cannot run\n\
     in host Termux.\n\n\
     Use one of:\n\
       hcom <tool> --headless\n\
       hcom <tool> --run-here\n\
       hcom <tool> --terminal tmux"
}

fn validate_termux_dispatch_status(status: std::process::ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("Termux RUN_COMMAND dispatch failed: {status}")
    }
}

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
    if !is_native_termux_runtime() {
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

/// Resolve preset name to an open-command argv template (placeholders intact).
///
/// On macOS, if CLI binary isn't in PATH but .app bundle exists,
/// uses a hardcoded fallback or substitutes the full binary path. The returned
/// argv still contains placeholders like `{script}`; substitute via
/// `substitute_open_argv`.
pub fn resolve_terminal_open_argv(preset_name: &str) -> Option<Vec<String>> {
    let merged = crate::config::get_merged_preset(preset_name)?;
    let mut open_argv = merged.open_argv(cfg!(windows));
    let app_name = merged.app_name.as_deref().unwrap_or(preset_name);

    if let Some(ref binary) = merged.binary
        && which_bin(binary).is_none()
        && cfg!(target_os = "macos")
    {
        // New-window presets have hardcoded fallbacks using `open -a`
        for &(name, fallback) in MACOS_APP_FALLBACKS {
            if name == preset_name && find_macos_app(app_name).is_some() {
                let mut argv: Vec<String> = fallback.iter().map(|s| s.to_string()).collect();
                rewrite_macos_open_app_argv(&mut argv, app_name);
                return Some(argv);
            }
        }
        // Tab/split presets: substitute leading binary element with full path
        if let Some(full_path) = resolve_binary_path(binary, Some(app_name), preset_name)
            && open_argv.first().map(String::as_str) == Some(binary.as_str())
        {
            open_argv[0] = full_path;
        }
    }

    rewrite_macos_open_app_argv(&mut open_argv, app_name);
    Some(open_argv)
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
    if let Some(presets_val) = crate::config::load_toml_presets(&toml_path)
        && let Some(table) = presets_val.as_table()
    {
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
    } else if format_type == "powershell" {
        valid
            .iter()
            .map(|(k, v)| format!("$env:{} = {}", k, ps_quote(v)))
            .collect::<Vec<_>>()
            .join("\n")
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

/// Resolve a human-readable tool name for the launch script title/banner,
/// detecting from the command when not explicitly provided.
fn launch_display_name<'a>(command_str: &str, tool_name: Option<&'a str>) -> &'a str {
    tool_name.unwrap_or_else(|| {
        let cmd_lower = command_str.to_lowercase();
        if cmd_lower.contains("opencode") {
            "OpenCode"
        } else if cmd_lower.contains("kilo") {
            "Kilo Code"
        } else if cmd_lower.contains("cursor-agent") {
            "Cursor Agent"
        } else if cmd_lower.contains("gemini") {
            "Gemini"
        } else if cmd_lower.contains("codex") {
            "Codex"
        } else if cmd_lower.contains("claude") {
            "Claude Code"
        } else {
            "hcom"
        }
    })
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
    let tool_name = launch_display_name(command_str, tool_name);

    let mut f = fs::File::create(script_file).context("Failed to create script file")?;

    writeln!(f, "#!/bin/bash")?;
    writeln!(f, "printf \"\\033]0;hcom: starting {}...\\007\"", tool_name)?;
    writeln!(f, "echo \"Starting {}...\"", tool_name)?;

    // Unset tool markers and identity vars to prevent inheritance
    writeln!(f, "unset {}", tool_marker_vars().join(" "))?;
    writeln!(f, "unset {}", HCOM_IDENTITY_VARS.join(" "))?;

    // Discover paths for minimal environments (kitty splits, etc.)
    let mut paths_to_add: Vec<String> = Vec::new();

    fn add_path(paths: &mut Vec<String>, binary_path: Option<String>) {
        if let Some(bp) = binary_path
            && let Some(dir) = Path::new(&bp).parent()
        {
            let dir_str = dir.to_string_lossy().to_string();
            if !paths.contains(&dir_str) {
                paths.push(dir_str);
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
    if !tool_cmd.is_empty()
        && let Some(tool_path) = which_bin(tool_cmd)
    {
        if let Some((launcher, prefix_args)) = resolve_termux_tool_launcher(tool_cmd, &tool_path) {
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

    writeln!(f, "{}", final_command)?;

    if opens_new_window {
        // Clear hcom state from the interactive shell left open after the tool
        // exits. Derive from HCOM_IDENTITY_VARS (so new identity/batch vars are
        // covered automatically) plus the non-identity per-launch vars exported
        // above that aren't in that list.
        let mut leftover_vars: Vec<&str> = HCOM_IDENTITY_VARS.to_vec();
        leftover_vars.extend(["HCOM_TAG", "HCOM_CODEX_SANDBOX_MODE"]);
        writeln!(f, "unset {}", leftover_vars.join(" "))?;
        writeln!(f, "rm -f {}", shell_quote(&script_file.to_string_lossy()))?;
        writeln!(f, "exec bash -l")?;
    } else if !background {
        writeln!(f, "hcom_status=$?")?;
        writeln!(f, "rm -f {}", shell_quote(&script_file.to_string_lossy()))?;
        writeln!(f, "exit $hcom_status")?;
    }

    // Make executable
    crate::sys::fs::set_executable(script_file)?;

    Ok(())
}

/// Quote a string as a PowerShell single-quoted literal (embedded `'` doubled).
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Build sorted `$env:K = 'V'` assignments, applying the same key validation
/// as `build_env_string` so only well-formed names are emitted.
fn ps_env_assignments(env_vars: &HashMap<String, String>) -> Vec<String> {
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
    valid
        .iter()
        .map(|(k, v)| format!("$env:{} = {}", k, ps_quote(v)))
        .collect()
}

/// Create a PowerShell launch script — the Windows-native equivalent of
/// `create_bash_script`. Emits a `.ps1` that sets the window title, scrubs
/// inherited tool/identity vars, prepends discovered tool directories to PATH,
/// assigns the per-launch env, changes directory, then runs the tool command
/// via the call operator. The window-open vs. run-once cleanup mirrors the
/// bash version; the window is kept alive by launching with `powershell -NoExit`.
pub fn create_powershell_script(
    script_file: &Path,
    env: &HashMap<String, String>,
    cwd: Option<&str>,
    command_str: &str,
    background: bool,
    tool_name: Option<&str>,
    opens_new_window: bool,
) -> Result<()> {
    let tool_name = launch_display_name(command_str, tool_name);

    let mut f = fs::File::create(script_file).context("Failed to create script file")?;

    writeln!(
        f,
        "$Host.UI.RawUI.WindowTitle = \"hcom: starting {}...\"",
        tool_name
    )?;
    writeln!(f, "Write-Host \"Starting {}...\"", tool_name)?;

    // Scrub inherited tool markers and identity vars so the child can't inherit
    // them (PowerShell ignores Env: entries that don't exist).
    let scrub: Vec<String> = tool_marker_vars()
        .iter()
        .chain(HCOM_IDENTITY_VARS.iter())
        .map(|v| format!("Env:{v}"))
        .collect();
    writeln!(
        f,
        "Remove-Item {} -ErrorAction SilentlyContinue",
        scrub.join(",")
    )?;

    // Discover paths for minimal environments.
    let mut paths_to_add: Vec<String> = Vec::new();

    fn add_path(paths: &mut Vec<String>, binary_path: Option<String>) {
        if let Some(bp) = binary_path
            && let Some(dir) = Path::new(&bp).parent()
        {
            let dir_str = dir.to_string_lossy().to_string();
            if !paths.contains(&dir_str) {
                paths.push(dir_str);
            }
        }
    }

    add_path(&mut paths_to_add, which_bin("hcom"));
    add_path(&mut paths_to_add, which_bin("python3"));
    let cmd_stripped = command_str.trim_start();
    let tool_cmd = cmd_stripped.split_whitespace().next().unwrap_or("");
    add_path(&mut paths_to_add, which_bin(tool_cmd));
    if tool_cmd == "claude" {
        add_path(&mut paths_to_add, which_bin("node"));
    }

    if !paths_to_add.is_empty() {
        // Windows PATH is `;`-separated.
        let prefix = format!("{};", paths_to_add.join(";"));
        writeln!(f, "$env:PATH = {} + $env:PATH", ps_quote(&prefix))?;
    }

    for line in ps_env_assignments(env) {
        writeln!(f, "{line}")?;
    }

    if let Some(dir) = cwd {
        writeln!(f, "Set-Location {}", ps_quote(dir))?;
    }

    // Resolve the tool to a full path and invoke it through the call operator so
    // a quoted path runs as a command. If the tool isn't found, fall through to
    // the bare command name (resolved via the PATH we just prepended).
    let mut final_command = command_str.to_string();
    if !tool_cmd.is_empty()
        && let Some(tool_path) = which_bin(tool_cmd)
    {
        let replaced = final_command.replacen(
            &format!("{tool_cmd} "),
            &format!("& {} ", ps_quote(&tool_path)),
            1,
        );
        final_command = if replaced != final_command {
            replaced
        } else {
            // No arguments: the command is exactly the tool name (no trailing
            // space to match), so replace the bare name directly.
            final_command.replacen(tool_cmd, &format!("& {}", ps_quote(&tool_path)), 1)
        };
    }

    writeln!(f, "{final_command}")?;

    if opens_new_window {
        // Clear hcom state from the interactive shell left open after the tool
        // exits (window persists via `powershell -NoExit`).
        let mut leftover_vars: Vec<&str> = HCOM_IDENTITY_VARS.to_vec();
        leftover_vars.extend(["HCOM_TAG", "HCOM_CODEX_SANDBOX_MODE"]);
        let leftover: Vec<String> = leftover_vars.iter().map(|v| format!("Env:{v}")).collect();
        writeln!(
            f,
            "Remove-Item {} -ErrorAction SilentlyContinue",
            leftover.join(",")
        )?;
        writeln!(
            f,
            "Remove-Item -Force -ErrorAction SilentlyContinue {}",
            ps_quote(&script_file.to_string_lossy())
        )?;
    } else if !background {
        writeln!(f, "$hcom_status = $LASTEXITCODE")?;
        writeln!(
            f,
            "Remove-Item -Force -ErrorAction SilentlyContinue {}",
            ps_quote(&script_file.to_string_lossy())
        )?;
        writeln!(f, "exit $hcom_status")?;
    }

    Ok(())
}

/// Build clean env for terminal launcher subprocesses.
///
/// Strips AI tool markers, hcom identity vars, and terminal context vars.
fn get_launcher_env() -> HashMap<String, String> {
    get_launcher_env_from(std::env::vars())
}

fn get_launcher_env_from<I>(vars: I) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut strip: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for v in tool_marker_vars() {
        strip.insert(v);
    }
    for v in HCOM_IDENTITY_VARS {
        strip.insert(v);
    }
    for v in TERMINAL_CONTEXT_VARS {
        strip.insert(v);
    }
    strip.insert("HCOM_LAUNCHED_PRESET");

    vars.into_iter()
        .filter(|(k, _)| !strip.contains(k.as_str()))
        .collect()
}

/// Inputs to terminal command template substitution.
///
/// All fields are borrowed and may be empty; unknown placeholders are left
/// in place by `substitute_open_argv` (no substitution panics).
#[derive(Default, Clone, Copy)]
pub(crate) struct TerminalCommandContext<'a> {
    pub script: &'a str,
    pub process_id: &'a str,
    pub cwd: &'a str,
    pub instance_name: &'a str,
    pub tool: &'a str,
    /// Pre-formatted pane title (e.g. `◉ team-luna [claude]`). Falls back to
    /// `instance_name` when None or empty.
    pub pane_title: Option<&'a str>,
}

/// Substitute placeholders into an open-command argv template, per element.
///
/// Each element of `template` is one argument (no shell splitting). Placeholders
/// are replaced inside each element with `String::replace`, so a Windows path
/// like `C:\Users\x\s.ps1` substituted into the `{script}` element survives
/// intact (no backslash mangling, no re-quoting). Requires at least one element
/// to contain `{script}`.
fn substitute_open_argv(
    template: &[String],
    ctx: TerminalCommandContext<'_>,
) -> Result<Vec<String>> {
    if !template.iter().any(|p| p.contains("{script}")) {
        bail!(
            "Custom terminal command must include {{script}} placeholder\n\
             Example: open -n -a kitty.app --args bash \"{{script}}\""
        );
    }

    let pane_title = ctx
        .pane_title
        .filter(|s| !s.is_empty())
        .unwrap_or(ctx.instance_name);

    let replaced: Vec<String> = template
        .iter()
        .map(|part| {
            let mut part = part.clone();
            for (placeholder, value) in [
                ("{process_id}", ctx.process_id),
                ("{cwd}", ctx.cwd),
                ("{instance_name}", ctx.instance_name),
                ("{tool}", ctx.tool),
                ("{pane_title}", pane_title),
                ("{script}", ctx.script),
            ] {
                if part.contains(placeholder) {
                    part = part.replace(placeholder, value);
                }
            }
            part
        })
        .collect();

    Ok(replaced)
}

/// Substitute placeholders into a close-command argv template, per element.
///
/// Mirrors the open path but for close placeholders. Returns `None` (caller
/// treats as "skip the close") when a required placeholder appears in the
/// template but the corresponding value is empty — preserving the previous
/// `close_terminal_pane` skip semantics. `effective_pane_id` is the resolved
/// pane id (caller falls back from `pane_id` to `terminal_id`).
fn substitute_close_argv(
    template: &[String],
    pid: u32,
    effective_pane_id: &str,
    process_id: &str,
    terminal_id: &str,
) -> Option<Vec<String>> {
    let needs = |ph: &str| template.iter().any(|p| p.contains(ph));

    if needs("{pane_id}") && effective_pane_id.is_empty() {
        return None;
    }
    if needs("{process_id}") && process_id.is_empty() {
        return None;
    }
    if needs("{id}") && terminal_id.is_empty() {
        return None;
    }

    let pid_str = pid.to_string();
    let argv: Vec<String> = template
        .iter()
        .map(|part| {
            let mut part = part.clone();
            for (placeholder, value) in [
                ("{pid}", pid_str.as_str()),
                ("{pane_id}", effective_pane_id),
                ("{process_id}", process_id),
                ("{id}", terminal_id),
            ] {
                if part.contains(placeholder) {
                    part = part.replace(placeholder, value);
                }
            }
            part
        })
        .collect();

    Some(argv)
}

/// Get macOS Terminal.app launch argv ({script} substituted by the caller).
fn get_macos_terminal_argv() -> Vec<String> {
    let mut argv: Vec<String> = ["open", "-a", "Terminal", "{script}"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    rewrite_macos_open_app_argv(&mut argv, "Terminal");
    argv
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
    if let Some(c) = cwd
        && Path::new(c).is_absolute()
    {
        return c.to_string();
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
                ["cmd.exe", "/c", "start", "wt.exe", "--", "bash", "{script}"]
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

/// Default Windows terminal launch argv ({script} substituted by the caller).
///
/// Prefers Windows Terminal, which parses everything after `--` as a literal
/// argv so a script path with spaces stays a single argument. Without it, opens
/// a fresh console via cmd's `start` (the empty arg is start's window-title
/// slot; the path is quoted so start keeps spaces together). Either way the
/// shell runs the generated `.ps1` with the execution policy bypassed and stays
/// open (`-NoExit`) for the new window.
fn get_windows_terminal_argv() -> Vec<String> {
    let to_vec = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    if which_bin("wt").is_some() {
        return to_vec(&[
            "wt",
            "--",
            "powershell",
            "-ExecutionPolicy",
            "Bypass",
            "-NoExit",
            "-File",
            "{script}",
        ]);
    }
    to_vec(&[
        "cmd",
        "/c",
        "start",
        "",
        "powershell",
        "-ExecutionPolicy",
        "Bypass",
        "-NoExit",
        "-File",
        "\"{script}\"",
    ])
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
            | "herdr"
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

fn zellij_action_stderr_failure(argv: &[String], stderr: &str) -> Option<String> {
    if argv.first().map(|s| s.as_str()) != Some("zellij") {
        return None;
    }

    let stderr = stderr.trim();
    if stderr.contains("Please specify the session name to send actions to") {
        return Some(stderr.to_string());
    }

    None
}

pub fn is_zellij_preset(preset_name: &str) -> bool {
    if preset_name == "zellij" {
        return true;
    }

    let is_zellij_argv0 = |argv: &[String]| argv.first().map(String::as_str) == Some("zellij");

    crate::config::get_merged_preset(preset_name).is_some_and(|preset| {
        preset.binary.as_deref() == Some("zellij")
            || is_zellij_argv0(&preset.open)
            || preset.open_windows.as_deref().is_some_and(is_zellij_argv0)
            || preset.close.as_deref().is_some_and(is_zellij_argv0)
            || preset.close_windows.as_deref().is_some_and(is_zellij_argv0)
    })
}

fn validate_terminal_launch_output(
    argv: &[String],
    output: &std::process::Output,
    inside_ai_tool: bool,
) -> Result<()> {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
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

    if let Some(msg) = zellij_action_stderr_failure(argv, &stderr) {
        bail!(maybe_append_ai_tool_launch_hint(
            format!("Terminal launch failed: {msg}"),
            argv,
            inside_ai_tool
        ));
    }

    Ok(())
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

        validate_terminal_launch_output(argv, &output, inside_ai_tool)?;

        Ok((true, captured))
    } else {
        // Normal case: wait for terminal launcher to complete
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .envs(env_vec.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .context("Failed to run terminal launcher")?;

        validate_terminal_launch_output(argv, &output, inside_ai_tool)?;

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
    let captured_id = normalize_captured_terminal_id(captured_id);
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

fn normalize_captured_terminal_id(captured_id: &str) -> String {
    let captured_id = captured_id.trim();

    // Herdr: parse JSON response and extract result.agent.pane_id
    if let Some(pane_id) = parse_herdr_pane_id(captured_id) {
        return pane_id;
    }

    // Waveterm: "run block created: block:abc123" -> "block:abc123"
    let Some((_, block_ref)) = captured_id.rsplit_once("block:") else {
        return captured_id.to_string();
    };
    let block_id = block_ref.split_whitespace().next().unwrap_or("");
    if block_id.is_empty() {
        captured_id.to_string()
    } else {
        format!("block:{block_id}")
    }
}

/// Parse herdr `agent start` JSON output and extract `result.agent.pane_id`.
///
/// Gated on the response `id` field (`cli:agent:start`) so non-herdr terminal
/// outputs that happen to be JSON don't accidentally match this shape.
fn parse_herdr_pane_id(captured: &str) -> Option<String> {
    let trimmed = captured.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if val.get("id").and_then(|v| v.as_str()) != Some("cli:agent:start") {
        return None;
    }
    val.get("result")?
        .get("agent")?
        .get("pane_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
    let extension = if cfg!(windows) {
        ".ps1"
    } else if should_use_command_extension(background, &terminal_mode) {
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

    // Create script. Windows uses a native PowerShell script; Unix uses bash.
    if cfg!(windows) {
        create_powershell_script(
            &script_file,
            &final_env,
            cwd,
            command,
            background,
            None,
            opens_new_window,
        )?;
    } else {
        create_bash_script(
            &script_file,
            &final_env,
            cwd,
            command,
            background,
            None,
            opens_new_window,
        )?;
    }

    // Background mode
    if background {
        let logs_dir = paths::hcom_path(&[paths::LOGS_DIR]);
        fs::create_dir_all(&logs_dir).ok();
        let log_name = env.get("HCOM_BACKGROUND").cloned().unwrap_or_default();
        let log_file = logs_dir.join(&log_name);

        let log_handle = fs::File::create(&log_file).context("Failed to create log file")?;

        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-ExecutionPolicy", "Bypass", "-File"]);
            c
        } else {
            Command::new("bash")
        };
        cmd.arg(&script_file)
            .stdin(std::process::Stdio::null())
            .stdout(log_handle.try_clone()?)
            .stderr(log_handle);

        // Detach child into its own session so it survives parent exit (no SIGHUP)
        crate::sys::process::detach_session(&mut cmd);

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
        // Replace this process entirely with the script's shell.
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-ExecutionPolicy", "Bypass", "-File"]);
            c
        } else {
            Command::new(which_bin("bash").unwrap_or_else(|| "/bin/bash".to_string()))
        };
        cmd.arg(script_file).env_clear().envs(&full_env);
        let err = crate::sys::process::exec_replace(cmd);
        bail!("exec failed: {}", err);
    }

    // New window / custom command mode
    let custom_cmd: Option<Vec<String>> = if terminal_mode == "default" {
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
        let mut argv = resolve_terminal_open_argv(&terminal_mode).unwrap_or_default();
        // Inject `--to <socket>` (after the `@`) for kitten commands launched
        // outside kitty. Splice as separate argv elements — no shell quoting.
        if !kitty_socket.is_empty()
            && argv.first().map(String::as_str) == Some("kitten")
            && argv.get(1).map(String::as_str) == Some("@")
            && !argv.iter().any(|a| a == "--to")
        {
            argv.splice(2..2, ["--to".to_string(), kitty_socket.clone()]);
        }
        // Target launcher's tab for splits: insert `--match window_id:<wid>`
        // before the `--` separator.
        if (terminal_mode == "kitty-tab" || terminal_mode == "kitty-split")
            && let Ok(wid) = std::env::var("KITTY_WINDOW_ID")
            && !wid.is_empty()
            && let Some(sep) = argv.iter().position(|a| a == "--")
        {
            argv.splice(
                sep..sep,
                ["--match".to_string(), format!("window_id:{wid}")],
            );
        }
        Some(argv)
    } else {
        // Custom command template string (HCOM_TERMINAL / config custom command).
        // Tokenize once via the double-quote-aware splitter; the array-form TOML
        // preset path never reaches here (those are known presets).
        match crate::tools::args_common::shell_split(&terminal_mode) {
            Ok(argv) if !argv.is_empty() => Some(argv),
            Ok(_) => bail!("custom terminal command is empty"),
            Err(e) => bail!("invalid quoting in custom terminal command: {e}"),
        }
    };

    let script_str = script_file.to_string_lossy().to_string();

    if terminal_mode == "warp" {
        let process_id = env.get("HCOM_PROCESS_ID").map(|s| s.as_str()).unwrap_or("");
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
        // {instance_name} falls back to process_id so presets that label panes
        // (e.g. herdr) never produce an empty `[tool]-` suffix when invoked
        // outside the normal launch flow.
        let process_id = env.get("HCOM_PROCESS_ID").map(|s| s.as_str()).unwrap_or("");
        let instance_name = env
            .get("HCOM_INSTANCE_NAME")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(process_id);
        let ctx = TerminalCommandContext {
            script: &script_str,
            process_id,
            cwd: cwd.unwrap_or(""),
            instance_name,
            tool: env
                .get("HCOM_TOOL")
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("hcom"),
            // launcher::launch pre-formats the title; here we only read it.
            pane_title: env
                .get("HCOM_PANE_TITLE")
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty()),
        };
        // The Windows `.ps1`-via-PowerShell variant is already selected by the
        // preset's `open_argv(cfg!(windows))`; no text rewrite needed.
        let final_argv = substitute_open_argv(&cmd_template, ctx)?;
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
        if termux_host_visible() {
            if !is_native_termux_runtime() {
                bail!("{}", proot_termux_launch_error());
            }

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
            let status = Command::new(am_argv[0])
                .args(&am_argv[1..])
                .status()
                .context("Failed to launch Termux")?;
            validate_termux_dispatch_status(status)?;
            return Ok((LaunchResult::Success, terminal_mode));
        }

        let argv = match platform::platform_name() {
            "Darwin" => substitute_open_argv(
                &get_macos_terminal_argv(),
                TerminalCommandContext {
                    script: &script_str,
                    process_id: env.get("HCOM_PROCESS_ID").map(|s| s.as_str()).unwrap_or(""),
                    cwd: cwd.unwrap_or(""),
                    instance_name: env
                        .get("HCOM_INSTANCE_NAME")
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                    tool: env.get("HCOM_TOOL").map(|s| s.as_str()).unwrap_or(""),
                    pane_title: None,
                },
            )?,
            "Linux" => get_linux_terminal_argv()
                .ok_or_else(|| anyhow::anyhow!("No supported terminal emulator found"))?,
            "Windows" => get_windows_terminal_argv(),
            other => bail!("Unsupported platform: {}", other),
        };

        let final_argv: Vec<String> = if platform::platform_name() == "Darwin" {
            argv
        } else {
            // Linux/Windows defaults carry only `{script}` placeholders.
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
        if tool_marker_vars().contains(&k.as_str()) {
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
    zellij_session_name: &str,
) -> bool {
    let merged = match crate::config::get_merged_preset(preset_name) {
        Some(p) => p,
        None => return false,
    };

    let close_template = match merged.close_argv(cfg!(windows)) {
        Some(c) => c,
        None => return false,
    };

    // Determine effective pane_id (fall back to terminal_id)
    let effective_pane_id = if pane_id.is_empty() && !terminal_id.is_empty() {
        terminal_id
    } else {
        pane_id
    };

    // Substitute close placeholders per-element. Returns None when a required
    // placeholder is present but its value is empty (skip the close).
    let mut argv = match substitute_close_argv(
        &close_template,
        pid,
        effective_pane_id,
        process_id,
        terminal_id,
    ) {
        Some(a) => a,
        None => return false,
    };

    let is_zellij = is_zellij_preset(preset_name);

    let zellij_before_close = if is_zellij {
        match zellij_terminal_pane_exists(zellij_session_name, effective_pane_id) {
            Some(true) => Some(true),
            Some(false) => return false,
            None => None,
        }
    } else {
        None
    };

    // Splice `--session <name>` right after `zellij` for `zellij action ...`.
    if is_zellij
        && !zellij_session_name.is_empty()
        && argv.first().map(String::as_str) == Some("zellij")
        && argv.get(1).map(String::as_str) == Some("action")
    {
        argv.splice(
            1..1,
            ["--session".to_string(), zellij_session_name.to_string()],
        );
    }

    // Resolve binary path via app bundle fallback (replace argv[0]).
    if let Some(ref binary) = merged.binary {
        let app_name = merged.app_name.as_deref().unwrap_or(preset_name);
        if let Some(full_path) = resolve_binary_path(binary, Some(app_name), preset_name)
            && argv.first().map(String::as_str) == Some(binary.as_str())
        {
            argv[0] = full_path;
        }
    }
    if argv.first().map(String::as_str) == Some("kitten")
        && let Some(full_path) = find_kitten_binary()
    {
        argv[0] = full_path;
    }

    // Inject `--to <socket>` (after the `@`) for kitten commands when we have
    // the socket path.
    if argv.first().map(String::as_str) == Some("kitten")
        && argv.get(1).map(String::as_str) == Some("@")
        && !kitty_listen_on.is_empty()
        && !argv.iter().any(|a| a == "--to")
        && !kitty_listen_on.starts_with("fd:")
    {
        argv.splice(2..2, ["--to".to_string(), kitty_listen_on.to_string()]);
    }

    if argv.is_empty() {
        return false;
    }

    // Run the close command directly (no shell) so it works on Windows too.
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();

    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    if is_zellij {
        return zellij_before_close == Some(true)
            && zellij_terminal_pane_exists(zellij_session_name, effective_pane_id) == Some(false);
    }

    true
}

fn zellij_terminal_pane_exists(session_name: &str, pane_id: &str) -> Option<bool> {
    let pane_num = pane_id
        .strip_prefix("terminal_")
        .unwrap_or(pane_id)
        .parse::<i64>()
        .ok()?;

    let mut command = Command::new("zellij");
    if !session_name.is_empty() {
        command.args(["--session", session_name]);
    }
    let output = command
        .args(["action", "list-panes", "--json", "--all"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let panes = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok()?;
    let panes = panes.as_array()?;
    Some(panes.iter().any(|pane| {
        pane.get("is_plugin").and_then(|v| v.as_bool()) == Some(false)
            && pane.get("id").and_then(|v| v.as_i64()) == Some(pane_num)
    }))
}

/// Close terminal pane (if applicable) then SIGTERM the process group.
pub fn kill_process(
    pid: u32,
    preset_name: &str,
    pane_id: &str,
    process_id: &str,
    kitty_listen_on: &str,
    terminal_id: &str,
    zellij_session_name: &str,
) -> (KillResult, bool) {
    let pane_closed = if !preset_name.is_empty() {
        close_terminal_pane(
            pid,
            preset_name,
            pane_id,
            process_id,
            kitty_listen_on,
            terminal_id,
            zellij_session_name,
        )
    } else {
        false
    };

    // SIGTERM the process group
    use crate::sys::process::GroupSignal;
    let kill_result = match crate::sys::process::terminate_group(pid) {
        GroupSignal::Sent => KillResult::Sent,
        GroupSignal::PermissionDenied => KillResult::PermissionDenied,
        GroupSignal::NotFound | GroupSignal::Other => KillResult::AlreadyDead,
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

    if let Some(launch_context_json) = launch_context_json.filter(|s| !s.is_empty())
        && let Ok(lc) = serde_json::from_str::<serde_json::Value>(launch_context_json)
    {
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
        if is_zellij_preset(&info.preset_name)
            && let Some(pane_id) = zellij_pane_id_from_terminal_id(&info.terminal_id)
        {
            info.pane_id = pane_id;
        }
        // Kitty socket from launch context or env snapshot
        let lc_env = lc.get("env").and_then(|v| v.as_object());
        info.kitty_listen_on = lc
            .get("kitty_listen_on")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| lc_env.and_then(|e| e.get("KITTY_LISTEN_ON").and_then(|v| v.as_str())))
            .unwrap_or("")
            .to_string();
        info.zellij_session_name = lc_env
            .and_then(|e| e.get("ZELLIJ_SESSION_NAME").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
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

fn zellij_pane_id_from_terminal_id(terminal_id: &str) -> Option<String> {
    terminal_id
        .strip_prefix("terminal_")
        .filter(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
        .map(|suffix| suffix.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;

    struct EnvGuard(Vec<(&'static str, Option<String>)>);

    impl EnvGuard {
        fn clear(vars: &'static [&'static str]) -> Self {
            let saved = vars
                .iter()
                .map(|&var| (var, std::env::var(var).ok()))
                .collect::<Vec<_>>();
            for &var in vars {
                unsafe {
                    std::env::remove_var(var);
                }
            }
            Self(saved)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (var, value) in &self.0 {
                unsafe {
                    if let Some(value) = value {
                        std::env::set_var(var, value);
                    } else {
                        std::env::remove_var(var);
                    }
                }
            }
        }
    }

    #[test]
    fn test_native_termux_runtime_requires_all_native_signals() {
        assert!(is_native_termux_runtime_from(true, true, true, true));
        assert!(!is_native_termux_runtime_from(false, true, true, true));
        assert!(!is_native_termux_runtime_from(true, false, true, true));
        assert!(!is_native_termux_runtime_from(true, true, false, true));
        assert!(!is_native_termux_runtime_from(true, true, true, false));
    }

    #[test]
    fn test_proot_termux_launch_error_is_actionable() {
        let message = proot_termux_launch_error();
        assert!(message.contains("--headless"));
        assert!(message.contains("--run-here"));
        assert!(message.contains("--terminal tmux"));
    }

    #[test]
    #[cfg(unix)]
    fn test_termux_dispatch_rejects_nonzero_exit_status() {
        let status = std::process::ExitStatus::from_raw(1 << 8);
        let err = validate_termux_dispatch_status(status)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Termux RUN_COMMAND dispatch failed"));
    }

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

    /// Build a `Vec<String>` argv from `&str` literals (test helper).
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
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
    fn test_launcher_env_preserves_zellij_session_but_strips_pane() {
        let env = get_launcher_env_from(vec![
            (
                "ZELLIJ_SESSION_NAME".to_string(),
                "wise-kangaroo".to_string(),
            ),
            ("ZELLIJ_PANE_ID".to_string(), "18".to_string()),
            ("HCOM_LAUNCHED_PRESET".to_string(), "zellij".to_string()),
            ("PATH".to_string(), "/bin".to_string()),
        ]);

        assert_eq!(
            env.get("ZELLIJ_SESSION_NAME").map(String::as_str),
            Some("wise-kangaroo")
        );
        assert!(!env.contains_key("ZELLIJ_PANE_ID"));
        assert!(!env.contains_key("HCOM_LAUNCHED_PRESET"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
    }

    #[test]
    fn test_launcher_env_keeps_herdr_socket_path() {
        // The herdr preset's CLI resolves its socket from env; see the comment
        // on TERMINAL_CONTEXT_VARS for why HERDR_* is not stripped.
        let env = get_launcher_env_from(vec![
            ("HERDR_SOCKET_PATH".into(), "/tmp/herdr.sock".into()),
            ("PATH".into(), "/bin".into()),
        ]);
        assert_eq!(
            env.get("HERDR_SOCKET_PATH").map(String::as_str),
            Some("/tmp/herdr.sock"),
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_zellij_session_ambiguity_stderr_fails_launch_even_with_exit_zero() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: b"Please specify the session name to send actions to. The following sessions are active:\n".to_vec(),
        };

        let err = validate_terminal_launch_output(
            &[
                "zellij".to_string(),
                "action".to_string(),
                "new-pane".to_string(),
            ],
            &output,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("Terminal launch failed"));
        assert!(err.contains("Please specify the session name"));
    }

    #[test]
    fn test_resolve_terminal_info_prefers_zellij_terminal_id_over_env_pane_id() {
        let info = resolve_terminal_info(
            Some("zellij"),
            Some(r#"{"pane_id":"18","terminal_id":"terminal_6","process_id":"proc-1"}"#),
        );

        assert_eq!(info.pane_id, "6");
        assert_eq!(info.terminal_id, "terminal_6");
    }

    #[test]
    fn test_is_zellij_preset_does_not_match_name_prefix_only() {
        assert!(!is_zellij_preset("zellijish"));
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

    // Unix-only: Warp is a macOS terminal and the assertion pins POSIX paths.
    #[cfg(unix)]
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
    fn test_ps_quote_doubles_single_quotes() {
        assert_eq!(ps_quote("plain"), "'plain'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote(""), "''");
    }

    #[test]
    fn test_ps_env_assignments_sorted_and_validated() {
        let mut env = HashMap::new();
        env.insert("ZED".to_string(), "z".to_string());
        env.insert("ABE".to_string(), "a'b".to_string());
        env.insert("1bad".to_string(), "skip".to_string()); // invalid name dropped
        let lines = ps_env_assignments(&env);
        assert_eq!(
            lines,
            vec![
                "$env:ABE = 'a''b'".to_string(),
                "$env:ZED = 'z'".to_string(),
            ]
        );
    }

    #[test]
    fn test_create_powershell_script_window_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("launch.ps1");
        let mut env = HashMap::new();
        env.insert("HCOM_TOOL".to_string(), "claude".to_string());
        create_powershell_script(
            &script,
            &env,
            Some("/work/dir"),
            "claude --foo",
            false, // background
            None,
            true, // opens_new_window
        )
        .unwrap();
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(content.contains("$Host.UI.RawUI.WindowTitle = \"hcom: starting Claude Code...\""));
        assert!(content.contains("Write-Host \"Starting Claude Code...\""));
        assert!(content.contains("Remove-Item Env:"));
        assert!(content.contains("$env:HCOM_TOOL = 'claude'"));
        assert!(content.contains("Set-Location '/work/dir'"));
        // The command args survive whether or not the tool resolved to a full
        // path (bare `claude --foo` or call-operator `& '<path>' --foo`).
        assert!(content.contains("--foo"));
        // Window mode self-deletes but does not `exit` (window persists via -NoExit).
        assert!(content.contains("Remove-Item -Force -ErrorAction SilentlyContinue"));
        assert!(!content.contains("exit $hcom_status"));
    }

    #[test]
    fn test_create_powershell_script_run_once_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("launch.ps1");
        let env = HashMap::new();
        create_powershell_script(
            &script, &env, None, "codex", false, // background
            None, false, // run-once (not a new window)
        )
        .unwrap();
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(content.contains("$hcom_status = $LASTEXITCODE"));
        assert!(content.contains("exit $hcom_status"));
        assert!(!content.contains("Set-Location"));
    }

    #[test]
    fn test_build_env_string_powershell_format() {
        let mut env = HashMap::new();
        env.insert("HCOM_A".to_string(), "x".to_string());
        env.insert("HCOM_B".to_string(), "y'z".to_string());
        let out = build_env_string(&env, "powershell");
        assert_eq!(out, "$env:HCOM_A = 'x'\n$env:HCOM_B = 'y''z'");
    }

    #[test]
    fn test_wezterm_open_argv_selects_powershell_on_windows() {
        // The PowerShell variant is now selected by the preset's PlatformArgv,
        // not a text rewrite. Confirm the merged preset surfaces it.
        let merged = crate::config::get_merged_preset("wezterm").unwrap();
        let win = merged.open_argv(true);
        assert!(win.iter().any(|a| a == "powershell"));
        assert!(win.iter().any(|a| a == "-File"));
        assert!(!win.iter().any(|a| a == "bash"));
        let unix = merged.open_argv(false);
        assert!(unix.iter().any(|a| a == "bash"));
    }

    #[test]
    fn test_mintty_open_argv_has_no_bash() {
        let merged = crate::config::get_merged_preset("mintty").unwrap();
        let argv = merged.open_argv(true);
        assert_eq!(argv.first().map(String::as_str), Some("mintty"));
        assert!(
            !argv.iter().any(|a| a == "bash"),
            "mintty must not hand a .ps1 to bash"
        );
        assert!(argv.iter().any(|a| a == "powershell"));
    }

    // Unix-only: "/abs/path" isn't absolute on Windows (no drive), so it would
    // be rewritten to the current dir.
    #[cfg(unix)]
    #[test]
    fn test_resolve_warp_cwd_keeps_absolute() {
        let home = Path::new("/h");
        assert_eq!(resolve_warp_cwd(Some("/abs/path"), home), "/abs/path");
    }

    #[test]
    #[serial]
    fn test_resolve_warp_cwd_uses_current_dir_for_relative_or_missing() {
        let home = Path::new("/h");
        let cwd_str = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
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
        let open = preset.open.select(false).unwrap();
        assert!(open.contains(&"warp://launch/hcom-{process_id}"));
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

    /// Detection vars not in `TERMINAL_CONTEXT_VARS` — tests that exercise
    /// `detect_terminal_from_env` must clear these explicitly so a host shell
    /// running inside herdr doesn't leak into the test.
    const DETECT_ONLY_VARS: &[&str] = &["HERDR_PANE_ID", "HERDR_SOCKET_PATH", "HERDR_ENV"];

    #[test]
    #[serial]
    fn test_normalize_terminal_mode_for_launch_resolves_socket_for_auto_detected_kitty() {
        let _env = EnvGuard::clear(TERMINAL_CONTEXT_VARS);
        let _detect = EnvGuard::clear(DETECT_ONLY_VARS);
        unsafe {
            std::env::set_var("KITTY_WINDOW_ID", "window-1");
            std::env::set_var("KITTY_LISTEN_ON", "unix:/tmp/kitty-test");
        }

        let (mode, socket) = normalize_terminal_mode_for_launch("default".to_string(), true, false);

        assert_eq!(mode, "kitty-split");
        assert_eq!(socket, "unix:/tmp/kitty-test");
    }

    #[test]
    #[serial]
    fn test_resolve_terminal_mode_for_tips_uses_normalized_auto_detected_mode() {
        let _env = EnvGuard::clear(TERMINAL_CONTEXT_VARS);
        let _detect = EnvGuard::clear(DETECT_ONLY_VARS);
        unsafe {
            std::env::set_var("KITTY_WINDOW_ID", "window-1");
            std::env::set_var("KITTY_LISTEN_ON", "unix:/tmp/kitty-test");
        }

        let (mode, auto) = resolve_terminal_mode_for_tips(None, "default", false, false);

        assert_eq!(mode, "kitty-split");
        assert!(auto);
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

    fn ctx_with_script(script: &str) -> TerminalCommandContext<'_> {
        TerminalCommandContext {
            script,
            ..TerminalCommandContext::default()
        }
    }

    #[test]
    fn test_substitute_open_argv_basic() {
        let out = substitute_open_argv(
            &argv(&["open", "-a", "Terminal", "{script}"]),
            ctx_with_script("/tmp/test.sh"),
        )
        .unwrap();
        assert_eq!(out, vec!["open", "-a", "Terminal", "/tmp/test.sh"]);
    }

    #[test]
    fn test_substitute_open_argv_preserves_windows_path() {
        // A backslashed Windows .ps1 path substituted into a single argv element
        // must survive byte-for-byte (no shell splitting, no escaping).
        let out = substitute_open_argv(
            &argv(&["wt", "--", "powershell", "-File", "{script}"]),
            ctx_with_script(r"C:\Users\x\s.ps1"),
        )
        .unwrap();
        assert_eq!(
            out,
            vec!["wt", "--", "powershell", "-File", r"C:\Users\x\s.ps1"]
        );
    }

    #[test]
    fn test_substitute_open_argv_process_id_element() {
        // `HCOM_PROCESS_ID={process_id}` is one element; the placeholder is
        // replaced inside it without needing quoting.
        let out = substitute_open_argv(
            &argv(&["kitty", "--env", "HCOM_PROCESS_ID={process_id}", "{script}"]),
            TerminalCommandContext {
                script: "/tmp/test.sh",
                process_id: "abc-123",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec!["kitty", "--env", "HCOM_PROCESS_ID=abc-123", "/tmp/test.sh"]
        );
    }

    #[test]
    fn test_rewrite_open_argv_with_app_path_keeps_plain_open_a() {
        // No `--args` tail ⇒ leave `-a Terminal` intact (file-open form).
        let mut v = argv(&["open", "-a", "Terminal", "{script}"]);
        rewrite_open_argv_with_app_path(
            &mut v,
            Path::new("/System/Applications/Utilities/Terminal.app"),
        );
        assert_eq!(v, vec!["open", "-a", "Terminal", "{script}"]);
    }

    #[test]
    fn test_rewrite_open_argv_with_combined_flag() {
        let mut v = argv(&[
            "open",
            "-na",
            "Ghostty.app",
            "--args",
            "-e",
            "bash",
            "{script}",
        ]);
        rewrite_open_argv_with_app_path(&mut v, Path::new("/Applications/Ghostty.app"));
        assert_eq!(
            v,
            vec![
                "open",
                "-n",
                "/Applications/Ghostty.app",
                "--args",
                "-e",
                "bash",
                "{script}"
            ]
        );
    }

    #[test]
    fn test_rewrite_open_argv_with_explicit_args() {
        let mut v = argv(&["open", "-a", "Terminal", "--args", "bash", "{script}"]);
        rewrite_open_argv_with_app_path(
            &mut v,
            Path::new("/System/Applications/Utilities/Terminal.app"),
        );
        assert_eq!(
            v,
            vec![
                "open",
                "/System/Applications/Utilities/Terminal.app",
                "--args",
                "bash",
                "{script}"
            ]
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
    fn test_substitute_open_argv_missing_placeholder() {
        assert!(
            substitute_open_argv(
                &argv(&["open", "-a", "Terminal"]),
                ctx_with_script("/tmp/test.sh")
            )
            .is_err()
        );
    }

    #[test]
    fn test_substitute_open_argv_with_process_id() {
        let out = substitute_open_argv(
            &argv(&["tmux", "split", "-t", "{process_id}", "--", "{script}"]),
            TerminalCommandContext {
                script: "/tmp/test.sh",
                process_id: "abc-123",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec!["tmux", "split", "-t", "abc-123", "--", "/tmp/test.sh"]
        );
    }

    #[test]
    fn test_waveterm_preset_uses_run_separator() {
        let cmd = resolve_terminal_open_argv("waveterm").unwrap();
        let out = substitute_open_argv(
            &cmd,
            TerminalCommandContext {
                script: "/tmp/test.sh",
                process_id: "abc-123",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(out, vec!["wsh", "run", "--", "bash", "/tmp/test.sh"]);
    }

    #[test]
    fn test_normalize_waveterm_run_block_stdout() {
        assert_eq!(
            normalize_captured_terminal_id("run block created: block:abc123\n"),
            "block:abc123"
        );
        assert_eq!(normalize_captured_terminal_id("terminal_6"), "terminal_6");
    }

    #[test]
    fn test_normalize_herdr_agent_start_json() {
        let json = r#"{"id":"cli:agent:start","result":{"agent":{"agent_status":"unknown","cwd":"/tmp","focused":false,"name":"hcom-abc123","pane_id":"w123abc-3","revision":0,"tab_id":"w123abc:2","terminal_id":"term_abc","workspace_id":"w123abc"},"argv":["bash","/tmp/script.sh"],"type":"agent_started"}}"#;
        assert_eq!(normalize_captured_terminal_id(json), "w123abc-3");
    }

    #[test]
    fn test_normalize_herdr_error_json_falls_through() {
        // Error JSON from herdr should not match (no result.agent.pane_id)
        let json = r#"{"error":{"code":"server_unavailable","message":"herdr server not running"},"id":"cli:agent:start"}"#;
        assert_eq!(normalize_captured_terminal_id(json), json);
    }

    #[test]
    fn test_normalize_herdr_empty_pane_id() {
        let json = r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":""}}}"#;
        assert_eq!(normalize_captured_terminal_id(json), json);
    }

    #[test]
    fn test_herdr_preset_template_uses_stable_instance_name() {
        // The herdr preset must launch with a stable agent name so
        // `herdr agent send <name>` keeps working — the styled status label
        // (`◉ luna [claude]`) is pushed separately via `pane.rename` from the
        // delivery loop, not baked into the agent name.
        let preset = crate::shared::terminal_presets::get_terminal_preset("herdr").unwrap();
        let open = preset.open.select(false).unwrap();
        assert!(open.contains(&"{script}"));
        assert!(open.contains(&"{instance_name}"));
        assert!(
            !open.contains(&"{pane_title}"),
            "herdr preset must not use {{pane_title}} as agent name"
        );
        assert!(open.contains(&"{cwd}"));
        assert!(!open.contains(&"{process_id}"));
        assert_eq!(preset.binary, Some("herdr"));
        // Close must use {pane_id} (stable raw `p_N`), not {id} (public
        // `<ws>-<N>` which herdr renumbers when sibling panes close — a kill
        // batch addressing the public id can land on the wrong pane).
        let close = preset.close.select(false).unwrap();
        assert!(close.contains(&"{pane_id}"));
    }

    #[test]
    fn test_substitute_herdr_open_argv_uses_pane_title() {
        let template = argv(&[
            "herdr",
            "agent",
            "start",
            "{pane_title}",
            "--cwd",
            "{cwd}",
            "--no-focus",
            "--",
            "bash",
            "{script}",
        ]);
        let out = substitute_open_argv(
            &template,
            TerminalCommandContext {
                script: "/tmp/test.sh",
                process_id: "abc-123",
                cwd: "/home/user/project",
                instance_name: "luna",
                tool: "claude",
                pane_title: Some("\u{25c9} luna [claude]"),
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                "herdr",
                "agent",
                "start",
                "\u{25c9} luna [claude]",
                "--cwd",
                "/home/user/project",
                "--no-focus",
                "--",
                "bash",
                "/tmp/test.sh"
            ]
        );
    }

    #[test]
    fn test_substitute_open_argv_pane_title_falls_back_to_instance_name() {
        let out = substitute_open_argv(
            &argv(&[
                "herdr",
                "agent",
                "start",
                "{pane_title}",
                "--",
                "bash",
                "{script}",
            ]),
            TerminalCommandContext {
                script: "/tmp/test.sh",
                instance_name: "abc-123",
                tool: "codex",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                "herdr",
                "agent",
                "start",
                "abc-123",
                "--",
                "bash",
                "/tmp/test.sh"
            ]
        );
    }

    #[test]
    fn test_substitute_open_argv_cwd_placeholder() {
        let out = substitute_open_argv(
            &argv(&["myterm", "--dir", "{cwd}", "--", "bash", "{script}"]),
            TerminalCommandContext {
                script: "/tmp/test.sh",
                cwd: "/home/user",
                ..TerminalCommandContext::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            vec![
                "myterm",
                "--dir",
                "/home/user",
                "--",
                "bash",
                "/tmp/test.sh"
            ]
        );
    }

    #[test]
    fn test_substitute_open_argv_empty_cwd() {
        // Templates without {cwd} should work with empty cwd
        let out = substitute_open_argv(
            &argv(&["open", "-a", "Terminal", "{script}"]),
            ctx_with_script("/tmp/test.sh"),
        )
        .unwrap();
        assert_eq!(out, vec!["open", "-a", "Terminal", "/tmp/test.sh"]);
    }

    #[test]
    fn test_substitute_close_argv_skips_when_pane_id_missing() {
        // Required {pane_id} placeholder but empty value ⇒ None (skip close).
        assert!(
            substitute_close_argv(
                &argv(&["wezterm", "cli", "kill-pane", "--pane-id", "{pane_id}"]),
                42,
                "",
                "proc-1",
                "",
            )
            .is_none()
        );
    }

    #[test]
    fn test_substitute_close_argv_substitutes_pane_id() {
        let out = substitute_close_argv(
            &argv(&["wezterm", "cli", "kill-pane", "--pane-id", "{pane_id}"]),
            42,
            "pane-7",
            "proc-1",
            "",
        )
        .unwrap();
        assert_eq!(
            out,
            vec!["wezterm", "cli", "kill-pane", "--pane-id", "pane-7"]
        );
    }

    #[test]
    fn test_zellij_close_argv_session_splice() {
        // Reproduce the close_terminal_pane splice: --session <name> after zellij.
        let mut a = substitute_close_argv(
            &argv(&["zellij", "action", "close-pane", "--pane-id", "{pane_id}"]),
            0,
            "6",
            "",
            "",
        )
        .unwrap();
        a.splice(1..1, ["--session".to_string(), "wise-kangaroo".to_string()]);
        assert_eq!(
            a,
            vec![
                "zellij",
                "--session",
                "wise-kangaroo",
                "action",
                "close-pane",
                "--pane-id",
                "6"
            ]
        );
    }

    #[test]
    fn test_kitten_close_argv_to_splice() {
        // Reproduce the close_terminal_pane splice: --to <socket> after `@`.
        let mut a = substitute_close_argv(
            &argv(&["kitten", "@", "close-window", "--match", "id:{pane_id}"]),
            0,
            "13",
            "",
            "",
        )
        .unwrap();
        a.splice(2..2, ["--to".to_string(), "unix:/tmp/kitty".to_string()]);
        assert_eq!(
            a,
            vec![
                "kitten",
                "@",
                "--to",
                "unix:/tmp/kitty",
                "close-window",
                "--match",
                "id:13"
            ]
        );
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
