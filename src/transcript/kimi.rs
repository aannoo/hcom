//! Kimi Code CLI transcript parser.
//!
//! Reads `context.jsonl` from Kimi session directories and normalizes into
//! the tool-agnostic `Exchange` format used by hcom.
//!
//! Kimi context.jsonl structure (per-line JSON):
//!   - `role`: "user" | "assistant" | "tool" | "_system_prompt" | "_checkpoint" | "_usage"
//!   - `content`: string or object (assistant has `think`/`text` sub-fields)
//!   - `tool_calls`: array on assistant messages
//!   - `tool_call_id`: on tool messages

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::shared::{Exchange, ToolUse, extract_edit_info, is_error_result, normalize_tool_name};

/// Parse Kimi `context.jsonl` into exchanges.
pub fn parse_kimi_context_jsonl(
    path: &Path,
    last: usize,
    detailed: bool,
) -> Result<Vec<Exchange>, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
    let reader = BufReader::new(file);

    let mut lines: Vec<Value> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("Read error: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line).map_err(|e| format!("JSON parse: {e}"))?;
        // Skip internal meta roles
        if let Some(role) = value.get("role").and_then(|v| v.as_str())
            && role.starts_with('_')
        {
            continue;
        }
        lines.push(value);
    }

    // Group messages into exchanges: user -> assistant [-> tool]*
    let mut exchanges: Vec<Exchange> = Vec::new();
    let mut i = 0;
    let mut position = 0;

    while i < lines.len() {
        let role = lines[i].get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" {
            i += 1;
            continue;
        }

        position += 1;
        let user_text = extract_text(&lines[i]);
        let assistant_idx = i + 1;

        // Gather tools and tool results that follow the assistant message
        let mut tools: Vec<ToolUse> = Vec::new();
        let mut edits: Vec<Value> = Vec::new();
        let mut errors: Vec<Value> = Vec::new();
        let mut ended_on_error = false;
        let mut action_parts: Vec<String> = Vec::new();

        if assistant_idx < lines.len() {
            let next_role = lines[assistant_idx]
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if next_role == "assistant" {
                // Extract tool_calls from assistant message
                if let Some(calls) = lines[assistant_idx].get("tool_calls").and_then(|v| v.as_array())
                {
                    // Build a map of tool_call_id -> tool result for matching
                    let mut results: HashMap<String, Value> = HashMap::new();
                    let mut j = assistant_idx + 1;
                    while j < lines.len() {
                        let r = lines[j].get("role").and_then(|v| v.as_str()).unwrap_or("");
                        if r == "tool" {
                            if let Some(id) = lines[j]
                                .get("tool_call_id")
                                .and_then(|v| v.as_str())
                            {
                                results.insert(id.to_string(), lines[j].clone());
                            }
                            j += 1;
                        } else {
                            break;
                        }
                    }

                    for call in calls {
                        let name = call
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let arguments = call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        let args_parsed: Value =
                            serde_json::from_str(arguments).unwrap_or(Value::Object(Default::default()));

                        let result = results.get(call_id).cloned();
                        let is_err = result.as_ref().map(is_error_result).unwrap_or(false);
                        let result_text = result
                            .as_ref()
                            .and_then(|r| r.get("content").and_then(|v| v.as_str()))
                            .unwrap_or("");

                        let file = args_parsed
                            .get("file_path")
                            .or_else(|| args_parsed.get("path"))
                            .or_else(|| args_parsed.get("file"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        let command = args_parsed
                            .get("command")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        let canonical_name = normalize_tool_name(&name).to_string();

                        if is_err {
                            ended_on_error = true;
                            errors.push(serde_json::json!({
                                "tool": canonical_name,
                                "error": result_text,
                            }));
                        }

                        if let Some(edit) = extract_edit_info(&result, &args_parsed) {
                            edits.push(edit);
                        }
                        if !action_parts.contains(&canonical_name) {
                            action_parts.push(canonical_name.clone());
                        }

                        tools.push(ToolUse {
                            name: canonical_name,
                            is_error: is_err,
                            file: file.clone(),
                            command: command.clone(),
                        });
                    }
                }

                // Advance past assistant + any tool results we consumed
                i = assistant_idx + 1;
                while i < lines.len()
                    && lines[i].get("role").and_then(|v| v.as_str()) == Some("tool")
                {
                    i += 1;
                }
            } else {
                i = assistant_idx;
            }
        } else {
            i = assistant_idx;
        }

        let action = if action_parts.is_empty() {
            String::new()
        } else {
            action_parts.join(", ")
        };

        let files: Vec<String> = tools
            .iter()
            .filter_map(|t| t.file.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        exchanges.push(Exchange {
            position,
            user: user_text,
            action,
            files,
            timestamp: String::new(),
            tools,
            edits,
            errors,
            ended_on_error,
        });
    }

    if !detailed && exchanges.len() > last {
        exchanges = exchanges.split_off(exchanges.len() - last);
    }

    Ok(exchanges)
}

/// Extract human-readable text from a Kimi message.
fn extract_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(obj)) => {
            // Assistant message with think/text parts
            let think = obj.get("think").and_then(|v| v.as_str()).unwrap_or("");
            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if think.is_empty() {
                text.to_string()
            } else if text.is_empty() {
                format!("[think]\n{think}")
            } else {
                format!("[think]\n{think}\n\n{text}")
            }
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_temp_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file
    }

    #[test]
    fn parse_simple_user_assistant() {
        let jsonl = make_temp_jsonl(&[
            r#"{"role":"user","content":"hello"}"#,
            r#"{"role":"assistant","content":{"think":"","text":"hi there"}}"#,
        ]);
        let exchanges = parse_kimi_context_jsonl(jsonl.path(), 10, true).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user, "hello");
        assert!(exchanges[0].action.is_empty());
        assert!(exchanges[0].tools.is_empty());
    }

    #[test]
    fn parse_with_bash_tool() {
        let jsonl = make_temp_jsonl(&[
            r#"{"role":"user","content":"run ls"}"#,
            r#"{"role":"assistant","content":{"think":"","text":""},"tool_calls":[{"id":"call_1","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]}"#,
            r#"{"role":"tool","content":"file.txt","tool_call_id":"call_1"}"#,
        ]);
        let exchanges = parse_kimi_context_jsonl(jsonl.path(), 10, true).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].tools.len(), 1);
        assert_eq!(exchanges[0].tools[0].name, "Bash");
        assert_eq!(exchanges[0].tools[0].command, Some("ls".to_string()));
        assert_eq!(exchanges[0].action, "Bash");
    }

    #[test]
    fn skips_internal_roles() {
        let jsonl = make_temp_jsonl(&[
            r#"{"role":"_system_prompt","content":"you are helpful"}"#,
            r#"{"role":"user","content":"hi"}"#,
            r#"{"role":"assistant","content":{"think":"","text":"hello"}}"#,
            r#"{"role":"_usage","content":""}"#,
        ]);
        let exchanges = parse_kimi_context_jsonl(jsonl.path(), 10, true).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user, "hi");
    }
}
