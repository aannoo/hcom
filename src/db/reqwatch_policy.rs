//! Request-watch idle policy — tool-specific deferral before "abandoned request" notify.

use serde_json::Value;

/// After agy reports `listening`, wait before "idle without reply" (seconds).
///
/// Per idle spell only — reset when the target goes `active`/`blocked` again.
pub(crate) const AGY_REQWATCH_IDLE_GRACE_SEC: f64 = 10.0;

/// How a request-watch subscription should react to a matching event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReqwatchNotifyDecision {
    /// Defer caller notification; caller should bump `last_id` and maybe set grace.
    Defer { set_grace_if_absent: bool },
    /// Proceed to notify the request watcher.
    Proceed,
    /// Ignore this event for reqwatch (agy non-listening/non-stopped).
    Skip,
}

/// Tool-specific reqwatch gating for a single subscription match.
pub(crate) fn reqwatch_notify_decision(
    target_tool: &str,
    event_type: &str,
    data: &Value,
    sub: &Value,
    now: f64,
) -> ReqwatchNotifyDecision {
    if target_tool != "antigravity" {
        return ReqwatchNotifyDecision::Proceed;
    }

    let is_listening =
        event_type == "status" && data.get("status").and_then(|v| v.as_str()) == Some("listening");
    let is_stopped =
        event_type == "life" && data.get("action").and_then(|v| v.as_str()) == Some("stopped");

    if is_listening {
        let grace_until = sub.get("idle_grace_until").and_then(|v| v.as_f64());
        let defer = match grace_until {
            None => true,
            Some(until) if now < until => true,
            Some(_) => false,
        };
        if defer {
            return ReqwatchNotifyDecision::Defer {
                set_grace_if_absent: grace_until.is_none(),
            };
        }
        return ReqwatchNotifyDecision::Proceed;
    }

    if is_stopped {
        return ReqwatchNotifyDecision::Proceed;
    }

    ReqwatchNotifyDecision::Skip
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_non_antigravity_proceeds_immediately() {
        let sub = json!({});
        let data = json!({"status": "listening"});
        assert_eq!(
            reqwatch_notify_decision("gemini", "status", &data, &sub, 0.0),
            ReqwatchNotifyDecision::Proceed
        );
    }

    #[test]
    fn test_agy_listening_defers_and_sets_grace() {
        let sub = json!({});
        let data = json!({"status": "listening"});
        assert_eq!(
            reqwatch_notify_decision("antigravity", "status", &data, &sub, 100.0),
            ReqwatchNotifyDecision::Defer {
                set_grace_if_absent: true
            }
        );
    }

    #[test]
    fn test_agy_listening_proceeds_after_grace_expired() {
        let sub = json!({"idle_grace_until": 50.0});
        let data = json!({"status": "listening"});
        assert_eq!(
            reqwatch_notify_decision("antigravity", "status", &data, &sub, 100.0),
            ReqwatchNotifyDecision::Proceed
        );
    }

    #[test]
    fn test_agy_active_status_skipped() {
        let sub = json!({});
        let data = json!({"status": "active"});
        assert_eq!(
            reqwatch_notify_decision("antigravity", "status", &data, &sub, 0.0),
            ReqwatchNotifyDecision::Skip
        );
    }
}
