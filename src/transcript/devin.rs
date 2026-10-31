//! Devin CLI transcript parser (ATIF `.json`).
//!
//! Devin writes one JSON document per session to
//! `<data dir>/transcripts/<session_id>.json` in the Agent Trajectory
//! Interchange Format:
//!
//! ```jsonc
//! {
//!   "schema_version": "ATIF-v1.7",
//!   "session_id": "oasis-jump",
//!   "agent": {"name": "devin", "model_name": "…", "tool_definitions": […]},
//!   "steps": [
//!     {"step_id": 7, "source": "system", "message": "<system_info>…</system_info>"},
//!     {"step_id": 9, "source": "user", "message": "run the tests"},
//!     {"step_id": 11, "source": "agent", "message": "", "model_name": "…",
//!      "tool_calls": [{"tool_call_id": "t1", "function_name": "exec",
//!                      "arguments": {"command": "cargo test"}}],
//!      "observation": {"results": [{"source_call_id": "t1", "content": "…"}]},
//!      "metrics": {"prompt_tokens": 27491, "completion_tokens": 262,
//!                  "cached_tokens": 13367,
//!                  "extra": {"cache_creation_input_tokens": 12287}}}
//!   ],
//!   "final_metrics": {…}
//! }
//! ```
//!
//! `prompt_tokens` includes the cached and cache-creation tokens.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};

use super::shared::{
    Exchange, TokenUsage, ToolUse, Transcript, UsageCollector, capture_tool_output, display_path,
    final_segment, finalize_action_text, normalize_tool_name, read_file_lossy, token_counter,
    tool_call_detail, tool_input_file, truncate_str,
};

/// Exit status Devin's exec tool appends to its output (`Exit code: 1`).
fn exec_failed(output: &str) -> bool {
    output
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("Exit code: "))
        .is_some_and(|code| code.trim() != "0")
}

/// The session's working directory, from the `<system_info>` step Devin
/// records at session start (`  /path/to/dir (cwd)`).
pub(crate) fn session_cwd(steps: &[Value]) -> Option<String> {
    steps
        .iter()
        .filter(|step| step.get("source").and_then(Value::as_str) == Some("system"))
        .filter_map(|step| step.get("message").and_then(Value::as_str))
        .filter(|message| message.contains("<system_info>"))
        .flat_map(str::lines)
        .find_map(|line| line.trim().strip_suffix(" (cwd)"))
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
}

fn record_usage(usage: &mut UsageCollector, step: &Value, exchange: Option<usize>) {
    let Some(metrics) = step.get("metrics").filter(|v| v.is_object()) else {
        return;
    };
    let prompt = token_counter(metrics, "prompt_tokens");
    let cache_read = token_counter(metrics, "cached_tokens");
    let cache_write = metrics
        .get("extra")
        .map(|extra| token_counter(extra, "cache_creation_input_tokens"))
        .unwrap_or(0);
    let tokens = TokenUsage {
        input: prompt
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output: token_counter(metrics, "completion_tokens"),
        cache_read,
        cache_write,
        reasoning: None,
    };
    usage.record(
        tokens,
        Some(prompt),
        None,
        step.get("model_name").and_then(Value::as_str),
        exchange,
    );
}

#[derive(Default)]
struct Turn {
    user: String,
    timestamp: String,
    action: String,
    /// Where the text after the last tool call starts in `action`.
    final_start: usize,
    tool_since_text: bool,
    tools: Vec<ToolUse>,
    files: Vec<String>,
    errors: Vec<Value>,
    last_was_error: bool,
}

impl Turn {
    fn is_empty(&self) -> bool {
        self.user.is_empty() && self.action.is_empty() && self.tools.is_empty()
    }

    fn into_exchange(self, position: usize) -> Exchange {
        Exchange {
            usage: None,
            position,
            final_text: final_segment(&self.action, self.final_start),
            action: finalize_action_text(
                &self.action,
                &self.tools,
                &self.errors,
                self.last_was_error,
            ),
            user: self.user,
            files: self.files,
            timestamp: self.timestamp,
            tools: self.tools,
            edits: Vec::new(),
            errors: self.errors,
            ended_on_error: self.last_was_error,
        }
    }
}

pub(crate) fn parse_devin_json_with_usage(path: &Path) -> Result<Transcript, String> {
    let content = read_file_lossy(path)?;
    let data: Value =
        serde_json::from_str(&content).map_err(|err| format!("Invalid Devin transcript: {err}"))?;
    let steps = data
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| "Devin transcript has no steps".to_string())?;
    let cwd = session_cwd(steps);

    let mut usage = UsageCollector::default();
    let mut exchanges = Vec::new();
    let mut turn = Turn::default();

    for step in steps {
        let source = step.get("source").and_then(Value::as_str).unwrap_or("");
        let message = step.get("message").and_then(Value::as_str).unwrap_or("");
        match source {
            "user" => {
                if !turn.is_empty() {
                    let position = exchanges.len() + 1;
                    exchanges.push(std::mem::take(&mut turn).into_exchange(position));
                }
                turn.user = message.trim().to_string();
                turn.timestamp = step
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
            "agent" => {
                record_usage(
                    &mut usage,
                    step,
                    (!turn.user.is_empty()).then_some(exchanges.len()),
                );
                let calls = step
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let text = message.trim();
                if !text.is_empty() {
                    if !turn.action.is_empty() {
                        turn.action.push('\n');
                    }
                    // Text after the last tool call is the turn's conclusion.
                    if std::mem::take(&mut turn.tool_since_text) {
                        turn.final_start = turn.action.len();
                    }
                    turn.action.push_str(text);
                }
                let outputs: HashMap<&str, &str> = step
                    .get("observation")
                    .and_then(|o| o.get("results"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|result| {
                        Some((
                            result.get("source_call_id")?.as_str()?,
                            result.get("content")?.as_str()?,
                        ))
                    })
                    .collect();
                for call in calls {
                    let raw_name = call
                        .get("function_name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let name = normalize_tool_name(raw_name);
                    let args = call.get("arguments").cloned().unwrap_or(json!({}));
                    let output = call
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .and_then(|id| outputs.get(id).copied());
                    let is_error = name == "Bash" && output.is_some_and(exec_failed);
                    let file =
                        tool_input_file(name, &args).map(|f| display_path(f, cwd.as_deref()));
                    if let Some(file) = &file
                        && !turn.files.contains(file)
                    {
                        turn.files.push(file.clone());
                    }
                    turn.tools.push(ToolUse {
                        name: name.to_string(),
                        is_error,
                        file,
                        command: tool_call_detail(name, &args),
                        output: output.and_then(capture_tool_output),
                    });
                    if is_error {
                        turn.errors.push(json!({
                            "tool": name,
                            "content": truncate_str(output.unwrap_or(""), 300),
                        }));
                    }
                    turn.last_was_error = is_error;
                    turn.tool_since_text = true;
                }
            }
            _ => {}
        }
    }
    if !turn.is_empty() {
        let position = exchanges.len() + 1;
        exchanges.push(turn.into_exchange(position));
    }
    Ok(usage.attach(exchanges))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(steps: Value) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            json!({"schema_version": "ATIF-v1.7", "session_id": "s", "steps": steps}).to_string(),
        )
        .unwrap();
        file
    }

    #[test]
    fn parses_turns_tools_and_usage() {
        let file = transcript(json!([
            {"step_id": 1, "source": "system", "message": "You are Devin"},
            {"step_id": 2, "source": "system",
             "message": "<system_info>\nCurrent workspace directories:\n  /w/proj (cwd)\n</system_info>"},
            {"step_id": 3, "source": "user", "message": "fix it", "timestamp": "t1"},
            {"step_id": 4, "source": "agent", "message": "Looking", "model_name": "m",
             "tool_calls": [
                {"tool_call_id": "a", "function_name": "exec", "arguments": {"command": "cargo test"}},
                {"tool_call_id": "b", "function_name": "edit",
                 "arguments": {"file_path": "/w/proj/src/lib.rs", "old_string": "x", "new_string": "y"}}
             ],
             "observation": {"results": [
                {"source_call_id": "a", "content": "Output from command in shell 1:\nboom\n\nExit code: 101"},
                {"source_call_id": "b", "content": "ok"}
             ]},
             "metrics": {"prompt_tokens": 100, "completion_tokens": 7, "cached_tokens": 60,
                         "extra": {"cache_creation_input_tokens": 30}}},
            {"step_id": 5, "source": "agent", "message": "Fixed.", "tool_calls": [],
             "metrics": {"prompt_tokens": 120, "completion_tokens": 3, "cached_tokens": 100}},
            {"step_id": 6, "source": "user", "message": "thanks", "timestamp": "t2"},
            {"step_id": 7, "source": "agent", "message": "Sure", "tool_calls": []}
        ]));
        let parsed = parse_devin_json_with_usage(file.path()).unwrap();
        let exchanges = parsed.exchanges;
        assert_eq!(exchanges.len(), 2);
        let first = &exchanges[0];
        assert_eq!(first.user, "fix it");
        assert_eq!(first.timestamp, "t1");
        assert_eq!(first.action, "Looking\nFixed.");
        assert_eq!(first.final_text, "Fixed.");
        assert_eq!(first.tools.len(), 2);
        assert_eq!(first.tools[0].name, "Bash");
        assert!(first.tools[0].is_error);
        assert_eq!(first.tools[0].command.as_deref(), Some("cargo test"));
        assert!(first.tools[0].output.as_deref().unwrap().contains("boom"));
        assert_eq!(first.tools[1].name, "Edit");
        assert!(!first.tools[1].is_error);
        assert_eq!(first.files, ["src/lib.rs"]);
        assert!(!first.ended_on_error);
        assert_eq!(exchanges[1].user, "thanks");

        let usage = parsed.usage.unwrap();
        assert_eq!(usage.tokens.output, 10);
        assert_eq!(usage.tokens.cache_read, 160);
        assert_eq!(usage.tokens.cache_write, 30);
        assert_eq!(usage.tokens.input, 30);
        assert_eq!(usage.context_tokens, Some(120));
        assert_eq!(usage.model.as_deref(), Some("m"));
    }

    #[test]
    fn final_text_spans_every_step_after_the_last_tool_call() {
        let file = transcript(json!([
            {"source": "user", "message": "fix it"},
            {"source": "agent", "message": "Looking",
             "tool_calls": [{"tool_call_id": "a", "function_name": "exec", "arguments": {"command": "make"}}]},
            {"source": "agent", "message": "Fixed the bug.", "tool_calls": []},
            {"source": "agent", "message": "Tests pass.", "tool_calls": []}
        ]));
        let parsed = parse_devin_json_with_usage(file.path()).unwrap();
        assert_eq!(
            parsed.exchanges[0].final_text,
            "Fixed the bug.\nTests pass."
        );

        // A turn without tools has no separate conclusion.
        let file = transcript(json!([
            {"source": "user", "message": "hi"},
            {"source": "agent", "message": "Hello.", "tool_calls": []},
            {"source": "agent", "message": "Anything else?", "tool_calls": []}
        ]));
        let parsed = parse_devin_json_with_usage(file.path()).unwrap();
        assert_eq!(parsed.exchanges[0].final_text, "");
    }

    #[test]
    fn successful_exec_is_not_an_error() {
        let file = transcript(json!([
            {"source": "user", "message": "ls"},
            {"source": "agent", "message": "",
             "tool_calls": [{"tool_call_id": "a", "function_name": "exec", "arguments": {"command": "ls"}}],
             "observation": {"results": [{"source_call_id": "a", "content": "a.txt\n\nExit code: 0"}]}}
        ]));
        let parsed = parse_devin_json_with_usage(file.path()).unwrap();
        let tool = &parsed.exchanges[0].tools[0];
        assert!(!tool.is_error);
        assert!(tool.output.as_deref().unwrap().contains("a.txt"));
        assert_eq!(parsed.exchanges[0].action, "(tool-only turn: Bash)");
    }

    #[test]
    fn session_cwd_reads_system_info() {
        let steps = json!([
            {"source": "user", "message": "  /not/this (cwd)"},
            {"source": "system", "message": "<system_info>\n  /w/a b (cwd)\n</system_info>"}
        ]);
        assert_eq!(
            session_cwd(steps.as_array().unwrap()).as_deref(),
            Some("/w/a b")
        );
        assert_eq!(session_cwd(&[]), None);
    }

    #[test]
    fn rejects_documents_without_steps() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "{}").unwrap();
        assert!(parse_devin_json_with_usage(file.path()).is_err());
    }
}
