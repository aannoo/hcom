//! hcom: High-performance PTY wrapper, daemon client, and TUI
//!
//! Modes:
//!   hcom pty <tool> [args...]   - PTY wrapper mode
//!   hcom <command> [args...]    - Daemon client mode (hooks/CLI)
//!   hcom                        - TUI mode (Rust default, Python if HCOM_PYTHON_FALLBACK=1)
//!
//! PTY mode outputs on startup:
//!   INJECT_PORT=<port>   - TCP port for text injection
//!   STATE_PORT=<port>    - TCP port for state queries
//!   READY                - Signal that PTY is ready for use

mod client;
mod config;
mod db;
mod delivery;
mod log;
mod notify;
mod paths;
mod pty;
mod tool;
mod transcript;
mod tui;

use anyhow::{Context, Result, bail};
use std::env;
use std::panic;
use std::str::FromStr;

/// Action to take based on command-line arguments
#[derive(Debug, PartialEq)]
enum MainAction {
    /// Run PTY wrapper mode with tool args
    RunPty(Vec<String>),
    /// Run daemon client mode with command args
    RunClient(Vec<String>),
    /// Run Rust TUI (default)
    RunTui,
    /// Fallback to Python TUI (HCOM_PYTHON_FALLBACK=1)
    FallbackToPython,
}

/// Determine what action to take based on command-line arguments
fn determine_action(args: &[String]) -> MainAction {
    if args.len() < 2 {
        if env::var("HCOM_PYTHON_FALLBACK").as_deref() == Ok("1") {
            return MainAction::FallbackToPython;
        }
        return MainAction::RunTui;
    }

    match args[1].as_str() {
        "pty" => MainAction::RunPty(args[2..].to_vec()),
        _ => MainAction::RunClient(args[1..].to_vec()),
    }
}

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

    let args: Vec<String> = env::args().collect();

    match determine_action(&args) {
        MainAction::RunTui => {
            tui::run().map_err(|e| anyhow::anyhow!("{e:#}"))?;
        }
        MainAction::FallbackToPython => {
            client::exec_python_fallback(&[]);
        }
        MainAction::RunPty(pty_args) => {
            run_pty(&pty_args)?;
        }
        MainAction::RunClient(client_args) => {
            client::run(&client_args)?;
        }
    }

    Ok(())
}

fn run_pty(args: &[String]) -> Result<()> {
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

    // Build command (use original string for execve)
    let command = tool_str.as_str();

    // Create and run PTY
    let mut proxy = pty::Proxy::spawn(
        command,
        &tool_args,
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
    use super::*;
    use serial_test::serial;
    use std::env;

    fn with_python_fallback<F>(value: Option<&str>, f: F)
    where
        F: FnOnce(),
    {
        let saved = env::var("HCOM_PYTHON_FALLBACK").ok();

        if let Some(v) = value {
            // SAFETY: Test-only env manipulation with serial test execution.
            unsafe { env::set_var("HCOM_PYTHON_FALLBACK", v) };
        } else {
            // SAFETY: Test-only env manipulation with serial test execution.
            unsafe { env::remove_var("HCOM_PYTHON_FALLBACK") };
        }

        f();

        if let Some(v) = saved {
            // SAFETY: Test-only env manipulation with serial test execution.
            unsafe { env::set_var("HCOM_PYTHON_FALLBACK", v) };
        } else {
            // SAFETY: Test-only env manipulation with serial test execution.
            unsafe { env::remove_var("HCOM_PYTHON_FALLBACK") };
        }
    }

    /// Test that no args runs Rust TUI by default
    #[test]
    #[serial]
    fn test_no_args_runs_rust_tui() {
        with_python_fallback(None, || {
            let args = vec!["hcom".to_string()];
            assert_eq!(determine_action(&args), MainAction::RunTui);
        });
    }

    /// Test that no args with HCOM_PYTHON_FALLBACK=1 falls back to Python TUI
    #[test]
    #[serial]
    fn test_no_args_with_python_fallback_env() {
        with_python_fallback(Some("1"), || {
            let args = vec!["hcom".to_string()];
            assert_eq!(determine_action(&args), MainAction::FallbackToPython);
        });
    }

    /// Test that PTY mode is correctly identified
    #[test]
    fn test_pty_mode() {
        let args = vec!["hcom".to_string(), "pty".to_string(), "claude".to_string()];
        match determine_action(&args) {
            MainAction::RunPty(pty_args) => {
                assert_eq!(pty_args, vec!["claude".to_string()]);
            }
            _ => panic!("Expected RunPty action"),
        }
    }

    /// Test that client mode is correctly identified for non-pty commands
    #[test]
    fn test_client_mode() {
        let args = vec!["hcom".to_string(), "list".to_string()];
        match determine_action(&args) {
            MainAction::RunClient(client_args) => {
                assert_eq!(client_args, vec!["list".to_string()]);
            }
            _ => panic!("Expected RunClient action"),
        }
    }

    /// Test PTY mode with multiple args
    #[test]
    fn test_pty_mode_with_args() {
        let args = vec![
            "hcom".to_string(),
            "pty".to_string(),
            "claude".to_string(),
            "--arg1".to_string(),
            "--arg2".to_string(),
        ];
        match determine_action(&args) {
            MainAction::RunPty(pty_args) => {
                assert_eq!(
                    pty_args,
                    vec![
                        "claude".to_string(),
                        "--arg1".to_string(),
                        "--arg2".to_string()
                    ]
                );
            }
            _ => panic!("Expected RunPty action"),
        }
    }
}
