//! Antigravity (agy) delivery — minimal PTY wake; hook-primary message bodies.
//!
//! Full payloads are delivered via `gemini-beforeagent` / `gemini-aftertool`
//! (`additionalContext` + `commit_delivery_ack`). The PTY thread only injects
//! `<hcom>` + Enter once per pending batch to start a turn when idle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::db::HcomDb;
use crate::log::{log_info, log_warn};
use crate::notify::NotifyServer;
use crate::shared::ST_INACTIVE;

use super::{
    DeliveryState, IDLE_WAIT, ToolConfig, evaluate_gate, inject_enter, inject_text,
    refresh_binding, refresh_display_name, refresh_status,
};

/// Wake tag injected into the agy prompt (not the full message body).
pub const WAKE_TAG: &str = "<hcom>";

const WAKE_ENTER_DELAY: Duration = Duration::from_millis(200);

/// Run hook-primary delivery for Antigravity (minimal PTY wake loop).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_antigravity_delivery_loop(
    running: Arc<AtomicBool>,
    db: &mut HcomDb,
    notify: &NotifyServer,
    state: &DeliveryState,
    current_name: &mut String,
    process_id: &str,
    config: &ToolConfig,
    shared_name: Option<Arc<std::sync::RwLock<String>>>,
    shared_status: Option<Arc<std::sync::RwLock<String>>>,
) {
    log_info(
        "native",
        "delivery.antigravity_mode",
        &format!(
            "Antigravity mode for {}: minimal PTY wake, hooks deliver bodies",
            current_name
        ),
    );

    let mut current_status = "listening".to_string();
    // Cursor at last wake inject; cleared when cursor advances or pending drains.
    let mut wake_sent_at_cursor: Option<i64> = None;

    while running.load(Ordering::Acquire) {
        refresh_binding(db, process_id, current_name, &shared_name);
        refresh_status(db, current_name, &mut current_status, &shared_status);
        refresh_display_name(db, current_name, &shared_name);

        notify.wait(IDLE_WAIT);
        if !running.load(Ordering::Acquire) {
            break;
        }

        db.reconnect_if_stale();

        let cursor = db.get_cursor(current_name);
        if !db.has_pending(current_name) {
            wake_sent_at_cursor = None;
        } else if let Some(sent_at) = wake_sent_at_cursor
            && cursor > sent_at
        {
            wake_sent_at_cursor = None;
        }

        if db.has_pending(current_name) && wake_sent_at_cursor.is_none() {
            let is_idle = if config.require_idle {
                db.is_idle(current_name)
            } else {
                true
            };
            let gate = evaluate_gate(config, state, is_idle);
            if gate.safe {
                if inject_text(state.inject_port, WAKE_TAG) {
                    std::thread::sleep(WAKE_ENTER_DELAY);
                    if inject_enter(state.inject_port) {
                        wake_sent_at_cursor = Some(cursor);
                        log_info(
                            "native",
                            "delivery.antigravity_wake",
                            &format!("Wake inject for {} at cursor={}", current_name, cursor),
                        );
                    } else {
                        log_warn(
                            "native",
                            "delivery.antigravity_wake_enter_fail",
                            "Enter after wake tag failed",
                        );
                    }
                } else {
                    log_warn(
                        "native",
                        "delivery.antigravity_wake_inject_fail",
                        "TCP wake tag inject failed",
                    );
                }
            }
        }

        if let Err(e) = db.update_heartbeat(current_name) {
            log_warn("native", "delivery.heartbeat_fail", &format!("{}", e));
        }
        if let Err(e) = db.register_notify_port(current_name, notify.port()) {
            log_warn("native", "delivery.register_notify_fail", &format!("{}", e));
        }
        if let Err(e) = db.register_inject_port(current_name, state.inject_port) {
            log_warn("native", "delivery.register_inject_fail", &format!("{}", e));
        }
        if let Err(e) = db.update_tcp_mode(current_name, true) {
            log_warn("native", "delivery.tcp_mode_fail", &format!("{}", e));
        }
    }
}

/// PTY exit cleanup when SessionEnd already soft-stopped the instance.
pub(crate) fn cleanup_antigravity_pty_exit(
    db: &mut HcomDb,
    current_name: &str,
    process_id: &str,
    owns_instance: bool,
) {
    if !owns_instance {
        log_info(
            "native",
            "delivery.cleanup_skipped",
            &format!(
                "Skipping instance cleanup for {} — name reassigned to new process",
                current_name
            ),
        );
    } else {
        let already_inactive = db
            .get_status(current_name)
            .ok()
            .flatten()
            .is_some_and(|(status, _)| status == ST_INACTIVE);

        if already_inactive {
            log_info(
                "native",
                "delivery.cleanup_soft_stopped",
                &format!(
                    "Instance {} already inactive (soft SessionEnd); skipping delete_instance",
                    current_name
                ),
            );
            if let Err(e) = db.delete_notify_endpoints(current_name) {
                log_warn(
                    "native",
                    "delivery.cleanup_endpoints_fail",
                    &format!("{}", e),
                );
            }
            if let Err(e) = db.cleanup_subscriptions(current_name) {
                log_warn("native", "delivery.cleanup_subs_fail", &format!("{}", e));
            }
        } else {
            super::cleanup_deleted_instance(db, current_name);
        }
    }

    if !process_id.is_empty()
        && let Err(e) = db.delete_process_binding(process_id)
    {
        log_warn("native", "delivery.cleanup_binding_fail", &format!("{}", e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_tag_is_minimal() {
        assert_eq!(WAKE_TAG, "<hcom>");
        assert!(!WAKE_TAG.contains('\n'));
    }
}
