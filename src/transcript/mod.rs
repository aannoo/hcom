//! Transcript reading: per-tool parsers and a unified read API.
//!
//! Adapters for Claude (.jsonl), Gemini (.json), Codex (.jsonl), and OpenCode
//! (SQLite). All return a tool-agnostic `Vec<Exchange>` so callers can format
//! and project without caring about the source tool.

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod shared;

use std::path::Path;

use serde_json::{Value, json};

pub use shared::{Exchange, ToolUse, format_exchanges, summarize_action};

pub(crate) use opencode::{get_opencode_db_path, search_opencode_sessions};

/// Which tool produced the transcript (replaces stringly-typed agent codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Claude,
    Gemini,
    Codex,
    OpenCode,
}

/// Options for reading a transcript.
pub struct ReadOptions {
    pub last: usize,
    pub detailed: bool,
    /// Required by OpenCode (SQLite) parsers.
    pub session_id: Option<String>,
    /// Codex-only: short retry when the rollout JSONL has not yet been flushed
    /// past the user turn.
    pub allow_codex_retry: bool,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            last: 10,
            detailed: false,
            session_id: None,
            allow_codex_retry: true,
        }
    }
}

/// Read exchanges from a transcript at `path` for the given tool.
pub fn read(path: &Path, kind: ToolKind, opts: &ReadOptions) -> Result<Vec<Exchange>, String> {
    if !path.exists() {
        return Err(format!("Transcript not found: {}", path.display()));
    }

    let mut exchanges = match kind {
        ToolKind::Claude => claude::parse_claude_jsonl(path, opts.last, opts.detailed),
        ToolKind::Gemini => gemini::parse_gemini_json(path, opts.last),
        ToolKind::Codex => codex::parse_codex_jsonl(path, opts.last, opts.detailed),
        ToolKind::OpenCode => {
            let sid = opts.session_id.as_deref().unwrap_or("");
            if sid.is_empty() {
                return Err("OpenCode transcript requires a session_id".to_string());
            }
            opencode::parse_opencode_sqlite(path, sid, opts.last)
        }
    }?;

    if matches!(kind, ToolKind::Codex)
        && opts.allow_codex_retry
        && codex::should_retry_codex_transcript(&exchanges)
    {
        // Codex rollout JSONL can briefly contain the user turn before the
        // assistant text for that same turn lands. Local transcript reads do a
        // short retry; RPC handlers opt out so they do not block the relay
        // reader thread.
        exchanges = codex::retry_codex_transcript(path, opts.last, opts.detailed, exchanges)?;
    }

    Ok(exchanges)
}

/// Detect tool kind from a transcript file extension. Returns None for the
/// ambiguous `.jsonl`/extensionless cases so callers can fall through to their
/// own default (Claude in the existing CLI flow).
pub fn detect_kind_from_path(path: &str) -> Option<ToolKind> {
    if path.ends_with(".json") {
        Some(ToolKind::Gemini)
    } else if path.ends_with(".db") {
        Some(ToolKind::OpenCode)
    } else {
        None
    }
}

/// Map the agent string used elsewhere ("claude" / "gemini" / "codex" /
/// "opencode") to a `ToolKind`. Falls back to extension inference, then Claude.
pub fn kind_from_agent_or_path(agent: &str, path: &str) -> ToolKind {
    match agent {
        "claude" => ToolKind::Claude,
        "gemini" => ToolKind::Gemini,
        "codex" => ToolKind::Codex,
        "opencode" => ToolKind::OpenCode,
        _ => detect_kind_from_path(path).unwrap_or(ToolKind::Claude),
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

/// Public wrapper for read (used by bundle prepare/cat).
///
/// Returns a JSON projection that intentionally drops tools/edits/errors/
/// ended_on_error — bundle consumers only read user/action/files/timestamp.
pub fn get_exchanges_pub(q: &TranscriptQuery) -> Result<Vec<Value>, String> {
    let kind = kind_from_agent_or_path(q.agent, q.path);
    let opts = ReadOptions {
        last: q.last,
        detailed: q.detailed,
        session_id: q.session_id.map(|s| s.to_string()),
        allow_codex_retry: true,
    };
    let exchanges = read(Path::new(q.path), kind, &opts)?;
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
    let kind = kind_from_agent_or_path(q.agent, q.path);
    let opts = ReadOptions {
        last: q.last,
        detailed: q.detailed,
        session_id: q.session_id.map(|s| s.to_string()),
        allow_codex_retry: true,
    };
    let exchanges = read(Path::new(q.path), kind, &opts)?;
    Ok(format_exchanges(&exchanges, instance, full, q.detailed))
}
