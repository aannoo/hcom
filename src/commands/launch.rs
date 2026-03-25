//! Launch command: `hcom [N] <tool> [--tag X] [--terminal X] [--headless] [--hcom-prompt X] [--hcom-system-prompt X] [--batch-id X] [tool-args...]`
//!
//!
//! Parses hcom-level flags, merges env config with CLI args via tool-specific
//! parsers, then delegates to `launcher::launch()`.

use anyhow::{Result, bail};

use crate::config::HcomConfig;
use crate::db::HcomDb;
use crate::hooks::claude_args;
use crate::identity;
use crate::launcher::{self, LaunchParams};
use crate::log::log_info;
use crate::router::GlobalFlags;
use crate::shared::HcomContext;
use crate::tools::{codex_args, gemini_args};

/// Run the launch command. `argv` is the full argv[1..] including count/tool.
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    let (count, tool, hcom_flags, tool_args) = parse_launch_argv(argv)?;

    // Count validation
    if count == 0 {
        bail!("Count must be positive.");
    }
    let max_count: usize = if tool == "claude" { 100 } else { 10 };
    if count > max_count {
        bail!("Too many agents requested (max {}).", max_count);
    }

    let tag = hcom_flags.tag;
    let terminal = hcom_flags.terminal;
    let headless = hcom_flags.headless;

    // Load config for env-based args
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|_| {
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    // Merge env config args with CLI args
    let mut merged_args = merge_tool_args(&tool, &tool_args, &hcom_config);

    // Determine PTY and background
    let background = headless || is_background_from_args(&tool, &merged_args);
    let use_pty = tool != "claude" || (!background && cfg!(unix));

    // Add background defaults for Claude headless
    if tool == "claude" && background {
        let spec = claude_args::resolve_claude_args(Some(&merged_args), None);
        let updated = claude_args::add_background_defaults(&spec);
        merged_args = updated.rebuild_tokens(true);
    }

    // --go confirmation gate: preview when args present or large batch
    let ctx = HcomContext::from_os();
    if ctx.is_inside_ai_tool() && !flags.go && (!tool_args.is_empty() || count > 5) {
        print_launch_preview(&tool, count, background, &tool_args, &tag, &hcom_config);
        return Ok(0);
    }

    // System/initial prompt handling
    let system_prompt = hcom_flags.system_prompt;
    let initial_prompt = hcom_flags.initial_prompt;

    // Open DB
    let db = HcomDb::open()?;

    let launcher_name = resolve_launcher_name(
        &db,
        flags,
        std::env::var("HCOM_PROCESS_ID").ok().as_deref(),
    );
    let launcher_name_ref = launcher_name.as_str();

    // Clone for post-launch tips (originals are moved into LaunchParams)
    let tag_for_tips = tag.clone();
    let terminal_for_tips = terminal.clone();

    let result = launcher::launch(
        &db,
        LaunchParams {
            tool: tool.clone(),
            count,
            args: merged_args,
            tag,
            system_prompt,
            initial_prompt,
            pty: use_pty,
            background,
            cwd: Some(
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string()),
            ),
            env: None,
            launcher: Some(launcher_name.clone()),
            run_here: hcom_flags.run_here,
            batch_id: hcom_flags.batch_id,
            name: None, // --name is caller identity, not instance name
            skip_validation: false,
            terminal,
        },
    )?;

    // Surface errors
    if result.failed > 0 {
        for err in &result.errors {
            if let Some(msg) = err.get("error").and_then(|v| v.as_str()) {
                eprintln!("Error: {}", msg);
            }
        }
    }

    if result.launched == 0 && result.failed > 0 {
        return Ok(1);
    }

    // Print summary
    let tool_label = capitalize(&tool);
    let plural = if count != 1 { "s" } else { "" };
    if result.failed > 0 {
        println!(
            "Started the launch process for {}/{} {} agent{} ({} failed)",
            result.launched, count, tool_label, plural, result.failed
        );
    } else {
        let s = if result.launched != 1 { "s" } else { "" };
        println!(
            "Started the launch process for {} {} agent{}",
            result.launched, tool_label, s
        );
    }

    let instance_names: Vec<&str> = result
        .handles
        .iter()
        .filter_map(|h| h.get("instance_name").and_then(|v| v.as_str()))
        .collect();
    if !instance_names.is_empty() {
        println!("Names: {}", instance_names.join(", "));
    }
    println!("Batch id: {}", result.batch_id);
    println!("To block until ready or fail (30s timeout), run: hcom events launch");

    // Launch tips
    let launcher_participating = db
        .get_instance_full(launcher_name_ref)
        .ok()
        .flatten()
        .is_some();
    let detected_terminal;
    let terminal_auto_detected;
    let terminal_mode = if let Some(t) = terminal_for_tips.as_deref() {
        terminal_auto_detected = false;
        t
    } else if hcom_config.terminal != "default" && !hcom_config.terminal.is_empty() {
        terminal_auto_detected = false;
        &hcom_config.terminal
    } else {
        detected_terminal = crate::terminal::detect_terminal_from_env()
            .unwrap_or_else(|| "default".to_string());
        terminal_auto_detected = detected_terminal != "default";
        &detected_terminal
    };
    crate::core::tips::print_launch_tips(
        &db,
        result.launched,
        tag_for_tips.as_deref(),
        Some(launcher_name_ref),
        launcher_participating,
        background,
        terminal_mode,
        terminal_auto_detected,
    );

    // Log summary
    log_info(
        "launch",
        "cmd.launch",
        &format!(
            "tool={} count={} launched={} failed={} batch={}",
            tool, count, result.launched, result.failed, result.batch_id
        ),
    );

    Ok(if result.failed == 0 { 0 } else { 1 })
}

fn resolve_launcher_name(
    db: &HcomDb,
    flags: &GlobalFlags,
    process_id: Option<&str>,
) -> String {
    // Launch caller identity only needs explicit --name, then process binding.
    flags
        .name
        .as_deref()
        .map(|name| crate::instances::resolve_display_name(db, name).unwrap_or_else(|| name.to_string()))
        .or_else(|| flags.name.clone())
        .unwrap_or_else(|| {
        identity::resolve_identity(db, None, None, None, process_id, None, None)
            .map(|id| id.name)
            .unwrap_or_else(|_| "user".to_string())
        })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

/// Print launch preview when --go gate blocks inside AI tool.
fn print_launch_preview(
    tool: &str,
    count: usize,
    background: bool,
    args: &[String],
    tag: &Option<String>,
    config: &HcomConfig,
) {
    let mode = if background {
        "headless"
    } else {
        "interactive"
    };
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let args_key = format!("HCOM_{}_ARGS", tool.to_uppercase());
    let env_args = match tool {
        "claude" => &config.claude_args,
        "gemini" => &config.gemini_args,
        "codex" => &config.codex_args,
        "opencode" => &config.opencode_args,
        _ => "",
    };

    let terminal = std::env::var("HCOM_TERMINAL").unwrap_or_else(|_| config.terminal.clone());

    println!("\n== LAUNCH PREVIEW ==");
    println!("Add --go to proceed.\n");
    println!("Tool: {:<10} Count: {:<4} Mode: {}", tool, count, mode);
    println!("Directory: {}", cwd);
    println!("Terminal: {}", terminal);
    if let Some(t) = tag {
        println!("Tag: {} (names will be {}-*)", t, t);
    }

    // Args — only show if there's something to show
    if !env_args.is_empty() || !args.is_empty() {
        println!("\nArgs:");
        if !env_args.is_empty() {
            println!("  From config ({}): {}", args_key, env_args);
        }
        if !args.is_empty() {
            println!("  From CLI: {}", args.join(" "));
        }
        if !env_args.is_empty() && !args.is_empty() {
            println!("  (CLI overrides config per-flag)");
        }
    }
}

/// Hcom-level flags extracted from launch argv.
#[derive(Debug, Default)]
struct HcomLaunchFlags {
    tag: Option<String>,
    terminal: Option<String>,
    headless: bool,
    system_prompt: Option<String>,
    initial_prompt: Option<String>,
    run_here: Option<bool>,
    batch_id: Option<String>,
}

/// Parse launch argv: extract count, tool name, hcom flags, and tool-specific args.
///
/// Input forms: `[N] <tool> [--tag X] [--terminal X] [--headless] [--hcom-prompt X] [--hcom-system-prompt X] [--batch-id X] [tool-args...]`
fn parse_launch_argv(argv: &[String]) -> Result<(usize, String, HcomLaunchFlags, Vec<String>)> {
    if argv.is_empty() {
        bail!("Usage: hcom [N] <tool> [args...]");
    }

    let mut idx = 0;

    // Skip --name/--go (global flags already extracted by router)
    while idx < argv.len() {
        match argv[idx].as_str() {
            "--name" => {
                idx += 2;
                continue;
            }
            "--go" => {
                idx += 1;
                continue;
            }
            _ => break,
        }
    }

    if idx >= argv.len() {
        bail!("Missing tool name");
    }

    // Count (optional numeric prefix)
    let count: usize = if argv[idx].parse::<u32>().is_ok() {
        let c = argv[idx].parse::<usize>().unwrap_or(1);
        idx += 1;
        c
    } else {
        1
    };

    if idx >= argv.len() {
        bail!("Missing tool name after count");
    }

    // Tool name
    let tool = argv[idx].to_string();
    idx += 1;

    // Extract hcom flags from anywhere in remaining args (order-independent).
    // Everything not recognized as an hcom flag is passed through as a tool arg.
    let mut flags = HcomLaunchFlags::default();
    let mut tool_args = Vec::new();
    let remaining = &argv[idx..];
    let mut i = 0;

    while i < remaining.len() {
        // Bare `--` separates hcom flags from tool args — pass the rest through
        if remaining[i] == "--" {
            tool_args.extend_from_slice(&remaining[i + 1..]);
            break;
        }
        // Handle --tag=value and --terminal=value equals-form
        if remaining[i].starts_with("--tag=") {
            flags.tag = Some(remaining[i][6..].to_string());
            i += 1;
            continue;
        }
        if remaining[i].starts_with("--terminal=") {
            flags.terminal = Some(remaining[i][11..].to_string());
            i += 1;
            continue;
        }
        match remaining[i].as_str() {
            "--tag" if i + 1 < remaining.len() => {
                flags.tag = Some(remaining[i + 1].clone());
                i += 2;
            }
            "--terminal" if i + 1 < remaining.len() => {
                flags.terminal = Some(remaining[i + 1].clone());
                i += 2;
            }
            "--headless" => {
                flags.headless = true;
                i += 1;
            }
            "--hcom-system-prompt" if i + 1 < remaining.len() => {
                flags.system_prompt = Some(remaining[i + 1].clone());
                i += 2;
            }
            // Legacy alias
            "--system" if i + 1 < remaining.len() => {
                flags.system_prompt = Some(remaining[i + 1].clone());
                i += 2;
            }
            "--hcom-prompt" if i + 1 < remaining.len() => {
                flags.initial_prompt = Some(remaining[i + 1].clone());
                i += 2;
            }
            "--batch-id" if i + 1 < remaining.len() => {
                flags.batch_id = Some(remaining[i + 1].clone());
                i += 2;
            }
            "--run-here" => {
                flags.run_here = Some(true);
                i += 1;
            }
            "--no-run-here" => {
                flags.run_here = Some(false);
                i += 1;
            }
            // Skip global flags (--name/--go) that weren't at the start of argv
            "--name" if i + 1 < remaining.len() => {
                i += 2;
            }
            "--go" => {
                i += 1;
            }
            _ => {
                tool_args.push(remaining[i].clone());
                i += 1;
            }
        }
    }

    Ok((count, tool, flags, tool_args))
}

/// Merge env config args with CLI args via tool-specific parsers.
fn merge_tool_args(tool: &str, cli_args: &[String], config: &HcomConfig) -> Vec<String> {
    match tool {
        "claude" | "claude-pty" => {
            let env_str = &config.claude_args;
            if env_str.is_empty() {
                return cli_args.to_vec();
            }
            let env_tokens: Vec<String> =
                crate::tools::args_common::shell_split(env_str).unwrap_or_default();
            let env_spec = claude_args::resolve_claude_args(Some(&env_tokens), None);
            let cli_spec = claude_args::resolve_claude_args(Some(cli_args), None);
            let merged = claude_args::merge_claude_args(&env_spec, &cli_spec);
            merged.rebuild_tokens(true)
        }
        "gemini" => {
            let env_str = &config.gemini_args;
            if env_str.is_empty() {
                return cli_args.to_vec();
            }
            let env_tokens: Vec<String> =
                crate::tools::args_common::shell_split(env_str).unwrap_or_default();
            let env_spec = gemini_args::resolve_gemini_args(Some(&env_tokens), None);
            let cli_spec = gemini_args::resolve_gemini_args(Some(cli_args), None);
            let merged = gemini_args::merge_gemini_args(&env_spec, &cli_spec);
            merged.rebuild_tokens(true, true)
        }
        "codex" => {
            let env_str = &config.codex_args;
            if env_str.is_empty() {
                return cli_args.to_vec();
            }
            let env_tokens: Vec<String> =
                crate::tools::args_common::shell_split(env_str).unwrap_or_default();
            let env_spec = codex_args::resolve_codex_args(Some(&env_tokens), None);
            let cli_spec = codex_args::resolve_codex_args(Some(cli_args), None);
            let merged = codex_args::merge_codex_args(&env_spec, &cli_spec);
            merged.rebuild_tokens(true, true)
        }
        _ => cli_args.to_vec(), // opencode: pass through
    }
}

/// Check if args indicate background/headless mode.
fn is_background_from_args(tool: &str, args: &[String]) -> bool {
    match tool {
        "claude" | "claude-pty" => {
            let spec = claude_args::resolve_claude_args(Some(args), None);
            spec.is_background
        }
        "gemini" => {
            let spec = gemini_args::resolve_gemini_args(Some(args), None);
            spec.is_headless
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    #[test]
    fn test_parse_launch_argv_simple() {
        let (count, tool, _flags, args) = parse_launch_argv(&s(&["claude"])).unwrap();
        assert_eq!(count, 1);
        assert_eq!(tool, "claude");
        assert!(args.is_empty());
    }

    #[test]
    fn test_parse_launch_argv_with_count() {
        let (count, tool, _, args) =
            parse_launch_argv(&s(&["3", "gemini", "-m", "flash"])).unwrap();
        assert_eq!(count, 3);
        assert_eq!(tool, "gemini");
        assert_eq!(args, s(&["-m", "flash"]));
    }

    #[test]
    fn test_parse_launch_argv_with_tag() {
        let (_, tool, flags, args) =
            parse_launch_argv(&s(&["claude", "--tag", "test", "--model", "haiku"])).unwrap();
        assert_eq!(tool, "claude");
        assert_eq!(flags.tag, Some("test".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_parse_launch_argv_tag_after_tool_args() {
        // --tag after tool-specific args should still be extracted (order-independent)
        let (_, tool, flags, args) =
            parse_launch_argv(&s(&["claude", "--model", "haiku", "--tag", "test"])).unwrap();
        assert_eq!(tool, "claude");
        assert_eq!(flags.tag, Some("test".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_parse_launch_argv_headless() {
        let (_, _, flags, _) = parse_launch_argv(&s(&["claude", "--headless"])).unwrap();
        assert!(flags.headless);
    }

    #[test]
    fn test_parse_launch_argv_no_run_here() {
        let (_, _, flags, _) = parse_launch_argv(&s(&["claude", "--no-run-here"])).unwrap();
        assert_eq!(flags.run_here, Some(false));
    }

    #[test]
    fn test_parse_launch_argv_with_terminal() {
        let (_, _, flags, _) =
            parse_launch_argv(&s(&["claude", "--terminal", "kitty-tab"])).unwrap();
        assert_eq!(flags.terminal, Some("kitty-tab".to_string()));
    }

    #[test]
    fn test_parse_launch_argv_skips_global_flags() {
        let (count, tool, _, _) =
            parse_launch_argv(&s(&["--name", "bot", "--go", "2", "codex"])).unwrap();
        assert_eq!(count, 2);
        assert_eq!(tool, "codex");
    }

    #[test]
    fn test_parse_launch_argv_empty_fails() {
        assert!(parse_launch_argv(&[]).is_err());
    }

    #[test]
    fn test_merge_tool_args_passthrough() {
        let config = HcomConfig::default();
        let args = s(&["--model", "haiku"]);
        let merged = merge_tool_args("claude", &args, &config);
        assert_eq!(merged, args);
    }

    #[test]
    fn test_parse_launch_argv_name_after_tool_args() {
        // --name after tool args should be stripped, not passed as tool arg
        let (count, tool, flags, args) = parse_launch_argv(&s(&[
            "1", "claude", "--model", "haiku", "--tag", "test-cl", "--name", "nafo",
        ]))
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(tool, "claude");
        assert_eq!(flags.tag, Some("test-cl".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_parse_launch_argv_go_after_tool_args() {
        // --go after tool args should be stripped
        let (_, _, _, args) =
            parse_launch_argv(&s(&["claude", "--model", "haiku", "--go"])).unwrap();
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_parse_launch_argv_hcom_prompt() {
        let (_, _, flags, args) = parse_launch_argv(&s(&[
            "claude",
            "--hcom-prompt",
            "do the thing",
            "--model",
            "haiku",
        ]))
        .unwrap();
        assert_eq!(flags.initial_prompt, Some("do the thing".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_parse_launch_argv_hcom_system_prompt() {
        let (_, _, flags, args) = parse_launch_argv(&s(&[
            "claude",
            "--hcom-system-prompt",
            "you are helpful",
            "--model",
            "haiku",
        ]))
        .unwrap();
        assert_eq!(flags.system_prompt, Some("you are helpful".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_parse_launch_argv_system_legacy_alias() {
        let (_, _, flags, args) =
            parse_launch_argv(&s(&["claude", "--system", "you are helpful"])).unwrap();
        assert_eq!(flags.system_prompt, Some("you are helpful".to_string()));
        assert!(args.is_empty());
    }

    #[test]
    fn test_parse_launch_argv_batch_id() {
        let (_, _, flags, args) = parse_launch_argv(&s(&[
            "claude",
            "--batch-id",
            "batch-123",
            "--model",
            "haiku",
        ]))
        .unwrap();
        assert_eq!(flags.batch_id, Some("batch-123".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_is_background_claude_headless() {
        assert!(is_background_from_args(
            "claude",
            &s(&["-p", "fix tests", "--output-format", "json"])
        ));
    }

    #[test]
    fn test_is_background_claude_interactive() {
        assert!(!is_background_from_args(
            "claude",
            &s(&["--model", "haiku"])
        ));
    }

    #[test]
    fn test_resolve_launcher_name_prefers_explicit_name() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        let flags = GlobalFlags {
            name: Some("explicit".to_string()),
            go: false,
        };

        let name = resolve_launcher_name(&db, &flags, Some("pid-123"));
        assert_eq!(name, "explicit");
    }

    #[test]
    fn test_resolve_launcher_name_falls_back_to_process_binding() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        let now = crate::shared::time::now_epoch_f64();
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, directory, last_event_id, last_stop, created_at, status, status_time, status_context, tool)
                 VALUES (?1, '', '.', 0, 0, ?2, 'active', ?2, 'test', 'claude')",
                rusqlite::params!["bound", now],
            )
            .unwrap();
        db.set_process_binding("pid-123", "", "bound").unwrap();

        let name = resolve_launcher_name(&db, &GlobalFlags::default(), Some("pid-123"));
        assert_eq!(name, "bound");
    }
}
