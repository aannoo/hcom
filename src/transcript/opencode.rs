//! OpenCode transcript parser (SQLite).

use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{Value, json};

use super::shared::{Exchange, ToolUse, finalize_action_text, normalize_tool_name, truncate_str};
use crate::log::log_warn;

#[derive(Debug, Clone)]
pub(crate) struct TranscriptSearchMatch {
    pub path: String,
    pub agent: String,
    pub line: usize,
    pub text: String,
    pub matches: usize,
    pub session_id: Option<String>,
    pub label: Option<String>,
}

pub(crate) fn get_opencode_db_path() -> Option<PathBuf> {
    let xdg_data = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.local/share")
    });
    let db_path = PathBuf::from(xdg_data).join("opencode").join("opencode.db");
    db_path.exists().then_some(db_path)
}

pub(crate) fn search_opencode_sessions(
    db_path: &Path,
    pattern: &str,
    limit: usize,
) -> Result<Vec<TranscriptSearchMatch>, String> {
    let re = Regex::new(pattern).map_err(|e| format!("Invalid regex: {e}"))?;
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Cannot open OpenCode DB: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.title, p.data
             FROM session s
             JOIN part p ON p.session_id = s.id
             WHERE json_extract(p.data, '$.type') = 'text'
             ORDER BY p.time_created ASC",
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
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| format!("Query error: {e}"))?;

    for row in rows {
        let (session_id, title, data_str) = match row {
            Ok(row) => row,
            Err(_) => continue,
        };
        let data = match serde_json::from_str::<Value>(&data_str) {
            Ok(data) => data,
            Err(_) => continue,
        };
        if data
            .get("synthetic")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }

        let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if text.is_empty() || !re.is_match(text) {
            continue;
        }

        let entry = by_session.entry(session_id.clone()).or_insert_with(|| {
            order.push(session_id.clone());
            TranscriptSearchMatch {
                path: db_path.to_string_lossy().to_string(),
                agent: "opencode".to_string(),
                line: 0,
                text: truncate_str(&text.replace('\n', " "), 100).to_string(),
                matches: 0,
                session_id: Some(session_id.clone()),
                label: Some(title.clone()),
            }
        });
        entry.matches += 1;
    }

    Ok(order
        .into_iter()
        .filter_map(|session_id| by_session.remove(&session_id))
        .take(limit)
        .collect())
}

/// Parse OpenCode SQLite transcript database.
///
/// OpenCode stores conversations in `opencode.db` with `message` and `part` tables.
/// Messages have role in their JSON `data` column; parts contain text, tool calls, etc.
pub(crate) fn parse_opencode_sqlite(
    db_path: &Path,
    session_id: &str,
    last: usize,
) -> Result<Vec<Exchange>, String> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Cannot open OpenCode DB: {e}"))?;

    // Fetch messages for this session (include time_created for timestamp)
    let mut stmt = conn.prepare(
        "SELECT id, data, time_created FROM message WHERE session_id = ? ORDER BY time_created ASC"
    ).map_err(|e| format!("Query error: {e}"))?;

    struct MsgRow {
        id: String,
        _data: Value,
        role: String,
        time_created: i64,
    }

    let messages: Vec<MsgRow> = stmt
        .query_map(rusqlite::params![session_id], |row| {
            let id: String = row.get(0)?;
            let data_str: String = row.get(1)?;
            let time_created: i64 = row.get::<_, i64>(2).unwrap_or(0);
            Ok((id, data_str, time_created))
        })
        .map_err(|e| format!("Query error: {e}"))?
        .filter_map(|r| r.ok())
        .filter_map(
            |(id, data_str, time_created)| match serde_json::from_str::<Value>(&data_str) {
                Ok(data) => {
                    let role = data
                        .get("role")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    Some(MsgRow {
                        id,
                        _data: data,
                        role,
                        time_created,
                    })
                }
                Err(e) => {
                    log_warn(
                        "transcript",
                        "opencode_parse",
                        &format!("skipping message {id}: invalid JSON in data column: {e}"),
                    );
                    None
                }
            },
        )
        .collect();

    if messages.is_empty() {
        return Ok(Vec::new());
    }

    // Prefetch parts keyed by message_id.
    // Query per message_id to avoid dependency on part.session_id column.
    let mut parts_by_msg: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();
    for msg in &messages {
        if let Ok(mut parts_stmt) =
            conn.prepare("SELECT data FROM part WHERE message_id = ? ORDER BY id ASC")
        {
            if let Ok(rows) =
                parts_stmt.query_map(rusqlite::params![msg.id], |row| row.get::<_, String>(0))
            {
                for data_str in rows.flatten() {
                    if let Ok(v) = serde_json::from_str::<Value>(&data_str) {
                        parts_by_msg.entry(msg.id.clone()).or_default().push(v);
                    }
                }
            }
        }
    }

    // Build exchanges: group by user messages
    let mut exchanges = Vec::new();
    let mut position = 0;

    // Find user message indices (with actual text)
    let mut user_indices: Vec<usize> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg.role != "user" {
            continue;
        }
        let parts = parts_by_msg.get(&msg.id).cloned().unwrap_or_default();
        let has_text = parts.iter().any(|p| {
            p.get("type").and_then(|v| v.as_str()) == Some("text")
                && !p
                    .get("synthetic")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                && !p
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
        });
        if has_text {
            user_indices.push(i);
        }
    }

    for (ui_pos, &user_idx) in user_indices.iter().enumerate() {
        let next_user_idx = user_indices
            .get(ui_pos + 1)
            .copied()
            .unwrap_or(messages.len());
        let user_msg = &messages[user_idx];
        let user_parts = parts_by_msg.get(&user_msg.id).cloned().unwrap_or_default();

        // Extract user text
        let user_text: String = user_parts
            .iter()
            .filter(|p| {
                p.get("type").and_then(|v| v.as_str()) == Some("text")
                    && !p
                        .get("synthetic")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
            })
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        let timestamp = if user_msg.time_created > 0 {
            let secs = user_msg.time_created / 1000;
            chrono::DateTime::from_timestamp(secs, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };

        // Process assistant messages between this user msg and next
        let mut action_parts: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        let mut tools: Vec<ToolUse> = Vec::new();

        for msg in &messages[(user_idx + 1)..next_user_idx] {
            if msg.role != "assistant" {
                continue;
            }
            let msg_parts = parts_by_msg.get(&msg.id).cloned().unwrap_or_default();
            for p in &msg_parts {
                let ptype = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ptype {
                    "text" => {
                        if p.get("synthetic")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                action_parts.push(text.to_string());
                            }
                        }
                    }
                    "tool" => {
                        let tool_name = p.get("tool").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let normalized = normalize_tool_name(tool_name);
                        let state = p.get("state").cloned().unwrap_or(json!({}));
                        let input = state.get("input").cloned().unwrap_or(json!({}));
                        let is_err = state.get("status").and_then(|v| v.as_str()) == Some("error");

                        // Extract file paths
                        if let Some(obj) = input.as_object() {
                            for field in &["file_path", "filePath", "path", "pattern", "file"] {
                                if let Some(val) = obj.get(*field).and_then(|v| v.as_str()) {
                                    if !val.is_empty() {
                                        let fname = Path::new(val)
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .unwrap_or(val)
                                            .to_string();
                                        if !files.contains(&fname) {
                                            files.push(fname);
                                        }
                                    }
                                }
                            }
                        }

                        let command = if normalized == "Bash" {
                            input
                                .get("command")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        } else {
                            None
                        };
                        let file = input
                            .get("file_path")
                            .or_else(|| input.get("filePath"))
                            .or_else(|| input.get("path"))
                            .and_then(|v| v.as_str())
                            .map(|s| {
                                Path::new(s)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(s)
                                    .to_string()
                            });

                        tools.push(ToolUse {
                            name: normalized.to_string(),
                            is_error: is_err,
                            file,
                            command,
                        });
                    }
                    _ => {}
                }
            }
        }

        position += 1;
        files.truncate(5);

        // Collect errors from tools with is_error
        let errors: Vec<Value> = tools
            .iter()
            .filter(|t| t.is_error)
            .map(|t| json!({"tool": t.name, "content": ""}))
            .collect();
        let ended_on_error = tools.last().map(|t| t.is_error).unwrap_or(false);
        let action =
            finalize_action_text(&action_parts.join("\n"), &tools, &errors, ended_on_error);

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

    // Apply last N
    if exchanges.len() > last {
        let skip = exchanges.len() - last;
        exchanges = exchanges.into_iter().skip(skip).collect();
    }

    Ok(exchanges)
}
