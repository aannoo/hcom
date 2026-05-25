//! Antigravity (agy) delivery — preview PTY wake; hook-primary message bodies.
//!
//! Full payloads are delivered via `gemini-beforeagent` / `gemini-aftertool`
//! (`additionalContext` + `commit_delivery_ack`). The PTY thread injects a
//! `<hcom>…</hcom>` preview (envelope + sender + snippet) + Enter when idle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::db::HcomDb;
use crate::log::{log_info, log_warn};
use crate::notify::NotifyServer;
use crate::shared::ST_INACTIVE;

use super::{
    DeliveryState, IDLE_WAIT, ToolConfig, evaluate_gate, inject_enter, inject_text,
    refresh_binding, refresh_display_name, refresh_status,
};

const WAKE_ENTER_DELAY: Duration = Duration::from_millis(200);
/// Re-inject preview wake if pending remains at the same cursor (hook never acked).
const STALE_WAKE_RETRY: Duration = Duration::from_secs(90);

/// Whether to clear wake dedup and allow another PTY preview inject.
pub(crate) fn antigravity_should_retry_wake(
    pending: bool,
    wake_cursor: Option<i64>,
    current_cursor: i64,
    wake_age: Option<Duration>,
    retry_after: Duration,
) -> bool {
    if !pending {
        return false;
    }
    let Some(sent_at) = wake_cursor else {
        return false;
    };
    if current_cursor != sent_at {
        return false;
    }
    wake_age.is_some_and(|age| age >= retry_after)
}

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
            "Antigravity mode for {}: preview PTY wake, hooks deliver bodies",
            current_name
        ),
    );

    let mut current_status = "listening".to_string();
    // Cursor at last wake inject; cleared when cursor advances or pending drains.
    let mut wake_sent_at_cursor: Option<i64> = None;
    let mut wake_sent_at: Option<Instant> = None;

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
        let pending = db.has_pending(current_name);
        if !pending {
            wake_sent_at_cursor = None;
            wake_sent_at = None;
        } else if let Some(sent_at) = wake_sent_at_cursor
            && cursor > sent_at
        {
            wake_sent_at_cursor = None;
            wake_sent_at = None;
        } else if antigravity_should_retry_wake(
            pending,
            wake_sent_at_cursor,
            cursor,
            wake_sent_at.map(|t| t.elapsed()),
            STALE_WAKE_RETRY,
        ) {
            log_info(
                "native",
                "delivery.antigravity_wake_retry",
                &format!(
                    "Re-wake for {} at cursor={} after stale pending",
                    current_name, cursor
                ),
            );
            wake_sent_at_cursor = None;
            wake_sent_at = None;
        }

        if pending && wake_sent_at_cursor.is_none() {
            let is_idle = if config.require_idle {
                db.is_idle(current_name)
            } else {
                true
            };
            let gate = evaluate_gate(config, state, is_idle);
            if gate.safe {
                let cols = state.screen.read().map(|s| s.cols).unwrap_or(80) as usize;
                let input_box_width = cols.saturating_sub(15).max(10);
                let wake_text = super::build_wake_inject_text(db, current_name, input_box_width);
                if inject_text(state.inject_port, &wake_text) {
                    std::thread::sleep(WAKE_ENTER_DELAY);
                    if inject_enter(state.inject_port) {
                        wake_sent_at_cursor = Some(cursor);
                        wake_sent_at = Some(Instant::now());
                        log_info(
                            "native",
                            "delivery.antigravity_wake",
                            &format!(
                                "Wake inject for {} at cursor={} preview={}",
                                current_name,
                                cursor,
                                super::truncate_chars(&wake_text, 48)
                            ),
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
    use std::time::Duration;

    #[test]
    fn test_antigravity_should_retry_wake_when_stale() {
        assert!(antigravity_should_retry_wake(
            true,
            Some(5),
            5,
            Some(Duration::from_secs(90)),
            Duration::from_secs(90),
        ));
    }

    #[test]
    fn test_antigravity_should_not_retry_when_cursor_advanced() {
        assert!(!antigravity_should_retry_wake(
            true,
            Some(5),
            6,
            Some(Duration::from_secs(120)),
            Duration::from_secs(90),
        ));
    }

    #[test]
    fn test_antigravity_should_not_retry_when_too_soon() {
        assert!(!antigravity_should_retry_wake(
            true,
            Some(5),
            5,
            Some(Duration::from_secs(30)),
            Duration::from_secs(90),
        ));
    }

    #[test]
    fn test_antigravity_should_not_retry_without_pending() {
        assert!(!antigravity_should_retry_wake(
            false,
            Some(5),
            5,
            Some(Duration::from_secs(120)),
            Duration::from_secs(90),
        ));
    }
}

