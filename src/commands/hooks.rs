//! `hcom hooks` command — add/remove/status for tool hooks.
//!
//!
//! Manages hook installation across Claude, Gemini, Codex, and OpenCode.

use crate::db::HcomDb;
use crate::shared::CommandContext;

/// Parsed arguments for `hcom hooks`.
#[derive(clap::Parser, Debug)]
#[command(name = "hooks", about = "Manage tool hooks")]
pub struct HooksArgs {
    /// Subcommand and arguments (status/add/remove [tool])
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Valid tool names for hooks management.
const HOOK_TOOLS: &[&str] = &["claude", "gemini", "codex", "opencode", "antigravity"];

/// Get hook installation status for each tool.
fn get_tool_status() -> Vec<(&'static str, bool, String)> {
    let claude_installed = crate::hooks::claude::verify_claude_hooks_installed(None, false);
    let claude_path = crate::hooks::claude::get_claude_settings_path()
        .to_string_lossy()
        .to_string();

    let gemini_installed = crate::hooks::gemini::verify_gemini_hooks_installed(false);
    let gemini_path = crate::hooks::gemini::get_gemini_settings_path()
        .to_string_lossy()
        .to_string();

    let codex_installed = crate::hooks::codex::verify_codex_hooks_installed(false);
    let codex_path = crate::hooks::codex::get_codex_config_path()
        .to_string_lossy()
        .to_string();

    let opencode_installed = crate::hooks::opencode::verify_opencode_plugin_installed();
    let opencode_path = crate::hooks::opencode::get_opencode_plugin_path()
        .to_string_lossy()
        .to_string();

    let (antigravity_installed, antigravity_path) = {
        let (ok, path) = crate::hooks::antigravity::get_extension_status();
        (ok, path)
    };

    vec![
        ("claude", claude_installed, claude_path),
        ("gemini", gemini_installed, gemini_path),
        ("codex", codex_installed, codex_path),
        ("opencode", opencode_installed, opencode_path),
        ("antigravity", antigravity_installed, antigravity_path),
    ]
}

/// Show hook installation status for all tools.
fn cmd_hooks_status() -> i32 {
    let status = get_tool_status();
    for (tool, installed, path) in &status {
        if *installed {
            println!("{tool}:  installed    ({path})");
        } else {
            println!("{tool}:  not installed");
        }
    }
    0
}

/// Add hooks for specified tool(s).
fn cmd_hooks_add(argv: &[String]) -> i32 {
    // Get auto_approve from config
    let include_permissions = crate::config::load_config_snapshot().core.auto_approve;

    // Determine which tools to install
    let tools: Vec<&str> = if argv.is_empty() {
        // Auto-detect current tool
        let current = detect_current_tool();
        if HOOK_TOOLS.contains(&current) {
            vec![current]
        } else {
            HOOK_TOOLS.to_vec()
        }
    } else if argv[0] == "all" {
        HOOK_TOOLS.to_vec()
    } else if HOOK_TOOLS.contains(&argv[0].as_str()) {
        vec![argv[0].as_str()]
    } else {
        eprintln!("Error: Unknown tool: {}", argv[0]);
        eprintln!("Valid options: claude, gemini, codex, opencode, antigravity, all");
        return 1;
    };

    // Install hooks — propagate error detail where available
    // Outcome: "already" = was already installed, "added" = newly added, "failed" = error
    enum AddResult {
        Already,
        Added,
        Failed(Option<String>),
    }
    let mut results: Vec<(&str, AddResult)> = Vec::new();
    for tool in &tools {
        let already = match *tool {
            "claude" => {
                crate::hooks::claude::verify_claude_hooks_installed(None, include_permissions)
            }
            "gemini" => crate::hooks::gemini::verify_gemini_hooks_installed(include_permissions),
            "codex" => crate::hooks::codex::verify_codex_hooks_installed(include_permissions),
            "opencode" => crate::hooks::opencode::verify_opencode_plugin_installed(),
            "antigravity" => crate::hooks::antigravity::verify_extension_installed(),
            _ => false,
        };
        if already {
            results.push((tool, AddResult::Already));
            continue;
        }
        let outcome = match *tool {
            "claude" => match crate::hooks::claude::try_setup_claude_hooks(include_permissions) {
                Ok(()) => AddResult::Added,
                Err(e) => AddResult::Failed(Some(e.to_string())),
            },
            "gemini" => match crate::hooks::gemini::try_setup_gemini_hooks(include_permissions) {
                Ok(()) => AddResult::Added,
                Err(e) => AddResult::Failed(Some(e.to_string())),
            },
            "codex" => match crate::hooks::codex::try_setup_codex_hooks(include_permissions) {
                Ok(()) => AddResult::Added,
                Err(e) => AddResult::Failed(Some(e.to_string())),
            },
            "opencode" => match crate::hooks::opencode::install_opencode_plugin() {
                Ok(true) => AddResult::Added,
                Ok(false) => AddResult::Failed(None),
                Err(e) => AddResult::Failed(Some(e.to_string())),
            },
            "antigravity" => match crate::hooks::antigravity::install_extension() {
                Ok(true) => AddResult::Added,
                Ok(false) => AddResult::Already,
                Err(e) => AddResult::Failed(Some(e.to_string())),
            },
            _ => AddResult::Failed(None),
        };
        results.push((tool, outcome));
    }

    // Report results
    let post_status = get_tool_status();
    let mut added_count = 0;
    let mut fail_count = 0;
    for (tool, outcome) in &results {
        let path = post_status
            .iter()
            .find(|(t, _, _)| t == tool)
            .map(|(_, _, p)| p.as_str())
            .unwrap_or("");
        match outcome {
            AddResult::Already => println!("{tool} hooks already installed  ({path})"),
            AddResult::Added => {
                println!("Added {tool} hooks  ({path})");
                added_count += 1;
            }
            AddResult::Failed(Some(e)) => {
                eprintln!("Failed to add {tool} hooks: {e}");
                fail_count += 1;
            }
            AddResult::Failed(None) => {
                eprintln!("Failed to add {tool} hooks");
                fail_count += 1;
            }
        }
    }

    if added_count > 0 {
        println!();
        if tools.len() == 1 {
            let tool_name = match tools[0] {
                "claude" => "Claude Code",
                "gemini" => "Gemini CLI",
                "codex" => "Codex",
                "opencode" => "OpenCode",
                "antigravity" => "Antigravity",
                other => other,
            };
            println!("Restart {tool_name} to activate hooks.");
        } else {
            println!("Restart the tool(s) to activate hooks.");
        }
    }

    if fail_count > 0 { 1 } else { 0 }
}

/// Remove hooks for specified tool(s). Called from both `hcom hooks remove` and `hcom reset hooks`.
pub fn cmd_hooks_remove(argv: &[String]) -> i32 {
    // Determine which tools to remove
    let tools: Vec<&str> = if argv.is_empty() || (argv.len() == 1 && argv[0] == "all") {
        HOOK_TOOLS.to_vec()
    } else if HOOK_TOOLS.contains(&argv[0].as_str()) {
        vec![argv[0].as_str()]
    } else {
        eprintln!("Error: Unknown tool: {}", argv[0]);
        eprintln!("Valid options: claude, gemini, codex, opencode, antigravity, all");
        return 1;
    };

    // Check status for messaging, but always attempt removal for all paths
    // to clean up stale hooks at old paths (e.g. before env var override was set).
    let pre_status = get_tool_status();
    let mut fail_count = 0;
    for tool in &tools {
        let was_installed = pre_status
            .iter()
            .find(|(t, _, _)| t == tool)
            .map(|(_, installed, _)| *installed)
            .unwrap_or(false);

        let ok = match *tool {
            "claude" => crate::hooks::claude::remove_claude_hooks(),
            "gemini" => crate::hooks::gemini::remove_gemini_hooks(),
            "codex" => crate::hooks::codex::remove_codex_hooks(),
            "opencode" => match crate::hooks::opencode::remove_opencode_plugin() {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("Failed to remove {tool} hooks: {e}");
                    fail_count += 1;
                    continue;
                }
            },
            "antigravity" => match crate::hooks::antigravity::remove_extension() {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("Failed to remove antigravity extension: {e}");
                    fail_count += 1;
                    continue;
                }
            },
            _ => false,
        };
        if ok {
            if was_installed {
                println!("Removed {tool} hooks");
            } else {
                println!("{tool} hooks already removed");
            }
        } else {
            eprintln!("Failed to remove {tool} hooks");
            fail_count += 1;
        }
    }

    if fail_count > 0 { 1 } else { 0 }
}

/// Detect current AI tool from environment.
fn detect_current_tool() -> &'static str {
    crate::shared::detect_current_tool_from_env()
}

pub fn cmd_hooks(_db: &HcomDb, args: &HooksArgs, _ctx: Option<&CommandContext>) -> i32 {
    let argv = &args.args;
    if argv.is_empty() {
        // No args = show status
        return cmd_hooks_status();
    }

    let first = argv[0].as_str();

    if first == "--help" || first == "-h" {
        println!(
            "hcom hooks - Manage tool hooks for hcom integration\n\n\
             Hooks enable automatic message delivery and status tracking. Without hooks,\n\
             you can still use hcom in ad-hoc mode (run hcom start in any ai tool).\n\n\
              Usage:\n  \
              hcom hooks                  Show hook status for all tools\n  \
              hcom hooks status           Same as above\n  \
              hcom hooks add [tool]       Add hooks (claude|gemini|codex|opencode|antigravity|all)\n  \
              hcom hooks remove [tool]    Remove hooks (claude|gemini|codex|opencode|antigravity|all)\n\n\
             Examples:\n  \
             hcom hooks add claude       Add Claude Code hooks only\n  \
             hcom hooks add              Auto-detect tool or add all\n  \
             hcom hooks remove all       Remove all hooks\n\n\
             After adding, restart the tool to activate hooks."
        );
        return 0;
    }

    let sub_argv = argv[1..].to_vec();

    match first {
        "status" => cmd_hooks_status(),
        "add" | "install" => cmd_hooks_add(&sub_argv),
        "remove" | "uninstall" => cmd_hooks_remove(&sub_argv),
        _ => {
            eprintln!("Error: Unknown hooks subcommand: {first}");
            eprintln!("Usage: hcom hooks [status|add|remove] [tool]");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_current_tool_default() {
        // In test env, none of the AI tool vars should be set
        // (unless running inside one, which is fine — it'll detect it)
        let tool = detect_current_tool();
        assert!(
            ["claude", "gemini", "codex", "opencode", "adhoc"].contains(&tool),
            "unexpected tool: {tool}"
        );
    }
}
