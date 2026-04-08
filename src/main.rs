//! hcom — inter-agent communication for AI coding tools.
//!
//! Humans usually launch agents with `hcom <tool>` and talk to them through
//! each tool's own UI. Agents use `hcom` CLI commands (learnt through bootstrap.rs)
//! as a side-channel for messaging and coordination with other agents.

mod bootstrap;
mod cli_context;
pub mod commands;
mod config;
pub mod core;
mod db;
mod delivery;
pub mod hooks;
pub mod identity;
mod instance_names;
mod instances;
pub mod launcher;
mod log;
pub mod messages;
mod notify;
mod paths;
mod pidtrack;
mod pty;
pub mod relay;
pub mod router;
mod runtime_env;
pub mod scripts;
pub mod shared;
pub mod terminal;
mod tool;
pub mod tools;
mod transcript;
mod tui;
mod update;

use anyhow::{Context, Result, bail};
use std::panic;
use std::str::FromStr;

fn main() -> Result<()> {
    // Initialize global config from environment variables
    config::Config::init();

    // Set custom panic hook to log to file instead of stderr (prevents TUI corruption)
    panic::set_hook(Box::new(|panic_info| {
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        log::log_error("native", "panic", &format!("{} at {}", message, location));
    }));

    // Dispatch via router (replaces manual MainAction matching)
    router::dispatch()
}

/// Run PTY wrapper mode.
pub fn run_pty(args: &[String]) -> Result<()> {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        eprintln!("hcom pty - PTY wrapper for hcom");
        eprintln!();
        eprintln!("Usage: hcom pty <tool> [args...]");
        eprintln!();
        eprintln!("Tools: claude, gemini, codex");
        eprintln!();
        eprintln!("The PTY wrapper provides:");
        eprintln!("  - Text injection via TCP port (INJECT_PORT)");
        eprintln!("  - State queries via TCP port (STATE_PORT)");
        eprintln!("  - Ready detection for tool startup");
        eprintln!();
        eprintln!("Environment:");
        eprintln!("  HCOM_INSTANCE_NAME    Instance name for logging");
        eprintln!("  HCOM_DIR              Custom hcom directory");
        if args.is_empty() {
            bail!("Tool name required");
        }
        return Ok(());
    }

    let tool_str = &args[0];
    let tool_args: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();

    // Parse tool - use enum for known tools, raw string for testing arbitrary commands
    let (ready_pattern, tool_name) = match tool::Tool::from_str(tool_str) {
        Ok(tool) => (tool.ready_pattern().to_vec(), tool_str.to_string()),
        Err(_) => (vec![], tool_str.to_string()), // Allow arbitrary commands for testing
    };

    let instance_name = config::Config::get().instance_name;

    // Resolve tool to full path (PATH may be minimal in launched environments)
    let resolved = terminal::which_bin(tool_str).unwrap_or_else(|| tool_str.to_string());

    // On Termux, some wrapped tools need a launcher override instead of direct exec.
    let (command, extra_args): (String, Vec<String>);
    if let Some((launcher, prefix_args)) =
        terminal::resolve_termux_tool_launcher(tool_str, &resolved)
    {
        command = launcher;
        extra_args = prefix_args;
    } else {
        command = resolved;
        extra_args = vec![];
    }
    let full_args: Vec<&str> = extra_args
        .iter()
        .map(|s| s.as_str())
        .chain(tool_args.iter().copied())
        .collect();

    // Create and run PTY
    let mut proxy = pty::Proxy::spawn(
        &command,
        &full_args,
        pty::ProxyConfig {
            ready_pattern,
            instance_name,
            tool: tool_name,
        },
    )
    .context("Failed to spawn PTY")?;

    let exit_code = proxy.run().context("PTY run failed")?;

    // Drop proxy to run cleanup (join delivery thread, which does DB cleanup)
    drop(proxy);

    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use crate::router::{self, Action};

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    /// Test that no args runs Rust TUI
    #[test]
    fn test_no_args_runs_rust_tui() {
        let action = router::resolve_action(&[]);
        assert_eq!(action, Action::Tui);
    }

    /// Test that PTY mode is correctly identified
    #[test]
    fn test_pty_mode() {
        let action = router::resolve_action(&args(&["pty", "claude"]));
        assert_eq!(
            action,
            Action::Pty {
                args: args(&["claude"])
            }
        );
    }

    /// Test that client mode is correctly identified for non-pty commands
    #[test]
    fn test_client_mode() {
        let action = router::resolve_action(&args(&["list"]));
        match action {
            Action::Command { cmd, .. } => assert_eq!(cmd, "list"),
            _ => panic!("Expected Command action, got {:?}", action),
        }
    }

    /// Test PTY mode with multiple args
    #[test]
    fn test_pty_mode_with_args() {
        let action = router::resolve_action(&args(&["pty", "claude", "--arg1", "--arg2"]));
        assert_eq!(
            action,
            Action::Pty {
                args: args(&["claude", "--arg1", "--arg2"])
            }
        );
    }
}
