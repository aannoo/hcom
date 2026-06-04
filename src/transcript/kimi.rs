//! Kimi Code CLI transcript parser.
//!
//! Reads a session's `wire.jsonl` (the kimi-code wire log) and normalizes it
//! into the tool-agnostic `Exchange` format used by hcom.
//!
//! `wire.jsonl` is a streaming event log — one JSON object per line, tagged by
//! `type`. A conversational turn is structured as:
//!   - `turn.prompt` — `{input:[{type:"text",text}], origin}` — the submitted prompt
//!   - `context.append_message` — persisted messages. Only `role:"user"` is
//!     persisted live (assistant content is streamed, see below). The `origin`
//!     distinguishes real input (`user`) from hcom hook deliveries
//!     (`hook_result`, `system_trigger`).
//!   - `context.append_loop_event` — carries the assistant turn:
//!       - `event.type:"content.part"` → `part:{type:"text"|"think", …}`
//!       - `event.type:"tool.call"`    → `{toolCallId, name, args:{…}}`
//!       - `event.type:"tool.result"`  → `{toolCallId, result:{output, isError}}`
//!
//! Other event types (metadata, config.update, usage.record, step.*, …) are
//! skipped. Assistant text/think/tools are NOT in `context.append_message`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::{Value, json};

use super::shared::{
    Exchange, ToolUse, extract_content_text, extract_edit_info, finalize_action_text,
    is_error_result, normalize_tool_name,
};

/// In-progress turn accumulator.
#[derive(Default)]
struct Turn {
    user: String,
    think: String,
    text: String,
    tools: Vec<ToolUse>,
    edits: Vec<Value>,
    errors: Vec<Value>,
    ended_on_error: bool,
    files: Vec<String>,
    /// toolCallId → index into `tools`, for matching results back to calls.
    call_index: HashMap<String, usize>,
}

/// Parse a Kimi `wire.jsonl` into exchanges.
pub fn parse_kimi_wire_jsonl(
    path: &Path,
    last: usize,
    detailed: bool,
) -> Result<Vec<Exchange>, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
    let reader = BufReader::new(file);

    let mut exchanges: Vec<Exchange> = Vec::new();
    let mut position = 0;
    let mut cur: Option<Turn> = None;

    for line in reader.lines() {
        let line = line.map_err(|e| format!("Read error: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).map_err(|e| format!("JSON parse: {e}"))?;

        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "turn.prompt" => {
                if let Some(t) = cur.take() {
                    position += 1;
                    exchanges.push(build_exchange(t, position, detailed));
                }
                cur = Some(Turn {
                    user: extract_content_text(v.get("input")),
                    ..Default::default()
                });
            }
            "context.append_message" => {
                let Some(t) = cur.as_mut() else { continue };
                let Some(m) = v.get("message") else { continue };
                if m.get("role").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                // hcom messages arrive as hook-injected user messages; prefer
                // their (unwrapped) content over the bare `<hcom>` trigger that
                // turn.prompt recorded.
                let origin = m
                    .get("origin")
                    .and_then(|o| o.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if origin == "hook_result" || origin == "system_trigger" {
                    let cleaned = unwrap_hook_result(&extract_content_text(m.get("content")));
                    if !cleaned.is_empty() {
                        t.user = cleaned;
                    }
                }
            }
            "context.append_loop_event" => {
                let Some(t) = cur.as_mut() else { continue };
                let Some(ev) = v.get("event") else { continue };
                match ev.get("type").and_then(Value::as_str).unwrap_or("") {
                    "content.part" => {
                        let part = ev.get("part").unwrap_or(&Value::Null);
                        match part.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" => {
                                if let Some(s) = part.get("text").and_then(Value::as_str) {
                                    t.text.push_str(s);
                                }
                            }
                            "think" if detailed => {
                                if let Some(s) = part.get("think").and_then(Value::as_str) {
                                    t.think.push_str(s);
                                }
                            }
                            _ => {}
                        }
                    }
                    "tool.call" => {
                        let name = ev.get("name").and_then(Value::as_str).unwrap_or("unknown");
                        let args = ev.get("args").cloned().unwrap_or_else(|| json!({}));
                        // Kimi file tools key the path off `path`; alias it to
                        // `file_path` for the shared file/edit extractors.
                        let mut aliased = args.clone();
                        if let Some(obj) = aliased.as_object_mut()
                            && !obj.contains_key("file_path")
                            && let Some(p) = obj.get("path").cloned()
                        {
                            obj.insert("file_path".into(), p);
                        }
                        let file = aliased
                            .get("file_path")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let command = aliased
                            .get("command")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        if let Some(f) = &file
                            && !t.files.contains(f)
                        {
                            t.files.push(f.clone());
                        }
                        if let Some(edit) = extract_edit_info(&None, &aliased) {
                            t.edits.push(edit);
                        }
                        if let Some(id) = ev.get("toolCallId").and_then(Value::as_str) {
                            t.call_index.insert(id.to_string(), t.tools.len());
                        }
                        t.tools.push(ToolUse {
                            name: normalize_tool_name(name).to_string(),
                            is_error: false,
                            file,
                            command,
                        });
                    }
                    "tool.result" => {
                        let id = ev.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                        let result = ev.get("result").unwrap_or(&Value::Null);
                        let output = result.get("output").and_then(Value::as_str).unwrap_or("");
                        let is_err = result
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                            || is_error_result(&json!({"is_error": false, "content": output}));
                        t.ended_on_error = is_err;
                        if is_err && let Some(&idx) = t.call_index.get(id) {
                            t.tools[idx].is_error = true;
                            let tool = t.tools[idx].name.clone();
                            t.errors.push(json!({"tool": tool, "error": output}));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if let Some(t) = cur.take() {
        position += 1;
        exchanges.push(build_exchange(t, position, detailed));
    }

    if !detailed && exchanges.len() > last {
        exchanges = exchanges.split_off(exchanges.len() - last);
    }

    Ok(exchanges)
}

fn build_exchange(t: Turn, position: usize, detailed: bool) -> Exchange {
    let text = t.text.trim();
    let think = t.think.trim();
    let action_text = if detailed && !think.is_empty() {
        if text.is_empty() {
            format!("[think]\n{think}")
        } else {
            format!("[think]\n{think}\n\n{text}")
        }
    } else {
        text.to_string()
    };
    let action = finalize_action_text(&action_text, &t.tools, &t.errors, t.ended_on_error);

    Exchange {
        position,
        user: t.user,
        action,
        files: t.files,
        timestamp: String::new(),
        tools: t.tools,
        edits: t.edits,
        errors: t.errors,
        ended_on_error: t.ended_on_error,
    }
}

/// Strip the `<hook_result hook_event="…">…</hook_result>` wrapper that kimi
/// puts around UserPromptSubmit hook deliveries, leaving the inner message.
fn unwrap_hook_result(text: &str) -> String {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("<hook_result")
        && let Some(gt) = rest.find('>')
    {
        let inner = rest[gt + 1..]
            .trim_end()
            .strip_suffix("</hook_result>")
            .unwrap_or(&rest[gt + 1..]);
        return inner.trim().to_string();
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn turn_prompt(text: &str) -> String {
        json!({"type": "turn.prompt", "input": [{"type": "text", "text": text}], "origin": {"kind": "user"}}).to_string()
    }
    fn content_part(part: Value) -> String {
        json!({"type": "context.append_loop_event", "event": {"type": "content.part", "part": part}})
            .to_string()
    }
    fn tool_call(id: &str, name: &str, args: Value) -> String {
        json!({"type": "context.append_loop_event", "event": {"type": "tool.call", "toolCallId": id, "name": name, "args": args}}).to_string()
    }
    fn tool_result(id: &str, result: Value) -> String {
        json!({"type": "context.append_loop_event", "event": {"type": "tool.result", "toolCallId": id, "result": result}}).to_string()
    }
    fn append_user(origin: Value, text: &str) -> String {
        json!({"type": "context.append_message", "message": {"role": "user", "content": [{"type": "text", "text": text}], "origin": origin}}).to_string()
    }

    fn make_temp_jsonl(lines: &[String]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file
    }

    #[test]
    fn extracts_assistant_text_from_loop_events() {
        let lines = vec![
            json!({"type": "metadata", "protocol_version": "1.3"}).to_string(),
            turn_prompt("what is hcom?"),
            content_part(json!({"type": "think", "think": "pondering"})),
            content_part(json!({"type": "text", "text": "hcom is a comms tool."})),
            json!({"type": "usage.record", "usage": {}}).to_string(),
        ];
        let jsonl = make_temp_jsonl(&lines);

        let ex = parse_kimi_wire_jsonl(jsonl.path(), 10, false).unwrap();
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].user, "what is hcom?");
        // The core regression: assistant text must be extracted (not "(no response)").
        assert_eq!(ex[0].action, "hcom is a comms tool.");
    }

    #[test]
    fn detailed_includes_think() {
        let lines = vec![
            turn_prompt("q"),
            content_part(json!({"type": "think", "think": "reasoning"})),
            content_part(json!({"type": "text", "text": "answer"})),
        ];
        let jsonl = make_temp_jsonl(&lines);

        let ex = parse_kimi_wire_jsonl(jsonl.path(), 10, true).unwrap();
        assert!(ex[0].action.contains("[think]"));
        assert!(ex[0].action.contains("reasoning"));
        assert!(ex[0].action.contains("answer"));
    }

    #[test]
    fn parses_tool_call_and_result() {
        let lines = vec![
            turn_prompt("run who"),
            tool_call("c1", "Bash", json!({"command": "who"})),
            tool_result("c1", json!({"output": "anno  console"})),
            content_part(json!({"type": "text", "text": "done"})),
        ];
        let jsonl = make_temp_jsonl(&lines);

        let ex = parse_kimi_wire_jsonl(jsonl.path(), 10, true).unwrap();
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].tools.len(), 1);
        assert_eq!(ex[0].tools[0].name, "Bash");
        assert_eq!(ex[0].tools[0].command.as_deref(), Some("who"));
        assert!(!ex[0].tools[0].is_error);
        assert_eq!(ex[0].action, "done");
    }

    #[test]
    fn flags_tool_error_via_iserror() {
        let lines = vec![
            turn_prompt("edit"),
            tool_call(
                "c2",
                "Edit",
                json!({"path": "/a.txt", "old_string": "x", "new_string": "y"}),
            ),
            tool_result("c2", json!({"output": "rejected by user", "isError": true})),
        ];
        let jsonl = make_temp_jsonl(&lines);

        let ex = parse_kimi_wire_jsonl(jsonl.path(), 10, true).unwrap();
        assert!(ex[0].tools[0].is_error);
        assert!(ex[0].ended_on_error);
        assert_eq!(ex[0].tools[0].name, "Edit");
        assert_eq!(ex[0].files, vec!["/a.txt".to_string()]);
        assert_eq!(ex[0].errors.len(), 1);
    }

    #[test]
    fn hcom_delivery_uses_hook_message_not_trigger() {
        // hcom delivery: the trigger is `<hcom>` but the real message arrives as
        // a hook_result user message — the transcript should show the message.
        let lines = vec![
            turn_prompt("<hcom>"),
            append_user(json!({"kind": "user"}), "<hcom>"),
            append_user(
                json!({"kind": "hook_result", "event": "UserPromptSubmit"}),
                "<hook_result hook_event=\"UserPromptSubmit\">\n<hcom>[request #1] bigboss → me: ping</hcom>\n</hook_result>",
            ),
            content_part(json!({"type": "text", "text": "pong"})),
        ];
        let jsonl = make_temp_jsonl(&lines);

        let ex = parse_kimi_wire_jsonl(jsonl.path(), 10, false).unwrap();
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].user, "<hcom>[request #1] bigboss → me: ping</hcom>");
        assert_eq!(ex[0].action, "pong");
    }

    #[test]
    fn multiple_turns_split_correctly() {
        let lines = vec![
            turn_prompt("first"),
            content_part(json!({"type": "text", "text": "one"})),
            turn_prompt("second"),
            content_part(json!({"type": "text", "text": "two"})),
        ];
        let jsonl = make_temp_jsonl(&lines);

        let ex = parse_kimi_wire_jsonl(jsonl.path(), 10, false).unwrap();
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].user, "first");
        assert_eq!(ex[0].action, "one");
        assert_eq!(ex[1].user, "second");
        assert_eq!(ex[1].action, "two");
    }
}
