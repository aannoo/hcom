//! Cursor-agent transcript parser (.jsonl).
//!
//! cursor-agent writes one JSON object per line at
//! `~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl` (the path
//! the hook hands hcom verbatim as `transcript_path`). Each line is
//! Anthropic-shaped:
//!
//! ```jsonc
//! {"role": "user",      "message": {"content": [{"type": "text", "text": "…"}]}}
//! {"role": "assistant", "message": {"content": [
//!     {"type": "text", "text": "…"},
//!     {"type": "tool_use", "name": "Shell", "input": {"command": "…"}}
//! ]}}
//! ```
//!
//! Notable differences from the Claude/Codex transcripts:
//! - **No timestamps, no `sessionId`/`cwd`, no `id` on tool_use blocks.**
//! - **Tool outputs are not recorded** (no `tool_result` blocks), so error
//!   detection has nothing to read — every tool use is reported non-error.
//! - User prompts are wrapped in `<user_query>…</user_query>`; we unwrap it.

use std::path::Path;

use serde_json::Value;

use super::shared::{
    Exchange, ToolUse, dedup_sorted_capped, finalize_action_text, normalize_tool_name,
    read_file_lossy, same_trimmed_text, truncate_str,
};

/// Map cursor's tool names onto hcom's canonical (Claude) names. cursor already
/// uses Claude-style CamelCase for most tools (`Read`, `Write`, `Grep`, `Glob`,
/// `WebFetch`, `TodoWrite`, `Shell`), so only its two divergent names need
/// remapping before falling through to the shared normalizer.
fn normalize_cursor_tool(name: &str) -> &str {
    match name {
        "StrReplace" => "Edit",
        "ReadFile" => "Read",
        other => normalize_tool_name(other),
    }
}

/// Strip cursor's `<user_query>…</user_query>` wrapper from a user prompt,
/// returning the inner text trimmed. Leaves unwrapped text untouched.
fn unwrap_user_query(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(inner) = trimmed
        .strip_prefix("<user_query>")
        .and_then(|s| s.strip_suffix("</user_query>"))
    {
        return inner.trim().to_string();
    }
    trimmed.to_string()
}

/// Join the text blocks of a `message.content` array, skipping non-text blocks
/// (tool_use / any future block types). Tolerates a bare-string `content`.
fn content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(text) = block.get("text").and_then(Value::as_str)
                {
                    let trimmed = text.trim();
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

/// Basename of a tool-input path field, if any of `path`/`file_path`/`file`.
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

/// Parse a cursor-agent JSONL transcript into the shared `Exchange` model.
pub(crate) fn parse_cursor_jsonl(
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

    // Flush the in-progress exchange (if any user/assistant content accrued).
    let flush = |exchanges: &mut Vec<Exchange>,
                 position: &mut usize,
                 in_exchange: &mut bool,
                 user: &mut String,
                 action: &mut String,
                 tools: &mut Vec<ToolUse>,
                 files: &mut Vec<String>,
                 chunks: &mut Vec<String>| {
        if !user.is_empty() || !action.is_empty() {
            *position += 1;
            let final_action = finalize_action_text(action, tools, &[], false);
            exchanges.push(Exchange {
                position: *position,
                user: std::mem::take(user),
                action: final_action,
                files: dedup_sorted_capped(files, 5),
                timestamp: String::new(),
                tools: std::mem::take(tools),
                edits: Vec::new(),
                errors: Vec::new(),
                ended_on_error: false,
            });
            action.clear();
            files.clear();
            chunks.clear();
        }
        *in_exchange = false;
    };

    for line in content.lines() {
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = entry.get("role").and_then(Value::as_str).unwrap_or("");
        let Some(message) = entry.get("message") else {
            continue;
        };

        match role {
            "user" => {
                let text = unwrap_user_query(&content_text(message));
                // A user message with no text (e.g. a tool_result-only turn)
                // must not start a new exchange.
                if text.is_empty() {
                    continue;
                }
                // Repeated identical user line before any assistant reply (e.g.
                // a re-submitted prompt): keep the open exchange, don't split.
                if in_exchange
                    && current_action.is_empty()
                    && same_trimmed_text(&current_user, &text)
                {
                    continue;
                }
                flush(
                    &mut exchanges,
                    &mut position,
                    &mut in_exchange,
                    &mut current_user,
                    &mut current_action,
                    &mut current_tools,
                    &mut current_files,
                    &mut assistant_chunks,
                );
                current_user = text;
                in_exchange = true;
            }
            "assistant" => {
                let text = content_text(message);
                if !text.is_empty()
                    && !assistant_chunks
                        .iter()
                        .any(|chunk| same_trimmed_text(chunk, &text))
                {
                    if !current_action.is_empty() {
                        current_action.push('\n');
                    }
                    current_action.push_str(&text);
                    assistant_chunks.push(text);
                }
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue;
                        }
                        let raw_name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        let name = normalize_cursor_tool(raw_name).to_string();
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        let file = tool_file(&input);
                        if let Some(ref f) = file
                            && !current_files.contains(f)
                        {
                            current_files.push(f.clone());
                        }
                        let command = input.get("command").and_then(Value::as_str).map(|s| {
                            if s.len() > 80 {
                                format!("{}...", truncate_str(s, 77))
                            } else {
                                s.to_string()
                            }
                        });
                        current_tools.push(ToolUse {
                            name,
                            is_error: false,
                            file,
                            command,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    flush(
        &mut exchanges,
        &mut position,
        &mut in_exchange,
        &mut current_user,
        &mut current_action,
        &mut current_tools,
        &mut current_files,
        &mut assistant_chunks,
    );

    if exchanges.len() > last {
        let start = exchanges.len() - last;
        exchanges = exchanges[start..].to_vec();
    }

    Ok(exchanges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn write_jsonl(lines: &[Value]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chat.jsonl");
        fs::write(
            &path,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn parses_user_query_unwrap_and_tool_use() {
        let (_d, path) = write_jsonl(&[
            json!({"role": "user", "message": {"content": [
                {"type": "text", "text": "<user_query>\nfix the parser\n</user_query>"}
            ]}}),
            json!({"role": "assistant", "message": {"content": [
                {"type": "text", "text": "On it."},
                {"type": "tool_use", "name": "Shell", "input": {"command": "cargo build", "description": "build"}}
            ]}}),
            json!({"role": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "StrReplace", "input": {"path": "/repo/src/main.rs", "old_string": "a", "new_string": "b"}}
            ]}}),
        ]);
        let ex = parse_cursor_jsonl(&path, 10, true).unwrap();
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].user, "fix the parser");
        assert_eq!(ex[0].action, "On it.");
        // Two tool_use blocks across two assistant lines, one exchange.
        assert_eq!(ex[0].tools.len(), 2);
        assert_eq!(ex[0].tools[0].name, "Shell");
        assert_eq!(ex[0].tools[0].command.as_deref(), Some("cargo build"));
        // StrReplace → canonical Edit; file basename captured.
        assert_eq!(ex[0].tools[1].name, "Edit");
        assert_eq!(ex[0].files, vec!["main.rs".to_string()]);
    }

    #[test]
    fn tool_only_turn_and_empty_user_turn_handled() {
        let (_d, path) = write_jsonl(&[
            // user turn with no text (tool_result-only shape) must not split.
            json!({"role": "user", "message": {"content": [
                {"type": "text", "text": "first"}
            ]}}),
            json!({"role": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "Read", "input": {"path": "a.txt"}}
            ]}}),
            json!({"role": "user", "message": {"content": []}}),
            json!({"role": "assistant", "message": {"content": [
                {"type": "text", "text": "done"}
            ]}}),
        ]);
        let ex = parse_cursor_jsonl(&path, 10, false).unwrap();
        // The empty user turn did not open a new exchange; assistant text
        // accrued onto the first (tool-only) exchange.
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0].user, "first");
        assert_eq!(ex[0].action, "done");
        assert_eq!(ex[0].tools.len(), 1);
        assert_eq!(ex[0].tools[0].name, "Read");
    }

    #[test]
    fn last_truncates_to_most_recent() {
        let mut lines = Vec::new();
        for i in 0..5 {
            lines.push(json!({"role": "user", "message": {"content": [
                {"type": "text", "text": format!("q{i}")}
            ]}}));
            lines.push(json!({"role": "assistant", "message": {"content": [
                {"type": "text", "text": format!("a{i}")}
            ]}}));
        }
        let (_d, path) = write_jsonl(&lines);
        let ex = parse_cursor_jsonl(&path, 2, false).unwrap();
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].user, "q3");
        assert_eq!(ex[1].user, "q4");
        // Positions are 1-based and contiguous after truncation.
        assert_eq!(ex[1].position, 5);
    }
}
