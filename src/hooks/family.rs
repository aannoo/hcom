//! Tool family detection, hook→tool mapping, and per-tool helpers.
//!
//! and tool-specific settings module patterns.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::db::HcomDb;
use crate::instances;
use crate::log;

/// Map tool categories to specific tool names, per AI tool.
///
/// Used by extract_tool_detail() to determine what detail to show
/// in status display for each tool invocation.
///
pub static TOOL_NAME_MAPPINGS: LazyLock<
    HashMap<&'static str, HashMap<&'static str, Vec<&'static str>>>,
> = LazyLock::new(|| {
    let mut m = HashMap::new();

    let mut claude = HashMap::new();
    claude.insert("bash", vec!["Bash"]);
    claude.insert("file", vec!["Write", "Edit"]);
    claude.insert("delegate", vec!["Task"]);
    m.insert("claude", claude);

    let mut gemini = HashMap::new();
    gemini.insert("bash", vec!["run_shell_command"]);
    gemini.insert("file", vec!["write_file", "replace"]);
    gemini.insert("delegate", vec!["delegate_to_agent"]);
    m.insert("gemini", gemini);

    let mut codex = HashMap::new();
    codex.insert("bash", vec!["execute_command", "shell", "shell_command"]);
    codex.insert("file", vec!["apply_patch"]);
    m.insert("codex", codex);

    m
});

/// Extract human-readable detail from tool input for status display.
///
/// Centralizes tool detail extraction across claude/gemini/codex hooks.
/// Returns the relevant field (command for bash, file_path for file ops,
/// prompt for delegate) or empty string if tool not recognized.
///
pub fn extract_tool_detail(tool: &str, tool_name: &str, tool_input: &serde_json::Value) -> String {
    let Some(mappings) = TOOL_NAME_MAPPINGS.get(tool) else {
        return String::new();
    };

    for (category, tool_names) in mappings {
        if tool_names.contains(&tool_name) {
            return match *category {
                "bash" => tool_input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                "file" => tool_input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                "delegate" => tool_input
                    .get("prompt")
                    .or_else(|| tool_input.get("task"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
        }
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

    #[test]
    fn test_tool_name_mappings_claude() {
        let mappings = TOOL_NAME_MAPPINGS.get("claude").unwrap();
        assert!(mappings["bash"].contains(&"Bash"));
        assert!(mappings["file"].contains(&"Write"));
        assert!(mappings["file"].contains(&"Edit"));
        assert!(mappings["delegate"].contains(&"Task"));
    }

    #[test]
    fn test_tool_name_mappings_gemini() {
        let mappings = TOOL_NAME_MAPPINGS.get("gemini").unwrap();
        assert!(mappings["bash"].contains(&"run_shell_command"));
        assert!(mappings["file"].contains(&"write_file"));
        assert!(mappings["delegate"].contains(&"delegate_to_agent"));
    }

    #[test]
    fn test_tool_name_mappings_codex() {
        let mappings = TOOL_NAME_MAPPINGS.get("codex").unwrap();
        assert!(mappings["bash"].contains(&"execute_command"));
        assert!(mappings["file"].contains(&"apply_patch"));
        assert!(!mappings.contains_key("delegate"));
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
