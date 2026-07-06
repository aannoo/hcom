//! Launch-context capture plus process/session binding for instance records.
//!
//! This module owns the hook-facing identity handshake:
//! launch metadata capture, placeholder/canonical binding, and instance-row
//! initialization for newly launched or recovered sessions.

use crate::db::{HcomDb, InstanceRow};
use crate::instance_names::{PLACEHOLDER_CONTEXT, PLACEHOLDER_STATUS};
use crate::instances::update_instance_position;
use crate::shared::time::{now_epoch_f64, now_epoch_i64};
use crate::shared::{ST_INACTIVE, ST_LISTENING};

/// Persist terminal launch metadata without clobbering other launch_context fields.
///
/// The launcher owns the authoritative preset decision. launch_context is only
/// for late-bound metadata such as pane_id, terminal_id, and env snapshot.
pub fn persist_terminal_launch_context(
    db: &HcomDb,
    instance_name: &str,
    requested_preset: Option<&str>,
    effective_preset: &str,
    process_id: Option<&str>,
) {
    let mut ctx = db
        .get_instance_full(instance_name)
        .ok()
        .flatten()
        .and_then(|pos| pos.launch_context)
        .and_then(|json| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json).ok()
        })
        .unwrap_or_default();

    if let Some(pid) = process_id.filter(|v| !v.is_empty()) {
        ctx.insert("process_id".into(), serde_json::json!(pid));
    }
    if !effective_preset.is_empty() {
        ctx.insert(
            "terminal_preset_effective".into(),
            serde_json::json!(effective_preset),
        );
        // Legacy compatibility for older readers and migration logic.
        ctx.insert(
            "terminal_preset".into(),
            serde_json::json!(effective_preset),
        );
    }
    if let Some(requested) = requested_preset.filter(|v| !v.is_empty() && *v != "default") {
        ctx.insert(
            "terminal_preset_requested".into(),
            serde_json::json!(requested),
        );
    }

    let mut updates = serde_json::Map::new();
    updates.insert(
        "terminal_preset_requested".into(),
        serde_json::json!(
            requested_preset
                .filter(|v| !v.is_empty() && *v != "default")
                .unwrap_or("")
        ),
    );
    updates.insert(
        "terminal_preset_effective".into(),
        serde_json::json!(effective_preset),
    );
    updates.insert(
        "launch_context".into(),
        serde_json::json!(serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".to_string())),
    );
    update_instance_position(db, instance_name, &updates);
}

/// Capture environment context and store it for the instance.
///
/// Captures git branch, terminal program, tty, and relevant env vars.
pub fn capture_and_store_launch_context(db: &HcomDb, instance_name: &str) {
    let new_ctx = capture_context();

    // Preserve fields from prior context that can't be recaptured in hook env
    let preserve_keys = [
        "pane_id",
        "terminal_id",
        "kitty_listen_on",
        "process_id",
        "terminal_preset_effective",
    ];
    let mut ctx = new_ctx;

    let missing: Vec<&str> = preserve_keys
        .iter()
        .filter(|k| launch_context_value_missing(ctx.get(**k)))
        .copied()
        .collect();

    if !missing.is_empty()
        && let Ok(Some(pos)) = db.get_instance_full(instance_name)
        && let Some(old_json) = &pos.launch_context
        && let Ok(old_ctx) = serde_json::from_str::<serde_json::Value>(old_json)
    {
        for k in &missing {
            if let Some(val) = old_ctx.get(*k)
                && !launch_context_value_missing(Some(val))
            {
                ctx.insert(k.to_string(), val.clone());
            }
        }
    }

    let json = serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".to_string());
    let mut updates = serde_json::Map::new();
    updates.insert("launch_context".into(), serde_json::json!(json));
    update_instance_position(db, instance_name, &updates);
}

/// "Missing" for the preserve-from-prior-context check. Treats absent, JSON
/// null, and empty strings as missing. Non-string non-null values (numbers,
/// objects, arrays) are considered present even though every preserved field
/// is currently a string — this is intentionally conservative so a future
/// non-string preserved field doesn't silently get clobbered by re-capture.
fn launch_context_value_missing(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(s)) => s.is_empty(),
        Some(_) => false,
    }
}

/// Capture launch context snapshot.
fn capture_context() -> serde_json::Map<String, serde_json::Value> {
    let mut ctx = serde_json::Map::new();

    // Git branch
    let git_branch = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    ctx.insert("git_branch".into(), serde_json::json!(git_branch));

    // TTY
    let tty = std::process::Command::new("tty")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    ctx.insert("tty".into(), serde_json::json!(tty));

    // Env vars (only include if set)
    let env_keys = [
        "TERM_PROGRAM",
        "TERM_SESSION_ID",
        "WINDOWID",
        "ITERM_SESSION_ID",
        "KITTY_WINDOW_ID",
        "KITTY_PID",
        "KITTY_LISTEN_ON",
        "ALACRITTY_WINDOW_ID",
        "WEZTERM_PANE",
        "GNOME_TERMINAL_SCREEN",
        "KONSOLE_DBUS_WINDOW",
        "TERMINATOR_UUID",
        "TILIX_ID",
        "GUAKE_TAB_UUID",
        "WT_SESSION",
        "ConEmuHWND",
        "TMUX_PANE",
        "STY",
        "ZELLIJ_SESSION_NAME",
        "ZELLIJ_PANE_ID",
        "SSH_TTY",
        "SSH_CONNECTION",
        "WSL_DISTRO_NAME",
        "VSCODE_PID",
        "CURSOR_AGENT",
        "INSIDE_EMACS",
        "NVIM_LISTEN_ADDRESS",
        "CODESPACE_NAME",
        "GITPOD_WORKSPACE_ID",
        "CLOUD_SHELL",
        "REPL_ID",
    ];
    let mut env_map = serde_json::Map::new();
    for key in &env_keys {
        if let Ok(val) = std::env::var(key)
            && !val.is_empty()
        {
            env_map.insert((*key).to_string(), serde_json::json!(val));
        }
    }
    ctx.insert("env".into(), serde_json::Value::Object(env_map));

    // The launcher already resolved the effective preset; record it so
    // child agents can inherit it (see commands::launch). Pane IDs are
    // late-bound from the env vars the preset declares.
    if let Ok(preset_name) = std::env::var("HCOM_LAUNCHED_PRESET")
        && !preset_name.is_empty()
    {
        ctx.insert(
            "terminal_preset_effective".into(),
            serde_json::json!(preset_name),
        );

        if let Some(pane_id_env) = crate::config::get_merged_preset_pane_id_env(&preset_name)
            && let Ok(pane_id) = std::env::var(pane_id_env)
            && !pane_id.is_empty()
        {
            ctx.insert("pane_id".into(), serde_json::json!(pane_id));
        }
    }

    // Process ID for kitty close-by-env matching
    if let Ok(pid) = std::env::var("HCOM_PROCESS_ID")
        && !pid.is_empty()
    {
        ctx.insert("process_id".into(), serde_json::json!(pid));

        // Terminal ID from parent's stdout capture
        let id_file = crate::paths::hcom_dir()
            .join(".tmp")
            .join("terminal_ids")
            .join(&pid);
        if id_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&id_file) {
                let terminal_id = content.trim().to_string();
                if !terminal_id.is_empty() {
                    if std::env::var("HCOM_LAUNCHED_PRESET").as_deref() == Ok("zellij")
                        && let Some(pane_id) = zellij_pane_id_from_terminal_id(&terminal_id)
                    {
                        ctx.insert("pane_id".into(), serde_json::json!(pane_id));
                    }
                    ctx.insert("terminal_id".into(), serde_json::json!(terminal_id));
                }
            }
            let _ = std::fs::remove_file(&id_file);
        }
    }

    ctx
}

fn zellij_pane_id_from_terminal_id(terminal_id: &str) -> Option<String> {
    terminal_id
        .strip_prefix("terminal_")
        .filter(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
        .map(|suffix| suffix.to_string())
}

fn is_true_launch_placeholder(data: Option<&InstanceRow>) -> bool {
    let Some(data) = data else {
        return false;
    };
    if data.session_id.is_some() {
        return false;
    }

    if crate::instances::is_launching_placeholder(data) {
        return true;
    }

    // OpenCode and other PTY-backed tools can be marked ready/listening by the PTY
    // watcher before their later session-start hook binds a session id. Those rows
    // are still launch placeholders, but no longer match the stricter "pending/new"
    // display predicate. Keep this narrow: active no-session rows are real work, not
    // launch placeholders.
    data.status == ST_LISTENING
        && matches!(
            data.status_context.as_str(),
            "start" | "ready_observed" | "launch_blocked_cleared"
        )
}

fn migrate_placeholder_notify(db: &HcomDb, placeholder_name: &str, canonical_name: &str) -> bool {
    match db.migrate_notify_endpoints(placeholder_name, canonical_name) {
        Ok(()) => true,
        Err(e) => {
            crate::log::log_error("binding", "placeholder.migrate_endpoints", &format!("{e}"));
            false
        }
    }
}

/// Delete a true launch placeholder row. Notify endpoints remain on the canonical instance.
fn delete_true_placeholder_instance(db: &HcomDb, placeholder_name: &str) {
    match db.delete_instance(placeholder_name) {
        Ok(true) => {}
        Ok(false) => {
            crate::log::log_info(
                "binding",
                "placeholder.delete_missing",
                &format!("placeholder={placeholder_name}"),
            );
        }
        Err(e) => {
            crate::log::log_error("binding", "placeholder.delete", &format!("{e}"));
        }
    }
}

/// Carry runtime state the PTY wrapper wrote onto the placeholder row over to the
/// canonical instance before the placeholder is deleted. The OS `pid` and terminal
/// `launch_context` (pane_id) are written once at spawn under the launch name
/// (`src/pty/mod.rs`); without migrating them, `hcom kill <canonical>` finds no pid
/// and can't close the terminal pane.
fn migrate_placeholder_runtime_state(
    db: &HcomDb,
    canonical_name: &str,
    placeholder_data: Option<&InstanceRow>,
) {
    let Some(ph) = placeholder_data else {
        return;
    };
    if let Some(pid) = ph.pid
        && let Ok(pid_u32) = u32::try_from(pid)
        && let Err(e) = db.update_instance_pid(canonical_name, pid_u32)
    {
        crate::log::log_error("binding", "placeholder.migrate_pid", &format!("{e}"));
    }
    if let Some(ref ctx) = ph.launch_context
        && let Err(e) = db.store_launch_context(canonical_name, ctx)
    {
        crate::log::log_error(
            "binding",
            "placeholder.migrate_launch_context",
            &format!("{e}"),
        );
    }
}

fn delete_true_placeholder_if_migrated(
    db: &HcomDb,
    placeholder_name: &str,
    canonical_name: &str,
    placeholder_data: Option<&InstanceRow>,
) {
    if is_true_launch_placeholder(placeholder_data) {
        // Move pid/launch_context to the canonical row before dropping the placeholder
        // so the restored agent stays killable and its pane closeable.
        migrate_placeholder_runtime_state(db, canonical_name, placeholder_data);
        delete_true_placeholder_instance(db, placeholder_name);
    }
}

/// Path 2: after restore_stopped bind, merge notify ports and drop the launch placeholder.
fn retire_true_placeholder_after_canonical_bind(
    db: &HcomDb,
    placeholder_name: Option<&String>,
    canonical_name: &str,
    placeholder_data: Option<&InstanceRow>,
) {
    let Some(ph_name) = placeholder_name else {
        return;
    };
    if ph_name == canonical_name {
        return;
    }

    if !migrate_placeholder_notify(db, ph_name, canonical_name) {
        return;
    }

    delete_true_placeholder_if_migrated(db, ph_name, canonical_name, placeholder_data);
}

/// Recreate a missing instance row from an active placeholder (resume after stop/kill).
fn recreate_instance_from_placeholder(
    db: &HcomDb,
    target_name: &str,
    session_id: &str,
    ph: Option<&InstanceRow>,
) {
    if db.get_instance_full(target_name).ok().flatten().is_some() {
        return;
    }
    let Some(ph) = ph else {
        return;
    };
    initialize_instance_in_position_file(
        db,
        target_name,
        Some(session_id),
        ph.parent_session_id.as_deref(),
        ph.parent_name.as_deref(),
        ph.agent_id.as_deref(),
        (!ph.transcript_path.is_empty()).then_some(ph.transcript_path.as_str()),
        Some(ph.tool.as_str()),
        ph.background != 0,
        ph.tag.as_deref(),
        None,
        None,
        ph.hints.as_deref(),
        Some(ph.directory.as_str()),
    );
}

/// Bind session_id to canonical instance for process_id.
/// Handles 4 paths: canonical exists (with placeholder merge/switch), placeholder bind,
/// and two no-op paths.
pub fn bind_session_to_process(
    db: &HcomDb,
    session_id: &str,
    process_id: Option<&str>,
) -> Option<String> {
    if session_id.is_empty() {
        crate::log::log_info("binding", "bind_session_to_process.no_session_id", "");
        return None;
    }

    crate::log::log_info(
        "binding",
        "bind_session_to_process.entry",
        &format!("session_id={}, process_id={:?}", session_id, process_id),
    );

    // Find placeholder from process binding
    let (placeholder_name, placeholder_data) = if let Some(pid) = process_id {
        match db.get_process_binding(pid) {
            Ok(Some(name)) => {
                let data = match db.get_instance_full(&name) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("[hcom] warn: get_instance_full failed for {name}: {e}");
                        None
                    }
                };
                (Some(name), data)
            }
            _ => (None, None),
        }
    } else {
        (None, None)
    };

    // Find canonical from session binding
    let canonical = match db.get_session_binding(session_id) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[hcom] warn: get_session_binding failed for {session_id}: {e}");
            None
        }
    };

    // Path 1: Canonical exists (session already bound)
    if let Some(ref canonical_name) = canonical {
        crate::log::log_info(
            "binding",
            "bind_session_to_process.canonical_exists",
            &format!(
                "canonical={}, placeholder={:?}",
                canonical_name, placeholder_name
            ),
        );

        recreate_instance_from_placeholder(
            db,
            canonical_name,
            session_id,
            placeholder_data.as_ref(),
        );

        // Reset last_stop on resume
        let now = now_epoch_i64();
        let mut resume_updates = serde_json::Map::new();
        resume_updates.insert("last_stop".into(), serde_json::json!(now));

        if let Some(ref ph_name) = placeholder_name
            && ph_name != canonical_name
        {
            let migrated = migrate_placeholder_notify(db, ph_name, canonical_name);

            if is_true_launch_placeholder(placeholder_data.as_ref()) {
                // Path 1a: True placeholder merge
                if let Some(ref ph_data) = placeholder_data {
                    if let Some(ref tag) = ph_data.tag {
                        resume_updates.insert("tag".into(), serde_json::json!(tag));
                    }
                    if ph_data.background != 0 {
                        resume_updates
                            .insert("background".into(), serde_json::json!(ph_data.background));
                    }
                    if let Some(ref args) = ph_data.launch_args {
                        resume_updates.insert("launch_args".into(), serde_json::json!(args));
                    }
                    // Reset status_context for ready event
                    if std::env::var("HCOM_LAUNCHED").as_deref() == Ok("1") {
                        resume_updates.insert("status_context".into(), serde_json::json!("new"));
                    }
                }

                if migrated {
                    delete_true_placeholder_if_migrated(
                        db,
                        ph_name,
                        canonical_name,
                        placeholder_data.as_ref(),
                    );
                }
            } else {
                // Path 1b: Session switch — retire the real old identity. Unlike a true
                // launch placeholder (deletion above stays gated on endpoint migration),
                // this is a live old identity: retire it regardless of migration outcome,
                // else a migrate failure leaves a duplicate active/listening row and a
                // stale session binding. Endpoints may remain imperfect on the old name,
                // but the delivery loop re-registers under the canonical name.
                if !migrated {
                    crate::log::log_info(
                        "binding",
                        "bind_canonical.session_switch_migrate_failed",
                        &format!("endpoints may remain on {ph_name}; retiring identity anyway"),
                    );
                }
                crate::instance_lifecycle::set_status(
                    db,
                    ph_name,
                    ST_INACTIVE,
                    "exit:session_switch",
                    Default::default(),
                );
                if let Err(e) = db.delete_session_bindings_for_instance(ph_name) {
                    crate::log::log_error(
                        "binding",
                        "bind_canonical.delete_session_bindings",
                        &format!("{e}"),
                    );
                }
            }
        }

        update_instance_position(db, canonical_name, &resume_updates);

        if let Some(pid) = process_id
            && let Err(e) = db.set_process_binding(pid, session_id, canonical_name)
        {
            crate::log::log_error(
                "binding",
                "bind_canonical.set_process_binding",
                &format!("{e}"),
            );
        }

        return Some(canonical_name.clone());
    }

    // Path 2: session_bindings CASCADE'd on delete — recover canonical name from life.stopped
    if canonical.is_none()
        && let Ok(Some(stopped_name)) = db.find_stopped_instance_by_session_id(session_id)
    {
        crate::log::log_info(
            "binding",
            "bind_session_to_process.restore_stopped",
            &format!("stopped_name={stopped_name}, session_id={session_id}"),
        );
        recreate_instance_from_placeholder(
            db,
            &stopped_name,
            session_id,
            placeholder_data.as_ref(),
        );

        if let Err(e) = db.clear_session_id_from_other_instances(session_id, &stopped_name) {
            crate::log::log_error("binding", "restore_stopped.clear_session", &format!("{e}"));
        }
        let mut updates = serde_json::Map::new();
        updates.insert("session_id".into(), serde_json::json!(session_id));
        update_instance_position(db, &stopped_name, &updates);
        if let Err(e) = db.rebind_session(session_id, &stopped_name) {
            crate::log::log_error("binding", "restore_stopped.rebind_session", &format!("{e}"));
        }
        if let Some(pid) = process_id
            && let Err(e) = db.set_process_binding(pid, session_id, &stopped_name)
        {
            crate::log::log_error(
                "binding",
                "restore_stopped.set_process_binding",
                &format!("{e}"),
            );
        }

        retire_true_placeholder_after_canonical_bind(
            db,
            placeholder_name.as_ref(),
            &stopped_name,
            placeholder_data.as_ref(),
        );

        return Some(stopped_name);
    }

    // Path 3: No canonical, but placeholder exists — bind session to placeholder
    if let Some(ref ph_name) = placeholder_name {
        crate::log::log_info(
            "binding",
            "bind_session_to_process.bind_placeholder",
            &format!("placeholder={}, session_id={}", ph_name, session_id),
        );

        if let Err(e) = db.clear_session_id_from_other_instances(session_id, ph_name) {
            crate::log::log_error("binding", "bind_placeholder.clear_session", &format!("{e}"));
        }

        let mut updates = serde_json::Map::new();
        updates.insert("session_id".into(), serde_json::json!(session_id));
        update_instance_position(db, ph_name, &updates);

        if let Err(e) = db.rebind_session(session_id, ph_name) {
            crate::log::log_error(
                "binding",
                "bind_placeholder.rebind_session",
                &format!("{e}"),
            );
        }
        if let Some(pid) = process_id
            && let Err(e) = db.set_process_binding(pid, session_id, ph_name)
        {
            crate::log::log_error(
                "binding",
                "bind_placeholder.set_process_binding",
                &format!("{e}"),
            );
        }

        return Some(ph_name.clone());
    }

    crate::log::log_info("binding", "bind_session_to_process.return_none", "");
    None
}

/// Initialize the DB row and default bindings for an instance identity.
///
/// This is the shared setup path used by launch, resume, and orphan recovery.
#[allow(clippy::too_many_arguments)]
pub fn initialize_instance_in_position_file(
    db: &HcomDb,
    instance_name: &str,
    session_id: Option<&str>,
    parent_session_id: Option<&str>,
    parent_name: Option<&str>,
    agent_id: Option<&str>,
    transcript_path: Option<&str>,
    tool: Option<&str>,
    background: bool,
    tag: Option<&str>,
    wait_timeout: Option<i64>,
    subagent_timeout: Option<i64>,
    hints: Option<&str>,
    cwd_override: Option<&str>,
) -> bool {
    let cwd = cwd_override.map(|s| s.to_string()).unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    let is_launched = std::env::var("HCOM_LAUNCHED").as_deref() == Ok("1");

    match db.get_instance_full(instance_name) {
        Ok(Some(existing)) => {
            let mut updates = serde_json::Map::new();
            updates.insert("directory".into(), serde_json::json!(cwd));

            if let Some(sid) = session_id {
                updates.insert("session_id".into(), serde_json::json!(sid));
            }
            if let Some(psid) = parent_session_id {
                updates.insert("parent_session_id".into(), serde_json::json!(psid));
            }
            if let Some(pn) = parent_name {
                updates.insert("parent_name".into(), serde_json::json!(pn));
            }
            if let Some(aid) = agent_id {
                updates.insert("agent_id".into(), serde_json::json!(aid));
            }
            if let Some(tp) = transcript_path {
                updates.insert("transcript_path".into(), serde_json::json!(tp));
            }
            if let Some(t) = tool {
                updates.insert("tool".into(), serde_json::json!(t));
            }
            if let Some(t) = tag {
                updates.insert("tag".into(), serde_json::json!(t));
            }
            if background {
                updates.insert("background".into(), serde_json::json!(1));
            }

            let is_true_placeholder = existing.session_id.is_none();
            let is_pending_placeholder = is_true_placeholder
                && existing.status == PLACEHOLDER_STATUS
                && existing.status_context == PLACEHOLDER_CONTEXT;
            if existing.last_event_id == 0 && is_true_placeholder {
                let current_max = db.get_last_event_id();
                let launch_event_id = std::env::var("HCOM_LAUNCH_EVENT_ID")
                    .ok()
                    .and_then(|s| s.parse::<i64>().ok());

                let eid = match launch_event_id {
                    Some(id) if id <= current_max => id,
                    _ => current_max,
                };
                updates.insert("last_event_id".into(), serde_json::json!(eid));
            }

            if is_launched {
                updates.insert("status_context".into(), serde_json::json!("new"));
            }

            if !updates.is_empty() {
                let _ = db.update_instance_fields(instance_name, &updates);
            }

            if is_pending_placeholder {
                auto_subscribe_defaults(db, instance_name, tool.unwrap_or(existing.tool.as_str()));
            }

            true
        }
        Ok(None) => {
            let now = now_epoch_f64();
            let current_max = db.get_last_event_id();
            let launch_event_id = std::env::var("HCOM_LAUNCH_EVENT_ID")
                .ok()
                .and_then(|s| s.parse::<i64>().ok());

            let initial_event_id = match launch_event_id {
                Some(id) if id <= current_max => id,
                _ => current_max,
            };

            let mut data = serde_json::Map::new();
            data.insert("name".into(), serde_json::json!(instance_name));
            data.insert("last_event_id".into(), serde_json::json!(initial_event_id));
            data.insert("directory".into(), serde_json::json!(cwd));
            data.insert("last_stop".into(), serde_json::json!(0));
            data.insert("created_at".into(), serde_json::json!(now));
            data.insert(
                "session_id".into(),
                match session_id {
                    Some(s) if !s.is_empty() => serde_json::json!(s),
                    _ => serde_json::Value::Null,
                },
            );
            data.insert("transcript_path".into(), serde_json::json!(""));
            data.insert("name_announced".into(), serde_json::json!(0));
            data.insert("tag".into(), serde_json::Value::Null);
            data.insert("status".into(), serde_json::json!(ST_INACTIVE));
            data.insert("status_time".into(), serde_json::json!(now_epoch_i64()));
            data.insert("status_context".into(), serde_json::json!("new"));
            data.insert("tool".into(), serde_json::json!(tool.unwrap_or("claude")));
            data.insert(
                "background".into(),
                serde_json::json!(if background { 1 } else { 0 }),
            );

            if let Some(t) = tag {
                data.insert("tag".into(), serde_json::json!(t));
            } else if (session_id.is_some() || parent_session_id.is_some() || is_launched)
                && let Ok(hcom_config) = crate::config::HcomConfig::load(None)
                && !hcom_config.tag.is_empty()
            {
                data.insert("tag".into(), serde_json::json!(hcom_config.tag));
            }

            // Resolve HCOM_TIMEOUT explicitly rather than leaving the column
            // unset — the schema's DEFAULT 86400 would otherwise silently
            // mask the configured value for every non-PTY instance (issue #71).
            let effective_wait_timeout =
                wait_timeout.unwrap_or_else(crate::config::HcomConfig::effective_timeout);
            data.insert(
                "wait_timeout".into(),
                serde_json::json!(effective_wait_timeout),
            );
            if let Some(st) = subagent_timeout {
                data.insert("subagent_timeout".into(), serde_json::json!(st));
            }
            if let Some(h) = hints {
                data.insert("hints".into(), serde_json::json!(h));
            }
            if let Some(psid) = parent_session_id {
                data.insert("parent_session_id".into(), serde_json::json!(psid));
            }
            if let Some(pn) = parent_name {
                data.insert("parent_name".into(), serde_json::json!(pn));
            }
            if let Some(aid) = agent_id {
                data.insert("agent_id".into(), serde_json::json!(aid));
            }
            if let Some(tp) = transcript_path {
                data.insert("transcript_path".into(), serde_json::json!(tp));
            }

            match db.save_instance_named(instance_name, &data) {
                Ok(true) => {
                    log_created_and_auto_subscribe(
                        db,
                        instance_name,
                        is_launched,
                        parent_session_id,
                        parent_name,
                        tool.unwrap_or(""),
                    );
                    true
                }
                _ => true,
            }
        }
        Err(_) => false,
    }
}

fn log_created_and_auto_subscribe(
    db: &HcomDb,
    instance_name: &str,
    is_launched: bool,
    parent_session_id: Option<&str>,
    parent_name: Option<&str>,
    tool: &str,
) {
    let launcher = std::env::var("HCOM_LAUNCHED_BY").unwrap_or_else(|_| "unknown".to_string());
    let event_data = serde_json::json!({
        "action": "created",
        "by": launcher,
        "is_hcom_launched": is_launched,
        "is_subagent": parent_session_id.is_some(),
        "parent_name": parent_name.unwrap_or(""),
    });
    let _ = db.log_event("life", instance_name, &event_data);
    auto_subscribe_defaults(db, instance_name, tool);
}

/// Create orphaned PTY identity — called when process binding exists but session_id
/// is fresh (e.g., after /clear). Generates new name, creates instance, binds it.
pub fn create_orphaned_pty_identity(
    db: &HcomDb,
    session_id: &str,
    process_id: Option<&str>,
    tool: &str,
) -> Option<String> {
    let name = match crate::instance_names::generate_unique_name(db) {
        Ok(n) => n,
        Err(e) => {
            crate::log::log_error(
                "instances",
                "create_orphaned_pty_identity.name_gen",
                &e.to_string(),
            );
            return None;
        }
    };

    let success = initialize_instance_in_position_file(
        db,
        &name,
        Some(session_id),
        None,
        None,
        None,
        None,
        Some(tool),
        false,
        None,
        None,
        None,
        None,
        None,
    );

    if !success {
        return None;
    }

    if let Err(e) = db.rebind_session(session_id, &name) {
        eprintln!("[hcom] warn: rebind_session failed for {name}: {e}");
    }
    if let Some(pid) = process_id
        && let Err(e) = db.set_process_binding(pid, session_id, &name)
    {
        eprintln!("[hcom] warn: set_process_binding failed for {name}: {e}");
    }

    Some(name)
}

/// Resolve instance name for a process_id via process_bindings.
pub fn resolve_process_binding(db: &HcomDb, process_id: Option<&str>) -> Option<String> {
    let pid = process_id?;
    db.get_process_binding(pid).ok()?
}

/// Resolve instance via process binding, session binding, or transcript marker.
pub fn resolve_instance_from_binding(
    db: &HcomDb,
    session_id: Option<&str>,
    process_id: Option<&str>,
) -> Option<InstanceRow> {
    if let Some(pid) = process_id
        && let Ok(Some(name)) = db.get_process_binding(pid)
        && let Ok(Some(instance)) = db.get_instance_full(&name)
    {
        return Some(instance);
    }

    if let Some(sid) = session_id
        && let Some(name) = db.get_session_binding(sid).ok().flatten()
        && let Ok(Some(instance)) = db.get_instance_full(&name)
    {
        return Some(instance);
    }

    None
}

/// Auto-subscribe instance to default event subscriptions from config.
/// Called during instance creation.
fn auto_subscribe_eligible(tool: &str) -> bool {
    tool.parse::<crate::tool::Tool>()
        .is_ok_and(|tool| tool.spec().released)
}

fn auto_subscribe_defaults(db: &HcomDb, instance_name: &str, tool: &str) {
    if !auto_subscribe_eligible(tool) {
        return;
    }

    let _ = db.cleanup_subscriptions(instance_name);
    let _ = db.cleanup_thread_memberships_for_name_reuse(instance_name);
    let config = match crate::config::HcomConfig::load(None) {
        Ok(c) => c,
        Err(_) => return,
    };
    if config.auto_subscribe.is_empty() {
        return;
    }

    use std::collections::HashMap;

    let preset_to_flags: HashMap<&str, Vec<(&str, &str)>> = HashMap::from([
        ("collision", vec![("collision", "1")]),
        ("created", vec![("action", "created")]),
        ("stopped", vec![("action", "stopped")]),
        ("blocked", vec![("status", "blocked")]),
    ]);

    for preset in config
        .auto_subscribe
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if let Some(flag_pairs) = preset_to_flags.get(preset) {
            let mut filters: HashMap<String, Vec<String>> = HashMap::new();
            for (key, val) in flag_pairs {
                filters
                    .entry(key.to_string())
                    .or_default()
                    .push(val.to_string());
            }
            let _ = crate::db::subscriptions::create_filter_subscription(
                db,
                &filters,
                &[],
                instance_name,
                false,
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: tests using this guard are marked #[serial].
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: tests using this guard are marked #[serial].
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: tests using this guard are marked #[serial].
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn setup_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_instance_binding_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let db = HcomDb::open_at(&db_path).unwrap();
        (db, db_path)
    }

    fn cleanup(path: PathBuf) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn auto_subscribe_eligibility_follows_released_specs() {
        assert!(auto_subscribe_eligible("pi"));
        assert!(auto_subscribe_eligible("kimi"));
        assert!(!auto_subscribe_eligible("adhoc"));
        assert!(!auto_subscribe_eligible("unknown"));
    }

    #[test]
    fn test_persist_terminal_launch_context_stores_presets_in_launch_context() {
        let (db, path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params!["luna", "codex", 1.0f64],
            )
            .unwrap();

        persist_terminal_launch_context(&db, "luna", Some("kitty"), "kitty-tab", Some("proc-1"));

        let row = db.get_instance_full("luna").unwrap().unwrap();
        let ctx: serde_json::Value =
            serde_json::from_str(row.launch_context.as_deref().unwrap_or("{}")).unwrap();

        assert_eq!(
            ctx.get("terminal_preset_effective")
                .and_then(|v| v.as_str()),
            Some("kitty-tab")
        );
        assert_eq!(
            ctx.get("terminal_preset").and_then(|v| v.as_str()),
            Some("kitty-tab")
        );
        assert_eq!(
            ctx.get("terminal_preset_requested")
                .and_then(|v| v.as_str()),
            Some("kitty")
        );
        assert_eq!(
            ctx.get("process_id").and_then(|v| v.as_str()),
            Some("proc-1")
        );

        cleanup(path);
    }

    #[test]
    #[serial]
    fn test_capture_context_records_launched_preset() {
        // capture_context tags the launch with HCOM_LAUNCHED_PRESET so later
        // child agents launched from inside this pane can inherit the preset.
        // Running tests inside a herdr session would otherwise leak
        // HERDR_PANE_ID into the captured context, so explicitly clear the
        // herdr-related identity vars before exercising the capture path.
        crate::config::Config::init();
        let _preset = EnvVarGuard::set("HCOM_LAUNCHED_PRESET", "herdr");
        let _herdr_pane = EnvVarGuard::set("HERDR_PANE_ID", "");
        let _herdr_socket = EnvVarGuard::set("HERDR_SOCKET_PATH", "");
        let _herdr_env = EnvVarGuard::set("HERDR_ENV", "");
        let _process_id = EnvVarGuard::set("HCOM_PROCESS_ID", "");

        let ctx = capture_context();

        assert_eq!(
            ctx.get("terminal_preset_effective")
                .and_then(|v| v.as_str()),
            Some("herdr")
        );
        assert!(
            ctx.get("pane_id").is_none(),
            "pane_id should be absent when HERDR_PANE_ID isn't set"
        );
    }

    #[test]
    #[serial]
    fn test_capture_and_store_launch_context_preserves_terminal_metadata() {
        // Preserve only the fields we can't recapture from hook env:
        // pane_id, terminal_id, kitty_listen_on, process_id, and the resolved
        // terminal preset name.
        let (db, path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, created_at, launch_context) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "luna",
                    "claude",
                    1.0f64,
                    r#"{"terminal_preset_effective":"herdr","pane_id":"p_7","process_id":"proc-1"}"#
                ],
            )
            .unwrap();
        let _preset = EnvVarGuard::set("HCOM_LAUNCHED_PRESET", "");
        let _process_id = EnvVarGuard::set("HCOM_PROCESS_ID", "");

        capture_and_store_launch_context(&db, "luna");

        let row = db.get_instance_full("luna").unwrap().unwrap();
        let ctx: serde_json::Value =
            serde_json::from_str(row.launch_context.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(
            ctx.get("terminal_preset_effective")
                .and_then(|v| v.as_str()),
            Some("herdr")
        );
        assert_eq!(ctx.get("pane_id").and_then(|v| v.as_str()), Some("p_7"));
        assert_eq!(
            ctx.get("process_id").and_then(|v| v.as_str()),
            Some("proc-1")
        );

        cleanup(path);
    }

    #[test]
    fn test_bind_session_path2_placeholder() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        db.save_instance_named("luna", &data).unwrap();

        db.set_process_binding("pid-123", "", "luna").unwrap();

        let result = bind_session_to_process(&db, "sid-456", Some("pid-123"));
        assert_eq!(result, Some("luna".to_string()));

        let inst = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sid-456"));

        let binding = db.get_session_binding("sid-456").unwrap();
        assert_eq!(binding, Some("luna".to_string()));

        cleanup(path);
    }

    #[test]
    fn test_bind_session_path1a_true_placeholder_merge() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut canonical_data = serde_json::Map::new();
        canonical_data.insert("name".into(), serde_json::json!("miso"));
        canonical_data.insert("session_id".into(), serde_json::json!("sid-789"));
        canonical_data.insert("created_at".into(), serde_json::json!(now));
        canonical_data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("miso", &canonical_data).unwrap();
        db.rebind_session("sid-789", "miso").unwrap();

        let mut ph_data = serde_json::Map::new();
        ph_data.insert("name".into(), serde_json::json!("temp"));
        ph_data.insert("tag".into(), serde_json::json!("team"));
        ph_data.insert("created_at".into(), serde_json::json!(now));
        ph_data.insert("status".into(), serde_json::json!("pending"));
        ph_data.insert("status_context".into(), serde_json::json!("new"));
        db.save_instance_named("temp", &ph_data).unwrap();

        db.set_process_binding("pid-123", "", "temp").unwrap();

        let result = bind_session_to_process(&db, "sid-789", Some("pid-123"));
        assert_eq!(result, Some("miso".to_string()));

        assert!(db.get_instance_full("temp").unwrap().is_none());

        let inst = db.get_instance_full("miso").unwrap().unwrap();
        assert_eq!(inst.tag.as_deref(), Some("team"));

        cleanup(path);
    }

    #[test]
    fn test_bind_session_path1b_session_switch_marks_old_inactive() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut canonical_data = serde_json::Map::new();
        canonical_data.insert("name".into(), serde_json::json!("miso"));
        canonical_data.insert("session_id".into(), serde_json::json!("sid-789"));
        canonical_data.insert("created_at".into(), serde_json::json!(now));
        canonical_data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("miso", &canonical_data).unwrap();
        db.rebind_session("sid-789", "miso").unwrap();

        let mut ph_data = serde_json::Map::new();
        ph_data.insert("name".into(), serde_json::json!("temp"));
        ph_data.insert("session_id".into(), serde_json::json!("sid-old"));
        ph_data.insert("created_at".into(), serde_json::json!(now));
        ph_data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("temp", &ph_data).unwrap();
        db.rebind_session("sid-old", "temp").unwrap();
        db.set_process_binding("pid-123", "sid-old", "temp")
            .unwrap();

        let result = bind_session_to_process(&db, "sid-789", Some("pid-123"));
        assert_eq!(result, Some("miso".to_string()));

        let placeholder = db.get_instance_full("temp").unwrap().unwrap();
        assert_eq!(placeholder.status, ST_INACTIVE);
        assert_eq!(placeholder.status_context, "exit:session_switch");

        assert_eq!(db.get_session_binding("sid-old").unwrap(), None);
        assert_eq!(
            db.get_process_binding("pid-123").unwrap(),
            Some("miso".to_string())
        );

        cleanup(path);
    }

    #[test]
    fn test_bind_session_no_match() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let result = bind_session_to_process(&db, "sid-999", None);
        assert_eq!(result, None);

        cleanup(path);
    }

    #[test]
    fn test_create_orphaned_pty_identity_basic() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let result = create_orphaned_pty_identity(&db, "sess-orphan", Some("pid-orphan"), "claude");
        assert!(result.is_some(), "should create orphaned identity");

        let name = result.unwrap();
        let inst = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sess-orphan"));
        assert_eq!(inst.tool, "claude");

        assert_eq!(
            db.get_session_binding("sess-orphan").unwrap(),
            Some(name.clone())
        );
        assert_eq!(db.get_process_binding("pid-orphan").unwrap(), Some(name));

        cleanup(path);
    }

    #[test]
    fn test_create_orphaned_pty_identity_no_process_id() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let result = create_orphaned_pty_identity(&db, "sess-orphan2", None, "gemini");
        assert!(result.is_some());

        let name = result.unwrap();
        let inst = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(inst.tool, "gemini");
        assert_eq!(db.get_session_binding("sess-orphan2").unwrap(), Some(name));

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_process_binding() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("session_id".into(), serde_json::json!("sess-1"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("luna", &data).unwrap();
        db.set_process_binding("pid-1", "sess-1", "luna").unwrap();

        let result = resolve_instance_from_binding(&db, None, Some("pid-1"));
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "luna");

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_session_binding() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("nova"));
        data.insert("session_id".into(), serde_json::json!("sess-2"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("status".into(), serde_json::json!("active"));
        db.save_instance_named("nova", &data).unwrap();
        db.rebind_session("sess-2", "nova").unwrap();

        let result = resolve_instance_from_binding(&db, Some("sess-2"), None);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "nova");

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_process_over_session() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut d1 = serde_json::Map::new();
        d1.insert("name".into(), serde_json::json!("luna"));
        d1.insert("created_at".into(), serde_json::json!(now));
        d1.insert("status".into(), serde_json::json!("active"));
        db.save_instance_named("luna", &d1).unwrap();
        db.set_process_binding("pid-1", "", "luna").unwrap();

        let mut d2 = serde_json::Map::new();
        d2.insert("name".into(), serde_json::json!("nova"));
        d2.insert("session_id".into(), serde_json::json!("sess-2"));
        d2.insert("created_at".into(), serde_json::json!(now));
        d2.insert("status".into(), serde_json::json!("active"));
        db.save_instance_named("nova", &d2).unwrap();
        db.rebind_session("sess-2", "nova").unwrap();

        let result = resolve_instance_from_binding(&db, Some("sess-2"), Some("pid-1"));
        assert_eq!(result.unwrap().name, "luna");

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_process_binding_instance_deleted() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        db.set_process_binding("pid-ghost", "", "ghost").unwrap();

        let result = resolve_instance_from_binding(&db, None, Some("pid-ghost"));
        assert!(result.is_none());

        cleanup(path);
    }

    #[test]
    fn test_resolve_from_binding_no_match() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let result = resolve_instance_from_binding(&db, Some("nonexistent"), Some("nope"));
        assert!(result.is_none());

        cleanup(path);
    }

    #[test]
    fn test_session_binding_cascade_on_instance_delete() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("session_id".into(), serde_json::json!("sess-1"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("status".into(), serde_json::json!("active"));
        db.save_instance_named("luna", &data).unwrap();
        db.rebind_session("sess-1", "luna").unwrap();

        assert_eq!(
            db.get_session_binding("sess-1").unwrap(),
            Some("luna".to_string())
        );

        db.delete_instance("luna").unwrap();
        assert_eq!(db.get_session_binding("sess-1").unwrap(), None);

        cleanup(path);
    }

    #[test]
    fn test_bind_session_restores_deleted_canonical_from_placeholder() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut canonical_data = serde_json::Map::new();
        canonical_data.insert("name".into(), serde_json::json!("miso"));
        canonical_data.insert("session_id".into(), serde_json::json!("sid-resume"));
        canonical_data.insert("tool".into(), serde_json::json!("antigravity"));
        canonical_data.insert("created_at".into(), serde_json::json!(now));
        canonical_data.insert("status".into(), serde_json::json!("listening"));
        db.save_instance_named("miso", &canonical_data).unwrap();
        db.rebind_session("sid-resume", "miso").unwrap();

        let mut ph_data = serde_json::Map::new();
        ph_data.insert("name".into(), serde_json::json!("nova"));
        ph_data.insert("tool".into(), serde_json::json!("antigravity"));
        ph_data.insert("tag".into(), serde_json::json!("work"));
        ph_data.insert("created_at".into(), serde_json::json!(now));
        ph_data.insert("status".into(), serde_json::json!("pending"));
        ph_data.insert("status_context".into(), serde_json::json!("new"));
        db.save_instance_named("nova", &ph_data).unwrap();
        db.set_process_binding("pid-agy", "", "nova").unwrap();

        let snapshot = serde_json::json!({
            "session_id": "sid-resume",
            "tool": "antigravity",
            "tag": "work",
        });
        db.log_life_event("miso", "stopped", "test", "exit", Some(snapshot))
            .unwrap();

        db.delete_instance("miso").unwrap();
        assert!(db.get_instance_full("miso").unwrap().is_none());
        assert_eq!(db.get_session_binding("sid-resume").unwrap(), None);

        let result = bind_session_to_process(&db, "sid-resume", Some("pid-agy"));
        assert_eq!(result, Some("miso".to_string()));

        let restored = db.get_instance_full("miso").unwrap().unwrap();
        assert_eq!(restored.session_id.as_deref(), Some("sid-resume"));
        assert_eq!(restored.tool, "antigravity");
        assert_eq!(restored.tag.as_deref(), Some("work"));
        assert!(db.get_instance_full("nova").unwrap().is_none());

        cleanup(path);
    }

    #[test]
    #[serial]
    fn test_bind_session_restore_stopped_deletes_true_placeholder() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut fano_data = serde_json::Map::new();
        fano_data.insert("name".into(), serde_json::json!("fano"));
        fano_data.insert("tool".into(), serde_json::json!("opencode"));
        fano_data.insert("created_at".into(), serde_json::json!(now));
        fano_data.insert("status".into(), serde_json::json!("inactive"));
        db.save_instance_named("fano", &fano_data).unwrap();

        let snapshot = serde_json::json!({
            "session_id": "ses-opencode-1",
            "tool": "opencode",
        });
        db.log_life_event("fano", "stopped", "test", "exit", Some(snapshot))
            .unwrap();

        let mut mozi_data = serde_json::Map::new();
        mozi_data.insert("name".into(), serde_json::json!("mozi"));
        mozi_data.insert("tool".into(), serde_json::json!("opencode"));
        mozi_data.insert("created_at".into(), serde_json::json!(now));
        mozi_data.insert("status".into(), serde_json::json!("pending"));
        mozi_data.insert("status_context".into(), serde_json::json!("new"));
        db.save_instance_named("mozi", &mozi_data).unwrap();
        db.set_process_binding("pid-oc", "", "mozi").unwrap();

        db.upsert_notify_endpoint("mozi", "pty", 55_568).unwrap();
        db.upsert_notify_endpoint("fano", "plugin", 58_898).unwrap();

        let result = bind_session_to_process(&db, "ses-opencode-1", Some("pid-oc"));
        assert_eq!(result, Some("fano".to_string()));

        assert!(db.get_instance_full("mozi").unwrap().is_none());
        let fano = db.get_instance_full("fano").unwrap().unwrap();
        assert_eq!(fano.session_id.as_deref(), Some("ses-opencode-1"));
        assert_eq!(
            db.get_session_binding("ses-opencode-1").unwrap(),
            Some("fano".to_string())
        );
        assert_eq!(
            db.get_process_binding("pid-oc").unwrap(),
            Some("fano".to_string())
        );

        cleanup(path);
    }

    #[test]
    #[serial]
    fn test_restore_stopped_migrates_pid_and_launch_context_to_canonical() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut fano_data = serde_json::Map::new();
        fano_data.insert("name".into(), serde_json::json!("fano"));
        fano_data.insert("tool".into(), serde_json::json!("opencode"));
        fano_data.insert("created_at".into(), serde_json::json!(now));
        fano_data.insert("status".into(), serde_json::json!("inactive"));
        db.save_instance_named("fano", &fano_data).unwrap();
        db.log_life_event(
            "fano",
            "stopped",
            "test",
            "exit",
            Some(serde_json::json!({ "session_id": "ses-oc-pid", "tool": "opencode" })),
        )
        .unwrap();

        // Launch placeholder with the runtime state the PTY wrapper writes at spawn.
        let mut mozi_data = serde_json::Map::new();
        mozi_data.insert("name".into(), serde_json::json!("mozi"));
        mozi_data.insert("tool".into(), serde_json::json!("opencode"));
        mozi_data.insert("created_at".into(), serde_json::json!(now));
        mozi_data.insert("status".into(), serde_json::json!("pending"));
        mozi_data.insert("status_context".into(), serde_json::json!("new"));
        db.save_instance_named("mozi", &mozi_data).unwrap();
        db.set_process_binding("pid-oc", "", "mozi").unwrap();
        db.update_instance_pid("mozi", 4242).unwrap();
        db.store_launch_context("mozi", r#"{"pane_id":"kitty-99"}"#)
            .unwrap();

        let result = bind_session_to_process(&db, "ses-oc-pid", Some("pid-oc"));
        assert_eq!(result, Some("fano".to_string()));

        // Placeholder gone; pid + launch_context now live on the canonical so the
        // restored agent stays killable and its pane closeable.
        assert!(db.get_instance_full("mozi").unwrap().is_none());
        let fano = db.get_instance_full("fano").unwrap().unwrap();
        assert_eq!(fano.pid, Some(4242));
        assert!(
            fano.launch_context
                .as_deref()
                .unwrap_or_default()
                .contains("kitty-99"),
            "launch_context not migrated: {:?}",
            fano.launch_context
        );

        cleanup(path);
    }

    #[test]
    #[serial]
    fn test_restore_stopped_migrates_ready_promoted_placeholder_runtime_state() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut fano_data = serde_json::Map::new();
        fano_data.insert("name".into(), serde_json::json!("fano"));
        fano_data.insert("tool".into(), serde_json::json!("opencode"));
        fano_data.insert("created_at".into(), serde_json::json!(now));
        fano_data.insert("status".into(), serde_json::json!("inactive"));
        db.save_instance_named("fano", &fano_data).unwrap();
        db.log_life_event(
            "fano",
            "stopped",
            "test",
            "exit",
            Some(serde_json::json!({ "session_id": "ses-oc-ready", "tool": "opencode" })),
        )
        .unwrap();

        // PTY ready detection can promote the launch row before opencode-start binds
        // the session. It is still the launch placeholder and must be retired.
        let mut mozi_data = serde_json::Map::new();
        mozi_data.insert("name".into(), serde_json::json!("mozi"));
        mozi_data.insert("tool".into(), serde_json::json!("opencode"));
        mozi_data.insert("created_at".into(), serde_json::json!(now));
        mozi_data.insert("status".into(), serde_json::json!("listening"));
        mozi_data.insert("status_context".into(), serde_json::json!("start"));
        db.save_instance_named("mozi", &mozi_data).unwrap();
        db.set_process_binding("pid-oc-ready", "", "mozi").unwrap();
        db.update_instance_pid("mozi", 4343).unwrap();
        db.store_launch_context("mozi", r#"{"pane_id":"kitty-101"}"#)
            .unwrap();

        let result = bind_session_to_process(&db, "ses-oc-ready", Some("pid-oc-ready"));
        assert_eq!(result, Some("fano".to_string()));

        assert!(db.get_instance_full("mozi").unwrap().is_none());
        let fano = db.get_instance_full("fano").unwrap().unwrap();
        assert_eq!(fano.pid, Some(4343));
        assert!(
            fano.launch_context
                .as_deref()
                .unwrap_or_default()
                .contains("kitty-101"),
            "launch_context not migrated: {:?}",
            fano.launch_context
        );

        cleanup(path);
    }

    #[test]
    fn test_restore_stopped_keeps_active_no_session_row() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut fano_data = serde_json::Map::new();
        fano_data.insert("name".into(), serde_json::json!("fano"));
        fano_data.insert("tool".into(), serde_json::json!("opencode"));
        fano_data.insert("created_at".into(), serde_json::json!(now));
        fano_data.insert("status".into(), serde_json::json!("inactive"));
        db.save_instance_named("fano", &fano_data).unwrap();
        db.log_life_event(
            "fano",
            "stopped",
            "test",
            "exit",
            Some(serde_json::json!({ "session_id": "ses-keep", "tool": "opencode" })),
        )
        .unwrap();

        // An ACTIVE row bound to the pid that happens to lack a session_id — NOT a launch
        // placeholder (status_context != "new", status active). It must not be deleted.
        let mut busy_data = serde_json::Map::new();
        busy_data.insert("name".into(), serde_json::json!("busy"));
        busy_data.insert("tool".into(), serde_json::json!("opencode"));
        busy_data.insert("created_at".into(), serde_json::json!(now));
        busy_data.insert("status".into(), serde_json::json!("active"));
        busy_data.insert("status_context".into(), serde_json::json!("tool:write"));
        db.save_instance_named("busy", &busy_data).unwrap();
        db.set_process_binding("pid-busy", "", "busy").unwrap();

        let result = bind_session_to_process(&db, "ses-keep", Some("pid-busy"));
        assert_eq!(result, Some("fano".to_string()));

        assert!(
            db.get_instance_full("busy").unwrap().is_some(),
            "active non-placeholder row must not be deleted by restore_stopped"
        );

        cleanup(path);
    }

    fn notify_endpoint_port(db: &HcomDb, instance: &str, kind: &str) -> Option<i64> {
        db.conn()
            .query_row(
                "SELECT port FROM notify_endpoints WHERE instance = ?1 AND kind = ?2",
                rusqlite::params![instance, kind],
                |row| row.get(0),
            )
            .ok()
    }

    struct MigrateNotifyFailGuard;

    impl MigrateNotifyFailGuard {
        fn enable() -> Self {
            HcomDb::set_test_migrate_notify_fail(true);
            Self
        }
    }

    impl Drop for MigrateNotifyFailGuard {
        fn drop(&mut self) {
            HcomDb::set_test_migrate_notify_fail(false);
        }
    }

    #[test]
    fn test_delete_placeholder_failure_keeps_notify_on_canonical() {
        let (db, path) = setup_test_db();

        db.upsert_notify_endpoint("fano", "plugin", 58_898).unwrap();
        db.upsert_notify_endpoint("mozi", "pty", 55_568).unwrap();
        db.migrate_notify_endpoints("mozi", "fano").unwrap();

        delete_true_placeholder_instance(&db, "ghost_placeholder");

        assert_eq!(notify_endpoint_port(&db, "fano", "plugin"), Some(58_898));
        assert_eq!(notify_endpoint_port(&db, "fano", "pty"), Some(55_568));

        cleanup(path);
    }

    #[test]
    #[serial]
    fn test_retire_skips_delete_when_migrate_fails() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let _guard = MigrateNotifyFailGuard::enable();
        let now = now_epoch_i64();

        let mut fano_data = serde_json::Map::new();
        fano_data.insert("name".into(), serde_json::json!("fano"));
        fano_data.insert("tool".into(), serde_json::json!("opencode"));
        fano_data.insert("created_at".into(), serde_json::json!(now));
        fano_data.insert("status".into(), serde_json::json!("inactive"));
        db.save_instance_named("fano", &fano_data).unwrap();

        let snapshot = serde_json::json!({
            "session_id": "ses-opencode-1",
            "tool": "opencode",
        });
        db.log_life_event("fano", "stopped", "test", "exit", Some(snapshot))
            .unwrap();

        let mut mozi_data = serde_json::Map::new();
        mozi_data.insert("name".into(), serde_json::json!("mozi"));
        mozi_data.insert("tool".into(), serde_json::json!("opencode"));
        mozi_data.insert("created_at".into(), serde_json::json!(now));
        mozi_data.insert("status".into(), serde_json::json!("pending"));
        mozi_data.insert("status_context".into(), serde_json::json!("new"));
        db.save_instance_named("mozi", &mozi_data).unwrap();
        db.set_process_binding("pid-oc", "", "mozi").unwrap();

        let result = bind_session_to_process(&db, "ses-opencode-1", Some("pid-oc"));
        assert_eq!(result, Some("fano".to_string()));

        assert!(db.get_instance_full("mozi").unwrap().is_some());
        assert_eq!(
            db.get_session_binding("ses-opencode-1").unwrap(),
            Some("fano".to_string())
        );

        cleanup(path);
    }

    #[test]
    fn test_bind_session_idempotent_same_session() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        db.save_instance_named("luna", &data).unwrap();
        db.set_process_binding("pid-1", "", "luna").unwrap();

        let r1 = bind_session_to_process(&db, "sess-1", Some("pid-1"));
        assert_eq!(r1, Some("luna".to_string()));

        let r2 = bind_session_to_process(&db, "sess-1", Some("pid-1"));
        assert_eq!(r2, Some("luna".to_string()));

        let inst = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sess-1"));

        cleanup(path);
    }

    #[test]
    fn test_auto_subscribe_creates_collision_subscription() {
        let (db, path) = setup_test_db();

        use std::collections::HashMap;
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("collision".to_string(), vec!["1".to_string()]);

        let result = crate::db::subscriptions::create_filter_subscription(
            &db,
            &filters,
            &[],
            "test-agent",
            false,
            None,
        );
        assert!(result.is_ok(), "subscription creation should succeed");

        let rows: Vec<String> = db
            .conn()
            .prepare("SELECT key FROM kv WHERE key LIKE 'events_sub:%'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(rows.len(), 1, "should have 1 subscription");

        cleanup(path);
    }

    #[test]
    #[serial]
    fn test_pending_placeholder_promotion_auto_subscribes_without_created_event() {
        let _env = EnvVarGuard::set("HCOM_AUTO_SUBSCRIBE", "collision");
        let (db, path) = setup_test_db();

        let now = now_epoch_i64();
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("luna"));
        data.insert("status".into(), serde_json::json!(PLACEHOLDER_STATUS));
        data.insert(
            "status_context".into(),
            serde_json::json!(PLACEHOLDER_CONTEXT),
        );
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert(
            "last_event_id".into(),
            serde_json::json!(db.get_last_event_id()),
        );
        db.save_instance_named("luna", &data).unwrap();

        let ok = initialize_instance_in_position_file(
            &db,
            "luna",
            None,
            None,
            None,
            None,
            None,
            Some("codex"),
            false,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(ok);
        let ok_again = initialize_instance_in_position_file(
            &db,
            "luna",
            None,
            None,
            None,
            None,
            None,
            Some("codex"),
            false,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(ok_again);

        let created_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE instance = 'luna'
                   AND type = 'life'
                   AND json_extract(data, '$.action') = 'created'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created_count, 0);

        let collision_sub_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM kv
                 WHERE key LIKE 'events_sub:%'
                   AND json_extract(value, '$.caller') = 'luna'
                   AND json_extract(value, '$.filters.collision[0]') IS NOT NULL
                   AND COALESCE(json_extract(value, '$.delivery_only'), 0) != 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(collision_sub_count, 1);

        cleanup(path);
    }

    #[test]
    #[serial]
    fn new_row_honors_configured_hcom_timeout() {
        // Regression test for issue #71: a brand-new instance row (the path
        // used by vanilla `hcom start`, launched, and resumed sessions) must
        // carry the effective HCOM_TIMEOUT rather than silently falling back
        // to the old always-86400 schema default.
        let _env = EnvVarGuard::set("HCOM_TIMEOUT", "30");
        let (db, path) = setup_test_db();

        let ok = initialize_instance_in_position_file(
            &db, "luna", None, None, None, None, None, Some("claude"), false, None, None, None,
            None, None,
        );
        assert!(ok);

        let row = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(row.wait_timeout, Some(30));

        cleanup(path);
    }

    #[test]
    #[serial]
    fn new_row_falls_back_to_120_without_config() {
        let _env = EnvVarGuard::unset("HCOM_TIMEOUT");
        let (db, path) = setup_test_db();

        let ok = initialize_instance_in_position_file(
            &db, "luna", None, None, None, None, None, Some("claude"), false, None, None, None,
            None, None,
        );
        assert!(ok);

        let row = db.get_instance_full("luna").unwrap().unwrap();
        // Default HcomConfig::timeout is 86400 (schema-equivalent default),
        // preserved for anyone who hasn't set HCOM_TIMEOUT.
        assert_eq!(row.wait_timeout, Some(86400));

        cleanup(path);
    }
}
