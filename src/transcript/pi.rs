//! Pi Coding Agent transcript parser (.jsonl).

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};

use super::shared::{
    Exchange, ToolUse, capture_tool_output, dedup_sorted_capped, extract_content_text,
    finalize_action_text, normalize_tool_name, read_file_lossy, truncate_str,
};

fn message_text(message: &Value) -> String {
    if let Some(s) = message.get("content").and_then(Value::as_str) {
        return s.trim().to_string();
    }
    extract_content_text(message.get("content"))
        .trim()
        .to_string()
}

fn tool_name(message: &Value) -> String {
    message
        .get("toolName")
        .or_else(|| message.get("tool_name"))
        .or_else(|| message.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn tool_input(message: &Value) -> Value {
    message
        .get("input")
        .or_else(|| message.get("toolInput"))
        .or_else(|| message.get("tool_input"))
        .cloned()
        .unwrap_or_default()
}

fn tool_file(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    for field in ["path", "file_path", "file"] {
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

fn tool_command(input: &Value) -> Option<String> {
    input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

pub(crate) fn parse_pi_jsonl(
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
    let mut current_errors: Vec<Value> = Vec::new();
    let mut current_last_was_error = false;
    // Pi carries tool arguments (command, file path) on the assistant's inline
    // `toolCall` block; the later `toolResult` line has `input: null` and links
    // back by `toolCallId`. Map id → arguments so we can recover the command.
    let mut call_args: HashMap<String, Value> = HashMap::new();
    let mut timestamp = String::new();
    let mut position = 0usize;

    let flush = |exchanges: &mut Vec<Exchange>,
                 position: &mut usize,
                 user: &mut String,
                 action: &mut String,
                 tools: &mut Vec<ToolUse>,
                 files: &mut Vec<String>,
                 errors: &mut Vec<Value>,
                 last_was_error: &mut bool,
                 ts: &mut String| {
        if user.is_empty() && action.is_empty() && tools.is_empty() {
            return;
        }
        *position += 1;
        let final_action = finalize_action_text(action, tools, errors, *last_was_error);
        exchanges.push(Exchange {
            position: *position,
            user: std::mem::take(user),
            action: final_action,
            files: dedup_sorted_capped(files, 5),
            timestamp: std::mem::take(ts),
            tools: std::mem::take(tools),
            edits: Vec::new(),
            errors: std::mem::take(errors),
            ended_on_error: *last_was_error,
        });
        action.clear();
        files.clear();
        *last_was_error = false;
    };

    for line in content.lines() {
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let message = entry.get("message").unwrap_or(&entry);
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "user" => {
                flush(
                    &mut exchanges,
                    &mut position,
                    &mut current_user,
                    &mut current_action,
                    &mut current_tools,
                    &mut current_files,
                    &mut current_errors,
                    &mut current_last_was_error,
                    &mut timestamp,
                );
                current_user = message_text(message);
                timestamp = entry
                    .get("timestamp")
                    .or_else(|| message.get("timestamp"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
            "assistant" => {
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                            continue;
                        }
                        if let (Some(id), Some(args)) = (
                            block.get("id").and_then(Value::as_str),
                            block.get("arguments"),
                        ) {
                            call_args.insert(id.to_string(), args.clone());
                        }
                    }
                }
                let text = message_text(message);
                if !text.is_empty() {
                    if !current_action.is_empty() {
                        current_action.push_str("\n\n");
                    }
                    current_action.push_str(&text);
                }
            }
            "toolResult" | "tool_result" | "tool" => {
                let name = tool_name(message);
                if name.is_empty() {
                    continue;
                }
                let mut input = tool_input(message);
                // `toolResult.input` is null in real pi transcripts; recover the
                // arguments recorded on the matching assistant `toolCall` block.
                let input_missing =
                    !input.is_object() || input.as_object().is_some_and(|o| o.is_empty());
                if input_missing
                    && let Some(id) = message
                        .get("toolCallId")
                        .or_else(|| message.get("tool_call_id"))
                        .and_then(Value::as_str)
                    && let Some(args) = call_args.get(id)
                {
                    input = args.clone();
                }
                let file = tool_file(&input);
                if let Some(file) = &file {
                    current_files.push(file.clone());
                }
                let is_err = message
                    .get("isError")
                    .or_else(|| message.get("is_error"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let output = message_text(message);
                current_tools.push(ToolUse {
                    name: normalize_tool_name(&name).to_string(),
                    is_error: is_err,
                    file,
                    command: tool_command(&input),
                    output: capture_tool_output(&output),
                });
                if is_err {
                    current_errors.push(json!({
                        "tool": name,
                        "content": truncate_str(&output, 300),
                    }));
                }
                current_last_was_error = is_err;
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
        &mut current_errors,
        &mut current_last_was_error,
        &mut timestamp,
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

    #[test]
    fn parses_pi_message_jsonl() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"session","id":"abc"}"#,
                "\n",
                r#"{"type":"message","timestamp":"t1","message":{"role":"user","content":"hello"}}"#,
                "\n",
                // Real pi: command lives on the assistant's inline toolCall block.
                r#"{"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"..."},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"pwd"}}]}}"#,
                "\n",
                // Real pi: toolResult has input:null and links by toolCallId.
                r#"{"type":"message","message":{"role":"toolResult","toolCallId":"call_1","toolName":"bash","input":null,"content":[{"type":"text","text":"/work"}],"isError":false}}"#,
                "\n",
                r#"{"type":"message","message":{"role":"assistant","content":"done"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let exchanges = parse_pi_jsonl(file.path(), 10, false).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user, "hello");
        assert_eq!(exchanges[0].action, "done");
        assert_eq!(exchanges[0].tools[0].name, "Bash");
        // Command recovered from the assistant toolCall block despite null input.
        assert_eq!(exchanges[0].tools[0].command.as_deref(), Some("pwd"));
        assert_eq!(exchanges[0].tools[0].output.as_deref(), Some("/work"));
    }
}
