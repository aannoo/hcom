//! Codex transcript parser (.jsonl).

use std::path::Path;

use serde_json::{Value, json};

use super::shared::{
    Exchange, ToolUse, capture_tool_output, codex_is_error, collapse_codex_duplicate_exchanges,
    dedup_sorted_capped, extract_codex_event_message_text, extract_text_content,
    finalize_action_text, is_codex_system_injected_user_text, is_no_response_action,
    normalize_tool_name, read_file_lossy, same_trimmed_text, truncate_str,
};

/// Extract plain text from a `function_call_output.output` value.
///
/// Codex encodes this field as either a plain string (older format) or an
/// array of content items `[{"type":"input_text","text":...}, ...]` (current
/// format, codex-cli 0.14x). This mirrors codex's own lossy
/// `FunctionCallOutputContentItem` → text conversion: keep `input_text` text,
/// drop image/audio/encrypted items, join with newlines.
fn codex_output_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Forks of forks are followed this deep; a cycle or a runaway chain stops here.
const MAX_FORK_DEPTH: usize = 16;

/// A rollout's entries, preceded by whatever it inherited as a fork.
///
/// Since Codex 0.152 a fork's rollout no longer copies the parent's history: its
/// `session_meta` names the parent (`forked_from_id`) and the inherited prefix
/// (every parent entry with `ordinal < forked_from_ordinal_exclusive`). Without
/// resolving that, a fork's transcript starts at its first own turn.
fn read_codex_rollout_entries(path: &Path) -> Result<Vec<Value>, String> {
    let own = read_rollout_lines(path)?;
    let mut entries = inherited_entries(path, &own, 0);
    entries.extend(own);
    Ok(entries)
}

fn read_rollout_lines(path: &Path) -> Result<Vec<Value>, String> {
    Ok(read_file_lossy(path)?
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// What the rollout at `path` (whose parsed lines are `own`) inherited from its
/// fork parent, recursively. Empty when it is not a fork, or when the parent
/// rollout is gone — the fork's own turns still render.
fn inherited_entries(path: &Path, own: &[Value], depth: usize) -> Vec<Value> {
    let Some((parent_id, bound)) = own.first().and_then(fork_reference) else {
        return Vec::new();
    };
    if depth >= MAX_FORK_DEPTH {
        return Vec::new();
    }
    let Some(parent_path) = find_sibling_rollout(path, &parent_id) else {
        return Vec::new();
    };
    let Ok(parent_own) = read_rollout_lines(&parent_path) else {
        return Vec::new();
    };
    let mut entries = inherited_entries(&parent_path, &parent_own, depth + 1);
    entries.extend(
        parent_own
            .into_iter()
            .enumerate()
            .filter(|(index, entry)| {
                let ordinal = entry.get("ordinal").and_then(Value::as_u64);
                ordinal.unwrap_or(*index as u64) < bound
            })
            .map(|(_, entry)| entry),
    );
    entries
}

/// `(parent id, inherited ordinal bound)` when `entry` is a forked rollout's
/// `session_meta` that references its parent rather than copying it.
fn fork_reference(entry: &Value) -> Option<(String, u64)> {
    if entry.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let meta = entry.get("payload")?;
    let parent_id = meta.get("forked_from_id")?.as_str()?.to_string();
    let inherited = meta.get("forked_from_ordinal_exclusive")?.as_u64()?;
    Some((parent_id, inherited))
}

/// The rollout for session `id` under the same `sessions` root as `path`
/// (rollouts live in dated subdirectories, so the parent may be days away).
fn find_sibling_rollout(path: &Path, id: &str) -> Option<std::path::PathBuf> {
    let sessions = path
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "sessions"))?;
    let pattern = format!("{}/**/rollout-*-{id}.jsonl", sessions.display());
    glob::glob(&pattern).ok()?.filter_map(Result::ok).next()
}

/// Parse Codex JSONL transcript.
/// Handles both response_item (older) and event_msg (newer) formats.
pub(crate) fn parse_codex_jsonl(
    path: &Path,
    last: usize,
    detailed: bool,
) -> Result<Vec<Exchange>, String> {
    let entries = read_codex_rollout_entries(path)?;

    // First pass: build call_id → output map for error detection
    let mut call_outputs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut parsed_lines: Vec<Value> = Vec::new();

    for entry in entries {
        let payload = entry.get("payload").cloned().unwrap_or(entry.clone());
        let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if payload_type == "function_call_output" {
            let call_id = payload
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output = codex_output_text(payload.get("output"));
            call_outputs.insert(call_id, output);
        }
        parsed_lines.push(entry);
    }

    // Second pass: build exchanges
    let mut exchanges = Vec::new();
    let mut current_user = String::new();
    let mut current_action = String::new();
    let mut current_tools: Vec<ToolUse> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();
    let mut current_ts = String::new();
    let mut current_errors: Vec<Value> = Vec::new();
    let mut current_last_was_error = false;
    let mut current_assistant_chunks: Vec<String> = Vec::new();
    let mut position = 0;
    let mut in_exchange = false; // track whether we have a real user entry

    let save_exchange = |exchanges: &mut Vec<Exchange>,
                         position: &mut usize,
                         in_exchange: &mut bool,
                         user: &mut String,
                         action: &mut String,
                         files: &mut Vec<String>,
                         ts: &mut String,
                         tools: &mut Vec<ToolUse>,
                         errors: &mut Vec<Value>,
                         assistant_chunks: &mut Vec<String>,
                         last_was_error: &mut bool| {
        if !user.is_empty() || !action.is_empty() {
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
            let _ = std::mem::take(action);
            files.clear();
            assistant_chunks.clear();
            *last_was_error = false;
        }
        *in_exchange = false;
    };

    for entry in &parsed_lines {
        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let payload = entry.get("payload").cloned().unwrap_or(entry.clone());
        let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Skip system entry types
        if matches!(
            entry_type,
            "session_meta" | "turn_context" | "session_start" | "session_end"
        ) {
            continue;
        }

        // Handle response_item format (older Codex)
        if entry_type == "response_item" || (entry_type.is_empty() && payload_type == "message") {
            match payload_type {
                "message" => {
                    let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
                    if role == "user" {
                        let text = extract_text_content(&payload);
                        // Only start exchange if user has actual text
                        if text.is_empty() || is_codex_system_injected_user_text(&text) {
                            continue;
                        }
                        if in_exchange
                            && current_action.is_empty()
                            && same_trimmed_text(&current_user, &text)
                        {
                            current_ts = ts.clone();
                            continue;
                        }
                        save_exchange(
                            &mut exchanges,
                            &mut position,
                            &mut in_exchange,
                            &mut current_user,
                            &mut current_action,
                            &mut current_files,
                            &mut current_ts,
                            &mut current_tools,
                            &mut current_errors,
                            &mut current_assistant_chunks,
                            &mut current_last_was_error,
                        );
                        current_user = text;
                        current_ts = ts.clone();
                        in_exchange = true;
                    } else if role == "assistant" {
                        let text = extract_text_content(&payload);
                        if !text.is_empty() {
                            if current_assistant_chunks
                                .iter()
                                .any(|chunk| same_trimmed_text(chunk, &text))
                            {
                                continue;
                            }
                            if !current_action.is_empty() {
                                current_action.push('\n');
                            }
                            current_action.push_str(&text);
                            current_assistant_chunks.push(text);
                        }
                    }
                }
                "function_call" => {
                    let raw_name = payload
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let tool_name = normalize_tool_name(raw_name);
                    let args_str = payload
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                    let call_id = payload
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    // Extract files from args
                    if let Some(obj) = args.as_object() {
                        for field in &["file_path", "path", "file"] {
                            if let Some(val) = obj.get(*field).and_then(|v| v.as_str())
                                && !val.is_empty()
                            {
                                let fname = Path::new(val)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(val)
                                    .to_string();
                                if !current_files.contains(&fname) {
                                    current_files.push(fname);
                                }
                            }
                        }
                    }

                    // Determine is_error from call output
                    let output = call_outputs.get(&call_id).map(|s| s.as_str()).unwrap_or("");
                    let is_err = if detailed {
                        codex_is_error(output)
                    } else {
                        false
                    };

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
                        output: capture_tool_output(output),
                    });

                    if is_err {
                        let truncated = truncate_str(output, 300);
                        current_errors.push(json!({
                            "tool": tool_name,
                            "content": truncated,
                        }));
                        current_last_was_error = true;
                    } else {
                        current_last_was_error = false;
                    }
                }
                _ => {}
            }
        }
        // Handle event_msg format (newer Codex)
        else if entry_type == "event_msg" {
            match payload_type {
                "user_message" => {
                    let text = extract_codex_event_message_text(&payload);
                    if text.is_empty() {
                        continue;
                    }
                    if in_exchange
                        && current_action.is_empty()
                        && same_trimmed_text(&current_user, &text)
                    {
                        continue;
                    }
                    save_exchange(
                        &mut exchanges,
                        &mut position,
                        &mut in_exchange,
                        &mut current_user,
                        &mut current_action,
                        &mut current_files,
                        &mut current_ts,
                        &mut current_tools,
                        &mut current_errors,
                        &mut current_assistant_chunks,
                        &mut current_last_was_error,
                    );
                    current_user = text;
                    current_ts = ts.clone();
                    in_exchange = true;
                }
                "agent_message" => {
                    let text = extract_codex_event_message_text(&payload);
                    if !text.is_empty() {
                        if current_assistant_chunks
                            .iter()
                            .any(|chunk| same_trimmed_text(chunk, &text))
                        {
                            continue;
                        }
                        if !current_action.is_empty() {
                            current_action.push('\n');
                        }
                        current_action.push_str(&text);
                        current_assistant_chunks.push(text);
                    }
                }
                _ => {} // token_count, agent_reasoning, etc — skip
            }
        }
    }

    // Last exchange
    save_exchange(
        &mut exchanges,
        &mut position,
        &mut in_exchange,
        &mut current_user,
        &mut current_action,
        &mut current_files,
        &mut current_ts,
        &mut current_tools,
        &mut current_errors,
        &mut current_assistant_chunks,
        &mut current_last_was_error,
    );

    exchanges = collapse_codex_duplicate_exchanges(exchanges);

    if exchanges.len() > last {
        let start = exchanges.len() - last;
        exchanges = exchanges[start..].to_vec();
    }

    Ok(exchanges)
}

pub(crate) fn should_retry_codex_transcript(exchanges: &[Exchange]) -> bool {
    exchanges
        .last()
        .map(|ex| is_no_response_action(&ex.action))
        .unwrap_or(false)
}

pub(crate) fn retry_codex_transcript(
    path: &Path,
    last: usize,
    detailed: bool,
    mut exchanges: Vec<Exchange>,
) -> Result<Vec<Exchange>, String> {
    for _ in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let retried = parse_codex_jsonl(path, last, detailed)?;
        if !should_retry_codex_transcript(&retried) {
            return Ok(retried);
        }
        if retried.len() > exchanges.len() {
            exchanges = retried;
        }
    }
    Ok(exchanges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{ReadOptions, TranscriptBackend, read};
    use std::fs;

    /// One rollout under `sessions/<day>/`, as Codex >= 0.152 writes it: every
    /// line carries an `ordinal`, and a fork's `session_meta` references its
    /// parent instead of copying the parent's history.
    fn write_rollout(
        sessions: &Path,
        day: &str,
        id: &str,
        fork_of: Option<(&str, u64)>,
        turns: &[(&str, &str)],
    ) -> std::path::PathBuf {
        let mut meta = json!({"session_id": id, "id": id});
        if let Some((parent, bound)) = fork_of {
            meta["forked_from_id"] = json!(parent);
            meta["forked_from_ordinal_exclusive"] = json!(bound);
        }
        let mut lines = vec![json!({"type": "session_meta", "payload": meta})];
        for (user, assistant) in turns {
            for (role, kind, text) in [
                ("user", "input_text", user),
                ("assistant", "output_text", assistant),
            ] {
                lines.push(json!({
                    "type": "response_item",
                    "payload": {"type": "message", "role": role, "content": [{"type": kind, "text": text}]}
                }));
            }
        }
        let body: String = lines
            .into_iter()
            .enumerate()
            .map(|(ordinal, mut line)| {
                line["ordinal"] = json!(ordinal);
                format!("{line}\n")
            })
            .collect();
        let dir = sessions.join(day);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-{day}-{id}.jsonl"));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn test_parse_codex_fork_includes_inherited_history_through_nested_forks() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        // Ordinals: 0 session_meta, then 2 per turn. The child forks after the
        // root's first turn (bound 3), so the root's later turn must not leak in.
        write_rollout(&sessions, "d1", "root", None, &[("r1", "R1"), ("r2", "R2")]);
        write_rollout(&sessions, "d2", "child", Some(("root", 3)), &[("c1", "C1")]);
        let grandchild = write_rollout(
            &sessions,
            "d3",
            "grandchild",
            Some(("child", 3)),
            &[("g1", "G1")],
        );

        let exchanges = parse_codex_jsonl(&grandchild, 10, false).unwrap();
        let users: Vec<&str> = exchanges.iter().map(|e| e.user.as_str()).collect();
        assert_eq!(users, ["r1", "c1", "g1"]);
    }

    #[test]
    fn test_parse_codex_fork_with_missing_parent_keeps_own_turns() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let orphan = write_rollout(
            &sessions,
            "d1",
            "orphan",
            Some(("gone", 5)),
            &[("o1", "O1")],
        );

        let exchanges = parse_codex_jsonl(&orphan, 10, false).unwrap();
        let users: Vec<&str> = exchanges.iter().map(|e| e.user.as_str()).collect();
        assert_eq!(users, ["o1"]);
    }

    #[test]
    fn test_parse_codex_prefers_response_items_over_event_msgs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            json!({
                "type": "event_msg",
                "timestamp": "2026-03-27T10:00:00.100Z",
                "payload": {"type": "user_message", "message": "response user"}
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-03-27T10:00:01.100Z",
                "payload": {"type": "agent_message", "message": "response assistant"}
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "response user"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:01Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "response assistant"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:02Z",
                "payload": {
                    "type": "function_call",
                    "name": "shell",
                    "call_id": "call_1",
                    "arguments": "{\"command\":\"pwd\"}"
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:03Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    // Current codex encodes output as an array of content items,
                    // not a plain string. The parser must join input_text items.
                    "output": [
                        {"type": "input_text", "text": "Chunk ID: abc"},
                        {"type": "input_image", "image_url": "data:image/png;base64,xxx"},
                        {"type": "input_text", "text": "Process exited with code 0\nFinal output:\n/work"}
                    ]
                }
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-03-27T10:01:00Z",
                "payload": {"type": "user_message", "message": "event only user"}
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-03-27T10:01:01Z",
                "payload": {"type": "agent_message", "message": "event only assistant"}
            }),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let exchanges = parse_codex_jsonl(&path, 10, false).unwrap();
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].user, "response user");
        assert_eq!(exchanges[0].action, "response assistant");
        assert_eq!(exchanges[0].tools.len(), 1);
        assert_eq!(exchanges[0].tools[0].name, "Bash");
        // Array output: both input_text segments joined, image item dropped.
        assert_eq!(
            exchanges[0].tools[0].output.as_deref(),
            Some("Chunk ID: abc\nProcess exited with code 0\nFinal output:\n/work")
        );
        assert_eq!(exchanges[1].user, "event only user");
        assert_eq!(exchanges[1].action, "event only assistant");
    }

    #[test]
    fn codex_output_text_handles_string_and_array_bodies() {
        assert_eq!(
            codex_output_text(Some(&json!("plain string"))),
            "plain string"
        );
        assert_eq!(
            codex_output_text(Some(&json!([
                {"type": "input_text", "text": "a"},
                {"type": "input_text", "text": "  "},
                {"type": "input_image", "image_url": "x"},
                {"type": "input_text", "text": "b"}
            ]))),
            "a\nb"
        );
        assert_eq!(codex_output_text(None), "");
    }

    #[test]
    fn test_parse_codex_dedupes_repeated_assistant_chunks_within_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "analyze more code and write implementation plan"}]
                }
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-03-27T10:00:01Z",
                "payload": {"type": "agent_message", "message": "first commentary"}
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:01Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "first commentary"}]
                }
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-03-27T10:00:02Z",
                "payload": {"type": "agent_message", "message": "second answer"}
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:02Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "second answer"}]
                }
            }),
        ];
        fs::write(
            &path,
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let exchanges = parse_codex_jsonl(&path, 10, false).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].action, "first commentary\nsecond answer");
    }

    #[test]
    fn test_get_exchanges_retries_transient_codex_no_response_tail() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("rollout.jsonl");
        fs::write(
            &transcript_path,
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "user prompt"}]
                }
            })
            .to_string(),
        )
        .unwrap();

        let path_for_thread = transcript_path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            fs::write(
                &path_for_thread,
                [
                    json!({
                        "type": "response_item",
                        "timestamp": "2026-03-27T10:00:00Z",
                        "payload": {
                            "type": "message",
                            "role": "user",
                            "content": [{"type": "input_text", "text": "user prompt"}]
                        }
                    }),
                    json!({
                        "type": "response_item",
                        "timestamp": "2026-03-27T10:00:01Z",
                        "payload": {
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "assistant answer"}]
                        }
                    }),
                ]
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            )
            .unwrap();
        });

        let opts = ReadOptions {
            last: 10,
            detailed: false,
            session_id: None,
            allow_codex_retry: true,
        };
        let exchanges = read(&transcript_path, TranscriptBackend::CodexJsonl, &opts).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].action, "assistant answer");
    }
}
