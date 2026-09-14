//! Sender identity and command context types for message routing.

/// Sender identity for message routing.
#[derive(Debug, Clone)]
pub struct SenderIdentity {
    /// Identity type: determines routing behavior.
    pub kind: SenderKind,
    /// Display name stored in events.instance column.
    pub name: String,
    /// Full instance data from DB (for kind=Instance only).
    pub instance_data: Option<serde_json::Value>,
    /// Claude session ID for transcript binding.
    pub session_id: Option<String>,
}

/// Sender identity kind — determines routing rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderKind {
    /// Registered hcom participant (full routing rules apply).
    Instance,
    /// External sender via --from flag (broadcasts to all).
    External,
    /// System-generated message (broadcasts to all).
    System,
}

impl SenderIdentity {
    /// External and system senders broadcast to everyone.
    pub fn broadcasts(&self) -> bool {
        matches!(self.kind, SenderKind::External | SenderKind::System)
    }

    /// Group session ID for routing (session-based group membership).
    ///
    /// For subagents: uses parent_session_id (groups them with parent).
    /// For parents: uses own session_id.
    pub fn group_id(&self) -> Option<&str> {
        let data = self.instance_data.as_ref()?;
        // Subagent — use parent_session_id
        if let Some(parent_sid) = data.get("parent_session_id").and_then(|v| v.as_str())
            && !parent_sid.is_empty()
        {
            return Some(parent_sid);
        }
        // Parent — use own session_id
        data.get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    }
}

/// Resolved identity context for a single CLI invocation.
#[derive(Debug, Clone)]
pub struct CommandContext {
    /// Raw `--name` value (if provided).
    pub explicit_name: Option<String>,
    /// Resolved instance identity (best-effort; may be None).
    pub identity: Option<SenderIdentity>,
    /// Whether --go flag was provided.
    pub go: bool,
    /// Warning to print when `--name` disagrees with the identity this shell
    /// resolves to. `None` when they agree or the shell has no identity.
    pub identity_warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sender_identity_broadcasts() {
        let instance = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: None,
            session_id: None,
        };
        assert!(!instance.broadcasts());

        let external = SenderIdentity {
            kind: SenderKind::External,
            name: "user".into(),
            instance_data: None,
            session_id: None,
        };
        assert!(external.broadcasts());

        let system = SenderIdentity {
            kind: SenderKind::System,
            name: "hcom".into(),
            instance_data: None,
            session_id: None,
        };
        assert!(system.broadcasts());
    }

    #[test]
    fn test_sender_identity_group_id() {
        let parent = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: Some(serde_json::json!({"session_id": "sess-123"})),
            session_id: None,
        };
        assert_eq!(parent.group_id(), Some("sess-123"));

        let subagent = SenderIdentity {
            kind: SenderKind::Instance,
            name: "sub1".into(),
            instance_data: Some(serde_json::json!({
                "session_id": "sub-sess",
                "parent_session_id": "parent-sess"
            })),
            session_id: None,
        };
        assert_eq!(subagent.group_id(), Some("parent-sess"));

        let no_data = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: None,
            session_id: None,
        };
        assert_eq!(no_data.group_id(), None);

        let empty = SenderIdentity {
            kind: SenderKind::Instance,
            name: "luna".into(),
            instance_data: Some(serde_json::json!({})),
            session_id: None,
        };
        assert_eq!(empty.group_id(), None);
    }
}
