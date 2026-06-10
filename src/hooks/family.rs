//! Tool family detection, hook→tool mapping, and per-tool helpers.
//!
//! and tool-specific settings module patterns.

use crate::db::HcomDb;
use crate::instances;
use crate::log;
use crate::tool::Tool;

/// Extract human-readable detail from tool input for status display.
///
/// Reads per-tool tool-name categories from `IntegrationSpec.status_detail`.
/// Returns the relevant field (command for bash, file_path for file ops,
/// prompt for delegate) or empty string if tool not recognized.
pub fn extract_tool_detail(tool: &str, tool_name: &str, tool_input: &serde_json::Value) -> String {
    let Ok(tool_enum) = tool.parse::<Tool>() else {
        return String::new();
    };
    let detail = &tool_enum.spec().status_detail;

    if detail.bash.contains(&tool_name) {
        return tool_input
            .get("command")
            .or_else(|| tool_input.get("CommandLine"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    let is_notebook_edit = tool_enum == Tool::Claude && tool_name == "NotebookEdit";
    if detail.file.contains(&tool_name) || is_notebook_edit {
        return tool_input
            .get("file_path")
            .or_else(|| tool_input.get("notebook_path")) // claude NotebookEdit
            .or_else(|| tool_input.get("TargetFile")) // antigravity
            .or_else(|| tool_input.get("path")) // cursor/copilot
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    if detail.delegate.contains(&tool_name) {
        return tool_input
            .get("prompt")
            .or_else(|| tool_input.get("task"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }

    String::new()
}

/// Persist vanilla instance binding (session + transcript + tool).
///
/// Called after marker extraction (each tool extracts differently).
/// Returns instance_name on success or error, None only if nothing to bind.
///
pub fn bind_vanilla_instance(
    db: &HcomDb,
    instance_name: &str,
    session_id: Option<&str>,
    transcript_path: Option<&str>,
    tool: &str,
    hook: &str,
) -> Option<String> {
    if session_id.is_none() && transcript_path.is_none() {
        return Some(instance_name.to_string());
    }

    let result: Result<(), anyhow::Error> = (|| {
        let mut updates = serde_json::Map::new();
        updates.insert("tool".into(), serde_json::Value::String(tool.to_string()));

        if let Some(sid) = session_id {
            updates.insert(
                "session_id".into(),
                serde_json::Value::String(sid.to_string()),
            );
            db.rebind_instance_session(instance_name, sid)?;
        }

        if let Some(tp) = transcript_path {
            updates.insert(
                "transcript_path".into(),
                serde_json::Value::String(tp.to_string()),
            );
        }

        instances::update_instance_position(db, instance_name, &updates);
        log::log_info(
            "hooks",
            &format!("{}.bind.success", tool),
            &format!("instance={} session_id={:?}", instance_name, session_id),
        );
        Ok(())
    })();

    if let Err(e) = result {
        log::log_error(
            "hooks",
            "hook.error",
            &format!("hook={} op=bind_vanilla err={}", hook, e),
        );
    }

    Some(instance_name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_for(tool: &str) -> &'static crate::integration_spec::StatusDetailSpec {
        &tool.parse::<Tool>().unwrap().spec().status_detail
    }

    #[test]
    fn test_tool_name_mappings_claude() {
        let d = spec_for("claude");
        assert!(d.bash.contains(&"Bash"));
        assert!(d.file.contains(&"Write"));
        assert!(d.file.contains(&"Edit"));
        assert!(d.delegate.contains(&"Task"));
    }

    #[test]
    fn test_tool_name_mappings_gemini() {
        let d = spec_for("gemini");
        assert!(d.bash.contains(&"run_shell_command"));
        assert!(d.file.contains(&"write_file"));
        assert!(d.delegate.contains(&"delegate_to_agent"));
    }

    #[test]
    fn test_tool_name_mappings_codex() {
        let d = spec_for("codex");
        assert!(d.bash.contains(&"execute_command"));
        assert!(d.file.contains(&"apply_patch"));
        assert!(d.delegate.is_empty());
    }

    #[test]
    fn test_tool_name_mappings_antigravity() {
        let d = spec_for("antigravity");
        assert!(d.bash.contains(&"run_command"));
        assert!(d.file.contains(&"write_to_file"));
        assert!(d.file.contains(&"replace_file_content"));
        assert!(d.file.contains(&"multi_replace_file_content"));
        assert!(d.delegate.contains(&"invoke_subagent"));
    }

    #[test]
    fn test_extract_tool_detail_bash() {
        let input = serde_json::json!({"command": "ls -la"});
        assert_eq!(extract_tool_detail("claude", "Bash", &input), "ls -la");
        assert_eq!(
            extract_tool_detail("gemini", "run_shell_command", &input),
            "ls -la"
        );
        assert_eq!(
            extract_tool_detail("codex", "execute_command", &input),
            "ls -la"
        );
    }

    #[test]
    fn test_extract_tool_detail_antigravity_bash() {
        let input = serde_json::json!({"CommandLine": "ls -la"});
        assert_eq!(
            extract_tool_detail("antigravity", "run_command", &input),
            "ls -la"
        );
    }

    #[test]
    fn test_extract_tool_detail_antigravity_file() {
        let input = serde_json::json!({"TargetFile": "/src/main.rs"});
        assert_eq!(
            extract_tool_detail("antigravity", "write_to_file", &input),
            "/src/main.rs"
        );
    }

    #[test]
    fn test_extract_tool_detail_file() {
        let input = serde_json::json!({"file_path": "/src/main.rs"});
        assert_eq!(
            extract_tool_detail("claude", "Write", &input),
            "/src/main.rs"
        );
        assert_eq!(
            extract_tool_detail("gemini", "write_file", &input),
            "/src/main.rs"
        );
    }

    #[test]
    fn test_extract_tool_detail_notebook_edit() {
        let input = serde_json::json!({"notebook_path": "/src/analysis.ipynb"});
        assert_eq!(
            extract_tool_detail("claude", "NotebookEdit", &input),
            "/src/analysis.ipynb"
        );
    }

    #[test]
    fn test_extract_tool_detail_covers_all_registered_operations() {
        let input = serde_json::json!({
            "command": "echo ok",
            "CommandLine": "echo ok",
            "file_path": "/src/main.rs",
            "TargetFile": "/src/main.rs",
            "path": "/src/main.rs",
            "prompt": "delegate",
            "task": "delegate",
        });

        for spec in crate::integration_spec::ALL {
            for operation in spec.status_detail.bash {
                assert!(
                    !extract_tool_detail(spec.name, operation, &input).is_empty(),
                    "shell detail extraction missing for {} tool:{}",
                    spec.name,
                    operation
                );
            }
            for operation in spec.status_detail.file {
                assert!(
                    !extract_tool_detail(spec.name, operation, &input).is_empty(),
                    "file detail extraction missing for {} tool:{}",
                    spec.name,
                    operation
                );
            }
            for operation in spec.status_detail.delegate {
                assert!(
                    !extract_tool_detail(spec.name, operation, &input).is_empty(),
                    "delegate detail extraction missing for {} tool:{}",
                    spec.name,
                    operation
                );
            }
        }
    }

    #[test]
    fn test_extract_tool_detail_delegate() {
        let input = serde_json::json!({"prompt": "analyze this code"});
        assert_eq!(
            extract_tool_detail("claude", "Task", &input),
            "analyze this code"
        );

        // Fallback to "task" field
        let input2 = serde_json::json!({"task": "do something"});
        assert_eq!(
            extract_tool_detail("gemini", "delegate_to_agent", &input2),
            "do something"
        );
    }

    #[test]
    fn test_extract_tool_detail_cursor() {
        // Shell command (and the run_terminal_cmd variant) → command field.
        let shell = serde_json::json!({"command": "cargo build", "description": "build"});
        assert_eq!(
            extract_tool_detail("cursor", "Shell", &shell),
            "cargo build"
        );
        assert_eq!(
            extract_tool_detail("cursor", "run_terminal_cmd", &shell),
            "cargo build"
        );
        // Edit (StrReplace) and Write → `path` field (cursor uses `path`, not file_path).
        let edit =
            serde_json::json!({"path": "/src/main.rs", "old_string": "a", "new_string": "b"});
        assert_eq!(
            extract_tool_detail("cursor", "StrReplace", &edit),
            "/src/main.rs"
        );
        let write = serde_json::json!({"path": "/src/lib.rs", "contents": "x"});
        assert_eq!(
            extract_tool_detail("cursor", "Write", &write),
            "/src/lib.rs"
        );
        // Delegate (Task/Subagent) → prompt field.
        let task = serde_json::json!({"prompt": "explore the codebase", "subagent_type": "x"});
        assert_eq!(
            extract_tool_detail("cursor", "Task", &task),
            "explore the codebase"
        );
        assert_eq!(
            extract_tool_detail("cursor", "Subagent", &task),
            "explore the codebase"
        );
        // `Edit` is a Claude tool name, never emitted by cursor → no detail.
        assert_eq!(extract_tool_detail("cursor", "Edit", &edit), "");
    }

    #[test]
    fn test_extract_tool_detail_copilot() {
        // Shell (bash/powershell) → command field.
        let shell = serde_json::json!({"command": "cargo build"});
        assert_eq!(
            extract_tool_detail("copilot", "bash", &shell),
            "cargo build"
        );
        assert_eq!(
            extract_tool_detail("copilot", "powershell", &shell),
            "cargo build"
        );
        // File tools (create/edit/apply_patch) → `path` field (copilot uses `path`).
        let edit = serde_json::json!({"path": "/src/main.rs"});
        assert_eq!(
            extract_tool_detail("copilot", "edit", &edit),
            "/src/main.rs"
        );
        assert_eq!(
            extract_tool_detail("copilot", "create", &edit),
            "/src/main.rs"
        );
        assert_eq!(
            extract_tool_detail("copilot", "apply_patch", &edit),
            "/src/main.rs"
        );
        // Delegate (task) → prompt field.
        let task = serde_json::json!({"prompt": "explore the codebase"});
        assert_eq!(
            extract_tool_detail("copilot", "task", &task),
            "explore the codebase"
        );
        // Claude-style names are never emitted by copilot → no detail.
        assert_eq!(extract_tool_detail("copilot", "Bash", &shell), "");
    }

    #[test]
    fn test_extract_tool_detail_unknown() {
        let input = serde_json::json!({"command": "ls"});
        assert_eq!(extract_tool_detail("claude", "UnknownTool", &input), "");
        assert_eq!(extract_tool_detail("unknown_tool", "Bash", &input), "");
    }

    #[test]
    fn test_extract_tool_detail_missing_field() {
        let input = serde_json::json!({});
        assert_eq!(extract_tool_detail("claude", "Bash", &input), "");
    }

    fn make_test_db() -> (tempfile::TempDir, crate::db::HcomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = crate::db::HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (dir, db)
    }

    fn insert_test_instance(db: &crate::db::HcomDb, name: &str) {
        let now = chrono::Utc::now().timestamp() as f64;
        db.conn().execute(
            "INSERT INTO instances (name, status, created_at, tool) VALUES (?1, 'active', ?2, 'claude')",
            rusqlite::params![name, now],
        ).unwrap();
    }

    #[test]
    fn test_bind_vanilla_instance_with_session() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();
        insert_test_instance(&db, "luna");

        let result =
            bind_vanilla_instance(&db, "luna", Some("sess-v1"), None, "claude", "PostToolUse");
        assert_eq!(result, Some("luna".to_string()));

        // Session binding should be created
        assert_eq!(
            db.get_session_binding("sess-v1").unwrap(),
            Some("luna".to_string())
        );

        // Instance should have session_id set
        let inst = db.get_instance_full("luna").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sess-v1"));
    }

    #[test]
    fn test_bind_vanilla_instance_with_transcript() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();
        insert_test_instance(&db, "nova");

        let result = bind_vanilla_instance(
            &db,
            "nova",
            None,
            Some("/tmp/transcript.jsonl"),
            "gemini",
            "AfterTool",
        );
        assert_eq!(result, Some("nova".to_string()));

        // Instance should have transcript_path and tool updated
        let inst = db.get_instance_full("nova").unwrap().unwrap();
        assert_eq!(inst.transcript_path, "/tmp/transcript.jsonl");
        assert_eq!(inst.tool, "gemini");
    }

    #[test]
    fn test_bind_vanilla_instance_no_session_no_transcript() {
        // Early return — no binding to do
        crate::config::Config::init();
        let (_dir, db) = make_test_db();
        insert_test_instance(&db, "miso");

        let result = bind_vanilla_instance(&db, "miso", None, None, "claude", "PostToolUse");
        assert_eq!(result, Some("miso".to_string()));

        // No session binding should exist
        let bindings: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM session_bindings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bindings, 0);
    }

    #[test]
    fn test_bind_vanilla_instance_with_both() {
        crate::config::Config::init();
        let (_dir, db) = make_test_db();
        insert_test_instance(&db, "kira");

        let result = bind_vanilla_instance(
            &db,
            "kira",
            Some("sess-v2"),
            Some("/tmp/t2.jsonl"),
            "codex",
            "PostToolUse",
        );
        assert_eq!(result, Some("kira".to_string()));

        assert_eq!(
            db.get_session_binding("sess-v2").unwrap(),
            Some("kira".to_string())
        );
        let inst = db.get_instance_full("kira").unwrap().unwrap();
        assert_eq!(inst.session_id.as_deref(), Some("sess-v2"));
        assert_eq!(inst.transcript_path, "/tmp/t2.jsonl");
        assert_eq!(inst.tool, "codex");
    }
}
