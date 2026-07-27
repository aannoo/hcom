//! Verified Claude hook actor capabilities for CLI invocations.
//!
//! Claude's PreToolUse hook knows the exact actor for a shell call. The hook
//! exports an opaque, database-backed token into that command; the hcom CLI
//! validates it here and resolves the root or exact child row without relying
//! on shared session/process state.

use crate::db::HcomDb;
use crate::identity;
use crate::shared::{HcomError, SenderIdentity};
use std::collections::HashMap;

pub const ENV_VAR: &str = "HCOM_CLAUDE_ACTOR";
pub const SESSION_ENV_VAR: &str = "HCOM_CLAUDE_ACTOR_SESSION";

fn env_nonempty<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

/// Resolve the verified actor exported by Claude's Bash PreToolUse hook.
///
/// The capability is an *enhancement*, never a requirement: a valid token
/// pins the exact acting actor (root or a specific child), but any absent,
/// malformed, expired, or revoked token yields `None` so the caller falls back
/// to ordinary identity resolution. This must never hard-fail — plenty of
/// legitimate hcom calls run in a participating Claude session with no injected
/// token (the human `! command` bash box bypasses PreToolUse entirely; so do
/// manual shells and any tool whose hook didn't rewrite the command).
pub fn resolve_env_actor(db: &HcomDb) -> Result<Option<SenderIdentity>, HcomError> {
    let env: HashMap<String, String> = std::env::vars().collect();
    resolve_actor_from_env(db, &env)
}

fn resolve_actor_from_env(
    db: &HcomDb,
    env: &HashMap<String, String>,
) -> Result<Option<SenderIdentity>, HcomError> {
    let (Some(token), Some(session_id)) = (
        env_nonempty(env, ENV_VAR),
        env_nonempty(env, SESSION_ENV_VAR),
    ) else {
        return Ok(None);
    };

    let Some(actor_name) = db
        .resolve_claude_actor_capability(token, session_id)
        .map_err(|error| HcomError::DatabaseError(error.to_string()))?
    else {
        return Ok(None);
    };

    identity::resolve_from_name(db, &actor_name).map(Some)
}

/// Reject an explicit identity that conflicts with the verified Claude actor.
pub fn ensure_explicit_matches(
    db: &HcomDb,
    actor: &SenderIdentity,
    explicit_name: &str,
) -> Result<(), HcomError> {
    let explicit = identity::resolve_from_name(db, explicit_name)?;
    if explicit.name == actor.name {
        return Ok(());
    }

    Err(HcomError::InvalidInput(format!(
        "Explicit --name '{}' conflicts with verified Claude actor '{}'",
        explicit_name, actor.name
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn explicit_name_must_match_verified_actor() {
        let temp = TempDir::new().unwrap();
        let db = HcomDb::open_raw(&temp.path().join("actor-match.db")).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'a6d9caf', 'claude',
                         'active', 0, 0, 0)",
                [],
            )
            .unwrap();

        // Real Claude agent_ids are dashless 7-char hex; resolve either by the
        // row name or by that agent_id.
        let actor = identity::resolve_from_name(&db, "a6d9caf").unwrap();
        assert!(ensure_explicit_matches(&db, &actor, "nova_task_1").is_ok());
        assert!(ensure_explicit_matches(&db, &actor, "a6d9caf").is_ok());
        assert!(ensure_explicit_matches(&db, &actor, "nova").is_err());
    }

    #[test]
    fn missing_token_falls_back_to_manual_resolution() {
        // The human `! command` bash box and any hook-less shell run inside a
        // bound Claude session with no injected token. That must NOT hard-fail:
        // it yields None so the caller falls back to ordinary resolution.
        let temp = TempDir::new().unwrap();
        let db = HcomDb::open_raw(&temp.path().join("actor-fallback.db")).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();

        let result = resolve_actor_from_env(&db, &env(&[("CLAUDECODE", "1")])).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn valid_token_resolves_verified_actor() {
        let temp = TempDir::new().unwrap();
        let db = HcomDb::open_raw(&temp.path().join("actor-valid.db")).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        let token = db
            .issue_claude_actor_capability("sess-1", "tool-1", None, "nova")
            .unwrap();

        let actor =
            resolve_actor_from_env(&db, &env(&[(ENV_VAR, &token), (SESSION_ENV_VAR, "sess-1")]))
                .unwrap()
                .expect("valid token resolves an actor");
        assert_eq!(actor.name, "nova");
    }

    #[test]
    fn invalid_or_expired_token_falls_back_to_none() {
        let temp = TempDir::new().unwrap();
        let db = HcomDb::open_raw(&temp.path().join("actor-invalid.db")).unwrap();
        db.init_db().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();

        // Bogus token, and a valid-shaped token with no session pairing: both None.
        assert!(
            resolve_actor_from_env(
                &db,
                &env(&[
                    ("HCOM_CLAUDE_ACTOR", "tampered"),
                    (SESSION_ENV_VAR, "sess-1")
                ]),
            )
            .unwrap()
            .is_none()
        );
        let token = db
            .issue_claude_actor_capability("sess-1", "tool-1", None, "nova")
            .unwrap();
        assert!(
            resolve_actor_from_env(&db, &env(&[(ENV_VAR, &token)]))
                .unwrap()
                .is_none(),
            "token without its session binding must not resolve"
        );
    }
}
