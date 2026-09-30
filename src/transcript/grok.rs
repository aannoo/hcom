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
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::shared::{
    Exchange, ToolUse, capture_tool_output, finalize_action_text, normalize_tool_name,
    read_file_lossy, truncate_str,
};

/// `$GROK_HOME` if set, else `<tool_config_root>/.grok`.
pub(crate) fn grok_config_dir() -> PathBuf {
    std::env::var("GROK_HOME")
        .ok()
        .map(|home| home.trim().to_string())
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::runtime_env::tool_config_root().join(".grok"))
}

/// `urlencoding::encode`, which Grok uses for the cwd component of session
/// directories: everything but RFC 3986 unreserved bytes is percent-encoded.
fn encode_path_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Grok's longest percent-encoded cwd directory name; longer cwds get a
/// `<slug>-<blake3 prefix>` name instead (`encode_cwd_dirname`).
const MAX_DIRNAME_BYTES: usize = 255;

/// Where Grok stores a session's `updates.jsonl`.
pub(crate) fn session_transcript_path(cwd: &str, session_id: &str) -> PathBuf {
    let sessions = grok_config_dir().join("sessions");
    let encoded = encode_path_component(cwd);
    let dir = if encoded.len() <= MAX_DIRNAME_BYTES {
        sessions.join(encoded).join(session_id)
    } else {
        // Hashed name: find the session by id instead of re-deriving it.
        std::fs::read_dir(&sessions)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join(session_id))
            .find(|dir| dir.is_dir())
            .unwrap_or_else(|| sessions.join(encoded).join(session_id))
    };
    dir.join("updates.jsonl")
}

/// The lines of `updates.jsonl` still on the live branch. A `rewind_marker`
/// discards everything from the start of its target prompt, as Grok's own
/// replay does (`session/storage` `filter_rewind_by`): prompts are counted by
/// runs of user chunks, keyed by `promptIndex` once any chunk carries one.
pub(crate) fn live_lines(content: &str) -> Vec<&str> {
    let lines: Vec<&str> = content.lines().collect();
    if !content.contains("rewind_marker") {
        return lines;
    }
    let mut live: Vec<&str> = Vec::with_capacity(lines.len());
    let mut prompt_starts: Vec<usize> = Vec::new();
    let (mut seen_index, mut in_user, mut run_index) = (false, false, None);
    for line in lines {
        let value: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let xai = value["method"] == "_x.ai/session/update";
        let update = &value["params"]["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        if xai
            && kind == "rewind_marker"
            && let Some(target) = update["target_prompt_index"].as_u64()
        {
            let target = target as usize;
            live.truncate(prompt_starts.get(target).copied().unwrap_or(live.len()));
            prompt_starts.truncate(target);
            in_user = false;
            continue;
        }
        if !xai && kind == "user_message_chunk" && update["_meta"]["hostTurn"] != true {
            let index = update["_meta"]["promptIndex"].as_u64();
            seen_index |= index.is_some();
            let new_run = !in_user || ((seen_index || index.is_some()) && index != run_index);
            if new_run {
                run_index = index;
                if !seen_index || index.is_some() {
                    prompt_starts.push(live.len());
                }
            }
            in_user = true;
        } else {
            in_user = false;
        }
        live.push(line);
    }
    live
}

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

    for line in live_lines(&content) {
        let Ok(root) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(ts) = root.get("timestamp").and_then(|v| {
            v.as_str().map(str::to_string).or_else(|| {
                v.as_i64().and_then(|seconds| {
                    chrono::DateTime::from_timestamp(seconds, 0)
                        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                })
            })
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
                // An interrupt (`cancelled`) is the user's choice, not an error.
                if !matches!(reason, "end_turn" | "cancelled") {
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
    fn numeric_timestamps_use_utc_iso_format_for_timeline() {
        let lines = [
            json!({"timestamp": 1790689536, "params": {"update": {
                "sessionUpdate": "user_message_chunk", "content": {"text": "go"}
            }}})
            .to_string(),
            update(json!({"sessionUpdate": "turn_completed"})),
        ];
        let exchanges = parse(&lines, false);
        assert_eq!(exchanges[0].timestamp, "2026-09-29T13:45:36Z");
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
            update(json!({"sessionUpdate": "turn_completed", "stop_reason": "rate_limit"})),
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

        let interrupted = [
            chunk("user_message_chunk", "go"),
            update(json!({"sessionUpdate": "turn_completed", "stop_reason": "cancelled"})),
        ];
        let turn = &parse(&interrupted, false)[0];
        assert!(!turn.ended_on_error);
        assert!(turn.errors.is_empty());
    }

    #[test]
    fn rewound_turns_are_dropped() {
        let user = |text: &str, index: u64| {
            json!({"method": "session/update", "params": {"update": {
                "sessionUpdate": "user_message_chunk", "content": {"text": text},
                "_meta": {"promptIndex": index}}}})
            .to_string()
        };
        let end = || update(json!({"sessionUpdate": "turn_completed", "stop_reason": "end_turn"}));
        let rewind = |target: u64| {
            json!({"method": "_x.ai/session/update", "params": {"update": {
                "sessionUpdate": "rewind_marker", "target_prompt_index": target}}})
            .to_string()
        };
        let lines = [
            user("first", 0),
            end(),
            user("abandoned", 1),
            chunk("agent_message_chunk", "gone"),
            end(),
            rewind(1),
            user("retry", 1),
            end(),
        ];
        let exchanges = parse(&lines, false);
        let users: Vec<&str> = exchanges.iter().map(|e| e.user.as_str()).collect();
        assert_eq!(users, ["first", "retry"]);
    }

    #[test]
    #[serial_test::serial]
    fn long_cwds_are_found_by_session_id() {
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("GROK_HOME", home.path()) };
        let long = format!("/{}", "a".repeat(300));
        let hashed = home.path().join("sessions/aaaa-0123456789abcdef/s1");
        std::fs::create_dir_all(&hashed).unwrap();
        assert_eq!(
            session_transcript_path(&long, "s1"),
            hashed.join("updates.jsonl")
        );
        assert_eq!(
            session_transcript_path("/a b", "s2"),
            home.path().join("sessions/%2Fa%20b/s2/updates.jsonl")
        );
    }
}
