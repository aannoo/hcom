use std::io::Read as _;
use std::time::Instant;

use serde_json::{Value, json};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::{HookPayload, HookResult, common};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_LISTENING};

use super::config::kimi_config_dir;

fn resolve_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        payload.session_id.as_deref(),
        ctx.process_id.as_deref(),
    )
}

/// Resolve a Kimi session's transcript file.
///
/// Kimi stores each session's wire log at:
/// ```text
///   $KIMI_CODE_HOME/sessions/wd_<dir>_<hash>/<session_id>/agents/main/wire.jsonl
/// ```
/// The working-directory bucket (`wd_*`) is unknown here, so scan the buckets
/// for the one containing this session. `session_id` already carries the
/// `session_` prefix (matching the on-disk directory name).
pub fn derive_kimi_transcript_path(session_id: &str) -> Option<String> {
    let base = kimi_config_dir().join("sessions");
    if !base.exists() {
        return None;
    }
    let entries = std::fs::read_dir(&base).ok()?;
    for entry in entries.flatten() {
        let wd = entry.path();
        if !wd.is_dir() {
            continue;
        }
        let candidate = wd
            .join(session_id)
            .join("agents")
            .join("main")
            .join("wire.jsonl");
        if candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

fn update_position(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload, instance_name: &str) {
    let mut updates = serde_json::Map::new();
    if let Some(session_id) = payload.session_id.as_ref().filter(|s| !s.is_empty()) {
        updates.insert("session_id".into(), Value::String(session_id.clone()));
        if let Some(tp) = derive_kimi_transcript_path(session_id) {
            updates.insert("transcript_path".into(), Value::String(tp));
        }
    }
    let cwd = payload
        .raw
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_else(|| ctx.cwd.to_str().unwrap_or(""));
    if !cwd.is_empty() {
        updates.insert("directory".into(), Value::String(cwd.to_string()));
    }
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, &updates);
    }
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    if ctx.process_id.is_none() {
        return HookResult::Allow {
            additional_context: Some(format!(
                "[hcom available - run '{} start' to participate]",
                crate::runtime_env::build_hcom_command()
            )),
            system_message: None,
            delivery_ack: None,
        };
    }

    let session_id = match payload.session_id.as_deref() {
        Some(sid) => sid,
        None => return hook_noop(),
    };

    let prior_tool = ctx.process_id.as_deref().and_then(|pid| {
        db.get_process_binding(pid)
            .ok()
            .flatten()
            .and_then(|name| db.get_instance_full(&name).ok().flatten())
            .map(|row| row.tool)
    });

    let instance_name =
        instance_binding::bind_session_to_process(db, session_id, ctx.process_id.as_deref());

    log::log_info(
        "hooks",
        "kimi.sessionstart.bind",
        &format!(
            "instance={:?} session_id={} process_id={:?}",
            instance_name, session_id, ctx.process_id,
        ),
    );

    let instance_name = match instance_name {
        Some(name) => name,
        None => {
            if let Some(ref pid) = ctx.process_id {
                let env_tool = std::env::var("HCOM_TOOL").ok();
                let refuse = prior_tool.as_deref().is_some_and(|t| t != "kimi")
                    || env_tool.as_deref().is_some_and(|t| t != "kimi");
                if refuse {
                    log::log_warn(
                        "hooks",
                        "kimi.sessionstart.orphan_refused",
                        &format!(
                            "session_id={} process_id={} prior_tool={:?} env_tool={:?}",
                            session_id, pid, prior_tool, env_tool
                        ),
                    );
                    return hook_noop();
                }
                match instance_binding::create_orphaned_pty_identity(
                    db,
                    session_id,
                    Some(pid.as_str()),
                    "kimi",
                ) {
                    Some(name) => name,
                    None => return hook_noop(),
                }
            } else {
                return hook_noop();
            }
        }
    };

    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    update_position(db, ctx, payload, &instance_name);

    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );

    crate::runtime_env::set_terminal_title(&instance_name);
    crate::relay::worker::ensure_worker(true);

    // NOTE: bootstrap is intentionally NOT injected here. Kimi does not add
    // SessionStart hook output to model context (it is an observation-only
    // event), so bootstrapping happens on the first UserPromptSubmit instead
    // (see handle_userpromptsubmit).
    hook_noop()
}

fn handle_userpromptsubmit(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;
    update_position(db, ctx, payload, instance_name);

    // Bootstrap is delivered here, NOT at SessionStart: kimi only injects
    // UserPromptSubmit hook output into model context — SessionStart output is
    // not added to context (see kimi hooks docs). Prepend it to the first
    // delivery so a launched agent learns it's on hcom.
    //
    // KNOWN LIMITATION (kimi 0.9.0): this makes the bootstrap *visible* — kimi
    // wraps UserPromptSubmit output as a `<hook_result>` block in the turn,
    // unlike codex/claude which inject it invisibly into the system prompt.
    // Kimi has no per-instance invisible channel today: no `--system-prompt`
    // flag, no `-c` config override, no `systemPrompt` config key, and AGENTS.md
    // (its only invisible system-prompt source) loads from shared paths so it
    // can't carry a per-instance name without polluting the workspace. Revisit
    // and switch to an invisible launch-time injection (mirroring codex's
    // developer_instructions path) if a future kimi release adds a system-prompt
    // append flag or env var.
    let bootstrap =
        common::inject_bootstrap_once(db, ctx, instance_name, &instance, &instance.tool);
    let pending = common::prepare_pending_messages(db, instance_name);

    let additional_context = match (&bootstrap, &pending) {
        (Some(boot), Some(p)) => Some(format!("{boot}\n\n{}", p.formatted)),
        (Some(boot), None) => Some(boot.clone()),
        (None, Some(p)) => Some(p.formatted.clone()),
        (None, None) => None,
    };

    if let Some(additional_context) = additional_context {
        return HookResult::Allow {
            additional_context: Some(additional_context),
            system_message: None,
            delivery_ack: pending.map(|p| p.ack),
        };
    }

    hook_noop()
}

fn handle_pretooluse(db: &HcomDb, _ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, _ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    let detail =
        crate::hooks::family::extract_tool_detail("kimi", &payload.tool_name, &payload.tool_input);
    if !detail.is_empty() {
        lifecycle::set_status(db, instance_name, ST_ACTIVE, &detail, Default::default());
    }

    hook_noop()
}

fn handle_posttooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    if let Some(prepared) = common::prepare_pending_messages(db, instance_name) {
        return HookResult::Allow {
            additional_context: Some(prepared.formatted),
            system_message: None,
            delivery_ack: Some(prepared.ack),
        };
    }

    hook_noop()
}

/// PermissionRequest (observation-only): kimi fires this just before it blocks
/// waiting for the user to approve/reject a tool call. Mark the agent `blocked`
/// so `hcom list` reflects the stall and the delivery gate (require_idle) holds
/// off injecting until the user responds. `PermissionResult` clears it.
///
/// hcom's own `[[permission.rules]]` allow-rules mean an agent's `hcom send`
/// etc. are auto-approved and never reach an `ask` — so this only fires for
/// tool calls that genuinely need a human (mirrors claude's handle_permission_request).
fn handle_permissionrequest(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    let detail =
        crate::hooks::family::extract_tool_detail("kimi", &payload.tool_name, &payload.tool_input);
    lifecycle::set_status(
        db,
        instance_name,
        ST_BLOCKED,
        "approval",
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    );

    hook_noop()
}

/// PermissionResult (observation-only): kimi fires this once approval resolves.
/// In every case the agent is still mid-turn — an approved tool now runs, a
/// declined one feeds rejection back to the model — so flip out of `blocked`
/// back to `active`. The `Stop` hook sets `listening` when the turn actually
/// ends. (PermissionResult carries no tool_input, so the tool name is the best
/// available detail; mirrors claude's PostToolUse "approved:" restore.)
///
/// The payload's `decision` (`approved`/`rejected`/`cancelled`/`error`) drives a
/// decision-aware context so a declined call isn't mislabeled as approved:
/// `approved:<tool>` only when actually approved, `denied:<tool>` otherwise.
fn handle_permissionresult(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    let decision = payload.raw.get("decision").and_then(Value::as_str);
    let verb = if decision == Some("approved") {
        "approved"
    } else {
        "denied"
    };
    lifecycle::set_status(
        db,
        instance_name,
        ST_ACTIVE,
        &format!("{verb}:{}", payload.tool_name),
        Default::default(),
    );

    hook_noop()
}

pub(crate) fn handle_stop(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    if let Some(prepared) = common::prepare_pending_messages(db, instance_name) {
        // The Stop hook delivers via Block{reason}, which cannot carry the ack
        // back to the dispatch — commit inline so the cursor advances.
        common::commit_delivery_ack(db, &prepared.ack);
        return HookResult::Block {
            reason: prepared.formatted,
        };
    }

    lifecycle::set_status(db, instance_name, ST_LISTENING, "", Default::default());
    common::notify_hook_instance_with_db(db, instance_name);
    hook_noop()
}

pub(crate) fn handle_sessionend(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    if instance.tool != "kimi" {
        log::log_warn(
            "hooks",
            "kimi.sessionend.tool_mismatch_ignored",
            &format!(
                "instance={} tool={} payload_session_id={:?}",
                instance_name, instance.tool, payload.session_id
            ),
        );
        return hook_noop();
    }

    if let Some(incoming) = payload.session_id.as_deref()
        && let Some(primary) = instance.session_id.as_deref()
        && incoming != primary
    {
        log::log_warn(
            "hooks",
            "kimi.sessionend.historical_ignored",
            &format!(
                "instance={} incoming={} primary={}",
                instance_name, incoming, primary
            ),
        );
        return hook_noop();
    }

    let os_pid_alive = instance
        .pid
        .and_then(|p| u32::try_from(p).ok())
        .map(crate::sys::process::is_alive)
        .unwrap_or(false);
    let launched_live = std::env::var("HCOM_LAUNCHED").as_deref() == Ok("1")
        && ctx
            .process_id
            .as_deref()
            .and_then(|pid| db.get_process_binding(pid).ok().flatten())
            .as_deref()
            == Some(instance_name);

    if os_pid_alive || launched_live {
        common::soft_finalize_session(db, instance_name, "sessionend", None, true);
    } else {
        common::finalize_session(db, instance_name, "sessionend", None);
    }

    hook_noop()
}

fn handle_subagentstart(_db: &HcomDb, _ctx: &HcomContext, _payload: &HookPayload) -> HookResult {
    hook_noop()
}

fn handle_subagentstop(_db: &HcomDb, _ctx: &HcomContext, _payload: &HookPayload) -> HookResult {
    hook_noop()
}

fn handle_notification(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    if let Some(prepared) = common::prepare_pending_messages(db, instance_name) {
        return HookResult::Allow {
            additional_context: Some(prepared.formatted),
            system_message: None,
            delivery_ack: Some(prepared.ack),
        };
    }

    hook_noop()
}

fn hook_noop() -> HookResult {
    HookResult::Allow {
        additional_context: None,
        system_message: None,
        delivery_ack: None,
    }
}

pub(crate) fn get_handler(
    hook_name: &str,
) -> Option<fn(&HcomDb, &HcomContext, &HookPayload) -> HookResult> {
    match hook_name {
        "kimi-sessionstart" => Some(handle_sessionstart),
        "kimi-userpromptsubmit" => Some(handle_userpromptsubmit),
        "kimi-pretooluse" => Some(handle_pretooluse),
        "kimi-posttooluse" => Some(handle_posttooluse),
        "kimi-permissionrequest" => Some(handle_permissionrequest),
        "kimi-permissionresult" => Some(handle_permissionresult),
        "kimi-stop" => Some(handle_stop),
        "kimi-sessionend" => Some(handle_sessionend),
        "kimi-subagentstart" => Some(handle_subagentstart),
        "kimi-subagentstop" => Some(handle_subagentstop),
        "kimi-notification" => Some(handle_notification),
        _ => None,
    }
}

pub fn dispatch_kimi_hook(hook_name: &str) -> i32 {
    let start = Instant::now();

    let ctx = HcomContext::from_os();

    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        log::log_error(
            "hooks",
            "kimi.stdin_error",
            &format!("hook={} err={}", hook_name, e),
        );
        return 0;
    }

    let raw: Value = match serde_json::from_slice(&input) {
        Ok(v) => v,
        Err(e) => {
            log::log_error(
                "hooks",
                "kimi.parse_error",
                &format!("hook={} err={}", hook_name, e),
            );
            return 0;
        }
    };

    let payload = HookPayload::from_kimi(hook_name, raw);

    // Pre-gate: skip UserPromptSubmit for non-participants
    if !ctx.is_launched && hook_name == "kimi-userpromptsubmit" {
        let sid = match payload.session_id.as_deref() {
            Some(sid) => sid,
            None => return 0,
        };
        if let Ok(db) = HcomDb::open() {
            if db.get_session_binding(sid).ok().flatten().is_none() {
                return 0;
            }
        } else {
            return 0;
        }
    }

    if !crate::paths::ensure_hcom_directories() {
        return 0;
    }

    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log::log_error("hooks", "kimi.db.error", &format!("{}", e));
            return 0;
        }
    };

    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }

    let handler = match get_handler(hook_name) {
        Some(h) => h,
        None => {
            log::log_error(
                "hooks",
                "kimi.dispatch.unknown",
                &format!("Unknown Kimi hook: {}", hook_name),
            );
            return 0;
        }
    };

    let result = common::dispatch_with_panic_guard(
        "kimi",
        hook_name,
        HookResult::Allow {
            additional_context: None,
            system_message: None,
            delivery_ack: None,
        },
        || handler(&db, &ctx, &payload),
    );

    let exit_code = match &result {
        HookResult::Allow { .. } => 0,
        HookResult::Block { .. } => 2,
        HookResult::UpdateInput { .. } => 0,
    };

    match result {
        HookResult::Allow {
            additional_context: Some(ctx),
            delivery_ack,
            ..
        } => {
            let output = json!({
                "hookSpecificOutput": {
                    "message": ctx,
                }
            });
            println!("{}", output);
            // Advance the delivery cursor only after the message is handed to
            // kimi (stdout). Without this the PTY delivery loop never observes
            // the cursor advancing and keeps re-injecting `<hcom>`.
            if let Some(ack) = delivery_ack {
                common::commit_delivery_ack(&db, &ack);
            }
        }
        HookResult::Block { reason } => {
            let output = json!({
                "hookSpecificOutput": {
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            });
            eprintln!("{}", reason);
            println!("{}", output);
        }
        _ => {}
    }

    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    log::log_info(
        "hooks",
        "kimi.dispatch.timing",
        &format!(
            "hook={} exit_code={} total_ms={:.2}",
            hook_name, exit_code, total_ms
        ),
    );

    exit_code
}
