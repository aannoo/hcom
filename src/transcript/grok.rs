//! Grok Build transcript parser (`updates.jsonl`).
//!
//! Grok persists every ACP session update under
//! `$GROK_HOME/sessions/<url-encoded-cwd>/<session-id>/updates.jsonl`:
//!
//! ```jsonc
//! {"timestamp":1790689536,"method":"session/update","params":{"update":{
//!   "sessionUpdate":"user_message_chunk","content":{"type":"text","text":"…"}}}}
//! ```
//!
//! A turn is its user chunks, then agent chunks and `tool_call` /
//! `tool_call_update` events, closed by `turn_completed` (carrying
//! `stop_reason`). Queued prompts only start after the previous turn
//! completes, so `turn_completed` is the exchange boundary.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};

use super::shared::{
    Exchange, ToolUse, capture_tool_output, finalize_action_text, normalize_tool_name,
    read_file_lossy, truncate_str,
};

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Object(obj) => match obj.get("text").and_then(Value::as_str) {
            Some(text) => text.to_string(),
            // tool_call_update wraps blocks: {"type":"content","content":{…}}
            None => obj.get("content").map(text_of).unwrap_or_default(),
        },
        Value::Array(blocks) => blocks.iter().map(text_of).collect(),
        _ => String::new(),
    }
}

fn tool_from_call(update: &Value) -> ToolUse {
    let name = update
        .pointer("/_meta/x.ai~1tool/name")
        .or_else(|| update.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let input = update.get("rawInput").unwrap_or(&Value::Null);
    let file = ["path", "file_path", "target_file"]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .map(|path| {
            Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_string()
        });
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .map(|command| truncate_str(command, 200).to_string());
    ToolUse {
        name: normalize_tool_name(name).to_string(),
        is_error: false,
        file,
        command,
        output: None,
    }
}

#[derive(Default)]
struct Turn {
    user: String,
    action: String,
    tools: Vec<ToolUse>,
    tool_index: HashMap<String, usize>,
    files: Vec<String>,
    errors: Vec<Value>,
    ended_on_error: bool,
    timestamp: String,
}

impl Turn {
    fn is_empty(&self) -> bool {
        self.user.is_empty() && self.action.is_empty() && self.tools.is_empty()
    }

    fn into_exchange(mut self, position: usize) -> Exchange {
        self.files.sort();
        self.files.dedup();
        let action =
            finalize_action_text(&self.action, &self.tools, &self.errors, self.ended_on_error);
        Exchange {
            position,
            user: self.user,
            action,
            files: self.files,
            timestamp: self.timestamp,
            tools: self.tools,
            edits: Vec::new(),
            errors: self.errors,
            ended_on_error: self.ended_on_error,
        }
    }
}

/// Parse a Grok Build `updates.jsonl` transcript into shared exchanges.
pub(crate) fn parse_grok_updates_jsonl(
    path: &Path,
    last: usize,
    detailed: bool,
) -> Result<Vec<Exchange>, String> {
    let content = read_file_lossy(path)?;
    let mut exchanges: Vec<Exchange> = Vec::new();
    let mut turn = Turn::default();
    let mut timestamp = String::new();

    let finish = |turn: &mut Turn, exchanges: &mut Vec<Exchange>| {
        let done = std::mem::take(turn);
        if !done.is_empty() {
            exchanges.push(done.into_exchange(exchanges.len() + 1));
        }
    };

    for line in content.lines() {
        let Ok(root) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(ts) = root.get("timestamp").and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        }) {
            timestamp = ts;
        }
        let Some(update) = root.pointer("/params/update") else {
            continue;
        };
        if turn.timestamp.is_empty() {
            turn.timestamp = timestamp.clone();
        }
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "user_message_chunk" => {
                turn.user
                    .push_str(&text_of(update.get("content").unwrap_or(&Value::Null)));
            }
            "agent_message_chunk" => {
                turn.action
                    .push_str(&text_of(update.get("content").unwrap_or(&Value::Null)));
            }
            "tool_call" => {
                let tool = tool_from_call(update);
                if let Some(file) = &tool.file {
                    turn.files.push(file.clone());
                }
                if let Some(id) = update.get("toolCallId").and_then(Value::as_str) {
                    turn.tool_index.insert(id.to_string(), turn.tools.len());
                }
                turn.tools.push(tool);
            }
            "tool_call_update" => {
                let status = update.get("status").and_then(Value::as_str);
                let Some(&index) = update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .and_then(|id| turn.tool_index.get(id))
                else {
                    continue;
                };
                if !matches!(status, Some("completed" | "failed")) {
                    continue;
                }
                let output = text_of(update.get("content").unwrap_or(&Value::Null));
                let tool = &mut turn.tools[index];
                if detailed {
                    tool.output = capture_tool_output(&output);
                }
                if status == Some("failed") {
                    tool.is_error = true;
                    turn.errors.push(json!({
                        "tool": tool.name,
                        "content": truncate_str(&output, 300),
                    }));
                }
            }
            "turn_completed" => {
                let reason = update
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn");
                if reason != "end_turn" {
                    turn.ended_on_error = true;
                    turn.errors.push(json!({ "stop_reason": reason }));
                }
                finish(&mut turn, &mut exchanges);
            }
            _ => {}
        }
    }
    finish(&mut turn, &mut exchanges);

    if last > 0 && exchanges.len() > last {
        Ok(exchanges.split_off(exchanges.len() - last))
    } else {
        Ok(exchanges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(update: Value) -> String {
        json!({"timestamp": 1, "method": "session/update", "params": {"update": update}})
            .to_string()
    }

    fn parse(lines: &[String], detailed: bool) -> Vec<Exchange> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        parse_grok_updates_jsonl(&path, 0, detailed).unwrap()
    }

    fn chunk(kind: &str, text: &str) -> String {
        update(json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}))
    }

    #[test]
    fn joins_streamed_chunks_into_one_exchange_per_turn() {
        let lines = [
            chunk("user_message_chunk", "fix "),
            chunk("user_message_chunk", "the bug"),
            chunk("agent_thought_chunk", "hmm"),
            chunk("agent_message_chunk", "Hello "),
            chunk("agent_message_chunk", "world"),
            update(json!({"sessionUpdate": "turn_completed", "stop_reason": "end_turn"})),
            chunk("user_message_chunk", "second"),
            chunk("agent_message_chunk", "ok"),
            update(json!({"sessionUpdate": "turn_completed", "stop_reason": "end_turn"})),
        ];
        let exchanges = parse(&lines, false);
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].user, "fix the bug");
        assert_eq!(exchanges[0].action, "Hello world");
        assert_eq!(exchanges[1].position, 2);
        assert!(!exchanges[0].ended_on_error);
    }

    #[test]
    fn tool_results_failures_and_stop_reasons() {
        let call = |id: &str, command: &str| {
            update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": "run_terminal_command",
                "rawInput": {"command": command},
                "_meta": {"x.ai/tool": {"name": "run_terminal_command"}}
            }))
        };
        let result = |id: &str, status: &str, text: &str| {
            update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": id,
                "status": status,
                "content": [{"type": "content", "content": {"type": "text", "text": text}}]
            }))
        };
        let lines = [
            chunk("user_message_chunk", "go"),
            call("a", "ls"),
            result("a", "completed", "file.txt"),
            call("b", "false"),
            result("b", "failed", "exit 1"),
            update(json!({"sessionUpdate": "turn_completed", "stop_reason": "cancelled"})),
        ];
        let exchanges = parse(&lines, true);
        let turn = &exchanges[0];
        assert_eq!(turn.tools.len(), 2);
        assert_eq!(turn.tools[0].name, "Bash");
        assert_eq!(turn.tools[0].command.as_deref(), Some("ls"));
        assert_eq!(turn.tools[0].output.as_deref(), Some("file.txt"));
        assert!(!turn.tools[0].is_error);
        assert!(turn.tools[1].is_error);
        assert!(turn.ended_on_error);
        assert_eq!(turn.errors.len(), 2);
        assert!(parse(&lines, false)[0].tools[0].output.is_none());
    }
}
