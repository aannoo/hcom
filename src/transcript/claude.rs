//! Claude transcript parser (.jsonl).

use std::path::Path;

use serde_json::{Value, json};

use super::shared::{
    Exchange, ToolUse, collapse_codex_duplicate_exchanges, dedup_sorted_capped, extract_content_text,
    extract_edit_info, extract_text_content, finalize_action_text, has_user_text, is_error_result,
    normalize_tool_name, read_file_lossy, truncate_str,
};

/// Parse Claude JSONL transcript.
pub(crate) fn parse_claude_jsonl(
    path: &Path,
    last: usize,
    detailed: bool,
) -> Result<Vec<Exchange>, String> {
    let content = read_file_lossy(path)?;

    // First pass: parse all entries and build tool_use index for detailed mode
    struct ParsedEntry {
        entry_type: String,
        ts: String,
        data: Value,
    }

    let mut entries = Vec::new();
    // Map (session_id, tool_use_id) -> {name, input} for matching tool_results to tool_uses
    let mut tool_use_index: std::collections::HashMap<(String, String), (String, Value)> =
        std::collections::HashMap::new();

    for line in content.lines() {
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Skip meta/system entries
        if entry
            .get("isMeta")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || entry
                .get("isCompactSummary")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            || entry
                .get("isSidechain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            continue;
        }

        let entry_type = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ts = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Skip system-level entry types
        if matches!(
            entry_type.as_str(),
            "summary"
                | "system"
                | "result"
                | "progress"
                | "file-history-snapshot"
                | "saved_hook_context"
        ) {
            continue;
        }

        // Build tool_use index from assistant entries
        if entry_type == "assistant" {
            let session_id = entry
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(arr) = entry
                .get("message")
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_array())
            {
                for block in arr {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        let tool_id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = block.get("input").cloned().unwrap_or(json!({}));
                        tool_use_index.insert((session_id.clone(), tool_id), (name, input));
                    }
                }
            }
        }

        entries.push(ParsedEntry {
            entry_type,
            ts,
            data: entry,
        });
    }

    // Second pass: build exchanges
    let mut exchanges = Vec::new();
    let mut current_user = String::new();
    let mut current_action = String::new();
    let mut current_tools: Vec<ToolUse> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();
    let mut current_ts = String::new();
    let mut current_edits: Vec<Value> = Vec::new();
    let mut current_errors: Vec<Value> = Vec::new();
    let mut current_last_was_error = false;
    let mut position = 0;

    for pe in &entries {
        match pe.entry_type.as_str() {
            "user" => {
                // Check if this user entry has actual user text (not just tool_result blocks)
                let has_text = has_user_text(&pe.data.get("message").cloned().unwrap_or(json!({})));
                if !has_text {
                    // tool_result-only user entry — process for error detection in detailed mode
                    if detailed {
                        let session_id = pe
                            .data
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let tool_use_result = pe.data.get("toolUseResult").cloned();
                        if let Some(arr) = pe
                            .data
                            .get("message")
                            .and_then(|v| v.get("content"))
                            .and_then(|v| v.as_array())
                        {
                            for block in arr {
                                if block.get("type").and_then(|v| v.as_str()) != Some("tool_result")
                                {
                                    continue;
                                }
                                let tool_use_id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let (tool_name, tool_input) = tool_use_index
                                    .get(&(session_id.clone(), tool_use_id.clone()))
                                    .map(|(n, i)| (n.clone(), i.clone()))
                                    .unwrap_or_else(|| ("unknown".to_string(), json!({})));

                                let is_err = is_error_result(block);
                                let normalized = normalize_tool_name(&tool_name);

                                let file = if normalized == "Edit" {
                                    tool_use_result
                                        .as_ref()
                                        .and_then(|r| r.get("filePath").and_then(|v| v.as_str()))
                                        .or_else(|| {
                                            tool_input.get("file_path").and_then(|v| v.as_str())
                                        })
                                        .map(|s| {
                                            Path::new(s)
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .unwrap_or(s)
                                                .to_string()
                                        })
                                } else {
                                    None
                                };

                                let command = if normalized == "Bash" {
                                    tool_input.get("command").and_then(|v| v.as_str()).map(|s| {
                                        if s.len() > 80 {
                                            format!("{}...", truncate_str(s, 77))
                                        } else {
                                            s.to_string()
                                        }
                                    })
                                } else {
                                    None
                                };

                                current_tools.push(ToolUse {
                                    name: normalized.to_string(),
                                    is_error: is_err,
                                    file,
                                    command,
                                });

                                // Extract edit info for Edit tools
                                if normalized == "Edit" {
                                    if let Some(edit) =
                                        extract_edit_info(&tool_use_result, &tool_input)
                                    {
                                        current_edits.push(edit);
                                    }
                                }

                                if is_err {
                                    let raw_content = extract_content_text(block.get("content"));
                                    let truncated = truncate_str(&raw_content, 300);
                                    current_errors.push(json!({
                                        "tool": normalized,
                                        "content": truncated,
                                    }));
                                    current_last_was_error = true;
                                } else {
                                    current_last_was_error = false;
                                }
                            }
                        }
                    }
                    continue;
                }

                // Save previous exchange
                if !current_user.is_empty() || !current_action.is_empty() {
                    position += 1;
                    exchanges.push(Exchange {
                        position,
                        user: current_user.clone(),
                        action: finalize_action_text(
                            &current_action,
                            &current_tools,
                            &current_errors,
                            current_last_was_error,
                        ),
                        files: dedup_sorted_capped(&current_files, 5),
                        timestamp: current_ts.clone(),
                        tools: std::mem::take(&mut current_tools),
                        edits: std::mem::take(&mut current_edits),
                        errors: std::mem::take(&mut current_errors),
                        ended_on_error: current_last_was_error,
                    });
                }

                // Extract user text
                current_user =
                    extract_text_content(&pe.data.get("message").cloned().unwrap_or(json!({})));
                current_action = String::new();
                current_tools = Vec::new();
                current_files = Vec::new();
                current_edits = Vec::new();
                current_errors = Vec::new();
                current_last_was_error = false;
                current_ts = pe.ts.clone();
            }
            "assistant" => {
                let msg = pe.data.get("message").cloned().unwrap_or(json!({}));

                // Extract text and tool_use blocks
                if let Some(content) = msg.get("content") {
                    if let Some(arr) = content.as_array() {
                        for block in arr {
                            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    if !current_action.is_empty() {
                                        current_action.push('\n');
                                    }
                                    current_action.push_str(text);
                                }
                            } else if block.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                            {
                                let tool_name =
                                    block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let input = block.get("input").cloned().unwrap_or(json!({}));

                                // Extract file from tool input (including notebook_path)
                                let file = input
                                    .get("file_path")
                                    .or_else(|| input.get("path"))
                                    .or_else(|| input.get("filePath"))
                                    .or_else(|| input.get("notebook_path"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| {
                                        Path::new(s)
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .unwrap_or(s)
                                            .to_string()
                                    });

                                if let Some(ref f) = file {
                                    if !current_files.contains(f) {
                                        current_files.push(f.clone());
                                    }
                                }

                                // Don't push tool to current_tools here in detailed mode —
                                // tools come from tool_result processing for accurate is_error.
                                // In non-detailed mode, push with is_error=false.
                                if !detailed {
                                    let command = if normalize_tool_name(tool_name) == "Bash" {
                                        input.get("command").and_then(|v| v.as_str()).map(|s| {
                                            if s.len() > 80 {
                                                format!("{}...", truncate_str(s, 77))
                                            } else {
                                                s.to_string()
                                            }
                                        })
                                    } else {
                                        None
                                    };

                                    current_tools.push(ToolUse {
                                        name: normalize_tool_name(tool_name).to_string(),
                                        is_error: false,
                                        file,
                                        command,
                                    });
                                }
                            }
                        }
                    } else if let Some(text) = content.as_str() {
                        current_action = text.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    // Save last exchange
    if !current_user.is_empty() || !current_action.is_empty() {
        position += 1;
        exchanges.push(Exchange {
            position,
            user: current_user,
            action: finalize_action_text(
                &current_action,
                &current_tools,
                &current_errors,
                current_last_was_error,
            ),
            files: dedup_sorted_capped(&current_files, 5),
            timestamp: current_ts,
            tools: current_tools,
            edits: current_edits,
            errors: current_errors,
            ended_on_error: current_last_was_error,
        });
    }

    // Apply last N
    exchanges = collapse_codex_duplicate_exchanges(exchanges);

    if exchanges.len() > last {
        let start = exchanges.len() - last;
        exchanges = exchanges[start..].to_vec();
    }

    Ok(exchanges)
}
