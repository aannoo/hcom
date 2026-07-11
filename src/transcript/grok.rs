//! Grok Build transcript parser (`updates.jsonl`).
//!
//! Grok persists ACP-style session update lines under
//! `~/.grok/sessions/<url-encoded-cwd>/<session-uuid>/updates.jsonl`.
//! Each line is a JSON-RPC-ish envelope:
//!
//! ```jsonc
//! {"method":"session/update","params":{"update":{
//!   "sessionUpdate":"user_message_chunk",
//!   "content":{"type":"text","text":"…"}
//! }}}
//! ```
//!
//! We rebuild exchanges from user/agent message chunks and tool_call events.

use std::path::Path;

use serde_json::Value;

use super::shared::{
    Exchange, ToolUse, finalize_action_text, normalize_tool_name, read_file_lossy, truncate_str,
};

fn update_kind(update: &Value) -> &str {
    update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.trim().to_string(),
        Value::Object(obj) => obj
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn tool_from_call(update: &Value) -> Option<ToolUse> {
    let name = update
        .get("title")
        .or_else(|| update.get("toolName"))
        .or_else(|| update.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let input = update
        .get("rawInput")
        .or_else(|| update.get("input"))
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let file = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .or_else(|| input.get("target_file"))
        .and_then(Value::as_str)
        .map(|p| {
            Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(p)
                .to_string()
        });
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .map(|s| truncate_str(s, 200).to_string());
    Some(ToolUse {
        name: normalize_tool_name(name).to_string(),
        is_error: false,
        file,
        command,
    })
}

/// Parse a Grok Build `updates.jsonl` transcript into shared exchanges.
pub(crate) fn parse_grok_updates_jsonl(
    path: &Path,
    last: usize,
    _detailed: bool,
) -> Result<Vec<Exchange>, String> {
    let content = read_file_lossy(path)?;

    let mut exchanges: Vec<Exchange> = Vec::new();
    let mut current_user = String::new();
    let mut current_action = String::new();
    let mut current_tools: Vec<ToolUse> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();
    let mut assistant_chunks: Vec<String> = Vec::new();
    let mut position = 0usize;
    let mut in_exchange = false;
    let mut timestamp = String::new();

    let flush = |exchanges: &mut Vec<Exchange>,
                 position: &mut usize,
                 current_user: &mut String,
                 current_action: &mut String,
                 current_tools: &mut Vec<ToolUse>,
                 current_files: &mut Vec<String>,
                 assistant_chunks: &mut Vec<String>,
                 in_exchange: &mut bool,
                 timestamp: &str| {
        if !*in_exchange
            && current_user.is_empty()
            && assistant_chunks.is_empty()
            && current_tools.is_empty()
        {
            return;
        }
        *position += 1;
        let tools = std::mem::take(current_tools);
        let action = if !assistant_chunks.is_empty() {
            assistant_chunks.join("")
        } else {
            finalize_action_text(current_action, &tools, &[], false)
        };
        let mut files = std::mem::take(current_files);
        files.sort();
        files.dedup();
        exchanges.push(Exchange {
            position: *position,
            user: std::mem::take(current_user),
            action,
            files,
            timestamp: timestamp.to_string(),
            tools,
            edits: Vec::new(),
            errors: Vec::new(),
            ended_on_error: false,
        });
        current_action.clear();
        assistant_chunks.clear();
        *in_exchange = false;
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(root) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(ts) = root.get("timestamp").and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        }) {
            timestamp = ts;
        }
        let update = root
            .pointer("/params/update")
            .or_else(|| root.get("update"))
            .cloned()
            .unwrap_or(Value::Null);
        if update.is_null() {
            continue;
        }
        match update_kind(&update) {
            "user_message_chunk" => {
                if in_exchange && (!current_user.is_empty() || !assistant_chunks.is_empty()) {
                    flush(
                        &mut exchanges,
                        &mut position,
                        &mut current_user,
                        &mut current_action,
                        &mut current_tools,
                        &mut current_files,
                        &mut assistant_chunks,
                        &mut in_exchange,
                        &timestamp,
                    );
                }
                let text = content_text(update.get("content").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    if !current_user.is_empty() {
                        current_user.push('\n');
                    }
                    current_user.push_str(&text);
                    in_exchange = true;
                }
            }
            "agent_message_chunk" => {
                let text = content_text(update.get("content").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    assistant_chunks.push(text);
                    in_exchange = true;
                }
            }
            "tool_call" => {
                if let Some(tool) = tool_from_call(&update) {
                    if let Some(ref f) = tool.file {
                        current_files.push(f.clone());
                    }
                    if current_action.is_empty() {
                        current_action = tool.name.clone();
                    }
                    current_tools.push(tool);
                    in_exchange = true;
                }
            }
            "turn_completed" | "agent_end_turn" => {
                flush(
                    &mut exchanges,
                    &mut position,
                    &mut current_user,
                    &mut current_action,
                    &mut current_tools,
                    &mut current_files,
                    &mut assistant_chunks,
                    &mut in_exchange,
                    &timestamp,
                );
            }
            _ => {}
        }
    }

    flush(
        &mut exchanges,
        &mut position,
        &mut current_user,
        &mut current_action,
        &mut current_tools,
        &mut current_files,
        &mut assistant_chunks,
        &mut in_exchange,
        &timestamp,
    );

    if last > 0 && exchanges.len() > last {
        Ok(exchanges.split_off(exchanges.len() - last))
    } else {
        Ok(exchanges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_user_and_agent_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"timestamp":1,"params":{{"update":{{"sessionUpdate":"user_message_chunk","content":{{"type":"text","text":"hello grok"}}}}}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"timestamp":2,"params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"hi"}}}}}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"timestamp":3,"params":{{"update":{{"sessionUpdate":"turn_completed"}}}}}}"#
        )
        .unwrap();
        let exchanges = parse_grok_updates_jsonl(&path, 10, false).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user, "hello grok");
        assert_eq!(exchanges[0].action, "hi");
    }
}
