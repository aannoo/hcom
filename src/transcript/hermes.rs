//! Hermes transcript parser (SQLite `state.db` under HERMES_HOME).
//!
//! Hermes persists sessions in a SQLite database (`state.db`) with `sessions`
//! and `messages` tables. `messages.content` is plain text, or a
//! `\x00json:`-prefixed JSON blob for structured (multimodal) content.
//! Assistant tool calls live in the `tool_calls` JSON column (OpenAI-style),
//! and tool results are `tool`-role rows carrying `tool_call_id` + `tool_name`.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use super::opencode::TranscriptSearchMatch;
use super::shared::{
    Exchange, ToolUse, capture_tool_output, finalize_action_text, is_error_result,
    normalize_tool_name, truncate_str,
};

/// Hermes JSON-encodes non-scalar message content under this sentinel prefix.
const CONTENT_JSON_PREFIX: &str = "\x00json:";

/// Resolve the Hermes state database: `$HERMES_HOME/state.db`, else
/// `~/.hermes/state.db`, only if it exists.
pub(crate) fn get_hermes_db_path() -> Option<PathBuf> {
    let home = if let Ok(dir) = std::env::var("HERMES_HOME")
        && !dir.is_empty()
    {
        PathBuf::from(dir)
    } else {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".hermes")
    };
    let db = home.join("state.db");
    db.exists().then_some(db)
}

/// Decode a stored `content` value: plain strings pass through; sentinel-
/// prefixed JSON (multimodal lists/dicts) is decoded to its JSON value.
fn decode_content(raw: &str) -> Value {
    if let Some(payload) = raw.strip_prefix(CONTENT_JSON_PREFIX) {
        return serde_json::from_str(payload).unwrap_or(Value::String(raw.to_string()));
    }
    Value::String(raw.to_string())
}

/// Extract human-readable text from decoded content (string, list of parts,
/// or nested dict) for use as user text or assistant action text.
fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.trim().to_string(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part.get("text").and_then(Value::as_str)
                } else {
                    part.as_str()
                }
            })
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                return text.trim().to_string();
            }
            if let Some(content) = map.get("content") {
                return content_to_text(content);
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// Display text for tool output: plain strings pass through unwrapped (no JSON
/// quoting), structured content is flattened like user text.
fn output_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.trim().to_string(),
        _ => content_to_text(content),
    }
}

/// Parse Hermes SQLite transcript database for one session.
///
/// Exchanges are grouped around user messages; assistant text, tool calls,
/// and their `tool`-role results are attached to the surrounding turn.
pub(crate) fn parse_hermes_sqlite(
    db_path: &Path,
    session_id: &str,
    last: usize,
) -> Result<Vec<Exchange>, String> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Cannot open Hermes DB: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT role, content, tool_call_id, tool_calls, tool_name, timestamp
             FROM messages
             WHERE session_id = ? AND active = 1
             ORDER BY id ASC",
        )
        .map_err(|e| format!("Query error: {e}"))?;

    struct MsgRow {
        role: String,
        content: Option<String>,
        tool_call_id: Option<String>,
        tool_calls: Option<String>,
        tool_name: Option<String>,
        timestamp: f64,
    }

    let messages: Vec<MsgRow> = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok(MsgRow {
                role: row.get(0)?,
                content: row.get(1)?,
                tool_call_id: row.get(2)?,
                tool_calls: row.get(3)?,
                tool_name: row.get(4)?,
                timestamp: row.get::<_, f64>(5).unwrap_or(0.0),
            })
        })
        .map_err(|e| format!("Query error: {e}"))?
        .filter_map(|r| r.ok())
        .filter_map(|row| {
            if row.role == "system" {
                return None;
            }
            Some(row)
        })
        .collect();

    if messages.is_empty() {
        return Ok(Vec::new());
    }

    // Tool results (role=tool) carry tool_call_id; index them so assistant
    // tool calls can be paired with their outputs.
    let tool_results: std::collections::HashMap<String, &MsgRow> = messages
        .iter()
        .filter(|row| row.role == "tool")
        .filter_map(|row| {
            row.tool_call_id
                .as_ref()
                .filter(|id| !id.is_empty())
                .map(|id| (id.clone(), row))
        })
        .collect();

    // Map of tool_call_id -> (name, is_error, output) built lazily per turn.
    let mut exchanges = Vec::new();
    let mut position = 0;

    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, row)| row.role == "user")
        .map(|(i, _)| i)
        .collect();

    for (ui_pos, &user_idx) in user_indices.iter().enumerate() {
        let next_user_idx = user_indices
            .get(ui_pos + 1)
            .copied()
            .unwrap_or(messages.len());
        let user_row = &messages[user_idx];
        let user_content = decode_content(user_row.content.as_deref().unwrap_or(""));
        let user_text = content_to_text(&user_content);
        if user_text.is_empty() {
            continue;
        }

        let timestamp = if user_row.timestamp > 0.0 {
            chrono::DateTime::from_timestamp(user_row.timestamp as i64, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mut action_parts: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        let mut tools: Vec<ToolUse> = Vec::new();
        let mut errors: Vec<Value> = Vec::new();

        for row in &messages[(user_idx + 1)..next_user_idx] {
            match row.role.as_str() {
                "assistant" => {
                    let content = decode_content(row.content.as_deref().unwrap_or(""));
                    let text = content_to_text(&content);
                    if !text.is_empty() {
                        action_parts.push(text);
                    }
                    if let Some(tool_calls_json) = &row.tool_calls {
                        if let Ok(tool_calls) = serde_json::from_str::<Value>(tool_calls_json) {
                            for call in tool_calls.as_array().into_iter().flatten() {
                                let name = call
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(Value::as_str)
                                    .or_else(|| call.get("name").and_then(Value::as_str))
                                    .unwrap_or("unknown");
                                let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                                let arguments = call
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("{}");
                                let normalized = normalize_tool_name(name);
                                let input: Value =
                                    serde_json::from_str(arguments).unwrap_or(Value::Null);

                                let file = extract_file(&input);
                                if let Some(fname) = file.clone() {
                                    if !files.contains(&fname) {
                                        files.push(fname);
                                    }
                                }
                                let command = if normalized == "Bash" {
                                    input
                                        .get("command")
                                        .and_then(Value::as_str)
                                        .map(|s| s.to_string())
                                } else {
                                    None
                                };

                                let output = tool_results
                                    .get(id)
                                    .and_then(|result_row| {
                                        let raw = decode_content(
                                            result_row.content.as_deref().unwrap_or(""),
                                        );
                                        Some((raw, result_row.tool_name.as_deref().unwrap_or(name)))
                                    })
                                    .map(|(raw, _)| raw);

                                let is_error = output
                                    .as_ref()
                                    .map(|raw| is_error_result(raw))
                                    .unwrap_or(false);

                                if is_error {
                                    let content = output
                                        .as_ref()
                                        .map(|raw| output_to_text(raw))
                                        .unwrap_or_default();
                                    errors.push(serde_json::json!({
                                        "tool": normalized,
                                        "content": truncate_str(&content, 300),
                                    }));
                                }

                                tools.push(ToolUse {
                                    name: normalized.to_string(),
                                    is_error,
                                    file,
                                    command,
                                    output: output.and_then(|raw| {
                                        capture_tool_output(&output_to_text(&raw))
                                    }),
                                });
                            }
                        }
                    }
                }
                "tool" => {
                    // Standalone tool results that couldn't be paired with an
                    // assistant tool call (compaction slices) still surface the
                    // tool name when present.
                    if let Some(tool_name) = &row.tool_name {
                        if tool_results
                            .get(row.tool_call_id.as_deref().unwrap_or(""))
                            .is_some()
                        {
                            continue;
                        }
                        let normalized = normalize_tool_name(tool_name);
                        let raw = decode_content(row.content.as_deref().unwrap_or(""));
                        let text = output_to_text(&raw);
                        let is_error = is_error_result(&raw);
                        if is_error {
                            errors.push(serde_json::json!({
                                "tool": normalized,
                                "content": truncate_str(&text, 300),
                            }));
                        }
                        tools.push(ToolUse {
                            name: normalized.to_string(),
                            is_error,
                            file: None,
                            command: None,
                            output: capture_tool_output(&text),
                        });
                    }
                }
                _ => {}
            }
        }

        position += 1;
        files.truncate(5);

        let ended_on_error = tools.last().map(|t| t.is_error).unwrap_or(false);
        let action = finalize_action_text(&action_parts.join("\n"), &tools, &errors, ended_on_error);

        exchanges.push(Exchange {
            position,
            user: user_text,
            action,
            files,
            timestamp,
            tools,
            edits: Vec::new(),
            errors,
            ended_on_error,
        });
    }

    if exchanges.len() > last {
        let skip = exchanges.len() - last;
        exchanges = exchanges.into_iter().skip(skip).collect();
    }

    Ok(exchanges)
}

/// Extract a file name from a tool-call input object, if present.
fn extract_file(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    for field in ["file_path", "filePath", "path", "file", "target_file"] {
        if let Some(val) = obj.get(field).and_then(Value::as_str)
            && !val.is_empty()
        {
            return Some(
                Path::new(val)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(val)
                    .to_string(),
            );
        }
    }
    None
}

/// Search Hermes sessions for a regex pattern.
pub(crate) fn search_hermes_sessions(
    db_path: &Path,
    pattern: &str,
    limit: usize,
) -> Result<Vec<TranscriptSearchMatch>, String> {
    let re = Regex::new(pattern).map_err(|e| format!("Invalid regex: {e}"))?;
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Cannot open Hermes DB: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT m.session_id, COALESCE(s.title, ''), m.content, m.tool_name
             FROM messages m
             JOIN sessions s ON s.id = m.session_id
             WHERE m.active = 1 AND m.role = 'user'
             ORDER BY m.id ASC",
        )
        .map_err(|e| format!("Query error: {e}"))?;

    let mut by_session: std::collections::HashMap<String, TranscriptSearchMatch> =
        std::collections::HashMap::new();
    let mut order = Vec::new();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| format!("Query error: {e}"))?;

    for row in rows {
        let (session_id, title, content, tool_name) = match row {
            Ok(row) => row,
            Err(_) => continue,
        };
        let raw = content.unwrap_or_default();
        let decoded = decode_content(&raw);
        let text = content_to_text(&decoded);
        if text.is_empty() || !re.is_match(&text) {
            continue;
        }

        let entry = by_session.entry(session_id.clone()).or_insert_with(|| {
            order.push(session_id.clone());
            TranscriptSearchMatch {
                path: db_path.to_string_lossy().to_string(),
                agent: "hermes".to_string(),
                line: 0,
                text: truncate_str(&text.replace('\n', " "), 100).to_string(),
                matches: 0,
                session_id: Some(session_id.clone()),
                label: Some(if title.is_empty() {
                    session_id.clone()
                } else {
                    title
                }),
            }
        });
        entry.matches += 1;
        let _ = tool_name;
    }

    Ok(order
        .into_iter()
        .filter_map(|session_id| by_session.remove(&session_id))
        .take(limit)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_db() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn create_tables(conn: &rusqlite::Connection) {
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                title TEXT,
                message_count INTEGER DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                role TEXT NOT NULL,
                content TEXT,
                tool_call_id TEXT,
                tool_calls TEXT,
                tool_name TEXT,
                timestamp REAL NOT NULL,
                active INTEGER NOT NULL DEFAULT 1,
                compacted INTEGER NOT NULL DEFAULT 0
             );",
        )
        .unwrap();
    }

    fn insert(
        conn: &rusqlite::Connection,
        session_id: &str,
        role: &str,
        content: Option<&str>,
        tool_call_id: Option<&str>,
        tool_calls: Option<&str>,
        tool_name: Option<&str>,
        ts: f64,
    ) {
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                session_id,
                role,
                content,
                tool_call_id,
                tool_calls,
                tool_name,
                ts
            ],
        )
        .unwrap();
    }

    #[test]
    fn parses_basic_hermes_session() {
        let dir = make_db();
        let db_path = dir.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        create_tables(&conn);
        conn.execute(
            "INSERT INTO sessions (id, source, title) VALUES ('ses_1', 'acp', 'test')",
            [],
        )
        .unwrap();
        insert(&conn, "ses_1", "user", Some("fix the bug"), None, None, None, 1.0);
        insert(&conn, "ses_1", "assistant", Some("Let me check."), None, None, None, 2.0);
        insert(
            &conn,
            "ses_1",
            "assistant",
            None,
            None,
            Some(r#"[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"cargo test\"}"}}]"#),
            None,
            3.0,
        );
        insert(
            &conn,
            "ses_1",
            "tool",
            Some("All tests passed"),
            Some("call_1"),
            None,
            Some("Bash"),
            4.0,
        );
        insert(&conn, "ses_1", "assistant", Some("Done!"), None, None, None, 5.0);

        let exchanges = parse_hermes_sqlite(&db_path, "ses_1", 10).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user, "fix the bug");
        assert_eq!(exchanges[0].action, "Let me check.\nDone!");
        assert_eq!(exchanges[0].tools.len(), 1);
        assert_eq!(exchanges[0].tools[0].name, "Bash");
        assert_eq!(
            exchanges[0].tools[0].command.as_deref(),
            Some("cargo test")
        );
        assert_eq!(exchanges[0].tools[0].output.as_deref(), Some("All tests passed"));
        assert!(!exchanges[0].ended_on_error);
    }

    #[test]
    fn skips_compacted_messages_and_json_content() {
        let dir = make_db();
        let db_path = dir.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        create_tables(&conn);
        conn.execute(
            "INSERT INTO sessions (id, source, title) VALUES ('ses_2', 'acp', '')",
            [],
        )
        .unwrap();
        insert(&conn, "ses_2", "user", Some("hi"), None, None, None, 1.0);
        // Compaction removes content but may leave a residual row; active=0 skips it.
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, active)
             VALUES ('ses_2', 'assistant', 'compacted', 2.0, 0)",
            [],
        )
        .unwrap();
        insert(
            &conn,
            "ses_2",
            "user",
            Some("\x00json:[{\"type\":\"text\",\"text\":\"multimodal ask\"}]"),
            None,
            None,
            None,
            3.0,
        );
        insert(&conn, "ses_2", "assistant", Some("ok"), None, None, None, 4.0);

        let exchanges = parse_hermes_sqlite(&db_path, "ses_2", 10).unwrap();
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].action, "(no response)");
        assert_eq!(exchanges[1].user, "multimodal ask");
        assert_eq!(exchanges[1].action, "ok");
    }

    #[test]
    fn search_matches_across_sessions() {
        let dir = make_db();
        let db_path = dir.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        create_tables(&conn);
        conn.execute(
            "INSERT INTO sessions (id, source, title) VALUES ('ses_a', 'acp', 'PONG'), ('ses_b', 'acp', 'other')",
            [],
        )
        .unwrap();
        insert(&conn, "ses_a", "user", Some("reply with PONG"), None, None, None, 1.0);
        insert(&conn, "ses_b", "user", Some("hello world"), None, None, None, 2.0);

        let matches = search_hermes_sessions(&db_path, "PONG", 10).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session_id.as_deref(), Some("ses_a"));
        assert_eq!(matches[0].agent, "hermes");
        assert_eq!(matches[0].label.as_deref(), Some("PONG"));
    }

    #[test]
    fn no_tool_data_returns_empty_session() {
        let dir = make_db();
        let db_path = dir.path().join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        create_tables(&conn);
        conn.execute(
            "INSERT INTO sessions (id, source, title) VALUES ('empty', 'acp', '')",
            [],
        )
        .unwrap();
        let exchanges = parse_hermes_sqlite(&db_path, "empty", 10).unwrap();
        assert!(exchanges.is_empty());
    }
}
