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
    let name = crate::identity::resolve_display_name_or_stopped(&db, name)
        .unwrap_or_else(|| name.to_string());
    let hcom_config = load_hcom_config();
    let ctx = crate::shared::HcomContext::from_os();

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

    let (resolved, plan) = resolve_name_to_plan(&db, &name, fork, extra_args, flags)?;
    let is_adoption = plan.launch.name.is_none();
    if ctx.is_inside_ai_tool() && !flags.go && should_preview_resume_rpc(extra_args) {
        print_resume_preview(&plan, &hcom_config, &resolved, fork);
        return Ok(0);
    }

    let exit = execute_prepared_resume(&db, &resolved, fork, &plan, &hcom_config, true)?;
    if is_adoption {
        log_info(
            if fork { "fork" } else { "resume" },
            &format!("cmd.adopt_{}", if fork { "fork" } else { "resume" }),
            &format!("session_id={}", resolved),
        );
    }
    Ok(exit)
}

/// Remote-RPC entry point: run the same resolution chain as `do_resume`
/// (UUID/thread-name → binding → events-fallback → adoption → name-based
/// resume) but return a `LaunchResult` instead of printing feedback. Called
/// by `handle_remote_resume` on the target device, where the device suffix
/// has already been stripped by the dispatcher.
pub fn run_local_resume_result(
    db: &HcomDb,
    name: &str,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
) -> Result<LaunchResult> {
    let (resolved, plan) = resolve_name_to_plan(db, name, fork, extra_args, flags)?;
    execute_prepared_resume_result(db, &resolved, fork, &plan)
}

/// Walk the resume/fork resolution chain once, in a single place:
///
/// 1. Resolve display-name or stopped-name shorthand to a canonical name.
/// 2. If input is a session ID (UUID or `ses_`), check `session_bindings`
///    as a collision guard against a live instance, then try to reclaim
///    identity via the life.stopped events lookup, then fall through to
///    on-disk adoption. The binding is a best-effort lookaside — a stale
///    row (crash/orphaned) must NOT short-circuit to a name-based resume
///    whose snapshot might be missing or out of date; events are the
///    source of truth.
/// 3. If the name isn't a known hcom instance and lacks a device suffix,
///    try resolving it as a Claude/Codex thread name, then run the same
///    binding → events → adoption chain on the resolved session ID.
/// 4. Otherwise, prepare a plan for an existing hcom instance.
///
/// Returns `(resolved_name_for_display, prepared_plan)`. The loop form
/// avoids re-opening the DB that the old recursive `do_resume` calls did.
fn resolve_name_to_plan(
    db: &HcomDb,
    name: &str,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
) -> Result<(String, PreparedResume)> {
    let mut current = crate::identity::resolve_display_name_or_stopped(db, name)
        .unwrap_or_else(|| name.to_string());

    // A loop over reclaim hops (binding → events → redirect to instance name).
    // Bounded by MAX_HOPS in case of pathological DB state.
    for _ in 0..8 {
        if is_session_id(&current) {
            if let Ok(Some(bound)) = db.get_session_binding(&current) {
                if matches!(db.get_instance_full(&bound), Ok(Some(_))) {
                    bail!(
                        "Session {} is currently active as '{}' — run hcom kill {} first",
                        current,
                        bound,
                        bound
                    );
                }
                // Stale binding: events are authoritative. Fall through.
            }
            if let Ok(Some(instance_name)) = db.find_stopped_instance_by_session_id(&current) {
                current = instance_name;
                continue;
            }
            let plan = build_adopt_plan(db, &current, fork, extra_args, flags)?;
            return Ok((current, plan));
        }

        if matches!(db.get_instance_full(&current), Ok(None) | Err(_))
            && crate::relay::control::split_device_suffix(&current).is_none()
        {
            if let Some(session_id) = resolve_thread_name(&current)? {
                if let Ok(Some(bound)) = db.get_session_binding(&session_id) {
                    if matches!(db.get_instance_full(&bound), Ok(Some(_))) {
                        bail!(
                            "Session {} (thread '{}') is currently active as '{}' — run hcom kill {} first",
                            session_id,
                            current,
                            bound,
                            bound
                        );
                    }
                    // Stale binding: fall through to events.
                }
                if let Ok(Some(instance_name)) = db.find_stopped_instance_by_session_id(&session_id)
                {
                    current = instance_name;
                    continue;
                }
                let plan = build_adopt_plan(db, &session_id, fork, extra_args, flags)?;
                return Ok((session_id, plan));
            }
        }

        let plan = prepare_resume_plan(db, &current, fork, extra_args, flags)?;
        return Ok((current, plan));
    }

    bail!(
        "Name resolution for '{}' did not converge (possible circular binding)",
        name
    );
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
    let (
        tool,
        session_id,
        launch_args_str,
        tag,
        background,
        last_event_id,
        snapshot_dir,
        display_name,
    ) = match source {
        ResumeSource::Instance { name } => {
            if !fork {
                if let Ok(Some(_)) = db.get_instance_full(name) {
                    bail!("'{}' is still active — run hcom kill {} first", name, name);
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
        // Named-fork belt-and-suspenders: the pre-registered instance row
        // was created with last_event_id=0 and may inherit a stale
        // HCOM_LAUNCH_EVENT_ID from the parent's env if the tool doesn't
        // propagate our override cleanly. Stamp the current position
        // directly on the DB so there's no replay window.
        //
        // Adoption-fork (plan.launch.name=None) doesn't need the belt:
        // there's no pre-reg row, so the SessionStart hook creates the
        // instance fresh using HCOM_LAUNCH_EVENT_ID (always set by
        // launcher::launch to current max) — no zero-cursor window to
        // protect against.
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
    // Filter action='stopped' in SQL so we can't miss it past a LIMIT window
    // (old 10-row LIMIT could drop the snapshot after many relaunches).
    let mut stmt = db.conn().prepare(
        "SELECT data FROM events
         WHERE type='life'
           AND instance=?
           AND json_extract(data, '$.action') = 'stopped'
         ORDER BY id DESC LIMIT 1",
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
        "No stopped snapshot found for '{name}'. Not a known hcom instance, \
         session UUID, or recognized thread name."
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
        "opencode" | "kilocode" => {
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
        "opencode" | "kilocode" => merge_opencode_args(original, resume),
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

// ── Thread-name resolution ───────────────────────────────────────────────

/// Resolve a user-visible thread name (e.g. a Claude `/rename` title or a
/// Codex thread name) to a session UUID by scanning tool-specific indexes.
///
/// Returns `Ok(Some(uuid))` on a unique match, `Ok(None)` if no tool
/// recognizes the name, or `Err` on within-tool or cross-tool ambiguity.
fn resolve_thread_name(name: &str) -> Result<Option<String>> {
    let claude_match = resolve_claude_thread_name(name)?;
    let codex_match = resolve_codex_thread_name(name)?;

    match (claude_match, codex_match) {
        (Some(claude_id), Some(codex_id)) => {
            bail!(
                "Thread name '{}' matches both Claude (session {}) and Codex (session {}).\n\
                 Use `hcom claude --resume '{}'` or `hcom codex resume '{}'` to disambiguate.",
                name,
                claude_id,
                codex_id,
                name,
                name
            );
        }
        (Some(id), None) => {
            log_info(
                "resume",
                "resolve_thread_name.claude",
                &format!("name={} session_id={}", name, id),
            );
            Ok(Some(id))
        }
        (None, Some(id)) => {
            log_info(
                "resume",
                "resolve_thread_name.codex",
                &format!("name={} session_id={}", name, id),
            );
            Ok(Some(id))
        }
        (None, None) => Ok(None),
    }
}

/// One candidate match produced while scanning a tool's thread-name index.
struct ThreadMatch {
    session_id: String,
    /// Human-readable "when last touched" (mtime ISO-ish for Claude,
    /// `updated_at` for Codex). Used only in ambiguity error messages.
    when: String,
}

/// Resolve a Claude Code custom title to a session UUID by scanning
/// `~/.claude/projects/*/*.jsonl` for `{"type":"custom-title","customTitle":"..."}`.
///
/// `/rename` appends a new `custom-title` entry each time, so within a single
/// transcript only the LAST entry reflects the session's current title. A
/// session renamed `A → B → C` must not match for `A` or `B`.
///
/// Across files, if multiple distinct sessions currently have the same title,
/// we bail rather than silently pick the most recent — the user may have
/// accidentally reused a name.
fn resolve_claude_thread_name(name: &str) -> Result<Option<String>> {
    let projects_dir = claude_config_dir().join("projects");
    if !projects_dir.is_dir() {
        return Ok(None);
    }

    let mut matches: Vec<ThreadMatch> = Vec::new();

    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let sub_entries = match std::fs::read_dir(entry.path()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for sub_entry in sub_entries.flatten() {
            let path = sub_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader = std::io::BufReader::new(file);
            // Track the LAST custom-title entry in the file — that's the
            // session's current title.
            let mut last_title: Option<(String, String)> = None;
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                // Fast pre-filter: skip lines that can't contain a custom-title entry.
                if !line.contains("custom-title") {
                    continue;
                }
                let parsed: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if parsed.get("type").and_then(|v| v.as_str()) != Some("custom-title") {
                    continue;
                }
                let title = match parsed.get("customTitle").and_then(|v| v.as_str()) {
                    Some(t) => t.to_string(),
                    None => continue,
                };
                let session_id = match parsed.get("sessionId").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                last_title = Some((title, session_id));
            }
            if let Some((title, session_id)) = last_title {
                if title == name {
                    let when = sub_entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(format_system_time)
                        .unwrap_or_else(|| "unknown".to_string());
                    matches.push(ThreadMatch { session_id, when });
                }
            }
        }
    }

    resolve_one_match("Claude", name, matches)
}

/// Resolve a Codex thread name to a session UUID by scanning
/// `~/.codex/session_index.jsonl`. If multiple rows currently share the
/// name, bail instead of silently picking by `updated_at`.
fn resolve_codex_thread_name(name: &str) -> Result<Option<String>> {
    let index_path = match dirs::home_dir() {
        Some(h) => h.join(".codex/session_index.jsonl"),
        None => return Ok(None),
    };
    let file = match std::fs::File::open(&index_path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let reader = std::io::BufReader::new(file);
    let mut matches: Vec<ThreadMatch> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if parsed.get("thread_name").and_then(|v| v.as_str()) != Some(name) {
            continue;
        }
        let Some(id) = parsed.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let when = parsed
            .get("updated_at")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        matches.push(ThreadMatch {
            session_id: id.to_string(),
            when,
        });
    }

    resolve_one_match("Codex", name, matches)
}

/// Turn a vec of candidate matches into at most one. >1 → ambiguity bail.
fn resolve_one_match(tool: &str, name: &str, matches: Vec<ThreadMatch>) -> Result<Option<String>> {
    if matches.len() <= 1 {
        return Ok(matches.into_iter().next().map(|m| m.session_id));
    }

    let mut lines = String::new();
    for m in &matches {
        lines.push_str(&format!("  - {} (touched {})\n", m.session_id, m.when));
    }
    bail!(
        "Thread name '{}' matches {} {} sessions:\n{}\
         Pass the UUID directly to disambiguate.",
        name,
        matches.len(),
        tool,
        lines,
    );
}

/// Format a SystemTime as an ISO-8601-ish UTC string for error messages.
fn format_system_time(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0);
    dt.map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "unknown".to_string())
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
    s.starts_with("ses_") && s.len() >= 8 && s[4..].chars().all(|c| c.is_ascii_alphanumeric())
}

/// Locate opencode's data dir. Opencode itself follows XDG on every platform
/// (including macOS, unlike `dirs::data_dir`), so we check XDG-style paths
/// first and only fall back to the platform default when neither exists.
///
/// Resolution order, returning the first that actually exists on disk:
/// 1. `$XDG_DATA_HOME/opencode` (if `XDG_DATA_HOME` is set)
/// 2. `~/.local/share/opencode` (macOS XDG-style + Linux)
/// 3. `dirs::data_dir().join("opencode")` (macOS Apple-style + Windows
///    `%LOCALAPPDATA%`)
///
/// If none exist, falls through to the platform default path (even though
/// it's absent) so callers can surface a useful "searched here" message.
fn opencode_data_dir() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            candidates.push(std::path::PathBuf::from(xdg).join("opencode"));
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/share/opencode"));
    }
    if let Some(data) = dirs::data_dir() {
        candidates.push(data.join("opencode"));
    }

    for candidate in &candidates {
        if candidate.is_dir() {
            return Some(candidate.clone());
        }
    }
    candidates.into_iter().next_back()
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

fn lookup_kilocode_session(session_id: &str) -> Option<String> {
    let kilocode_data = opencode_data_dir().map(|d| {
        let parent = d.parent()?;
        Some(parent.join("kilocode"))
    })??;
    let db_path = kilocode_data.join("kilocode.db");
    if !db_path.exists() {
        return None;
    }
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
    // 1. Opencode/KiloCode: prefix-scoped, query the SQLite DB directly.
    if is_opencode_session_id(session_id) {
        if lookup_opencode_session(session_id).is_some() {
            return Some(("opencode".to_string(), None));
        }
        if lookup_kilocode_session(session_id).is_some() {
            return Some(("kilocode".to_string(), None));
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
/// - **Claude**: `cwd` is on most entry lines but NOT on line 1 (which is a
///   `permission-mode` header). Scan the first few lines until one carries a
///   non-empty `cwd`; give up after a small cap to keep I/O bounded.
/// - **Codex**: first line is `session_meta` with `payload.cwd`; read line 1.
/// - **Gemini**: the session JSON has `projectHash = sha256(cwd)` (hex).
///   `~/.gemini/projects.json` maps `cwd → short-id`. Hash each key and
///   match against `projectHash` to recover the original CWD.
fn extract_cwd_from_transcript(path: &str, tool: &str) -> Option<String> {
    match tool {
        "claude" => scan_lines_for_cwd(path, 20, |v| v.get("cwd").and_then(|c| c.as_str())),
        "codex" => scan_lines_for_cwd(path, 1, |v| {
            v.get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
        }),
        "gemini" => recover_gemini_cwd(path),
        _ => None,
    }
}

/// Read up to `max_lines` lines from a JSONL transcript and return the first
/// non-empty CWD produced by `pick`. Malformed lines are skipped.
fn scan_lines_for_cwd(
    path: &str,
    max_lines: usize,
    pick: impl Fn(&serde_json::Value) -> Option<&str>,
) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(max_lines) {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if let Some(cwd) = pick(&parsed) {
            if !cwd.is_empty() {
                return Some(cwd.to_string());
            }
        }
    }
    None
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

/// Locate a session on disk, recover its CWD, and build the `PreparedResume`.
/// Callers: `resolve_name_to_plan` (both interactive and RPC paths).
fn build_adopt_plan(
    db: &HcomDb,
    session_id: &str,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
) -> Result<PreparedResume> {
    let (tool, transcript_path) = find_session_on_disk(session_id).ok_or_else(|| {
        // find_session_on_disk short-circuits for `ses_` IDs (only opencode is
        // searched), so scope the error to match what was actually checked.
        let opencode_db = opencode_data_dir()
            .map(|d| d.join("opencode.db").display().to_string())
            .unwrap_or_else(|| "(no data dir)".to_string());
        let kilocode_db = opencode_data_dir()
            .and_then(|d| {
                let parent = d.parent()?;
                Some(parent.join("kilocode/kilocode.db").display().to_string())
            })
            .unwrap_or_else(|| "(no data dir)".to_string());
        if is_opencode_session_id(session_id) {
            anyhow::anyhow!(
                "Session {sid} not found. Searched:\n  \
                 - Opencode: {opencode_db} (table 'session')\n  \
                 - KiloCode: {kilocode_db} (table 'session')",
                sid = session_id,
                opencode_db = opencode_db,
                kilocode_db = kilocode_db,
            )
        } else {
            let claude_projects = claude_config_dir().join("projects");
            anyhow::anyhow!(
                "Session {sid} not found. Searched:\n  \
                 - Claude:   {claude}/*/{sid}.jsonl\n  \
                 - Codex:    ~/.codex/sessions/**/*-{sid}.jsonl\n  \
                 - Gemini:   ~/.gemini/tmp/*/chats/session-*-{short}*.json",
                sid = session_id,
                claude = claude_projects.display(),
                short = session_id.split('-').next().unwrap_or(session_id),
            )
        }
    })?;

    // CWD recovery: for opencode, the session row carries `directory` directly.
    // For Claude/Codex, scan transcript entries for the recorded cwd (CWD is
    // fixed at session start; no tool changes CWD mid-session). For fork we
    // start in $PWD (or --dir) by design.
    let cwd_hint = if fork {
        None
    } else {
        let raw = if tool == "opencode" {
            lookup_opencode_session(session_id)
        } else if tool == "kilocode" {
            lookup_kilocode_session(session_id)
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

    prepare_resume_plan_from_source(
        db,
        ResumeSource::Disk {
            session_id: session_id.to_string(),
            tool,
            cwd_hint,
        },
        fork,
        extra_args,
        flags,
    )
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
    fn test_resolve_one_match_zero_or_one() {
        assert_eq!(resolve_one_match("Claude", "x", vec![]).unwrap(), None);
        let single = vec![ThreadMatch {
            session_id: "abc".to_string(),
            when: "t1".to_string(),
        }];
        assert_eq!(
            resolve_one_match("Claude", "x", single).unwrap(),
            Some("abc".to_string())
        );
    }

    #[test]
    fn test_resolve_one_match_ambiguous_bails() {
        let multi = vec![
            ThreadMatch {
                session_id: "sid-a".to_string(),
                when: "2026-01-01T00:00:00Z".to_string(),
            },
            ThreadMatch {
                session_id: "sid-b".to_string(),
                when: "2026-02-01T00:00:00Z".to_string(),
            },
        ];
        let err = resolve_one_match("Claude", "dup", multi)
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 Claude sessions"), "got: {err}");
        assert!(err.contains("sid-a") && err.contains("sid-b"), "got: {err}");
        assert!(err.contains("UUID directly"), "got: {err}");
    }

    /// Point claude_config_dir() at `dir` for the duration of `f` by setting
    /// CLAUDE_CONFIG_DIR. Restored on exit. serial_test required.
    fn with_claude_config_dir<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("CLAUDE_CONFIG_DIR").ok();
        // SAFETY: tests using this must be serial_test::serial — only one
        // test at a time touches this env var.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", dir);
        }
        let out = f();
        match prev {
            Some(v) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
        }
        out
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_claude_thread_name_prefers_last_custom_title() {
        // A session renamed A → B → C must match for C (the current title),
        // not A or B (obsolete).
        let cfg = std::env::temp_dir().join("hcom_test_claude_rename_chain");
        let projects = cfg.join("projects/proj");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(
            projects.join("s1.jsonl"),
            r#"{"type":"custom-title","customTitle":"A","sessionId":"s1"}
{"type":"custom-title","customTitle":"B","sessionId":"s1"}
{"type":"custom-title","customTitle":"C","sessionId":"s1"}
"#,
        )
        .unwrap();

        let (old, mid, cur) = with_claude_config_dir(&cfg, || {
            (
                resolve_claude_thread_name("A").unwrap(),
                resolve_claude_thread_name("B").unwrap(),
                resolve_claude_thread_name("C").unwrap(),
            )
        });

        assert_eq!(old, None, "obsolete title A must not resolve");
        assert_eq!(mid, None, "obsolete title B must not resolve");
        assert_eq!(cur, Some("s1".to_string()), "current title C must resolve");
        std::fs::remove_dir_all(&cfg).ok();
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_claude_thread_name_bails_on_within_tool_duplicate() {
        // Two distinct sessions both currently have customTitle="dup" — must
        // bail rather than silently pick by mtime.
        let cfg = std::env::temp_dir().join("hcom_test_claude_dup_title");
        let projects = cfg.join("projects/proj");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(
            projects.join("s1.jsonl"),
            r#"{"type":"custom-title","customTitle":"dup","sessionId":"sess-aaaa"}
"#,
        )
        .unwrap();
        std::fs::write(
            projects.join("s2.jsonl"),
            r#"{"type":"custom-title","customTitle":"dup","sessionId":"sess-bbbb"}
"#,
        )
        .unwrap();

        let res = with_claude_config_dir(&cfg, || resolve_claude_thread_name("dup"));

        let err = res.unwrap_err().to_string();
        assert!(err.contains("matches 2 Claude sessions"), "got: {err}");
        assert!(
            err.contains("sess-aaaa") && err.contains("sess-bbbb"),
            "got: {err}"
        );
        std::fs::remove_dir_all(&cfg).ok();
    }

    #[test]
    fn test_run_local_resume_result_routes_uuid_to_adoption() {
        // Remote-RPC entrypoint must walk the UUID/thread-name resolution
        // chain. A UUID with no on-disk transcript should error with the
        // adoption "Session not found" message (proving we hit find_session_on_disk),
        // not the name-based "No stopped snapshot found" message.
        let db = test_db();
        let err = run_local_resume_result(
            &db,
            "12345678-1234-5678-1234-567812345678",
            false,
            &[],
            &GlobalFlags::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("Session 12345678-1234-5678-1234-567812345678 not found"),
            "expected adoption error, got: {err}"
        );
    }

    #[test]
    fn test_run_local_resume_result_routes_opencode_uuid_to_adoption() {
        // Sanity: opencode-style IDs also route through adoption, with the
        // opencode-specific error.
        let db = test_db();
        let err = run_local_resume_result(
            &db,
            "ses_nonexistentfakesession12345",
            false,
            &[],
            &GlobalFlags::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("Session ses_nonexistentfakesession12345 not found"),
            "expected adoption error, got: {err}"
        );
        assert!(
            err.contains("Opencode"),
            "error should mention opencode: {err}"
        );
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
    fn test_extract_cwd_claude_skips_permission_mode_header() {
        // Real Claude transcripts start with a `permission-mode` line that has
        // no `cwd`; cwd first appears on a later entry. Must scan forward.
        let dir = std::env::temp_dir().join("hcom_test_cwd_claude_header");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"abc"}
{"type":"snapshot","messageId":"m1"}
{"parentUuid":null,"type":"user","cwd":"/real/cwd","message":"hi"}
"#,
        )
        .unwrap();
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), "claude");
        assert_eq!(result, Some("/real/cwd".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_extract_cwd_claude_gives_up_after_cap() {
        // If no cwd appears in the first 20 lines, return None rather than
        // reading the full transcript.
        let dir = std::env::temp_dir().join("hcom_test_cwd_claude_cap");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");
        let mut content = String::new();
        for _ in 0..25 {
            content.push_str(r#"{"type":"noise"}"#);
            content.push('\n');
        }
        content.push_str(r#"{"type":"user","cwd":"/late/cwd"}"#);
        content.push('\n');
        std::fs::write(&path, content).unwrap();
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), "claude");
        assert_eq!(result, None);
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
