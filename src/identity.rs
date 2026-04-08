//! Identity resolution — 3-tier binding (process → session → ad-hoc).

use regex::Regex;
use std::sync::LazyLock;

use crate::db::HcomDb;
use crate::shared::{HcomError, SenderIdentity, SenderKind};

/// UUID pattern for agent_id detection.
static UUID_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
});

/// Valid base instance name: lowercase letters, digits, underscore.
static BASE_NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9_]+$").unwrap());

/// Dangerous characters for user-provided names (injection prevention).
static DANGEROUS_CHARS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[|&;$`<>]").unwrap());

/// Dangerous characters including @ (for --from validation).
static DANGEROUS_CHARS_WITH_AT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[|&;$`<>@]").unwrap());

/// Commands that require a resolved identity to operate.
const REQUIRE_IDENTITY: &[&str] = &["send", "listen"];

/// Check if value looks like a UUID (agent_id format).
pub fn looks_like_uuid(value: &str) -> bool {
    UUID_PATTERN.is_match(value)
}

/// Check if name looks like a Claude Task agent_id (7-char hex).
pub fn looks_like_agent_id(name: &str) -> bool {
    name.len() == 7 && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Check if name is a valid base instance name (lowercase letters, digits, underscore).
pub fn is_valid_base_name(name: &str) -> bool {
    BASE_NAME_RE.is_match(name)
}

/// Build error message for invalid base instance names.
pub fn base_name_error(name: &str) -> String {
    format!(
        "Invalid instance name '{name}'. Use base name only (lowercase letters, numbers, underscore)."
    )
}

/// Generate actionable error message for instance not found.
///
/// For subagent agent_ids, don't suggest `--as` (causes process binding conflicts).
pub fn instance_not_found_error(name: &str) -> String {
    if looks_like_agent_id(name) {
        return format!(
            "Instance '{name}' not found. Your session may have ended. Stop working and end your turn."
        );
    }
    format!("Instance '{name}' not found. Run 'hcom start --as {name}' to reclaim your identity.")
}

/// Validate user-provided name input for length and dangerous characters.
///
/// Used for `--name` and `--from` flag validation in CLI commands.
pub fn validate_name_input(name: &str, max_length: usize, allow_at: bool) -> Result<(), String> {
    if name.len() > max_length {
        return Err(format!(
            "Name too long ({} chars, max {max_length})",
            name.len()
        ));
    }

    let pattern = if allow_at {
        &*DANGEROUS_CHARS
    } else {
        &*DANGEROUS_CHARS_WITH_AT
    };

    let bad_chars: Vec<&str> = pattern.find_iter(name).map(|m| m.as_str()).collect();
    if !bad_chars.is_empty() {
        let unique: std::collections::HashSet<&str> = bad_chars.into_iter().collect();
        let chars_str: Vec<&str> = unique.into_iter().collect();
        return Err(format!(
            "Name contains invalid characters: {}",
            chars_str.join(" ")
        ));
    }

    Ok(())
}

/// Resolve `--name NAME` with strict instance lookup.
///
/// Resolution order:
/// 1. Instance name lookup (exact) -> kind=Instance if found
/// 2. Agent ID (UUID) lookup -> kind=Instance if found
/// 3. Error if not found
pub fn resolve_from_name(db: &HcomDb, name: &str) -> Result<SenderIdentity, HcomError> {
    let mut resolved_name = name.to_string();

    // Reject invalid base names, but allow tag-name format (e.g. "team-luna")
    if !looks_like_uuid(name) && !is_valid_base_name(name) {
        // Try tag-name resolution before rejecting
        match crate::instances::resolve_display_name(db, name) {
            Some(base) => resolved_name = base,
            None => return Err(HcomError::InvalidInput(base_name_error(name))),
        }
    }

    // 1. Instance name lookup (exact match)
    if let Ok(Some(data)) = db.get_instance(&resolved_name) {
        crate::log::log_info(
            "identity",
            "resolve_from_name",
            &format!("name={}, method=instance_name", resolved_name),
        );
        return Ok(SenderIdentity {
            kind: SenderKind::Instance,
            name: resolved_name,
            session_id: data
                .get("session_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            instance_data: Some(data),
        });
    }

    // 2. Agent ID lookup (Claude Code sends short IDs like 'a6d9caf')
    if let Ok(Some(instance_name)) = db.get_instance_by_agent_id(&resolved_name) {
        if let Ok(Some(data)) = db.get_instance(&instance_name) {
            crate::log::log_info(
                "identity",
                "resolve_from_name",
                &format!(
                    "name={}, method=agent_id, resolved={}",
                    resolved_name, instance_name
                ),
            );
            return Ok(SenderIdentity {
                kind: SenderKind::Instance,
                name: instance_name,
                session_id: data
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string()),
                instance_data: Some(data),
            });
        }
    }

    // 3. Not found
    crate::log::log_info(
        "identity",
        "resolve_from_name.not_found",
        &format!("name={}", resolved_name),
    );
    Err(HcomError::NotFound(instance_not_found_error(
        &resolved_name,
    )))
}

/// Resolve sender identity for CLI commands and hook handlers.
///
/// # Arguments
///
/// * `db` - Database handle
/// * `name` - Instance name from `--name` flag (strict lookup)
/// * `system_sender` - System notification sender name (e.g., 'hcom-launcher')
/// * `session_id` - Explicit session_id (for hook context, bypasses env detection)
/// * `process_id` - HCOM_PROCESS_ID (for launched instances)
/// * `codex_thread_id` - Codex thread ID for opportunistic session binding
/// * `transcript_fallback` - Optional closure for transcript marker resolution
///
/// # Priority
///
/// 1. `system_sender` - system notifications
/// 2. `session_id` - explicit session (internal use)
/// 3. `name` (--name) - strict instance lookup
/// 4. Auto-detect from `process_id` (HCOM_PROCESS_ID)
/// 5. `transcript_fallback` - transcript marker scan (hook extension point)
/// 6. Error if no identity
#[allow(clippy::type_complexity)]
fn resolve_identity_with_expectation(
    db: &HcomDb,
    name: Option<&str>,
    system_sender: Option<&str>,
    session_id: Option<&str>,
    process_id: Option<&str>,
    codex_thread_id: Option<&str>,
    identity_expected: bool,
    transcript_fallback: Option<&dyn Fn(&HcomDb) -> Option<SenderIdentity>>,
) -> Result<SenderIdentity, HcomError> {
    // 1. System sender (internal use)
    if let Some(sender) = system_sender {
        return Ok(SenderIdentity {
            kind: SenderKind::System,
            name: sender.to_string(),
            instance_data: None,
            session_id: None,
        });
    }

    // 2. Explicit session_id (internal use)
    if let Some(sid) = session_id {
        if !sid.is_empty() {
            let resolved_name = db
                .get_session_binding(sid)
                .map_err(|e| HcomError::DatabaseError(e.to_string()))?;

            match resolved_name {
                Some(inst_name) => {
                    let data = db
                        .get_instance(&inst_name)
                        .map_err(|e| HcomError::DatabaseError(e.to_string()))?;

                    match data {
                        Some(d) => {
                            crate::log::log_info(
                                "identity",
                                "resolve",
                                &format!("method=session_id, name={}", inst_name),
                            );
                            return Ok(SenderIdentity {
                                kind: SenderKind::Instance,
                                name: inst_name,
                                session_id: Some(sid.to_string()),
                                instance_data: Some(d),
                            });
                        }
                        None => {
                            return Err(HcomError::NotFound(
                                "Instance not found for session_id".to_string(),
                            ));
                        }
                    }
                }
                None => {
                    crate::log::log_warn(
                        "identity",
                        "resolve.session_id_not_found",
                        &format!("session_id={}", &sid[..sid.len().min(8)]),
                    );
                    return Err(HcomError::NotFound(
                        "Instance not found for session_id".to_string(),
                    ));
                }
            }
        }
    }

    // 3. Strict instance lookup (--name NAME)
    if let Some(n) = name {
        if !n.is_empty() {
            return resolve_from_name(db, n);
        }
    }

    // 4. Auto-detect from process binding (hcom-launched instances)
    if let Some(pid) = process_id {
        if !pid.is_empty() {
            let bound_name = db
                .get_process_binding(pid)
                .map_err(|e| HcomError::DatabaseError(e.to_string()))?;

            match bound_name {
                Some(inst_name) => {
                    let data = db
                        .get_instance(&inst_name)
                        .map_err(|e| HcomError::DatabaseError(e.to_string()))?;

                    match data {
                        Some(d) => {
                            let has_session = d
                                .get("session_id")
                                .and_then(|v| v.as_str())
                                .is_some_and(|s| !s.is_empty());

                            // Opportunistic session binding for Codex (no SessionStart hook).
                            // First hcom command the Codex agent runs triggers this.
                            // Uses bind_session_to_process for proper resume/placeholder handling.
                            let mut final_name = inst_name.clone();
                            if !has_session {
                                if let Some(thread_id) = codex_thread_id {
                                    if !thread_id.is_empty() {
                                        if let Some(resolved) =
                                            crate::instance_binding::bind_session_to_process(
                                                db, thread_id, process_id,
                                            )
                                        {
                                            final_name = resolved;
                                        }
                                    }
                                }
                            }

                            // Re-read instance data — session_id may have been set during binding
                            let final_data = db
                                .get_instance(&final_name)
                                .map_err(|e| HcomError::DatabaseError(e.to_string()))?
                                .unwrap_or(d);

                            let sid = final_data
                                .get("session_id")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_string());

                            crate::log::log_info(
                                "identity",
                                "resolve",
                                &format!(
                                    "method=process_binding, name={}, process_id={}",
                                    final_name, pid
                                ),
                            );
                            return Ok(SenderIdentity {
                                kind: SenderKind::Instance,
                                name: final_name,
                                session_id: sid,
                                instance_data: Some(final_data),
                            });
                        }
                        None => {
                            crate::log::log_warn(
                                "identity",
                                "resolve.process_instance_missing",
                                &format!("process_id={}, bound_name={}", pid, inst_name),
                            );
                            return Err(HcomError::NotFound(instance_not_found_error(&inst_name)));
                        }
                    }
                }
                None => {
                    if identity_expected {
                        crate::log::log_warn(
                            "identity",
                            "resolve.process_binding_expired",
                            &format!("process_id={}", pid),
                        );
                    }
                    return Err(HcomError::IdentityRequired(
                        "Session expired. Run 'hcom start' to reconnect.".to_string(),
                    ));
                }
            }
        }
    }

    // 5. Transcript marker fallback (hook extension point)
    if let Some(fallback) = transcript_fallback {
        if let Some(identity) = fallback(db) {
            return Ok(identity);
        }
    }

    // 6. No identity
    if identity_expected {
        crate::log::log_warn(
            "identity",
            "resolve.no_identity",
            &format!(
                "has_process_id={}, has_name={}",
                process_id.is_some_and(|s| !s.is_empty()),
                name.is_some_and(|s| !s.is_empty()),
            ),
        );
    }
    Err(HcomError::IdentityRequired(
        "No hcom identity. Run 'hcom start' first, then use --name <yourname> on commands."
            .to_string(),
    ))
}

#[allow(clippy::type_complexity)]
pub fn resolve_identity(
    db: &HcomDb,
    name: Option<&str>,
    system_sender: Option<&str>,
    session_id: Option<&str>,
    process_id: Option<&str>,
    codex_thread_id: Option<&str>,
    transcript_fallback: Option<&dyn Fn(&HcomDb) -> Option<SenderIdentity>>,
) -> Result<SenderIdentity, HcomError> {
    resolve_identity_with_expectation(
        db,
        name,
        system_sender,
        session_id,
        process_id,
        codex_thread_id,
        crate::shared::is_inside_ai_tool(),
        transcript_fallback,
    )
}

/// Check if a command requires identity gating.
pub fn requires_identity(cmd: &str) -> bool {
    REQUIRE_IDENTITY.contains(&cmd)
}

/// Identity gate check for CLI commands.
///
/// Returns `Ok(())` if the identity requirement is satisfied, or `Err` with
/// an actionable error message. The `send` command bypasses the gate when
/// external sender flags (`--from`, `-b`) are present.
pub fn require_identity_gate(
    cmd: &str,
    explicit_name: Option<&str>,
    has_from_flag: bool,
) -> Result<(), HcomError> {
    if !requires_identity(cmd) {
        return Ok(());
    }

    // --name provided: identity will be resolved later
    if explicit_name.is_some() {
        return Ok(());
    }

    // send command: --from or -b bypasses identity requirement
    if cmd == "send" && has_from_flag {
        return Ok(());
    }

    Err(HcomError::IdentityRequired(format!(
        "'{cmd}' requires identity. Use --name <yourname> or run inside an hcom-launched session."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_db() -> (HcomDb, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (db, dir)
    }

    fn insert_instance(db: &HcomDb, name: &str, session_id: Option<&str>, tag: Option<&str>) {
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO instances (name, session_id, tag, status, created_at, tool)
             VALUES (?1, ?2, ?3, 'active', ?4, 'claude')",
                rusqlite::params![name, session_id, tag, now],
            )
            .unwrap();
    }

    fn insert_process_binding(db: &HcomDb, process_id: &str, instance_name: &str) {
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO process_bindings (process_id, instance_name, updated_at)
             VALUES (?1, ?2, ?3)",
                rusqlite::params![process_id, instance_name, now],
            )
            .unwrap();
    }

    fn insert_session_binding(db: &HcomDb, session_id: &str, instance_name: &str) {
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn()
            .execute(
                "INSERT INTO session_bindings (session_id, instance_name, created_at)
             VALUES (?1, ?2, ?3)",
                rusqlite::params![session_id, instance_name, now],
            )
            .unwrap();
    }

    // ── Name validation tests ──────────────────────────────────────────

    #[test]
    fn test_looks_like_uuid() {
        assert!(looks_like_uuid("12345678-1234-1234-1234-123456789abc"));
        assert!(looks_like_uuid("ABCDEF01-2345-6789-ABCD-EF0123456789"));
        assert!(!looks_like_uuid("not-a-uuid"));
        assert!(!looks_like_uuid("12345678-1234-1234-1234-12345678"));
    }

    #[test]
    fn test_looks_like_agent_id() {
        assert!(looks_like_agent_id("a6d9caf"));
        assert!(looks_like_agent_id("1234567"));
        assert!(!looks_like_agent_id("a6d9ca")); // too short
        assert!(!looks_like_agent_id("a6d9cafg")); // too long
        assert!(!looks_like_agent_id("a6d9caz")); // non-hex
    }

    #[test]
    fn test_is_valid_base_name() {
        assert!(is_valid_base_name("luna"));
        assert!(is_valid_base_name("test_name_123"));
        assert!(!is_valid_base_name("Luna")); // uppercase
        assert!(!is_valid_base_name("my-name")); // dash
        assert!(!is_valid_base_name("")); // empty
        assert!(!is_valid_base_name("name with space"));
    }

    #[test]
    fn test_instance_not_found_error() {
        // Normal name: suggest --as
        let err = instance_not_found_error("luna");
        assert!(err.contains("--as luna"));

        // Agent ID: don't suggest --as
        let err = instance_not_found_error("a6d9caf");
        assert!(err.contains("Stop working"));
        assert!(!err.contains("--as"));
    }

    #[test]
    fn test_validate_name_input() {
        // Valid
        assert!(validate_name_input("luna", 50, true).is_ok());
        assert!(validate_name_input("test_name", 50, true).is_ok());

        // Too long
        let long_name = "a".repeat(51);
        let err = validate_name_input(&long_name, 50, true).unwrap_err();
        assert!(err.contains("too long"));

        // Dangerous chars
        let err = validate_name_input("name;evil", 50, true).unwrap_err();
        assert!(err.contains("invalid characters"));

        // @ allowed by default
        assert!(validate_name_input("@luna", 50, true).is_ok());

        // @ rejected when allow_at=false
        assert!(validate_name_input("@luna", 50, false).is_err());
    }

    // ── resolve_from_name tests ────────────────────────────────────────

    #[test]
    fn test_resolve_from_name_exact() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", Some("sess-1"), None);

        let identity = resolve_from_name(&db, "luna").unwrap();
        assert_eq!(identity.name, "luna");
        assert!(matches!(identity.kind, SenderKind::Instance));
        assert_eq!(identity.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn test_resolve_from_name_agent_id() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", None, None);
        // Set agent_id
        db.conn().execute(
            "UPDATE instances SET agent_id = '12345678-1234-1234-1234-123456789abc' WHERE name = 'luna'",
            [],
        ).unwrap();

        let identity = resolve_from_name(&db, "12345678-1234-1234-1234-123456789abc").unwrap();
        assert_eq!(identity.name, "luna");
    }

    #[test]
    fn test_resolve_from_name_tag_name() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", None, Some("team"));

        let identity = resolve_from_name(&db, "team-luna").unwrap();
        assert_eq!(identity.name, "luna");
    }

    #[test]
    fn test_resolve_from_name_not_found() {
        let (db, _dir) = make_test_db();

        let err = resolve_from_name(&db, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_resolve_from_name_invalid() {
        let (db, _dir) = make_test_db();

        let err = resolve_from_name(&db, "Invalid-Name!").unwrap_err();
        assert!(err.to_string().contains("Invalid instance name"));
    }

    // ── resolve_identity tests ─────────────────────────────────────────

    #[test]
    fn test_resolve_identity_system_sender() {
        let (db, _dir) = make_test_db();

        let identity =
            resolve_identity(&db, None, Some("hcom-launcher"), None, None, None, None).unwrap();
        assert!(matches!(identity.kind, SenderKind::System));
        assert_eq!(identity.name, "hcom-launcher");
    }

    #[test]
    fn test_resolve_identity_session_id() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", Some("sess-1"), None);
        insert_session_binding(&db, "sess-1", "luna");

        let identity = resolve_identity(&db, None, None, Some("sess-1"), None, None, None).unwrap();
        assert!(matches!(identity.kind, SenderKind::Instance));
        assert_eq!(identity.name, "luna");
        assert_eq!(identity.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn test_resolve_identity_name() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", None, None);

        let identity = resolve_identity(&db, Some("luna"), None, None, None, None, None).unwrap();
        assert_eq!(identity.name, "luna");
    }

    #[test]
    fn test_resolve_identity_process_binding() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", Some("sess-1"), None);
        insert_process_binding(&db, "pid-123", "luna");

        let identity =
            resolve_identity(&db, None, None, None, Some("pid-123"), None, None).unwrap();
        assert_eq!(identity.name, "luna");
        assert_eq!(identity.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn test_resolve_identity_process_binding_codex_session_bind() {
        let (db, _dir) = make_test_db();
        // Instance without session_id
        insert_instance(&db, "luna", None, None);
        insert_process_binding(&db, "pid-123", "luna");

        let identity = resolve_identity(
            &db,
            None,
            None,
            None,
            Some("pid-123"),
            Some("thread-abc"),
            None,
        )
        .unwrap();
        assert_eq!(identity.name, "luna");
        // Session should now be bound
        assert_eq!(identity.session_id.as_deref(), Some("thread-abc"));

        // Verify binding was persisted
        let bound = db.get_session_binding("thread-abc").unwrap();
        assert_eq!(bound, Some("luna".to_string()));
    }

    #[test]
    fn test_resolve_identity_process_binding_expired() {
        let (db, _dir) = make_test_db();
        // No process binding exists

        let err = resolve_identity(&db, None, None, None, Some("pid-123"), None, None).unwrap_err();
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn test_resolve_identity_transcript_fallback() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "nova", Some("sess-2"), None);

        let fallback = |_db: &HcomDb| -> Option<SenderIdentity> {
            Some(SenderIdentity {
                kind: SenderKind::Instance,
                name: "nova".to_string(),
                instance_data: None,
                session_id: Some("sess-2".to_string()),
            })
        };

        let identity =
            resolve_identity(&db, None, None, None, None, None, Some(&fallback)).unwrap();
        assert_eq!(identity.name, "nova");
    }

    #[test]
    fn test_resolve_identity_no_identity() {
        let (db, _dir) = make_test_db();

        let err = resolve_identity(&db, None, None, None, None, None, None).unwrap_err();
        assert!(err.to_string().contains("No hcom identity"));
    }

    #[test]
    fn test_resolve_identity_priority_system_over_name() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", None, None);

        // system_sender takes priority over name
        let identity = resolve_identity(
            &db,
            Some("luna"),
            Some("hcom-launcher"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(matches!(identity.kind, SenderKind::System));
        assert_eq!(identity.name, "hcom-launcher");
    }

    #[test]
    fn test_resolve_identity_priority_session_over_name() {
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", Some("sess-1"), None);
        insert_instance(&db, "nova", Some("sess-2"), None);
        insert_session_binding(&db, "sess-1", "luna");

        // session_id takes priority over name
        let identity =
            resolve_identity(&db, Some("nova"), None, Some("sess-1"), None, None, None).unwrap();
        assert_eq!(identity.name, "luna");
    }

    // ── Identity gating tests ──────────────────────────────────────────

    #[test]
    fn test_requires_identity() {
        assert!(requires_identity("send"));
        assert!(requires_identity("listen"));
        assert!(!requires_identity("list"));
        assert!(!requires_identity("status"));
        assert!(!requires_identity("events"));
    }

    #[test]
    fn test_identity_gate_non_gated_command() {
        assert!(require_identity_gate("list", None, false).is_ok());
    }

    #[test]
    fn test_identity_gate_with_name() {
        assert!(require_identity_gate("send", Some("luna"), false).is_ok());
    }

    #[test]
    fn test_identity_gate_send_with_from() {
        assert!(require_identity_gate("send", None, true).is_ok());
    }

    #[test]
    fn test_identity_gate_send_no_identity() {
        let err = require_identity_gate("send", None, false).unwrap_err();
        assert!(err.to_string().contains("requires identity"));
    }

    #[test]
    fn test_identity_gate_listen_no_identity() {
        let err = require_identity_gate("listen", None, false).unwrap_err();
        assert!(err.to_string().contains("requires identity"));
    }

    // ── Codex resume regression tests ──────────────────────────────────

    #[test]
    fn test_codex_session_bind_resume_switches_to_canonical() {
        // Regression: Codex thread_id matches an existing session binding (resume scenario).
        // Must switch identity to canonical instance, not stay on placeholder.
        let (db, _dir) = make_test_db();
        insert_instance(&db, "canonical", Some("thread-resume"), None);
        insert_session_binding(&db, "thread-resume", "canonical");
        insert_instance(&db, "placeholder", None, None);
        insert_process_binding(&db, "pid-codex", "placeholder");

        let identity = resolve_identity(
            &db,
            None,
            None,
            None,
            Some("pid-codex"),
            Some("thread-resume"),
            None,
        )
        .unwrap();

        // Must resolve to canonical, not placeholder
        assert_eq!(identity.name, "canonical");
        assert_eq!(identity.session_id.as_deref(), Some("thread-resume"));

        // Placeholder should be deleted (was true placeholder)
        assert!(db.get_instance("placeholder").unwrap().is_none());
    }

    #[test]
    fn test_codex_session_bind_already_bound_same_instance() {
        // Session already bound to the same instance we're on → no-op, no crash
        let (db, _dir) = make_test_db();
        insert_instance(&db, "luna", Some("thread-same"), None);
        insert_session_binding(&db, "thread-same", "luna");
        insert_process_binding(&db, "pid-codex", "luna");

        let identity = resolve_identity(
            &db,
            None,
            None,
            None,
            Some("pid-codex"),
            Some("thread-same"),
            None,
        )
        .unwrap();

        assert_eq!(identity.name, "luna");
        assert_eq!(identity.session_id.as_deref(), Some("thread-same"));
    }
}
