//! Shared types and helpers used by all transcript parsers.

use std::path::Path;

use regex::Regex;
use serde_json::{Value, json};

/// Disjoint counters: input excludes cache; reasoning is a subset of output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: Option<u64>,
}
impl TokenUsage {
    pub fn is_zero(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_write == 0
            && self.reasoning.unwrap_or(0) == 0
    }
    pub fn saturating_sub(self, previous: Self) -> Self {
        Self {
            input: self.input.saturating_sub(previous.input),
            output: self.output.saturating_sub(previous.output),
            cache_read: self.cache_read.saturating_sub(previous.cache_read),
            cache_write: self.cache_write.saturating_sub(previous.cache_write),
            reasoning: self
                .reasoning
                .map(|v| v.saturating_sub(previous.reasoning.unwrap_or(0))),
        }
    }
}
impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cache_read = self.cache_read.saturating_add(rhs.cache_read);
        self.cache_write = self.cache_write.saturating_add(rhs.cache_write);
        if let Some(v) = rhs.reasoning {
            self.reasoning = Some(self.reasoning.unwrap_or(0).saturating_add(v));
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct SessionUsage {
    pub tokens: TokenUsage,
    pub context_tokens: Option<u64>,
    /// Fraction of the context window, when only a ratio is recorded.
    pub context_usage_ratio: Option<f64>,
    pub context_window: Option<u64>,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
}
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub exchanges: Vec<Exchange>,
    pub usage: Option<SessionUsage>,
}
impl Transcript {
    pub(crate) fn truncate(&mut self, last: usize) {
        if last > 0 && self.exchanges.len() > last {
            self.exchanges = self.exchanges.split_off(self.exchanges.len() - last);
        }
    }
}
/// Accumulates calls during parsing, including calls outside visible exchanges.
#[derive(Default)]
pub(crate) struct UsageCollector {
    pub session: Option<SessionUsage>,
    exchanges: std::collections::HashMap<usize, TokenUsage>,
}
impl UsageCollector {
    pub fn record(
        &mut self,
        tokens: TokenUsage,
        context: Option<u64>,
        cost: Option<f64>,
        model: Option<&str>,
        exchange: Option<usize>,
    ) -> &mut SessionUsage {
        if let Some(index) = exchange
            && !tokens.is_zero()
        {
            *self.exchanges.entry(index).or_default() += tokens;
        }
        let session = self.session.get_or_insert_with(SessionUsage::default);
        session.tokens += tokens;
        // A call that omits context keeps the last known value.
        if context.is_some() {
            session.context_tokens = context;
        }
        if let Some(cost) = cost {
            session.cost_usd = Some(session.cost_usd.unwrap_or(0.0) + cost);
        }
        if let Some(model) = model {
            session.model = Some(model.to_owned());
        }
        session
    }
    pub fn attach(self, mut exchanges: Vec<Exchange>) -> Transcript {
        for (index, tokens) in self.exchanges {
            if let Some(ex) = exchanges.get_mut(index) {
                ex.usage = Some(tokens);
            }
        }
        Transcript {
            exchanges,
            usage: self.session,
        }
    }
}
pub(crate) fn token_counter(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// An exchange in a transcript.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub usage: Option<TokenUsage>,
    pub position: usize,
    pub user: String,
    pub action: String,
    /// Assistant text after the turn's last tool call: the conclusion of a
    /// finished turn, the latest note of a running one. Summaries use it so
    /// they show results rather than an opening "Let me check…". Empty when
    /// the parser can't tell, in which case summaries fall back to `action`.
    pub final_text: String,
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
        "run_shell_command"
        | "shell"
        | "shell_command"
        | "bash"
        | "run_terminal_command"
        | "exec" => "Bash",
        "read_file" | "read" | "read_many_files" => "Read",
        "write_file" | "write" => "Write",
        "edit_file" | "edit" | "apply_patch" | "patch" | "replace" | "search_replace" => "Edit",
        "search_files" | "grep" | "grep_search" => "Grep",
        "list_files" | "list_directory" | "glob" => "Glob",
        "fetch" => "WebFetch",
        "skill" => "Skill",
        _ => name,
    }
}

/// `action[start..]` as an exchange's `final_text`; empty when the final text
/// is the whole action (start 0), so summaries fall back to `action` itself.
pub(crate) fn final_segment(action: &str, start: usize) -> String {
    if start == 0 {
        String::new()
    } else {
        action.get(start..).unwrap_or("").trim().to_string()
    }
}

/// Path of the file a tool call works on. The directory argument of a
/// search/list tool is not a file, so it is ignored.
pub(crate) fn tool_input_file<'a>(tool: &str, input: &'a Value) -> Option<&'a str> {
    let keys: &[&str] = if matches!(normalize_tool_name(tool), "Grep" | "Glob" | "LS") {
        &["file_path", "filePath", "notebook_path"]
    } else {
        &["file_path", "filePath", "notebook_path", "path", "file"]
    };
    keys.iter()
        .find_map(|key| input.get(*key)?.as_str())
        .filter(|path| !path.is_empty())
}

/// A file path as shown in FILES and tool details: relative to the session's
/// working directory when under it, `~/`-abbreviated otherwise.
pub(crate) fn display_path(path: &str, cwd: Option<&str>) -> String {
    let p = Path::new(path);
    if let Some(cwd) = cwd.filter(|c| !c.is_empty())
        && let Ok(rel) = p.strip_prefix(cwd)
        && !rel.as_os_str().is_empty()
    {
        return rel.display().to_string();
    }
    if let Some(home) = dirs::home_dir()
        && let Ok(rel) = p.strip_prefix(&home)
        && !rel.as_os_str().is_empty()
    {
        return format!("~/{}", rel.display());
    }
    path.to_string()
}

/// The argument that identifies a non-file tool call: shell command, search
/// pattern, URL, subagent task. Kept whole so search sees all of it; display
/// shortens it.
pub(crate) fn tool_call_detail(tool: &str, input: &Value) -> Option<String> {
    let key = match normalize_tool_name(tool) {
        "Bash" => "command",
        "Grep" | "Glob" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Agent" | "Task" => "description",
        _ => return None,
    };
    input.get(key)?.as_str().map(str::to_string)
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
    if let Some(usage) = src.usage {
        *dst.usage.get_or_insert_with(TokenUsage::default) += usage;
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
pub fn format_exchanges(exchanges: &[Exchange], full: bool, detailed: bool) -> String {
    format_exchanges_inner(exchanges, full, detailed, false)
}

pub fn format_transcript_exchanges(exchanges: &[Exchange], full: bool, detailed: bool) -> String {
    format_exchanges_inner(exchanges, full, detailed, true)
}

fn format_exchanges_inner(
    exchanges: &[Exchange],
    full: bool,
    detailed: bool,
    show_usage: bool,
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
            summarize_exchange(ex)
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
                    .map(|d| single_line_ellipsized(d, 80))
                    .unwrap_or_default();
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
            if show_usage && let Some(usage) = ex.usage {
                let reasoning = usage.reasoning.map_or_else(String::new, |tokens| {
                    format!(" ({} reasoning)", format_token_count(tokens))
                });
                lines.push(format!(
                    "  tokens: {} in · {} out{} · {} cache read · {} cache write",
                    format_token_count(usage.input),
                    format_token_count(usage.output),
                    reasoning,
                    format_token_count(usage.cache_read),
                    format_token_count(usage.cache_write),
                ));
            }
        }

        lines.push(String::new()); // blank line between exchanges
    }

    lines.join("\n")
}

/// `hcom transcript` footer pointing at the flags that reveal what was elided.
pub fn flag_hint(full: bool, detailed: bool) -> Option<&'static str> {
    match (full, detailed) {
        (false, false) => Some(
            "Note: Conversation text truncated. Use --full for full text; use --detailed for tool outputs, file edits, and errors.",
        ),
        (false, true) => Some("Note: Conversation text truncated. Use --full for full text."),
        (true, false) => {
            Some("Note: Tool outputs, file edits, and errors hidden. Use --detailed to show them.")
        }
        (true, true) => None,
    }
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

/// One-line summary of an exchange's response, from its final text when known.
pub fn summarize_exchange(ex: &Exchange) -> String {
    if ex.final_text.trim().is_empty() {
        summarize_action(&ex.action)
    } else {
        summarize_action(&ex.final_text)
    }
}

/// Summarize action text (first 3 lines).
pub fn summarize_action(text: &str) -> String {
    if text.is_empty() {
        return "(no response)".to_string();
    }
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        return "(no response)".to_string();
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
            usage: None,
            position: 1,
            user: "question".to_string(),
            action: "answer".to_string(),
            final_text: String::new(),
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
    fn detailed_renders_outputs_edits_errors() {
        let rendered = format_exchanges(&[exchange()], true, true);
        assert!(rendered.contains("OUTPUT: line one line two"));
        assert!(rendered.contains("Δ EDIT a.rs: -old +new"));
        assert!(rendered.contains("✗ ERROR Bash: exit 1"));
        assert!(!rendered.contains("Note:"));
    }

    #[test]
    fn detailed_usage_keeps_reasoning_inside_output_and_preserves_missing_usage() {
        let mut ex = exchange();
        ex.usage = Some(TokenUsage {
            input: 41_000,
            output: 2_100,
            cache_read: 380_000,
            cache_write: 500,
            reasoning: Some(1_000),
        });
        let rendered = format_transcript_exchanges(std::slice::from_ref(&ex), true, true);
        assert!(rendered.contains(
            "  tokens: 41k in · 2.1k out (1k reasoning) · 380k cache read · 500 cache write"
        ));
        assert!(
            !format_transcript_exchanges(std::slice::from_ref(&ex), true, false)
                .contains("tokens:")
        );
        assert!(!format_exchanges(std::slice::from_ref(&ex), true, true).contains("tokens:"));
        ex.usage = None;
        assert!(!format_transcript_exchanges(&[ex], true, true).contains("tokens:"));
    }

    #[test]
    fn flag_hint_names_only_flags_not_already_active() {
        assert_eq!(flag_hint(true, true), None);
        let detailed_only = flag_hint(false, true).unwrap();
        assert!(detailed_only.contains("Use --full"));
        assert!(!detailed_only.contains("--detailed"));
        assert!(!flag_hint(true, false).unwrap().contains("--full"));
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
        let rendered = format_exchanges(&[ex], true, true);
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

    #[test]
    fn tool_input_file_ignores_search_directories() {
        let grep = json!({"pattern": "fn main", "path": "/repo/src"});
        assert_eq!(tool_input_file("Grep", &grep), None);
        assert_eq!(tool_input_file("grep_search", &grep), None);
        let read = json!({"path": "/repo/src/main.rs"});
        assert_eq!(
            tool_input_file("read_file", &read),
            Some("/repo/src/main.rs")
        );
        let notebook = json!({"notebook_path": "/repo/n.ipynb"});
        assert_eq!(
            tool_input_file("NotebookEdit", &notebook),
            Some("/repo/n.ipynb")
        );
    }

    #[test]
    fn display_path_is_relative_to_cwd_then_home() {
        assert_eq!(display_path("/repo/src/a.rs", Some("/repo")), "src/a.rs");
        assert_eq!(display_path("/repo", Some("/repo")), "/repo");
        assert_eq!(
            display_path("/elsewhere/a.rs", Some("/repo")),
            "/elsewhere/a.rs"
        );
        assert_eq!(display_path("rel/a.rs", None), "rel/a.rs");
        if let Some(home) = dirs::home_dir() {
            let path = home.join("x/a.rs");
            assert_eq!(
                display_path(path.to_str().unwrap(), Some("/repo")),
                "~/x/a.rs"
            );
        }
    }

    #[test]
    fn tool_call_detail_names_the_call() {
        assert_eq!(
            tool_call_detail("Grep", &json!({"pattern": "TODO", "path": "."})).as_deref(),
            Some("TODO")
        );
        assert_eq!(
            tool_call_detail("Agent", &json!({"description": "review branch"})).as_deref(),
            Some("review branch")
        );
        assert_eq!(tool_call_detail("Read", &json!({"file_path": "/a"})), None);
    }

    #[test]
    fn summary_prefers_final_text() {
        let mut ex = exchange();
        ex.action = "I'll check the tests first.\nAll 12 tests pass.".to_string();
        assert_eq!(
            summarize_exchange(&ex),
            "I'll check the tests first. All 12 tests pass."
        );
        ex.final_text = "All 12 tests pass.".to_string();
        assert_eq!(summarize_exchange(&ex), "All 12 tests pass.");
        assert_eq!(final_segment("abc def", 0), "");
        assert_eq!(final_segment("abc\n def", 4), "def");
    }
}

/// Compact token count for shared usage summaries.
fn format_token_count(tokens: u64) -> String {
    let (value, suffix) = if tokens >= 1_000_000 {
        (tokens as f64 / 1_000_000.0, "M")
    } else if tokens >= 1_000 {
        (tokens as f64 / 1_000.0, "k")
    } else {
        return tokens.to_string();
    };
    let number = format!("{value:.1}");
    format!("{}{suffix}", number.strip_suffix(".0").unwrap_or(&number))
}

/// Latest recorded context, with a window only when the tool supplied one.
pub fn format_context(usage: &SessionUsage) -> Option<String> {
    let Some(tokens) = usage.context_tokens else {
        return usage
            .context_usage_ratio
            .filter(|ratio| ratio.is_finite() && (0.0..=1.0).contains(ratio))
            .map(|ratio| format!("{:.0}%", ratio * 100.0));
    };
    let context = format_token_count(tokens);
    Some(match usage.context_window {
        Some(window) => format!("{context}/{}", format_token_count(window)),
        None => context,
    })
}

/// Whole-session usage; costs and context are shown only when recorded.
///
/// Leads with tokens the model newly processed (input, cache writes, output);
/// cache reads usually dwarf them, so they are listed separately.
pub fn format_usage(usage: &SessionUsage) -> String {
    let tokens = usage.tokens;
    let processed = tokens
        .input
        .saturating_add(tokens.cache_write)
        .saturating_add(tokens.output);
    let mut parts = Vec::new();
    // Some tools (Qoder) record all-zero counters next to a context ratio.
    if processed > 0 || tokens.cache_read > 0 {
        let mut part = format!("{} tok", format_token_count(processed));
        if tokens.cache_read > 0 {
            part.push_str(&format!(
                " + {} cached",
                format_token_count(tokens.cache_read)
            ));
        }
        parts.push(part);
    }
    if let Some(context) = format_context(usage) {
        let percent = match (usage.context_tokens, usage.context_window) {
            (Some(context), Some(window)) if window > 0 => {
                format!(" ({:.0}%)", context as f64 / window as f64 * 100.0)
            }
            _ => String::new(),
        };
        parts.push(format!("ctx {context}{percent}"));
    }
    // Zero is a free model or a subscription, not a figure worth showing.
    if let Some(cost) = usage.cost_usd.filter(|cost| *cost > 0.0) {
        parts.push(format!("${cost:.2}"));
    }
    if parts.is_empty() {
        parts.push("0 tok".to_string());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod usage_format_tests {
    use super::*;
    #[test]
    fn formats_recorded_context_and_disjoint_totals() {
        let mut usage = SessionUsage {
            tokens: TokenUsage {
                input: 100_000,
                output: 10_000,
                cache_read: 100_000,
                reasoning: Some(5_000),
                ..Default::default()
            },
            context_tokens: Some(100_000),
            context_window: Some(110_000),
            cost_usd: Some(0.42),
            ..Default::default()
        };
        assert_eq!(format_context(&usage).as_deref(), Some("100k/110k"));
        assert_eq!(
            format_usage(&usage),
            "110k tok + 100k cached · ctx 100k/110k (91%) · $0.42"
        );
        usage.context_window = None;
        assert_eq!(format_context(&usage).as_deref(), Some("100k"));
        usage.context_tokens = None;
        usage.cost_usd = None;
        assert_eq!(format_context(&usage), None);
        assert_eq!(format_usage(&usage), "110k tok + 100k cached");
        usage.tokens.cache_read = 0;
        assert_eq!(format_usage(&usage), "110k tok");
        usage.tokens = TokenUsage::default();
        assert_eq!(format_usage(&usage), "0 tok");
        usage.cost_usd = Some(0.0);
        assert_eq!(format_usage(&usage), "0 tok", "zero cost is not shown");
        usage.cost_usd = None;
        usage.context_usage_ratio = Some(0.33);
        assert_eq!(format_usage(&usage), "ctx 33%");
    }
    #[test]
    fn zero_window_does_not_divide_by_zero() {
        let usage = SessionUsage {
            context_tokens: Some(0),
            context_window: Some(0),
            ..Default::default()
        };
        assert_eq!(format_usage(&usage), "ctx 0/0");
    }
}
