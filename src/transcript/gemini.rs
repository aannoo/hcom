//! Gemini transcript parser (.json).

use std::path::Path;

use serde_json::{Value, json};

use super::shared::{
    Exchange, ToolUse, extract_gemini_user_text, finalize_action_text, normalize_tool_name,
    read_file_lossy, truncate_str,
};

/// Parse Gemini JSON transcript.
pub(crate) fn parse_gemini_json(path: &Path, last: usize) -> Result<Vec<Exchange>, String> {
    let content = read_file_lossy(path)?;

    let data: Value = serde_json::from_str(&content).map_err(|e| format!("Invalid JSON: {e}"))?;

    let messages = data
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or("No messages array")?;

    let mut exchanges = Vec::new();
    let mut current_user = String::new();
    let mut current_action = String::new();
    let mut current_ts = String::new();
    let mut position = 0;

    let mut current_tools: Vec<ToolUse> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();

    for msg in messages {
        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = msg
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if msg_type == "user" {
            if !current_user.is_empty() || !current_action.is_empty() {
                position += 1;
                exchanges.push(Exchange {
                    position,
                    user: current_user.clone(),
                    action: finalize_action_text(&current_action, &current_tools, &[], false),
                    files: std::mem::take(&mut current_files),
                    timestamp: current_ts.clone(),
                    tools: std::mem::take(&mut current_tools),
                    edits: Vec::new(),
                    errors: Vec::new(),
                    ended_on_error: false,
                });
            }
            // Gemini user content can be a string or array of {text: ...} blocks.
            // Use displayContent if available (user-visible text without hook context).
            current_user = extract_gemini_user_text(msg);
            current_action = String::new();
            current_tools = Vec::new();
            current_files = Vec::new();
            current_ts = ts;
        } else if msg_type == "gemini" || msg_type == "model" {
            if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                if !current_action.is_empty() {
                    current_action.push('\n');
                }
                current_action.push_str(text);
            }

            // Extract tool calls
            if let Some(tool_calls) = msg.get("toolCalls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    let raw_name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let tool_name = normalize_tool_name(raw_name);
                    let args = tc.get("args").cloned().unwrap_or(json!({}));
                    let is_err = tc
                        .get("status")
                        .and_then(|v| v.as_str())
                        .is_some_and(|status| {
                            !matches!(
                                status.to_ascii_lowercase().as_str(),
                                "ok" | "success" | "completed"
                            )
                        });

                    // Extract file paths from tool args
                    if let Some(obj) = args.as_object() {
                        for field in &["file", "path", "file_path", "directory"] {
                            if let Some(val) = obj.get(*field).and_then(|v| v.as_str()) {
                                if !val.is_empty() {
                                    let fname = Path::new(val)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(val)
                                        .to_string();
                                    if !current_files.contains(&fname) {
                                        current_files.push(fname.clone());
                                    }
                                }
                            }
                        }
                    }

                    let command = if tool_name == "Bash" {
                        args.get("command").and_then(|v| v.as_str()).map(|s| {
                            if s.len() > 80 {
                                format!("{}...", truncate_str(s, 77))
                            } else {
                                s.to_string()
                            }
                        })
                    } else {
                        None
                    };

                    let file = args.as_object().and_then(|o| {
                        o.get("file_path")
                            .or(o.get("path"))
                            .or(o.get("file"))
                            .and_then(|v| v.as_str())
                            .map(|s| {
                                Path::new(s)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(s)
                                    .to_string()
                            })
                    });

                    current_tools.push(ToolUse {
                        name: tool_name.to_string(),
                        is_error: is_err,
                        file,
                        command,
                    });
                }
            }
        }
    }

    // Last exchange
    if !current_user.is_empty() || !current_action.is_empty() {
        position += 1;
        exchanges.push(Exchange {
            position,
            user: current_user,
            action: finalize_action_text(&current_action, &current_tools, &[], false),
            files: current_files,
            timestamp: current_ts,
            tools: current_tools,
            edits: Vec::new(),
            errors: Vec::new(),
            ended_on_error: false,
        });
    }

    if exchanges.len() > last {
        let start = exchanges.len() - last;
        exchanges = exchanges[start..].to_vec();
    }

    Ok(exchanges)
}
