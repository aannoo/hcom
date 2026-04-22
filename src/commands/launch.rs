//! Launch command: `hcom [N] <tool> [--tag X] [--terminal X] [--headless] [--hcom-prompt X] [--hcom-system-prompt X] [--batch-id X] [tool-args...]`
//!
//!
//! Parses hcom-level flags, merges env config with CLI args via tool-specific
//! parsers, then delegates to `launcher::launch()`.

use crate::config::HcomConfig;
use crate::core::tips::{self, LaunchTipsContext};
use crate::db::HcomDb;
use crate::hooks::claude_args;
use crate::identity;
use crate::launcher::{self, LaunchParams, LaunchResult};
use crate::log::log_info;
use crate::router::GlobalFlags;
use crate::shared::HcomContext;
use crate::tools::{codex_args, gemini_args};
use anyhow::{Result, bail};
use serde_json::json;

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
    let remote_device = hcom_flags.device.clone();
    let dir_override = hcom_flags.dir.clone();
    let tag_for_output = tag.clone();
    let terminal_for_output = terminal.clone();

    let hcom_config = load_hcom_config();
    let preview_background = headless || is_background_from_args(&tool, &tool_args);

    let ctx = HcomContext::from_os();
    if ctx.is_inside_ai_tool() && !flags.go && (!tool_args.is_empty() || count > 5) {
        let remote_launch_note = "Remote launch requested; the target device will still apply its own configured defaults.";
        let remote_preview_note = "Mode shown here is only a local preview; the remote target decides the final launch mode.";
        let notes = if remote_device.is_some() {
            [remote_launch_note, remote_preview_note]
        } else {
            ["", ""]
        };
        print_launch_preview(LaunchPreview {
            action: "launch",
            tool: &tool,
            count,
            background: preview_background,
            args: &tool_args,
            tag: tag.as_deref(),
            cwd: dir_override.as_deref(),
            terminal: terminal.as_deref(),
            config: &hcom_config,
            show_config_args: remote_device.is_none(),
            notes: if remote_device.is_some() { &notes } else { &[] },
        });
        return Ok(0);
    }

    if let Some(ref device) = remote_device {
        if hcom_flags.run_here == Some(true) {
            bail!("Remote launch does not support --run-here");
        }
        let remote_cwd = dir_override.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Remote launch requires --dir to specify the working directory on the target device"
            )
        })?;
        let db = HcomDb::open()?;
        let launcher_name =
            resolve_launcher_name(&db, flags, std::env::var("HCOM_PROCESS_ID").ok().as_deref());
        let params = json!({
            "tool": tool,
            "count": count,
            "args": tool_args,
            "tag": tag,
            "launcher": launcher_name,
            "background": headless,
            "terminal": terminal,
            "cwd": remote_cwd,
            "initial_prompt": hcom_flags.initial_prompt,
            "system_prompt": hcom_flags.system_prompt,
        });

        match crate::relay::control::dispatch_remote(
            &db,
            device,
            None,
            crate::relay::control::rpc_action::LAUNCH,
            &params,
            crate::relay::control::RPC_LAUNCH_TIMEOUT,
        ) {
            Ok(inner) => {
                let launch_result = launch_result_from_json(&inner).map_err(anyhow::Error::msg)?;
                let remote_output = build_remote_launch_output(
                    &db,
                    flags,
                    &launch_result,
                    tag_for_output.clone(),
                    terminal_for_output.clone(),
                    hcom_flags.run_here,
                );
                let output = LaunchOutputContext {
                    action: "launch",
                    tool: &remote_output.tool,
                    requested_count: count,
                    tag: remote_output.tag.as_deref(),
                    launcher_name: &remote_output.launcher_name,
                    terminal: remote_output.terminal.as_deref(),
                    background: remote_output.background,
                    run_here: remote_output.run_here,
                    hcom_config: &hcom_config,
                };
                print_launch_feedback(&db, &launch_result, &output)?;
                return Ok(0);
            }
            Err(e) => bail!("Remote launch failed for device {device}: {e}"),
        }
    }

    // Merge env config args with CLI args
    let (merged_args, background, use_pty) =
        prepare_launch_execution(&tool, &tool_args, &hcom_config, headless);

    // System/initial prompt handling
    let system_prompt = hcom_flags.system_prompt;
    let initial_prompt = hcom_flags.initial_prompt;

    // Open DB
    let db = HcomDb::open()?;

    let launcher_name =
        resolve_launcher_name(&db, flags, std::env::var("HCOM_PROCESS_ID").ok().as_deref());
    let launcher_name_ref = launcher_name.as_str();

    let output = LaunchOutputContext {
        action: "launch",
        tool: &tool,
        requested_count: count,
        tag: tag_for_output.as_deref(),
        launcher_name: launcher_name_ref,
        terminal: terminal_for_output.as_deref(),
        background,
        run_here: hcom_flags.run_here,
        hcom_config: &hcom_config,
    };

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
            cwd: Some(if let Some(ref dir) = dir_override {
                let path = std::path::Path::new(dir);
                if !path.is_dir() {
                    bail!("--dir path does not exist or is not a directory: {}", dir);
                }
                path.canonicalize()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| dir.clone())
            } else {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            }),
            env: None,
            launcher: Some(launcher_name.clone()),
            run_here: hcom_flags.run_here,
            batch_id: hcom_flags.batch_id,
            name: None, // --name is caller identity, not instance name
            skip_validation: false,
            terminal,
            append_reply_handoff: true,
        },
    )?;

    print_launch_feedback(&db, &result, &output)?;

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

pub(crate) fn prepare_launch_execution(
    tool: &str,
    cli_args: &[String],
    config: &HcomConfig,
    headless: bool,
) -> (Vec<String>, bool, bool) {
    let mut merged_args = merge_tool_args(tool, cli_args, config);
    let background = headless || is_background_from_args(tool, &merged_args);
    let use_pty = tool != "claude" || (!background && cfg!(unix));

    if tool == "claude" && background {
        let spec = claude_args::resolve_claude_args(Some(&merged_args), None);
        let updated = claude_args::add_background_defaults(&spec);
        merged_args = updated.rebuild_tokens(true);
    }

    (merged_args, background, use_pty)
}

pub(crate) fn launch_result_to_json(result: &LaunchResult) -> serde_json::Value {
    serde_json::to_value(result).unwrap_or_else(|_| json!({}))
}

pub(crate) fn launch_result_from_json(value: &serde_json::Value) -> Result<LaunchResult, String> {
    serde_json::from_value(value.clone()).map_err(|e| e.to_string())
}

struct RemoteLaunchOutput {
    tool: String,
    tag: Option<String>,
    launcher_name: String,
    terminal: Option<String>,
    background: bool,
    run_here: Option<bool>,
}

fn build_remote_launch_output(
    db: &HcomDb,
    flags: &GlobalFlags,
    launch_result: &LaunchResult,
    tag: Option<String>,
    terminal: Option<String>,
    run_here: Option<bool>,
) -> RemoteLaunchOutput {
    let launcher_name =
        resolve_launcher_name(db, flags, std::env::var("HCOM_PROCESS_ID").ok().as_deref());
    RemoteLaunchOutput {
        tool: launch_result.tool.clone(),
        tag,
        launcher_name,
        terminal,
        background: launch_result.background,
        run_here,
    }
}

pub(crate) fn resolve_launcher_name(
    db: &HcomDb,
    flags: &GlobalFlags,
    process_id: Option<&str>,
) -> String {
    // Launch caller identity only needs explicit --name, then process binding.
    flags
        .name
        .as_deref()
        .map(|name| {
            crate::instances::resolve_display_name(db, name).unwrap_or_else(|| name.to_string())
        })
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
pub(crate) struct LaunchPreview<'a> {
    pub action: &'a str,
    pub tool: &'a str,
    pub count: usize,
    pub background: bool,
    pub args: &'a [String],
    pub tag: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub terminal: Option<&'a str>,
    pub config: &'a HcomConfig,
    pub show_config_args: bool,
    pub notes: &'a [&'a str],
}

pub(crate) fn print_launch_preview(preview: LaunchPreview<'_>) {
    let mode = if preview.background {
        "headless"
    } else {
        "interactive"
    };
    let cwd = preview.cwd.map(|s| s.to_string()).unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });
    let args_key = format!("HCOM_{}_ARGS", preview.tool.to_uppercase());
    let env_args = if preview.show_config_args {
        match preview.tool {
            "claude" => &preview.config.claude_args,
            "gemini" => &preview.config.gemini_args,
            "codex" => &preview.config.codex_args,
            "opencode" => &preview.config.opencode_args,
            _ => "",
        }
    } else {
        ""
    };

    let terminal = preview
        .terminal
        .map(|s| s.to_string())
        .or_else(|| std::env::var("HCOM_TERMINAL").ok())
        .unwrap_or_else(|| preview.config.terminal.clone());

    println!("\n== LAUNCH PREVIEW ==");
    println!("Add --go to proceed.\n");
    println!("Action: {}", preview.action);
    println!(
        "Tool: {:<10} Count: {:<4} Mode: {}",
        preview.tool, preview.count, mode
    );
    println!("Directory: {}", cwd);
    println!("Terminal: {}", terminal);
    if let Some(t) = preview.tag {
        println!("Tag: {} (names will be {}-*)", t, t);
    }
    for note in preview.notes {
        println!("{note}");
    }

    // Args — only show if there's something to show
    if !env_args.is_empty() || !preview.args.is_empty() {
        println!("\nArgs:");
        if !env_args.is_empty() {
            println!("  From config ({}): {}", args_key, env_args);
        }
        if !preview.args.is_empty() {
            println!("  From CLI: {}", preview.args.join(" "));
        }
        if !env_args.is_empty() && !preview.args.is_empty() {
            println!("  (CLI overrides config per-flag)");
        }
    }
}

/// Hcom-level flags extracted from launch argv.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HcomLaunchFlags {
    pub tag: Option<String>,
    pub terminal: Option<String>,
    pub device: Option<String>,
    pub headless: bool,
    pub system_prompt: Option<String>,
    pub initial_prompt: Option<String>,
    pub run_here: Option<bool>,
    pub batch_id: Option<String>,
    pub dir: Option<String>,
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

    let (flags, tool_args) = extract_launch_flags(&argv[idx..]);

    Ok((count, tool, flags, tool_args))
}

/// Merge env config args with CLI args via tool-specific parsers.
pub(crate) fn merge_tool_args(tool: &str, cli_args: &[String], config: &HcomConfig) -> Vec<String> {
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
pub(crate) fn is_background_from_args(tool: &str, args: &[String]) -> bool {
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

pub(crate) fn load_hcom_config() -> HcomConfig {
    HcomConfig::load(None).unwrap_or_else(|_| {
        let mut c = HcomConfig::default();
        c.normalize();
        c
    })
}

pub(crate) fn extract_launch_flags(args: &[String]) -> (HcomLaunchFlags, Vec<String>) {
    let mut flags = HcomLaunchFlags::default();
    let mut tool_args = Vec::new();
    let mut i = 0;

    while i < args.len() {
        if args[i] == "--" {
            tool_args.extend_from_slice(&args[i + 1..]);
            break;
        }
        if args[i].starts_with("--tag=") {
            flags.tag = Some(args[i][6..].to_string());
            i += 1;
            continue;
        }
        if args[i].starts_with("--terminal=") {
            flags.terminal = Some(args[i][11..].to_string());
            i += 1;
            continue;
        }
        if args[i].starts_with("--device=") {
            flags.device = Some(args[i][9..].to_string());
            i += 1;
            continue;
        }
        if args[i].starts_with("--dir=") {
            flags.dir = Some(args[i][6..].to_string());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--tag" if i + 1 < args.len() => {
                flags.tag = Some(args[i + 1].clone());
                i += 2;
            }
            "--terminal" if i + 1 < args.len() => {
                flags.terminal = Some(args[i + 1].clone());
                i += 2;
            }
            "--device" if i + 1 < args.len() => {
                flags.device = Some(args[i + 1].clone());
                i += 2;
            }
            "--dir" if i + 1 < args.len() => {
                flags.dir = Some(args[i + 1].clone());
                i += 2;
            }
            "--headless" => {
                flags.headless = true;
                i += 1;
            }
            "--hcom-system-prompt" if i + 1 < args.len() => {
                flags.system_prompt = Some(args[i + 1].clone());
                i += 2;
            }
            "--system" if i + 1 < args.len() => {
                flags.system_prompt = Some(args[i + 1].clone());
                i += 2;
            }
            "--hcom-prompt" if i + 1 < args.len() => {
                flags.initial_prompt = Some(args[i + 1].clone());
                i += 2;
            }
            "--batch-id" if i + 1 < args.len() => {
                flags.batch_id = Some(args[i + 1].clone());
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
            "--name" if i + 1 < args.len() => {
                i += 2;
            }
            "--go" => {
                i += 1;
            }
            _ => {
                tool_args.push(args[i].clone());
                i += 1;
            }
        }
    }

    (flags, tool_args)
}

pub(crate) struct LaunchOutputContext<'a> {
    pub action: &'a str,
    pub tool: &'a str,
    pub requested_count: usize,
    pub tag: Option<&'a str>,
    pub launcher_name: &'a str,
    pub terminal: Option<&'a str>,
    pub background: bool,
    pub run_here: Option<bool>,
    pub hcom_config: &'a HcomConfig,
}

pub(crate) fn print_launch_feedback(
    db: &HcomDb,
    result: &LaunchResult,
    ctx: &LaunchOutputContext<'_>,
) -> Result<()> {
    if result.failed > 0 {
        for err in &result.errors {
            if let Some(msg) = err.get("error").and_then(|v| v.as_str()) {
                eprintln!("Error: {}", msg);
            }
        }
    }

    if result.launched == 0 && result.failed > 0 {
        return Ok(());
    }

    let tool_label = capitalize(ctx.tool);
    let plural = if ctx.requested_count != 1 { "s" } else { "" };
    if result.failed > 0 {
        println!(
            "Started the {} process for {}/{} {} agent{} ({} failed)",
            ctx.action, result.launched, ctx.requested_count, tool_label, plural, result.failed
        );
    } else {
        let s = if result.launched != 1 { "s" } else { "" };
        println!(
            "Started the {} process for {} {} agent{}",
            ctx.action, result.launched, tool_label, s
        );
    }

    let instance_names: Vec<&str> = result
        .handles
        .iter()
        .filter_map(|h| h.get("instance_name").and_then(|v| v.as_str()))
        .collect();
    if !instance_names.is_empty() {
        println!("Names: {}", instance_names.join(" "));
    }
    println!("Batch id: {}", result.batch_id);
    println!("To block until ready or fail (30s timeout), run: hcom events launch");

    let launcher_participating = db
        .get_instance_full(ctx.launcher_name)
        .ok()
        .flatten()
        .is_some();
    let (terminal_mode, terminal_auto_detected) = crate::terminal::resolve_terminal_mode_for_tips(
        ctx.terminal,
        &ctx.hcom_config.terminal,
        ctx.background,
        ctx.run_here.unwrap_or(false),
    );
    tips::print_launch_tips(
        db,
        LaunchTipsContext {
            launched: result.launched,
            tag: ctx.tag,
            launcher_name: Some(ctx.launcher_name),
            launcher_participating,
            background: ctx.background,
            terminal_mode: &terminal_mode,
            terminal_auto_detected,
        },
    );
    Ok(())
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
    fn test_parse_launch_argv_device() {
        let (_, _, flags, args) =
            parse_launch_argv(&s(&["claude", "--device", "ABCD", "--model", "haiku"])).unwrap();
        assert_eq!(flags.device, Some("ABCD".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_prepare_launch_execution_adds_claude_background_defaults() {
        let config = HcomConfig::default();
        let (args, background, use_pty) =
            prepare_launch_execution("claude", &s(&["-p"]), &config, true);
        assert!(background);
        assert!(!use_pty);

        let spec = crate::hooks::claude_args::resolve_claude_args(Some(&args), None);
        assert!(spec.has_flag(&["--output-format"], &["--output-format="]));
        assert!(spec.has_flag(&["--verbose"], &[]));
    }

    #[test]
    fn test_launch_result_json_roundtrip() {
        let result = LaunchResult {
            tool: "claude".to_string(),
            batch_id: "batch-1".to_string(),
            launched: 1,
            failed: 0,
            background: true,
            log_files: vec!["/tmp/test.log".to_string()],
            handles: vec![serde_json::json!({"instance_name": "luna"})],
            errors: Vec::new(),
        };
        let parsed = launch_result_from_json(&launch_result_to_json(&result)).unwrap();
        assert_eq!(parsed.tool, "claude");
        assert_eq!(parsed.batch_id, "batch-1");
        assert_eq!(parsed.launched, 1);
        assert!(parsed.background);
    }

    #[test]
    fn test_build_remote_launch_output_prefers_remote_background() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();

        let output = build_remote_launch_output(
            &db,
            &GlobalFlags::default(),
            &LaunchResult {
                tool: "claude".to_string(),
                batch_id: "batch-1".to_string(),
                launched: 1,
                failed: 0,
                background: false,
                log_files: Vec::new(),
                handles: Vec::new(),
                errors: Vec::new(),
            },
            Some("ops".to_string()),
            Some("kitty".to_string()),
            Some(false),
        );

        assert_eq!(output.tool, "claude");
        assert_eq!(output.tag.as_deref(), Some("ops"));
        assert_eq!(output.terminal.as_deref(), Some("kitty"));
        assert!(!output.background);
        assert_eq!(output.run_here, Some(false));
    }

    #[test]
    fn test_build_remote_launch_output_uses_remote_launch_result_background() {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();

        let output = build_remote_launch_output(
            &db,
            &GlobalFlags::default(),
            &LaunchResult {
                tool: "codex".to_string(),
                batch_id: "batch-2".to_string(),
                launched: 1,
                failed: 0,
                background: false,
                log_files: Vec::new(),
                handles: Vec::new(),
                errors: Vec::new(),
            },
            None,
            None,
            None,
        );

        assert_eq!(output.tool, "codex");
        assert!(!output.background);
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

    #[test]
    fn test_parse_launch_argv_dir_flag() {
        let (_, _, flags, args) =
            parse_launch_argv(&s(&["claude", "--dir", "/tmp/project", "--model", "haiku"]))
                .unwrap();
        assert_eq!(flags.dir, Some("/tmp/project".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_parse_launch_argv_dir_equals() {
        let (_, _, flags, args) =
            parse_launch_argv(&s(&["claude", "--dir=/tmp/project", "--model", "haiku"])).unwrap();
        assert_eq!(flags.dir, Some("/tmp/project".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_parse_launch_argv_dir_not_passed_to_tool() {
        let (_, _, flags, args) =
            parse_launch_argv(&s(&["gemini", "--dir", "/tmp/proj", "-m", "flash"])).unwrap();
        assert_eq!(flags.dir, Some("/tmp/proj".to_string()));
        assert_eq!(args, s(&["-m", "flash"]));
    }
}
