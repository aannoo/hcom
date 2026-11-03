//! Resume command: `hcom r <name> [tool-args...]`
//!
//!
//! Loads a stopped instance's snapshot and relaunches with --resume session_id.

use anyhow::{Result, bail};
use serde_json::json;
use std::io::BufRead;

use crate::commands::launch::{
    LaunchOutputContext, LaunchPreview, extract_launch_flags, is_background_from_args,
    load_hcom_config, print_launch_preview, report_launch, resolve_launcher_name,
};
use crate::db::HcomDb;
use crate::hooks::codex::derive_codex_transcript_path;
use crate::hooks::gemini::derive_gemini_transcript_path;
use crate::hooks::kimi::derive_kimi_transcript_path;
use crate::launcher::{self, LaunchParams, LaunchResult, LaunchTool};
use crate::log::log_info;
use crate::router::GlobalFlags;
use crate::shared::ST_INACTIVE;
use crate::tool::Tool;
use crate::transcript::{claude_projects_dir, detect_tool_from_path, qoder_projects_dir};
use rusqlite::OptionalExtension;

/// Where to load the resume/fork plan from.
enum ResumeSource<'a> {
    /// Resume an hcom-tracked instance by name (active or stopped).
    /// `stop_event` pins the life.stopped event to load the conversation
    /// from, set when the request named a session rather than the agent.
    Instance {
        name: &'a str,
        stop_event: Option<i64>,
    },
    /// Adopt a session from its on-disk transcript (first-time bring-in under hcom).
    Disk {
        session_id: String,
        tool: Tool,
        cwd_hint: Option<String>,
    },
}

struct PreparedResume {
    tool: Tool,
    output: ResumeOutputContext,
    launch: LaunchParams,
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
    json: bool,
}

struct TrackedForkIdentity {
    parent_name: String,
    effective_tag: Option<String>,
    custom_initial_prompt: Option<String>,
    custom_system_prompt: Option<String>,
}

struct ResumePromptInput<'a> {
    tool: Tool,
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
    let (name, extra_args) = match parse_resume_argv(argv, "r") {
        Ok(parsed) => parsed,
        Err(e) => return Ok(crate::commands::help::usage_error("resume", &e.to_string())),
    };

    do_resume(&name, false, &extra_args, flags)
}

/// Parse resume/fork argv: `r|f <name> [extra-args...]`
pub fn parse_resume_argv(argv: &[String], cmd: &str) -> Result<(String, Vec<String>)> {
    let mut i = 0;
    let mut saw_cmd = false;

    // Skip the command name (once: a generated name like "resume" is a valid
    // target) and global flags
    while i < argv.len() {
        match argv[i].as_str() {
            s if !saw_cmd && (s == cmd || s == "resume" || s == "fork" || s == "f") => {
                saw_cmd = true;
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
        bail!("hcom {cmd} needs an agent name");
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
    if extract_resume_flags(extra_args).1.stdio {
        bail!("--stdio is a launch transport; use hcom claude --stdio -- -p --resume <session-id>");
    }
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

        // Validate locally what the peer will never see: `--json` shapes local
        // output only, and a peer running an older hcom would forward it to
        // the tool.
        let (_, launch_flags, _) = extract_resume_flags(extra_args);
        crate::commands::launch::json_run_here(&launch_flags)?;
        let remote_args = crate::commands::launch::without_json_flag(extra_args);
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
                "extra_args": remote_args,
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
            remote_terminal: Some(crate::commands::launch::remote_terminal_preset(
                &inner, None,
            )),
            background: remote_output.background,
            run_here: remote_output.run_here,
            hcom_config: &hcom_config,
            inline_readiness_wait_secs: None,
            json: remote_output.json,
        };
        report_launch(&db, &launch_result, &output)?;
        return Ok(crate::commands::launch::readiness_exit_code(
            None,
            launch_result.failed,
        ));
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
    let (_, plan) = resolve_name_to_plan(db, name, fork, extra_args, flags)?;
    execute_prepared_resume_result(db, &plan)
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
    // A session ID selects one stopped snapshot. Carry it to the plan rather
    // than re-resolving by name, which would load the name's newest stop.
    let mut stop_event: Option<i64> = None;

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
            if let Ok(Some((instance_name, event_id))) =
                db.find_stopped_event_by_session_id(&current)
            {
                current = instance_name;
                stop_event = Some(event_id);
                continue;
            }
            let plan = build_adopt_plan(db, &current, fork, extra_args, flags)?;
            return Ok((current, plan));
        }

        if stop_event.is_none()
            && matches!(db.get_instance_full(&current), Ok(None) | Err(_))
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
            if let Ok(Some((instance_name, event_id))) =
                db.find_stopped_event_by_session_id(&session_id)
            {
                current = instance_name;
                stop_event = Some(event_id);
                continue;
            }
            let plan = build_adopt_plan(db, &session_id, fork, extra_args, flags)?;
            return Ok((session_id, plan));
        }

        let plan = prepare_resume_plan_from_source(
            db,
            ResumeSource::Instance {
                name: &current,
                stop_event,
            },
            fork,
            extra_args,
            flags,
        )?;
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
    prepare_resume_plan_from_source(
        db,
        ResumeSource::Instance {
            name,
            stop_event: None,
        },
        fork,
        extra_args,
        flags,
    )
}

/// Refuse to resume an agent whose process is still running; a verifiably
/// dead one is stopped here (writing its snapshot) instead of waiting for a
/// sweep. The process identity decides when there is one, because a live
/// process can sit at `inactive` (soft-finalize, StopFailure) and a dead one
/// at `listening`; the stored status is only the fallback.
fn ensure_not_running(db: &HcomDb, name: &str) -> Result<()> {
    use crate::instance_lifecycle::{TrackedProcess, reap_if_process_gone};
    let Ok(Some(inst)) = db.get_instance_full(name) else {
        return Ok(());
    };
    let running = match reap_if_process_gone(db, &inst) {
        TrackedProcess::Alive => true,
        TrackedProcess::Reaped => false,
        TrackedProcess::ReapFailed(e) => bail!("'{name}' exited but could not be stopped: {e}"),
        TrackedProcess::Unverifiable => inst.status != ST_INACTIVE,
    };
    if running {
        bail!(
            "'{name}' is still running.\n  Branch a copy instead: hcom f {name}\n  Or stop it first:     hcom kill {name}"
        );
    }
    Ok(())
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
        ResumeSource::Instance { name, stop_event } => {
            if !fork {
                ensure_not_running(db, name)?;
            }
            let (tool, sid, largs, tag, bg, leid, snap) = match stop_event {
                Some(event_id) => load_pinned_snapshot(db, name, event_id)?,
                None if fork => load_instance_data(db, name)?,
                None => load_stopped_snapshot(db, name)?,
            };
            let tool = resolve_adhoc_resume_tool(name, tool, &sid, fork)?;
            (tool, sid, largs, tag, bg, leid, snap, name.to_string())
        }
        ResumeSource::Disk {
            session_id,
            tool,
            cwd_hint,
        } => {
            let display = session_id.clone();
            (
                tool.as_str().to_string(),
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
        bail!(missing_session_error(db, &display_name, fork));
    }

    let launch_tool = validate_resume_operation(&tool, fork)?;
    let tool = launch_tool.tool();
    let inherited_tag = if tag.is_empty() {
        None
    } else {
        Some(tag.clone())
    };

    // Extract hcom-level flags from extra args before tool parsing.
    let (dir_override, launch_flags, clean_extra) = extract_resume_flags(extra_args);
    let run_here = crate::commands::launch::json_run_here(&launch_flags)?;

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
    let session_args = build_resume_args(tool, &session_id, fork);
    let (persisted_args, merged_cli_args) =
        compose_resume_args(tool, &original_args, &session_args, &clean_extra);

    let mut merged_args = merged_cli_args.clone();

    let is_headless =
        launch_flags.headless || is_background_from_args(&launch_tool, &merged_args) || background;

    // Claude background defaults to the live PTY session; only an explicit
    // `-p`/`--print` (NativePrint) gets the detached print-mode defaults. This
    // mirrors `prepare_launch_execution` on the launch path so resume/fork and
    // launch share one execution-mode policy. `-p` is opt-in because, from
    // 2026-06-15, `claude -p` draws from a separate Agent SDK credit pool.
    if launch_tool == Tool::Claude.into()
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
            tool,
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
        tool,
        output: ResumeOutputContext {
            action: if fork { "fork" } else { "resume" }.to_string(),
            tool: launch_tool.as_str().to_string(),
            tag: output_tag,
            terminal: launch_flags.terminal.clone(),
            background: is_headless,
            run_here,
            launcher_name: launcher_name_for_output,
            json: launch_flags.json,
        },
        launch: LaunchParams {
            stdio: false,
            tool: launch_tool.as_str().to_string(),
            count: 1,
            args: merged_args,
            persisted_args: Some(persisted_args),
            // Forks start a new session on first turn; only plain resume
            // inherits the prior id so kill-before-bind stays resumable.
            prior_session_id: (!fork).then(|| session_id.clone()),
            resume_cursor: (!fork && !is_adoption).then_some(last_event_id),
            tag: launch_tag,
            system_prompt: effective_system_prompt,
            initial_prompt: fork_initial_prompt,
            background: is_headless,
            cwd: Some(effective_cwd),
            env: None,
            launcher: Some(launcher_name),
            run_here,
            batch_id: launch_flags.batch_id.clone(),
            name: launch_name,
            skip_validation: false,
            terminal: launch_flags.terminal.clone(),
            // Codex tracked-instance fork uses initial_prompt for an identity
            // reset; don't dilute it with a reply-handoff suffix. Adoption-fork
            // has no identity-reset prompt, so normal handoff rules apply.
            append_reply_handoff,
        },
        session_id,
        tracked_fork_identity,
    })
}

/// A missing session is not evidence that no work happened. Refuse to
/// relaunch, and include the last launch's saved output when available.
fn missing_session_error(db: &HcomDb, name: &str, fork: bool) -> String {
    let log_file = db
        .get_instance_full(name)
        .ok()
        .flatten()
        .map(|instance| instance.background_log_file)
        .or_else(|| {
            db.conn()
                .query_row(
                    "SELECT json_extract(data, '$.snapshot.background_log_file')
                     FROM events WHERE type = 'life' AND instance = ?1
                       AND json_extract(data, '$.action') = 'stopped'
                     ORDER BY id DESC LIMIT 1",
                    [name],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
        });
    let mut message = format!(
        "No session ID recorded for '{name}' — cannot {}",
        if fork { "fork" } else { "resume" }
    );
    if let Some(tail) = log_file
        .filter(|path| !path.is_empty())
        .and_then(|path| crate::instance_lifecycle::read_launch_log_tail(&path))
    {
        message.push_str("\nSaved PTY output:\n");
        message.push_str(&tail);
    }
    message
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
    let result = execute_prepared_resume_result(db, plan)?;

    if print_feedback_now {
        let output = LaunchOutputContext {
            action: &plan.output.action,
            tool: &plan.output.tool,
            requested_count: 1,
            tag: plan.output.tag.as_deref(),
            launcher_name: &plan.output.launcher_name,
            terminal: plan.output.terminal.as_deref(),
            remote_terminal: None,
            background: plan.output.background,
            run_here: plan.output.run_here,
            hcom_config,
            inline_readiness_wait_secs,
            json: plan.output.json,
        };
        // Resume/fork share launch's inline readiness wait + exit-code mapping:
        // when invoked from inside an AI tool we block briefly so the caller
        // sees ready/blocked/failed instead of a bare "spawned" line.
        let readiness_state = report_launch(db, &result, &output)?;
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

fn execute_prepared_resume_result(db: &HcomDb, plan: &PreparedResume) -> Result<LaunchResult> {
    let launch = prepare_launch_for_execution(db, plan)?;
    launcher::launch(db, launch)
}

fn prepare_launch_for_execution(db: &HcomDb, plan: &PreparedResume) -> Result<LaunchParams> {
    let mut launch = plan.launch.clone();
    let Some(identity) = &plan.tracked_fork_identity else {
        return Ok(launch);
    };

    let reserved_child_name = crate::instance_names::reserve_generated_name(db)?;
    let (system_prompt, initial_prompt, append_reply_handoff) =
        build_resume_prompts(ResumePromptInput {
            tool: plan.tool,
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
        json: launch_flags.json,
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
        json: plan.output.json,
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
    // `--json` only changes how the outcome is reported, not what launches.
    let launch_flags = crate::commands::launch::HcomLaunchFlags {
        json: false,
        ..launch_flags.clone()
    };
    !tool_args.is_empty() || launch_flags != crate::commands::launch::HcomLaunchFlags::default()
}

/// A direct Claude/Codex run that joined with `hcom start` is stored as
/// `adhoc` but keeps its native session id. Resolve the owning tool from the
/// transcript on disk so resume/fork relaunches that session under hcom (with
/// hooks) under the same name.
fn resolve_adhoc_resume_tool(name: &str, tool: String, sid: &str, fork: bool) -> Result<String> {
    if tool != "adhoc" {
        return Ok(tool);
    }
    let op = if fork { "fork" } else { "resume" };
    if sid.is_empty() {
        bail!("'{name}' joined ad-hoc with no native session, so there is nothing to {op}");
    }
    match find_session_on_disk(sid) {
        Some((tool, _)) => Ok(tool.as_str().to_string()),
        None => bail!(
            "'{name}' joined ad-hoc from session {sid}, but no transcript for it was found, so hcom cannot tell which tool to {op}"
        ),
    }
}

/// Parse the saved tool name (canonical, alias, or `claude-pty`) and check it
/// supports the operation.
fn validate_resume_operation(tool: &str, fork: bool) -> Result<LaunchTool> {
    let launch_tool = LaunchTool::from_str(tool)
        .map_err(|_| anyhow::anyhow!("Unknown tool '{}' in saved session metadata", tool))?;
    // Drive fork support from the spec so help text and validation can't drift.
    let spec = launch_tool.spec();
    if fork && spec.resume.and_then(|r| r.fork).is_none() {
        bail!("{} does not support session forking (hcom f)", spec.label);
    }
    Ok(launch_tool)
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

    // Codex and Qoder fork identity resets belong in the initial prompt.
    // Qoder must preserve its saved system prompt across resume/fork.
    // Adoption-fork has no prior hcom identity, so normal bootstrap handles it.
    let initial_prompt = if fork && matches!(tool, Tool::Codex | Tool::Qoder) && !is_adoption {
        let child_name = child_name.expect("tracked fork child name should be available");
        let child_display = effective_tag
            .map(|tag| format!("{tag}-{child_name}"))
            .unwrap_or_else(|| child_name.to_string());
        let identity_reset = format!(
            "You are a fork of {display_name}, but your new hcom identity is now {child_display}.\n\
             Your hcom name is {child_name}.\n\
             Do not use {display_name}'s hcom identity anymore, even if it appears in inherited thread history.\n\
             Use `hcom ... --name {child_name}` for all hcom commands.\n\
             If asked about your identity, answer exactly: {child_display}"
        );
        Some(match custom_initial_prompt {
            Some(user_prompt) if !user_prompt.trim().is_empty() => {
                format!("{identity_reset}\n\n{user_prompt}")
            }
            _ => identity_reset,
        })
    } else if tool == Tool::Qoder && !is_adoption {
        let identity = resume_system_prompt(tool, display_name, false, None);
        Some(match custom_initial_prompt {
            Some(prompt) => format!("{identity}\n\n{prompt}"),
            None => identity,
        })
    } else {
        custom_initial_prompt.map(ToString::to_string)
    };

    // System prompt:
    // - Tracked-instance resume/fork: identity-carrying prompt (existing behavior).
    // - Adoption: None — SessionStart issues the normal fresh-launch bootstrap.
    let base_system_prompt = if is_adoption || tool == Tool::Qoder {
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
    let append_reply_handoff = !(fork && matches!(tool, Tool::Codex | Tool::Qoder) && !is_adoption);
    (system_prompt, initial_prompt, append_reply_handoff)
}

fn resume_system_prompt(tool: Tool, name: &str, fork: bool, child_name: Option<&str>) -> String {
    if fork {
        if tool == Tool::Codex {
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
fn load_instance_data(db: &HcomDb, name: &str) -> Result<SnapshotData> {
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

type SnapshotData = (String, String, String, String, bool, i64, String);

/// Load the conversation from one specific stop, and the participant's
/// cursor and tag from its newest stop. Both stops belong to the same agent
/// (names are never reassigned), so messages it already received after the
/// pinned stop are not redelivered.
fn load_pinned_snapshot(db: &HcomDb, name: &str, event_id: i64) -> Result<SnapshotData> {
    let data: Option<String> = db
        .conn()
        .query_row(
            "SELECT data FROM events
             WHERE id = ? AND type = 'life' AND instance = ?
               AND json_extract(data, '$.action') = 'stopped'",
            rusqlite::params![event_id, name],
            |row| row.get(0),
        )
        .optional()?;
    let Some(mut pinned) = data.as_deref().and_then(parse_stopped_snapshot) else {
        bail!("Stopped snapshot #{event_id} for '{name}' is missing or unreadable");
    };
    if let Ok((_, _, _, tag, _, cursor, _)) = load_stopped_snapshot(db, name) {
        pinned.3 = tag;
        pinned.5 = pinned.5.max(cursor);
    }
    Ok(pinned)
}

/// Load stopped snapshot from life events.
fn load_stopped_snapshot(db: &HcomDb, name: &str) -> Result<SnapshotData> {
    // Filter action='stopped' in SQL so we can't miss it past a LIMIT window
    // (old 10-row LIMIT could drop the snapshot after many relaunches).
    let data: Option<String> = db
        .conn()
        .query_row(
            "SELECT data FROM events
             WHERE type='life'
               AND instance=?
               AND json_extract(data, '$.action') = 'stopped'
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(snapshot) = data.as_deref().and_then(parse_stopped_snapshot) {
        return Ok(snapshot);
    }

    let known = crate::identity::known_agent_names(db);
    bail!(
        "No agent named '{name}' to resume (not a known hcom agent, session UUID, or thread name){}\n  Stopped agents: hcom list --stopped",
        crate::shared::suggest::did_you_mean(name, known.iter().map(String::as_str))
    )
}

fn parse_stopped_snapshot(data_str: &str) -> Option<SnapshotData> {
    let data = serde_json::from_str::<serde_json::Value>(data_str).ok()?;
    if data.get("action").and_then(|v| v.as_str()) != Some("stopped") {
        return None;
    }
    let snapshot = data.get("snapshot")?;
    let text = |key: &str| {
        snapshot
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let int = |key: &str| snapshot.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    Some((
        text("tool"),
        text("session_id"),
        text("launch_args"),
        text("tag"),
        int("background") != 0,
        int("last_event_id"),
        text("directory"),
    ))
}

/// Build tool-specific resume/fork args from the integration spec.
fn build_resume_args(tool: Tool, session_id: &str, fork: bool) -> Vec<String> {
    use crate::integration_spec::{ForkArgs, ResumeArgs};
    let Some(resume_spec) = tool.spec().resume.as_ref() else {
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

/// Resolve reusable settings once, then render the session operation from the
/// same settings that will be persisted. Session controls never enter the saved
/// argv, and explicit prompts/cwd flags survive for this invocation.
fn compose_resume_args(
    tool: Tool,
    original: &[String],
    session: &[String],
    extra: &[String],
) -> (Vec<String>, Vec<String>) {
    use crate::integration_spec::ResumeMerge;
    use crate::tools::argument_overrides::{override_args, saved_args};
    let inherited = saved_args(tool, original);
    let resolved = override_args(tool, &inherited, extra);
    let merge = tool.spec().resume.as_ref().expect("resume spec").merge;
    let command = match merge {
        ResumeMerge::Append => {
            let mut command = resolved.clone();
            crate::hooks::runtime::insert_before_separator(&mut command, session.iter().cloned());
            command
        }
        ResumeMerge::Prepend => [session, resolved.as_slice()].concat(),
    };
    (resolved, command)
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

fn opencode_family_db_path(tool: Tool) -> Option<std::path::PathBuf> {
    crate::runtime_env::opencode_family_db_path(tool.as_str())
}

/// Query an OpenCode-family SQLite DB for a session's working directory.
///
/// OpenCode 1 and Kilo keep sessions in `session`; OpenCode 2 keeps them in
/// `session_v2`. Either table may be absent, so a failed query (missing
/// table or row) just falls through to the next.
fn lookup_opencode_family_session(tool: Tool, session_id: &str) -> Option<String> {
    let db_path = opencode_family_db_path(tool)?;
    if !db_path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    ["session", "session_v2"].into_iter().find_map(|table| {
        conn.query_row(
            &format!("SELECT directory FROM {table} WHERE id = ?1"),
            rusqlite::params![session_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    })
}

fn lookup_family_session(session_id: &str) -> Option<(Tool, String)> {
    [Tool::OpenCode, Tool::Kilo]
        .into_iter()
        .find_map(|tool| lookup_opencode_family_session(tool, session_id).map(|cwd| (tool, cwd)))
}

/// Resolve a session ID to the owning tool and (optionally) a pre-recovered
/// working directory.
///
/// - Claude, Codex, Gemini: returns `(tool, Some(transcript_path))`.
///   Caller reads the transcript's first line to recover CWD.
/// - OpenCode family: returns `(tool, None)` — opencode stores sessions
///   in SQLite; CWD comes from a separate DB query, not a transcript file.
fn find_session_on_disk(session_id: &str) -> Option<(Tool, Option<String>)> {
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
                    if let Some(tool) = detect_tool_from_path(&path_str) {
                        return Some((tool, Some(path_str)));
                    }
                }
            }
        }
    }

    // 3. Codex
    if let Some(path) = derive_codex_transcript_path(session_id)
        && let Some(tool) = detect_tool_from_path(&path)
    {
        return Some((tool, Some(path)));
    }

    // 4. Gemini
    if let Some(path) = derive_gemini_transcript_path(session_id)
        && let Some(tool) = detect_tool_from_path(&path)
    {
        return Some((tool, Some(path)));
    }

    // 5. Cursor
    if let Some(path) = derive_cursor_transcript_path(session_id)
        && let Some(tool) = detect_tool_from_path(&path)
    {
        return Some((tool, Some(path)));
    }

    // 6. Kimi
    if let Some(path) = derive_kimi_transcript_path(session_id)
        && let Some(tool) = detect_tool_from_path(&path)
    {
        return Some((tool, Some(path)));
    }

    // 6b. Qoder (Claude-format transcripts under its own config dir)
    if let Some(path) = derive_qoder_transcript_path(session_id) {
        return Some((Tool::Qoder, Some(path)));
    }

    // 7. Copilot
    if let Some(path) = derive_copilot_transcript_path(session_id)
        && let Some(tool) = detect_tool_from_path(&path)
    {
        return Some((tool, Some(path)));
    }

    // 8. Grok
    if let Some(path) = derive_grok_transcript_path(session_id) {
        return Some((Tool::Grok, Some(path)));
    }

    // 9. Pi / OMP. Attribute by root PROVENANCE, not probe order:
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
    Found { tool: Tool, path: String },
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
            (true, false) => Some(Some(Tool::Pi)),
            (false, true) => Some(Some(Tool::Omp)),
            (false, false) => None,
        }
    });

    let roots = pi_exclusive_roots()
        .into_iter()
        .map(|root| (root, Some(Tool::Pi)))
        .chain(
            omp_exclusive_roots()
                .into_iter()
                .map(|root| (root, Some(Tool::Omp))),
        )
        .chain(shared.zip(shared_owner));

    for (root, owner) in roots {
        if root.exists()
            && let Some(path) = find_pi_transcript_in_root(&root, session_id)
        {
            return match owner.or_else(|| pi_omp_path_owner(&path)) {
                Some(tool) => PiOmpMatch::Found { tool, path },
                None => PiOmpMatch::Ambiguous { path },
            };
        }
    }
    PiOmpMatch::None
}

/// Return the owner encoded in a shared path's `.pi` or `.omp` marker.
fn pi_omp_path_owner(path: &str) -> Option<Tool> {
    detect_tool_from_path(path).filter(|tool| matches!(tool, Tool::Pi | Tool::Omp))
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

/// Locate a Qoder CLI transcript by session UUID:
/// `<QODER_CONFIG_DIR or ~/.qoder>/projects/<slug>/<uuid>.jsonl`.
fn derive_qoder_transcript_path(session_id: &str) -> Option<String> {
    let projects = qoder_projects_dir();
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(format!("{session_id}.jsonl")))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
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

/// Grok keeps each session in `$GROK_HOME/sessions/<url-encoded cwd>/<id>/`.
fn derive_grok_transcript_path(session_id: &str) -> Option<String> {
    let sessions = crate::transcript::grok::grok_config_dir().join("sessions");
    std::fs::read_dir(sessions)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(session_id).join("updates.jsonl"))
        .find(|path| path.exists())
        .map(|path| path.to_string_lossy().into_owned())
}

/// Grok's `summary.json`, next to `updates.jsonl`, records the session cwd.
fn recover_grok_cwd(transcript_path: &str) -> Option<String> {
    let summary = std::path::Path::new(transcript_path)
        .parent()?
        .join("summary.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(summary).ok()?).ok()?;
    value["info"]["cwd"].as_str().map(str::to_string)
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
fn extract_cwd_from_transcript(path: &str, tool: Tool) -> Option<String> {
    match tool {
        // Qoder transcripts are Claude-format: `cwd` rides on the first user record.
        Tool::Claude | Tool::Qoder => {
            scan_lines_for_cwd(path, 20, |v| v.get("cwd").and_then(|c| c.as_str()))
        }
        Tool::Codex => scan_lines_for_cwd(path, 1, |v| {
            v.get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
        }),
        // Antigravity shares Gemini's session tree/format (GEMINI_CLI_HOME);
        // an agy-pathed transcript detected as Antigravity still has the
        // gemini `projectHash` field, so reuse the same recovery.
        Tool::Gemini | Tool::Antigravity => recover_gemini_cwd(path),
        Tool::Cursor => recover_cursor_cwd(path),
        Tool::Kimi => None, // Kimi context.jsonl does not store cwd
        Tool::Grok => recover_grok_cwd(path),
        Tool::Copilot => scan_lines_for_cwd(path, 20, |v| {
            v.get("event")
                .or_else(|| v.get("type"))
                .and_then(|event| event.as_str())
                .filter(|event| *event == "session.start")
                .and_then(|_| v.get("data"))
                .and_then(|data| data.get("cwd"))
                .and_then(|cwd| cwd.as_str())
        }),
        Tool::Pi | Tool::Omp => scan_lines_for_cwd(path, 10, |v| {
            if v.get("type").and_then(|t| t.as_str()) == Some("session") {
                v.get("cwd")
                    .or_else(|| v.get("session").and_then(|s| s.get("cwd")))
                    .and_then(|c| c.as_str())
            } else {
                None
            }
        }),
        // Found through their own session stores, not a transcript path.
        Tool::OpenCode | Tool::Kilo | Tool::Hermes | Tool::Adhoc => None,
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
        let kilo_db = opencode_family_db_path(Tool::Kilo)
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
                 - Qoder:    ~/.qoder/projects/*/{sid}.jsonl\n  \
                 - Copilot:  ~/.copilot/session-state/{sid}/events.jsonl\n  \
                 - Grok:     {grok}/*/{sid}/updates.jsonl\n  \
                 - Oh My Pi: ~/.omp/agent/sessions/**/*{sid}*.jsonl
                 - Pi:       ~/.pi/agent/sessions/**/*{sid}*.jsonl",
                sid = session_id,
                claude = claude_projects.display(),
                grok = crate::transcript::grok::grok_config_dir()
                    .join("sessions")
                    .display(),
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
        let raw = if matches!(tool, Tool::OpenCode | Tool::Kilo) {
            lookup_opencode_family_session(tool, session_id)
        } else if let Some(ref path) = transcript_path {
            extract_cwd_from_transcript(path, tool)
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

    // Existing fixtures supply the operation selector and overrides together.
    fn merge_resume_args(tool: Tool, original: &[String], resume: &[String]) -> Vec<String> {
        let end = if matches!(
            resume.get(2).map(String::as_str),
            Some("--fork-session" | "--fork")
        ) {
            3
        } else {
            2
        };
        compose_resume_args(tool, original, &resume[..end], &resume[end..]).1
    }

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
    fn resume_cursor_is_only_inherited_by_tracked_resumes() {
        for cursor in [0, 1] {
            let db = test_db();
            db.log_event(
                "life",
                "luna",
                &json!({"action": "stopped", "snapshot": {
                    "tool": "codex", "session_id": "test-session",
                    "last_event_id": cursor, "directory": "/tmp", "launch_args": "[]"
                }}),
            )
            .unwrap();
            for fork in [false, true] {
                let tracked = prepare_resume_plan_from_source(
                    &db,
                    ResumeSource::Instance {
                        name: "luna",
                        stop_event: None,
                    },
                    fork,
                    &[],
                    &GlobalFlags::default(),
                )
                .unwrap();
                assert_eq!(tracked.launch.resume_cursor, (!fork).then_some(cursor));
                let adopted = prepare_resume_plan_from_source(
                    &db,
                    ResumeSource::Disk {
                        session_id: "untracked-session".into(),
                        tool: Tool::Codex,
                        cwd_hint: Some("/tmp".into()),
                    },
                    fork,
                    &[],
                    &GlobalFlags::default(),
                )
                .unwrap();
                assert_eq!(adopted.launch.resume_cursor, None);
            }
        }
    }

    /// Issue #166 comment: a session ID selects one stopped snapshot. It must
    /// not collapse to the name and load whichever stop is newest under it.
    #[test]
    fn session_id_resume_keeps_the_selected_snapshot() {
        let db = test_db();
        let old_sid = "11111111-1111-4111-8111-111111111111";
        let new_sid = "22222222-2222-4222-8222-222222222222";
        for (sid, cursor, tag) in [(old_sid, 10, "old"), (new_sid, 50, "new")] {
            db.log_event(
                "life",
                "luna",
                &json!({"action": "stopped", "snapshot": {
                    "tool": "codex", "session_id": sid, "tag": tag,
                    "last_event_id": cursor, "directory": "/tmp", "launch_args": "[]"
                }}),
            )
            .unwrap();
        }

        for fork in [false, true] {
            let (name, plan) =
                resolve_name_to_plan(&db, old_sid, fork, &[], &GlobalFlags::default()).unwrap();
            assert_eq!(name, "luna");
            assert_eq!(plan.session_id, old_sid, "fork={fork}");
            assert!(plan.launch.args.iter().any(|a| a == old_sid), "fork={fork}");
            if !fork {
                // Same agent: keep its newest cursor and tag so messages it
                // already received after the old stop aren't redelivered.
                assert_eq!(plan.launch.resume_cursor, Some(50));
                assert_eq!(plan.launch.tag.as_deref(), Some("new"));
            }
        }

        let (_, by_name) =
            resolve_name_to_plan(&db, "luna", false, &[], &GlobalFlags::default()).unwrap();
        assert_eq!(by_name.session_id, new_sid);
    }

    #[test]
    fn missing_session_refuses_resume_and_fork_with_saved_output() {
        let db = test_db();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("pty.log");
        std::fs::write(&log, "Startup failed: invalid model\n").unwrap();
        db.log_event(
            "life",
            "luna",
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex", "session_id": "", "launch_args": "[]",
                    "background_log_file": log,
                }
            }),
        )
        .unwrap();
        for fork in [false, true] {
            let error = prepare_resume_plan(&db, "luna", fork, &[], &GlobalFlags::default())
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("No session ID recorded"), "{error}");
            assert!(error.contains(if fork { "cannot fork" } else { "cannot resume" }));
            assert!(error.contains("Saved PTY output:\nStartup failed: invalid model"));
        }
        assert!(db.get_instance_full("luna").unwrap().is_none());
        std::fs::remove_file(log).unwrap();
        let error = prepare_resume_plan(&db, "luna", false, &[], &GlobalFlags::default())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("cannot resume"));
        assert!(!error.contains("PTY output"));
    }

    #[test]
    fn recorded_session_is_passed_to_native_resume_without_disk_lookup() {
        let db = test_db();
        for tool in ["claude", "pi", "omp"] {
            db.log_event(
                "life",
                tool,
                &json!({
                    "action": "stopped",
                    "snapshot": {"tool": tool, "session_id": "missing-on-disk", "launch_args": "[]"}
                }),
            )
            .unwrap();
            let plan = prepare_resume_plan(&db, tool, false, &[], &GlobalFlags::default()).unwrap();
            assert_eq!(
                plan.launch.prior_session_id.as_deref(),
                Some("missing-on-disk")
            );
            assert!(
                plan.launch
                    .args
                    .iter()
                    .any(|arg| arg.contains("missing-on-disk"))
            );
        }
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

        // Generated names can be command words ("resume" is CVCVCV).
        let (name, extra) = parse_resume_argv(&s(&["r", "resume", "--model", "x"]), "r").unwrap();
        assert_eq!(name, "resume");
        assert_eq!(extra, s(&["--model", "x"]));
        let (name, _) = parse_resume_argv(&s(&["f", "resume"]), "f").unwrap();
        assert_eq!(name, "resume");
    }

    #[test]
    fn test_build_resume_args_claude() {
        let args = build_resume_args(Tool::Claude, "sess-123", false);
        assert_eq!(args, s(&["--resume", "sess-123"]));
    }

    #[test]
    fn resume_and_fork_reuse_the_resolved_settings_without_growth() {
        let mut saved = s(&["-m", "old", "-s", "read-only", "--model", "stale"]);
        for (fork, extra, expected_model) in [
            (
                false,
                s(&["--model", "new", "--sandbox=workspace-write"]),
                "new",
            ),
            (false, s(&["-m", "new", "-s", "workspace-write"]), "new"),
            (true, vec![], "new"),
            (false, vec![], "new"),
        ] {
            let session = build_resume_args(Tool::Codex, "session-id", fork);
            let (persisted, command) = compose_resume_args(Tool::Codex, &saved, &session, &extra);
            let model_at = persisted
                .iter()
                .position(|s| s == "-m" || s == "--model")
                .unwrap();
            assert_eq!(persisted[model_at + 1], expected_model);
            assert!(
                persisted.len() <= 4,
                "singletons must not accumulate: {persisted:?}"
            );
            assert!(
                !persisted
                    .iter()
                    .any(|s| s == "resume" || s == "fork" || s == "session-id")
            );
            let mut expected = session;
            expected.extend(persisted.clone());
            assert_eq!(
                command, expected,
                "execution and persistence must use the same settings"
            );
            saved = persisted;
        }
    }

    #[test]
    fn resume_plan_repairs_poisoned_snapshot_and_keeps_explicit_prompt() {
        let db = test_db();
        db.log_event(
            "life",
            "memo",
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex", "session_id": "old-session",
                    "launch_args": serde_json::to_string(&s(&[
                        "-m", "old", "--model", "new", "--", "old prompt"
                    ])).unwrap(),
                }
            }),
        )
        .unwrap();
        let plan = prepare_resume_plan(
            &db,
            "memo",
            false,
            &s(&["--", "--", "new prompt"]),
            &GlobalFlags::default(),
        )
        .unwrap();
        assert_eq!(
            plan.launch.persisted_args.unwrap(),
            s(&["--model", "new", "--", "new prompt"])
        );
        assert_eq!(
            plan.launch.args,
            s(&[
                "resume",
                "old-session",
                "--model",
                "new",
                "--",
                "new prompt"
            ])
        );
    }

    #[test]
    fn test_build_resume_args_claude_fork() {
        let args = build_resume_args(Tool::Claude, "sess-123", true);
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
        let args = build_resume_args(Tool::Codex, "sess-456", false);
        assert_eq!(args, s(&["resume", "sess-456"]));
    }

    #[test]
    fn test_build_resume_args_codex_fork() {
        let args = build_resume_args(Tool::Codex, "sess-456", true);
        assert_eq!(args, s(&["fork", "sess-456"]));
    }

    #[test]
    fn codex_resume_replays_config_in_session_scope_with_new_overrides_last() {
        let original = s(&[
            "--model",
            "gpt-6-luna",
            "-c",
            "model_reasoning_effort=\"low\"",
            "--yolo",
            "--config=service_tier=\"fast\"",
            "-c=features.foo=true",
            "-cfeatures.bar=false",
        ]);
        for fork in [false, true] {
            let mut resume = build_resume_args(Tool::Codex, "session-id", fork);
            resume.extend(s(&[
                "--model",
                "gpt-6-astra",
                "--config",
                "model_reasoning_effort=\"high\"",
            ]));
            let merged = merge_resume_args(Tool::Codex, &original, &resume);
            assert_eq!(
                merged,
                s(&[
                    if fork { "fork" } else { "resume" },
                    "session-id",
                    "-c",
                    "model_reasoning_effort=\"low\"",
                    "--yolo",
                    "--config=service_tier=\"fast\"",
                    "-c=features.foo=true",
                    "-cfeatures.bar=false",
                    "--model",
                    "gpt-6-astra",
                    "--config",
                    "model_reasoning_effort=\"high\"",
                ])
            );
        }
    }

    #[test]
    fn test_build_resume_args_gemini() {
        let args = build_resume_args(Tool::Gemini, "sess-789", false);
        assert_eq!(args, s(&["--resume", "sess-789"]));
    }

    #[test]
    fn test_build_resume_args_omp_resume() {
        let args = build_resume_args(Tool::Omp, "sess-omp", false);
        assert_eq!(args, s(&["--resume", "sess-omp"]));
    }

    #[test]
    fn test_build_resume_args_omp_fork() {
        // Top-level `omp --help` omits `--fork`, but v15.1.9+ implements it:
        // `omp --fork <id>` takes the id as its value and routes through
        // SessionManager.forkFrom(...). Same shape as Pi — fork must emit
        // `["--fork", <id>]`, replacing `--resume`, not `["--resume", <id>,
        // "--fork"]`. hcom must not degrade `hcom f` into a plain `--resume`.
        let args = build_resume_args(Tool::Omp, "sess-omp", true);
        assert_eq!(args, s(&["--fork", "sess-omp"]));
    }

    #[test]
    fn test_merge_omp_args_fork_replaces_prior_session_controls() {
        // A fork's build_resume_args output (`--fork <id>`) merged over stored
        // launch args must strip the old resume/session/fork controls and keep
        // config flags.
        let fork = s(&["--fork", "new"]);
        assert_eq!(
            merge_resume_args(Tool::Omp, &s(&["--model", "opus", "--fork", "old"]), &fork),
            s(&["--fork", "new", "--model", "opus"])
        );
        assert_eq!(
            merge_resume_args(Tool::Omp, &s(&["--fork=old", "-r", "prev"]), &fork),
            s(&["--fork", "new"])
        );
    }

    #[test]
    fn test_merge_omp_args_strips_resume_aliases_and_values() {
        let resume = s(&["--resume", "new"]);
        assert_eq!(
            merge_resume_args(Tool::Omp, &s(&["--model", "opus", "-r", "old"]), &resume),
            s(&["--resume", "new", "--model", "opus"])
        );
        assert_eq!(
            merge_resume_args(
                Tool::Omp,
                &s(&["--resume=old", "--thinking", "high"]),
                &resume
            ),
            s(&["--resume", "new", "--thinking", "high"])
        );
        assert_eq!(
            merge_resume_args(
                Tool::Omp,
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
            merge_resume_args(
                Tool::Omp,
                &s(&["--session-dir", "--model", "opus"]),
                &resume
            ),
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
        assert_eq!(found.0, Tool::Omp);
    }

    #[test]
    #[serial_test::serial]
    fn test_lookup_opencode_family_session_falls_back_to_session_v2() {
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("oc.db");
        let _xdg = crate::hooks::test_helpers::EnvVarGuard::set("XDG_DATA_HOME", dir.path());
        let _db = crate::hooks::test_helpers::EnvVarGuard::set("OPENCODE_DB", &db_path);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        // v2-only DB: no `session` table at all.
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL);
             INSERT INTO session_v2 VALUES ('ses_v2only', '/work/v2');",
        )
        .unwrap();
        assert_eq!(
            lookup_opencode_family_session(Tool::OpenCode, "ses_v2only").as_deref(),
            Some("/work/v2")
        );
        // Migrated DB: the v1 row wins.
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);
             INSERT INTO session VALUES ('ses_v2only', '/work/v1');",
        )
        .unwrap();
        assert_eq!(
            lookup_opencode_family_session(Tool::OpenCode, "ses_v2only").as_deref(),
            Some("/work/v1")
        );
        assert_eq!(
            lookup_opencode_family_session(Tool::OpenCode, "ses_missing"),
            None
        );
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
        assert_eq!(found.0, Tool::Pi);
    }

    #[test]
    #[serial_test::serial]
    fn test_find_session_on_disk_attributes_shared_agent_dir_by_path_marker() {
        // The genuinely-shared PI_CODING_AGENT_DIR: attribution must key on the
        // path's product marker (.pi vs .omp), not on which tool's root list or
        // probe order found it. Agent dirs conventionally sit under .pi or
        // .omp, so the marker is present.
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
        assert_eq!(find_session_on_disk("mgpi").unwrap().0, Tool::Pi);

        // Managed OMP: PI_CODING_AGENT_DIR=<base>/.omp -> sessions/<file>.
        let omp_dir = base.path().join(".omp");
        let omp_root = omp_dir.join("sessions").join("proj");
        std::fs::create_dir_all(&omp_root).unwrap();
        std::fs::write(omp_root.join("mgomp.jsonl"), "{}").unwrap();
        unsafe { std::env::set_var("PI_CODING_AGENT_DIR", &omp_dir) }
        assert_eq!(find_session_on_disk("mgomp").unwrap().0, Tool::Omp);
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
        let args = build_resume_args(Tool::Antigravity, "conv-abc", false);
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
            Tool::Antigravity,
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
            Tool::Antigravity,
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
            Tool::Antigravity,
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
            Tool::Antigravity,
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
            Tool::Cursor,
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
            Tool::Cursor,
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
            Tool::Cursor,
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
            Tool::Cursor,
            &s(&["--model", "y", "-H", "X-Baked: 1", "--force", "stale task"]),
            &s(&["--resume", "sid", "--model", "x", "-H", "X-Resume: 1"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--resume",
                "sid",
                // Saved repeatables precede explicit repeatables.
                "-H",
                "X-Baked: 1",
                "--force",
                "--model",
                "x",
                "-H",
                "X-Resume: 1",
            ])
        );
    }

    /// A baked `--print`/`-p`/`--stream-partial-output` stays visible so the
    /// launcher can reject it clearly instead of silently changing semantics.
    #[test]
    fn test_merge_resume_args_cursor_preserves_print_flags_for_validation() {
        let merged = merge_resume_args(
            Tool::Cursor,
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

    /// An agy row with a recorded process and a stopped snapshot, as left by
    /// soft-finalize. `identity` None records the live identity of `pid`.
    fn insert_tracked_agy_row(db: &HcomDb, name: &str, status: &str, identity: Option<&str>) {
        let mut data = serde_json::Map::new();
        data.insert("session_id".into(), json!("agy-session-002"));
        data.insert("tool".into(), json!("antigravity"));
        data.insert("status".into(), json!(status));
        data.insert("directory".into(), json!("/tmp"));
        data.insert("created_at".into(), json!(1.0));
        db.save_instance_named(name, &data).unwrap();
        let pid = std::process::id();
        match identity {
            Some(identity) => db
                .update_instance_pid_with_identity(name, pid, Some(identity))
                .unwrap(),
            None => db.update_instance_pid(name, pid).unwrap(),
        }
        let snapshot = serde_json::json!({
            "action": "stopped",
            "snapshot": {
                "tool": "antigravity",
                "session_id": "agy-session-002",
                "launch_args": "[]",
                "directory": "/tmp"
            }
        });
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?, 'life', ?, ?)",
                rusqlite::params!["2026-01-01T00:00:00Z", name, snapshot.to_string()],
            )
            .unwrap();
    }

    #[test]
    fn test_resume_reaps_dead_process_reported_as_running() {
        let db = test_db();
        // Crash: the row still says listening, but its process incarnation is gone.
        insert_tracked_agy_row(
            &db,
            "dead",
            crate::shared::ST_LISTENING,
            Some("previous-boot-process"),
        );

        let result = prepare_resume_plan(&db, "dead", false, &[], &GlobalFlags::default());

        assert!(
            result.is_ok(),
            "dead process must be resumable: {:?}",
            result.err()
        );
        assert!(
            db.get_instance_full("dead").unwrap().is_none(),
            "row reaped"
        );
    }

    #[test]
    fn test_resume_refuses_live_process_marked_inactive() {
        let db = test_db();
        // Soft-finalize / StopFailure: inactive with a snapshot, process still running.
        insert_tracked_agy_row(&db, "live", ST_INACTIVE, None);

        let err = prepare_resume_plan(&db, "live", false, &[], &GlobalFlags::default())
            .err()
            .expect("a live process must not get a second copy");

        assert!(err.to_string().contains("still running"), "{err}");
        assert!(db.get_instance_full("live").unwrap().is_some());
    }

    #[test]
    fn test_resume_without_identity_falls_back_to_status() {
        let db = test_db();
        let mut data = serde_json::Map::new();
        data.insert("tool".into(), json!("antigravity"));
        data.insert("status".into(), json!(crate::shared::ST_LISTENING));
        data.insert("created_at".into(), json!(1.0));
        db.save_instance_named("legacy", &data).unwrap();

        let err = prepare_resume_plan(&db, "legacy", false, &[], &GlobalFlags::default())
            .err()
            .expect("unverifiable row trusts its status");

        assert!(err.to_string().contains("still running"), "{err}");
    }

    #[test]
    fn test_merge_resume_args_opencode_preserves_non_session_flags() {
        let merged = merge_resume_args(
            Tool::OpenCode,
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
            Tool::OpenCode,
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
        let args = build_resume_args(Tool::OpenCode, "sess-000", true);
        assert_eq!(args, s(&["--session", "sess-000", "--fork"]));
    }

    #[test]
    fn test_build_resume_args_kilo_fork() {
        let args = build_resume_args(Tool::Kilo, "sess-000", true);
        assert_eq!(args, s(&["--session", "sess-000", "--fork"]));
    }

    #[test]
    fn test_build_resume_args_pi_fork() {
        // Pi's `--fork <id>` takes the id as its value and is mutually exclusive
        // with `--session` (pi errors "--fork cannot be combined with
        // --session"), so fork must emit `["--fork", <id>]`, not
        // `["--session", <id>, "--fork"]`.
        let args = build_resume_args(Tool::Pi, "sess-000", true);
        assert_eq!(args, s(&["--fork", "sess-000"]));
    }

    #[test]
    fn test_merge_resume_args_pi_strips_session_controls_and_positional_prompt() {
        let merged = merge_resume_args(
            Tool::Pi,
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
    fn test_merge_resume_args_pi_boolean_flag_does_not_replay_prompt() {
        let merged = merge_resume_args(
            Tool::Pi,
            &s(&["--verbose", "fix x", "-t", "read,bash"]),
            &s(&["--session", "new-sess"]),
        );
        assert_eq!(
            merged,
            s(&["--session", "new-sess", "--verbose", "-t", "read,bash"])
        );
    }

    #[test]
    fn test_merge_resume_args_resume_flag_replaces_saved_across_spellings() {
        // OpenCode keeps its positional project dir; `-m` and `--model` are one flag.
        let merged = merge_resume_args(
            Tool::OpenCode,
            &s(&["-m", "old/model", "--prompt", "task", "proj"]),
            &s(&["--session", "ses_1", "--model", "new/model"]),
        );
        assert_eq!(
            merged,
            s(&["--session", "ses_1", "proj", "--model", "new/model"])
        );
        let merged = merge_resume_args(
            Tool::Kimi,
            &s(&["--model", "k2", "--agent", "coder", "--add-dir", "a"]),
            &s(&["--session", "sid", "-m", "k3", "--add-dir", "b"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--session",
                "sid",
                "--add-dir",
                "a",
                "-m",
                "k3",
                "--add-dir",
                "b"
            ])
        );
    }

    #[test]
    fn test_merge_resume_args_value_may_start_with_dash() {
        let merged = merge_resume_args(
            Tool::Qoder,
            &s(&[
                "--append-system-prompt",
                "- Always run tests",
                "--model",
                "m",
            ]),
            &s(&["--resume", "sid", "--append-system-prompt", "New rules"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--resume",
                "sid",
                "--model",
                "m",
                "--append-system-prompt",
                "New rules"
            ])
        );
    }

    #[test]
    fn test_merge_resume_args_pi_keeps_extension_flag_values() {
        let merged = merge_resume_args(
            Tool::Pi,
            &s(&["--extension", "preset.ts", "--preset", "review", "task"]),
            &s(&["--session", "new-sess"]),
        );
        assert_eq!(
            merged,
            s(&[
                "--session",
                "new-sess",
                "--extension",
                "preset.ts",
                "--preset",
                "review"
            ])
        );
    }

    #[test]
    fn test_merge_resume_args_antigravity_double_dash_short_aliases() {
        let merged = merge_resume_args(
            Tool::Antigravity,
            &s(&["--i", "old task", "--c", "-sandbox"]),
            &s(&["--conversation", "c2"]),
        );
        assert_eq!(merged, s(&["--conversation", "c2", "-sandbox"]));
    }

    #[test]
    fn test_merge_resume_args_antigravity_single_dash_long_flags() {
        let merged = merge_resume_args(
            Tool::Antigravity,
            &s(&[
                "-model",
                "old",
                "-conversation",
                "c1",
                "-i",
                "task",
                "--sandbox",
            ]),
            &s(&["--conversation", "c2", "--model", "new"]),
        );
        assert_eq!(
            merged,
            s(&["--conversation", "c2", "--sandbox", "--model", "new"])
        );
    }

    #[test]
    fn test_merge_resume_args_kilo_preserves_non_session_flags() {
        let merged = merge_resume_args(
            Tool::Kilo,
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
    fn test_should_preview_resume_ignores_json() {
        let flags = crate::commands::launch::HcomLaunchFlags {
            json: true,
            ..Default::default()
        };
        assert!(!should_preview_resume(&flags, &[]));
    }

    #[test]
    fn test_headless_fork_allowed_for_non_claude_tools() {
        // Every tool runs headless via the PTY runner, so `hcom f <x> --headless`
        // must not be gated to Claude.
        let db = test_db();
        for (name, tool) in [("luna", "codex"), ("nova", "opencode"), ("pira", "pi")] {
            let mut data = serde_json::Map::new();
            data.insert("session_id".into(), json!(format!("{tool}-session")));
            data.insert("tool".into(), json!(tool));
            data.insert("status".into(), json!("listening"));
            data.insert("created_at".into(), json!(1.0));
            db.save_instance_named(name, &data).unwrap();

            let plan = prepare_resume_plan(
                &db,
                name,
                true,
                &s(&["--headless"]),
                &GlobalFlags::default(),
            )
            .unwrap_or_else(|e| panic!("{tool} headless fork rejected: {e}"));
            assert!(plan.launch.background, "{tool} fork should be headless");
        }
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
                .contains(&format!("Your hcom name is {reserved_name}."))
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
        let prompt = resume_system_prompt(Tool::Codex, "luna", true, None);
        assert!(prompt.contains("already-assigned hcom identity"));
        assert!(!prompt.contains("Run hcom start"));
    }

    #[test]
    fn test_resume_system_prompt_non_codex_fork_states_new_identity() {
        let prompt = resume_system_prompt(Tool::Claude, "luna", true, Some("feri"));
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
        let prompt_none = resume_system_prompt(Tool::Claude, "luna", true, None);
        assert!(prompt_none.contains("already-assigned hcom identity"));
        assert!(!prompt_none.contains("Run hcom start"));
    }

    #[test]
    fn test_build_remote_resume_output_uses_actual_launch_result_background() {
        let db = test_db();
        let output = build_remote_resume_output(
            &db,
            &LaunchResult {
                exit_code: None,
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

    /// Point Claude's config dir at `dir` for the duration of `f` by setting
    /// CLAUDE_CONFIG_DIR. Restored on exit. serial_test required.
    fn with_claude_config_dir<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _dir = crate::hooks::test_helpers::EnvVarGuard::set("CLAUDE_CONFIG_DIR", dir);
        f()
    }

    #[test]
    #[serial_test::serial]
    fn adhoc_resume_resolves_tool_from_native_session() {
        let cfg_dir = tempfile::tempdir().unwrap();
        let projects = cfg_dir.path().join("projects/proj");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(projects.join("sid-plain.jsonl"), "{}\n").unwrap();

        with_claude_config_dir(cfg_dir.path(), || {
            assert_eq!(
                resolve_adhoc_resume_tool("vibe", "adhoc".into(), "sid-plain", false).unwrap(),
                "claude"
            );
            let err = resolve_adhoc_resume_tool("vibe", "adhoc".into(), "sid-gone", true)
                .unwrap_err()
                .to_string();
            assert!(err.contains("no transcript"), "got: {err}");
        });
        let err = resolve_adhoc_resume_tool("vibe", "adhoc".into(), "", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no native session"), "got: {err}");
        assert_eq!(
            resolve_adhoc_resume_tool("vibe", "codex".into(), "", false).unwrap(),
            "codex"
        );
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
        // not the name-based "No agent named ... to resume" message.
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
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), Tool::Claude);
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
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), Tool::Claude);
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
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), Tool::Claude);
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
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), Tool::Codex);
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
        let _gemini_home = crate::hooks::test_helpers::EnvVarGuard::set("GEMINI_CLI_HOME", base);
        let result = extract_cwd_from_transcript(session_path.to_str().unwrap(), Tool::Gemini);

        assert_eq!(result, Some(fake_cwd.to_string()));
    }

    #[test]
    fn test_build_resume_args_copilot() {
        let args = build_resume_args(Tool::Copilot, "sess-abc", false);
        assert_eq!(args, s(&["--resume", "sess-abc"]));
    }

    #[test]
    fn test_build_resume_args_copilot_fork_rejected() {
        // copilot has fork: None, so build_resume_args returns resume-only args
        let args = build_resume_args(Tool::Copilot, "sess-abc", true);
        assert_eq!(args, s(&["--resume", "sess-abc"]));
    }

    #[test]
    fn test_merge_grok_args_drops_worktree_and_keeps_rules() {
        let original = s(&["--worktree", "feat", "--rules", "BOOT", "--always-approve"]);
        let resume = s(&["--resume", "sess-1"]);
        let merged = merge_resume_args(Tool::Grok, &original, &resume);
        assert!(!merged.iter().any(|t| t == "--worktree" || t == "feat"));
        assert!(merged.contains(&"--rules".to_string()));
        assert!(merged.contains(&"BOOT".to_string()));
        assert!(merged.contains(&"--always-approve".to_string()));
    }

    #[test]
    fn test_merge_grok_args_drops_cwd() {
        let original = s(&["--cwd", "/old", "--model", "grok-build"]);
        let merged = merge_resume_args(Tool::Grok, &original, &s(&["--resume", "sess-1"]));
        assert!(!merged.iter().any(|arg| arg == "--cwd" || arg == "/old"));
        assert!(merged.contains(&"grok-build".to_string()));
    }

    #[test]
    fn test_merge_grok_args_bare_worktree_does_not_eat_model() {
        let original = s(&["--worktree", "--model", "grok-build"]);
        let resume = s(&["--resume", "sess-1"]);
        let merged = merge_resume_args(Tool::Grok, &original, &resume);
        assert!(!merged.contains(&"--worktree".to_string()));
        assert!(merged.contains(&"--model".to_string()));
        assert!(merged.contains(&"grok-build".to_string()));
    }

    #[test]
    fn test_merge_grok_args_short_worktree_drops_name() {
        let original = s(&["-w", "mytree", "--always-approve"]);
        let resume = s(&["--resume", "sess-1"]);
        let merged = merge_resume_args(Tool::Grok, &original, &resume);
        assert!(!merged.iter().any(|t| t == "-w" || t == "mytree"));
        assert!(merged.contains(&"--always-approve".to_string()));
    }

    #[test]
    fn test_merge_grok_args_drops_prompt_after_separator() {
        let original = s(&["--model", "grok-4", "--", "-x looks like a flag"]);
        let resume = s(&["--resume", "sess-1"]);
        let merged = merge_resume_args(Tool::Grok, &original, &resume);
        assert_eq!(merged, s(&["--resume", "sess-1", "--model", "grok-4"]));
    }

    #[test]
    fn test_build_resume_args_qoder_resume_and_fork() {
        assert_eq!(
            build_resume_args(Tool::Qoder, "sess-abc", false),
            s(&["--resume", "sess-abc"])
        );
        assert_eq!(
            build_resume_args(Tool::Qoder, "sess-abc", true),
            s(&["--resume", "sess-abc", "--fork-session"])
        );
    }

    #[test]
    fn test_merge_qoder_args_drops_prompt_and_selectors_keeps_flags() {
        let original = s(&[
            "--model",
            "Qwen3.8-Flash",
            "--permission-mode=accept_edits",
            "--resume",
            "old",
            "--fork-session",
            "-i",
            "do a task",
            "--",
            "-x looks like a flag",
        ]);
        let resume = s(&["--resume", "sess-abc", "--fork-session"]);
        let merged = merge_resume_args(Tool::Qoder, &original, &resume);
        assert_eq!(
            merged,
            s(&[
                "--resume",
                "sess-abc",
                "--fork-session",
                "--model",
                "Qwen3.8-Flash",
                "--permission-mode=accept_edits",
            ])
        );
    }

    #[test]
    fn test_merge_qoder_args_resume_model_wins_and_value_not_orphaned() {
        let original = s(&[
            "--model",
            "Qwen3.8-Flash",
            "--add-dir",
            "/a",
            "--name",
            "n1",
        ]);
        let resume = s(&["--resume", "sess-abc", "-m", "other", "--name", "n2"]);
        let merged = merge_resume_args(Tool::Qoder, &original, &resume);
        // The resume `-m` replaces the original `--model`; the shared `--name`
        // keeps only the resume value and nothing is left orphaned.
        assert!(!merged.contains(&"--model".to_string()));
        assert!(!merged.contains(&"Qwen3.8-Flash".to_string()));
        assert_eq!(merged.iter().filter(|t| t.as_str() == "--name").count(), 1);
        assert!(merged.contains(&"n2".to_string()));
        assert!(!merged.contains(&"n1".to_string()));
        let at = merged.iter().position(|t| t == "--add-dir").unwrap();
        assert_eq!(merged[at + 1], "/a");
    }

    #[test]
    fn qoder_resume_identity_does_not_replace_saved_system_prompt() {
        for fork in [false, true] {
            let (system, initial, _) = build_resume_prompts(ResumePromptInput {
                tool: Tool::Qoder,
                display_name: "memo",
                fork,
                is_adoption: false,
                child_name: Some("nova"),
                effective_tag: None,
                custom_system_prompt: None,
                custom_initial_prompt: Some("do work"),
            });
            assert!(system.is_none());
            let initial = initial.unwrap();
            assert!(initial.contains(if fork { "nova" } else { "memo" }));
            assert!(initial.contains("do work"));
        }
    }

    #[test]
    fn qoder_resume_preserves_and_replaces_variadic_tools() {
        for original in [
            s(&[
                "--tools", "Read", "Grep", "Edit", "--model", "m", "--", "prompt",
            ]),
            s(&[
                "--tools=Read",
                "Grep",
                "Edit",
                "--model",
                "m",
                "--",
                "prompt",
            ]),
        ] {
            let merged = merge_resume_args(Tool::Qoder, &original, &s(&["--resume", "sid"]));
            assert_eq!(&merged[2..], &original[..original.len() - 2]);
            let resume = s(&["--resume", "sid", "--tools", "Bash", "Write"]);
            assert_eq!(
                merge_resume_args(Tool::Qoder, &original, &resume),
                s(&[
                    "--resume", "sid", "--model", "m", "--tools", "Bash", "Write"
                ])
            );
        }
        assert_eq!(
            merge_resume_args(Tool::Qoder, &s(&["--tools", ""]), &s(&["--resume", "sid"])),
            s(&["--resume", "sid", "--tools", ""])
        );
    }

    #[test]
    fn test_merge_qoder_args_drops_saved_cwd_and_worktree() {
        let original = s(&[
            "--cwd",
            "sub",
            "--worktree",
            "feature-a",
            "-w=other",
            "--worktree",
            "--model",
            "Qwen3.8-Flash",
        ]);
        let resume = s(&["--resume", "sess-abc"]);
        assert_eq!(
            merge_resume_args(Tool::Qoder, &original, &resume),
            s(&["--resume", "sess-abc", "--model", "Qwen3.8-Flash"])
        );
        // A directory chosen at resume time is the caller's and is kept.
        let resume = s(&["--resume", "sess-abc", "--cwd", "elsewhere"]);
        assert_eq!(
            merge_resume_args(Tool::Qoder, &original, &resume),
            s(&[
                "--resume",
                "sess-abc",
                "--model",
                "Qwen3.8-Flash",
                "--cwd",
                "elsewhere"
            ])
        );
    }

    #[test]
    fn test_extract_cwd_qoder_from_first_user_record() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"workspace-directories","sessionId":"abc","directories":[]}"#,
                "\n",
                r#"{"type":"runtime-config","sessionId":"abc"}"#,
                "\n",
                r#"{"type":"user","cwd":"/home/user/proj","sessionId":"abc","message":{"role":"user","content":"hi"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let cwd = extract_cwd_from_transcript(file.path().to_str().unwrap(), Tool::Qoder);
        assert_eq!(cwd, Some("/home/user/proj".to_string()));
    }

    #[test]
    fn test_merge_copilot_args_preserves_model_drops_prompt() {
        let original = s(&["--model", "claude-haiku-4.5", "-i", "do a task"]);
        let resume = s(&["--resume", "sess-abc"]);
        let merged = merge_resume_args(Tool::Copilot, &original, &resume);
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
        let merged = merge_resume_args(Tool::Copilot, &original, &resume);
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
        let cwd = extract_cwd_from_transcript(file.path().to_str().unwrap(), Tool::Copilot);
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
        let _gemini_home = crate::hooks::test_helpers::EnvVarGuard::set("GEMINI_CLI_HOME", base);
        let result = extract_cwd_from_transcript(path.to_str().unwrap(), Tool::Gemini);
        assert_eq!(result, None);
    }
}
