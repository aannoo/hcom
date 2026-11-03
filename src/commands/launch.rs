//! Launch command: `hcom [N] <tool> [--tag X] [--terminal X] [--headless] [--hcom-prompt X] [--hcom-system-prompt X] [--batch-id X] [tool-args...]`
//!
//!
//! Parses hcom-level flags, merges env config with CLI args via tool-specific
//! parsers, then delegates to `launcher::launch()`.

use crate::config::HcomConfig;
use crate::core::launch_status::{self, LaunchStatus};
use crate::core::tips::{self, LaunchTipsContext};
use crate::db::HcomDb;
use crate::identity;
use crate::launcher::{self, LaunchParams, LaunchResult, LaunchTool};
use crate::log::log_info;
use crate::router::GlobalFlags;
use crate::shared::HcomContext;
use anyhow::{Result, bail};
use serde_json::json;
use std::time::Instant;

pub(crate) const INLINE_SINGLE_LAUNCH_WAIT_SECS: u64 = 15;

/// Run the launch command. `argv` is the full argv[1..] including count/tool.
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    let (count, tool, mut hcom_flags, tool_args) = parse_launch_argv(argv)?;
    let launch_tool = LaunchTool::from_str(&tool)?;
    hcom_flags.run_here = json_run_here(&hcom_flags)?;
    validate_stdio_flags(&hcom_flags, &tool, count)?;
    if hcom_flags.stdio {
        hcom_flags.run_here = Some(true);
    }
    let json_output = hcom_flags.json;

    // Count validation
    if count == 0 {
        bail!("Count must be positive.");
    }
    let max_count = launch_tool.spec().launch.max_launch_count;
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
    let preview_background = headless || is_background_from_args(&launch_tool, &tool_args);

    let ctx = HcomContext::from_os();
    if ctx.is_inside_ai_tool() && !flags.go && (!tool_args.is_empty() || count > 5) {
        if hcom_flags.stdio {
            bail!("--stdio requires --go when launched from an agent");
        }
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
            json: json_output,
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
            "terminal": terminal.clone(),
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
                    remote_terminal: Some(remote_terminal_preset(&inner, terminal.as_deref())),
                    background: remote_output.background,
                    run_here: remote_output.run_here,
                    hcom_config: &hcom_config,
                    inline_readiness_wait_secs: None,
                    json: json_output,
                };
                report_launch(&db, &launch_result, &output)?;
                return Ok(readiness_exit_code(None, launch_result.failed));
            }
            Err(e) => bail!("Remote launch failed for device {device}: {e}"),
        }
    }

    // System/initial prompt handling
    let system_prompt = hcom_flags.system_prompt;
    let initial_prompt = hcom_flags.initial_prompt;

    // Merge env config args with CLI args
    let (merged_args, background) = if hcom_flags.stdio {
        (
            merge_tool_args(&launch_tool, &tool_args, &hcom_config),
            false,
        )
    } else {
        prepare_launch_execution(&launch_tool, &tool_args, &hcom_config, headless)
    };

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
        remote_terminal: None,
        background,
        run_here: hcom_flags.run_here,
        hcom_config: &hcom_config,
        inline_readiness_wait_secs: if ctx.is_inside_ai_tool() && count == 1 {
            Some(INLINE_SINGLE_LAUNCH_WAIT_SECS)
        } else {
            None
        },
        json: json_output,
    };

    let result = launcher::launch(
        &db,
        LaunchParams {
            tool: tool.clone(),
            count,
            args: merged_args,
            persisted_args: None,
            prior_session_id: None,
            resume_cursor: None,
            tag,
            system_prompt,
            initial_prompt,
            background,
            stdio: hcom_flags.stdio,
            cwd: Some(if let Some(ref dir) = dir_override {
                let path = std::path::Path::new(dir);
                if !path.is_dir() {
                    bail!("--dir path does not exist or is not a directory: {}", dir);
                }
                path.canonicalize()
                    .map(|p| crate::shared::platform::child_process_path(&p))
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

    if hcom_flags.stdio {
        return result
            .exit_code
            .ok_or_else(|| anyhow::anyhow!("stdio child did not return an exit status"));
    }

    let readiness_state = report_launch(&db, &result, &output)?;

    // Log summary
    log_info(
        "launch",
        "cmd.launch",
        &format!(
            "tool={} count={} launched={} failed={} batch={}",
            tool, count, result.launched, result.failed, result.batch_id
        ),
    );

    Ok(readiness_exit_code(readiness_state, result.failed))
}

pub(crate) fn prepare_launch_execution(
    tool: &LaunchTool,
    cli_args: &[String],
    config: &HcomConfig,
    headless: bool,
) -> (Vec<String>, bool) {
    let mut merged_args = merge_tool_args(tool, cli_args, config);
    let background = headless || is_background_from_args(tool, &merged_args);

    // Print mode needs stream-json for the stop-hook loop. Keep this deliberately
    // grammar-free: hcom appends its required defaults and lets Claude resolve
    // duplicates or reject incompatible combinations.
    if tool.tool() == crate::tool::Tool::Claude
        && background
        && args_contain_any(&merged_args, &["-p", "--print"])
    {
        merged_args.extend([
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ]);
    }

    (merged_args, background)
}

pub(crate) fn launch_result_to_json(result: &LaunchResult) -> serde_json::Value {
    serde_json::to_value(result).unwrap_or_else(|_| json!({}))
}

/// Preset a remote device reported using; older devices don't report one.
pub(crate) fn remote_terminal_preset<'a>(
    inner: &'a serde_json::Value,
    requested: Option<&'a str>,
) -> &'a str {
    inner
        .get("terminal_preset")
        .and_then(|v| v.as_str())
        .or(requested)
        .unwrap_or("default")
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
            crate::identity::resolve_display_name(db, name).unwrap_or_else(|| name.to_string())
        })
        .or_else(|| flags.name.clone())
        .unwrap_or_else(|| {
            identity::resolve_identity(db, None, None, None, process_id, None)
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
    pub json: bool,
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
    // Drive the args-env label from the spec so we never invent a key (e.g.
    // `HCOM_ANTIGRAVITY_ARGS`) for tools that don't have one.
    let args_key: Option<&'static str> = preview
        .tool
        .parse::<crate::tool::Tool>()
        .ok()
        .and_then(|t| t.spec().launch.args_env);
    let env_args = if preview.show_config_args {
        preview
            .tool
            .parse::<crate::tool::Tool>()
            .ok()
            .map(|tool| preview.config.tool_args(tool))
            .unwrap_or("")
    } else {
        ""
    };

    let terminal = preview
        .terminal
        .map(|s| s.to_string())
        .or_else(|| std::env::var("HCOM_TERMINAL").ok())
        .unwrap_or_else(|| preview.config.terminal.clone());

    if preview.json {
        let notes: Vec<&str> = preview
            .notes
            .iter()
            .copied()
            .filter(|n| !n.is_empty())
            .collect();
        println!(
            "{}",
            json!({
                "preview": true,
                "action": preview.action,
                "tool": preview.tool,
                "count": preview.count,
                "mode": mode,
                "dir": cwd,
                "terminal": terminal,
                "tag": preview.tag,
                "notes": notes,
                "config_args": env_args,
                "config_args_key": args_key,
                "cli_args": preview.args,
                "hint": "Add --go to proceed.",
            })
        );
        return;
    }

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
            match args_key {
                Some(key) => println!("  From config ({}): {}", key, env_args),
                None => println!("  From config: {}", env_args),
            }
        }
        if !preview.args.is_empty() {
            println!("  From CLI: {}", preview.args.join(" "));
        }
        if !env_args.is_empty() && !preview.args.is_empty() {
            println!(
                "  (CLI overrides known singleton defaults; other args pass through in order)"
            );
        }
    }

    println!("\n[Preview Mode] Add --go to proceed with launch.");
}

/// Hcom-level flags extracted from launch argv.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HcomLaunchFlags {
    pub tag: Option<String>,
    pub terminal: Option<String>,
    pub device: Option<String>,
    pub headless: bool,
    /// Keep the native Claude print process on the caller's three pipes.
    pub stdio: bool,
    pub system_prompt: Option<String>,
    pub initial_prompt: Option<String>,
    pub run_here: Option<bool>,
    pub batch_id: Option<String>,
    pub dir: Option<String>,
    /// `--json`: report the outcome as one JSON object on stdout.
    pub json: bool,
}

fn validate_stdio_flags(flags: &HcomLaunchFlags, tool: &str, count: usize) -> Result<()> {
    if !flags.stdio {
        return Ok(());
    }
    if tool != "claude" || count != 1 {
        bail!("--stdio requires exactly one claude process");
    }
    if flags.headless
        || flags.json
        || flags.device.is_some()
        || flags.terminal.is_some()
        || flags.run_here == Some(false)
    {
        bail!(
            "--stdio cannot be combined with --headless, --json, --device, --terminal or --no-run-here"
        );
    }
    Ok(())
}

/// `--json` output is meant to be captured by a script, so the agent must not
/// take over the caller's terminal: open it elsewhere, and reject an explicit
/// request to run here.
pub(crate) fn json_run_here(flags: &HcomLaunchFlags) -> Result<Option<bool>> {
    if !flags.json {
        return Ok(flags.run_here);
    }
    if flags.run_here == Some(true) || flags.terminal.as_deref() == Some("here") {
        bail!("--json cannot be combined with --run-here or --terminal here");
    }
    Ok(Some(false))
}

/// hcom launch flags that take the next token as their value, matching the
/// arms of `extract_launch_flags` (a test keeps the two in step).
const LAUNCH_VALUE_FLAGS: &[&str] = &[
    "--tag",
    "--terminal",
    "--device",
    "--dir",
    "--hcom-system-prompt",
    "--system",
    "--hcom-prompt",
    "--batch-id",
    "--name",
];

/// `args` minus the `--json` tokens that `extract_launch_flags` reads as the
/// flag. A value that happens to be `--json` (`--hcom-prompt --json`) and
/// anything after `--` are kept.
pub(crate) fn without_json_flag(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            out.extend_from_slice(&args[i..]);
            break;
        }
        if LAUNCH_VALUE_FLAGS.contains(&arg) && i + 1 < args.len() {
            out.extend_from_slice(&args[i..i + 2]);
            i += 2;
            continue;
        }
        if arg != "--json" {
            out.push(args[i].clone());
        }
        i += 1;
    }
    out
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

/// Split configured defaults before composing explicit arguments.
fn config_arg_tokens(config_args: &str) -> Vec<String> {
    if config_args.is_empty() {
        Vec::new()
    } else {
        // Don't silently drop hand-edited config args on a parse error (e.g. an
        // unterminated quote) — surface it so the launch isn't quietly missing
        // flags the user configured.
        crate::tools::args_common::shell_split(config_args, cfg!(windows)).unwrap_or_else(|err| {
            eprintln!("hcom: ignoring malformed configured args ({err}): {config_args}");
            Vec::new()
        })
    }
}

pub(crate) fn merge_tool_args(
    tool: &LaunchTool,
    cli_args: &[String],
    config: &HcomConfig,
) -> Vec<String> {
    crate::tools::argument_overrides::override_args(
        tool.tool(),
        &config_arg_tokens(config.tool_args(tool.tool())),
        cli_args,
    )
}

fn args_contain_any(args: &[String], needles: &[&str]) -> bool {
    args.iter().any(|arg| needles.contains(&arg.as_str()))
}

/// Check if args indicate background/headless mode.
pub(crate) fn is_background_from_args(tool: &LaunchTool, args: &[String]) -> bool {
    // Other tools are always hosted in hcom's PTY. Their native
    // non-interactive modes are rejected by validate_tool_args.
    tool.spec().launch.background == crate::integration_spec::BackgroundMode::NativePrint
        && args_contain_any(args, &["-p", "--print"])
}

pub(crate) fn load_hcom_config() -> HcomConfig {
    HcomConfig::load(None)
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
            "--stdio" => {
                flags.stdio = true;
                i += 1;
            }
            "--json" => {
                flags.json = true;
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
    /// Remote launches: the preset the target device used. Resolving
    /// `terminal` here would describe this device, not the target.
    pub remote_terminal: Option<&'a str>,
    pub background: bool,
    pub run_here: Option<bool>,
    pub hcom_config: &'a HcomConfig,
    pub inline_readiness_wait_secs: Option<u64>,
    pub json: bool,
}

/// Report a launch, resume, or fork: human feedback followed by the optional
/// inline readiness wait, or with `--json` a single object on stdout. Returns
/// the readiness outcome when a wait ran.
pub(crate) fn report_launch(
    db: &HcomDb,
    result: &LaunchResult,
    ctx: &LaunchOutputContext<'_>,
) -> Result<Option<InlineLaunchReadiness>> {
    let wait_secs = ctx
        .inline_readiness_wait_secs
        .filter(|_| result.launched == 1);
    if !ctx.json {
        print_launch_feedback(db, result, ctx)?;
        return Ok(wait_secs.map(|secs| print_inline_launch_readiness(db, result, secs)));
    }

    // Errors also go to stderr so a script that only parses stdout still
    // surfaces them to whoever is watching.
    let errors = launch_error_messages(result);
    for msg in &errors {
        eprintln!("Error: {}", msg);
    }
    let wait = wait_secs
        .map(|secs| launch_status::wait_for_launch(db, None, Some(&result.batch_id), secs));
    println!(
        "{}",
        json!({
            "action": ctx.action,
            "tool": ctx.tool,
            "requested": ctx.requested_count,
            "launched": result.launched,
            "failed": result.failed,
            "names": instance_names_from_launch_result(result),
            "tag": ctx.tag,
            "batch_id": result.batch_id,
            "background": ctx.background,
            "log_files": result.log_files,
            "errors": errors,
            // Same shape as `hcom events launch`; null when no wait ran.
            "readiness": wait.as_ref().map(|w| w.to_json()),
        })
    );
    Ok(wait.map(|w| inline_readiness_from_status(&w.status)))
}

fn launch_error_messages(result: &LaunchResult) -> Vec<&str> {
    result
        .errors
        .iter()
        .filter_map(|err| err.get("error").and_then(|v| v.as_str()))
        .collect()
}

fn print_launch_feedback(
    db: &HcomDb,
    result: &LaunchResult,
    ctx: &LaunchOutputContext<'_>,
) -> Result<()> {
    if result.failed > 0 {
        for msg in launch_error_messages(result) {
            eprintln!("Error: {}", msg);
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
    if ctx.inline_readiness_wait_secs.is_none() {
        println!(
            "To block until ready or fail (30s timeout), run: hcom events launch {}",
            result.batch_id
        );
    }

    let launcher_participating = db
        .get_instance_full(ctx.launcher_name)
        .ok()
        .flatten()
        .is_some();
    let (terminal_mode, terminal_auto_detected) = match ctx.remote_terminal {
        Some(preset) => (preset.to_string(), false),
        None => crate::terminal::resolve_terminal_mode_for_tips(
            ctx.terminal,
            &ctx.hcom_config.terminal,
            ctx.background,
            ctx.run_here.unwrap_or(false),
        ),
    };
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InlineLaunchReadiness {
    Ready,
    Failed,
    Blocked,
    Launching,
}

/// Map an inline-readiness outcome to a process exit code, shared by launch
/// and resume/fork so all three report readiness the same way. `None` means
/// no readiness wait ran (not inside an AI tool, or multi-launch).
pub(crate) fn readiness_exit_code(state: Option<InlineLaunchReadiness>, failed: usize) -> i32 {
    match state {
        Some(InlineLaunchReadiness::Failed) => 1,
        Some(InlineLaunchReadiness::Launching) | Some(InlineLaunchReadiness::Blocked) => 2,
        _ if failed == 0 => 0,
        _ => 1,
    }
}

fn inline_readiness_from_status(status: &LaunchStatus) -> InlineLaunchReadiness {
    match status {
        LaunchStatus::Ready => InlineLaunchReadiness::Ready,
        LaunchStatus::Error => InlineLaunchReadiness::Failed,
        LaunchStatus::Blocked => InlineLaunchReadiness::Blocked,
        LaunchStatus::Timeout | LaunchStatus::NoLaunches => InlineLaunchReadiness::Launching,
    }
}

fn print_inline_launch_readiness(
    db: &HcomDb,
    result: &LaunchResult,
    timeout_secs: u64,
) -> InlineLaunchReadiness {
    println!("Waiting up to {timeout_secs}s for launch readiness...");
    let start = Instant::now();
    let wait = launch_status::wait_for_launch(db, None, Some(&result.batch_id), timeout_secs);
    let elapsed_secs = start.elapsed().as_secs_f64();

    let state = inline_readiness_from_status(&wait.status);
    let details = match state {
        InlineLaunchReadiness::Failed => wait.failures,
        InlineLaunchReadiness::Blocked => wait.blockers,
        InlineLaunchReadiness::Ready | InlineLaunchReadiness::Launching => Vec::new(),
    };

    println!(
        "{}",
        format_inline_launch_readiness(state, result, &wait.instances, elapsed_secs, &details)
    );
    state
}

fn instance_names_from_launch_result(result: &LaunchResult) -> Vec<String> {
    result
        .handles
        .iter()
        .filter_map(|h| {
            h.get("instance_name")
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
        })
        .collect()
}

fn format_inline_launch_readiness(
    state: InlineLaunchReadiness,
    result: &LaunchResult,
    ready_instances: &[String],
    elapsed_secs: f64,
    failures: &[String],
) -> String {
    let names = instance_names_from_launch_result(result);
    let target = if names.is_empty() {
        "agent".to_string()
    } else {
        names.join(" ")
    };
    let progress = format!("{}/{} ready", ready_instances.len(), result.launched);
    let elapsed = format!("{elapsed_secs:.1}s");

    match state {
        InlineLaunchReadiness::Ready => {
            let ready = if ready_instances.is_empty() {
                target
            } else {
                ready_instances.join(" ")
            };
            format!("Launch ready: {ready} ({progress}, {elapsed}).")
        }
        InlineLaunchReadiness::Failed => {
            let detail = if failures.is_empty() {
                "no failure detail available".to_string()
            } else {
                failures.join("; ")
            };
            format!("Launch failed: {detail} (batch: {}).", result.batch_id)
        }
        InlineLaunchReadiness::Blocked => {
            let detail = if failures.is_empty() {
                "human attention needed".to_string()
            } else {
                failures.join("; ")
            };
            format!("Launch blocked: {detail} (batch: {}).", result.batch_id)
        }
        InlineLaunchReadiness::Launching => {
            let list_cmd = match names.as_slice() {
                [name] => format!("hcom list {name}"),
                _ => "hcom list -v".to_string(),
            };
            format!(
                "Still launching after {elapsed}: {target} ({progress}).\n  \
                 hcom events launch {batch}   blocks until ready, failed, or blocked on approval (30s, --timeout N)\n  \
                 {list_cmd}   current status",
                batch = result.batch_id,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    fn lt(tool: &str) -> LaunchTool {
        LaunchTool::from_str(tool).unwrap()
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
    fn explicit_args_override_configured_singletons_and_preserve_config_overrides() {
        let mut config = HcomConfig::default();
        config
            .set_field("codex_args", "-m old -s read-only -c key=1")
            .unwrap();
        assert_eq!(
            merge_tool_args(
                &lt("codex"),
                &s(&[
                    "--model=new",
                    "--sandbox",
                    "workspace-write",
                    "--config",
                    "key=2"
                ]),
                &config
            ),
            s(&[
                "-c",
                "key=1",
                "--model=new",
                "--sandbox",
                "workspace-write",
                "--config",
                "key=2"
            ])
        );
    }

    #[test]
    fn test_primary_tool_args_are_concatenated_verbatim() {
        for (tool, field) in [
            ("claude", "claude_args"),
            ("gemini", "gemini_args"),
            ("codex", "codex_args"),
        ] {
            let mut config = HcomConfig::default();
            config.set_field(field, "--future-config value").unwrap();
            let cli = s(&["--future-upstream-flag", "raw-value"]);
            let merged = merge_tool_args(&lt(tool), &cli, &config);
            assert_eq!(
                merged,
                s(&[
                    "--future-config",
                    "value",
                    "--future-upstream-flag",
                    "raw-value"
                ])
            );
        }
    }

    #[test]
    fn test_merge_tool_args_applies_config_for_opencode_family_kimi_and_qoder() {
        // These tools previously fell through to the `_` pass-through arm, which
        // silently dropped their `*_args` config at launch.
        let cli = s(&["--yolo"]);
        for (tool, field) in [
            ("opencode", "opencode_args"),
            ("kilo", "kilo_args"),
            ("kimi", "kimi_args"),
            ("qoder", "qoder_args"),
        ] {
            let mut config = HcomConfig::default();
            config.set_field(field, "--model from-config").unwrap();
            let merged = merge_tool_args(&lt(tool), &cli, &config);
            assert_eq!(
                merged,
                s(&["--model", "from-config", "--yolo"]),
                "config args must be merged for {tool}"
            );
        }
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
    fn test_parse_launch_argv_json_is_hcom_flag_unless_after_dashdash() {
        let (_, _, flags, args) =
            parse_launch_argv(&s(&["claude", "--json", "--model", "haiku"])).unwrap();
        assert!(flags.json);
        assert_eq!(args, s(&["--model", "haiku"]));

        let (_, _, flags, args) = parse_launch_argv(&s(&["claude", "--", "--json"])).unwrap();
        assert!(!flags.json);
        assert_eq!(args, s(&["--json"]));
    }

    #[test]
    fn test_without_json_flag_keeps_values_and_tool_args() {
        assert_eq!(
            without_json_flag(&s(&[
                "--json", "--tag", "x", "--model", "m", "--", "--json"
            ])),
            s(&["--tag", "x", "--model", "m", "--", "--json"])
        );
        // Every value flag swallows a following `--json` as its value, in the
        // parser and in the stripper alike.
        for flag in LAUNCH_VALUE_FLAGS {
            let args = s(&[flag, "--json"]);
            let (flags, _) = extract_launch_flags(&args);
            assert!(!flags.json, "{flag} should take --json as its value");
            assert_eq!(without_json_flag(&args), args, "{flag}");
        }
    }

    #[test]
    fn test_json_run_here_opens_elsewhere_and_rejects_run_here() {
        let json = HcomLaunchFlags {
            json: true,
            ..Default::default()
        };
        assert_eq!(json_run_here(&json).unwrap(), Some(false));
        for flags in [
            HcomLaunchFlags {
                run_here: Some(true),
                ..json.clone()
            },
            HcomLaunchFlags {
                terminal: Some("here".to_string()),
                ..json.clone()
            },
        ] {
            assert!(json_run_here(&flags).is_err());
        }
        // Without --json, run-here resolution is untouched.
        let plain = HcomLaunchFlags {
            run_here: Some(true),
            ..Default::default()
        };
        assert_eq!(json_run_here(&plain).unwrap(), Some(true));
        assert_eq!(json_run_here(&HcomLaunchFlags::default()).unwrap(), None);
    }

    #[test]
    fn test_parse_launch_argv_device() {
        let (_, _, flags, args) =
            parse_launch_argv(&s(&["claude", "--device", "ABCD", "--model", "haiku"])).unwrap();
        assert_eq!(flags.device, Some("ABCD".to_string()));
        assert_eq!(args, s(&["--model", "haiku"]));
    }

    #[test]
    fn test_prepare_launch_execution_claude_print_adds_background_defaults() {
        // Explicit `-p` opts into print mode → detached print-mode defaults applied.
        let config = HcomConfig::default();
        let (args, background) =
            prepare_launch_execution(&lt("claude"), &s(&["-p"]), &config, true);
        assert!(background);

        assert!(
            args.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(args.iter().any(|arg| arg == "--verbose"));
    }

    #[test]
    fn test_prepare_launch_execution_headless_no_print_flag_stays_pty() {
        // `hcom claude --headless` (no -p) is the live PTY session now — no -p is
        // injected and no print-mode defaults are added.
        let config = HcomConfig::default();
        let (args, background) = prepare_launch_execution(&lt("claude"), &s(&[]), &config, true);
        assert!(background);
        assert!(
            !args
                .iter()
                .any(|arg| matches!(arg.as_str(), "-p" | "--print"))
        );
        assert!(!args.iter().any(|arg| arg == "--output-format"));
    }

    #[test]
    fn test_prepare_launch_execution_headless_positional_prompt_stays_pty() {
        // `hcom claude --headless "task text"` — positional prompt, no -p → PTY.
        let config = HcomConfig::default();
        let (args, _background) =
            prepare_launch_execution(&lt("claude"), &s(&["task text"]), &config, true);
        assert_eq!(args, s(&["task text"]));
    }

    #[test]
    fn test_prepare_launch_execution_headless_only_applies_to_claude() {
        // --headless on other tools must not grow a -p; that flag is Claude-specific.
        let config = HcomConfig::default();
        let (args, _bg) = prepare_launch_execution(&lt("codex"), &s(&[]), &config, true);
        assert!(!args.iter().any(|t| t == "-p"));
    }

    #[test]
    fn test_prepare_launch_execution_interactive_claude_unchanged() {
        // Foreground `hcom claude` (no --headless, no -p) stays untouched.
        let config = HcomConfig::default();
        let (args, background) = prepare_launch_execution(&lt("claude"), &s(&[]), &config, false);
        assert!(!background);
        assert!(args.is_empty());
    }

    #[test]
    fn test_launch_result_json_roundtrip() {
        let result = LaunchResult {
            exit_code: None,
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
    fn test_format_inline_launch_readiness_ready() {
        let result = LaunchResult {
            exit_code: None,
            tool: "codex".to_string(),
            batch_id: "batch-1".to_string(),
            launched: 1,
            failed: 0,
            background: false,
            log_files: Vec::new(),
            handles: vec![serde_json::json!({"instance_name": "luna"})],
            errors: Vec::new(),
        };

        let line = format_inline_launch_readiness(
            InlineLaunchReadiness::Ready,
            &result,
            &["luna".to_string()],
            2.2,
            &[],
        );

        assert_eq!(line, "Launch ready: luna (1/1 ready, 2.2s).");
    }

    #[test]
    fn test_format_inline_launch_readiness_launching_has_followup_command() {
        let result = LaunchResult {
            exit_code: None,
            tool: "gemini".to_string(),
            batch_id: "batch-2".to_string(),
            launched: 1,
            failed: 0,
            background: false,
            log_files: Vec::new(),
            handles: vec![serde_json::json!({"instance_name": "mari"})],
            errors: Vec::new(),
        };

        let line = format_inline_launch_readiness(
            InlineLaunchReadiness::Launching,
            &result,
            &[],
            15.0,
            &[],
        );

        assert!(line.contains("Still launching after 15.0s: mari (0/1 ready)."));
        assert!(line.contains("hcom events launch batch-2   blocks until ready"));
        assert!(line.contains("hcom list mari   current status"));
    }

    #[test]
    fn test_format_inline_launch_readiness_failed_includes_detail() {
        let result = LaunchResult {
            exit_code: None,
            tool: "claude".to_string(),
            batch_id: "batch-3".to_string(),
            launched: 1,
            failed: 0,
            background: true,
            log_files: Vec::new(),
            handles: vec![serde_json::json!({"instance_name": "nola"})],
            errors: Vec::new(),
        };

        let line = format_inline_launch_readiness(
            InlineLaunchReadiness::Failed,
            &result,
            &[],
            0.5,
            &["nola: executable not found".to_string()],
        );

        assert_eq!(
            line,
            "Launch failed: nola: executable not found (batch: batch-3)."
        );
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
                exit_code: None,
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
                exit_code: None,
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
            &lt("claude"),
            &s(&["-p", "fix tests", "--output-format", "json"])
        ));
    }

    #[test]
    fn test_is_background_claude_interactive() {
        assert!(!is_background_from_args(
            &lt("claude"),
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
