//! Shared hook infrastructure for all tools (Claude, Gemini, Codex, OpenCode).

pub mod claude;
pub mod claude_args;
pub mod cline;
pub mod codex;
pub mod codex_file_edits;
pub mod common;
pub mod family;
pub mod gemini;
pub mod kilo;
pub mod opencode;
pub mod utils;

use serde_json::Value;

/// Shared test helpers for hook test modules (claude, codex, gemini).
#[cfg(test)]
pub mod test_helpers {
    use std::path::PathBuf;

    /// RAII guard that saves/restores HCOM_DIR and HOME env vars, and resets Config.
    pub struct EnvGuard {
        saved_hcom: Option<String>,
        saved_home: Option<String>,
        saved_test_codex_cli_version: Option<String>,
    }

    impl Default for EnvGuard {
        fn default() -> Self {
            Self::new()
        }
    }

    impl EnvGuard {
        pub fn new() -> Self {
            Self {
                saved_hcom: std::env::var("HCOM_DIR").ok(),
                saved_home: std::env::var("HOME").ok(),
                saved_test_codex_cli_version: std::env::var("HCOM_TEST_CODEX_CLI_VERSION").ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.saved_hcom {
                    Some(v) => std::env::set_var("HCOM_DIR", v),
                    None => std::env::remove_var("HCOM_DIR"),
                }
                match &self.saved_home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match &self.saved_test_codex_cli_version {
                    Some(v) => std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", v),
                    None => std::env::remove_var("HCOM_TEST_CODEX_CLI_VERSION"),
                }
            }
            crate::config::Config::reset();
            crate::config::Config::init();
        }
    }

    /// Create an isolated test env: tempdir with .hcom dir, env vars set.
    /// Returns (tempdir, hcom_dir, test_home, guard).
    pub fn isolated_test_env() -> (tempfile::TempDir, PathBuf, PathBuf, EnvGuard) {
        let guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let test_home = dir.path().to_path_buf();
        let hcom_dir = test_home.join(".hcom");
        std::fs::create_dir_all(&hcom_dir).unwrap();
        unsafe {
            std::env::set_var("HCOM_DIR", &hcom_dir);
            std::env::set_var("HOME", &test_home);
            std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.129.0");
        }
        crate::config::Config::reset();
        crate::config::Config::init();
        (dir, hcom_dir, test_home, guard)
    }
}

// Re-export key types.
pub use common::{
    deliver_pending_messages, finalize_session, find_last_bind_marker, get_pending_instances,
    init_hook_context, inject_bootstrap_once, poll_messages, stop_instance,
};
pub use family::{bind_vanilla_instance, extract_tool_detail};
pub use utils::{HOOK_REGISTRY, HookCategory, HookInfo};

/// Delivery cursor/status update to apply after hook output is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryAck {
    pub instance_name: String,
    pub last_event_id: i64,
    pub status_context: String,
    pub msg_ts: String,
}

/// Normalized hook payload — unified across all tools.
///
/// Each tool's raw hook JSON is different. Factory methods normalize into
/// this common struct so shared functions work identically across tools.
///
#[derive(Debug, Clone)]
pub struct HookPayload {
    /// Claude/Gemini session ID, Codex thread ID. None if not provided.
    pub session_id: Option<String>,
    /// Path to tool's JSONL transcript (Claude) or conversation log. None if not provided.
    pub transcript_path: Option<String>,
    /// Hook name (e.g., "Stop", "PostToolUse", "PreToolUse").
    pub hook_name: String,
    /// Tool type string ("claude", "gemini", "codex", "opencode").
    pub tool: String,
    /// Tool name from hook (e.g., "Bash", "Write" for PostToolUse).
    pub tool_name: String,
    /// Tool input dict (for extract_tool_detail).
    pub tool_input: Value,
    /// Tool result/response (for AfterTool/PostToolUse hooks).
    pub tool_result: String,
    /// Notification type (for Notification hooks, e.g., "ToolPermission").
    pub notification_type: Option<String>,
    /// Raw hook payload for tool-specific access.
    pub raw: Value,
}

impl HookPayload {
    /// Extract a string from the first matching key, or empty string.
    fn str_field(raw: &Value, keys: &[&str]) -> String {
        for key in keys {
            if let Some(s) = raw.get(*key).and_then(|v| v.as_str()) {
                return s.to_string();
            }
        }
        String::new()
    }

    /// Extract an optional string from the first matching key.
    fn opt_str_field(raw: &Value, keys: &[&str]) -> Option<String> {
        for key in keys {
            if let Some(s) = raw.get(*key).and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        None
    }

    /// Extract a value from the first matching key, or empty object.
    fn obj_field(raw: &Value, keys: &[&str]) -> Value {
        for key in keys {
            if let Some(v) = raw.get(*key) {
                return v.clone();
            }
        }
        Value::Object(Default::default())
    }

    /// Build from Claude hook JSON.
    ///
    /// Claude hook stdin format (all keys at root level):
    ///   { "session_id", "transcript_path", "tool_name", "tool_input",
    ///     "tool_response", "notification_type", "agent_id", "agent_type" }
    pub fn from_claude(raw: Value) -> Self {
        let tool_result = match raw.get("tool_response") {
            Some(Value::Object(obj)) => obj
                .get("stdout")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };

        Self {
            session_id: Self::opt_str_field(&raw, &["session_id", "sessionId"]),
            transcript_path: Self::opt_str_field(&raw, &["transcript_path"]),
            hook_name: Self::str_field(&raw, &["hook_name"]),
            tool: "claude".to_string(),
            tool_name: Self::str_field(&raw, &["tool_name"]),
            tool_input: Self::obj_field(&raw, &["tool_input"]),
            tool_result,
            notification_type: Self::opt_str_field(&raw, &["notification_type"]),
            raw,
        }
    }

    /// Build from Gemini hook JSON.
    ///
    /// Gemini hook stdin format (all keys at root level):
    ///   { "session_id"/"sessionId", "transcript_path"/"session_path",
    ///     "tool_name"/"toolName", "tool_input"/"toolInput",
    ///     "tool_response", "notification_type" }
    pub fn from_gemini(raw: Value) -> Self {
        let tool_result = match raw.get("tool_response") {
            Some(Value::Object(obj)) => obj
                .get("llmContent")
                .or_else(|| obj.get("output"))
                .or_else(|| obj.get("response").and_then(|r| r.get("output")))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            Some(v) => v
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string()),
            None => String::new(),
        };

        Self {
            session_id: Self::opt_str_field(&raw, &["session_id", "sessionId"]),
            transcript_path: Self::opt_str_field(&raw, &["transcript_path", "session_path"]),
            hook_name: Self::str_field(&raw, &["hook_name"]),
            tool: "gemini".to_string(),
            tool_name: Self::str_field(&raw, &["tool_name", "toolName"]),
            tool_input: Self::obj_field(&raw, &["tool_input", "toolInput"]),
            tool_result,
            notification_type: Self::opt_str_field(&raw, &["notification_type"]),
            raw,
        }
    }

    /// Build from native Codex hook JSON.
    ///
    /// Codex hooks pass JSON on stdin with snake_case fields such as:
    ///   { "session_id", "transcript_path", "hook_event_name",
    ///     "tool_name", "tool_input", "tool_response", "prompt", "source" }
    pub fn from_codex_native(hook_type: &str, raw: Value) -> Self {
        Self {
            session_id: Self::opt_str_field(&raw, &["session_id"]),
            transcript_path: Self::opt_str_field(&raw, &["transcript_path", "session_path"]),
            hook_name: if hook_type.is_empty() {
                Self::str_field(&raw, &["hook_event_name"])
            } else {
                hook_type.to_string()
            },
            tool: "codex".to_string(),
            tool_name: Self::str_field(&raw, &["tool_name"]),
            tool_input: Self::obj_field(&raw, &["tool_input"]),
            tool_result: match raw.get("tool_response") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            },
            notification_type: None,
            raw,
        }
    }

    /// Build from OpenCode hook JSON.
    ///
    /// OpenCode hooks: session_id from env, minimal tool info.
    pub fn from_opencode(raw: Value) -> Self {
        Self {
            session_id: Self::opt_str_field(&raw, &["session_id"]),
            transcript_path: Self::opt_str_field(&raw, &["transcript_path"]),
            hook_name: Self::str_field(&raw, &["hook_name"]),
            tool: "opencode".to_string(),
            tool_name: Self::str_field(&raw, &["tool_name"]),
            tool_input: Self::obj_field(&raw, &["tool_input"]),
            tool_result: String::new(),
            notification_type: None,
            raw,
        }
    }
}

/// Hook handler result — determines exit code and stdout output.
///
/// the dispatcher into exit codes + JSON output.
#[derive(Debug, Clone)]
pub enum HookResult {
    /// Allow the operation (exit 0, optional additionalContext/systemMessage).
    Allow {
        /// Additional context injected into the model's context window.
        additional_context: Option<String>,
        /// System message update (Claude-specific).
        system_message: Option<String>,
        /// Delivery ack to commit after stdout is successfully written.
        delivery_ack: Option<DeliveryAck>,
    },

    /// Block the operation (exit 2, with reason for blocking).
    /// Used by Stop hook to deliver messages.
    Block {
        /// Reason text (formatted messages for delivery).
        reason: String,
    },

    /// Update the tool input before execution (exit 0, updatedInput field).
    /// Used by PreToolUse to modify tool arguments.
    UpdateInput {
        /// Modified tool input JSON.
        updated_input: Value,
    },
}

impl HookResult {
    /// Exit code for this result.
    pub fn exit_code(&self) -> i32 {
        match self {
            HookResult::Allow { .. } => 0,
            HookResult::Block { .. } => 2,
            HookResult::UpdateInput { .. } => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_payload_from_claude() {
        // Matches actual Claude hook stdin: all keys at root level
        let raw = serde_json::json!({
            "session_id": "sess-123",
            "transcript_path": "/tmp/transcript.jsonl",
            "hook_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"}
        });
        let payload = HookPayload::from_claude(raw);
        assert_eq!(payload.session_id.as_deref(), Some("sess-123"));
        assert_eq!(
            payload.transcript_path.as_deref(),
            Some("/tmp/transcript.jsonl")
        );
        assert_eq!(payload.hook_name, "PostToolUse");
        assert_eq!(payload.tool, "claude");
        assert_eq!(payload.tool_name, "Bash");
        assert_eq!(payload.notification_type, None);
    }

    #[test]
    fn test_hook_payload_from_gemini() {
        // Matches actual Gemini hook stdin: tool_name/tool_input at root
        let raw = serde_json::json!({
            "session_id": "gem-456",
            "hook_name": "after_tool_call",
            "tool_name": "run_shell_command",
            "tool_input": {"command": "echo hi"}
        });
        let payload = HookPayload::from_gemini(raw);
        assert_eq!(payload.session_id.as_deref(), Some("gem-456"));
        assert_eq!(payload.tool, "gemini");
        assert_eq!(payload.tool_name, "run_shell_command");
        assert_eq!(payload.tool_input["command"], "echo hi");
    }

    #[test]
    fn test_hook_payload_from_codex() {
        // Matches native Codex stdin payload
        let raw = serde_json::json!({
            "session_id": "thread-789",
            "tool_name": "Bash",
            "tool_input": {"command": "pwd"},
            "tool_response": {"output": "ok"}
        });
        let payload = HookPayload::from_codex_native("PostToolUse", raw);
        assert_eq!(payload.session_id.as_deref(), Some("thread-789"));
        assert_eq!(payload.tool, "codex");
        assert_eq!(payload.hook_name, "PostToolUse");
        assert_eq!(payload.tool_name, "Bash");
        assert_eq!(payload.tool_input["command"], "pwd");
    }

    #[test]
    fn test_hook_payload_from_opencode() {
        let raw = serde_json::json!({
            "session_id": "oc-111",
            "hook_name": "PostToolUse",
            "tool_name": "bash",
            "tool_input": {"command": "pwd"}
        });
        let payload = HookPayload::from_opencode(raw);
        assert_eq!(payload.session_id.as_deref(), Some("oc-111"));
        assert_eq!(payload.tool, "opencode");
        assert_eq!(payload.tool_name, "bash");
    }

    #[test]
    fn test_hook_payload_missing_fields() {
        let raw = serde_json::json!({});
        let payload = HookPayload::from_claude(raw);
        assert_eq!(payload.session_id, None);
        assert_eq!(payload.transcript_path, None);
        assert_eq!(payload.tool_name, "");
    }

    #[test]
    fn test_hook_payload_from_gemini_camelcase_fallbacks() {
        // sessionId fallback
        let raw = serde_json::json!({
            "sessionId": "gem-camel",
            "session_path": "/tmp/gemini/chat.json",
            "hook_name": "BeforeAgent"
        });
        let payload = HookPayload::from_gemini(raw);
        assert_eq!(payload.session_id.as_deref(), Some("gem-camel"));
        assert_eq!(
            payload.transcript_path.as_deref(),
            Some("/tmp/gemini/chat.json")
        );
    }

    #[test]
    fn test_hook_payload_from_gemini_tool_response_string() {
        // String tool_response should not be JSON-quoted
        let raw = serde_json::json!({
            "session_id": "gem-1",
            "tool_response": "plain text output"
        });
        let payload = HookPayload::from_gemini(raw);
        assert_eq!(payload.tool_result, "plain text output");
    }

    #[test]
    fn test_hook_payload_from_claude_notification_type() {
        let raw = serde_json::json!({
            "session_id": "claude-1",
            "hook_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash"
        });
        let payload = HookPayload::from_claude(raw);
        assert_eq!(
            payload.notification_type.as_deref(),
            Some("permission_prompt")
        );
    }

    #[test]
    fn test_hook_result_allow() {
        let result = HookResult::Allow {
            additional_context: Some("bootstrap text".into()),
            system_message: None,
            delivery_ack: None,
        };
        assert_eq!(result.exit_code(), 0);
        match &result {
            HookResult::Allow {
                additional_context,
                system_message,
                delivery_ack,
            } => {
                assert_eq!(additional_context.as_deref(), Some("bootstrap text"));
                assert!(system_message.is_none());
                assert!(delivery_ack.is_none());
            }
            _ => panic!("expected Allow"),
        }
    }

    #[test]
    fn test_hook_result_allow_empty() {
        let result = HookResult::Allow {
            additional_context: None,
            system_message: None,
            delivery_ack: None,
        };
        assert_eq!(result.exit_code(), 0);
        match &result {
            HookResult::Allow {
                additional_context,
                system_message,
                delivery_ack,
            } => {
                assert!(additional_context.is_none());
                assert!(system_message.is_none());
                assert!(delivery_ack.is_none());
            }
            _ => panic!("expected Allow"),
        }
    }

    #[test]
    fn test_hook_result_block() {
        let result = HookResult::Block {
            reason: "<hcom>message here</hcom>".into(),
        };
        assert_eq!(result.exit_code(), 2);
        match &result {
            HookResult::Block { reason } => {
                assert_eq!(reason, "<hcom>message here</hcom>");
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn test_hook_result_update_input() {
        let result = HookResult::UpdateInput {
            updated_input: serde_json::json!({"command": "echo modified"}),
        };
        assert_eq!(result.exit_code(), 0);
        match &result {
            HookResult::UpdateInput { updated_input } => {
                assert_eq!(updated_input["command"], "echo modified");
            }
            _ => panic!("expected UpdateInput"),
        }
    }
}
