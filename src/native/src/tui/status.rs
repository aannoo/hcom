use crate::tui::model::{Agent, AgentStatus};

// ── Timeout constants (from Python core/timeouts.py) ─────────────

/// Heartbeat threshold when TCP notify endpoint exists (pty, listen, hooks).
pub const HEARTBEAT_THRESHOLD_TCP: f64 = 35.0;

/// Heartbeat threshold when no TCP listener (rapid stale detection).
pub const HEARTBEAT_THRESHOLD_NO_TCP: f64 = 10.0;

/// No status update for this long + stale heartbeat → inactive.
pub const STATUS_ACTIVITY_TIMEOUT: f64 = 300.0;

/// Instance created but no first status update within this window.
pub const LAUNCH_PLACEHOLDER_TIMEOUT: f64 = 30.0;

/// After system wake, skip stale checks for this long.
pub const WAKE_GRACE_PERIOD: f64 = 60.0;

/// Sentinel for missing heartbeat data.
const UNKNOWN_HEARTBEAT_AGE: f64 = 999999.0;

// ── Computed status result ────────────────────────────────────────

pub struct ComputedStatus {
    pub status: AgentStatus,
    pub status_context: String,
    pub status_detail: String,
}

/// Compute runtime status for an agent using stale detection logic.
///
/// Port of Python `get_instance_status()`. Takes agent data + current time,
/// returns the computed status (may differ from stored status if heartbeat
/// has gone stale).
///
/// `wake_time`: if Some, the epoch time of last system wake. Used for
/// wake grace period (skip stale checks for 60s after wake).
pub fn compute_status(agent: &Agent, now: f64, wake_time: Option<f64>) -> ComputedStatus {
    let status = agent.status;
    let ctx = &agent.status_context;
    let detail = &agent.status_detail;

    let passthrough = || ComputedStatus {
        status,
        status_context: ctx.clone(),
        status_detail: detail.clone(),
    };

    let in_wake_grace = wake_time
        .map(|wt| (now - wt) < WAKE_GRACE_PERIOD)
        .unwrap_or(false);

    // ── Launching: instance created but session not yet bound ─────
    if ctx == "new" && matches!(status, AgentStatus::Inactive | AgentStatus::Launching) {
        let age = if agent.created_at > 0.0 {
            now - agent.created_at
        } else {
            0.0
        };
        if age < LAUNCH_PLACEHOLDER_TIMEOUT {
            return ComputedStatus {
                status: AgentStatus::Launching,
                status_context: "launching".into(),
                status_detail: String::new(),
            };
        } else {
            return ComputedStatus {
                status: AgentStatus::Inactive,
                status_context: "launch_failed".into(),
                status_detail: "launch probably failed — check logs".into(),
            };
        }
    }

    // ── Listening: heartbeat-proven liveness ──────────────────────
    if status == AgentStatus::Listening {
        // Remote agents: trust synced status, skip local stale detection
        if agent.is_remote() {
            return passthrough();
        }

        let heartbeat_age = if agent.last_heartbeat > 0.0 {
            now - agent.last_heartbeat
        } else if agent.status_time > 0.0 {
            now - agent.status_time
        } else {
            UNKNOWN_HEARTBEAT_AGE
        };

        let threshold = if agent.has_tcp {
            HEARTBEAT_THRESHOLD_TCP
        } else {
            HEARTBEAT_THRESHOLD_NO_TCP
        };

        if heartbeat_age > threshold {
            if in_wake_grace {
                // Heartbeat will refresh soon after wake
                return passthrough();
            }
            return ComputedStatus {
                status: AgentStatus::Inactive,
                status_context: "stale".into(),
                status_detail: "listening".into(),
            };
        }

        // Heartbeat within threshold — agent is confirmed alive
        return passthrough();
    }

    // ── Active/Blocked: activity timeout check ───────────────────
    if status != AgentStatus::Inactive {
        let status_age = if agent.status_time > 0.0 {
            now - agent.status_time
        } else if agent.created_at > 0.0 {
            now - agent.created_at
        } else {
            0.0
        };

        if status_age > STATUS_ACTIVITY_TIMEOUT {
            if agent.is_remote() {
                // Remote: trust relay
                return ComputedStatus {
                    status,
                    status_context: ctx.clone(),
                    status_detail: detail.clone(),
                };
            }

            // Check if heartbeat is fresh (process still alive despite no status update)
            let heartbeat_fresh = agent.last_heartbeat > 0.0
                && (now - agent.last_heartbeat) < HEARTBEAT_THRESHOLD_TCP;

            if heartbeat_fresh || in_wake_grace {
                return passthrough();
            }

            let prev_status = match status {
                AgentStatus::Active => "active",
                AgentStatus::Blocked => "blocked",
                AgentStatus::Launching => "launching",
                _ => "unknown",
            };
            return ComputedStatus {
                status: AgentStatus::Inactive,
                status_context: "stale".into(),
                status_detail: prev_status.into(),
            };
        }
    }

    // ── No stale condition detected ──────────────────────────────
    passthrough()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::Tool;

    fn test_agent(status: AgentStatus, ctx: &str) -> Agent {
        Agent {
            name: "test".into(),
            tool: Tool::Claude,
            status,
            status_context: ctx.into(),
            status_detail: String::new(),
            created_at: 1000.0,
            status_time: 1000.0,
            last_heartbeat: 1000.0,
            has_tcp: true,
            directory: String::new(),
            tag: String::new(),
            unread: 0,
            device_name: None,
            sync_age: None,
            last_event_id: None,
            headless: false,
            session_id: None,
            pid: None,
            terminal_preset: None,
        }
    }

    #[test]
    fn launching_within_timeout() {
        let mut agent = test_agent(AgentStatus::Inactive, "new");
        agent.created_at = 1000.0;
        let result = compute_status(&agent, 1020.0, None);
        assert_eq!(result.status, AgentStatus::Launching);
    }

    #[test]
    fn launching_past_timeout() {
        let mut agent = test_agent(AgentStatus::Inactive, "new");
        agent.created_at = 1000.0;
        let result = compute_status(&agent, 1050.0, None);
        assert_eq!(result.status, AgentStatus::Inactive);
        assert_eq!(result.status_context, "launch_failed");
    }

    #[test]
    fn listening_fresh_heartbeat() {
        let mut agent = test_agent(AgentStatus::Listening, "listening");
        agent.last_heartbeat = 990.0;
        let result = compute_status(&agent, 1000.0, None);
        assert_eq!(result.status, AgentStatus::Listening);
    }

    #[test]
    fn listening_stale_heartbeat_tcp() {
        let mut agent = test_agent(AgentStatus::Listening, "listening");
        agent.last_heartbeat = 950.0;
        let result = compute_status(&agent, 1000.0, None);
        assert_eq!(result.status, AgentStatus::Inactive);
        assert_eq!(result.status_context, "stale");
    }

    #[test]
    fn listening_stale_no_tcp_faster() {
        let mut agent = test_agent(AgentStatus::Listening, "listening");
        agent.has_tcp = false;
        agent.last_heartbeat = 985.0; // 15s ago > 10s threshold
        let result = compute_status(&agent, 1000.0, None);
        assert_eq!(result.status, AgentStatus::Inactive);
    }

    #[test]
    fn listening_wake_grace_skips_stale() {
        let mut agent = test_agent(AgentStatus::Listening, "listening");
        agent.last_heartbeat = 900.0; // very stale
        let result = compute_status(&agent, 1000.0, Some(980.0)); // woke 20s ago
        assert_eq!(result.status, AgentStatus::Listening);
    }

    #[test]
    fn remote_skips_stale_check() {
        let mut agent = test_agent(AgentStatus::Listening, "listening");
        agent.device_name = Some("BOXE".into());
        agent.last_heartbeat = 0.0; // no heartbeat data
        let result = compute_status(&agent, 1000.0, None);
        assert_eq!(result.status, AgentStatus::Listening);
    }

    #[test]
    fn active_with_activity_timeout() {
        let mut agent = test_agent(AgentStatus::Active, "tool");
        agent.status_time = 600.0;
        agent.last_heartbeat = 600.0; // also stale
        let result = compute_status(&agent, 1000.0, None);
        assert_eq!(result.status, AgentStatus::Inactive);
        assert_eq!(result.status_context, "stale");
        assert_eq!(result.status_detail, "active");
    }

    #[test]
    fn active_with_fresh_heartbeat_despite_old_status() {
        let mut agent = test_agent(AgentStatus::Active, "tool");
        agent.status_time = 600.0; // 400s ago > 300s threshold
        agent.last_heartbeat = 990.0; // fresh heartbeat
        let result = compute_status(&agent, 1000.0, None);
        assert_eq!(result.status, AgentStatus::Active); // kept alive by heartbeat
    }
}
