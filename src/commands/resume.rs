//! Resume command: `hcom r <name> [tool-args...]`
//!
//!
//! Loads a stopped instance's snapshot and relaunches with --resume session_id.

use anyhow::{Result, bail};
use serde_json::json;
use std::io::BufRead;

use crate::commands::launch::{
    LaunchOutputContext, LaunchPreview, extract_launch_flags, is_background_from_args,
    load_hcom_config, print_launch_feedback, print_launch_preview, resolve_launcher_name,
};
use crate::commands::transcript::{claude_config_dir, detect_agent_type};
use crate::db::HcomDb;
use crate::hooks::claude_args;
use crate::hooks::codex::derive_codex_transcript_path;
use crate::hooks::gemini::derive_gemini_transcript_path;
use crate::launcher::{self, LaunchParams, LaunchResult};
use crate::log::log_info;
use crate::router::GlobalFlags;
use crate::tools::{codex_args, gemini_args};

/// Where to load the resume/fork plan from.
enum ResumeSource<'a> {
    /// Resume an hcom-tracked instance by name (active or stopped).
    Instance { name: &'a str },
    /// Adopt a session from its on-disk transcript (first-time bring-in under hcom).
    Disk {
        session_id: String,
        tool: String,
        cwd_hint: Option<String>,
    },
}

struct PreparedResume {
    output: ResumeOutputContext,
    launch: LaunchParams,
    last_event_id: i64,
    session_id: String,
}

struct ResumeOutputContext {
    action: String,
    tool: String,
    tag: Option<String>,
    terminal: Option<String>,
    background: bool,
    run_here: Option<bool>,
    launcher_name: String,
}

/// Run the resume command. `argv` is the full argv[1..].
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    let (name, extra_args) = parse_resume_argv(argv, "r")?;

    do_resume(&name, false, &extra_args, flags)
}

/// Parse resume/fork argv: `r|f <name> [extra-args...]`
pub fn parse_resume_argv(argv: &[String], cmd: &str) -> Result<(String, Vec<String>)> {
    let mut i = 0;

    // Skip command name and global flags
    while i < argv.len() {
        match argv[i].as_str() {
            s if s == cmd || s == "resume" || s == "fork" || s == "f" => {
                i += 1;
            }
            "--name" => {
                i += 2;
            }
            "--go" => {
                i += 1;
            }
            _ => break,
        }
    }

    if i >= argv.len() {
        bail!("Usage: hcom {} <name> [tool-args...]", cmd);
    }

    let name = argv[i].clone();
    let extra_args = argv[i + 1..].to_vec();

    Ok((name, extra_args))
}

/// Core resume/fork logic.
pub fn do_resume(
    name: &str,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
) -> Result<i32> {
    let db = HcomDb::open()?;
    let name = crate::instances::resolve_display_name_or_stopped(&db, name)
        .unwrap_or_else(|| name.to_string());
    let hcom_config = load_hcom_config();
    let ctx = crate::shared::HcomContext::from_os();

    // Adoption path: UUID input for a session not yet tracked as an hcom instance.
    if is_session_id(&name) {
        // Collision guard: if the UUID is bound to a running hcom instance, refuse.
        // If bound to a stopped instance, delegate to the normal name-based path
        // (preserves the existing hcom identity across re-resumes).
        if let Ok(Some(instance_name)) = db.get_session_binding(&name) {
            if matches!(db.get_instance_full(&instance_name), Ok(Some(_))) {
                bail!(
                    "Session {} is currently active as '{}' — run hcom stop {} first",
                    name,
                    instance_name,
                    instance_name
                );
            }
            return do_resume(&instance_name, fork, extra_args, flags);
        }

        return do_adopt_session(&db, &name, fork, extra_args, flags, &hcom_config);
    }

    if let Some((base_name, device)) = crate::relay::control::split_device_suffix(&name) {
        if fork {
            let has_dir = extra_args
                .iter()
                .any(|a| a == "--dir" || a.starts_with("--dir="));
            if !has_dir {
                bail!(
                    "Remote fork requires --dir to specify the working directory on the target device"
                );
            }
        }
        if ctx.is_inside_ai_tool() && !flags.go && should_preview_resume_rpc(extra_args) {
            if let Ok(plan) = prepare_resume_plan(&db, &name, fork, extra_args, flags) {
                print_resume_preview(&plan, &hcom_config, &name, fork);
                return Ok(0);
            }
        }

        let launcher_name =
            resolve_launcher_name(&db, flags, std::env::var("HCOM_PROCESS_ID").ok().as_deref());
        let inner = crate::relay::control::dispatch_remote(
            &db,
            device,
            Some(&name),
            crate::relay::control::rpc_action::RESUME,
            &json!({
                "target": base_name,
                "fork": fork,
                "extra_args": extra_args,
                "launcher": launcher_name,
            }),
            crate::relay::control::RPC_RESUME_TIMEOUT,
        )
        .map_err(anyhow::Error::msg)?;
        let launch_result =
            crate::commands::launch::launch_result_from_json(&inner).map_err(anyhow::Error::msg)?;
        let remote_output =
            build_remote_resume_output(&db, &launch_result, extra_args, fork, flags);
        let output = LaunchOutputContext {
            action: &remote_output.action,
            tool: &remote_output.tool,
            requested_count: 1,
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

    let plan = prepare_resume_plan(&db, &name, fork, extra_args, flags)?;
    if ctx.is_inside_ai_tool() && !flags.go && should_preview_resume_rpc(extra_args) {
        print_resume_preview(&plan, &hcom_config, &name, fork);
        return Ok(0);
    }

    execute_prepared_resume(&db, &name, fork, &plan, &hcom_config, true)
}

pub fn run_local_resume_result(
    db: &HcomDb,
    name: &str,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
) -> Result<LaunchResult> {
    let plan = prepare_resume_plan(db, name, fork, extra_args, flags)?;
    execute_prepared_resume_result(db, name, fork, &plan)
}

fn prepare_resume_plan(
    db: &HcomDb,
    name: &str,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
) -> Result<PreparedResume> {
    prepare_resume_plan_from_source(db, ResumeSource::Instance { name }, fork, extra_args, flags)
}

fn prepare_resume_plan_from_source(
    db: &HcomDb,
    source: ResumeSource<'_>,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
) -> Result<PreparedResume> {
    let is_adoption = matches!(source, ResumeSource::Disk { .. });

    // Load the (tool, session_id, prior-launch-args, tag, background, last_event_id, cwd_hint, display_name)
    // from either the DB (instance) or the on-disk transcript (adoption).
    let (tool, session_id, launch_args_str, tag, background, last_event_id, snapshot_dir, display_name) =
        match source {
            ResumeSource::Instance { name } => {
                if !fork {
                    if let Ok(Some(_)) = db.get_instance_full(name) {
                        bail!("'{}' is still active — run hcom stop {} first", name, name);
                    }
                }
                let (tool, sid, largs, tag, bg, leid, snap) = if fork {
                    load_instance_data(db, name)?
                } else {
                    load_stopped_snapshot(db, name)?
                };
                (tool, sid, largs, tag, bg, leid, snap, name.to_string())
            }
            ResumeSource::Disk {
                session_id,
                tool,
                cwd_hint,
            } => {
                let display = session_id.clone();
                (
                    tool,
                    session_id,
                    String::new(),
                    String::new(),
                    false,
                    0,
                    cwd_hint.unwrap_or_default(),
                    display,
                )
            }
        };

    if session_id.is_empty() {
        bail!(
            "No session ID found for '{}' — cannot {}",
            display_name,
            if fork { "fork" } else { "resume" }
        );
    }

    validate_resume_operation(&tool, fork)?;
    let inherited_tag = if tag.is_empty() {
        None
    } else {
        Some(tag.clone())
    };

    // Extract hcom-level flags from extra args before tool parsing.
    let (dir_override, launch_flags, clean_extra) = extract_resume_flags(extra_args);

    // Determine effective working directory:
    // - Explicit --dir flag wins (validated and canonicalized)
    // - For fork (tracked instance): use current directory (start fresh in new context)
    // - Otherwise: use snapshot/transcript directory, falling back to current
    let effective_cwd = if let Some(ref dir) = dir_override {
        let path = std::path::Path::new(dir);
        if !path.is_dir() {
            bail!("--dir path does not exist or is not a directory: {}", dir);
        }
        path.canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| dir.clone())
    } else if fork && !is_adoption {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    } else if !snapshot_dir.is_empty() && std::path::Path::new(&snapshot_dir).is_dir() {
        snapshot_dir.clone()
    } else {
        if !snapshot_dir.is_empty() {
            eprintln!(
                "Warning: original directory '{}' no longer exists, using current directory",
                snapshot_dir
            );
        }
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    };

    let mut cli_tool_args = build_resume_args(&tool, &session_id, fork);
    cli_tool_args.extend(clean_extra);

    // Merge with original launch args (only applicable for tracked instances).
    let original_args: Vec<String> = if !launch_args_str.is_empty() {
        serde_json::from_str(&launch_args_str).unwrap_or_default()
    } else {
        Vec::new()
    };

    let merged_cli_args = if !original_args.is_empty() {
        merge_resume_args(&tool, &original_args, &cli_tool_args)
    } else {
        cli_tool_args
    };

    let mut merged_args = merged_cli_args.clone();

    if launch_flags.headless && tool != "claude" {
        bail!("--headless is only supported for Claude resume/fork launches");
    }

    let is_headless =
        launch_flags.headless || is_background_from_args(&tool, &merged_args) || background;
    let use_pty = tool == "claude" && !is_headless && cfg!(unix);

    if tool == "claude" && is_headless {
        let spec = claude_args::resolve_claude_args(Some(&merged_args), None);
        let updated = claude_args::add_background_defaults(&spec);
        merged_args = updated.rebuild_tokens(true);
    }

    let launcher_name =
        resolve_launcher_name(db, flags, std::env::var("HCOM_PROCESS_ID").ok().as_deref());
    let launcher_name_for_output = launcher_name.clone();

    // Pre-allocate a fork child name only for tracked-instance forks, since the
    // identity-reset prompts reference it. For adoption-fork there is no prior
    // hcom identity in the transcript, so the SessionStart hook handles naming.
    let fork_child_name = if fork && !is_adoption {
        let (alive_names, taken_names) = crate::instance_names::collect_taken_names(db)?;
        let candidate = crate::instance_names::allocate_name(
            &|n| taken_names.contains(n) || db.get_instance_full(n).ok().flatten().is_some(),
            &alive_names,
            200,
            1200,
            900.0,
        )?;
        Some(candidate)
    } else {
        None
    };
    let effective_tag = launch_flags.tag.clone().or(inherited_tag.clone());

    // Codex fork identity-reset: only for tracked-instance forks. Adoption-fork
    // transcripts have no prior hcom identity to override.
    let fork_initial_prompt = if fork && tool == "codex" && !is_adoption {
        let child_name = fork_child_name
            .as_deref()
            .expect("fork child name should be generated");
        let child_display = effective_tag
            .as_deref()
            .map(|tag| format!("{tag}-{child_name}"))
            .unwrap_or_else(|| child_name.to_string());
        let parent = display_name.as_str();
        let identity_reset = format!(
            "You are a fork of {parent}, but your new hcom identity is now {child_display}.\n\
             Your hcom name is {child_name}.\n\
             Do not use {parent}'s hcom identity anymore, even if it appears in inherited thread history.\n\
             Use [hcom:{child_name}] in your first response only.\n\
             Use `hcom ... --name {child_name}` for all hcom commands.\n\
             If asked about your identity, answer exactly: {child_display}"
        );
        Some(match launch_flags.initial_prompt.as_deref() {
            Some(user_prompt) if !user_prompt.trim().is_empty() => {
                format!("{identity_reset}\n\n{user_prompt}")
            }
            _ => identity_reset,
        })
    } else {
        launch_flags.initial_prompt.clone()
    };
    let output_tag = effective_tag.clone();
    let launch_tag = effective_tag.clone();

    // System prompt:
    // - Tracked-instance resume/fork: identity-carrying prompt (existing behavior).
    // - Adoption: None — the SessionStart hook issues the normal fresh-launch
    //   bootstrap under the auto-allocated name. The transcript has no prior hcom
    //   context to override.
    let base_system_prompt = if is_adoption {
        None
    } else {
        Some(resume_system_prompt(
            &tool,
            &display_name,
            fork,
            fork_child_name.as_deref(),
        ))
    };
    let effective_system_prompt = match (base_system_prompt, launch_flags.system_prompt.as_deref())
    {
        (Some(base), Some(custom)) if !custom.trim().is_empty() => {
            Some(format!("{base}\n\n{custom}"))
        }
        (Some(base), _) => Some(base),
        (None, Some(custom)) if !custom.trim().is_empty() => Some(custom.to_string()),
        (None, _) => None,
    };

    // Instance name for LaunchParams:
    // - Adoption: None (launcher allocates; SessionStart hook binds via session_bindings)
    // - Tracked fork: pre-allocated fork_child_name
    // - Tracked resume: preserve existing hcom name
    let launch_name = if is_adoption {
        None
    } else if fork {
        fork_child_name.clone()
    } else {
        Some(display_name.clone())
    };

    Ok(PreparedResume {
        output: ResumeOutputContext {
            action: if fork { "fork" } else { "resume" }.to_string(),
            tool: tool.clone(),
            tag: output_tag,
            terminal: launch_flags.terminal.clone(),
            background: is_headless,
            run_here: launch_flags.run_here,
            launcher_name: launcher_name_for_output,
        },
        launch: LaunchParams {
            tool: tool.clone(),
            count: 1,
            args: merged_args,
            tag: launch_tag,
            system_prompt: effective_system_prompt,
            initial_prompt: fork_initial_prompt,
            pty: use_pty,
            background: is_headless,
            cwd: Some(effective_cwd),
            env: None,
            launcher: Some(launcher_name),
            run_here: launch_flags.run_here,
            batch_id: launch_flags.batch_id.clone(),
            name: launch_name,
            skip_validation: false,
            terminal: launch_flags.terminal.clone(),
            // Codex tracked-instance fork uses initial_prompt for an identity
            // reset; don't dilute it with a reply-handoff suffix. Adoption-fork
            // has no identity-reset prompt, so normal handoff rules apply.
            append_reply_handoff: !(fork && tool == "codex" && !is_adoption),
        },
        last_event_id,
        session_id,
    })
}

fn execute_prepared_resume(
    db: &HcomDb,
    name: &str,
    fork: bool,
    plan: &PreparedResume,
    hcom_config: &crate::config::HcomConfig,
    print_feedback_now: bool,
) -> Result<i32> {
    let result = execute_prepared_resume_result(db, name, fork, plan)?;

    if print_feedback_now {
        let output = LaunchOutputContext {
            action: &plan.output.action,
            tool: &plan.output.tool,
            requested_count: 1,
            tag: plan.output.tag.as_deref(),
            launcher_name: &plan.output.launcher_name,
            terminal: plan.output.terminal.as_deref(),
            background: plan.output.background,
            run_here: plan.output.run_here,
            hcom_config,
        };
        print_launch_feedback(db, &result, &output)?;
        log_info(
            if fork { "fork" } else { "resume" },
            &format!("cmd.{}", if fork { "fork" } else { "resume" }),
            &format!(
                "name={} tool={} session={} launched={}",
                name, plan.output.tool, plan.session_id, result.launched
            ),
        );
    }

    Ok(if result.launched > 0 { 0 } else { 1 })
}

fn execute_prepared_resume_result(
    db: &HcomDb,
    name: &str,
    fork: bool,
    plan: &PreparedResume,
) -> Result<LaunchResult> {
    let result = launcher::launch(db, plan.launch.clone())?;

    if !fork && plan.last_event_id > 0 {
        crate::instances::update_instance_position(
            db,
            name,
            &serde_json::Map::from_iter([("last_event_id".to_string(), json!(plan.last_event_id))]),
        );
    }
    if fork {
        // Anchor fork child's cursor at current position so it doesn't replay
        // historical broadcasts. Pre-registration may inherit a stale
        // HCOM_LAUNCH_EVENT_ID from the parent environment.
        let current_max = db.get_last_event_id();
        if let Some(ref child_name) = plan.launch.name {
            crate::instances::update_instance_position(
                db,
                child_name,
                &serde_json::Map::from_iter([("last_event_id".to_string(), json!(current_max))]),
            );
        }
    }
    Ok(result)
}

fn build_remote_resume_output(
    db: &HcomDb,
    launch_result: &LaunchResult,
    extra_args: &[String],
    fork: bool,
    flags: &GlobalFlags,
) -> ResumeOutputContext {
    let (_dir_override, launch_flags, _clean_extra) = extract_resume_flags(extra_args);
    let launcher_name =
        resolve_launcher_name(db, flags, std::env::var("HCOM_PROCESS_ID").ok().as_deref());

    ResumeOutputContext {
        action: if fork { "fork" } else { "resume" }.to_string(),
        tool: launch_result.tool.clone(),
        tag: launch_flags.tag,
        terminal: launch_flags.terminal,
        background: launch_result.background,
        run_here: launch_flags.run_here,
        launcher_name,
    }
}

fn print_resume_preview(
    plan: &PreparedResume,
    hcom_config: &crate::config::HcomConfig,
    name: &str,
    fork: bool,
) {
    let is_adoption = plan.launch.name.is_none();
    let identity_note = if fork {
        format!("Fork source: {} (new identity)", name)
    } else if is_adoption {
        format!("Adopting session: {} (new hcom identity)", name)
    } else {
        format!("Resume target: {} (same identity)", name)
    };
    let cwd_str = plan.launch.cwd.as_deref().unwrap_or(".");
    let cwd_note = format!("Directory source: {}", cwd_str);
    let notes = [identity_note.as_str(), cwd_note.as_str()];
    print_launch_preview(LaunchPreview {
        action: &plan.output.action,
        tool: &plan.output.tool,
        count: 1,
        background: plan.output.background,
        args: &plan.launch.args,
        tag: plan.output.tag.as_deref(),
        cwd: Some(cwd_str),
        terminal: plan.output.terminal.as_deref(),
        config: hcom_config,
        show_config_args: false,
        notes: &notes,
    });
}

fn should_preview_resume_rpc(extra_args: &[String]) -> bool {
    let (dir_override, launch_flags, clean_extra) = extract_resume_flags(extra_args);
    let _ = dir_override;
    should_preview_resume(&launch_flags, &clean_extra)
}

/// Extract resume-only flags, then reuse the shared launch flag parser.
fn extract_resume_flags(
    args: &[String],
) -> (
    Option<String>,
    crate::commands::launch::HcomLaunchFlags,
    Vec<String>,
) {
    let mut dir = None;
    let mut filtered = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            filtered.extend_from_slice(&args[i..]);
            break;
        } else if args[i] == "--dir" && i + 1 < args.len() {
            dir = Some(args[i + 1].clone());
            i += 2;
        } else if args[i].starts_with("--dir=") {
            dir = Some(args[i][6..].to_string());
            i += 1;
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }
    let (flags, remaining) = extract_launch_flags(&filtered);
    (dir, flags, remaining)
}

fn should_preview_resume(
    launch_flags: &crate::commands::launch::HcomLaunchFlags,
    tool_args: &[String],
) -> bool {
    !tool_args.is_empty() || *launch_flags != crate::commands::launch::HcomLaunchFlags::default()
}

fn validate_resume_operation(tool: &str, fork: bool) -> Result<()> {
    if fork && tool == "gemini" {
        bail!("Gemini does not support session forking (hcom f)");
    }
    Ok(())
}

fn resume_system_prompt(tool: &str, name: &str, fork: bool, child_name: Option<&str>) -> String {
    if fork {
        if tool == "codex" {
            format!(
                "YOU ARE A FORK of agent '{}'. \
                 You have the same session history but are a NEW agent with an already-assigned hcom identity. \
                 Use that assigned identity for all hcom commands.",
                name
            )
        } else {
            // Claude gets its identity from the SessionStart hook bootstrap.
            // State the new name explicitly so it overrides the inherited history.
            match child_name {
                Some(child) => format!(
                    "YOU ARE A FORK of agent '{name}'. \
                     You have the same session history but are a NEW agent. \
                     Your new hcom identity is '{child}'. \
                     Use '--name {child}' for all hcom commands. \
                     Do NOT use '{name}'s identity, even if it appears in the inherited history.",
                ),
                None => format!(
                    "YOU ARE A FORK of agent '{name}'. \
                     You have the same session history but are a NEW agent with an already-assigned hcom identity. \
                     Use that assigned identity for all hcom commands.",
                ),
            }
        }
    } else {
        format!("YOUR SESSION HAS BEEN RESUMED! You are still '{}'.", name)
    }
}

/// Load data from an active or stopped instance.
fn load_instance_data(
    db: &HcomDb,
    name: &str,
) -> Result<(String, String, String, String, bool, i64, String)> {
    // Try active instance first
    if let Ok(Some(inst)) = db.get_instance_full(name) {
        return Ok((
            inst.tool.clone(),
            inst.session_id.as_deref().unwrap_or("").to_string(),
            inst.launch_args.as_deref().unwrap_or("").to_string(),
            inst.tag.as_deref().unwrap_or("").to_string(),
            inst.background != 0,
            inst.last_event_id,
            inst.directory.clone(),
        ));
    }

    // Fall back to stopped snapshot
    load_stopped_snapshot(db, name)
}

/// Load stopped snapshot from life events.
fn load_stopped_snapshot(
    db: &HcomDb,
    name: &str,
) -> Result<(String, String, String, String, bool, i64, String)> {
    // Query the latest "stopped" life event for this instance
    let mut stmt = db.conn().prepare(
        "SELECT data FROM events WHERE type='life' AND instance=? ORDER BY id DESC LIMIT 10",
    )?;

    let rows: Vec<String> = stmt
        .query_map(rusqlite::params![name], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    for data_str in &rows {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str) {
            if data.get("action").and_then(|v| v.as_str()) == Some("stopped") {
                if let Some(snapshot) = data.get("snapshot") {
                    let tool = snapshot
                        .get("tool")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let session_id = snapshot
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let launch_args = snapshot
                        .get("launch_args")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let tag = snapshot
                        .get("tag")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let background = snapshot
                        .get("background")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0)
                        != 0;
                    let last_event_id = snapshot
                        .get("last_event_id")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    let directory = snapshot
                        .get("directory")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    return Ok((
                        tool,
                        session_id,
                        launch_args,
                        tag,
                        background,
                        last_event_id,
                        directory,
                    ));
                }
            }
        }
    }

    bail!(
        "No stopped snapshot found for '{name}'.\n\
         If this is a session UUID, the transcript wasn't found on disk.\n\
         If this is a Claude/Codex thread name, resume it natively instead:\n  \
         hcom claude --resume '{name}'   (Claude resolves /rename titles)\n  \
         hcom codex  resume '{name}'     (Codex resolves thread names)\n  \
         hcom gemini --resume '{name}'   (Gemini accepts UUID, index, or 'latest')"
    )
}

/// Build tool-specific resume/fork args.
fn build_resume_args(tool: &str, session_id: &str, fork: bool) -> Vec<String> {
    match tool {
        "claude" | "claude-pty" => {
            let mut args = vec!["--resume".to_string(), session_id.to_string()];
            if fork {
                args.push("--fork-session".to_string());
            }
            args
        }
        "gemini" => {
            vec!["--resume".to_string(), session_id.to_string()]
        }
        "codex" => {
            let subcmd = if fork { "fork" } else { "resume" };
            vec![subcmd.to_string(), session_id.to_string()]
        }
        "opencode" => {
            let mut args = vec!["--session".to_string(), session_id.to_string()];
            if fork {
                args.push("--fork".to_string());
            }
            args
        }
        _ => Vec::new(),
    }
}

/// Merge original launch args with resume-specific args.
fn merge_resume_args(tool: &str, original: &[String], resume: &[String]) -> Vec<String> {
    // Resume args take precedence. We strip --resume/--session from original
    // and prepend resume args.
    match tool {
        "claude" | "claude-pty" => {
            let orig_spec = claude_args::resolve_claude_args(Some(original), None);
            let resume_spec = claude_args::resolve_claude_args(Some(resume), None);
            let merged = claude_args::merge_claude_args(&orig_spec, &resume_spec);
            merged.rebuild_tokens(true)
        }
        "gemini" => {
            let orig_spec = gemini_args::resolve_gemini_args(Some(original), None);
            let resume_spec = gemini_args::resolve_gemini_args(Some(resume), None);
            let merged = gemini_args::merge_gemini_args(&orig_spec, &resume_spec);
            merged.rebuild_tokens(true, true)
        }
        "codex" => {
            let stripped_original =
                crate::tools::codex_preprocessing::strip_codex_developer_instructions(original);
            let orig_spec = codex_args::resolve_codex_args(Some(&stripped_original), None);
            let resume_spec = codex_args::resolve_codex_args(Some(resume), None);
            let merged = codex_args::merge_codex_args(&orig_spec, &resume_spec);
            merged.rebuild_tokens(true, true)
        }
        "opencode" => merge_opencode_args(original, resume),
        _ => {
            // For unknown tools: resume args only.
            resume.to_vec()
        }
    }
}

fn merge_opencode_args(original: &[String], resume: &[String]) -> Vec<String> {
    let mut preserved = Vec::new();
    let mut i = 0;

    while i < original.len() {
        let token = &original[i];

        if token == "--session" || token == "--prompt" {
            i += 2;
            continue;
        }
        if token == "--fork" || token.starts_with("--session=") || token.starts_with("--prompt=") {
            i += 1;
            continue;
        }

        preserved.push(token.clone());
        i += 1;
    }

    let mut merged = resume.to_vec();
    merged.extend(preserved);
    merged
}

// ── Session-ID adoption ──────────────────────────────────────────────────

/// Check if a string looks like a known session-ID format.
/// Claude, Codex, and Gemini use UUIDs. Opencode uses `ses_<hex+base62>`.
fn is_session_id(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok() || is_opencode_session_id(s)
}

/// Opencode session IDs are `ses_` followed by 26 hex+base62 chars
/// (see opencode/packages/opencode/src/id/id.ts).
fn is_opencode_session_id(s: &str) -> bool {
    s.starts_with("ses_")
        && s.len() >= 8
        && s[4..].chars().all(|c| c.is_ascii_alphanumeric())
}

/// Locate an opencode session's data dir: `$XDG_DATA_HOME/opencode` on
/// Linux, `~/Library/Application Support/opencode` on macOS,
/// `%LOCALAPPDATA%\opencode` on Windows (via `dirs::data_dir`).
fn opencode_data_dir() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("opencode"))
}

/// Query opencode's SQLite DB for a session's working directory.
/// Returns (exists, cwd). `exists=true, cwd=None` is impossible given the
/// schema (directory is NOT NULL), so `cwd=None` implies the row is absent.
fn lookup_opencode_session(session_id: &str) -> Option<String> {
    let db_path = opencode_data_dir()?.join("opencode.db");
    if !db_path.exists() {
        return None;
    }
    // Open read-only; no hcom-side schema assumptions beyond `session(id, directory)`.
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.query_row(
        "SELECT directory FROM session WHERE id = ?1",
        rusqlite::params![session_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Resolve a session ID to the owning tool and (optionally) a pre-recovered
/// working directory.
///
/// - Claude, Codex, Gemini: returns `(tool, Some(transcript_path))`.
///   Caller reads the transcript's first line to recover CWD.
/// - Opencode: returns `(tool="opencode", None)` — opencode stores sessions
///   in SQLite; CWD comes from a separate DB query, not a transcript file.
fn find_session_on_disk(session_id: &str) -> Option<(String, Option<String>)> {
    // 1. Opencode: prefix-scoped, query the SQLite DB directly.
    if is_opencode_session_id(session_id) {
        if lookup_opencode_session(session_id).is_some() {
            return Some(("opencode".to_string(), None));
        }
        return None;
    }

    // 2. Claude: iterate project dirs, check for exact filename
    let projects_dir = claude_config_dir().join("projects");
    if projects_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&projects_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let candidate = entry.path().join(format!("{}.jsonl", session_id));
                    if candidate.exists() {
                        let path_str = candidate.to_string_lossy().to_string();
                        let tool = detect_agent_type(&path_str).to_string();
                        return Some((tool, Some(path_str)));
                    }
                }
            }
        }
    }

    // 3. Codex
    if let Some(path) = derive_codex_transcript_path(session_id) {
        let tool = detect_agent_type(&path).to_string();
        return Some((tool, Some(path)));
    }

    // 4. Gemini
    if let Some(path) = derive_gemini_transcript_path(session_id) {
        let tool = detect_agent_type(&path).to_string();
        return Some((tool, Some(path)));
    }

    None
}

/// Recover the session's working directory from the tool's on-disk state.
///
/// - **Claude**: `cwd` is on every line; read line 1 only (O(1) I/O).
/// - **Codex**: first line is `session_meta` with `payload.cwd`; read line 1.
/// - **Gemini**: the session JSON has `projectHash = sha256(cwd)` (hex).
///   `~/.gemini/projects.json` maps `cwd → short-id`. Hash each key and
///   match against `projectHash` to recover the original CWD.
fn extract_cwd_from_transcript(path: &str, tool: &str) -> Option<String> {
    match tool {
        "claude" => read_first_line_cwd(path, |v| v.get("cwd").and_then(|c| c.as_str())),
        "codex" => read_first_line_cwd(path, |v| {
            v.get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
        }),
        "gemini" => recover_gemini_cwd(path),
        _ => None,
    }
}

/// Read one line from a transcript and extract a CWD via the accessor.
fn read_first_line_cwd(path: &str, pick: impl Fn(&serde_json::Value) -> Option<&str>) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let cwd = pick(&parsed)?;
    if cwd.is_empty() {
        None
    } else {
        Some(cwd.to_string())
    }
}

/// Gemini session JSON has `projectHash = hex(sha256(cwd))`. Read it, then
/// reverse-lookup in `~/.gemini/projects.json` which maps `cwd → short-id`.
fn recover_gemini_cwd(transcript_path: &str) -> Option<String> {
    // The session file is a plain JSON object (not JSONL), so read the whole
    // file — but only as far as needed to parse the outer object. For
    // moderately large transcripts this is still comparable to a handful of
    // line reads, and there is no cheaper path.
    let data = std::fs::read_to_string(transcript_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
    let target_hash = parsed.get("projectHash").and_then(|v| v.as_str())?;

    let gemini_base = if let Ok(cli_home) = std::env::var("GEMINI_CLI_HOME") {
        std::path::PathBuf::from(cli_home).join(".gemini")
    } else {
        dirs::home_dir()?.join(".gemini")
    };
    let registry_path = gemini_base.join("projects.json");
    let registry_data = std::fs::read_to_string(&registry_path).ok()?;
    let registry: serde_json::Value = serde_json::from_str(&registry_data).ok()?;
    let projects = registry.get("projects").and_then(|v| v.as_object())?;

    use sha2::{Digest, Sha256};
    for cwd in projects.keys() {
        let digest = Sha256::digest(cwd.as_bytes());
        let hex = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
            use std::fmt::Write;
            let _ = write!(acc, "{:02x}", b);
            acc
        });
        if hex == target_hash {
            return Some(cwd.clone());
        }
    }
    None
}

/// Adopt a session into hcom by UUID: locate its transcript on disk, recover
/// the original working directory when possible, and launch through the
/// normal resume plan machinery so fork/resume semantics stay consistent.
fn do_adopt_session(
    db: &HcomDb,
    session_id: &str,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
    hcom_config: &crate::config::HcomConfig,
) -> Result<i32> {
    let (tool, transcript_path) = find_session_on_disk(session_id).ok_or_else(|| {
        let claude_projects = claude_config_dir().join("projects");
        let opencode_db = opencode_data_dir()
            .map(|d| d.join("opencode.db").display().to_string())
            .unwrap_or_else(|| "(no data dir)".to_string());
        anyhow::anyhow!(
            "Session {sid} not found. Searched:\n  \
             - Claude:   {claude}/*/{sid}.jsonl\n  \
             - Codex:    ~/.codex/sessions/**/*-{sid}.jsonl\n  \
             - Gemini:   ~/.gemini/tmp/*/chats/session-*-{short}*.json\n  \
             - Opencode: {opencode_db} (table 'session')",
            sid = session_id,
            claude = claude_projects.display(),
            short = session_id.split('-').next().unwrap_or(session_id),
            opencode_db = opencode_db,
        )
    })?;

    // CWD recovery: for opencode, the session row carries `directory` directly.
    // For Claude/Codex, read only the first line of the transcript (CWD is
    // fixed at session start; no tool changes CWD mid-session). For fork we
    // start in $PWD (or --dir) by design.
    let cwd_hint = if fork {
        None
    } else {
        let raw = if tool == "opencode" {
            lookup_opencode_session(session_id)
        } else if let Some(ref path) = transcript_path {
            extract_cwd_from_transcript(path, &tool)
        } else {
            None
        };

        match raw {
            Some(cwd) if std::path::Path::new(&cwd).is_dir() => Some(cwd),
            Some(cwd) => {
                eprintln!(
                    "Warning: original directory '{}' no longer exists, using current directory",
                    cwd
                );
                None
            }
            None => None,
        }
    };

    let plan = prepare_resume_plan_from_source(
        db,
        ResumeSource::Disk {
            session_id: session_id.to_string(),
            tool: tool.clone(),
            cwd_hint,
        },
        fork,
        extra_args,
        flags,
    )?;

    let ctx = crate::shared::HcomContext::from_os();
    if ctx.is_inside_ai_tool() && !flags.go && should_preview_resume_rpc(extra_args) {
        print_resume_preview(&plan, hcom_config, session_id, fork);
        return Ok(0);
    }

    let exit = execute_prepared_resume(db, session_id, fork, &plan, hcom_config, true)?;
    log_info(
        if fork { "fork" } else { "resume" },
        &format!("cmd.adopt_{}", if fork { "fork" } else { "resume" }),
        &format!(
            "session_id={} tool={} source={}",
            session_id,
            tool,
            transcript_path.as_deref().unwrap_or("opencode-db")
        ),
    );
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::HcomDb;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    fn test_db() -> HcomDb {
        crate::config::Config::init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        std::mem::forget(dir);
        db
    }

    #[test]
    fn test_parse_resume_argv() {
        let (name, extra) = parse_resume_argv(&s(&["r", "luna"]), "r").unwrap();
        assert_eq!(name, "luna");
        assert!(extra.is_empty());
    }

    #[test]
    fn test_parse_resume_argv_with_extra() {
        let (name, extra) = parse_resume_argv(&s(&["r", "luna", "--model", "opus"]), "r").unwrap();
        assert_eq!(name, "luna");
        assert_eq!(extra, s(&["--model", "opus"]));
    }

    #[test]
    fn test_parse_resume_argv_empty_fails() {
        assert!(parse_resume_argv(&s(&["r"]), "r").is_err());
    }

    #[test]
    fn test_build_resume_args_claude() {
        let args = build_resume_args("claude", "sess-123", false);
        assert_eq!(args, s(&["--resume", "sess-123"]));
    }

    #[test]
    fn test_build_resume_args_claude_fork() {
        let args = build_resume_args("claude", "sess-123", true);
        assert_eq!(args, s(&["--resume", "sess-123", "--fork-session"]));
    }

    #[test]
    fn test_build_resume_args_codex_resume() {
        let args = build_resume_args("codex", "sess-456", false);
        assert_eq!(args, s(&["resume", "sess-456"]));
    }

    #[test]
    fn test_build_resume_args_codex_fork() {
        let args = build_resume_args("codex", "sess-456", true);
        assert_eq!(args, s(&["fork", "sess-456"]));
    }

    #[test]
    fn test_build_resume_args_gemini() {
        let args = build_resume_args("gemini", "sess-789", false);
        assert_eq!(args, s(&["--resume", "sess-789"]));
    }

    #[test]
    fn test_validate_resume_operation_rejects_gemini_fork() {
        let err = validate_resume_operation("gemini", true)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Gemini does not support session forking (hcom f)");
    }

    #[test]
    fn test_validate_resume_operation_allows_gemini_resume() {
        assert!(validate_resume_operation("gemini", false).is_ok());
    }

    #[test]
    fn test_merge_resume_args_opencode_preserves_non_session_flags() {
        let merged = merge_resume_args(
            "opencode",
            &s(&[
                "--model",
                "openai/gpt-5.4",
                "--session",
                "old-sess",
                "--prompt",
                "old prompt",
                "--approval-mode",
                "on-request",
            ]),
            &s(&["--session", "new-sess", "--fork"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--session",
                "new-sess",
                "--fork",
                "--model",
                "openai/gpt-5.4",
                "--approval-mode",
                "on-request",
            ])
        );
    }

    #[test]
    fn test_merge_resume_args_opencode_strips_equals_form_session_and_prompt() {
        let merged = merge_resume_args(
            "opencode",
            &s(&[
                "--session=old-sess",
                "--prompt=old prompt",
                "--model",
                "anthropic/claude-sonnet-4-6",
            ]),
            &s(&["--session", "new-sess"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--session",
                "new-sess",
                "--model",
                "anthropic/claude-sonnet-4-6",
            ])
        );
    }

    #[test]
    fn test_build_resume_args_opencode_fork() {
        let args = build_resume_args("opencode", "sess-000", true);
        assert_eq!(args, s(&["--session", "sess-000", "--fork"]));
    }

    #[test]
    fn test_extract_resume_flags_terminal() {
        let (dir, flags, remaining) =
            extract_resume_flags(&s(&["--terminal", "alacritty", "--model", "opus"]));
        assert_eq!(dir, None);
        assert_eq!(flags.terminal, Some("alacritty".to_string()));
        assert_eq!(remaining, s(&["--model", "opus"]));
    }

    #[test]
    fn test_extract_resume_flags_tag_and_terminal() {
        let (dir, flags, remaining) =
            extract_resume_flags(&s(&["--tag", "test", "--terminal", "kitty"]));
        assert_eq!(dir, None);
        assert_eq!(flags.tag, Some("test".to_string()));
        assert_eq!(flags.terminal, Some("kitty".to_string()));
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_resume_flags_equals_form() {
        let (dir, flags, remaining) =
            extract_resume_flags(&s(&["--tag=test", "--terminal=alacritty"]));
        assert_eq!(dir, None);
        assert_eq!(flags.tag, Some("test".to_string()));
        assert_eq!(flags.terminal, Some("alacritty".to_string()));
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_resume_flags_none() {
        let (dir, flags, remaining) = extract_resume_flags(&s(&["--model", "opus"]));
        assert_eq!(dir, None);
        assert_eq!(flags.tag, None);
        assert_eq!(flags.terminal, None);
        assert_eq!(remaining, s(&["--model", "opus"]));
    }

    #[test]
    fn test_extract_resume_flags_dir() {
        let (dir, flags, remaining) =
            extract_resume_flags(&s(&["--dir", "/tmp/test", "--model", "opus"]));
        assert_eq!(dir, Some("/tmp/test".to_string()));
        assert_eq!(flags.tag, None);
        assert_eq!(flags.terminal, None);
        assert_eq!(remaining, s(&["--model", "opus"]));
    }

    #[test]
    fn test_extract_resume_flags_shared_launch_flags() {
        let (dir, flags, remaining) = extract_resume_flags(&s(&[
            "--dir=/tmp/test",
            "--headless",
            "--batch-id",
            "batch-1",
            "--run-here",
            "--hcom-prompt",
            "hi",
            "--hcom-system-prompt",
            "sys",
            "--model",
            "opus",
        ]));
        assert_eq!(dir, Some("/tmp/test".to_string()));
        assert!(flags.headless);
        assert_eq!(flags.batch_id, Some("batch-1".to_string()));
        assert_eq!(flags.run_here, Some(true));
        assert_eq!(flags.initial_prompt, Some("hi".to_string()));
        assert_eq!(flags.system_prompt, Some("sys".to_string()));
        assert_eq!(remaining, s(&["--model", "opus"]));
    }

    #[test]
    fn test_extract_resume_flags_stops_at_double_dash() {
        let (dir, flags, remaining) = extract_resume_flags(&s(&[
            "--dir",
            "/tmp/test",
            "--",
            "--dir",
            "tool-dir",
            "--model",
            "opus",
        ]));
        assert_eq!(dir, Some("/tmp/test".to_string()));
        assert_eq!(flags, crate::commands::launch::HcomLaunchFlags::default());
        assert_eq!(remaining, s(&["--dir", "tool-dir", "--model", "opus"]));
    }

    #[test]
    fn test_should_preview_resume_false_for_plain_resume() {
        assert!(!should_preview_resume(
            &crate::commands::launch::HcomLaunchFlags::default(),
            &[]
        ));
    }

    #[test]
    fn test_should_preview_resume_true_for_tool_args() {
        assert!(should_preview_resume(
            &crate::commands::launch::HcomLaunchFlags::default(),
            &s(&["--model", "opus"])
        ));
    }

    #[test]
    fn test_should_preview_resume_rpc_true_for_hcom_flags() {
        assert!(should_preview_resume_rpc(&s(&["--terminal", "kitty"])));
    }

    #[test]
    fn test_should_preview_resume_true_for_hcom_only_flags() {
        let flags = crate::commands::launch::HcomLaunchFlags {
            terminal: Some("kitty".to_string()),
            ..Default::default()
        };
        assert!(should_preview_resume(&flags, &[]));
    }

    #[test]
    fn test_resume_system_prompt_codex_fork_does_not_tell_agent_to_rebind() {
        let prompt = resume_system_prompt("codex", "luna", true, None);
        assert!(prompt.contains("already-assigned hcom identity"));
        assert!(!prompt.contains("Run hcom start"));
    }

    #[test]
    fn test_resume_system_prompt_non_codex_fork_states_new_identity() {
        let prompt = resume_system_prompt("claude", "luna", true, Some("feri"));
        assert!(prompt.contains("feri"), "should name the new identity");
        assert!(
            prompt.contains("--name feri"),
            "should state the --name flag"
        );
        assert!(
            !prompt.contains("Run hcom start"),
            "should not tell agent to rebind"
        );
        assert!(
            !prompt.contains("You are still 'luna'"),
            "should not resume as parent"
        );

        // None path falls back to already-assigned wording (no child name available)
        let prompt_none = resume_system_prompt("claude", "luna", true, None);
        assert!(prompt_none.contains("already-assigned hcom identity"));
        assert!(!prompt_none.contains("Run hcom start"));
    }

    #[test]
    fn test_build_remote_resume_output_uses_actual_launch_result_background() {
        let db = test_db();
        let output = build_remote_resume_output(
            &db,
            &LaunchResult {
                tool: "claude".to_string(),
                batch_id: "batch-1".to_string(),
                launched: 1,
                failed: 0,
                background: true,
                log_files: Vec::new(),
                handles: Vec::new(),
                errors: Vec::new(),
            },
            &s(&["--terminal", "kitty", "--tag", "ops", "--run-here"]),
            false,
            &GlobalFlags::default(),
        );

        assert_eq!(output.action, "resume");
        assert_eq!(output.tool, "claude");
        assert_eq!(output.tag.as_deref(), Some("ops"));
        assert_eq!(output.terminal.as_deref(), Some("kitty"));
        assert!(output.background);
        assert_eq!(output.run_here, Some(true));
    }

    #[test]
    fn test_build_remote_resume_output_marks_fork_action() {
        let db = test_db();
        let output = build_remote_resume_output(
            &db,
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
            &[],
            true,
            &GlobalFlags::default(),
        );

        assert_eq!(output.action, "fork");
        assert_eq!(output.tool, "codex");
        assert!(!output.background);
    }

    #[test]
    fn test_is_session_id_valid_uuid() {
        assert!(is_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        assert!(is_session_id("521cfc2b-be38-403a-b32e-4a49c9551b27"));
    }

    #[test]
    fn test_is_session_id_valid_opencode() {
        // opencode IDs are `ses_` + ULID-ish suffix (see opencode/src/id/id.ts)
        assert!(is_session_id("ses_019b12abcdefGHIJK0123456789"));
        assert!(is_session_id("ses_abcdef"));
    }

    #[test]
    fn test_is_session_id_rejects_names() {
        assert!(!is_session_id("cafe"));
        assert!(!is_session_id("boho"));
        assert!(!is_session_id("my-agent"));
        assert!(!is_session_id("impl-luna"));
        assert!(!is_session_id("review-kira"));
        assert!(!is_session_id(""));
        assert!(!is_session_id("ses_")); // prefix alone, no suffix
        assert!(!is_session_id("ses_with-dash")); // opencode IDs are alnum only
    }

    #[test]
    fn test_extract_cwd_claude() {
        // Claude records cwd in the first (and every) line. Read one line.
        let dir = std::env::temp_dir().join("hcom_test_cwd_claude");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","cwd":"/start/dir","message":"hi"}
{"type":"assistant","cwd":"/start/dir","message":"hello"}
"#,
        )
        .unwrap();
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), "claude");
        assert_eq!(result, Some("/start/dir".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_extract_cwd_codex() {
        // Codex records cwd in the session_meta payload on line 1.
        let dir = std::env::temp_dir().join("hcom_test_cwd_codex");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"cwd":"/start/dir"}}
{"type":"event_msg","payload":{"content":"hello"}}
"#,
        )
        .unwrap();
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), "codex");
        assert_eq!(result, Some("/start/dir".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial_test::serial]
    fn test_extract_cwd_gemini_reverse_hash_lookup() {
        // Gemini writes sha256(cwd) as `projectHash` in the session JSON, and
        // stores cwd → short-id in ~/.gemini/projects.json. This test wires up
        // a fake GEMINI_CLI_HOME and confirms the reverse lookup.
        use sha2::{Digest, Sha256};
        let base = std::env::temp_dir().join("hcom_test_cwd_gemini_ok");
        let gemini = base.join(".gemini");
        let session_dir = gemini.join("tmp/myproj/chats");
        std::fs::create_dir_all(&session_dir).unwrap();

        let fake_cwd = "/some/fake/cwd";
        let hex = Sha256::digest(fake_cwd.as_bytes())
            .iter()
            .fold(String::new(), |mut a, b| {
                use std::fmt::Write;
                let _ = write!(a, "{:02x}", b);
                a
            });

        // projects.json: cwd → short-id
        std::fs::write(
            gemini.join("projects.json"),
            format!(r#"{{"projects":{{"{fake_cwd}":"myproj"}}}}"#),
        )
        .unwrap();

        // Session JSON: projectHash = sha256(cwd)
        let session_path = session_dir.join("session-x.json");
        std::fs::write(&session_path, format!(r#"{{"projectHash":"{hex}"}}"#)).unwrap();

        // Stub GEMINI_CLI_HOME so recover_gemini_cwd reads from our fake tree.
        let prev = std::env::var("GEMINI_CLI_HOME").ok();
        // SAFETY: test is single-threaded enough for this module; serial_test
        // isn't in scope here, but other tests don't touch GEMINI_CLI_HOME.
        unsafe {
            std::env::set_var("GEMINI_CLI_HOME", &base);
        }
        let result = extract_cwd_from_transcript(session_path.to_str().unwrap(), "gemini");
        match prev {
            Some(v) => unsafe { std::env::set_var("GEMINI_CLI_HOME", v) },
            None => unsafe { std::env::remove_var("GEMINI_CLI_HOME") },
        }

        assert_eq!(result, Some(fake_cwd.to_string()));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    #[serial_test::serial]
    fn test_extract_cwd_gemini_no_registry_returns_none() {
        // When projects.json is missing, we can't reverse the hash → return None.
        let base = std::env::temp_dir().join("hcom_test_cwd_gemini_noreg");
        let gemini = base.join(".gemini/tmp/x/chats");
        std::fs::create_dir_all(&gemini).unwrap();
        let path = gemini.join("test.json");
        std::fs::write(&path, r#"{"projectHash":"deadbeef"}"#).unwrap();
        let prev = std::env::var("GEMINI_CLI_HOME").ok();
        unsafe {
            std::env::set_var("GEMINI_CLI_HOME", &base);
        }
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), "gemini");
        match prev {
            Some(v) => unsafe { std::env::set_var("GEMINI_CLI_HOME", v) },
            None => unsafe { std::env::remove_var("GEMINI_CLI_HOME") },
        }
        assert_eq!(result, None);
        std::fs::remove_dir_all(&base).ok();
    }
}
