//! Antigravity (agy) PTY exit cleanup.
//!
//! Delivery itself runs through the shared state machine in `delivery.rs`
//! (PTY injects `<hcom>` wake, hooks deliver bodies via `gemini-beforeagent`
//! and ack via `commit_delivery_ack` cursor advance). Only the teardown path
//! is special: SessionEnd may have already soft-stopped the instance, in
//! which case we must not re-delete its row.

use crate::db::HcomDb;
use crate::log::{log_info, log_warn};
use crate::shared::ST_INACTIVE;

/// PTY exit cleanup when SessionEnd already soft-stopped the instance.
pub(crate) fn cleanup_antigravity_pty_exit(
    db: &mut HcomDb,
    current_name: &str,
    process_id: &str,
    owns_instance: bool,
) {
    if !owns_instance {
        super::log_pty_cleanup_skipped(db, current_name);
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
