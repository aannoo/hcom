//! CLI router: clap-based dispatch for hooks, commands, PTY, and TUI.
//!
//! All hooks and commands are handled natively in Rust.

use std::env;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;

use crate::db::DEV_ROOT_KV_KEY;
use crate::log::{log_error, log_info, log_warn};
use crate::shared::dev_root_binary;

// ── Hook name registries ──────────────

/// Claude Code hooks (read payload from stdin)
const CLAUDE_HOOKS: &[&str] = &[
    "poll",
    "notify",
    "permission-request",
    "pre",
    "post",
    "sessionstart",
    "userpromptsubmit",
    "sessionend",
    "subagent-start",
    "subagent-stop",
];

/// Gemini CLI hooks (read payload from stdin)
const GEMINI_HOOKS: &[&str] = &[
    "gemini-sessionstart",
    "gemini-beforeagent",
    "gemini-afteragent",
    "gemini-beforetool",
    "gemini-aftertool",
    "gemini-notification",
    "gemini-sessionend",
];

/// Codex hooks (read payload from argv)
const CODEX_HOOKS: &[&str] = &["codex-notify"];

/// OpenCode hooks (read payload from argv)
const OPENCODE_HOOKS: &[&str] = &[
    "opencode-start",
    "opencode-status",
    "opencode-read",
    "opencode-stop",
];

/// All known hook names (for fast lookup)
fn is_hook(name: &str) -> bool {
    CLAUDE_HOOKS.contains(&name)
        || GEMINI_HOOKS.contains(&name)
        || CODEX_HOOKS.contains(&name)
        || OPENCODE_HOOKS.contains(&name)
}

// ── Known CLI commands ──────────────────────────────────────────────────

const COMMANDS: &[&str] = &[
    "send",
    "list",
    "events",
    "stop",
    "start",
    "listen",
    "status",
    "config",
    "hooks",
    "archive",
    "reset",
    "transcript",
    "bundle",
    "kill",
    "term",
    "relay",
    "run",
    "update",
];

/// Tools that support launch commands (hcom [N] <tool>)
const LAUNCH_TOOLS: &[&str] = &["claude", "codex", "gemini", "opencode", "f", "r"];

fn is_command(name: &str) -> bool {
    COMMANDS.contains(&name)
}

fn is_launch_tool(name: &str) -> bool {
    LAUNCH_TOOLS.contains(&name)
}

// ── Dispatch types ──────────────────────────────────────────────────────

/// Resolved action after argv inspection.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// Run a hook handler. Args are the full argv[1..] passed through.
    Hook { hook: String, args: Vec<String> },
    /// Run a CLI command. Args are the full argv[1..] passed through.
    Command { cmd: String, args: Vec<String> },
    /// Launch tool (e.g. `hcom 3 claude --model haiku`)
    Launch { args: Vec<String> },
    /// Run PTY wrapper mode
    Pty { args: Vec<String> },
    /// Run TUI (no arguments)
    Tui,
    /// Show version
    Version,
    /// Show help
    Help,
    /// Open TUI in new terminal window
    NewTerminal,
    /// Run relay-worker process
    RelayWorker,
}

/// Global flags extracted from argv before dispatch.
#[derive(Debug, Default, PartialEq)]
pub struct GlobalFlags {
    pub name: Option<String>,
    pub go: bool,
}

// ── Argv parsing (clap for flags, manual for command routing) ───────────
//
// Top-level command/hook routing stays manual because hcom's CLI is unusual:
// hooks appear as bare subcommands (`hcom sessionstart`), launch commands can
// start with a numeric count (`hcom 3 claude`), and ~40 hook/command/tool names
// need classification. clap will be used for per-command arg parsing as commands
// are ported to Rust.

/// Clap parser for global flags only. Remaining args collected as positionals.
#[derive(Parser, Debug)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct GlobalFlagParser {
    /// Instance name for identity
    #[arg(long)]
    name: Option<String>,

    /// Skip confirmation prompts
    #[arg(long, action = clap::ArgAction::SetTrue)]
    go: bool,

    /// Everything after global flags (command/hook name + its args)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    rest: Vec<String>,
}

/// Extract global flags (--name VALUE, --go) from argv using clap.
/// Returns (remaining_args, flags).
///
/// NOTE: clap's trailing_var_arg means --name after a positional (command token)
/// is NOT extracted. Use `extract_global_flags_full()` when you need to find
/// --name anywhere in argv (e.g., `hcom send --name vami @luna -- hello`).
pub fn extract_global_flags(argv: &[String]) -> (Vec<String>, GlobalFlags) {
    match GlobalFlagParser::try_parse_from(argv) {
        Ok(parsed) => (
            parsed.rest,
            GlobalFlags {
                name: parsed.name,
                go: parsed.go,
            },
        ),
        Err(_) => {
            // Clap couldn't parse (e.g. --name without value). Fall back to
            // manual extraction so the proper error can be reported.
            extract_global_flags_manual(argv)
        }
    }
}

/// Manual fallback for global flag extraction (handles edge cases clap rejects).
fn extract_global_flags_manual(argv: &[String]) -> (Vec<String>, GlobalFlags) {
    let mut remaining = Vec::with_capacity(argv.len());
    let mut flags = GlobalFlags::default();
    let mut i = 0;

    while i < argv.len() {
        match argv[i].as_str() {
            "--name" if i + 1 < argv.len() => {
                flags.name = Some(argv[i + 1].clone());
                i += 2;
            }
            "--go" => {
                flags.go = true;
                i += 1;
            }
            _ => {
                remaining.push(argv[i].clone());
                i += 1;
            }
        }
    }

    (remaining, flags)
}

/// Extract global flags from anywhere in argv, respecting `--` separator.
///
/// Unlike `extract_global_flags()` (clap-based, only finds flags before first
/// positional), this scans the full argv up to `--`. Used by `dispatch_native_command()`
/// to handle `hcom send --name vami @luna -- hello` correctly.
///
/// Also detects --help/-h requests (before `--`) for per-command help dispatch.
pub fn extract_global_flags_full(argv: &[String]) -> (Vec<String>, GlobalFlags, bool) {
    let sep_pos = argv.iter().position(|a| a == "--");
    let scan_end = sep_pos.unwrap_or(argv.len());

    let mut remaining = Vec::with_capacity(argv.len());
    let mut flags = GlobalFlags::default();
    let mut help_requested = false;
    let mut i = 0;

    while i < scan_end {
        match argv[i].as_str() {
            "--name" if i + 1 < scan_end => {
                flags.name = Some(argv[i + 1].clone());
                i += 2;
            }
            "--go" => {
                flags.go = true;
                i += 1;
            }
            "--help" | "-h" => {
                help_requested = true;
                i += 1;
            }
            _ => {
                remaining.push(argv[i].clone());
                i += 1;
            }
        }
    }

    // Append everything from separator onwards unchanged
    if i < argv.len() {
        remaining.extend_from_slice(&argv[i..]);
    }

    (remaining, flags, help_requested)
}

/// Determine action from argv (after binary name is stripped).
///
/// High-level precedence: no args -> TUI; top-level global flags / special modes
/// (`relay-worker`, `pty`); then first non-flag token as hook, CLI command, or
/// launch verb.
pub fn resolve_action(argv: &[String]) -> Action {
    // No args: TUI
    if argv.is_empty() {
        return Action::Tui;
    }

    let first = argv[0].as_str();

    // Global flags as commands
    match first {
        "--help" | "-h" => return Action::Help,
        "--version" | "-v" => return Action::Version,
        "--new-terminal" => return Action::NewTerminal,
        _ => {}
    }

    // Relay worker mode: `hcom relay-worker`
    if first == "relay-worker" {
        return Action::RelayWorker;
    }

    // PTY mode: `hcom pty <tool> [args...]`
    if first == "pty" {
        return Action::Pty {
            args: argv[1..].to_vec(),
        };
    }

    // Strip global flags for command/hook detection
    let (stripped, _flags) = extract_global_flags(argv);

    // Find the first non-flag token in stripped args
    let cmd_token = stripped.first().map(|s| s.as_str()).unwrap_or("");

    // Hook detection: argv[1] matches a known hook name
    if is_hook(cmd_token) {
        return Action::Hook {
            hook: cmd_token.to_string(),
            args: argv.to_vec(),
        };
    }

    // Command detection
    if is_command(cmd_token) {
        return Action::Command {
            cmd: cmd_token.to_string(),
            args: argv.to_vec(),
        };
    }

    // Launch detection: [N] <tool> or just <tool>
    if is_launch_tool(cmd_token) {
        return Action::Launch {
            args: argv.to_vec(),
        };
    }
    // Numeric count + tool: `hcom 3 claude`
    if cmd_token.parse::<u32>().is_ok() {
        if let Some(second) = stripped.get(1) {
            if is_launch_tool(second.as_str()) {
                return Action::Launch {
                    args: argv.to_vec(),
                };
            }
        }
    }

    // --new-terminal can appear after flags: `hcom --name foo --new-terminal`
    if stripped.iter().any(|a| a == "--new-terminal") {
        return Action::NewTerminal;
    }

    // Unknown — fall through to client
    Action::Command {
        cmd: cmd_token.to_string(),
        args: argv.to_vec(),
    }
}

// ── HCOM_DEV_ROOT re-exec ───────────────────────────────────────────────

/// If HCOM_DEV_ROOT is set and points to a different worktree, re-exec using
/// that worktree's binary.
/// so worktree development works: `HCOM_DEV_ROOT=/path/to/worktree hcom list`
/// will run the worktree's hcom binary instead of the installed one.
pub fn maybe_reexec_dev_root() {
    let (dev_root, source) = match resolve_effective_dev_root(&crate::paths::db_path()) {
        Some(v) => v,
        None => return,
    };

    // Find current binary's location
    let current_exe = match env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };

    let target_binary = match dev_root_binary(&dev_root) {
        Some(p) => p,
        None => {
            log_warn(
                "router",
                "dev_root_no_binary",
                &format!(
                    "dev_root={} ({source}) but no dev binary found. Run `cargo build` or `cargo build --release` in the worktree.",
                    dev_root.display(),
                ),
            );
            return;
        }
    };

    // Don't re-exec if we're already running the right binary
    if is_same_file(&current_exe, &target_binary) {
        return;
    }

    log_info(
        "router",
        "dev_root_reexec",
        &format!(
            "re-exec to {} (current={}, source={})",
            target_binary.display(),
            current_exe.display(),
            source,
        ),
    );

    // Re-exec: replace this process with the dev root's binary
    let args: Vec<String> = env::args().collect();
    let err = Command::new(&target_binary).args(&args[1..]).exec();
    // exec() only returns on error
    log_error(
        "router",
        "dev_root_reexec_failed",
        &format!("failed to exec {}: {}", target_binary.display(), err),
    );
}

/// True if argv is `hcom config dev_root [...]` (after stripping global flags).
fn is_config_dev_root_invocation(argv: &[String]) -> bool {
    let (positional, _, _) = extract_global_flags_full(argv);
    let mut iter = positional.iter().take_while(|a| a.as_str() != "--");
    matches!(
        (
            iter.next().map(String::as_str),
            iter.next().map(String::as_str)
        ),
        (Some("config"), Some("dev_root"))
    )
}

pub(crate) fn resolve_effective_dev_root(db_path: &Path) -> Option<(PathBuf, &'static str)> {
    if let Ok(r) = env::var("HCOM_DEV_ROOT") {
        if !r.is_empty() {
            return Some((PathBuf::from(r), "env"));
        }
    }

    read_dev_root_from_kv(db_path).map(|path| (path, "kv"))
}

fn read_dev_root_from_kv(db_path: &Path) -> Option<PathBuf> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| {
                log_warn(
                    "router",
                    "dev_root_kv_open_failed",
                    &format!("failed to open db at {}: {e}", db_path.display()),
                );
                e
            })
            .ok()?;

    conn.busy_timeout(std::time::Duration::from_millis(200))
        .ok();

    let value = conn
        .query_row(
            "SELECT value FROM kv WHERE key = ?",
            rusqlite::params![DEV_ROOT_KV_KEY],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|e| {
            log_warn(
                "router",
                "dev_root_kv_read_failed",
                &format!("failed to read dev_root from kv: {e}"),
            );
            e
        })
        .ok()
        .flatten()?;

    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

/// Check if two paths refer to the same file (follows symlinks).
fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

// ── Dispatch ────────────────────────────────────────────────────────────

/// Main entry point: resolve action and dispatch.
pub fn dispatch() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let argv = &args[1..]; // strip binary name

    // Skip dev_root re-exec for `config dev_root` so a stale pointer can't
    // trap the user — the invoked binary owns its own dev_root setting.
    if !is_config_dev_root_invocation(argv) {
        maybe_reexec_dev_root();
    }

    let action = resolve_action(argv);

    // Check for updates on CLI commands (not hooks/pty/relay — those need to be fast/silent).
    // Skip for `hcom update` itself — it handles its own output.
    let is_update_cmd = matches!(&action, Action::Command { cmd, .. } if cmd == "update");
    if !is_update_cmd
        && matches!(
            action,
            Action::Command { .. } | Action::Launch { .. } | Action::Version | Action::Help
        )
    {
        if let Some(notice) = crate::update::get_update_notice() {
            eprintln!("{notice}");
        }
    }

    match action {
        Action::Tui => {
            crate::tui::run().map_err(|e| anyhow::anyhow!("{e:#}"))?;
        }
        Action::Pty { args } => {
            crate::run_pty(&args)?;
        }
        Action::RelayWorker => {
            let exit_code = crate::relay::worker::run();
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Hook { ref hook, ref args } if hook == "codex-notify" => {
            // Codex notify hook — handled natively in Rust.
            let exit_code = crate::hooks::codex::dispatch_codex_hook(args);
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Hook { ref hook, ref args } if OPENCODE_HOOKS.contains(&hook.as_str()) => {
            // OpenCode hooks handled natively in Rust.
            let (exit_code, output) = crate::hooks::opencode::dispatch_opencode_hook(hook, args);
            if !output.is_empty() {
                print!("{}", output);
            }
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Hook { ref hook, .. } if GEMINI_HOOKS.contains(&hook.as_str()) => {
            // Gemini hooks handled natively in Rust.
            let exit_code = crate::hooks::gemini::dispatch_gemini_hook(hook);
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Hook { ref hook, .. } if CLAUDE_HOOKS.contains(&hook.as_str()) => {
            // Claude hooks handled natively in Rust.
            let exit_code = crate::hooks::claude::dispatch_claude_hook(hook);
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Launch { ref args } => {
            // Launch/resume/fork handled natively.
            let (stripped, flags, help) = extract_global_flags_full(args);
            let first_cmd = stripped.first().map(|s| s.as_str()).unwrap_or("");
            // Skip numeric count prefix
            let cmd = if first_cmd.parse::<u32>().is_ok() {
                stripped.get(1).map(|s| s.as_str()).unwrap_or("")
            } else {
                first_cmd
            };
            if help {
                crate::commands::help::print_command_help(cmd);
                return Ok(());
            }
            let exit_code = match cmd {
                "r" | "resume" => crate::commands::resume::run(args, &flags)?,
                "f" | "fork" => crate::commands::fork::run(args, &flags)?,
                _ => crate::commands::launch::run(args, &flags)?,
            };
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Command { ref cmd, ref args } if matches!(cmd.as_str(), "start" | "kill") => {
            let (_, flags, help) = extract_global_flags_full(args);
            if help {
                crate::commands::help::print_command_help(cmd);
                return Ok(());
            }
            let exit_code = match cmd.as_str() {
                "start" => crate::commands::start::run(args, &flags)?,
                "kill" => crate::commands::kill::run(args, &flags)?,
                _ => unreachable!(),
            };
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Command { ref cmd, ref args }
            if matches!(
                cmd.as_str(),
                "send"
                    | "list"
                    | "stop"
                    | "listen"
                    | "events"
                    | "transcript"
                    | "config"
                    | "status"
                    | "bundle"
                    | "archive"
                    | "reset"
                    | "hooks"
                    | "term"
                    | "relay"
                    | "run"
                    | "update"
            ) =>
        {
            let exit_code = dispatch_native_command(cmd, args);
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Hook { ref hook, .. } | Action::Command { cmd: ref hook, .. } => {
            eprintln!("Error: Unknown command '{}'", hook);
            eprintln!("Run 'hcom --help' for usage.");
            std::process::exit(1);
        }
        Action::Version => {
            println!("hcom {}", env!("CARGO_PKG_VERSION"));
        }
        Action::Help => {
            crate::commands::help::print_help();
        }
        Action::NewTerminal => {
            let exit_code = launch_new_terminal();
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
    }

    Ok(())
}

// ── New terminal ─────────────────────────────────────────────────────────

/// Open TUI in a new terminal window using native terminal.rs.
fn launch_new_terminal() -> i32 {
    use std::collections::HashMap;

    let exe = match env::current_exe() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(e) => {
            eprintln!("Error: Cannot determine hcom binary path: {}", e);
            return 1;
        }
    };

    let cwd = env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    // Pass through HCOM env vars
    let mut env_vars = HashMap::new();
    for (k, v) in env::vars() {
        if k.starts_with("HCOM_") {
            env_vars.insert(k, v);
        }
    }

    let inside_ai = crate::shared::is_inside_ai_tool();

    match crate::terminal::launch_terminal(
        &exe,
        &env_vars,
        cwd.as_deref(),
        false, // not background
        false, // not run_here (open new window)
        None,  // default terminal
        inside_ai,
    ) {
        Ok((crate::terminal::LaunchResult::Success, _)) => 0,
        Ok((crate::terminal::LaunchResult::Failed(msg), _)) => {
            eprintln!("Error: {}", msg);
            1
        }
        Ok(_) => 0,
        Err(e) => {
            eprintln!("Error: Failed to open new terminal: {}", e);
            1
        }
    }
}

// ── Native command dispatch ──────────────────────────────────────────────

/// Dispatch a natively-handled CLI command.
///
/// Opens DB, builds CommandContext, calls the appropriate cmd_* function.
/// Args are the full argv[1..] (includes the command name and global flags).
fn dispatch_native_command(cmd: &str, args: &[String]) -> i32 {
    use crate::cli_context::build_ctx_for_command;
    use crate::db::HcomDb;

    // Extract global flags (--name, --go, --help) from anywhere in args,
    // respecting -- separator. Uses full scan (not clap) so --name works
    // regardless of position: `hcom send --name vami @luna -- hello`.
    let (stripped, flags, help_requested) = extract_global_flags_full(args);

    // Per-command --help: native help text
    // ("run" handles --help itself for script-level help)
    if help_requested && cmd != "run" {
        crate::commands::help::print_command_help(cmd);
        return 0;
    }

    // Strip command name from stripped args to get command-specific argv.
    // For "run", re-inject --help so the script itself can handle it.
    let cmd_argv: Vec<String> = {
        let mut v: Vec<String> = stripped.iter().skip(1).cloned().collect();
        if cmd == "run" && help_requested {
            v.push("--help".to_string());
        }
        v
    };

    // "relay daemon" subcommand doesn't need DB or identity context
    if cmd == "relay" && cmd_argv.first().map(|s| s.as_str()) == Some("daemon") {
        return crate::commands::daemon::cmd_daemon(&cmd_argv[1..]);
    }

    // Open DB (includes schema migration/compat check)
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error: Failed to open database: {e}");
            return 1;
        }
    };

    // Build context (identity resolution, --go flag)
    let process_id = std::env::var("HCOM_PROCESS_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let codex_thread_id = std::env::var("CODEX_THREAD_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let ctx = match build_ctx_for_command(
        &db,
        Some(cmd),
        flags.name.as_deref(),
        flags.go,
        process_id.as_deref(),
        codex_thread_id.as_deref(),
    ) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // Identity gating: block unregistered sessions from gated commands
    let has_from_flag = cmd_argv.iter().any(|a| a == "--from" || a == "-b");
    let is_inside_ai = crate::shared::is_inside_ai_tool();
    if let Err(e) = crate::cli_context::check_identity_gate(cmd, &ctx, has_from_flag, is_inside_ai)
    {
        eprintln!("Error: {e}");
        return 1;
    }

    // Subagent context: require explicit --name for identity-gated commands inside Claude
    if crate::identity::requires_identity(cmd)
        && ctx.explicit_name.is_none()
        && std::env::var("CLAUDE_CODE_ENTRYPOINT").is_ok()
    {
        if let Some(ref identity) = ctx.identity {
            if crate::instances::in_subagent_context(&db, &identity.name) {
                eprintln!(
                    "Error: Subagent context active - explicit identity required\n\
                     Use: hcom {cmd} --name parent (for parent) or --name <uuid> (for subagent)"
                );
                return 1;
            }
        }
    }

    // Set hookless command status (subagent/codex/adhoc)
    crate::cli_context::set_hookless_command_status(&db, cmd, &ctx);

    // Dispatch to command handler
    let has_json = cmd_argv.iter().any(|a| a == "--json");
    /// Parse a clap Args struct from command argv, handling help/error output.
    /// Returns exit code on parse error (1 for errors, 0 for help/version).
    macro_rules! clap_parse {
        ($type:ty, $name:expr, $argv:expr) => {{
            use clap::Parser;
            <$type>::try_parse_from(std::iter::once($name.to_string()).chain($argv.iter().cloned()))
        }};
    }

    macro_rules! clap_dispatch {
        ($type:ty, $name:expr, $argv:expr, $handler:expr) => {{
            match clap_parse!($type, $name, $argv) {
                Ok(args) => $handler(args),
                Err(e) => {
                    e.print().ok();
                    if e.use_stderr() { 1 } else { 0 }
                }
            }
        }};
    }

    let result = match cmd {
        // Messaging
        "send" => match clap_parse!(crate::commands::send::SendArgs, cmd, &cmd_argv) {
            Ok(mut args) => {
                args.had_separator = cmd_argv.iter().any(|a| a == "--");
                crate::commands::send::cmd_send(&db, &args, Some(&ctx))
            }
            Err(e) => {
                e.print().ok();
                if e.use_stderr() { 1 } else { 0 }
            }
        },
        "list" => clap_dispatch!(crate::commands::list::ListArgs, cmd, &cmd_argv, |args| {
            crate::commands::list::cmd_list(&db, &args, Some(&ctx))
        }),
        "stop" => clap_dispatch!(crate::commands::stop::StopArgs, cmd, &cmd_argv, |args| {
            crate::commands::stop::cmd_stop(&db, &args, Some(&ctx))
        }),
        "listen" => clap_dispatch!(
            crate::commands::listen::ListenArgs,
            cmd,
            &cmd_argv,
            |args| crate::commands::listen::cmd_listen(&db, &args, Some(&ctx))
        ),
        // Diagnostics
        "events" => clap_dispatch!(
            crate::commands::events::EventsArgs,
            cmd,
            &cmd_argv,
            |args| crate::commands::events::cmd_events(&db, &args, Some(&ctx))
        ),
        "transcript" => clap_dispatch!(
            crate::commands::transcript::TranscriptArgs,
            cmd,
            &cmd_argv,
            |args| crate::commands::transcript::cmd_transcript(&db, &args, Some(&ctx))
        ),
        "config" => clap_dispatch!(
            crate::commands::config::ConfigArgs,
            cmd,
            &cmd_argv,
            |args| crate::commands::config::cmd_config(&db, &args, Some(&ctx))
        ),
        "status" => clap_dispatch!(
            crate::commands::status::StatusArgs,
            cmd,
            &cmd_argv,
            |args| crate::commands::status::cmd_status(&db, &args, Some(&ctx))
        ),
        "bundle" => clap_dispatch!(
            crate::commands::bundle::BundleArgs,
            cmd,
            &cmd_argv,
            |args| crate::commands::bundle::cmd_bundle(&db, &args, Some(&ctx))
        ),
        // Management
        "archive" => clap_dispatch!(
            crate::commands::archive::ArchiveArgs,
            cmd,
            &cmd_argv,
            |args| crate::commands::archive::cmd_archive(&db, &args, Some(&ctx))
        ),
        "reset" => clap_dispatch!(crate::commands::reset::ResetArgs, cmd, &cmd_argv, |args| {
            crate::commands::reset::cmd_reset(&db, &args, Some(&ctx))
        }),
        "hooks" => clap_dispatch!(crate::commands::hooks::HooksArgs, cmd, &cmd_argv, |args| {
            crate::commands::hooks::cmd_hooks(&db, &args, Some(&ctx))
        }),
        "term" => clap_dispatch!(crate::commands::term::TermArgs, cmd, &cmd_argv, |args| {
            crate::commands::term::cmd_term(&db, &args, Some(&ctx))
        }),
        "relay" => clap_dispatch!(crate::commands::relay::RelayArgs, cmd, &cmd_argv, |args| {
            crate::commands::relay::cmd_relay(&db, &args, Some(&ctx))
        }),
        "run" => clap_dispatch!(crate::commands::run::RunArgs, cmd, &cmd_argv, |args| {
            crate::commands::run::cmd_run(&db, &args, Some(&ctx))
        }),
        "update" => clap_dispatch!(
            crate::commands::update::UpdateArgs,
            cmd,
            &cmd_argv,
            |args| crate::commands::update::cmd_update(&db, &args, Some(&ctx))
        ),
        _ => {
            // Should never happen — only matched commands reach here
            eprintln!("Error: Unknown native command '{cmd}'");
            1
        }
    };

    // Deliver pending messages AFTER command.
    // Deliver pending messages AFTER command for hookless codex/adhoc instances.
    // This appends unread hcom messages to the command's stdout — keep in mind
    // when changing output contracts or adding machine-readable modes.
    if let Some(output) = crate::cli_context::maybe_deliver_pending_messages(&db, &ctx, has_json) {
        print!("{output}");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DEV_ROOT_KV_KEY;

    fn sv(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn read_dev_root_from_kv_returns_stored_value() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hcom.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)",
            rusqlite::params![DEV_ROOT_KV_KEY, "/tmp/dev-root"],
        )
        .unwrap();

        assert_eq!(
            read_dev_root_from_kv(&db_path),
            Some(PathBuf::from("/tmp/dev-root"))
        );
    }

    #[test]
    fn read_dev_root_from_kv_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hcom.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT)", [])
            .unwrap();

        assert_eq!(read_dev_root_from_kv(&db_path), None);
    }

    #[test]
    fn is_config_dev_root_invocation_matches_expected_shapes() {
        assert!(is_config_dev_root_invocation(&sv(&["config", "dev_root"])));
        assert!(is_config_dev_root_invocation(&sv(&[
            "config", "dev_root", "/tmp/x"
        ])));
        assert!(is_config_dev_root_invocation(&sv(&[
            "config", "dev_root", "--unset"
        ])));
        assert!(is_config_dev_root_invocation(&sv(&[
            "--name", "vami", "config", "dev_root", "/tmp/x"
        ])));

        assert!(!is_config_dev_root_invocation(&sv(&["config"])));
        assert!(!is_config_dev_root_invocation(&sv(&[
            "config", "terminal", "tmux"
        ])));
        assert!(!is_config_dev_root_invocation(&sv(&["list"])));
        assert!(!is_config_dev_root_invocation(&[]));
    }

    // ── resolve_action tests ────────────────────────────────────────────

    #[test]
    fn no_args_runs_tui() {
        let action = resolve_action(&[]);
        assert_eq!(action, Action::Tui);
    }

    #[test]
    fn pty_mode() {
        let action = resolve_action(&sv(&["pty", "claude"]));
        assert_eq!(
            action,
            Action::Pty {
                args: sv(&["claude"])
            }
        );
    }

    #[test]
    fn pty_mode_with_args() {
        let action = resolve_action(&sv(&["pty", "claude", "--arg1"]));
        assert_eq!(
            action,
            Action::Pty {
                args: sv(&["claude", "--arg1"])
            }
        );
    }

    // ── Hook detection ──────────────────────────────────────────────────

    #[test]
    fn claude_hooks_detected() {
        for hook_name in CLAUDE_HOOKS {
            let action = resolve_action(&sv(&[hook_name]));
            match &action {
                Action::Hook { hook, .. } => assert_eq!(hook, hook_name),
                _ => panic!("expected Hook for {}, got {:?}", hook_name, action),
            }
        }
    }

    #[test]
    fn gemini_hooks_detected() {
        for hook_name in GEMINI_HOOKS {
            let action = resolve_action(&sv(&[hook_name]));
            match &action {
                Action::Hook { hook, .. } => assert_eq!(hook, hook_name),
                _ => panic!("expected Hook for {}, got {:?}", hook_name, action),
            }
        }
    }

    #[test]
    fn codex_hook_detected() {
        let action = resolve_action(&sv(&["codex-notify", "{\"event\":\"idle\"}"]));
        match &action {
            Action::Hook { hook, args } => {
                assert_eq!(hook, "codex-notify");
                assert_eq!(*args, sv(&["codex-notify", "{\"event\":\"idle\"}"]));
            }
            _ => panic!("expected Hook, got {:?}", action),
        }
    }

    #[test]
    fn opencode_hooks_detected() {
        for hook_name in OPENCODE_HOOKS {
            let action = resolve_action(&sv(&[hook_name]));
            match &action {
                Action::Hook { hook, .. } => assert_eq!(hook, hook_name),
                _ => panic!("expected Hook for {}, got {:?}", hook_name, action),
            }
        }
    }

    // ── Command detection ───────────────────────────────────────────────

    #[test]
    fn cli_commands_detected() {
        for cmd_name in COMMANDS {
            let action = resolve_action(&sv(&[cmd_name]));
            match &action {
                Action::Command { cmd, .. } => assert_eq!(cmd, cmd_name),
                _ => panic!("expected Command for {}, got {:?}", cmd_name, action),
            }
        }
    }

    #[test]
    fn command_with_args() {
        let action = resolve_action(&sv(&["send", "@luna", "--", "hello"]));
        match &action {
            Action::Command { cmd, args } => {
                assert_eq!(cmd, "send");
                assert_eq!(*args, sv(&["send", "@luna", "--", "hello"]));
            }
            _ => panic!("expected Command, got {:?}", action),
        }
    }

    // ── Launch detection ────────────────────────────────────────────────

    #[test]
    fn launch_tool_direct() {
        let action = resolve_action(&sv(&["claude"]));
        assert_eq!(
            action,
            Action::Launch {
                args: sv(&["claude"])
            }
        );
    }

    #[test]
    fn launch_tool_with_count() {
        let action = resolve_action(&sv(&["3", "claude", "--model", "haiku"]));
        assert_eq!(
            action,
            Action::Launch {
                args: sv(&["3", "claude", "--model", "haiku"])
            }
        );
    }

    #[test]
    fn launch_tool_with_global_flags() {
        let action = resolve_action(&sv(&["--name", "mybot", "--go", "claude"]));
        assert_eq!(
            action,
            Action::Launch {
                args: sv(&["--name", "mybot", "--go", "claude"])
            }
        );
    }

    // ── Global flags ────────────────────────────────────────────────────

    #[test]
    fn extract_name_flag() {
        let (remaining, flags) = extract_global_flags(&sv(&["--name", "foo", "list"]));
        assert_eq!(remaining, sv(&["list"]));
        assert_eq!(flags.name, Some("foo".to_string()));
        assert!(!flags.go);
    }

    #[test]
    fn extract_go_flag() {
        let (remaining, flags) = extract_global_flags(&sv(&["--go", "stop", "all"]));
        assert_eq!(remaining, sv(&["stop", "all"]));
        assert!(flags.go);
        assert!(flags.name.is_none());
    }

    #[test]
    fn extract_both_flags() {
        let (remaining, flags) = extract_global_flags(&sv(&["--name", "bot", "--go", "stop"]));
        assert_eq!(remaining, sv(&["stop"]));
        assert_eq!(flags.name, Some("bot".to_string()));
        assert!(flags.go);
    }

    #[test]
    fn flags_before_command_extracted() {
        let (remaining, flags) =
            extract_global_flags(&sv(&["--name", "x", "send", "@luna", "--", "hi"]));
        assert_eq!(remaining, sv(&["send", "@luna", "--", "hi"]));
        assert_eq!(flags.name, Some("x".to_string()));
    }

    #[test]
    fn flags_after_command_not_extracted() {
        // --name after a positional stays in rest (clap's trailing_var_arg behavior).
        // This is correct: global flags belong before the command. The full argv
        // is still passed to handlers, which extract --name at the command level.
        let (remaining, flags) =
            extract_global_flags(&sv(&["send", "--name", "x", "@luna", "--", "hi"]));
        assert_eq!(remaining, sv(&["send", "--name", "x", "@luna", "--", "hi"]));
        assert!(flags.name.is_none());
    }

    // ── extract_global_flags_full (full argv scan) ────────────────────

    #[test]
    fn full_extract_name_after_command() {
        // --name after command token is extracted (unlike clap-based version)
        let (remaining, flags, help) =
            extract_global_flags_full(&sv(&["send", "--name", "vami", "@luna", "--", "hi"]));
        assert_eq!(remaining, sv(&["send", "@luna", "--", "hi"]));
        assert_eq!(flags.name, Some("vami".to_string()));
        assert!(!help);
    }

    #[test]
    fn full_extract_name_before_command() {
        let (remaining, flags, help) =
            extract_global_flags_full(&sv(&["--name", "vami", "list", "-v"]));
        assert_eq!(remaining, sv(&["list", "-v"]));
        assert_eq!(flags.name, Some("vami".to_string()));
        assert!(!help);
    }

    #[test]
    fn full_extract_respects_separator() {
        // --name after -- should NOT be extracted (it's message text)
        let (remaining, flags, _) =
            extract_global_flags_full(&sv(&["send", "@luna", "--", "--name", "not-a-flag"]));
        assert_eq!(
            remaining,
            sv(&["send", "@luna", "--", "--name", "not-a-flag"])
        );
        assert!(flags.name.is_none());
    }

    #[test]
    fn full_extract_help_detected() {
        let (remaining, flags, help) =
            extract_global_flags_full(&sv(&["send", "--name", "vami", "--help"]));
        assert_eq!(remaining, sv(&["send"]));
        assert_eq!(flags.name, Some("vami".to_string()));
        assert!(help);
    }

    #[test]
    fn full_extract_help_short() {
        let (_, _, help) = extract_global_flags_full(&sv(&["list", "-h"]));
        assert!(help);
    }

    #[test]
    fn full_extract_help_after_separator_ignored() {
        // --help in message text after -- is not a help request
        let (_, _, help) = extract_global_flags_full(&sv(&["send", "@luna", "--", "--help"]));
        assert!(!help);
    }

    #[test]
    fn full_extract_go_flag() {
        let (remaining, flags, _) = extract_global_flags_full(&sv(&["stop", "--go", "all"]));
        assert_eq!(remaining, sv(&["stop", "all"]));
        assert!(flags.go);
    }

    #[test]
    fn full_extract_combined() {
        let (remaining, flags, help) =
            extract_global_flags_full(&sv(&["config", "--name", "bot", "--go", "-h"]));
        assert_eq!(remaining, sv(&["config"]));
        assert_eq!(flags.name, Some("bot".to_string()));
        assert!(flags.go);
        assert!(help);
    }

    // ── Version / Help / NewTerminal ────────────────────────────────────

    #[test]
    fn version_flag() {
        assert_eq!(resolve_action(&sv(&["--version"])), Action::Version);
        assert_eq!(resolve_action(&sv(&["-v"])), Action::Version);
    }

    #[test]
    fn help_flag() {
        assert_eq!(resolve_action(&sv(&["--help"])), Action::Help);
        assert_eq!(resolve_action(&sv(&["-h"])), Action::Help);
    }

    #[test]
    fn new_terminal_flag() {
        assert_eq!(
            resolve_action(&sv(&["--new-terminal"])),
            Action::NewTerminal
        );
    }

    #[test]
    fn new_terminal_after_flags() {
        assert_eq!(
            resolve_action(&sv(&["--name", "foo", "--new-terminal"])),
            Action::NewTerminal
        );
    }

    // ── Hook with global flags ──────────────────────────────────────────

    #[test]
    fn hook_with_name_flag() {
        let action = resolve_action(&sv(&["--name", "foo", "sessionstart"]));
        match &action {
            Action::Hook { hook, args } => {
                assert_eq!(hook, "sessionstart");
                assert_eq!(*args, sv(&["--name", "foo", "sessionstart"]));
            }
            _ => panic!("expected Hook, got {:?}", action),
        }
    }

    // ── is_hook / is_command ────────────────────────────────────────────

    #[test]
    fn hook_registry_complete() {
        assert!(is_hook("poll"));
        assert!(is_hook("sessionstart"));
        assert!(is_hook("gemini-beforeagent"));
        assert!(is_hook("codex-notify"));
        assert!(is_hook("opencode-start"));
        assert!(!is_hook("send"));
        assert!(!is_hook("unknown"));
    }

    #[test]
    fn command_registry_complete() {
        assert!(is_command("send"));
        assert!(is_command("list"));
        assert!(is_command("run"));
        assert!(!is_command("poll"));
        assert!(!is_command("claude"));
    }

    // ── Dev root re-exec path building ──────────────────────────────────

    #[test]
    fn is_same_file_works() {
        let tmp = std::env::temp_dir().join("hcom_test_same_file");
        let _ = std::fs::write(&tmp, "test");
        assert!(is_same_file(&tmp, &tmp));
        let _ = std::fs::remove_file(&tmp);
    }
}
