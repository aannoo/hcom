//! Mistral Vibe transcript parser (unified session store, Vibe 3.x).
//!
//! A session lives in `$VIBE_HOME/logs/session/unified/<session_id>/`. hcom
//! records the session's `meta.json` as its transcript path (any file inside
//! the session directory works); the history is the latest snapshot:
//!
//! ```text
//! CURRENT                               {"generation": "<gen>", ...}
//! generations/<gen>/manifest.json       projection_state.chunks: [<sha>, ...]
//! chunks/<sha>.json                     [public history entry, ...]
//! ```
//!
//! Vibe writes a snapshot whenever the session goes quiet (every turn end), so
//! completed turns are always there; a turn still running is not shown.
//! Entries are `message` (user / assistant text), `effect` (tool calls) and
//! `notice`. User messages hold only the typed text: hook context (hcom's
//! bootstrap and deliveries) is kept out of them by Vibe.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::shared::{
    Exchange, ToolUse, Transcript, UsageCollector, capture_tool_output, dedup_sorted_capped,
    display_path, extract_content_text, final_segment, finalize_action_text, normalize_tool_name,
    tool_input_file, truncate_str,
};

/// The session directory: the nearest ancestor of `path` holding `CURRENT`.
fn session_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| dir.join("CURRENT").is_file())
        .map(Path::to_path_buf)
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("Invalid JSON in {}: {e}", path.display()))
}

/// Public history entries of the session's latest snapshot, in order.
fn snapshot_entries(dir: &Path) -> Result<Vec<Value>, String> {
    let current = read_json(&dir.join("CURRENT"))?;
    let generation = current
        .get("generation")
        .and_then(Value::as_str)
        .ok_or("Vibe session has no current generation")?;
    let manifest = read_json(
        &dir.join("generations")
            .join(generation)
            .join("manifest.json"),
    )?;
    let chunks = manifest
        .pointer("/projection_state/chunks")
        .and_then(Value::as_array)
        .ok_or("Vibe snapshot lists no history chunks")?;
    let mut entries = Vec::new();
    for sha in chunks.iter().filter_map(Value::as_str) {
        match read_json(&dir.join("chunks").join(format!("{sha}.json")))? {
            Value::Array(items) => entries.extend(items),
            _ => return Err(format!("Vibe history chunk {sha} is not an array")),
        }
    }
    Ok(entries)
}

fn format_ms(ms: Option<i64>) -> String {
    ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

#[derive(Default)]
struct Turn {
    user: String,
    action: String,
    final_start: usize,
    tool_since_text: bool,
    tools: Vec<ToolUse>,
    files: Vec<String>,
    errors: Vec<Value>,
    last_was_error: bool,
    timestamp: String,
}

impl Turn {
    fn is_empty(&self) -> bool {
        self.user.is_empty() && self.action.is_empty() && self.tools.is_empty()
    }

    fn into_exchange(self, position: usize) -> Exchange {
        Exchange {
            usage: None,
            position,
            action: finalize_action_text(
                &self.action,
                &self.tools,
                &self.errors,
                self.last_was_error,
            ),
            final_text: final_segment(&self.action, self.final_start),
            user: self.user,
            files: dedup_sorted_capped(&self.files, 5),
            timestamp: self.timestamp,
            tools: self.tools,
            edits: Vec::new(),
            errors: self.errors,
            ended_on_error: self.last_was_error,
        }
    }

    fn add_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !self.action.is_empty() {
            self.action.push_str("\n\n");
        }
        if std::mem::take(&mut self.tool_since_text) {
            self.final_start = self.action.len();
        }
        self.action.push_str(text);
    }

    fn add_tool(&mut self, entry: &Value, cwd: Option<&str>) {
        let detail = entry.get("detail").unwrap_or(&Value::Null);
        let qualified = detail.get("toolName").and_then(Value::as_str).unwrap_or("");
        if qualified.is_empty() {
            return;
        }
        // `file_system.bash` → `bash`.
        let leaf = qualified.rsplit('.').next().unwrap_or(qualified);
        let input = detail.get("input").cloned().unwrap_or_default();
        let state = entry.get("state").unwrap_or(&Value::Null);
        let is_error = state.pointer("/display/success").and_then(Value::as_bool) == Some(false);
        // The first block is the tool's own output; later blocks are hook
        // additions such as hcom's mid-turn deliveries.
        let output = state
            .pointer("/output/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let file = tool_input_file(leaf, &input).map(|f| display_path(f, cwd));
        if let Some(file) = &file {
            self.files.push(file.clone());
        }
        if is_error {
            self.errors.push(json!({
                "tool": leaf,
                "content": truncate_str(output, 300),
            }));
        }
        self.tools.push(ToolUse {
            name: normalize_tool_name(leaf).to_string(),
            is_error,
            file,
            command: input
                .get("command")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            output: capture_tool_output(output),
        });
        self.last_was_error = is_error;
        self.tool_since_text = true;
    }
}

pub(crate) fn parse_vibe_session_with_usage(
    path: &Path,
    _detailed: bool,
) -> Result<Transcript, String> {
    let dir = session_dir(path)
        .ok_or_else(|| format!("Not a Vibe session directory: {}", path.display()))?;
    let cwd = read_json(&dir.join("meta.json")).ok().and_then(|meta| {
        meta.pointer("/environment/working_directory")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    });
    let entries = snapshot_entries(&dir)?;

    let mut exchanges = Vec::new();
    let mut turn = Turn::default();
    for entry in &entries {
        match entry.get("type").and_then(Value::as_str) {
            Some("message") => {
                let text = extract_content_text(entry.get("content"))
                    .trim()
                    .to_string();
                match entry.get("role").and_then(Value::as_str) {
                    Some("user") => {
                        if !turn.is_empty() {
                            exchanges
                                .push(std::mem::take(&mut turn).into_exchange(exchanges.len() + 1));
                        }
                        turn.user = text;
                        turn.timestamp = format_ms(entry.get("createdAt").and_then(Value::as_i64));
                    }
                    Some("assistant") => {
                        // A reply a `post_agent` hook sent back for revision
                        // is shown settled but never reached the user.
                        let discarded = entry
                            .pointer("/outcome/type")
                            .and_then(Value::as_str)
                            .is_some_and(|outcome| outcome != "committed");
                        if !discarded {
                            turn.add_text(&text);
                        }
                    }
                    _ => {}
                }
            }
            Some("effect")
                if entry.pointer("/detail/kind").and_then(Value::as_str) == Some("tool") =>
            {
                turn.add_tool(entry, cwd.as_deref());
            }
            _ => {}
        }
    }
    if !turn.is_empty() {
        exchanges.push(turn.into_exchange(exchanges.len() + 1));
    }
    Ok(UsageCollector::default().attach(exchanges))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a minimal unified session store with the given history entries.
    fn write_session(root: &Path, entries: Value) -> PathBuf {
        let dir = root.join("logs/session/unified/sess-1");
        let generation = dir.join("generations/0000000000000004");
        std::fs::create_dir_all(&generation).unwrap();
        std::fs::create_dir_all(dir.join("chunks")).unwrap();
        std::fs::write(
            dir.join("CURRENT"),
            r#"{"generation":"0000000000000004","session_id":"sess-1"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"session_id":"sess-1","environment":{"working_directory":"/work"}}"#,
        )
        .unwrap();
        std::fs::write(
            generation.join("manifest.json"),
            r#"{"projection_state":{"chunks":["abc"]}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("chunks/abc.json"), entries.to_string()).unwrap();
        dir.join("meta.json")
    }

    #[test]
    fn parses_messages_and_tool_effects_from_latest_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = write_session(
            tmp.path(),
            json!([
                {"type": "notice", "message": "Added context for the model: \"<hcom>…\""},
                {"type": "message", "role": "user", "createdAt": 1795508166841_i64,
                 "content": [{"type": "text", "text": "run it"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "text", "text": "Let me run it."}]},
                {"type": "effect", "detail": {"kind": "tool", "toolName": "file_system.bash",
                  "input": {"command": "echo hi"}},
                 "state": {"display": {"success": true}, "output": {"content": [
                    {"type": "text", "text": "stdout: hi"},
                    {"type": "text", "text": "<hcom>[1 new message] luna -> vibe: ping</hcom>"}]}}},
                {"type": "effect", "detail": {"kind": "tool", "toolName": "file_system.write_file",
                  "input": {"path": "/work/src/a.rs"}},
                 "state": {"display": {"success": true}, "output": {"content": []}}},
                {"type": "message", "role": "assistant", "outcome": {"type": "retry"},
                 "content": [{"type": "text", "text": "discarded draft"}]},
                {"type": "message", "role": "assistant", "outcome": {"type": "committed"},
                 "content": [{"type": "text", "text": "Done."}]},
                {"type": "message", "role": "user", "content": [{"type": "text", "text": "<hcom>"}]},
                {"type": "effect", "detail": {"kind": "tool", "toolName": "file_system.bash",
                  "input": {"command": "false"}},
                 "state": {"display": {"success": false}, "output": {"content": [
                    {"type": "text", "text": "returncode: 1"}]}}}
            ]),
        );
        let transcript = parse_vibe_session_with_usage(&meta, false).unwrap();
        let ex = &transcript.exchanges;
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0].position, 1);
        assert_eq!(ex[0].user, "run it");
        assert_eq!(ex[0].timestamp, "2026-11-24T08:16:06Z");
        assert_eq!(ex[0].action, "Let me run it.\n\nDone.");
        assert_eq!(ex[0].final_text, "Done.");
        assert_eq!(ex[0].tools.len(), 2);
        assert_eq!(ex[0].tools[0].name, "Bash");
        assert_eq!(ex[0].tools[0].command.as_deref(), Some("echo hi"));
        // hcom's appended delivery is not part of the tool's output.
        assert_eq!(ex[0].tools[0].output.as_deref(), Some("stdout: hi"));
        assert_eq!(ex[0].tools[1].name, "Write");
        assert_eq!(ex[0].files, vec!["src/a.rs".to_string()]);
        assert_eq!(ex[1].user, "<hcom>");
        assert!(ex[1].ended_on_error);
        assert_eq!(ex[1].errors.len(), 1);
    }

    #[test]
    fn missing_snapshot_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = tmp.path().join("meta.json");
        std::fs::write(&meta, "{}").unwrap();
        assert!(parse_vibe_session_with_usage(&meta, false).is_err());
    }
}
