//! Shared types and helpers used by all transcript parsers.

use std::path::Path;

use regex::Regex;
use serde_json::{Value, json};

/// An exchange in a transcript.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub position: usize,
    pub user: String,
    pub action: String,
    pub files: Vec<String>,
    pub timestamp: String,
    pub tools: Vec<ToolUse>,
    pub edits: Vec<Value>,
    pub errors: Vec<Value>,
    pub ended_on_error: bool,
}

/// A tool use within an exchange.
#[derive(Debug, Clone)]
pub struct ToolUse {
    pub name: String,
    pub is_error: bool,
    pub file: Option<String>,
    pub command: Option<String>,
    pub output: Option<String>,
}

/// Lazy-initialized error detection regex.
/// Uses `(?:^|\W)error:` to match "error:" not preceded by a word character
/// (lookbehinds not supported by the regex crate).
pub(crate) fn error_patterns() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(rejected|interrupted|traceback|failed|exception)\b|(?:^|\W)error:|command failed with exit code|Traceback \(most recent call last\)").unwrap()
    })
}

/// Check if a tool result indicates an error.
pub(crate) fn is_error_result(result: &Value) -> bool {
    if result
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }
    let content = result.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if content.is_empty() {
        return false;
    }
    let check = truncate_str(content, 500);
    error_patterns().is_match(check)
}

/// Check if Codex tool output indicates an error.
pub(crate) fn codex_is_error(output: &str) -> bool {
    if output.is_empty() {
        return false;
    }
    if output.starts_with("Exit code:") {
        let exit_line = output.lines().next().unwrap_or("");
        if !exit_line.contains("Exit code: 0") {
            return true;
        }
    }
    let check = truncate_str(output, 200);
    error_patterns().is_match(check)
}

/// Extract text from tool_result content that may be a string or array of text blocks.
pub(crate) fn extract_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for block in arr {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Extract edit info from toolUseResult and/or tool_use input.
pub(crate) fn extract_edit_info(
    tool_use_result: &Option<Value>,
    tool_input: &Value,
) -> Option<Value> {
    // Try toolUseResult first
    if let Some(result) = tool_use_result.as_ref().and_then(|v| v.as_object())
        && (result.contains_key("structuredPatch") || result.contains_key("oldString"))
    {
        let file = result
            .get("filePath")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let diff = if let Some(patch) = result.get("structuredPatch").and_then(|v| v.as_array()) {
            format_structured_patch(patch)
        } else if let (Some(old), Some(new)) = (
            result.get("oldString").and_then(|v| v.as_str()),
            result.get("newString").and_then(|v| v.as_str()),
        ) {
            let old_preview = truncate_str(old, 100);
            let new_preview = truncate_str(new, 100);
            let old_suffix = if old.len() > 100 { "..." } else { "" };
            let new_suffix = if new.len() > 100 { "..." } else { "" };
            format!("-{old_preview}{old_suffix}\n+{new_preview}{new_suffix}")
        } else {
            String::new()
        };
        return Some(json!({"file": file, "diff": diff}));
    }

    // Fallback: extract from tool_use input
    if let Some(obj) = tool_input.as_object()
        && (obj.contains_key("old_string") || obj.contains_key("new_string"))
    {
        let file = obj.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
        let old = obj.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
        let new = obj.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
        let old_preview = truncate_str(old, 100);
        let new_preview = truncate_str(new, 100);
        let old_suffix = if old.len() > 100 { "..." } else { "" };
        let new_suffix = if new.len() > 100 { "..." } else { "" };
        return Some(
            json!({"file": file, "diff": format!("-{old_preview}{old_suffix}\n+{new_preview}{new_suffix}")}),
        );
    }

    None
}

/// Format structuredPatch into readable diff.
pub(crate) fn format_structured_patch(patch: &[Value]) -> String {
    let mut lines = Vec::new();
    for hunk in patch {
        if let Some(obj) = hunk.as_object() {
            let old_start = obj.get("oldStart").and_then(|v| v.as_u64()).unwrap_or(0);
            let new_start = obj.get("newStart").and_then(|v| v.as_u64()).unwrap_or(0);
            lines.push(format!("@@ -{old_start} +{new_start} @@"));
            if let Some(hunk_lines) = obj.get("lines").and_then(|v| v.as_array()) {
                for (i, line) in hunk_lines.iter().enumerate() {
                    if i >= 20 {
                        lines.push(format!("  ... +{} more lines", hunk_lines.len() - 20));
                        break;
                    }
                    if let Some(s) = line.as_str() {
                        lines.push(s.to_string());
                    }
                }
            }
        }
    }
    lines.join("\n")
}

/// Truncate a string to at most `max` bytes at a valid UTF-8 char boundary.
pub(crate) fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

const TOOL_OUTPUT_CAPTURE_MAX: usize = 1000;

/// Keep transcript parsing memory-bounded while preserving enough tool output
/// for the detailed renderer's shorter preview.
pub(crate) fn capture_tool_output(output: &str) -> Option<String> {
    if output.trim().is_empty() {
        None
    } else {
        Some(truncate_str(output, TOOL_OUTPUT_CAPTURE_MAX).to_string())
    }
}

/// Normalize tool names across agents to canonical Claude names.
pub(crate) fn normalize_tool_name(name: &str) -> &str {
    match name {
        "run_shell_command" | "shell" | "shell_command" | "bash" => "Bash",
        "read_file" | "read" | "read_many_files" => "Read",
        "write_file" | "write" => "Write",
        "edit_file" | "edit" | "apply_patch" | "replace" => "Edit",
        "search_files" | "grep" | "grep_search" => "Grep",
        "list_files" | "list_directory" | "glob" => "Glob",
        "fetch" => "WebFetch",
        "skill" => "Skill",
        _ => name,
    }
}

pub(crate) fn summarize_tool_names(tools: &[ToolUse]) -> String {
    let mut names = Vec::new();
    for tool in tools {
        if !names.contains(&tool.name) {
            names.push(tool.name.clone());
        }
    }

    match names.len() {
        0 => "tools".to_string(),
        1..=3 => names.join(", "),
        _ => format!("{}, +{} more", names[..3].join(", "), names.len() - 3),
    }
}

pub(crate) fn finalize_action_text(
    action: &str,
    tools: &[ToolUse],
    errors: &[Value],
    ended_on_error: bool,
) -> String {
    let trimmed = action.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }

    let _ = errors;
    if ended_on_error {
        if tools.is_empty() {
            "(turn ended in error)".to_string()
        } else {
            format!(
                "(turn ended in error after using {})",
                summarize_tool_names(tools)
            )
        }
    } else if !tools.is_empty() {
        format!("(tool-only turn: {})", summarize_tool_names(tools))
    } else {
        "(no response)".to_string()
    }
}

pub(crate) fn is_codex_system_injected_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions")
        || trimmed.starts_with("# AGENTS.md")
}

pub(crate) fn extract_codex_event_message_text(payload: &Value) -> String {
    if let Some(text) = payload.get("message").and_then(|v| v.as_str()) {
        return text.trim().to_string();
    }
    extract_text_content(payload)
}

pub(crate) fn same_trimmed_text(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

pub(crate) fn is_no_response_action(action: &str) -> bool {
    action.trim() == "(no response)"
}

pub(crate) fn merge_exchange_metadata(dst: &mut Exchange, src: Exchange) {
    for file in src.files {
        if !dst.files.contains(&file) {
            dst.files.push(file);
        }
    }
    dst.files = dedup_sorted_capped(&dst.files, 5);

    for tool in src.tools {
        dst.tools.push(tool);
    }
    for edit in src.edits {
        dst.edits.push(edit);
    }
    for error in src.errors {
        dst.errors.push(error);
    }
    dst.ended_on_error = src.ended_on_error;
}

pub(crate) fn collapse_codex_duplicate_exchanges(exchanges: Vec<Exchange>) -> Vec<Exchange> {
    let mut collapsed: Vec<Exchange> = Vec::new();

    for ex in exchanges {
        if let Some(last) = collapsed.last_mut() {
            let same_user = same_trimmed_text(&last.user, &ex.user);
            let same_action = same_trimmed_text(&last.action, &ex.action);

            if same_user
                && is_no_response_action(&last.action)
                && !is_no_response_action(&ex.action)
            {
                last.action = ex.action.clone();
                last.timestamp = ex.timestamp.clone();
                merge_exchange_metadata(last, ex);
                continue;
            }

            if same_user && same_action && !is_no_response_action(&last.action) {
                merge_exchange_metadata(last, ex);
                continue;
            }
        }

        collapsed.push(ex);
    }

    for (idx, ex) in collapsed.iter_mut().enumerate() {
        ex.position = idx + 1;
    }

    collapsed
}

/// Deduplicate, sort, and cap a list of file names.
pub(crate) fn dedup_sorted_capped(files: &[String], cap: usize) -> Vec<String> {
    let mut seen = Vec::new();
    for f in files {
        if !seen.contains(f) {
            seen.push(f.clone());
        }
    }
    seen.sort();
    seen.truncate(cap);
    seen
}

/// Check if a message has actual user text (not just tool_result blocks).
pub(crate) fn has_user_text(msg: &Value) -> bool {
    let content = msg.get("content");
    if let Some(text) = content.and_then(|v| v.as_str()) {
        return !text.trim().is_empty();
    }
    if let Some(arr) = content.and_then(|v| v.as_array()) {
        return arr.iter().any(|block| {
            block.get("type").and_then(|v| v.as_str()) == Some("text")
                && block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
        });
    }
    false
}

/// Extract user text from a Gemini user message.
/// Prefers displayContent (user-visible text without hook context),
/// falls back to content (string or array of {text: ...} blocks).
pub(crate) fn extract_gemini_user_text(msg: &Value) -> String {
    // displayContent: the user-visible text (excludes hook_context injections)
    if let Some(arr) = msg.get("displayContent").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    // Fallback: content as string
    if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    // Fallback: content as array of text blocks
    if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        return parts.join("\n");
    }
    String::new()
}

pub(crate) fn extract_text_content(msg: &Value) -> String {
    if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
        return text.trim().to_string();
    }
    if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            // Skip tool_result blocks (they're not user text)
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                continue;
            }
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        return parts.join("\n");
    }
    // Fallback: look for text directly
    msg.get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Read file to string with lossy UTF-8 conversion (handles binary/corrupted files).
pub(crate) fn read_file_lossy(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Cannot read transcript: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ── Formatting (Exchange → String) ───────────────────────────────────────

/// Format exchanges for display.
pub fn format_exchanges(
    exchanges: &[Exchange],
    _instance: &str,
    full: bool,
    detailed: bool,
) -> String {
    let mut lines = Vec::new();

    for ex in exchanges {
        let user_text = if full || ex.user.len() <= 300 {
            ex.user.clone()
        } else {
            format!("{}...", truncate_str(&ex.user, 297))
        };

        let action_text = if full {
            ex.action.clone()
        } else {
            summarize_action(&ex.action)
        };

        lines.push(format!("[{}] USER: {}", ex.position, user_text));
        lines.push(format!("ASSISTANT: {}", action_text));

        if !ex.files.is_empty() {
            lines.push(format!("FILES: {}", ex.files.join(", ")));
        }

        if detailed {
            for tool in &ex.tools {
                let marker = if tool.is_error { "  ✗" } else { "  ├─" };
                let detail = tool
                    .file
                    .as_deref()
                    .or(tool.command.as_deref())
                    .unwrap_or("");
                lines.push(format!("{marker} {} {detail}", tool.name));
                if let Some(output) = &tool.output
                    && !output.trim().is_empty()
                {
                    let duplicated_error = tool.is_error
                        && ex.errors.iter().any(|error| {
                            error.get("tool").and_then(Value::as_str) == Some(tool.name.as_str())
                                && error
                                    .get("content")
                                    .or_else(|| error.get("error"))
                                    .and_then(Value::as_str)
                                    .is_some_and(|content| {
                                        let content = content.trim();
                                        let output = output.trim();
                                        !content.is_empty()
                                            && (content == output
                                                || (content.len() >= 300
                                                    && output.starts_with(content)))
                                    })
                        });
                    if duplicated_error {
                        continue;
                    }
                    lines.push(format!(
                        "  │  OUTPUT: {}",
                        single_line_ellipsized(output, 400)
                    ));
                }
            }

            for edit in &ex.edits {
                let file = edit
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let diff = edit.get("diff").and_then(Value::as_str).unwrap_or("");
                lines.push(format!(
                    "  Δ EDIT {file}: {}",
                    single_line_ellipsized(diff, 400)
                ));
            }

            for error in &ex.errors {
                let tool = error
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let content = error
                    .get("content")
                    .or_else(|| error.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                lines.push(format!(
                    "  ✗ ERROR {tool}: {}",
                    single_line_ellipsized(content, 400)
                ));
            }
        }

        lines.push(String::new()); // blank line between exchanges
    }

    // Trailing hint
    if !exchanges.is_empty() {
        match (full, detailed) {
            (false, false) => lines.push(
                "Note: Conversation text truncated. Use --full for full text; use --detailed for tool outputs, file edits, and errors."
                    .to_string(),
            ),
            (false, true) => lines.push(
                "Note: Conversation text truncated. Use --full for full text.".to_string(),
            ),
            (true, false) => lines.push(
                "Note: Tool outputs, file edits, and errors hidden. Use --detailed to show them."
                    .to_string(),
            ),
            (true, true) => {}
        }
    }

    lines.join("\n")
}

fn single_line_ellipsized(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return "(empty)".to_string();
    }
    if one_line.len() <= max {
        one_line
    } else {
        format!("{}…", truncate_str(&one_line, max.saturating_sub(1)))
    }
}

/// Summarize action text (first 3 lines, strip prefixes).
pub fn summarize_action(text: &str) -> String {
    if text.is_empty() {
        return "(no response)".to_string();
    }
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        return "(no response)".to_string();
    }
    // Strip common prefixes
    for prefix in &["I'll ", "I will ", "Let me ", "Sure, ", "Okay, ", "OK, "] {
        if lines[0].starts_with(prefix) {
            lines[0] = lines[0][prefix.len()..].to_string();
            break;
        }
    }
    let summary = lines.join(" ");
    if summary.len() > 200 {
        format!("{}...", truncate_str(&summary, 197))
    } else if text.lines().filter(|l| !l.trim().is_empty()).count() > 3 {
        format!("{summary} ...")
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange() -> Exchange {
        Exchange {
            position: 1,
            user: "question".to_string(),
            action: "answer".to_string(),
            files: vec!["a.rs".to_string()],
            timestamp: String::new(),
            tools: vec![ToolUse {
                name: "Bash".to_string(),
                is_error: true,
                file: None,
                command: Some("cargo test".to_string()),
                output: Some("line one\nline two".to_string()),
            }],
            edits: vec![json!({"file": "a.rs", "diff": "-old\n+new"})],
            errors: vec![json!({"tool": "Bash", "content": "exit 1"})],
            ended_on_error: true,
        }
    }

    #[test]
    fn detailed_renders_outputs_edits_errors_and_no_active_flag_hint() {
        let rendered = format_exchanges(&[exchange()], "agent", true, true);
        assert!(rendered.contains("OUTPUT: line one line two"));
        assert!(rendered.contains("Δ EDIT a.rs: -old +new"));
        assert!(rendered.contains("✗ ERROR Bash: exit 1"));
        assert!(!rendered.contains("Use --detailed"));
    }

    #[test]
    fn detailed_without_full_only_hints_about_conversation_text() {
        let rendered = format_exchanges(&[exchange()], "agent", false, true);
        assert!(rendered.contains("Use --full for full text"));
        assert!(!rendered.contains("Use --detailed"));
    }

    #[test]
    fn detailed_skips_empty_outputs_and_duplicate_error_output() {
        let mut ex = exchange();
        ex.tools[0].output = Some("exit 1".to_string());
        ex.tools.push(ToolUse {
            name: "Wait".to_string(),
            is_error: false,
            file: None,
            command: None,
            output: Some(String::new()),
        });
        let rendered = format_exchanges(&[ex], "agent", true, true);
        assert!(!rendered.contains("OUTPUT: (empty)"));
        assert!(!rendered.contains("OUTPUT: exit 1"));
        assert_eq!(rendered.matches("ERROR Bash: exit 1").count(), 1);
    }

    #[test]
    fn capture_tool_output_skips_empty_and_caps_large_results() {
        assert_eq!(capture_tool_output("  \n"), None);
        let captured = capture_tool_output(&"x".repeat(2_000)).unwrap();
        assert_eq!(captured.len(), TOOL_OUTPUT_CAPTURE_MAX);
    }
}
