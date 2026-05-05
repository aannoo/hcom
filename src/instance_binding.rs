//! Launch-context capture plus process/session binding for instance records.
//!
//! This module owns the hook-facing identity handshake:
//! launch metadata capture, placeholder/canonical binding, and instance-row
//! initialization for newly launched or recovered sessions.

use crate::db::{HcomDb, InstanceRow};
use crate::instances::update_instance_position;
use crate::shared::ST_INACTIVE;
use crate::shared::time::{now_epoch_f64, now_epoch_i64};

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
    let preserve_keys = ["pane_id", "terminal_id", "kitty_listen_on", "process_id"];
    let mut ctx = new_ctx;

    let missing: Vec<&str> = preserve_keys
        .iter()
        .filter(|k| {
            ctx.get(**k)
                .and_then(|v| v.as_str())
                .is_none_or(|s| s.is_empty())
        })
        .copied()
        .collect();

    if !missing.is_empty() {
        if let Ok(Some(pos)) = db.get_instance_full(instance_name) {
            if let Some(old_json) = &pos.launch_context {
                if let Ok(old_ctx) = serde_json::from_str::<serde_json::Value>(old_json) {
                    for k in &missing {
                        if let Some(val) = old_ctx.get(*k) {
                            if let Some(s) = val.as_str() {
                                if !s.is_empty() {
                                    ctx.insert(k.to_string(), val.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let json = serde_json::to_string(&ctx).unwrap_or_else(|_| "{}".to_string());
    let mut updates = serde_json::Map::new();
    updates.insert("launch_context".into(), serde_json::json!(json));
    update_instance_position(db, instance_name, &updates);
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
        if let Ok(val) = std::env::var(key) {
            if !val.is_empty() {
                env_map.insert((*key).to_string(), serde_json::json!(val));
            }
        }
    }
    ctx.insert("env".into(), serde_json::Value::Object(env_map));

    // Pane IDs are late-bound. The launcher already persisted the effective preset.
    if let Ok(preset_name) = std::env::var("HCOM_LAUNCHED_PRESET") {
        if !preset_name.is_empty() {
            if let Some(pane_id_env) = crate::config::get_merged_preset_pane_id_env(&preset_name) {
                if let Ok(pane_id) = std::env::var(pane_id_env) {
                    if !pane_id.is_empty() {
                        ctx.insert("pane_id".into(), serde_json::json!(pane_id));
                    }
                }
            }
        }
    }

    // Process ID for kitty close-by-env matching
    if let Ok(pid) = std::env::var("HCOM_PROCESS_ID") {
        if !pid.is_empty() {
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
                        if std::env::var("HCOM_LAUNCHED_PRESET").as_deref() == Ok("zellij") {
                            if let Some(pane_id) = zellij_pane_id_from_terminal_id(&terminal_id) {
                                ctx.insert("pane_id".into(), serde_json::json!(pane_id));
                            }
                        }
                        ctx.insert("terminal_id".into(), serde_json::json!(terminal_id));
                    }
                }
                let _ = std::fs::remove_file(&id_file);
            }
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

        // Reset last_stop on resume
        let now = now_epoch_i64();
        let mut resume_updates = serde_json::Map::new();
        resume_updates.insert("last_stop".into(), serde_json::json!(now));

        if let Some(ref ph_name) = placeholder_name {
            if ph_name != canonical_name {
                // Always migrate notify_endpoints
                if let Err(e) = db.migrate_notify_endpoints(ph_name, canonical_name) {
                    crate::log::log_error(
                        "binding",
                        "bind_canonical.migrate_endpoints",
                        &format!("{e}"),
                    );
                }

                let is_true_placeholder = placeholder_data
                    .as_ref()
                    .map(|d| d.session_id.is_none())
                    .unwrap_or(false);

                if is_true_placeholder {
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
                            resume_updates
                                .insert("status_context".into(), serde_json::json!("new"));
                        }
                    }

                    // Delete true placeholder (temporary identity)
                    match db.delete_instance(ph_name) {
                        Ok(true) => {}
                        Ok(false) => {
                            if let Err(e) = db.migrate_notify_endpoints(canonical_name, ph_name) {
                                crate::log::log_error(
                                    "binding",
                                    "bind_canonical.rollback_endpoints",
                                    &format!("{e}"),
                                );
                            }
                        }
                        Err(e) => {
                            crate::log::log_error(
                                "binding",
                                "bind_canonical.delete_placeholder",
                                &format!("{e}"),
                            );
                            if let Err(e) = db.migrate_notify_endpoints(canonical_name, ph_name) {
                                crate::log::log_error(
                                    "binding",
                                    "bind_canonical.rollback_endpoints",
                                    &format!("{e}"),
                                );
                            }
                        }
                    }
                } else {
                    // Path 1b: Session switch — mark old instance inactive
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
        }

        update_instance_position(db, canonical_name, &resume_updates);

        if let Some(pid) = process_id {
            if let Err(e) = db.set_process_binding(pid, session_id, canonical_name) {
                crate::log::log_error(
                    "binding",
                    "bind_canonical.set_process_binding",
                    &format!("{e}"),
                );
            }
        }

        return Some(canonical_name.clone());
    }

    // Path 2: No canonical, but placeholder exists — bind session to placeholder
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
        if let Some(pid) = process_id {
            if let Err(e) = db.set_process_binding(pid, session_id, ph_name) {
                crate::log::log_error(
                    "binding",
                    "bind_placeholder.set_process_binding",
                    &format!("{e}"),
                );
            }
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
            } else if session_id.is_some() || parent_session_id.is_some() || is_launched {
                if let Ok(hcom_config) = crate::config::HcomConfig::load(None) {
                    if !hcom_config.tag.is_empty() {
                        data.insert("tag".into(), serde_json::json!(hcom_config.tag));
                    }
                }
            }

            if let Some(wt) = wait_timeout {
                data.insert("wait_timeout".into(), serde_json::json!(wt));
            }
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
                    let launcher =
                        std::env::var("HCOM_LAUNCHED_BY").unwrap_or_else(|_| "unknown".to_string());
                    let event_data = serde_json::json!({
                        "action": "created",
                        "by": launcher,
                        "is_hcom_launched": is_launched,
                        "is_subagent": parent_session_id.is_some(),
                        "parent_name": parent_name.unwrap_or(""),
                    });
                    let _ = db.log_event("life", instance_name, &event_data);
                    auto_subscribe_defaults(db, instance_name, tool.unwrap_or(""));
                    true
                }
                _ => true,
            }
        }
        Err(_) => false,
    }
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
    if let Some(pid) = process_id {
        if let Err(e) = db.set_process_binding(pid, session_id, &name) {
            eprintln!("[hcom] warn: set_process_binding failed for {name}: {e}");
        }
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
    if let Some(pid) = process_id {
        if let Ok(Some(name)) = db.get_process_binding(pid) {
            if let Ok(Some(instance)) = db.get_instance_full(&name) {
                return Some(instance);
            }
        }
    }

    if let Some(sid) = session_id {
        if let Some(name) = db.get_session_binding(sid).ok().flatten() {
            if let Ok(Some(instance)) = db.get_instance_full(&name) {
                return Some(instance);
            }
        }
    }

    None
}

/// Auto-subscribe instance to default event subscriptions from config.
/// Called during instance creation.
fn auto_subscribe_defaults(db: &HcomDb, instance_name: &str, tool: &str) {
    if !matches!(tool, "claude" | "gemini" | "codex" | "opencode") {
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
            let _ = crate::commands::events::create_filter_subscription(
                db,
                &filters,
                &[],
                instance_name,
                false,
                true,
                None,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;

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

        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA journal_mode=WAL;

             CREATE TABLE events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 timestamp TEXT NOT NULL,
                 type TEXT NOT NULL,
                 instance TEXT,
                 data TEXT NOT NULL
             );

             CREATE TABLE instances (
                 name TEXT PRIMARY KEY,
                 session_id TEXT UNIQUE,
                 parent_session_id TEXT,
                 parent_name TEXT,
                 tag TEXT,
                 last_event_id INTEGER DEFAULT 0,
                 status TEXT DEFAULT 'active',
                 status_time INTEGER DEFAULT 0,
                 status_context TEXT DEFAULT '',
                 status_detail TEXT DEFAULT '',
                 last_stop INTEGER DEFAULT 0,
                 directory TEXT,
                 created_at REAL NOT NULL DEFAULT 0,
                 transcript_path TEXT DEFAULT '',
                 tcp_mode INTEGER DEFAULT 0,
                 wait_timeout INTEGER DEFAULT 86400,
                 background INTEGER DEFAULT 0,
                 background_log_file TEXT DEFAULT '',
                 name_announced INTEGER DEFAULT 0,
                 agent_id TEXT UNIQUE,
                 running_tasks TEXT DEFAULT '',
                 origin_device_id TEXT DEFAULT '',
                 hints TEXT DEFAULT '',
                 subagent_timeout INTEGER,
                 tool TEXT DEFAULT 'claude',
                 launch_args TEXT DEFAULT '',
                 terminal_preset_requested TEXT DEFAULT '',
                 terminal_preset_effective TEXT DEFAULT '',
                 idle_since TEXT DEFAULT '',
                 pid INTEGER DEFAULT NULL,
                 launch_context TEXT DEFAULT '',
                 FOREIGN KEY (parent_session_id) REFERENCES instances(session_id) ON DELETE SET NULL
             );

             CREATE TABLE process_bindings (
                 process_id TEXT PRIMARY KEY,
                 session_id TEXT,
                 instance_name TEXT,
                 updated_at REAL NOT NULL
             );

             CREATE TABLE session_bindings (
                 session_id TEXT PRIMARY KEY,
                 instance_name TEXT NOT NULL,
                 created_at REAL NOT NULL,
                 FOREIGN KEY (instance_name) REFERENCES instances(name) ON DELETE CASCADE
             );

             CREATE TABLE notify_endpoints (
                 instance TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 port INTEGER NOT NULL,
                 updated_at REAL NOT NULL,
                 PRIMARY KEY (instance, kind)
             );

             CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        (db, db_path)
    }

    fn cleanup(path: PathBuf) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
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

        let result = crate::commands::events::create_filter_subscription(
            &db,
            &filters,
            &[],
            "test-agent",
            false,
            true,
            None,
        );
        assert_eq!(result, 0, "subscription creation should succeed");

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

    // ── --agent mode regression tests ────────────────────────────────────────
    //
    // In `--agent` startup, hcom can observe an OpenCode process before the
    // OpenCode session has been bound.  `bindIdentity()` is the path that calls
    // `opencode-start` and creates a `session_bindings` row once a session ID is
    // available.  The tests below verify the DB-level invariants that the plugin
    // fix relies on.

    /// Regression: calling `bind_session_to_process` without a prior
    /// `session.created` (the observed --agent startup gap) must still create a
    /// `session_bindings` row.  Before the fix, `HCOM_LAUNCHED` blocked
    /// `bindIdentity` and no row was inserted from later session-bearing events.
    #[test]
    fn test_agent_mode_bind_session_creates_session_binding() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        // Simulate: instance placeholder created by the launcher (no session_id yet,
        // matching the state before session.created would fire in normal mode).
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("agent-alpha"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("tool".into(), serde_json::json!("opencode"));
        db.save_instance_named("agent-alpha", &data).unwrap();
        db.set_process_binding("proc-agent-1", "", "agent-alpha").unwrap();

        // The plugin now calls this from later session-bearing plugin events.
        let result = bind_session_to_process(&db, "sess-agent-1", Some("proc-agent-1"));
        assert_eq!(result, Some("agent-alpha".to_string()));

        // session_bindings row must exist (was missing before the fix).
        let binding = db.get_session_binding("sess-agent-1").unwrap();
        assert_eq!(binding, Some("agent-alpha".to_string()));

        // Instance must carry the session_id.
        let inst = db.get_instance_full("agent-alpha").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sess-agent-1"));

        cleanup(path);
    }

    /// `bind_session_to_process` called twice with the same session_id must be
    /// idempotent: same name returned, no duplicate session_bindings rows.
    #[test]
    fn test_agent_mode_bind_session_idempotent() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("agent-beta"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        data.insert("tool".into(), serde_json::json!("opencode"));
        db.save_instance_named("agent-beta", &data).unwrap();
        db.set_process_binding("proc-agent-2", "", "agent-beta").unwrap();

        let r1 = bind_session_to_process(&db, "sess-agent-2", Some("proc-agent-2"));
        let r2 = bind_session_to_process(&db, "sess-agent-2", Some("proc-agent-2"));
        assert_eq!(r1, Some("agent-beta".to_string()));
        assert_eq!(r2, Some("agent-beta".to_string()));

        // Exactly one session_bindings row.
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM session_bindings WHERE session_id = ?",
                rusqlite::params!["sess-agent-2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "must have exactly one session_bindings row");

        cleanup(path);
    }

    /// Full sequence: `initialize_instance_in_position_file` (tool=opencode, no
    /// session_id) followed by `bind_session_to_process` — mirrors the launcher
    /// placeholder followed by a later OpenCode session binding.
    #[test]
    fn test_agent_mode_initialize_then_bind_session() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        // Step 1: launcher creates the placeholder row (no session_id yet).
        initialize_instance_in_position_file(
            &db,
            "agent-gamma",
            None,  // no session_id yet
            None, None, None, None,
            Some("opencode"),
            false, None, None, None, None, None,
        );
        db.set_process_binding("proc-agent-3", "", "agent-gamma").unwrap();

        // Row exists, no session yet.
        let before = db.get_instance_full("agent-gamma").unwrap().unwrap();
        assert!(before.session_id.is_none(), "session_id should be absent before binding");

        // Step 2: plugin calls bind once an OpenCode session ID is available.
        let result = bind_session_to_process(&db, "sess-agent-3", Some("proc-agent-3"));
        assert_eq!(result, Some("agent-gamma".to_string()));

        // session_bindings row created.
        assert_eq!(
            db.get_session_binding("sess-agent-3").unwrap(),
            Some("agent-gamma".to_string())
        );

        // Instance carries session_id.
        let after = db.get_instance_full("agent-gamma").unwrap().unwrap();
        assert_eq!(after.session_id.as_deref(), Some("sess-agent-3"));

        cleanup(path);
    }

    /// When `HCOM_PROCESS_ID` is absent (no process binding), `bind_session_to_process`
    /// with no process_id must return `None` and must NOT insert a phantom
    /// `session_bindings` row — mirroring the daemon-absent / headless error path
    /// where `opencode-start` returns `{"error": ...}`.
    #[test]
    fn test_agent_mode_no_process_id_no_session_binding() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        // No instance, no process binding.
        let result = bind_session_to_process(&db, "sess-agent-none", None);
        assert!(result.is_none(), "must return None when no process binding exists");

        // No phantom session_bindings row.
        let binding = db.get_session_binding("sess-agent-none").unwrap();
        assert!(binding.is_none(), "must not create a phantom session_bindings row");

        cleanup(path);
    }
}
