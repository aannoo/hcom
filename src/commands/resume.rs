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
use crate::commands::transcript::detect_agent_type;
use crate::db::HcomDb;
use crate::hooks::codex::derive_codex_transcript_path;
use crate::hooks::gemini::derive_gemini_transcript_path;
use crate::hooks::kimi::derive_kimi_transcript_path;
use crate::launcher::{self, LaunchParams, LaunchResult};
use crate::log::log_info;
use crate::router::GlobalFlags;
use crate::shared::ST_INACTIVE;
use crate::transcript::claude_projects_dir;

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
    tracked_fork_identity: Option<TrackedForkIdentity>,
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

struct TrackedForkIdentity {
    parent_name: String,
    effective_tag: Option<String>,
    custom_initial_prompt: Option<String>,
    custom_system_prompt: Option<String>,
}

struct ResumePromptInput<'a> {
    tool: &'a str,
    display_name: &'a str,
    fork: bool,
    is_adoption: bool,
    child_name: Option<&'a str>,
    effective_tag: Option<&'a str>,
    custom_system_prompt: Option<&'a str>,
    custom_initial_prompt: Option<&'a str>,
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
        if ctx.is_inside_ai_tool()
            && !flags.go
            && should_preview_resume_rpc(extra_args)
            && let Ok(plan) = prepare_resume_plan(&db, &name, fork, extra_args, flags)
        {
            print_resume_preview(&plan, &hcom_config, &name, fork);
            return Ok(0);
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
            inline_readiness_wait_secs: None,
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

    let inline_readiness_wait_secs = if ctx.is_inside_ai_tool() {
        Some(crate::commands::launch::INLINE_SINGLE_LAUNCH_WAIT_SECS)
    } else {
        None
    };
    let exit = execute_prepared_resume(
        &db,
        &resolved,
        fork,
        &plan,
        &hcom_config,
        true,
        inline_readiness_wait_secs,
    )?;
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
            if let Ok(Some(bound)) = db.get_session_binding(&current)
                && matches!(db.get_instance_full(&bound), Ok(Some(_)))
            {
                bail!(
                    "Session {} is currently active as '{}' — run hcom kill {} first",
                    current,
                    bound,
                    bound
                );
            }
            // Stale binding: events are authoritative. Fall through.
            if let Ok(Some(instance_name)) = db.find_stopped_instance_by_session_id(&current) {
                current = instance_name;
                continue;
            }
            let plan = build_adopt_plan(db, &current, fork, extra_args, flags)?;
            return Ok((current, plan));
        }

        if matches!(db.get_instance_full(&current), Ok(None) | Err(_))
            && crate::relay::control::split_device_suffix(&current).is_none()
            && let Some(session_id) = resolve_thread_name(&current)?
        {
            if let Ok(Some(bound)) = db.get_session_binding(&session_id)
                && matches!(db.get_instance_full(&bound), Ok(Some(_)))
            {
                bail!(
                    "Session {} (thread '{}') is currently active as '{}' — run hcom kill {} first",
                    session_id,
                    current,
                    bound,
                    bound
                );
            }
            // Stale binding: fall through to events.
            if let Ok(Some(instance_name)) = db.find_stopped_instance_by_session_id(&session_id) {
                current = instance_name;
                continue;
            }
            let plan = build_adopt_plan(db, &session_id, fork, extra_args, flags)?;
            return Ok((session_id, plan));
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
            if !fork
                && let Ok(Some(inst)) = db.get_instance_full(name)
                && inst.status != ST_INACTIVE
            {
                bail!("'{}' is still active — run hcom kill {} first", name, name);
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
            .map(|p| crate::shared::platform::child_process_path(&p))
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

    // Merge with original launch args (only applicable for tracked instances).
    let original_args: Vec<String> = if !launch_args_str.is_empty() {
        serde_json::from_str(&launch_args_str).unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut persisted_args = original_args.clone();
    persisted_args.extend(clean_extra.iter().cloned());

    let mut cli_tool_args = build_resume_args(&tool, &session_id, fork);
    cli_tool_args.extend(clean_extra);

    let merged_cli_args = if !original_args.is_empty() {
        merge_resume_args(&tool, &original_args, &cli_tool_args)
    } else {
        cli_tool_args
    };

    let mut merged_args = merged_cli_args.clone();

    if launch_flags.headless && tool != "claude" && tool != "kimi" {
        bail!("--headless is only supported for Claude and Kimi resume/fork launches");
    }

    let launch_tool = crate::launcher::LaunchTool::from_str(&tool)?;
    let is_headless =
        launch_flags.headless || is_background_from_args(&launch_tool, &merged_args) || background;

    // Claude background defaults to the live PTY session; only an explicit
    // `-p`/`--print` (NativePrint) gets the detached print-mode defaults. This
    // mirrors `prepare_launch_execution` on the launch path so resume/fork and
    // launch share one execution-mode policy. `-p` is opt-in because, from
    // 2026-06-15, `claude -p` draws from a separate Agent SDK credit pool.
    if tool == "claude"
        && is_headless
        && merged_args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-p" | "--print"))
    {
        merged_args.extend([
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ]);
    }

    crate::commands::launch::validate_claude_headless_launch(
        &tool,
        is_headless,
        &merged_args,
        launch_flags.initial_prompt.as_deref(),
    )?;

    let launcher_name =
        resolve_launcher_name(db, flags, std::env::var("HCOM_PROCESS_ID").ok().as_deref());
    let launcher_name_for_output = launcher_name.clone();

    // Choose a preview child name only for tracked-instance forks, since the
    // identity-reset prompts reference it. Actual launch reserves a fresh name
    // under the shared flock in execute_prepared_resume_result.
    let fork_child_name = if fork && !is_adoption {
        Some(crate::instance_names::allocate_unreserved_name(db)?)
    } else {
        None
    };
    let effective_tag = launch_flags.tag.clone().or(inherited_tag.clone());

    let (effective_system_prompt, fork_initial_prompt, append_reply_handoff) =
        build_resume_prompts(ResumePromptInput {
            tool: &tool,
            display_name: &display_name,
            fork,
            is_adoption,
            child_name: fork_child_name.as_deref(),
            effective_tag: effective_tag.as_deref(),
            custom_system_prompt: launch_flags.system_prompt.as_deref(),
            custom_initial_prompt: launch_flags.initial_prompt.as_deref(),
        });
    let output_tag = effective_tag.clone();
    let launch_tag = effective_tag.clone();

    // Instance name for LaunchParams:
    // - Adoption: None (launcher allocates; SessionStart hook binds via session_bindings)
    // - Tracked fork: preview-only fork_child_name; execute swaps in a reserved name
    // - Tracked resume: preserve existing hcom name
    let launch_name = if is_adoption {
        None
    } else if fork {
        fork_child_name.clone()
    } else {
        Some(display_name.clone())
    };
    let tracked_fork_identity = if fork && !is_adoption {
        Some(TrackedForkIdentity {
            parent_name: display_name.clone(),
            effective_tag: effective_tag.clone(),
            custom_initial_prompt: launch_flags.initial_prompt.clone(),
            custom_system_prompt: launch_flags.system_prompt.clone(),
        })
    } else {
        None
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
            persisted_args: Some(persisted_args),
            // Forks start a new session on first turn; only plain resume
            // inherits the prior id so kill-before-bind stays resumable.
            prior_session_id: (!fork).then(|| session_id.clone()),
            tag: launch_tag,
            system_prompt: effective_system_prompt,
            initial_prompt: fork_initial_prompt,
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
            append_reply_handoff,
        },
        last_event_id,
        session_id,
        tracked_fork_identity,
    })
}

fn execute_prepared_resume(
    db: &HcomDb,
    name: &str,
    fork: bool,
    plan: &PreparedResume,
    hcom_config: &crate::config::HcomConfig,
    print_feedback_now: bool,
    inline_readiness_wait_secs: Option<u64>,
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
            inline_readiness_wait_secs,
        };
        print_launch_feedback(db, &result, &output)?;
        // Resume/fork share launch's inline readiness wait + exit-code mapping:
        // when invoked from inside an AI tool we block briefly so the caller
        // sees ready/blocked/failed instead of a bare "spawned" line.
        let readiness_state = inline_readiness_wait_secs
            .filter(|_| result.launched == 1)
            .map(|secs| crate::commands::launch::print_inline_launch_readiness(db, &result, secs));
        log_info(
            if fork { "fork" } else { "resume" },
            &format!("cmd.{}", if fork { "fork" } else { "resume" }),
            &format!(
                "name={} tool={} session={} launched={}",
                name, plan.output.tool, plan.session_id, result.launched
            ),
        );
        return Ok(crate::commands::launch::readiness_exit_code(
            readiness_state,
            result.failed,
        ));
    }

    Ok(if result.launched > 0 { 0 } else { 1 })
}

fn execute_prepared_resume_result(
    db: &HcomDb,
    name: &str,
    fork: bool,
    plan: &PreparedResume,
) -> Result<LaunchResult> {
    let launch = prepare_launch_for_execution(db, plan)?;
    let result = launcher::launch(db, launch.clone())?;

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
        if let Some(ref child_name) = launch.name {
            crate::instances::update_instance_position(
                db,
                child_name,
                &serde_json::Map::from_iter([("last_event_id".to_string(), json!(current_max))]),
            );
        }
    }
    Ok(result)
}

fn prepare_launch_for_execution(db: &HcomDb, plan: &PreparedResume) -> Result<LaunchParams> {
    let mut launch = plan.launch.clone();
    let Some(identity) = &plan.tracked_fork_identity else {
        return Ok(launch);
    };

    let reserved_child_name = crate::instance_names::reserve_generated_name(db)?;
    let (system_prompt, initial_prompt, append_reply_handoff) =
        build_resume_prompts(ResumePromptInput {
            tool: &launch.tool,
            display_name: &identity.parent_name,
            fork: true,
            is_adoption: false,
            child_name: Some(&reserved_child_name),
            effective_tag: identity.effective_tag.as_deref(),
            custom_system_prompt: identity.custom_system_prompt.as_deref(),
            custom_initial_prompt: identity.custom_initial_prompt.as_deref(),
        });
    launch.name = Some(reserved_child_name);
    launch.system_prompt = system_prompt;
    launch.initial_prompt = initial_prompt;
    launch.append_reply_handoff = append_reply_handoff;
    Ok(launch)
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
    let tool_lookup = if tool == "claude-pty" { "claude" } else { tool };
    let parsed = tool_lookup
        .parse::<crate::tool::Tool>()
        .map_err(|_| anyhow::anyhow!("Unknown tool '{}' in saved session metadata", tool))?;

    if !fork {
        return Ok(());
    }
    // Drive fork support from the spec so help text and validation can't drift.
    // Accepts canonical names + aliases (e.g. `"agy"` → Antigravity).
    let spec = parsed.spec();
    if spec.resume.and_then(|r| r.fork).is_none() {
        bail!("{} does not support session forking (hcom f)", spec.label);
    }
    Ok(())
}

fn build_resume_prompts(input: ResumePromptInput<'_>) -> (Option<String>, Option<String>, bool) {
    let ResumePromptInput {
        tool,
        display_name,
        fork,
        is_adoption,
        child_name,
        effective_tag,
        custom_system_prompt,
        custom_initial_prompt,
    } = input;

    // Codex tracked-instance fork identity reset belongs in the initial prompt.
    // Adoption-fork has no prior hcom identity, so normal bootstrap handles it.
    let initial_prompt = if fork && tool == "codex" && !is_adoption {
        let child_name = child_name.expect("tracked fork child name should be available");
        let child_display = effective_tag
            .map(|tag| format!("{tag}-{child_name}"))
            .unwrap_or_else(|| child_name.to_string());
        let identity_reset = format!(
            "You are a fork of {display_name}, but your new hcom identity is now {child_display}.\n\
             Your hcom name is {child_name}.\n\
             Do not use {display_name}'s hcom identity anymore, even if it appears in inherited thread history.\n\
             Use [hcom:{child_name}] in your first response only.\n\
             Use `hcom ... --name {child_name}` for all hcom commands.\n\
             If asked about your identity, answer exactly: {child_display}"
        );
        Some(match custom_initial_prompt {
            Some(user_prompt) if !user_prompt.trim().is_empty() => {
                format!("{identity_reset}\n\n{user_prompt}")
            }
            _ => identity_reset,
        })
    } else {
        custom_initial_prompt.map(ToString::to_string)
    };

    // System prompt:
    // - Tracked-instance resume/fork: identity-carrying prompt (existing behavior).
    // - Adoption: None — SessionStart issues the normal fresh-launch bootstrap.
    let base_system_prompt = if is_adoption {
        None
    } else {
        Some(resume_system_prompt(tool, display_name, fork, child_name))
    };
    let system_prompt = match (base_system_prompt, custom_system_prompt) {
        (Some(base), Some(custom)) if !custom.trim().is_empty() => {
            Some(format!("{base}\n\n{custom}"))
        }
        (Some(base), _) => Some(base),
        (None, Some(custom)) if !custom.trim().is_empty() => Some(custom.to_string()),
        (None, _) => None,
    };

    // Codex tracked-instance fork uses initial_prompt for an identity reset;
    // don't dilute it with a reply-handoff suffix. Adoption-fork has no reset.
    let append_reply_handoff = !(fork && tool == "codex" && !is_adoption);
    (system_prompt, initial_prompt, append_reply_handoff)
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
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str)
            && data.get("action").and_then(|v| v.as_str()) == Some("stopped")
            && let Some(snapshot) = data.get("snapshot")
        {
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

    bail!(
        "No stopped snapshot found for '{name}'. Not a known hcom instance, \
         session UUID, or recognized thread name."
    )
}

/// Build tool-specific resume/fork args from the integration spec.
fn build_resume_args(tool: &str, session_id: &str, fork: bool) -> Vec<String> {
    use crate::integration_spec::{ForkArgs, ResumeArgs};
    // claude-pty resolves to the Claude spec.
    let tool_lookup = if tool == "claude-pty" { "claude" } else { tool };
    let Ok(tool_enum) = tool_lookup.parse::<crate::tool::Tool>() else {
        return Vec::new();
    };
    let Some(resume_spec) = tool_enum.spec().resume.as_ref() else {
        return Vec::new();
    };

    let mut args = match resume_spec.resume {
        ResumeArgs::Flag(flag) => vec![flag.to_string(), session_id.to_string()],
        ResumeArgs::Subcommand(sub) => vec![sub.to_string(), session_id.to_string()],
    };

    if fork {
        match resume_spec.fork {
            Some(ForkArgs::AppendFlag(flag)) => args.push(flag.to_string()),
            Some(ForkArgs::Subcommand(sub)) => {
                // Replace the resume subcommand with the fork subcommand.
                if let Some(first) = args.first_mut() {
                    *first = sub.to_string();
                }
            }
            None => {}
        }
    }

    args
}

/// Merge original launch args with resume-specific args.
fn merge_resume_args(tool: &str, original: &[String], resume: &[String]) -> Vec<String> {
    // Claude/Gemini/Codex stay grammar-free: preserve the stored user/config
    // vector verbatim and append hcom's resume injection.
    let tool_lookup = if tool == "claude-pty" { "claude" } else { tool };
    let tool = tool_lookup
        .parse::<crate::tool::Tool>()
        .expect("resume tool must be validated before argument merging");

    match tool {
        crate::tool::Tool::Claude | crate::tool::Tool::Gemini | crate::tool::Tool::Codex => {
            let mut merged = original.to_vec();
            merged.extend_from_slice(resume);
            merged
        }
        crate::tool::Tool::OpenCode | crate::tool::Tool::Kilo => {
            merge_opencode_args(original, resume)
        }
        crate::tool::Tool::Antigravity => merge_antigravity_args(original, resume),
        crate::tool::Tool::Cursor => merge_cursor_args(original, resume),
        crate::tool::Tool::Kimi => merge_kimi_args(original, resume),
        crate::tool::Tool::Copilot => merge_copilot_args(original, resume),
        crate::tool::Tool::Pi => merge_pi_args(original, resume),
        crate::tool::Tool::Omp => merge_omp_args(original, resume),
        crate::tool::Tool::Adhoc => {
            unreachable!("Adhoc sessions do not support resume argument merging")
        }
    }
}

/// Merge copilot original launch args with resume args.
///
/// copilot launch_args bake in `HCOM_COPILOT_ARGS` (e.g. `--model
/// claude-haiku-4.5`) plus the `-i <initial-prompt>` from the launcher.
/// On resume: drop `-i`/`--interactive` and its value, drop `--resume`
/// and its value; keep everything else (model flags, `--allow-*`, etc.).
/// Resume args take precedence for overlapping singular flags.
fn merge_copilot_args(original: &[String], resume: &[String]) -> Vec<String> {
    const VALUE_FLAGS: &[&str] = &["--model", "--name", "--add-dir", "--agent"];
    const DROP_WITH_VALUE: &[&str] = &["--resume", "-i", "--interactive"];
    const DROP_BOOLEAN: &[&str] = &["--allow-all-tools", "--allow-all", "--yolo"];

    let is_flag = |t: &str| t.starts_with('-');

    // Singular flags already present in resume args — original copies are dropped.
    let mut resume_flags: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut skip_next = false;
    for token in resume {
        if skip_next {
            skip_next = false;
            continue;
        }
        if is_flag(token) {
            let lower = token.to_lowercase();
            let bare = lower.split('=').next().unwrap_or(&lower).to_string();
            if VALUE_FLAGS.contains(&bare.as_str()) {
                skip_next = true;
            }
            if !DROP_WITH_VALUE.contains(&bare.as_str()) {
                resume_flags.insert(bare);
            }
        }
    }

    // Filter the original args.
    let mut filtered_original: Vec<String> = Vec::new();
    let mut i = 0;
    while i < original.len() {
        let token = &original[i];
        if is_flag(token) {
            let lower = token.to_lowercase();
            let (bare, has_eq_value) = if let Some(pos) = lower.find('=') {
                (lower[..pos].to_string(), true)
            } else {
                (lower.clone(), false)
            };
            if DROP_WITH_VALUE.contains(&bare.as_str()) {
                i += 1;
                if !has_eq_value && i < original.len() && !is_flag(&original[i]) {
                    i += 1;
                }
                continue;
            }
            if DROP_BOOLEAN.contains(&bare.as_str()) {
                i += 1;
                continue;
            }
            if resume_flags.contains(&bare) {
                i += 1;
                if !has_eq_value && VALUE_FLAGS.contains(&bare.as_str()) && i < original.len() {
                    i += 1;
                }
                continue;
            }
            filtered_original.push(token.clone());
            i += 1;
            if !has_eq_value && VALUE_FLAGS.contains(&bare.as_str()) && i < original.len() {
                filtered_original.push(original[i].clone());
                i += 1;
            }
        } else {
            // Skip bare positional values (e.g. the `-i` prompt that was split off).
            i += 1;
        }
    }

    let mut result = resume.to_vec();
    result.extend(filtered_original);
    result
}

/// Merge cursor-agent original launch args with resume args.
///
/// cursor's launch_args bake in `HCOM_CURSOR_ARGS` (e.g. `--model composer-2.5
/// --force`) plus the trailing positional task prompt that the launcher appends
/// (`launcher.rs` Positional shape). On resume we must:
///   - preserve user config flags (`--model`, `--force`/`--yolo`, `--sandbox`,
///     `--mode`/`--plan`, `--output-format`, `--api-key`, `-H`/`--header`,
///     `--plugin-dir`, …) so the resumed agent keeps its model/permissions;
///   - drop the stale positional prompt — re-submitting the original task on a
///     resume would re-run it;
///   - strip flags hcom owns at relaunch: prior session selectors
///     (`--resume`/`--continue`) and cwd/worktree selectors
///     (`--workspace`, `-w`/`--worktree`, `--worktree-base`,
///     `--skip-worktree-setup`) — the launcher sets the working directory
///     itself (snapshot dir or `--dir`), so a stale `--workspace` would fight
///     the recovered cwd and `--worktree` would spawn a *new* worktree.
///
/// Resume args (`--resume <session_id>` + any user-supplied extra args) come
/// first; preserved original flags follow. For **singular** flags that the
/// resume args already specify (e.g. a resume-time `--model x`), the baked
/// original copy is dropped so the resume value wins under commander.js
/// last-wins — matching claude/codex/gemini precedence. **Repeatable** flags
/// (`-H`/`--header`, `--plugin-dir`) are always kept and concatenate.
fn merge_cursor_args(original: &[String], resume: &[String]) -> Vec<String> {
    // Flags whose following token is a value (so it isn't mistaken for the
    // positional prompt). `--resume`/`--worktree` also accept a value but are
    // stripped, so they're handled by DROP_WITH_VALUE below. The repeatable
    // header short flag `-H` is handled separately (case-sensitive) because
    // lowercasing collides with the `-h` help flag.
    const VALUE_FLAGS: &[&str] = &[
        "--api-key",
        "--header",
        "--output-format",
        "--mode",
        "--model",
        "--sandbox",
        "--plugin-dir",
    ];
    // Strip these flags (and their value/optional-value token when split form).
    const DROP_WITH_VALUE: &[&str] = &[
        "--resume",
        "--workspace",
        "--worktree",
        "-w",
        "--worktree-base",
    ];
    const DROP_BOOLEAN: &[&str] = &["--continue", "--skip-worktree-setup"];
    // Repeatable flags concatenate across resume + original, so they're never
    // deduped (long names lowercased; `-H` matched case-sensitively below).
    const REPEATABLE: &[&str] = &["--header", "--plugin-dir"];

    let is_flag = |t: &str| t.starts_with('-');
    let header_flags = ["-H"]; // cursor's repeatable header short flag takes a value
    let is_repeatable = |token: &str, bare: &str| REPEATABLE.contains(&bare) || token == "-H";

    // Singular flag base-names already present in the resume args — the baked
    // original copy of any of these is dropped so resume wins (consistency with
    // the other tools' resume-precedence). Repeatables are excluded.
    let mut resume_singular_flags = std::collections::HashSet::new();
    for token in resume {
        if !is_flag(token) {
            continue;
        }
        let lower = token.to_lowercase();
        let bare = lower.split('=').next().unwrap_or(&lower).to_string();
        if is_repeatable(token, &bare) {
            continue;
        }
        resume_singular_flags.insert(bare);
    }

    let mut preserved = Vec::new();
    let mut i = 0;
    while i < original.len() {
        let token = &original[i];
        let lower = token.to_lowercase();
        let bare = lower.split('=').next().unwrap_or(&lower);

        if DROP_BOOLEAN.contains(&bare) {
            i += 1;
            continue;
        }
        if DROP_WITH_VALUE.contains(&bare) {
            // `--flag=value`: single token. Bare `--flag`: consume an optional
            // following value only when it isn't itself a flag.
            if !token.contains('=') && i + 1 < original.len() && !is_flag(&original[i + 1]) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Resume already specifies this singular flag → drop the baked dup
        // (and its value token, when split form) so resume wins.
        let deduped = !is_repeatable(token, bare) && resume_singular_flags.contains(bare);
        let takes_value = VALUE_FLAGS.contains(&bare) || header_flags.contains(&token.as_str());
        if takes_value {
            if !deduped {
                preserved.push(token.clone());
            }
            if !token.contains('=') && i + 1 < original.len() {
                if !deduped {
                    preserved.push(original[i + 1].clone());
                }
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if is_flag(token) {
            // Unknown/boolean config flag (e.g. --force, --yolo, --plan,
            // --approve-mcps, --trust): preserve unless resume overrides it.
            if !deduped {
                preserved.push(token.clone());
            }
            i += 1;
            continue;
        }
        // Bare positional = the stale task prompt. Drop it.
        i += 1;
    }

    let mut merged = resume.to_vec();
    merged.extend(preserved);
    // The unified launcher rejects cursor print flags with a clear error. Keep
    // them visible here so a stale baked flag cannot silently change meaning.
    merged
}

/// Merge agy original args with resume args.
///
/// Strips prior session/prompt flags (--conversation, --continue/-c,
/// --prompt/--prompt-interactive/-p/-i, --print) from the original to prevent
/// conflicting session IDs or prompts from the last launch bleeding into the resume.
/// Non-session flags (--sandbox, --add-dir, etc.) are preserved.
///
/// agy uses Go's `flag` package, which accepts `--flag value`, `--flag=value`,
/// `-flag value`, and `-flag=value`. All four forms are stripped.
fn merge_antigravity_args(original: &[String], resume: &[String]) -> Vec<String> {
    // Flag names without leading dashes — match against any leading-dash form.
    let session_flag_names: &[&str] = &[
        "conversation",
        "continue",
        "c",
        "prompt",
        "prompt-interactive",
        "i",
        "print",
        "p",
    ];
    // Flag names that consume the next token as their value when used in split form.
    let value_flag_names: &[&str] = &["conversation", "prompt", "prompt-interactive"];

    // Returns the bare flag name (e.g. "conversation") if `token` matches `--name`,
    // `-name`, `--name=value`, or `-name=value` for some name in `names`.
    fn match_flag(token: &str, names: &[&str]) -> Option<String> {
        let trimmed = token
            .strip_prefix("--")
            .or_else(|| token.strip_prefix('-'))?;
        let name = trimmed.split('=').next().unwrap_or(trimmed);
        if names.contains(&name) {
            Some(name.to_string())
        } else {
            None
        }
    }

    let mut preserved = Vec::new();
    let mut i = 0;
    while i < original.len() {
        let token = &original[i];
        if let Some(name) = match_flag(token, session_flag_names) {
            // `--flag=value` or `-flag=value`: one token, no following value.
            if token.contains('=') {
                i += 1;
                continue;
            }
            // Bare flag: consume the next token only if this flag takes a value.
            if value_flag_names.contains(&name.as_str()) && i + 1 < original.len() {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        preserved.push(token.clone());
        i += 1;
    }

    let mut merged = resume.to_vec();
    merged.extend(preserved);
    merged
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

fn merge_kimi_args(original: &[String], resume: &[String]) -> Vec<String> {
    let mut preserved = Vec::new();
    let mut i = 0;

    while i < original.len() {
        let token = &original[i];

        if token == "--resume" || token == "--fork" {
            i += 1;
            continue;
        }
        if token.starts_with("--resume=") || token.starts_with("--fork=") {
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

/// Merge pi original launch args with resume args.
///
/// Strips session-control flags (`--session`/`--session-id`/`--session-dir`/
/// `--fork`/`--continue`/`--resume`) and the positional initial prompt from the
/// original launch args; preserves the rest (model flags, etc.). Resume args
/// take precedence (prepended).
fn merge_pi_args(original: &[String], resume: &[String]) -> Vec<String> {
    let mut preserved = Vec::new();
    let mut i = 0;

    while i < original.len() {
        let token = &original[i];
        let token_str = token.as_str();

        if matches!(
            token_str,
            "--session" | "--session-id" | "--session-dir" | "--fork"
        ) {
            i += 2;
            continue;
        }
        if matches!(token_str, "--continue" | "--resume")
            || token_str.starts_with("--session=")
            || token_str.starts_with("--session-id=")
            || token_str.starts_with("--session-dir=")
            || token_str.starts_with("--fork=")
        {
            i += 1;
            continue;
        }

        if !token_str.starts_with('-') {
            // Pi initial prompts are positional; do not replay an old prompt
            // when resuming or forking a session.
            i += 1;
            continue;
        }

        preserved.push(token.clone());
        if i + 1 < original.len() && !original[i + 1].starts_with('-') {
            preserved.push(original[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
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
    let projects_dir = claude_projects_dir();
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
            if let Some((title, session_id)) = last_title
                && title == name
            {
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
/// Claude, Codex, and Gemini use UUIDs. OpenCode, Kilo, Pi, and OMP use specific prefixes or directories.
fn is_session_id(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok() || is_opencode_session_id(s)
}

/// Opencode session IDs are `ses_` followed by 26 hex+base62 chars
/// (see opencode/packages/opencode/src/id/id.ts).
fn is_opencode_session_id(s: &str) -> bool {
    s.starts_with("ses_") && s.len() >= 8 && s[4..].chars().all(|c| c.is_ascii_alphanumeric())
}

fn opencode_data_dir() -> Option<std::path::PathBuf> {
    crate::runtime_env::opencode_family_data_dir("opencode")
}

fn opencode_family_db_path(tool: &str) -> Option<std::path::PathBuf> {
    crate::runtime_env::opencode_family_db_path(tool)
}

/// Query an OpenCode-family SQLite DB for a session's working directory.
/// Returns (exists, cwd). `exists=true, cwd=None` is impossible given the
/// schema (directory is NOT NULL), so `cwd=None` implies the row is absent.
fn lookup_opencode_family_session(tool: &str, session_id: &str) -> Option<String> {
    let db_path = opencode_family_db_path(tool)?;
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

fn lookup_family_session(session_id: &str) -> Option<(String, String)> {
    ["opencode", "kilo"].into_iter().find_map(|tool| {
        lookup_opencode_family_session(tool, session_id).map(|cwd| (tool.to_string(), cwd))
    })
}

/// Resolve a session ID to the owning tool and (optionally) a pre-recovered
/// working directory.
///
/// - Claude, Codex, Gemini: returns `(tool, Some(transcript_path))`.
///   Caller reads the transcript's first line to recover CWD.
/// - Opencode: returns `(tool="opencode", None)` — opencode stores sessions
///   in SQLite; CWD comes from a separate DB query, not a transcript file.
fn find_session_on_disk(session_id: &str) -> Option<(String, Option<String>)> {
    // 1. OpenCode family: prefix-scoped, query the SQLite DBs directly.
    if is_opencode_session_id(session_id) {
        if let Some((tool, _)) = lookup_family_session(session_id) {
            return Some((tool, None));
        }
        return None;
    }

    // 2. Claude: iterate project dirs, check for exact filename
    let projects_dir = claude_projects_dir();
    if projects_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&projects_dir)
    {
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

    // 5. Cursor
    if let Some(path) = derive_cursor_transcript_path(session_id) {
        let tool = detect_agent_type(&path).to_string();
        return Some((tool, Some(path)));
    }

    // 6. Kimi
    if let Some(path) = derive_kimi_transcript_path(session_id) {
        let tool = detect_agent_type(&path).to_string();
        return Some((tool, Some(path)));
    }

    // 7. Copilot
    if let Some(path) = derive_copilot_transcript_path(session_id) {
        let tool = detect_agent_type(&path).to_string();
        return Some((tool, Some(path)));
    }

    // 8. Pi / OMP. Attribute by root PROVENANCE, not probe order:
    //   - `PI_CODING_AGENT_SESSION_DIR` + `~/.pi`            => Pi-exclusive
    //   - XDG-omp / `~/.omp` / OMP profile trees             => OMP-exclusive
    //   - `PI_CODING_AGENT_DIR/sessions`                     => genuinely shared
    // Exclusive roots are authoritative. The shared root is attributed only by
    // the path's product marker (managed configs carry `.pi`/`.omp`); a
    // markerless arbitrary shared dir is genuinely ambiguous (both tools write
    // identical headers) and is surfaced as an error by the caller rather than
    // guessed. `PiOmpMatch::Ambiguous` carries the path so the caller can name it.
    match resolve_pi_omp_on_disk(session_id) {
        PiOmpMatch::Found { tool, path } => return Some((tool, Some(path))),
        // Ambiguous is handled by build_adopt_plan (which re-derives via
        // `ambiguous_pi_omp_session`); fall through to the not-found path here so
        // untracked callers still get a definite answer.
        PiOmpMatch::Ambiguous { .. } | PiOmpMatch::None => {}
    }

    None
}

enum PiOmpMatch {
    Found { tool: String, path: String },
    Ambiguous { path: String },
    None,
}

/// Locate a Pi/OMP session id on disk and attribute it by root ownership.
fn resolve_pi_omp_on_disk(session_id: &str) -> PiOmpMatch {
    use crate::transcript::{
        omp_exclusive_roots, omp_session_roots, pi_exclusive_roots, pi_session_roots,
        shared_agent_dir_root,
    };

    let shared = shared_agent_dir_root();
    let shared_owner = shared.as_ref().and_then(|root| {
        let pi_active = pi_session_roots().contains(root);
        let omp_active = omp_session_roots().contains(root);
        match (pi_active, omp_active) {
            (true, true) => Some(None),
            (true, false) => Some(Some("pi")),
            (false, true) => Some(Some("omp")),
            (false, false) => None,
        }
    });

    let roots = pi_exclusive_roots()
        .into_iter()
        .map(|root| (root, Some("pi")))
        .chain(
            omp_exclusive_roots()
                .into_iter()
                .map(|root| (root, Some("omp"))),
        )
        .chain(shared.zip(shared_owner));

    for (root, owner) in roots {
        if root.exists()
            && let Some(path) = find_pi_transcript_in_root(&root, session_id)
        {
            return match owner.or_else(|| pi_omp_path_owner(&path)) {
                Some(tool) => PiOmpMatch::Found {
                    tool: tool.to_string(),
                    path,
                },
                None => PiOmpMatch::Ambiguous { path },
            };
        }
    }
    PiOmpMatch::None
}

/// Return the owner encoded in a shared path's `.pi` or `.omp` marker.
fn pi_omp_path_owner(path: &str) -> Option<&'static str> {
    match crate::transcript::detect_tool_from_path(path) {
        Some(crate::tool::Tool::Pi) => Some("pi"),
        Some(crate::tool::Tool::Omp) => Some("omp"),
        _ => None,
    }
}

/// The path of a Pi/OMP session that was found but is genuinely ambiguous
/// (markerless shared `PI_CODING_AGENT_DIR`). Used by the adoption path to raise
/// a precise disambiguation error instead of "not found".
fn ambiguous_pi_omp_session(session_id: &str) -> Option<String> {
    match resolve_pi_omp_on_disk(session_id) {
        PiOmpMatch::Ambiguous { path } => Some(path),
        _ => None,
    }
}

/// Merge `omp` transcript arguments for a resumed or forked session.
///
/// Omp uses `--resume <id>` and `--fork <id>`, and lacks `--session`.
fn merge_omp_args(original: &[String], resume: &[String]) -> Vec<String> {
    let mut preserved = Vec::new();
    let mut i = 0;

    while i < original.len() {
        let token = &original[i];
        let token_str = token.as_str();

        if matches!(
            token_str,
            "-r" | "--resume" | "--session" | "--session-id" | "--session-dir" | "--fork"
        ) {
            i += 1;
            if i < original.len() && !original[i].starts_with('-') {
                i += 1;
            }
            continue;
        }
        if matches!(token_str, "-c" | "--continue")
            || token_str.starts_with("--resume=")
            || token_str.starts_with("--session=")
            || token_str.starts_with("--session-id=")
            || token_str.starts_with("--session-dir=")
            || token_str.starts_with("--fork=")
        {
            i += 1;
            continue;
        }

        if !token_str.starts_with('-') {
            i += 1;
            continue;
        }

        preserved.push(token.clone());
        if i + 1 < original.len() && !original[i + 1].starts_with('-') {
            preserved.push(original[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }

    let mut final_args = resume.to_vec();
    final_args.extend(preserved);
    final_args
}

/// Locate an OMP transcript by session id under OMP's currently active session
/// root. Notably does **not** search `PI_CODING_AGENT_SESSION_DIR` — OMP never
/// reads it, so it stays Pi-exclusive. Test helper: production attribution goes through
/// [`resolve_pi_omp_on_disk`], which keeps exclusive/shared provenance.
#[cfg(test)]
fn derive_omp_transcript_path(session_id: &str) -> Option<String> {
    for root in crate::transcript::omp_session_roots() {
        if !root.exists() {
            continue;
        }
        if let Some(path) = find_pi_transcript_in_root(&root, session_id) {
            return Some(path);
        }
    }
    None
}

fn find_pi_transcript_in_root(root: &std::path::Path, session_id: &str) -> Option<String> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_pi_transcript_in_root(&path, session_id) {
                return Some(found);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.contains(session_id))
        {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

/// Locate a cursor-agent transcript by conversation UUID (== hcom session_id).
/// cursor writes it at `~/.cursor/projects/<slug>/agent-transcripts/<uuid>/
/// <uuid>.jsonl` — the same path the hook reports. The `<slug>` can't be
/// derived from the UUID, so scan the per-project dirs for the nested file.
/// (The flat `projects/agent-transcripts/<uuid>.jsonl` mirror is skipped: it
/// has no sibling `.workspace-trusted`, so no cwd could be recovered from it.)
fn derive_cursor_transcript_path(session_id: &str) -> Option<String> {
    let projects = dirs::home_dir()?.join(".cursor").join("projects");
    let file = format!("{session_id}.jsonl");
    for entry in std::fs::read_dir(&projects).ok()?.flatten() {
        let candidate = entry
            .path()
            .join("agent-transcripts")
            .join(session_id)
            .join(&file);
        if candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

/// Locate a Copilot CLI transcript by session UUID.
/// Copilot stores transcripts at `$COPILOT_HOME/session-state/<uuid>/events.jsonl`
/// where `COPILOT_HOME` defaults to `~/.copilot`.
fn derive_copilot_transcript_path(session_id: &str) -> Option<String> {
    let copilot_home = if let Ok(dir) = std::env::var("COPILOT_HOME")
        && !dir.is_empty()
    {
        std::path::PathBuf::from(dir)
    } else {
        dirs::home_dir()?.join(".copilot")
    };
    let candidate = copilot_home
        .join("session-state")
        .join(session_id)
        .join("events.jsonl");
    if candidate.exists() {
        Some(candidate.to_string_lossy().to_string())
    } else {
        None
    }
}

/// cursor transcripts carry no `cwd`, so recover it from the per-workspace
/// `.workspace-trusted` marker cursor writes at
/// `~/.cursor/projects/<slug>/.workspace-trusted` (`workspacePath` field). The
/// transcript lives under `<slug>/agent-transcripts/<uuid>/<uuid>.jsonl`, so
/// walk up to the project-slug dir (the direct child of `projects/`) and read
/// it. Returns the recorded (canonicalized) workspace path.
fn recover_cursor_cwd(transcript_path: &str) -> Option<String> {
    let path = std::path::Path::new(transcript_path);
    let mut slug_dir = None;
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        if dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("projects")
        {
            slug_dir = Some(dir);
            break;
        }
        cursor = dir.parent();
    }
    let marker = slug_dir?.join(".workspace-trusted");
    let data = std::fs::read_to_string(&marker).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
    parsed
        .get("workspacePath")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
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
/// - **Cursor**: the transcript has no `cwd`; recover it from the sibling
///   `~/.cursor/projects/<slug>/.workspace-trusted` marker (`workspacePath`).
/// - **Copilot**: `session.start` entry carries `cwd` in the `data` object;
///   scan the first few lines for it.
fn extract_cwd_from_transcript(path: &str, tool: &str) -> Option<String> {
    match tool {
        "claude" => scan_lines_for_cwd(path, 20, |v| v.get("cwd").and_then(|c| c.as_str())),
        "codex" => scan_lines_for_cwd(path, 1, |v| {
            v.get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
        }),
        // Antigravity shares Gemini's session tree/format (GEMINI_CLI_HOME);
        // an agy-pathed transcript that detect_agent_type labels "antigravity"
        // still has the gemini `projectHash` field, so reuse the same recovery.
        "gemini" | "antigravity" => recover_gemini_cwd(path),
        "cursor" => recover_cursor_cwd(path),
        "kimi" => None, // Kimi context.jsonl does not store cwd
        "copilot" => scan_lines_for_cwd(path, 20, |v| {
            v.get("event")
                .or_else(|| v.get("type"))
                .and_then(|event| event.as_str())
                .filter(|event| *event == "session.start")
                .and_then(|_| v.get("data"))
                .and_then(|data| data.get("cwd"))
                .and_then(|cwd| cwd.as_str())
        }),
        "omp" => scan_lines_for_cwd(path, 10, |v| {
            if v.get("type").and_then(|t| t.as_str()) == Some("session") {
                v.get("cwd")
                    .or_else(|| v.get("session").and_then(|s| s.get("cwd")))
                    .and_then(|c| c.as_str())
            } else {
                None
            }
        }),
        "pi" => scan_lines_for_cwd(path, 10, |v| {
            if v.get("type").and_then(|t| t.as_str()) == Some("session") {
                v.get("cwd")
                    .or_else(|| v.get("session").and_then(|s| s.get("cwd")))
                    .and_then(|c| c.as_str())
            } else {
                None
            }
        }),
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
        if let Some(cwd) = pick(&parsed)
            && !cwd.is_empty()
        {
            return Some(cwd.to_string());
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
    // A markerless session under a shared PI_CODING_AGENT_DIR cannot be
    // attributed to Pi vs OMP without guessing; surface it explicitly rather
    // than launching the wrong tool against the transcript.
    if let Some(path) = ambiguous_pi_omp_session(session_id) {
        bail!(
            "Session {session_id} was found under a shared PI_CODING_AGENT_DIR \
             ({path}) with no product marker, so hcom cannot tell whether it is a \
             Pi or Oh My Pi session (both write identical transcripts).\n\
             Disambiguate by launching the owning tool explicitly, e.g.\n  \
             hcom pi --resume {session_id}\n  hcom omp --resume {session_id}"
        );
    }

    let (tool, transcript_path) = find_session_on_disk(session_id).ok_or_else(|| {
        // find_session_on_disk short-circuits for `ses_` IDs (the OpenCode
        // family is searched), so scope the error to match that lookup.
        let opencode_db = opencode_data_dir()
            .map(|d| d.join("opencode.db").display().to_string())
            .unwrap_or_else(|| "(no data dir)".to_string());
        let kilo_db = opencode_family_db_path("kilo")
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(no data dir)".to_string());
        if is_opencode_session_id(session_id) {
            anyhow::anyhow!(
                "Session {sid} not found. Searched:\n  \
                 - Opencode: {opencode_db} (table 'session')\n  \
                 - Kilo:     {kilo_db} (table 'session')",
                sid = session_id,
                opencode_db = opencode_db,
                kilo_db = kilo_db,
            )
        } else {
            let claude_projects = claude_projects_dir();
            anyhow::anyhow!(
                "Session {sid} not found. Searched:\n  \
                 - Claude:   {claude}/*/{sid}.jsonl\n  \
                 - Codex:    ~/.codex/sessions/**/*-{sid}.jsonl\n  \
                 - Gemini:   ~/.gemini/tmp/*/chats/session-*-{short}*.json\n  \
                 - Cursor:   ~/.cursor/projects/*/agent-transcripts/{sid}/{sid}.jsonl\n  \
                 - Copilot:  ~/.copilot/session-state/{sid}/events.jsonl\n  \
                 - Oh My Pi: ~/.omp/agent/sessions/**/*{sid}*.jsonl
                 - Pi:       ~/.pi/agent/sessions/**/*{sid}*.jsonl",
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
        let raw = if tool == "opencode" || tool == "kilo" {
            lookup_opencode_family_session(&tool, session_id)
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
    fn test_recover_cursor_cwd_from_workspace_trusted() {
        let dir = tempfile::tempdir().unwrap();
        // Mirror ~/.cursor/projects/<slug>/{agent-transcripts/<uuid>/<uuid>.jsonl,.workspace-trusted}
        let slug = dir.path().join("projects").join("Users-anno-Dev-x");
        let tdir = slug.join("agent-transcripts").join("uuid-1");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(
            slug.join(".workspace-trusted"),
            json!({"workspacePath": "/Users/anno/Dev/x", "trustMethod": null}).to_string(),
        )
        .unwrap();
        let transcript = tdir.join("uuid-1.jsonl");
        std::fs::write(&transcript, "{}").unwrap();

        assert_eq!(
            recover_cursor_cwd(&transcript.to_string_lossy()),
            Some("/Users/anno/Dev/x".to_string())
        );
        // No marker → None (graceful: caller falls back to $PWD).
        std::fs::remove_file(slug.join(".workspace-trusted")).unwrap();
        assert_eq!(recover_cursor_cwd(&transcript.to_string_lossy()), None);
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
    fn test_build_resume_args_omp_resume() {
        let args = build_resume_args("omp", "sess-omp", false);
        assert_eq!(args, s(&["--resume", "sess-omp"]));
    }

    #[test]
    fn test_build_resume_args_omp_fork() {
        // Top-level `omp --help` omits `--fork`, but v15.1.9+ implements it:
        // `omp --fork <id>` takes the id as its value and routes through
        // SessionManager.forkFrom(...). Same shape as Pi — fork must emit
        // `["--fork", <id>]`, replacing `--resume`, not `["--resume", <id>,
        // "--fork"]`. hcom must not degrade `hcom f` into a plain `--resume`.
        let args = build_resume_args("omp", "sess-omp", true);
        assert_eq!(args, s(&["--fork", "sess-omp"]));
    }

    #[test]
    fn test_merge_omp_args_fork_replaces_prior_session_controls() {
        // A fork's build_resume_args output (`--fork <id>`) merged over stored
        // launch args must strip the old resume/session/fork controls and keep
        // config flags.
        let fork = s(&["--fork", "new"]);
        assert_eq!(
            merge_omp_args(&s(&["--model", "opus", "--fork", "old"]), &fork),
            s(&["--fork", "new", "--model", "opus"])
        );
        assert_eq!(
            merge_omp_args(&s(&["--fork=old", "-r", "prev"]), &fork),
            s(&["--fork", "new"])
        );
    }

    #[test]
    fn test_merge_omp_args_strips_resume_aliases_and_values() {
        let resume = s(&["--resume", "new"]);
        assert_eq!(
            merge_omp_args(&s(&["--model", "opus", "-r", "old"]), &resume),
            s(&["--resume", "new", "--model", "opus"])
        );
        assert_eq!(
            merge_omp_args(&s(&["--resume=old", "--thinking", "high"]), &resume),
            s(&["--resume", "new", "--thinking", "high"])
        );
        assert_eq!(
            merge_omp_args(
                &s(&["--continue", "-c", "--session-dir", "/tmp/s"]),
                &resume
            ),
            s(&["--resume", "new"])
        );
    }

    #[test]
    fn test_merge_omp_args_does_not_drop_following_flags() {
        let resume = s(&["--resume", "new"]);
        assert_eq!(
            merge_omp_args(&s(&["--session-dir", "--model", "opus"]), &resume),
            s(&["--resume", "new", "--model", "opus"])
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_derive_omp_transcript_path_checks_xdg_data_home() {
        if !cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "android"
        )) {
            return;
        }
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("omp").join("sessions").join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("session-xyz.jsonl"), "{}").unwrap();
        unsafe {
            std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
            std::env::set_var("XDG_DATA_HOME", dir.path());
        }

        let path = derive_omp_transcript_path("session-xyz").unwrap();
        assert!(
            path.contains("session-xyz.jsonl"),
            "unexpected path: {path}"
        );
    }

    // Unix-only: relies on redirecting the home dir via `isolated_test_env`'s
    // $HOME, but on Windows `dirs::home_dir()` queries the OS profile folder
    // directly and ignores it.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn test_find_session_on_disk_prefers_omp_for_omp_paths() {
        let (_dir, _hcom, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        unsafe {
            std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
        }
        let root = home
            .join(".omp")
            .join("agent")
            .join("sessions")
            .join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("session-omp.jsonl"), "{}").unwrap();

        let found = find_session_on_disk("session-omp").unwrap();
        assert_eq!(found.0, "omp");
    }

    #[test]
    #[serial_test::serial]
    fn test_find_session_on_disk_attributes_pi_session_dir_to_pi() {
        // Regression guard for the shared-root bug: a Pi session reached via the
        // Pi-exclusive PI_CODING_AGENT_SESSION_DIR override must be attributed to
        // Pi, not stolen by OMP (which never reads that variable).
        let (_dir, _hcom, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let pi_sessions = tempfile::tempdir().unwrap();
        let root = pi_sessions.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("session-pi.jsonl"), "{}").unwrap();
        unsafe {
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("HCOM_TOOL");
            std::env::set_var("PI_CODING_AGENT_SESSION_DIR", pi_sessions.path());
        }

        // OMP must not find it at all.
        assert!(derive_omp_transcript_path("session-pi").is_none());
        let found = find_session_on_disk("session-pi").unwrap();
        assert_eq!(found.0, "pi");
    }

    #[test]
    #[serial_test::serial]
    fn test_find_session_on_disk_attributes_shared_agent_dir_by_path_marker() {
        // The genuinely-shared PI_CODING_AGENT_DIR: attribution must key on the
        // path's product marker (.pi vs .omp), not on which tool's root list or
        // probe order found it. hcom isolates managed configs as <root>/.pi and
        // <root>/.omp, so the marker is present.
        let (_dir, _hcom, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let base = tempfile::tempdir().unwrap();
        unsafe {
            std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
            std::env::remove_var("HCOM_TOOL");
        }

        // Managed Pi: PI_CODING_AGENT_DIR=<base>/.pi -> sessions/<file>.
        let pi_dir = base.path().join(".pi");
        let pi_root = pi_dir.join("sessions").join("proj");
        std::fs::create_dir_all(&pi_root).unwrap();
        std::fs::write(pi_root.join("mgpi.jsonl"), "{}").unwrap();
        unsafe { std::env::set_var("PI_CODING_AGENT_DIR", &pi_dir) }
        assert_eq!(find_session_on_disk("mgpi").unwrap().0, "pi");

        // Managed OMP: PI_CODING_AGENT_DIR=<base>/.omp -> sessions/<file>.
        let omp_dir = base.path().join(".omp");
        let omp_root = omp_dir.join("sessions").join("proj");
        std::fs::create_dir_all(&omp_root).unwrap();
        std::fs::write(omp_root.join("mgomp.jsonl"), "{}").unwrap();
        unsafe { std::env::set_var("PI_CODING_AGENT_DIR", &omp_dir) }
        assert_eq!(find_session_on_disk("mgomp").unwrap().0, "omp");
    }

    #[test]
    #[serial_test::serial]
    fn test_markerless_shared_agent_dir_is_ambiguous_not_guessed() {
        // Arbitrary shared PI_CODING_AGENT_DIR with no .pi/.omp marker must be
        // reported ambiguous, never silently attributed — even when the caller
        // itself is Pi or OMP.
        let (_dir, _hcom, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let shared = tempfile::tempdir().unwrap();
        let root = shared.path().join("sessions").join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("amb.jsonl"), "{}").unwrap();
        unsafe {
            std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
            std::env::remove_var("HCOM_TOOL");
            std::env::set_var("PI_CODING_AGENT_DIR", shared.path());
        }

        // No determinate attribution -> find_session_on_disk yields None, and the
        // match is flagged ambiguous (found-but-unattributable).
        assert!(find_session_on_disk("amb").is_none());
        assert!(ambiguous_pi_omp_session("amb").is_some());

        for caller in ["pi", "omp"] {
            unsafe { std::env::set_var("HCOM_TOOL", caller) }
            assert!(
                find_session_on_disk("amb").is_none(),
                "caller {caller} must not claim a markerless shared transcript"
            );
            assert!(ambiguous_pi_omp_session("amb").is_some());
        }
    }

    // Unix-only: relies on redirecting the home dir via `isolated_test_env`'s
    // $HOME, but on Windows `dirs::home_dir()` queries the OS profile folder
    // directly and ignores it.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn test_derive_omp_transcript_path_checks_named_profile() {
        let (_dir, _hcom, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        unsafe {
            std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("PI_PROFILE");
            std::env::set_var("OMP_PROFILE", "work");
        }
        let root = home
            .join(".omp")
            .join("profiles")
            .join("work")
            .join("agent")
            .join("sessions")
            .join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("session-prof.jsonl"), "{}").unwrap();

        let path = derive_omp_transcript_path("session-prof").unwrap();
        assert!(path.contains("session-prof.jsonl"), "unexpected: {path}");
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
    fn test_validate_resume_operation_rejects_unknown_tool() {
        let err = validate_resume_operation("future-tool", false)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Unknown tool 'future-tool' in saved session metadata");
    }

    #[test]
    fn test_build_resume_args_antigravity_resume() {
        let args = build_resume_args("antigravity", "conv-abc", false);
        assert_eq!(args, s(&["--conversation", "conv-abc"]));
    }

    #[test]
    fn test_validate_resume_operation_rejects_antigravity_fork() {
        let err = validate_resume_operation("antigravity", true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Antigravity") && err.contains("fork"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_resume_operation_allows_antigravity_resume() {
        assert!(validate_resume_operation("antigravity", false).is_ok());
    }

    #[test]
    fn test_validate_resume_operation_rejects_agy_alias_fork() {
        // The alias is launcher-canonicalised today, but fork validation must
        // still reject `"agy"` so the rule lives on the spec, not on the DB
        // shape.
        let err = validate_resume_operation("agy", true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Antigravity") && err.contains("fork"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_merge_resume_args_antigravity_strips_session_and_prompt_flags() {
        // --conversation (value-consuming), --continue/-c (bare), --prompt-interactive (value), -p (bare)
        let merged = merge_resume_args(
            "antigravity",
            &s(&[
                "--conversation",
                "old-conv",
                "--sandbox",
                "--continue",
                "--prompt-interactive",
                "old prompt",
                "-p",
            ]),
            &s(&["--conversation", "new-conv"]),
        );
        assert_eq!(merged, s(&["--conversation", "new-conv", "--sandbox"]));
    }

    #[test]
    fn test_merge_resume_args_antigravity_preserves_sandbox_and_add_dir() {
        let merged = merge_resume_args(
            "antigravity",
            &s(&["--sandbox", "--add-dir", "/some/path"]),
            &s(&["--conversation", "conv-xyz"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--conversation",
                "conv-xyz",
                "--sandbox",
                "--add-dir",
                "/some/path"
            ])
        );
    }

    /// agy accepts `--flag=value` (Go flag convention). Stale `--conversation=old`
    /// in the original launch_args must not survive to conflict with the new --conversation.
    #[test]
    fn test_merge_resume_args_antigravity_strips_equals_form() {
        let merged = merge_resume_args(
            "antigravity",
            &s(&[
                "--conversation=old-id",
                "--sandbox",
                "--prompt-interactive=old prompt",
                "--prompt=alt",
            ]),
            &s(&["--conversation", "new-id"]),
        );
        assert_eq!(merged, s(&["--conversation", "new-id", "--sandbox"]));
    }

    /// agy / Go flag also accepts single-dash long form (`-conversation=old`).
    #[test]
    fn test_merge_resume_args_antigravity_strips_single_dash_long_form() {
        let merged = merge_resume_args(
            "antigravity",
            &s(&[
                "-conversation",
                "old-id",
                "-prompt-interactive=stale",
                "--sandbox",
                "-c",
            ]),
            &s(&["--conversation", "new-id"]),
        );
        assert_eq!(merged, s(&["--conversation", "new-id", "--sandbox"]));
    }

    /// cursor launch_args bake in HCOM_CURSOR_ARGS config plus a trailing
    /// positional prompt. Resume must preserve config flags, drop the stale
    /// prompt + old --resume, and prepend the new resume args.
    #[test]
    fn test_merge_resume_args_cursor_preserves_config_drops_prompt_and_session() {
        let merged = merge_resume_args(
            "cursor",
            &s(&[
                "--model",
                "composer-2.5",
                "--force",
                "--resume",
                "old-chat",
                "fix the parser bug",
            ]),
            &s(&["--resume", "new-chat-id"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--resume",
                "new-chat-id",
                "--model",
                "composer-2.5",
                "--force"
            ])
        );
    }

    /// cwd/worktree selectors are owned by the launcher (snapshot dir or --dir),
    /// so a stale --workspace / -w must be stripped and not fight the recovered
    /// cwd. --continue and the prompt are also dropped.
    #[test]
    fn test_merge_resume_args_cursor_strips_workspace_worktree_continue() {
        let merged = merge_resume_args(
            "cursor",
            &s(&[
                "--workspace",
                "/old/path",
                "--continue",
                "-w",
                "feature-x",
                "--sandbox",
                "enabled",
                "do the task",
            ]),
            &s(&["--resume", "sid"]),
        );
        assert_eq!(merged, s(&["--resume", "sid", "--sandbox", "enabled"]));
    }

    /// `--flag=value` form and the repeatable `-H` header value flag must be
    /// preserved intact; the stale `--resume=old` equals-form is stripped.
    #[test]
    fn test_merge_resume_args_cursor_equals_form_and_header() {
        let merged = merge_resume_args(
            "cursor",
            &s(&[
                "--model=sonnet-4",
                "--resume=old",
                "-H",
                "X-Trace: 1",
                "summarize",
            ]),
            &s(&["--resume", "sid"]),
        );
        assert_eq!(
            merged,
            s(&["--resume", "sid", "--model=sonnet-4", "-H", "X-Trace: 1"])
        );
    }

    /// Precedence: a resume-time `--model x` must beat the baked `--model y`.
    /// The baked singular copy is dropped (resume wins), while repeatable
    /// flags from both sides concatenate.
    #[test]
    fn test_merge_resume_args_cursor_resume_value_beats_baked() {
        let merged = merge_resume_args(
            "cursor",
            &s(&["--model", "y", "-H", "X-Baked: 1", "--force", "stale task"]),
            &s(&["--resume", "sid", "--model", "x", "-H", "X-Resume: 1"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--resume",
                "sid",
                "--model",
                "x",
                "-H",
                "X-Resume: 1",
                // baked --model y dropped (resume wins); -H kept (repeatable);
                // --force preserved.
                "-H",
                "X-Baked: 1",
                "--force",
            ])
        );
    }

    /// A baked `--print`/`-p`/`--stream-partial-output` stays visible so the
    /// launcher can reject it clearly instead of silently changing semantics.
    #[test]
    fn test_merge_resume_args_cursor_preserves_print_flags_for_validation() {
        let merged = merge_resume_args(
            "cursor",
            &s(&[
                "-p",
                "--print",
                "--stream-partial-output",
                "--model",
                "composer-2.5",
            ]),
            &s(&["--resume", "sid"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--resume",
                "sid",
                "-p",
                "--print",
                "--stream-partial-output",
                "--model",
                "composer-2.5"
            ])
        );
    }

    #[test]
    fn test_resume_inactive_agy_row_is_resumable() {
        let db = test_db();
        let mut data = serde_json::Map::new();
        data.insert("session_id".into(), json!("agy-session-001"));
        data.insert("tool".into(), json!("antigravity"));
        // Soft-finalized: instance row exists but status=inactive
        data.insert("status".into(), json!(ST_INACTIVE));
        data.insert("created_at".into(), json!(1.0));
        db.save_instance_named("zeno", &data).unwrap();

        // Emit a stopped life event so load_stopped_snapshot can find the snapshot
        let snapshot = serde_json::json!({
            "action": "stopped",
            "snapshot": {
                "tool": "antigravity",
                "session_id": "agy-session-001",
                "launch_args": "[]",
                "tag": "",
                "background": 0,
                "last_event_id": 0,
                "directory": "/tmp"
            }
        });
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?, 'life', 'zeno', ?)",
                rusqlite::params!["2026-01-01T00:00:00Z", snapshot.to_string()],
            )
            .unwrap();

        // Should NOT bail — inactive agy row is resumable.
        let result = prepare_resume_plan(&db, "zeno", false, &[], &GlobalFlags::default());
        assert!(
            result.is_ok(),
            "expected inactive agy row to be resumable, got: {:?}",
            result.err()
        );
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
    fn test_build_resume_args_kilo_fork() {
        let args = build_resume_args("kilo", "sess-000", true);
        assert_eq!(args, s(&["--session", "sess-000", "--fork"]));
    }

    #[test]
    fn test_build_resume_args_pi_fork() {
        // Pi's `--fork <id>` takes the id as its value and is mutually exclusive
        // with `--session` (pi errors "--fork cannot be combined with
        // --session"), so fork must emit `["--fork", <id>]`, not
        // `["--session", <id>, "--fork"]`.
        let args = build_resume_args("pi", "sess-000", true);
        assert_eq!(args, s(&["--fork", "sess-000"]));
    }

    #[test]
    fn test_merge_resume_args_pi_strips_session_controls_and_positional_prompt() {
        let merged = merge_resume_args(
            "pi",
            &s(&[
                "--model",
                "claude-3-5-sonnet",
                "--session-id",
                "old-sess",
                "--session-dir",
                "/tmp/old",
                "--continue",
                "old prompt",
            ]),
            &s(&["--session", "new-sess"]),
        );
        assert_eq!(
            merged,
            s(&["--session", "new-sess", "--model", "claude-3-5-sonnet"])
        );
    }

    #[test]
    fn test_merge_resume_args_kilo_preserves_non_session_flags() {
        let merged = merge_resume_args(
            "kilo",
            &s(&["--model", "kilo/kilo-auto/free", "--prompt", "old prompt"]),
            &s(&["--session", "new-sess"]),
        );
        assert_eq!(
            merged,
            s(&["--session", "new-sess", "--model", "kilo/kilo-auto/free"])
        );
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
    fn test_tracked_fork_plan_does_not_reserve_until_execution() {
        let db = test_db();
        let mut data = serde_json::Map::new();
        data.insert("session_id".into(), json!("session-123"));
        data.insert("tool".into(), json!("codex"));
        data.insert("status".into(), json!("listening"));
        data.insert("created_at".into(), json!(1.0));
        db.save_instance_named("luna", &data).unwrap();

        let before_count = db.iter_instances_full().unwrap().len();
        let plan = prepare_resume_plan(&db, "luna", true, &[], &GlobalFlags::default()).unwrap();
        let preview_name = plan
            .launch
            .name
            .as_ref()
            .expect("tracked fork should have preview name")
            .clone();

        assert_eq!(db.iter_instances_full().unwrap().len(), before_count);
        assert!(db.get_instance_full(&preview_name).unwrap().is_none());
        assert!(plan.tracked_fork_identity.is_some());

        let launch = prepare_launch_for_execution(&db, &plan).unwrap();
        let reserved_name = launch.name.as_ref().expect("reserved name");
        assert!(db.get_instance_full(reserved_name).unwrap().is_some());
        assert_eq!(db.iter_instances_full().unwrap().len(), before_count + 1);
        assert!(
            launch
                .initial_prompt
                .as_deref()
                .unwrap_or("")
                .contains(&format!("[hcom:{reserved_name}]"))
        );
    }

    #[test]
    fn test_resume_inherits_prior_session_id_fork_does_not() {
        let db = test_db();
        let mut data = serde_json::Map::new();
        data.insert("session_id".into(), json!("session-123"));
        data.insert("tool".into(), json!("codex"));
        data.insert("status".into(), json!(ST_INACTIVE));
        data.insert("created_at".into(), json!(1.0));
        db.save_instance_named("luna", &data).unwrap();

        // Inactive rows resolve via the stopped-snapshot life event.
        let snapshot = serde_json::json!({
            "action": "stopped",
            "snapshot": {
                "tool": "codex",
                "session_id": "session-123",
                "launch_args": "[]",
                "tag": "",
                "background": 0,
                "last_event_id": 0,
                "directory": "/tmp"
            }
        });
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?, 'life', 'luna', ?)",
                rusqlite::params!["2026-01-01T00:00:00Z", snapshot.to_string()],
            )
            .unwrap();

        let resume = prepare_resume_plan(&db, "luna", false, &[], &GlobalFlags::default()).unwrap();
        assert_eq!(
            resume.launch.prior_session_id.as_deref(),
            Some("session-123"),
            "plain resume must pre-seed the recreated row with the prior session id \
             so a kill before the first turn (no hook re-bind) stays resumable"
        );

        let fork = prepare_resume_plan(&db, "luna", true, &[], &GlobalFlags::default()).unwrap();
        assert_eq!(
            fork.launch.prior_session_id, None,
            "forks bind a fresh session on first turn; must not inherit the parent's"
        );
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
        let cfg_dir = tempfile::tempdir().unwrap();
        let cfg = cfg_dir.path();
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

        let (old, mid, cur) = with_claude_config_dir(cfg, || {
            (
                resolve_claude_thread_name("A").unwrap(),
                resolve_claude_thread_name("B").unwrap(),
                resolve_claude_thread_name("C").unwrap(),
            )
        });

        assert_eq!(old, None, "obsolete title A must not resolve");
        assert_eq!(mid, None, "obsolete title B must not resolve");
        assert_eq!(cur, Some("s1".to_string()), "current title C must resolve");
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_claude_thread_name_bails_on_within_tool_duplicate() {
        // Two distinct sessions both currently have customTitle="dup" — must
        // bail rather than silently pick by mtime.
        let cfg_dir = tempfile::tempdir().unwrap();
        let cfg = cfg_dir.path();
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

        let res = with_claude_config_dir(cfg, || resolve_claude_thread_name("dup"));

        let err = res.unwrap_err().to_string();
        assert!(err.contains("matches 2 Claude sessions"), "got: {err}");
        assert!(
            err.contains("sess-aaaa") && err.contains("sess-bbbb"),
            "got: {err}"
        );
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","cwd":"/start/dir","message":"hi"}
{"type":"assistant","cwd":"/start/dir","message":"hello"}
"#,
        )
        .unwrap();
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), "claude");
        assert_eq!(result, Some("/start/dir".to_string()));
    }

    #[test]
    fn test_extract_cwd_claude_skips_permission_mode_header() {
        // Real Claude transcripts start with a `permission-mode` line that has
        // no `cwd`; cwd first appears on a later entry. Must scan forward.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
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
    }

    #[test]
    fn test_extract_cwd_claude_gives_up_after_cap() {
        // If no cwd appears in the first 20 lines, return None rather than
        // reading the full transcript.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
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
    }

    #[test]
    fn test_extract_cwd_codex() {
        // Codex records cwd in the session_meta payload on line 1.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"cwd":"/start/dir"}}
{"type":"event_msg","payload":{"content":"hello"}}
"#,
        )
        .unwrap();
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), "codex");
        assert_eq!(result, Some("/start/dir".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn test_extract_cwd_gemini_reverse_hash_lookup() {
        // Gemini writes sha256(cwd) as `projectHash` in the session JSON, and
        // stores cwd → short-id in ~/.gemini/projects.json. This test wires up
        // a fake GEMINI_CLI_HOME and confirms the reverse lookup.
        use sha2::{Digest, Sha256};
        let base_dir = tempfile::tempdir().unwrap();
        let base = base_dir.path();
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
            std::env::set_var("GEMINI_CLI_HOME", base);
        }
        let result = extract_cwd_from_transcript(session_path.to_str().unwrap(), "gemini");
        match prev {
            Some(v) => unsafe { std::env::set_var("GEMINI_CLI_HOME", v) },
            None => unsafe { std::env::remove_var("GEMINI_CLI_HOME") },
        }

        assert_eq!(result, Some(fake_cwd.to_string()));
    }

    #[test]
    fn test_build_resume_args_copilot() {
        let args = build_resume_args("copilot", "sess-abc", false);
        assert_eq!(args, s(&["--resume", "sess-abc"]));
    }

    #[test]
    fn test_build_resume_args_copilot_fork_rejected() {
        // copilot has fork: None, so build_resume_args returns resume-only args
        let args = build_resume_args("copilot", "sess-abc", true);
        assert_eq!(args, s(&["--resume", "sess-abc"]));
    }

    #[test]
    fn test_merge_copilot_args_preserves_model_drops_prompt() {
        let original = s(&["--model", "claude-haiku-4.5", "-i", "do a task"]);
        let resume = s(&["--resume", "sess-abc"]);
        let merged = merge_resume_args("copilot", &original, &resume);
        assert!(merged.contains(&"--resume".to_string()));
        assert!(merged.contains(&"sess-abc".to_string()));
        assert!(merged.contains(&"--model".to_string()));
        assert!(merged.contains(&"claude-haiku-4.5".to_string()));
        // Original -i prompt must be dropped
        assert!(!merged.contains(&"-i".to_string()));
        assert!(!merged.contains(&"do a task".to_string()));
    }

    #[test]
    fn test_merge_copilot_args_resume_model_wins() {
        let original = s(&["--model", "claude-haiku-4.5", "-i", "task"]);
        let resume = s(&["--resume", "sess-abc", "--model", "claude-sonnet-4-5"]);
        let merged = merge_resume_args("copilot", &original, &resume);
        // Only one --model entry
        assert_eq!(merged.iter().filter(|t| t.as_str() == "--model").count(), 1);
        assert!(merged.contains(&"claude-sonnet-4-5".to_string()));
        assert!(!merged.contains(&"claude-haiku-4.5".to_string()));
    }

    #[test]
    fn test_extract_cwd_copilot_from_session_start() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"session.start","data":{"cwd":"/home/user/myproject","sessionId":"abc"}}"#,
                "\n",
                r#"{"type":"user.message","data":{"text":"hello"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let cwd = extract_cwd_from_transcript(file.path().to_str().unwrap(), "copilot");
        assert_eq!(cwd, Some("/home/user/myproject".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn test_extract_cwd_gemini_no_registry_returns_none() {
        // When projects.json is missing, we can't reverse the hash → return None.
        let base_dir = tempfile::tempdir().unwrap();
        let base = base_dir.path();
        let gemini = base.join(".gemini/tmp/x/chats");
        std::fs::create_dir_all(&gemini).unwrap();
        let path = gemini.join("test.json");
        std::fs::write(&path, r#"{"projectHash":"deadbeef"}"#).unwrap();
        let prev = std::env::var("GEMINI_CLI_HOME").ok();
        unsafe {
            std::env::set_var("GEMINI_CLI_HOME", base);
        }
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), "gemini");
        match prev {
            Some(v) => unsafe { std::env::set_var("GEMINI_CLI_HOME", v) },
            None => unsafe { std::env::remove_var("GEMINI_CLI_HOME") },
        }
        assert_eq!(result, None);
    }
}
