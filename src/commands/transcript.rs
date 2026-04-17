//! `hcom transcript` command — view and search agent conversation transcripts.
//!
//!
//! Supports:
//! - View transcript: `hcom transcript @instance [N | N-M] [--full] [--detailed] [--json] [--last N]`
//! - Timeline: `hcom transcript timeline [--last N] [--full] [--json]`
//! - Search: `hcom transcript search "pattern" [--live] [--all] [--limit N] [--agent TYPE]`

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use regex::Regex;

use crate::db::HcomDb;
use crate::log::log_warn;
use crate::shared::CommandContext;

/// Parsed arguments for `hcom transcript`.
#[derive(clap::Parser, Debug)]
#[command(name = "transcript", about = "View and search transcripts")]
pub struct TranscriptArgs {
    /// Subcommand (search, timeline) or view mode
    #[command(subcommand)]
    pub subcmd: Option<TranscriptSubcmd>,

    /// Target instance name (with or without @)
    pub name: Option<String>,
    /// Exchange range (e.g., "5" or "5-10")
    pub range_positional: Option<String>,

    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Full output (no streamlining)
    #[arg(long)]
    pub full: bool,
    /// Detailed output (include tool details)
    #[arg(long)]
    pub detailed: bool,
    /// Last N exchanges
    #[arg(long)]
    pub last: Option<usize>,
    /// Exchange range (flag form)
    #[arg(long = "range")]
    pub range_flag: Option<String>,
}

#[derive(clap::Subcommand, Debug)]
pub enum TranscriptSubcmd {
    /// Search transcripts for a pattern
    Search(TranscriptSearchArgs),
    /// Show timeline of all agents' recent activity
    Timeline(TranscriptTimelineArgs),
}

/// Args for `hcom transcript search`.
#[derive(clap::Args, Debug)]
pub struct TranscriptSearchArgs {
    /// Search pattern (regex)
    pub pattern: String,
    /// Live-watch mode
    #[arg(long)]
    pub live: bool,
    /// Search all transcripts on disk (not just tracked instances)
    #[arg(long)]
    pub all: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Exclude own transcript from search results
    #[arg(long)]
    pub exclude_self: bool,
    /// Max results (default: 20)
    #[arg(long, default_value = "20")]
    pub limit: usize,
    /// Filter by agent type (claude, gemini, codex, opencode)
    #[arg(long)]
    pub agent: Option<String>,
}

/// Args for `hcom transcript timeline`.
#[derive(clap::Args, Debug)]
pub struct TranscriptTimelineArgs {
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Full output
    #[arg(long)]
    pub full: bool,
    /// Detailed output
    #[arg(long)]
    pub detailed: bool,
    /// Last N exchanges per agent
    #[arg(long)]
    pub last: Option<usize>,
}

/// Lazy-initialized error detection regex.
/// Uses `(?:^|\W)error:` to match "error:" not preceded by a word character
/// (lookbehinds not supported by the regex crate).
fn error_patterns() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(rejected|interrupted|traceback|failed|exception)\b|(?:^|\W)error:|command failed with exit code|Traceback \(most recent call last\)").unwrap()
    })
}

/// Check if a tool result indicates an error.
fn is_error_result(result: &Value) -> bool {
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
fn codex_is_error(output: &str) -> bool {
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
///
fn extract_content_text(content: Option<&Value>) -> String {
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
fn extract_edit_info(tool_use_result: &Option<Value>, tool_input: &Value) -> Option<Value> {
    // Try toolUseResult first
    if let Some(result) = tool_use_result.as_ref().and_then(|v| v.as_object()) {
        if result.contains_key("structuredPatch") || result.contains_key("oldString") {
            let file = result
                .get("filePath")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let diff = if let Some(patch) = result.get("structuredPatch").and_then(|v| v.as_array())
            {
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
    }

    // Fallback: extract from tool_use input
    if let Some(obj) = tool_input.as_object() {
        if obj.contains_key("old_string") || obj.contains_key("new_string") {
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
    }

    None
}

/// Format structuredPatch into readable diff.
fn format_structured_patch(patch: &[Value]) -> String {
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
fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── Tool Aliases ─────────────────────────────────────────────────────────

/// Normalize tool names across agents to canonical Claude names.
fn normalize_tool_name(name: &str) -> &str {
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

// ── Transcript Path Discovery ────────────────────────────────────────────

/// Get Claude config directory.
pub(crate) fn claude_config_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"))
}

/// Detect agent type from transcript path.
pub(crate) fn detect_agent_type(path: &str) -> &str {
    if path.contains(".claude") || path.contains("/projects/") {
        "claude"
    } else if path.contains(".gemini") {
        "gemini"
    } else if path.contains(".codex") || path.contains("codex") {
        "codex"
    } else if path.contains("opencode") {
        "opencode"
    } else {
        "unknown"
    }
}

fn transcript_search_key(path: &str, session_id: Option<&str>) -> String {
    format!("{path}\u{0}{}", session_id.unwrap_or(""))
}

fn transcript_agent_matches(filter: Option<&str>, agent: &str) -> bool {
    filter.is_none_or(|f| agent.contains(f) || f.contains(agent))
}

fn get_opencode_db_path() -> Option<PathBuf> {
    let xdg_data = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/.local/share")
    });
    let db_path = PathBuf::from(xdg_data).join("opencode").join("opencode.db");
    db_path.exists().then_some(db_path)
}

#[derive(Debug, Clone)]
struct TranscriptSearchMatch {
    path: String,
    agent: String,
    line: usize,
    text: String,
    matches: usize,
    session_id: Option<String>,
    label: Option<String>,
}

fn search_opencode_sessions(
    db_path: &Path,
    pattern: &str,
    limit: usize,
) -> Result<Vec<TranscriptSearchMatch>, String> {
    let re = Regex::new(pattern).map_err(|e| format!("Invalid regex: {e}"))?;
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Cannot open OpenCode DB: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.title, p.data
             FROM session s
             JOIN part p ON p.session_id = s.id
             WHERE json_extract(p.data, '$.type') = 'text'
             ORDER BY p.time_created ASC",
        )
        .map_err(|e| format!("Query error: {e}"))?;

    let mut by_session: std::collections::HashMap<String, TranscriptSearchMatch> =
        std::collections::HashMap::new();
    let mut order = Vec::new();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| format!("Query error: {e}"))?;

    for row in rows {
        let (session_id, title, data_str) = match row {
            Ok(row) => row,
            Err(_) => continue,
        };
        let data = match serde_json::from_str::<Value>(&data_str) {
            Ok(data) => data,
            Err(_) => continue,
        };
        if data
            .get("synthetic")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }

        let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if text.is_empty() || !re.is_match(text) {
            continue;
        }

        let entry = by_session.entry(session_id.clone()).or_insert_with(|| {
            order.push(session_id.clone());
            TranscriptSearchMatch {
                path: db_path.to_string_lossy().to_string(),
                agent: "opencode".to_string(),
                line: 0,
                text: truncate_str(&text.replace('\n', " "), 100).to_string(),
                matches: 0,
                session_id: Some(session_id.clone()),
                label: Some(title.clone()),
            }
        });
        entry.matches += 1;
    }

    Ok(order
        .into_iter()
        .filter_map(|session_id| by_session.remove(&session_id))
        .take(limit)
        .collect())
}

/// Get transcript path for an instance from DB.
fn get_transcript_path(db: &HcomDb, name: &str) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT transcript_path FROM instances WHERE name = ?",
            rusqlite::params![name],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .filter(|p| !p.is_empty())
}

/// Build an appropriate error message when transcript resolution fails.
/// Uses resolve_display_name_or_stopped (which already handles prefix matching)
/// to check if the instance exists without a transcript.
fn no_transcript_error(db: &HcomDb, name: &str) -> String {
    if let Some(resolved) = crate::instances::resolve_display_name_or_stopped(db, name) {
        format!(
            "Agent '{}' has no transcript yet — no messages have been exchanged",
            resolved
        )
    } else {
        format!("Agent '{name}' not found")
    }
}

// ── Transcript Parsing (simplified) ──────────────────────────────────────

/// An exchange in a transcript.
#[derive(Debug, Clone)]
struct Exchange {
    position: usize,
    user: String,
    action: String,
    files: Vec<String>,
    timestamp: String,
    tools: Vec<ToolUse>,
    edits: Vec<Value>,
    errors: Vec<Value>,
    ended_on_error: bool,
}

/// A tool use within an exchange.
#[derive(Debug, Clone)]
struct ToolUse {
    name: String,
    is_error: bool,
    file: Option<String>,
    command: Option<String>,
}

fn summarize_tool_names(tools: &[ToolUse]) -> String {
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

fn finalize_action_text(
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

fn is_codex_system_injected_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions")
        || trimmed.starts_with("# AGENTS.md")
}

fn extract_codex_event_message_text(payload: &Value) -> String {
    if let Some(text) = payload.get("message").and_then(|v| v.as_str()) {
        return text.trim().to_string();
    }
    extract_text_content(payload)
}

fn same_trimmed_text(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

fn is_no_response_action(action: &str) -> bool {
    action.trim() == "(no response)"
}

fn merge_exchange_metadata(dst: &mut Exchange, src: Exchange) {
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

fn collapse_codex_duplicate_exchanges(exchanges: Vec<Exchange>) -> Vec<Exchange> {
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

/// Parse Claude JSONL transcript.
fn parse_claude_jsonl(path: &Path, last: usize, detailed: bool) -> Result<Vec<Exchange>, String> {
    let content = read_file_lossy(path)?;

    // First pass: parse all entries and build tool_use index for detailed mode
    struct ParsedEntry {
        entry_type: String,
        ts: String,
        data: Value,
    }

    let mut entries = Vec::new();
    // Map (session_id, tool_use_id) -> {name, input} for matching tool_results to tool_uses
    let mut tool_use_index: std::collections::HashMap<(String, String), (String, Value)> =
        std::collections::HashMap::new();

    for line in content.lines() {
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Skip meta/system entries
        if entry
            .get("isMeta")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || entry
                .get("isCompactSummary")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            || entry
                .get("isSidechain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            continue;
        }

        let entry_type = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ts = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Skip system-level entry types
        if matches!(
            entry_type.as_str(),
            "summary"
                | "system"
                | "result"
                | "progress"
                | "file-history-snapshot"
                | "saved_hook_context"
        ) {
            continue;
        }

        // Build tool_use index from assistant entries
        if entry_type == "assistant" {
            let session_id = entry
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(arr) = entry
                .get("message")
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_array())
            {
                for block in arr {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        let tool_id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = block.get("input").cloned().unwrap_or(json!({}));
                        tool_use_index.insert((session_id.clone(), tool_id), (name, input));
                    }
                }
            }
        }

        entries.push(ParsedEntry {
            entry_type,
            ts,
            data: entry,
        });
    }

    // Second pass: build exchanges
    let mut exchanges = Vec::new();
    let mut current_user = String::new();
    let mut current_action = String::new();
    let mut current_tools: Vec<ToolUse> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();
    let mut current_ts = String::new();
    let mut current_edits: Vec<Value> = Vec::new();
    let mut current_errors: Vec<Value> = Vec::new();
    let mut current_last_was_error = false;
    let mut position = 0;

    for pe in &entries {
        match pe.entry_type.as_str() {
            "user" => {
                // Check if this user entry has actual user text (not just tool_result blocks)
                let has_text = has_user_text(&pe.data.get("message").cloned().unwrap_or(json!({})));
                if !has_text {
                    // tool_result-only user entry — process for error detection in detailed mode
                    if detailed {
                        let session_id = pe
                            .data
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let tool_use_result = pe.data.get("toolUseResult").cloned();
                        if let Some(arr) = pe
                            .data
                            .get("message")
                            .and_then(|v| v.get("content"))
                            .and_then(|v| v.as_array())
                        {
                            for block in arr {
                                if block.get("type").and_then(|v| v.as_str()) != Some("tool_result")
                                {
                                    continue;
                                }
                                let tool_use_id = block
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let (tool_name, tool_input) = tool_use_index
                                    .get(&(session_id.clone(), tool_use_id.clone()))
                                    .map(|(n, i)| (n.clone(), i.clone()))
                                    .unwrap_or_else(|| ("unknown".to_string(), json!({})));

                                let is_err = is_error_result(block);
                                let normalized = normalize_tool_name(&tool_name);

                                let file = if normalized == "Edit" {
                                    tool_use_result
                                        .as_ref()
                                        .and_then(|r| r.get("filePath").and_then(|v| v.as_str()))
                                        .or_else(|| {
                                            tool_input.get("file_path").and_then(|v| v.as_str())
                                        })
                                        .map(|s| {
                                            Path::new(s)
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .unwrap_or(s)
                                                .to_string()
                                        })
                                } else {
                                    None
                                };

                                let command = if normalized == "Bash" {
                                    tool_input.get("command").and_then(|v| v.as_str()).map(|s| {
                                        if s.len() > 80 {
                                            format!("{}...", truncate_str(s, 77))
                                        } else {
                                            s.to_string()
                                        }
                                    })
                                } else {
                                    None
                                };

                                current_tools.push(ToolUse {
                                    name: normalized.to_string(),
                                    is_error: is_err,
                                    file,
                                    command,
                                });

                                // Extract edit info for Edit tools
                                if normalized == "Edit" {
                                    if let Some(edit) =
                                        extract_edit_info(&tool_use_result, &tool_input)
                                    {
                                        current_edits.push(edit);
                                    }
                                }

                                if is_err {
                                    let raw_content = extract_content_text(block.get("content"));
                                    let truncated = truncate_str(&raw_content, 300);
                                    current_errors.push(json!({
                                        "tool": normalized,
                                        "content": truncated,
                                    }));
                                    current_last_was_error = true;
                                } else {
                                    current_last_was_error = false;
                                }
                            }
                        }
                    }
                    continue;
                }

                // Save previous exchange
                if !current_user.is_empty() || !current_action.is_empty() {
                    position += 1;
                    exchanges.push(Exchange {
                        position,
                        user: current_user.clone(),
                        action: finalize_action_text(
                            &current_action,
                            &current_tools,
                            &current_errors,
                            current_last_was_error,
                        ),
                        files: dedup_sorted_capped(&current_files, 5),
                        timestamp: current_ts.clone(),
                        tools: std::mem::take(&mut current_tools),
                        edits: std::mem::take(&mut current_edits),
                        errors: std::mem::take(&mut current_errors),
                        ended_on_error: current_last_was_error,
                    });
                }

                // Extract user text
                current_user =
                    extract_text_content(&pe.data.get("message").cloned().unwrap_or(json!({})));
                current_action = String::new();
                current_tools = Vec::new();
                current_files = Vec::new();
                current_edits = Vec::new();
                current_errors = Vec::new();
                current_last_was_error = false;
                current_ts = pe.ts.clone();
            }
            "assistant" => {
                let msg = pe.data.get("message").cloned().unwrap_or(json!({}));

                // Extract text and tool_use blocks
                if let Some(content) = msg.get("content") {
                    if let Some(arr) = content.as_array() {
                        for block in arr {
                            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                    if !current_action.is_empty() {
                                        current_action.push('\n');
                                    }
                                    current_action.push_str(text);
                                }
                            } else if block.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                            {
                                let tool_name =
                                    block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let input = block.get("input").cloned().unwrap_or(json!({}));

                                // Extract file from tool input (including notebook_path)
                                let file = input
                                    .get("file_path")
                                    .or_else(|| input.get("path"))
                                    .or_else(|| input.get("filePath"))
                                    .or_else(|| input.get("notebook_path"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| {
                                        Path::new(s)
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .unwrap_or(s)
                                            .to_string()
                                    });

                                if let Some(ref f) = file {
                                    if !current_files.contains(f) {
                                        current_files.push(f.clone());
                                    }
                                }

                                // Don't push tool to current_tools here in detailed mode —
                                // tools come from tool_result processing for accurate is_error.
                                // In non-detailed mode, push with is_error=false.
                                if !detailed {
                                    let command = if normalize_tool_name(tool_name) == "Bash" {
                                        input.get("command").and_then(|v| v.as_str()).map(|s| {
                                            if s.len() > 80 {
                                                format!("{}...", truncate_str(s, 77))
                                            } else {
                                                s.to_string()
                                            }
                                        })
                                    } else {
                                        None
                                    };

                                    current_tools.push(ToolUse {
                                        name: normalize_tool_name(tool_name).to_string(),
                                        is_error: false,
                                        file,
                                        command,
                                    });
                                }
                            }
                        }
                    } else if let Some(text) = content.as_str() {
                        current_action = text.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    // Save last exchange
    if !current_user.is_empty() || !current_action.is_empty() {
        position += 1;
        exchanges.push(Exchange {
            position,
            user: current_user,
            action: finalize_action_text(
                &current_action,
                &current_tools,
                &current_errors,
                current_last_was_error,
            ),
            files: dedup_sorted_capped(&current_files, 5),
            timestamp: current_ts,
            tools: current_tools,
            edits: current_edits,
            errors: current_errors,
            ended_on_error: current_last_was_error,
        });
    }

    // Apply last N
    exchanges = collapse_codex_duplicate_exchanges(exchanges);

    if exchanges.len() > last {
        let start = exchanges.len() - last;
        exchanges = exchanges[start..].to_vec();
    }

    Ok(exchanges)
}

/// Deduplicate, sort, and cap a list of file names.
fn dedup_sorted_capped(files: &[String], cap: usize) -> Vec<String> {
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

/// Parse Gemini JSON transcript.
fn parse_gemini_json(path: &Path, last: usize) -> Result<Vec<Exchange>, String> {
    let content = read_file_lossy(path)?;

    let data: Value = serde_json::from_str(&content).map_err(|e| format!("Invalid JSON: {e}"))?;

    let messages = data
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or("No messages array")?;

    let mut exchanges = Vec::new();
    let mut current_user = String::new();
    let mut current_action = String::new();
    let mut current_ts = String::new();
    let mut position = 0;

    let mut current_tools: Vec<ToolUse> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();

    for msg in messages {
        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = msg
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if msg_type == "user" {
            if !current_user.is_empty() || !current_action.is_empty() {
                position += 1;
                exchanges.push(Exchange {
                    position,
                    user: current_user.clone(),
                    action: finalize_action_text(&current_action, &current_tools, &[], false),
                    files: std::mem::take(&mut current_files),
                    timestamp: current_ts.clone(),
                    tools: std::mem::take(&mut current_tools),
                    edits: Vec::new(),
                    errors: Vec::new(),
                    ended_on_error: false,
                });
            }
            // Gemini user content can be a string or array of {text: ...} blocks.
            // Use displayContent if available (user-visible text without hook context).
            current_user = extract_gemini_user_text(msg);
            current_action = String::new();
            current_tools = Vec::new();
            current_files = Vec::new();
            current_ts = ts;
        } else if msg_type == "gemini" || msg_type == "model" {
            if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                if !current_action.is_empty() {
                    current_action.push('\n');
                }
                current_action.push_str(text);
            }

            // Extract tool calls
            if let Some(tool_calls) = msg.get("toolCalls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    let raw_name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let tool_name = normalize_tool_name(raw_name);
                    let args = tc.get("args").cloned().unwrap_or(json!({}));
                    let is_err = tc
                        .get("status")
                        .and_then(|v| v.as_str())
                        .is_some_and(|status| {
                            !matches!(
                                status.to_ascii_lowercase().as_str(),
                                "ok" | "success" | "completed"
                            )
                        });

                    // Extract file paths from tool args
                    if let Some(obj) = args.as_object() {
                        for field in &["file", "path", "file_path", "directory"] {
                            if let Some(val) = obj.get(*field).and_then(|v| v.as_str()) {
                                if !val.is_empty() {
                                    let fname = Path::new(val)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(val)
                                        .to_string();
                                    if !current_files.contains(&fname) {
                                        current_files.push(fname.clone());
                                    }
                                }
                            }
                        }
                    }

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
                            .or(o.get("file"))
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
                    });
                }
            }
        }
    }

    // Last exchange
    if !current_user.is_empty() || !current_action.is_empty() {
        position += 1;
        exchanges.push(Exchange {
            position,
            user: current_user,
            action: finalize_action_text(&current_action, &current_tools, &[], false),
            files: current_files,
            timestamp: current_ts,
            tools: current_tools,
            edits: Vec::new(),
            errors: Vec::new(),
            ended_on_error: false,
        });
    }

    if exchanges.len() > last {
        let start = exchanges.len() - last;
        exchanges = exchanges[start..].to_vec();
    }

    Ok(exchanges)
}

/// Parse Codex JSONL transcript.
/// Handles both response_item (older) and event_msg (newer) formats.
fn parse_codex_jsonl(path: &Path, last: usize, detailed: bool) -> Result<Vec<Exchange>, String> {
    let content = read_file_lossy(path)?;

    // First pass: build call_id → output map for error detection
    let mut call_outputs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut parsed_lines: Vec<Value> = Vec::new();

    for line in content.lines() {
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let payload = entry.get("payload").cloned().unwrap_or(entry.clone());
        let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if payload_type == "function_call_output" {
            let call_id = payload
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output = payload
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
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
                            if let Some(val) = obj.get(*field).and_then(|v| v.as_str()) {
                                if !val.is_empty() {
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

/// Extract text content from a message (handles string or content blocks).
/// Check if a message has actual user text (not just tool_result blocks).
fn has_user_text(msg: &Value) -> bool {
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
fn extract_gemini_user_text(msg: &Value) -> String {
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

fn extract_text_content(msg: &Value) -> String {
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

/// Parse OpenCode SQLite transcript database.
///
/// OpenCode stores conversations in `opencode.db` with `message` and `part` tables.
/// Messages have role in their JSON `data` column; parts contain text, tool calls, etc.
fn parse_opencode_sqlite(
    db_path: &Path,
    session_id: &str,
    last: usize,
) -> Result<Vec<Exchange>, String> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Cannot open OpenCode DB: {e}"))?;

    // Fetch messages for this session (include time_created for timestamp)
    let mut stmt = conn.prepare(
        "SELECT id, data, time_created FROM message WHERE session_id = ? ORDER BY time_created ASC"
    ).map_err(|e| format!("Query error: {e}"))?;

    struct MsgRow {
        id: String,
        _data: Value,
        role: String,
        time_created: i64,
    }

    let messages: Vec<MsgRow> = stmt
        .query_map(rusqlite::params![session_id], |row| {
            let id: String = row.get(0)?;
            let data_str: String = row.get(1)?;
            let time_created: i64 = row.get::<_, i64>(2).unwrap_or(0);
            Ok((id, data_str, time_created))
        })
        .map_err(|e| format!("Query error: {e}"))?
        .filter_map(|r| r.ok())
        .filter_map(
            |(id, data_str, time_created)| match serde_json::from_str::<Value>(&data_str) {
                Ok(data) => {
                    let role = data
                        .get("role")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    Some(MsgRow {
                        id,
                        _data: data,
                        role,
                        time_created,
                    })
                }
                Err(e) => {
                    log_warn(
                        "transcript",
                        "opencode_parse",
                        &format!("skipping message {id}: invalid JSON in data column: {e}"),
                    );
                    None
                }
            },
        )
        .collect();

    if messages.is_empty() {
        return Ok(Vec::new());
    }

    // Prefetch parts keyed by message_id.
    // Query per message_id to avoid dependency on part.session_id column.
    let mut parts_by_msg: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();
    for msg in &messages {
        if let Ok(mut parts_stmt) =
            conn.prepare("SELECT data FROM part WHERE message_id = ? ORDER BY id ASC")
        {
            if let Ok(rows) =
                parts_stmt.query_map(rusqlite::params![msg.id], |row| row.get::<_, String>(0))
            {
                for data_str in rows.flatten() {
                    if let Ok(v) = serde_json::from_str::<Value>(&data_str) {
                        parts_by_msg.entry(msg.id.clone()).or_default().push(v);
                    }
                }
            }
        }
    }

    // Build exchanges: group by user messages
    let mut exchanges = Vec::new();
    let mut position = 0;

    // Find user message indices (with actual text)
    let mut user_indices: Vec<usize> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if msg.role != "user" {
            continue;
        }
        let parts = parts_by_msg.get(&msg.id).cloned().unwrap_or_default();
        let has_text = parts.iter().any(|p| {
            p.get("type").and_then(|v| v.as_str()) == Some("text")
                && !p
                    .get("synthetic")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                && !p
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
        });
        if has_text {
            user_indices.push(i);
        }
    }

    for (ui_pos, &user_idx) in user_indices.iter().enumerate() {
        let next_user_idx = user_indices
            .get(ui_pos + 1)
            .copied()
            .unwrap_or(messages.len());
        let user_msg = &messages[user_idx];
        let user_parts = parts_by_msg.get(&user_msg.id).cloned().unwrap_or_default();

        // Extract user text
        let user_text: String = user_parts
            .iter()
            .filter(|p| {
                p.get("type").and_then(|v| v.as_str()) == Some("text")
                    && !p
                        .get("synthetic")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
            })
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        let timestamp = if user_msg.time_created > 0 {
            let secs = user_msg.time_created / 1000;
            chrono::DateTime::from_timestamp(secs, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };

        // Process assistant messages between this user msg and next
        let mut action_parts: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        let mut tools: Vec<ToolUse> = Vec::new();

        for msg in &messages[(user_idx + 1)..next_user_idx] {
            if msg.role != "assistant" {
                continue;
            }
            let msg_parts = parts_by_msg.get(&msg.id).cloned().unwrap_or_default();
            for p in &msg_parts {
                let ptype = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ptype {
                    "text" => {
                        if p.get("synthetic")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                action_parts.push(text.to_string());
                            }
                        }
                    }
                    "tool" => {
                        let tool_name = p.get("tool").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let normalized = normalize_tool_name(tool_name);
                        let state = p.get("state").cloned().unwrap_or(json!({}));
                        let input = state.get("input").cloned().unwrap_or(json!({}));
                        let is_err = state.get("status").and_then(|v| v.as_str()) == Some("error");

                        // Extract file paths
                        if let Some(obj) = input.as_object() {
                            for field in &["file_path", "filePath", "path", "pattern", "file"] {
                                if let Some(val) = obj.get(*field).and_then(|v| v.as_str()) {
                                    if !val.is_empty() {
                                        let fname = Path::new(val)
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .unwrap_or(val)
                                            .to_string();
                                        if !files.contains(&fname) {
                                            files.push(fname);
                                        }
                                    }
                                }
                            }
                        }

                        let command = if normalized == "Bash" {
                            input
                                .get("command")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        } else {
                            None
                        };
                        let file = input
                            .get("file_path")
                            .or_else(|| input.get("filePath"))
                            .or_else(|| input.get("path"))
                            .and_then(|v| v.as_str())
                            .map(|s| {
                                Path::new(s)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(s)
                                    .to_string()
                            });

                        tools.push(ToolUse {
                            name: normalized.to_string(),
                            is_error: is_err,
                            file,
                            command,
                        });
                    }
                    _ => {}
                }
            }
        }

        position += 1;
        files.truncate(5);

        // Collect errors from tools with is_error
        let errors: Vec<Value> = tools
            .iter()
            .filter(|t| t.is_error)
            .map(|t| json!({"tool": t.name, "content": ""}))
            .collect();
        let ended_on_error = tools.last().map(|t| t.is_error).unwrap_or(false);
        let action =
            finalize_action_text(&action_parts.join("\n"), &tools, &errors, ended_on_error);

        exchanges.push(Exchange {
            position,
            user: user_text,
            action,
            files,
            timestamp,
            tools,
            edits: Vec::new(),
            errors,
            ended_on_error,
        });
    }

    // Apply last N
    if exchanges.len() > last {
        let skip = exchanges.len() - last;
        exchanges = exchanges.into_iter().skip(skip).collect();
    }

    Ok(exchanges)
}

/// Read file to string with lossy UTF-8 conversion (handles binary/corrupted files).
fn read_file_lossy(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Cannot read transcript: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn should_retry_codex_transcript(exchanges: &[Exchange]) -> bool {
    exchanges
        .last()
        .map(|ex| is_no_response_action(&ex.action))
        .unwrap_or(false)
}

fn retry_codex_transcript(
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

/// Get exchanges from a transcript file.
fn get_exchanges(
    path: &str,
    agent: &str,
    last: usize,
    detailed: bool,
    session_id: Option<&str>,
    retry_codex: bool,
) -> Result<Vec<Exchange>, String> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(format!("Transcript not found: {path}"));
    }

    let mut exchanges = match agent {
        "claude" => parse_claude_jsonl(p, last, detailed),
        "gemini" => parse_gemini_json(p, last),
        "codex" => parse_codex_jsonl(p, last, detailed),
        "opencode" => {
            let sid = session_id.unwrap_or("");
            if sid.is_empty() {
                return Err("OpenCode transcript requires a session_id".to_string());
            }
            parse_opencode_sqlite(p, sid, last)
        }
        _ => {
            // Try to detect from extension/path
            if path.ends_with(".json") {
                parse_gemini_json(p, last)
            } else if path.ends_with(".db") {
                let sid = session_id.unwrap_or("");
                if sid.is_empty() {
                    return Err("SQLite transcript requires a session_id".to_string());
                }
                parse_opencode_sqlite(p, sid, last)
            } else {
                parse_claude_jsonl(p, last, detailed)
            }
        }
    }?;

    if agent == "codex" && retry_codex && should_retry_codex_transcript(&exchanges) {
        // Codex rollout JSONL can briefly contain the user turn before the
        // assistant text for that same turn lands. Local transcript reads do a
        // short retry; RPC handlers opt out so they do not block the relay
        // reader thread.
        exchanges = retry_codex_transcript(p, last, detailed, exchanges)?;
    }

    Ok(exchanges)
}

// ── Formatting ───────────────────────────────────────────────────────────

/// Format exchanges for display
fn format_exchanges(exchanges: &[Exchange], _instance: &str, full: bool, detailed: bool) -> String {
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

        if detailed && !ex.tools.is_empty() {
            for tool in &ex.tools {
                let marker = if tool.is_error { "  ✗" } else { "  ├─" };
                let detail = tool
                    .file
                    .as_deref()
                    .or(tool.command.as_deref())
                    .unwrap_or("");
                lines.push(format!("{marker} {} {detail}", tool.name));
            }
        }

        lines.push(String::new()); // blank line between exchanges
    }

    // Trailing hint
    if !exchanges.is_empty() {
        if !full {
            lines.push("Note: Output truncated. Use --full for full text.".to_string());
        } else {
            lines.push(
                "Note: Tool outputs & file edits hidden. Use --detailed for full details."
                    .to_string(),
            );
        }
    }

    lines.join("\n")
}

/// Summarize action text (first 3 lines, strip prefixes).
fn summarize_action(text: &str) -> String {
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

// ── Search ───────────────────────────────────────────────────────────────

/// Correlate transcript file paths to hcom agent names via DB queries.
/// Checks instances table first, then stopped life events.
fn correlate_paths_to_hcom(
    db: &HcomDb,
    targets: &[(String, Option<String>)],
) -> std::collections::HashMap<String, String> {
    let mut result = std::collections::HashMap::new();
    let conn = db.conn();
    let target_keys: std::collections::HashSet<String> = targets
        .iter()
        .map(|(path, session_id)| transcript_search_key(path, session_id.as_deref()))
        .collect();

    // 1. Check current instances
    if let Ok(mut stmt) = conn.prepare(
        "SELECT name, transcript_path, session_id
         FROM instances
         WHERE transcript_path IS NOT NULL",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        }) {
            for (name, tp, session_id) in rows.flatten() {
                let key = transcript_search_key(&tp, session_id.as_deref());
                if target_keys.contains(&key) {
                    result.insert(key, name);
                }
            }
        }
    }

    // 2. Check stopped events for paths not yet matched
    if let Ok(mut stmt) = conn.prepare(
        "SELECT instance,
                json_extract(data, '$.snapshot.transcript_path') as tp,
                json_extract(data, '$.snapshot.session_id') as session_id \
         FROM events WHERE type = 'life' \
         AND json_extract(data, '$.action') = 'stopped' \
         AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL \
         ORDER BY id DESC",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        }) {
            for (name, tp, session_id) in rows.flatten() {
                let key = transcript_search_key(&tp, session_id.as_deref());
                if target_keys.contains(&key) && !result.contains_key(&key) {
                    result.insert(key, name);
                }
            }
        }
    }

    result
}

/// Search across transcripts: `hcom transcript search "pattern" [--live] [--all] [--limit N] [--exclude-self]`
fn cmd_transcript_search(
    db: &HcomDb,
    args: &TranscriptSearchArgs,
    ctx: Option<&CommandContext>,
) -> i32 {
    let live_mode = args.live;
    let all_mode = args.all;
    let json_mode = args.json;
    let limit = args.limit;
    let agent_filter = args.agent.as_ref();

    // Resolve self name for --exclude-self
    let ctx_name = if args.exclude_self {
        ctx.and_then(|c| c.identity.as_ref())
            .filter(|id| matches!(id.kind, crate::shared::SenderKind::Instance))
            .map(|id| id.name.clone())
    } else {
        None
    };

    let pattern = &args.pattern;

    // Collect transcript paths: (name, path, agent)
    let mut paths: Vec<(String, String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if all_mode {
        // --all: search disk-wide directories (not just hcom-tracked instances)
        let mut search_dirs: Vec<PathBuf> = Vec::new();
        let agent_filter = agent_filter.map(|s| s.as_str());
        let opencode_db = if transcript_agent_matches(agent_filter, "opencode") {
            get_opencode_db_path()
        } else {
            None
        };

        if transcript_agent_matches(agent_filter, "claude") {
            let p = claude_config_dir().join("projects");
            if p.exists() {
                search_dirs.push(p);
            }
        }
        if transcript_agent_matches(agent_filter, "gemini") {
            let home = dirs::home_dir().unwrap_or_default();
            let p = home.join(".gemini");
            if p.exists() {
                search_dirs.push(p);
            }
        }
        if transcript_agent_matches(agent_filter, "codex") {
            let home = dirs::home_dir().unwrap_or_default();
            let p = home.join(".codex").join("sessions");
            if p.exists() {
                search_dirs.push(p);
            }
        }

        if search_dirs.is_empty() && opencode_db.is_none() {
            println!("No transcript directories found on disk.");
            return 0;
        }

        // Phase 1: find matching files with rg -l (recursive, *.jsonl/*.json)
        let mut cmd = std::process::Command::new("rg");
        cmd.args(["-l", "--glob", "*.jsonl", "--glob", "*.json", pattern]);
        for d in &search_dirs {
            cmd.arg(d);
        }
        let output = cmd.output();
        let matching_files: Vec<String> = match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect(),
            _ => Vec::new(),
        };

        let opencode_matches = opencode_db
            .as_deref()
            .map(|db_path| search_opencode_sessions(db_path, pattern, limit))
            .transpose();
        let opencode_matches = match opencode_matches {
            Ok(Some(matches)) => matches,
            Ok(None) => Vec::new(),
            Err(err) => {
                eprintln!("Error: {err}");
                return 1;
            }
        };

        if matching_files.is_empty() && opencode_matches.is_empty() {
            if json_mode {
                println!("{}", json!({"count": 0, "results": [], "scope": "all"}));
            } else {
                println!("No matches for \"{pattern}\"");
            }
            return 0;
        }

        // Correlate transcript paths/session IDs to hcom names via DB.
        let mut targets: Vec<(String, Option<String>)> = matching_files
            .iter()
            .cloned()
            .map(|path| (path, None))
            .collect();
        targets.extend(
            opencode_matches
                .iter()
                .filter_map(|m| m.session_id.clone().map(|sid| (m.path.clone(), Some(sid)))),
        );
        let path_to_hcom = correlate_paths_to_hcom(db, &targets);

        // Extract line-level matches from each file
        let mut results = Vec::new();
        for file_path in &matching_files {
            if results.len() >= limit {
                break;
            }
            let agent = detect_agent_type(file_path);
            if let Some(af) = agent_filter {
                if !agent.contains(af) {
                    continue;
                }
            }
            let hcom_name = path_to_hcom
                .get(&transcript_search_key(file_path, None))
                .cloned()
                .unwrap_or_default();

            let remaining = limit - results.len();
            let out = std::process::Command::new("rg")
                .args([
                    "-n",
                    "--max-count",
                    &remaining.to_string(),
                    "--max-columns",
                    "500",
                    pattern,
                    file_path,
                ])
                .output();
            if let Ok(out) = out {
                if out.status.success() {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let lines: Vec<&str> = stdout.lines().collect();
                    let match_count = lines.len();
                    if match_count > 0 {
                        let first_line = lines[0];
                        let (line_num, snippet) = if let Some(colon_pos) = first_line.find(':') {
                            let num = first_line[..colon_pos].parse::<usize>().unwrap_or(0);
                            let text = &first_line[colon_pos + 1..];
                            let text = truncate_str(text, 100);
                            (num, text.to_string())
                        } else {
                            (0, first_line.to_string())
                        };

                        results.push(json!({
                            "hcom_name": if hcom_name.is_empty() { serde_json::Value::Null } else { json!(hcom_name) },
                            "agent": agent,
                            "path": file_path,
                            "line": line_num,
                            "text": snippet,
                            "matches": match_count,
                        }));
                    }
                }
            }
        }

        for opencode_match in &opencode_matches {
            if results.len() >= limit {
                break;
            }
            let hcom_name = path_to_hcom
                .get(&transcript_search_key(
                    &opencode_match.path,
                    opencode_match.session_id.as_deref(),
                ))
                .cloned()
                .unwrap_or_default();
            results.push(json!({
                "hcom_name": if hcom_name.is_empty() { serde_json::Value::Null } else { json!(hcom_name) },
                "agent": opencode_match.agent,
                "path": opencode_match.path,
                "line": opencode_match.line,
                "text": opencode_match.text,
                "matches": opencode_match.matches,
                "session_id": opencode_match.session_id,
                "label": opencode_match.label,
            }));
        }

        let scope_label = "";
        if json_mode {
            println!(
                "{}",
                json!({"count": results.len(), "results": results, "scope": "all"})
            );
        } else {
            if results.is_empty() {
                println!("No matches for \"{pattern}\"");
            } else {
                let _ = scope_label;
                println!(
                    "Found matches in {} transcripts (all on disk):",
                    results.len()
                );
                for r in &results {
                    let path = r["path"].as_str().unwrap_or("");
                    let agent = r["agent"].as_str().unwrap_or("?");
                    let line = r["line"].as_u64().unwrap_or(0);
                    let matches = r["matches"].as_u64().unwrap_or(0);
                    let snippet = r["text"].as_str().unwrap_or("");
                    let label = r["label"].as_str().unwrap_or("");
                    let session_id = r["session_id"].as_str().unwrap_or("");
                    let short_path = path
                        .split('/')
                        .rev()
                        .take(3)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("/");
                    let name_part = r["hcom_name"]
                        .as_str()
                        .map(|n| format!(" ({n})"))
                        .unwrap_or_default();
                    println!("  [{agent}]{name_part} .../{short_path}:{line}  ({matches} matches)");
                    if !label.is_empty() || !session_id.is_empty() {
                        let mut details = Vec::new();
                        if !label.is_empty() {
                            details.push(label.to_string());
                        }
                        if !session_id.is_empty() {
                            details.push(session_id.to_string());
                        }
                        println!("    {}", details.join(" | "));
                    }
                    if !snippet.is_empty() {
                        println!("    {snippet}");
                    }
                }
            }
        }
        return 0;
    } else {
        // Active instances
        if let Ok(mut stmt) = db.conn().prepare(
            "SELECT name, transcript_path, tool FROM instances WHERE transcript_path IS NOT NULL AND transcript_path != ''"
        ) {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            }) {
                for (name, path, tool) in rows.flatten() {
                    if let Some(agent) = agent_filter {
                        if !tool.contains(agent.as_str()) { continue; }
                    }
                    if args.exclude_self && ctx_name.as_deref() == Some(name.as_str()) { continue; }
                    seen.insert(name.clone());
                    paths.push((name, path, tool));
                }
            }
        }

        // Stopped instances from life event snapshots (C2/C3 fix)
        if !live_mode {
            if let Ok(mut stmt) = db.conn().prepare(
                "SELECT instance, json_extract(data, '$.snapshot.transcript_path'), json_extract(data, '$.snapshot.tool') FROM events WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped' AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL"
            ) {
                if let Ok(rows) = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                }) {
                    for (name, path, tool) in rows.flatten() {
                        if seen.contains(&name) { continue; }
                        if let Some(agent) = agent_filter {
                            if !tool.contains(agent.as_str()) { continue; }
                        }
                        if args.exclude_self && ctx_name.as_deref() == Some(name.as_str()) { continue; }
                        seen.insert(name.clone());
                        paths.push((name, path, tool));
                    }
                }
            }
        }
    }

    // Search using ripgrep (with line-level matches + snippets) — hcom-tracked/live paths
    let mut results = Vec::new();
    for (name, path, agent) in &paths {
        if !Path::new(path).exists() {
            continue;
        }

        // Use rg for line-level matches with context
        let remaining = limit - results.len();
        let output = std::process::Command::new("rg")
            .args([
                "-n",
                "--max-count",
                &remaining.to_string(),
                "--max-columns",
                "500",
                pattern,
                path,
            ])
            .output()
            .or_else(|_| {
                std::process::Command::new("grep")
                    .args(["-n", "-m", &remaining.to_string(), pattern, path])
                    .output()
            });

        if let Ok(out) = output {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let lines: Vec<&str> = stdout.lines().collect();
                let match_count = lines.len();
                if match_count > 0 {
                    // Extract first match line number and snippet
                    let first_line = lines[0];
                    let (line_num, snippet) = if let Some(colon_pos) = first_line.find(':') {
                        let num = first_line[..colon_pos].parse::<usize>().unwrap_or(0);
                        let text = &first_line[colon_pos + 1..];
                        let text = truncate_str(text, 100);
                        (num, text.to_string())
                    } else {
                        (0, first_line.to_string())
                    };

                    results.push(json!({
                        "hcom_name": name,
                        "agent": agent,
                        "path": path,
                        "line": line_num,
                        "text": snippet,
                        "matches": match_count,
                    }));
                }
            }
        }

        if results.len() >= limit {
            break;
        }
    }

    let scope_label = if live_mode {
        " (live agents)"
    } else if all_mode {
        ""
    } else {
        " (hcom-tracked)"
    };

    if json_mode {
        println!(
            "{}",
            json!({"count": results.len(), "results": results, "scope": if live_mode {"live"} else if all_mode {"all"} else {"hcom"}})
        );
    } else {
        if results.is_empty() {
            println!("No matches for \"{pattern}\"");
            return 0;
        }
        let limit_hit = results.len() >= limit;
        if limit_hit {
            println!(
                "Showing {} matches (limit {}){scope_label}:\n",
                results.len(),
                limit
            );
        } else {
            println!("Found {} matches{scope_label}:\n", results.len());
        }
        for result in &results {
            let hcom_name = result
                .get("hcom_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let agent = result.get("agent").and_then(|v| v.as_str()).unwrap_or("");
            let path = result.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let line = result.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            let snippet = result.get("text").and_then(|v| v.as_str()).unwrap_or("");

            let path_display = if path.len() > 60 {
                let mut start = path.len() - 57;
                while start < path.len() && !path.is_char_boundary(start) {
                    start += 1;
                }
                format!("...{}", &path[start..])
            } else {
                path.to_string()
            };

            println!("[{agent}:{hcom_name}] {path_display}:{line}");
            let snippet_clean = snippet.replace('\n', " ");
            let snippet_short = if snippet_clean.len() > 100 {
                format!("{}...", truncate_str(&snippet_clean, 100))
            } else {
                snippet_clean
            };
            println!("    {snippet_short}\n");
        }
    }

    0
}

/// Timeline: `hcom transcript timeline [--last N] [--full] [--json]`
fn cmd_transcript_timeline(db: &HcomDb, args: &TranscriptTimelineArgs) -> i32 {
    let json_mode = args.json;
    let full_mode = args.full;
    let detailed = args.detailed;
    let last_n = args.last.unwrap_or(10);

    // Collect all transcript paths (active + stopped sessions, C3 fix)
    let mut all_entries: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Active instances
    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT name, transcript_path, tool, session_id FROM instances WHERE transcript_path IS NOT NULL AND transcript_path != ''"
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        }) {
            for (name, path, tool, sid) in rows.flatten() {
                seen.insert(name.clone());
                if let Ok(exchanges) =
                    get_exchanges(&path, &tool, last_n, detailed, sid.as_deref(), true)
                {
                    for ex in exchanges {
                        all_entries.push(json!({
                            "instance": name,
                            "position": ex.position,
                            "user": ex.user,
                            "action": if full_mode { ex.action.clone() } else { summarize_action(&ex.action) },
                            "timestamp": ex.timestamp,
                            "files": ex.files,
                        }));
                    }
                }
            }
        }
    }

    // Stopped instances from life event snapshots
    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT instance, json_extract(data, '$.snapshot.transcript_path'), json_extract(data, '$.snapshot.tool'), json_extract(data, '$.snapshot.session_id') FROM events WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped' AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL"
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        }) {
            for (name, path, tool, sid) in rows.flatten() {
                if seen.contains(&name) { continue; }
                seen.insert(name.clone());
                if let Ok(exchanges) =
                    get_exchanges(&path, &tool, last_n, detailed, sid.as_deref(), true)
                {
                    for ex in exchanges {
                        all_entries.push(json!({
                            "instance": name,
                            "position": ex.position,
                            "user": ex.user,
                            "action": if full_mode { ex.action.clone() } else { summarize_action(&ex.action) },
                            "timestamp": ex.timestamp,
                            "files": ex.files,
                        }));
                    }
                }
            }
        }
    }

    // Sort by timestamp (most recent first)
    all_entries.sort_by(|a, b| {
        let ts_a = a.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let ts_b = b.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        ts_b.cmp(ts_a) // Reverse order
    });

    // Limit
    if all_entries.len() > last_n {
        all_entries.truncate(last_n);
    }

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&all_entries).unwrap_or_default()
        );
        return 0;
    }

    if all_entries.is_empty() {
        println!("No transcript entries found");
        return 0;
    }

    //
    println!("Timeline ({} exchanges):\n", all_entries.len());
    for entry in &all_entries {
        let inst = entry.get("instance").and_then(|v| v.as_str()).unwrap_or("");
        let ts = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let user = entry.get("user").and_then(|v| v.as_str()).unwrap_or("");
        let action = entry.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let files = entry.get("files").and_then(|v| v.as_array());

        let ts_short = if ts.contains('T') {
            ts.get(11..16).unwrap_or("??:??")
        } else if ts.len() >= 5 {
            ts.get(..5).unwrap_or("??:??")
        } else {
            "??:??"
        };

        let user_display = if user.len() > 80 {
            format!("{}...", truncate_str(user, 77))
        } else {
            user.to_string()
        };

        println!("[{ts_short}] \"{user_display}\"");

        if full_mode {
            for action_line in action.lines().take(10) {
                println!("  {action_line}");
            }
            let line_count = action.lines().count();
            if line_count > 10 {
                println!("  ... (+{} lines)", line_count - 10);
            }
        } else {
            let action_short = summarize_action(action);
            let action_display = if action_short.len() > 100 {
                format!("{}...", truncate_str(&action_short, 97))
            } else {
                action_short
            };
            println!("  → {action_display}");
        }

        if let Some(file_arr) = files {
            let file_strs: Vec<&str> = file_arr.iter().take(5).filter_map(|v| v.as_str()).collect();
            if !file_strs.is_empty() {
                println!("  Files: {}", file_strs.join(", "));
            }
        }

        // Command line (instance reference for navigation)
        println!(
            "  hcom transcript @{inst} {}",
            entry.get("position").and_then(|v| v.as_u64()).unwrap_or(1)
        );
        println!();
    }

    0
}

// ── Main Entry Point ─────────────────────────────────────────────────────

/// Main entry point for `hcom transcript` command.
pub fn cmd_transcript(db: &HcomDb, args: &TranscriptArgs, ctx: Option<&CommandContext>) -> i32 {
    // Handle subcommands
    match &args.subcmd {
        Some(TranscriptSubcmd::Search(search_args)) => {
            return cmd_transcript_search(db, search_args, ctx);
        }
        Some(TranscriptSubcmd::Timeline(timeline_args)) => {
            return cmd_transcript_timeline(db, timeline_args);
        }
        None => {}
    }

    let json_mode = args.json;
    let full_mode = args.full;
    let detailed = args.detailed;
    let last_n = args.last.unwrap_or(10);

    if let Some(ref name) = args.name {
        let stripped = name.strip_prefix('@').unwrap_or(name);
        let resolved = crate::instances::resolve_display_name_or_stopped(db, stripped)
            .unwrap_or_else(|| stripped.to_string());
        if let Some((base_name, device)) = crate::relay::control::split_device_suffix(&resolved) {
            return crate::relay::control::dispatch_remote_and_print(
                db,
                device,
                Some(&resolved),
                crate::relay::control::rpc_action::TRANSCRIPT,
                &json!({
                    "target": base_name,
                    "last": last_n,
                    "range": args.range_flag.as_ref().or(args.range_positional.as_ref()),
                    "json": json_mode,
                    "full": full_mode,
                    "detailed": detailed,
                }),
                crate::relay::control::RPC_DEFAULT_TIMEOUT,
                "content",
                "No remote transcript content",
            );
        }
    }

    // Resolve target and range from positional args
    let mut target = None;
    let mut range_str: Option<String> = args.range_flag.clone();

    if let Some(ref name) = args.name {
        let stripped = name.strip_prefix('@').unwrap_or(name);
        // Check if it looks like a range (digits and hyphens)
        if stripped.chars().all(|c| c.is_ascii_digit() || c == '-')
            && stripped.chars().any(|c| c.is_ascii_digit())
        {
            if range_str.is_none() {
                range_str = Some(stripped.to_string());
            }
        } else {
            target = Some(stripped.to_string());
        }
    }

    if let Some(ref range_pos) = args.range_positional {
        if range_str.is_none() {
            range_str = Some(range_pos.clone());
        }
    }

    // Resolve target to transcript path
    let (instance_name, transcript_path, agent_type, session_id) = if let Some(ref name) = target {
        // Try direct match
        let resolved = resolve_instance_transcript(db, name);
        match resolved {
            Some(r) => r,
            None => {
                eprintln!("Error: {}", no_transcript_error(db, name));
                return 1;
            }
        }
    } else if let Some(id) = ctx.and_then(|c| c.identity.as_ref()) {
        // Default to self
        match resolve_instance_transcript(db, &id.name) {
            Some(r) => r,
            None => {
                eprintln!("Error: No transcript available for current instance");
                return 1;
            }
        }
    } else {
        eprintln!("Usage: hcom transcript @instance [N | N-M] [--full] [--json]");
        return 1;
    };

    // Parse range
    let (range_start, range_end) = if let Some(ref r) = range_str {
        parse_range(r)
    } else {
        (None, None)
    };

    // Get exchanges
    let effective_last = if range_start.is_some() {
        usize::MAX
    } else {
        last_n
    };
    let exchanges = match get_exchanges(
        &transcript_path,
        &agent_type,
        effective_last,
        detailed,
        session_id.as_deref(),
        true,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // Apply range filter
    let filtered: Vec<&Exchange> = if let Some(start) = range_start {
        let end = range_end.unwrap_or(start);
        exchanges
            .iter()
            .filter(|e| e.position >= start && e.position <= end)
            .collect()
    } else {
        exchanges.iter().collect()
    };

    if json_mode {
        let json_output: Vec<Value> = filtered
            .iter()
            .map(|ex| {
                let mut obj = json!({
                    "position": ex.position,
                    "user": ex.user,
                    "action": ex.action,
                    "files": ex.files,
                    "timestamp": ex.timestamp,
                });
                if detailed {
                    obj["tools"] = json!(
                        ex.tools
                            .iter()
                            .map(|t| {
                                let mut tool = json!({
                                    "name": t.name,
                                    "is_error": t.is_error,
                                });
                                if let Some(ref f) = t.file {
                                    tool["file"] = json!(f);
                                }
                                if let Some(ref c) = t.command {
                                    tool["command"] = json!(c);
                                }
                                tool
                            })
                            .collect::<Vec<_>>()
                    );
                    obj["edits"] = json!(ex.edits);
                    obj["errors"] = json!(ex.errors);
                    obj["ended_on_error"] = json!(ex.ended_on_error);
                }
                obj
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json_output).unwrap_or_default()
        );
        return 0;
    }

    if filtered.is_empty() {
        println!("No exchanges found");
        return 0;
    }

    // Header: "Recent conversation (N exchanges, X-Y of Z) - @instance:"
    let first_pos = filtered.first().map(|e| e.position).unwrap_or(1);
    let last_pos = filtered.last().map(|e| e.position).unwrap_or(1);
    println!(
        "Recent conversation ({} exchanges, {}-{} of {}) - @{}:\n",
        filtered.len(),
        first_pos,
        last_pos,
        exchanges.len(),
        instance_name,
    );

    let owned: Vec<Exchange> = filtered.into_iter().cloned().collect();
    let formatted = format_exchanges(&owned, &instance_name, full_mode, detailed);
    println!("{formatted}");

    0
}

/// Display options for transcript rendering.
pub struct TranscriptRenderOpts<'a> {
    pub range: Option<&'a str>,
    pub last_n: usize,
    pub json_mode: bool,
    pub full_mode: bool,
    pub detailed: bool,
    pub retry_codex: bool,
}

impl Default for TranscriptRenderOpts<'_> {
    fn default() -> Self {
        Self {
            range: None,
            last_n: 10,
            json_mode: false,
            full_mode: false,
            detailed: false,
            retry_codex: true,
        }
    }
}

pub fn render_instance_transcript(
    db: &HcomDb,
    name: &str,
    last_n: usize,
) -> Result<String, String> {
    render_instance_transcript_impl(
        db,
        name,
        &TranscriptRenderOpts {
            last_n,
            ..Default::default()
        },
    )
}

pub fn render_instance_transcript_with_options_no_retry(
    db: &HcomDb,
    name: &str,
    range: Option<&str>,
    last_n: usize,
    json_mode: bool,
    full_mode: bool,
    detailed: bool,
) -> Result<String, String> {
    render_instance_transcript_impl(
        db,
        name,
        &TranscriptRenderOpts {
            range,
            last_n,
            json_mode,
            full_mode,
            detailed,
            retry_codex: false,
        },
    )
}

pub fn render_instance_transcript_with_options(
    db: &HcomDb,
    name: &str,
    range: Option<&str>,
    last_n: usize,
    json_mode: bool,
    full_mode: bool,
    detailed: bool,
) -> Result<String, String> {
    render_instance_transcript_impl(
        db,
        name,
        &TranscriptRenderOpts {
            range,
            last_n,
            json_mode,
            full_mode,
            detailed,
            retry_codex: true,
        },
    )
}

fn render_instance_transcript_impl(
    db: &HcomDb,
    name: &str,
    opts: &TranscriptRenderOpts<'_>,
) -> Result<String, String> {
    let (instance_name, transcript_path, agent_type, session_id) =
        resolve_instance_transcript(db, name).ok_or_else(|| no_transcript_error(db, name))?;
    let (range_start, range_end) = if let Some(r) = opts.range {
        parse_range(r)
    } else {
        (None, None)
    };
    let effective_last = if range_start.is_some() {
        usize::MAX
    } else {
        opts.last_n
    };
    let exchanges = get_exchanges(
        &transcript_path,
        &agent_type,
        effective_last,
        opts.detailed,
        session_id.as_deref(),
        opts.retry_codex,
    )
    .map_err(|e| e.to_string())?;

    let filtered: Vec<&Exchange> = if let Some(start) = range_start {
        let end = range_end.unwrap_or(start);
        exchanges
            .iter()
            .filter(|e| e.position >= start && e.position <= end)
            .collect()
    } else {
        exchanges.iter().collect()
    };

    if opts.json_mode {
        let json_output: Vec<Value> = filtered
            .iter()
            .map(|ex| {
                let mut obj = json!({
                    "position": ex.position,
                    "user": ex.user,
                    "action": ex.action,
                    "files": ex.files,
                    "timestamp": ex.timestamp,
                });
                if opts.detailed {
                    obj["tools"] = json!(
                        ex.tools
                            .iter()
                            .map(|t| {
                                let mut tool = json!({
                                    "name": t.name,
                                    "is_error": t.is_error,
                                });
                                if let Some(ref f) = t.file {
                                    tool["file"] = json!(f);
                                }
                                if let Some(ref c) = t.command {
                                    tool["command"] = json!(c);
                                }
                                tool
                            })
                            .collect::<Vec<_>>()
                    );
                    obj["edits"] = json!(ex.edits);
                    obj["errors"] = json!(ex.errors);
                    obj["ended_on_error"] = json!(ex.ended_on_error);
                }
                obj
            })
            .collect();
        return serde_json::to_string_pretty(&json_output).map_err(|e| e.to_string());
    }

    if filtered.is_empty() {
        return Ok("No exchanges found".to_string());
    }

    let first_pos = filtered.first().map(|e| e.position).unwrap_or(1);
    let last_pos = filtered.last().map(|e| e.position).unwrap_or(1);
    let owned: Vec<Exchange> = filtered.into_iter().cloned().collect();
    let formatted = format_exchanges(&owned, &instance_name, opts.full_mode, opts.detailed);
    Ok(format!(
        "Recent conversation ({} exchanges, {}-{} of {}) - @{}:\n\n{}",
        owned.len(),
        first_pos,
        last_pos,
        exchanges.len(),
        instance_name,
        formatted
    ))
}

/// Resolve instance name to (name, transcript_path, agent_type, session_id).
fn resolve_instance_transcript(
    db: &HcomDb,
    name: &str,
) -> Option<(String, String, String, Option<String>)> {
    let name = crate::instances::resolve_display_name_or_stopped(db, name)
        .unwrap_or_else(|| name.to_string());

    // Direct match
    if let Some(path) = get_transcript_path(db, &name) {
        let (tool, sid) = db
            .conn()
            .query_row(
                "SELECT tool, session_id FROM instances WHERE name = ?",
                rusqlite::params![&name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .unwrap_or_else(|_| (detect_agent_type(&path).to_string(), None));
        return Some((name, path, tool, sid));
    }

    // Prefix match
    if let Ok((matched_name, path, tool, sid)) = db.conn().query_row(
        "SELECT name, transcript_path, tool, session_id FROM instances WHERE name LIKE ? AND transcript_path IS NOT NULL AND transcript_path != '' LIMIT 1",
        rusqlite::params![format!("{}%", name)],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?)),
    ) {
        return Some((matched_name, path, tool, sid));
    }

    // Check stopped events (session_id from snapshot)
    if let Ok((path, sid)) = db.conn().query_row(
        "SELECT json_extract(data, '$.snapshot.transcript_path'), json_extract(data, '$.snapshot.session_id') FROM events WHERE type = 'life' AND instance = ? AND json_extract(data, '$.action') = 'stopped' ORDER BY id DESC LIMIT 1",
        rusqlite::params![&name],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
    ) {
        let agent = detect_agent_type(&path).to_string();
        return Some((name, path, agent, sid));
    }

    None
}

/// Parse range string "N-M" or "N".
fn parse_range(s: &str) -> (Option<usize>, Option<usize>) {
    if let Some(dash_pos) = s.find('-') {
        let start: Option<usize> = s[..dash_pos].parse().ok().filter(|&v: &usize| v >= 1);
        let end: Option<usize> = s[dash_pos + 1..].parse().ok().filter(|&v: &usize| v >= 1);
        // Validate start <= end
        if let (Some(s), Some(e)) = (start, end) {
            if s > e {
                eprintln!("Error: invalid range '{s}-{e}' (start must be <= end)");
                return (None, None);
            }
        }
        (start, end)
    } else {
        let pos: Option<usize> = s.parse().ok().filter(|&v: &usize| v >= 1);
        (pos, pos)
    }
}

// ── Public API for other commands (bundle) ──────────────────────────────

/// Options for querying and formatting transcript exchanges.
pub struct TranscriptQuery<'a> {
    pub path: &'a str,
    pub agent: &'a str,
    pub last: usize,
    pub detailed: bool,
    pub session_id: Option<&'a str>,
}

/// Public wrapper for get_exchanges (used by bundle prepare/cat).
pub fn get_exchanges_pub(q: &TranscriptQuery) -> Result<Vec<Value>, String> {
    let exchanges = get_exchanges(q.path, q.agent, q.last, q.detailed, q.session_id, true)?;
    Ok(exchanges
        .iter()
        .map(|ex| {
            json!({
                "position": ex.position,
                "user": ex.user,
                "action": ex.action,
                "files": ex.files,
                "timestamp": ex.timestamp,
            })
        })
        .collect())
}

/// Public wrapper for format_exchanges (used by bundle cat).
pub fn format_exchanges_pub(
    q: &TranscriptQuery,
    instance: &str,
    full: bool,
) -> Result<String, String> {
    let exchanges = get_exchanges(q.path, q.agent, q.last, q.detailed, q.session_id, true)?;
    Ok(format_exchanges(&exchanges, instance, full, q.detailed))
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_db() -> HcomDb {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        std::mem::forget(dir);
        db
    }

    #[test]
    fn test_normalize_tool_name() {
        assert_eq!(normalize_tool_name("run_shell_command"), "Bash");
        assert_eq!(normalize_tool_name("read_file"), "Read");
        assert_eq!(normalize_tool_name("write_file"), "Write");
        assert_eq!(normalize_tool_name("edit_file"), "Edit");
        assert_eq!(normalize_tool_name("search_files"), "Grep");
        assert_eq!(normalize_tool_name("replace"), "Edit");
        assert_eq!(normalize_tool_name("grep_search"), "Grep");
        assert_eq!(normalize_tool_name("read_many_files"), "Read");
        assert_eq!(normalize_tool_name("list_directory"), "Glob");
        assert_eq!(normalize_tool_name("Bash"), "Bash"); // Already canonical
    }

    #[test]
    fn test_parse_range() {
        assert_eq!(parse_range("5"), (Some(5), Some(5)));
        assert_eq!(parse_range("3-10"), (Some(3), Some(10)));
        assert_eq!(parse_range("abc"), (None, None));
    }

    #[test]
    fn test_summarize_action() {
        let short = "Hello world";
        assert_eq!(summarize_action(short), "Hello world");

        let multi = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5";
        let result = summarize_action(multi);
        assert!(result.contains("Line 1"));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_extract_text_content_string() {
        let msg = json!({"content": "hello"});
        assert_eq!(extract_text_content(&msg), "hello");
    }

    #[test]
    fn test_extract_text_content_blocks() {
        let msg = json!({
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"},
                {"type": "tool_result", "content": "ignored"}
            ]
        });
        let result = extract_text_content(&msg);
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
    }

    #[test]
    fn test_detect_agent_type() {
        assert_eq!(
            detect_agent_type("/home/user/.claude/projects/x/transcript.jsonl"),
            "claude"
        );
        assert_eq!(
            detect_agent_type("/home/user/.gemini/tmp/session.json"),
            "gemini"
        );
        assert_eq!(
            detect_agent_type("/home/user/.codex/sessions/x/rollout.jsonl"),
            "codex"
        );
        assert_eq!(
            detect_agent_type("/home/user/.local/share/opencode/opencode.db"),
            "opencode"
        );
    }

    #[test]
    fn test_search_opencode_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id text PRIMARY KEY, title text NOT NULL);
             CREATE TABLE part (
                 id text PRIMARY KEY,
                 message_id text NOT NULL,
                 session_id text NOT NULL,
                 time_created integer NOT NULL,
                 time_updated integer NOT NULL,
                 data text NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, title) VALUES (?, ?)",
            rusqlite::params!["ses_1", "Match Session"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, title) VALUES (?, ?)",
            rusqlite::params!["ses_2", "No Match Session"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "part_1",
                "msg_1",
                "ses_1",
                1_i64,
                1_i64,
                json!({"type": "text", "text": "first needle hit"}).to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "part_2",
                "msg_2",
                "ses_1",
                2_i64,
                2_i64,
                json!({"type": "text", "text": "second needle hit"}).to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "part_3",
                "msg_3",
                "ses_2",
                3_i64,
                3_i64,
                json!({"type": "text", "text": "plain text"}).to_string()
            ],
        )
        .unwrap();

        let matches = search_opencode_sessions(&db_path, "needle", 10).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].session_id.as_deref(), Some("ses_1"));
        assert_eq!(matches[0].label.as_deref(), Some("Match Session"));
        assert_eq!(matches[0].matches, 2);
        assert!(matches[0].text.contains("needle"));
    }

    #[test]
    fn test_correlate_paths_to_hcom_uses_session_id_for_opencode() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute_batch(
                "CREATE TABLE instances (
                     name text,
                     transcript_path text,
                     session_id text
                 );
                 CREATE TABLE events (
                     id integer PRIMARY KEY,
                     type text,
                     instance text,
                     data text
                 );",
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, transcript_path, session_id) VALUES (?, ?, ?)",
                rusqlite::params!["luna", "/tmp/opencode.db", "ses_a"],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, transcript_path, session_id) VALUES (?, ?, ?)",
                rusqlite::params!["nova", "/tmp/opencode.db", "ses_b"],
            )
            .unwrap();

        let correlated = correlate_paths_to_hcom(
            &db,
            &[
                ("/tmp/opencode.db".to_string(), Some("ses_a".to_string())),
                ("/tmp/opencode.db".to_string(), Some("ses_b".to_string())),
                ("/tmp/file.jsonl".to_string(), None),
            ],
        );

        assert_eq!(
            correlated.get(&transcript_search_key("/tmp/opencode.db", Some("ses_a"))),
            Some(&"luna".to_string())
        );
        assert_eq!(
            correlated.get(&transcript_search_key("/tmp/opencode.db", Some("ses_b"))),
            Some(&"nova".to_string())
        );
    }

    #[test]
    fn test_transcript_display_for_tool_only_and_error_turns() {
        let tools = vec![ToolUse {
            name: "Bash".to_string(),
            is_error: false,
            file: None,
            command: Some("pwd".to_string()),
        }];
        assert_eq!(
            finalize_action_text("", &tools, &[], false),
            "(tool-only turn: Bash)"
        );

        let errors = vec![json!({"tool": "Bash", "content": "Exit code: 1"})];
        assert_eq!(
            finalize_action_text("", &tools, &errors, true),
            "(turn ended in error after using Bash)"
        );
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
        assert_eq!(exchanges[1].user, "event only user");
        assert_eq!(exchanges[1].action, "event only assistant");
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
    fn test_parse_gemini_keeps_tool_only_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let transcript = json!({
            "messages": [
                {
                    "type": "user",
                    "timestamp": "2026-03-27T10:00:00Z",
                    "displayContent": [{"text": "find the bug"}]
                },
                {
                    "type": "gemini",
                    "timestamp": "2026-03-27T10:00:01Z",
                    "content": "",
                    "toolCalls": [
                        {
                            "name": "replace",
                            "status": "success",
                            "args": {"file_path": "/tmp/main.rs"}
                        }
                    ]
                }
            ]
        });
        fs::write(&path, transcript.to_string()).unwrap();

        let exchanges = parse_gemini_json(&path, 10).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user, "find the bug");
        assert_eq!(exchanges[0].action, "(tool-only turn: Edit)");
        assert_eq!(exchanges[0].files, vec!["main.rs".to_string()]);
        assert_eq!(exchanges[0].tools[0].name, "Edit");
    }

    #[test]
    fn test_parse_opencode_skips_synthetic_assistant_text() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                 id text PRIMARY KEY,
                 session_id text NOT NULL,
                 time_created integer NOT NULL,
                 data text NOT NULL
             );
             CREATE TABLE part (
                 id text PRIMARY KEY,
                 message_id text NOT NULL,
                 session_id text NOT NULL,
                 data text NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?, ?, ?, ?)",
            rusqlite::params!["m1", "ses_1", 1_i64, json!({"role": "user"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                "m2",
                "ses_1",
                2_i64,
                json!({"role": "assistant"}).to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, data) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                "p1",
                "m1",
                "ses_1",
                json!({"type": "text", "text": "user prompt", "synthetic": false}).to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, data) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                "p2",
                "m2",
                "ses_1",
                json!({"type": "text", "text": "synthetic note", "synthetic": true}).to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, data) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                "p3",
                "m2",
                "ses_1",
                json!({"type": "text", "text": "real assistant answer", "synthetic": false})
                    .to_string()
            ],
        )
        .unwrap();

        let exchanges = parse_opencode_sqlite(&db_path, "ses_1", 10).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user, "user prompt");
        assert_eq!(exchanges[0].action, "real assistant answer");
    }

    #[test]
    fn test_render_instance_transcript_with_options_range_matches_cli_shape() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("rollout.jsonl");
        let db = test_db();
        let now = crate::shared::time::now_epoch_f64();
        let lines = [
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "first user"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:01Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "first answer"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:01:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "second user"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:01:01Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "second answer"}]
                }
            }),
        ];
        fs::write(
            &transcript_path,
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let mut data = serde_json::Map::new();
        data.insert("created_at".into(), json!(now));
        data.insert("tool".into(), json!("codex"));
        data.insert(
            "transcript_path".into(),
            json!(transcript_path.to_string_lossy().to_string()),
        );
        db.save_instance_named("luna", &data).unwrap();

        let rendered = render_instance_transcript_with_options(
            &db,
            "luna",
            Some("2"),
            10,
            false,
            false,
            false,
        )
        .unwrap();

        assert!(rendered.contains("Recent conversation (1 exchanges, 2-2 of 2) - @luna:"));
        assert!(rendered.contains("second user"));
        assert!(rendered.contains("second answer"));
        assert!(!rendered.contains("first user"));
    }

    #[test]
    fn test_render_instance_transcript_with_options_json_contract() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("rollout.jsonl");
        let db = test_db();
        let now = crate::shared::time::now_epoch_f64();
        let lines = [
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
        ];
        fs::write(
            &transcript_path,
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let mut data = serde_json::Map::new();
        data.insert("created_at".into(), json!(now));
        data.insert("tool".into(), json!("codex"));
        data.insert(
            "transcript_path".into(),
            json!(transcript_path.to_string_lossy().to_string()),
        );
        db.save_instance_named("luna", &data).unwrap();

        let rendered =
            render_instance_transcript_with_options(&db, "luna", None, 10, true, false, false)
                .unwrap();
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        let first = parsed.as_array().unwrap().first().unwrap();

        assert_eq!(first["position"], 1);
        assert_eq!(first["user"], "user prompt");
        assert_eq!(first["action"], "assistant answer");
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

        let exchanges = get_exchanges(
            transcript_path.to_str().unwrap(),
            "codex",
            10,
            false,
            None,
            true,
        )
        .unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].action, "assistant answer");
    }

    #[test]
    fn test_finalize_action_text_uses_final_error_state_only() {
        let tools = vec![
            ToolUse {
                name: "Edit".to_string(),
                is_error: true,
                file: Some("a.rs".to_string()),
                command: None,
            },
            ToolUse {
                name: "Edit".to_string(),
                is_error: false,
                file: Some("a.rs".to_string()),
                command: None,
            },
        ];
        let errors = vec![json!({"tool": "Edit", "content": "old failure"})];
        assert_eq!(
            finalize_action_text("", &tools, &errors, false),
            "(tool-only turn: Edit)"
        );
    }

    // ── Clap parse tests ─────────────────────────────────────────────

    use clap::Parser;

    #[test]
    fn test_transcript_view_basic() {
        let args = TranscriptArgs::try_parse_from(["transcript", "peso"]).unwrap();
        assert!(args.subcmd.is_none());
        assert_eq!(args.name.as_deref(), Some("peso"));
        assert!(!args.json);
    }

    #[test]
    fn test_transcript_view_with_flags() {
        let args = TranscriptArgs::try_parse_from([
            "transcript",
            "@peso",
            "--json",
            "--full",
            "--last",
            "5",
        ])
        .unwrap();
        assert_eq!(args.name.as_deref(), Some("@peso"));
        assert!(args.json);
        assert!(args.full);
        assert_eq!(args.last, Some(5));
    }

    #[test]
    fn test_transcript_view_range() {
        let args = TranscriptArgs::try_parse_from(["transcript", "peso", "3-10"]).unwrap();
        assert_eq!(args.name.as_deref(), Some("peso"));
        assert_eq!(args.range_positional.as_deref(), Some("3-10"));
    }

    #[test]
    fn test_transcript_search() {
        let args = TranscriptArgs::try_parse_from([
            "transcript",
            "search",
            "error",
            "--live",
            "--limit",
            "50",
        ])
        .unwrap();
        match args.subcmd {
            Some(TranscriptSubcmd::Search(ref s)) => {
                assert_eq!(s.pattern, "error");
                assert!(s.live);
                assert_eq!(s.limit, 50);
            }
            _ => panic!("expected Search subcommand"),
        }
    }

    #[test]
    fn test_transcript_timeline() {
        let args =
            TranscriptArgs::try_parse_from(["transcript", "timeline", "--json", "--last", "3"])
                .unwrap();
        match args.subcmd {
            Some(TranscriptSubcmd::Timeline(ref t)) => {
                assert!(t.json);
                assert_eq!(t.last, Some(3));
            }
            _ => panic!("expected Timeline subcommand"),
        }
    }

    #[test]
    fn test_transcript_rejects_bogus() {
        assert!(TranscriptArgs::try_parse_from(["transcript", "--bogus"]).is_err());
    }
}
