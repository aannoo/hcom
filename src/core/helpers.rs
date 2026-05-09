//! Input validation utilities for message routing.
//!
//! - Scope and intent validation
//! - @mention matching with prefix/underscore/remote rules

use crate::shared::SenderIdentity;
use crate::shared::constants::extract_mentions;
use crate::shared::identity::SenderKind;

/// Valid scope values for message routing.
pub const VALID_SCOPES: &[&str] = &["broadcast", "mentions"];

/// Valid intent values for message envelope.
pub const VALID_INTENTS: &[&str] = &["ack", "inform", "request"];

/// Validate that scope is a valid value.
pub fn validate_scope(scope: &str) -> Result<(), String> {
    if VALID_SCOPES.contains(&scope) {
        Ok(())
    } else {
        Err(format!(
            "Invalid scope '{}'. Must be one of: {}",
            scope,
            VALID_SCOPES.join(", ")
        ))
    }
}

/// Validate that intent is a valid value.
pub fn validate_intent(intent: &str) -> Result<(), String> {
    if VALID_INTENTS.contains(&intent) {
        Ok(())
    } else {
        Err(format!(
            "Invalid intent '{}'. Must be one of: {}",
            intent,
            VALID_INTENTS.join(", ")
        ))
    }
}

/// Get bundle instance name from a SenderIdentity.
pub fn get_bundle_instance_name(identity: &SenderIdentity) -> String {
    match identity.kind {
        SenderKind::External => format!("ext_{}", identity.name),
        SenderKind::System => format!("sys_{}", identity.name),
        SenderKind::Instance => identity.name.clone(),
    }
}

/// Check if instance is @-mentioned in text using prefix matching.
///
/// Full name is `{tag}-{name}` if tag exists, else just `{name}`.
///
/// Matching rules:
/// - @api-luna matches full name "api-luna" (exact or prefix)
/// - @api- matches all instances with tag "api"
/// - @luna matches base name "luna" (when no tag, or as base name match)
/// - Underscore blocks prefix expansion (reserved for subagent hierarchy)
/// - Bare mentions exclude remote instances (no : in name)
pub fn is_mentioned(text: &str, name: &str, tag: Option<&str>) -> bool {
    let full_name = match tag {
        Some(t) if !t.is_empty() => format!("{}-{}", t, name),
        _ => name.to_string(),
    };

    let mentions = extract_mentions(text);

    for mention in &mentions {
        let mention_lower = mention.to_lowercase();

        if mention.contains(':') {
            // Remote mention — match any instance with prefix
            if full_name.to_lowercase().starts_with(&mention_lower) {
                return true;
            }
        } else {
            // Bare mention — only match local instances (no : in full name)
            // Don't match across underscore boundary (reserved for subagent hierarchy)
            if !full_name.contains(':')
                && full_name.to_lowercase().starts_with(&mention_lower)
                && (full_name.len() == mention.len() || full_name.as_bytes()[mention.len()] != b'_')
            {
                return true;
            }
            // Also check base name match (e.g., @luna matches api-luna)
            if !name.contains(':')
                && name.to_lowercase().starts_with(&mention_lower)
                && (name.len() == mention.len() || name.as_bytes()[mention.len()] != b'_')
            {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Scope/Intent Validation =====

    #[test]
    fn test_validate_scope_valid() {
        assert!(validate_scope("broadcast").is_ok());
        assert!(validate_scope("mentions").is_ok());
    }

    #[test]
    fn test_validate_scope_invalid() {
        let err = validate_scope("unicast").unwrap_err();
        assert!(err.contains("Invalid scope 'unicast'"));
    }

    #[test]
    fn test_validate_intent_valid() {
        assert!(validate_intent("request").is_ok());
        assert!(validate_intent("inform").is_ok());
        assert!(validate_intent("ack").is_ok());
    }

    #[test]
    fn test_validate_intent_invalid() {
        let err = validate_intent("demand").unwrap_err();
        assert!(err.contains("Invalid intent 'demand'"));
    }

    // ===== get_bundle_instance_name =====

    #[test]
    fn test_bundle_instance_name_instance() {
        let id = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: None,
            session_id: None,
            project: None,
        };
        assert_eq!(get_bundle_instance_name(&id), "luna");
    }

    #[test]
    fn test_bundle_instance_name_external() {
        let id = SenderIdentity {
            kind: SenderKind::External,
            name: "user".into(),
            instance_data: None,
            session_id: None,
            project: None,
        };
        assert_eq!(get_bundle_instance_name(&id), "ext_user");
    }

    #[test]
    fn test_bundle_instance_name_system() {
        let id = SenderIdentity {
            kind: SenderKind::System,
            name: "hcom".into(),
            instance_data: None,
            session_id: None,
            project: None,
        };
        assert_eq!(get_bundle_instance_name(&id), "sys_hcom");
    }

    // ===== is_mentioned =====

    #[test]
    fn test_mentioned_exact_full_name() {
        assert!(is_mentioned("Hey @api-luna", "luna", Some("api")));
    }

    #[test]
    fn test_mentioned_tag_prefix() {
        assert!(is_mentioned("Hey @api-", "luna", Some("api")));
    }

    #[test]
    fn test_mentioned_base_name_with_tag() {
        assert!(is_mentioned("Hey @luna", "luna", Some("api")));
    }

    #[test]
    fn test_mentioned_no_tag() {
        assert!(is_mentioned("Hey @luna", "luna", None));
    }

    #[test]
    fn test_mentioned_wrong_tag() {
        assert!(!is_mentioned("Hey @review-luna", "luna", Some("api")));
    }

    #[test]
    fn test_mentioned_underscore_blocks() {
        // @luna should NOT match luna_reviewer_1
        assert!(!is_mentioned("@luna", "luna_reviewer_1", None));
    }

    #[test]
    fn test_mentioned_case_insensitive() {
        assert!(is_mentioned("@Luna", "luna", None));
        assert!(is_mentioned("@LUNA", "luna", None));
    }

    #[test]
    fn test_mentioned_not_in_text() {
        assert!(!is_mentioned("Hello world", "luna", None));
    }

    #[test]
    fn test_mentioned_remote_excluded_from_bare() {
        // Bare @luna should not match remote "luna:BOXE"
        assert!(!is_mentioned("@luna", "luna:BOXE", None));
    }
}
