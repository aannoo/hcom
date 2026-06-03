//! `hcom hooks` command — add/remove/status for tool hooks.
//!
//!
//! Manages hook installation across Claude, Gemini, Codex, and OpenCode.

use crate::db::HcomDb;
use crate::shared::CommandContext;
use crate::tool::Tool;

/// Parsed arguments for `hcom hooks`.
#[derive(clap::Parser, Debug)]
#[command(name = "hooks", about = "Manage tool hooks")]
pub struct HooksArgs {
    /// Subcommand and arguments (status/add/remove [tool])
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Valid tool names for hooks management. Must stay in sync with released
/// hook-bearing specs in `integration_spec.rs` — see
/// `router::tests::hook_tools_match_released_specs_with_hooks` for the guard.
pub(crate) const HOOK_TOOLS: &[&str] = &[
    "claude",
    "gemini",
    "codex",
    "opencode",
    "kilo",
    "antigravity",
    "cursor",
    "kimi",
];

/// Refresh permission state for hook integrations that are already installed.
///
/// This is used after `auto_approve` changes. It intentionally skips tools
/// without installed hooks so changing one preference does not install new
/// integrations as a side effect.
pub(crate) fn refresh_installed_hook_permissions(enabled: bool) -> Vec<(&'static str, String)> {
    let mut failures = Vec::new();
    for name in HOOK_TOOLS {
        let Ok(tool) = name.parse::<Tool>() else {
            continue;
        };
        if !tool.verify_hooks_installed(false) {
            continue;
        }
        if let Err(error) = tool.try_setup_hooks(enabled) {
            failures.push((*name, error));
        }
    }
    failures
}

/// Get hook installation status for each tool.
///
/// Iterates [`HOOK_TOOLS`] and routes through the `Tool::*` hook adapter so a
/// new hook-bearing tool only needs a `Tool` arm + a HOOK_TOOLS entry.
fn get_tool_status() -> Vec<(&'static str, bool, String)> {
    HOOK_TOOLS
        .iter()
        .filter_map(|name| {
            let tool = name.parse::<Tool>().ok()?;
            Some((
                *name,
                tool.verify_hooks_installed(false),
                tool.hooks_settings_path(),
            ))
        })
        .collect()
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
        eprintln!("Valid options: claude, gemini, codex, opencode, kilo, antigravity, cursor, kimi, all");
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
        let Ok(parsed) = tool.parse::<Tool>() else {
            results.push((tool, AddResult::Failed(None)));
            continue;
        };
        if parsed.verify_hooks_installed(include_permissions) {
            results.push((tool, AddResult::Already));
            continue;
        }
        let outcome = match parsed.try_setup_hooks(include_permissions) {
            Ok(()) => AddResult::Added,
            Err(msg) if msg.is_empty() => AddResult::Failed(None),
            Err(msg) => AddResult::Failed(Some(msg)),
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
                "kilo" => "Kilo Code",
                "antigravity" => "Antigravity",
                "cursor" => "Cursor Agent",
                "kimi" => "Kimi Code",
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
        eprintln!("Valid options: claude, gemini, codex, opencode, kilo, antigravity, cursor, kimi, all");
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

        let ok = match tool.parse::<Tool>().map(|t| t.remove_hooks()) {
            Ok(Ok(ok)) => ok,
            Ok(Err(e)) => {
                eprintln!("Failed to remove {tool} hooks: {e}");
                fail_count += 1;
                continue;
            }
            Err(_) => false,
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
             hcom hooks add [tool]       Add hooks (claude|gemini|codex|opencode|kilo|antigravity|cursor|kimi|all)\n  \
             hcom hooks remove [tool]    Remove hooks (claude|gemini|codex|opencode|kilo|antigravity|cursor|kimi|all)\n\n\
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
            [
                "claude",
                "gemini",
                "codex",
                "opencode",
                "antigravity",
                "adhoc"
            ]
            .contains(&tool),
            "unexpected tool: {tool}"
        );
    }
}
