//! Transcript reading: per-tool parsers and a unified read API.
//!
//! Tool-specific adapters normalize JSON, JSONL, and SQLite transcripts into
//! a tool-agnostic `Vec<Exchange>`. Canonical [`Tool`](crate::tool::Tool)
//! identity is kept separate from parser backend so aliases and shared formats
//! cannot drift into a second tool registry.

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod gemini;
pub mod kimi;
pub mod opencode;
pub mod pi;
pub mod shared;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::tool::Tool;

pub use shared::{Exchange, ToolUse, format_exchanges, summarize_action};

pub(crate) use opencode::TranscriptSearchMatch;

/// Parser implementation used for a transcript format.
///
/// This deliberately describes the backend rather than duplicating tool
/// identity: Antigravity shares Claude's JSONL parser, while Kilo shares the
/// OpenCode SQLite parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptBackend {
    ClaudeJsonl,
    GeminiJson,
    CodexJsonl,
    OpenCodeSqlite,
    CursorJsonl,
    KimiWireJsonl,
    CopilotJsonl,
    PiJsonl,
}

/// Where `transcript search --all` discovers sessions for a tool.
///
/// Parser format and discovery location are intentionally declared together in
/// [`TranscriptProfile`], preventing support from being added to one workflow
/// while silently omitted from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptDiscovery {
    ClaudeProjects,
    GeminiTree,
    CodexSessions,
    OpenCodeDatabase,
    KiloDatabase,
    CursorProjects,
    KimiSessions,
    CopilotSessionState,
    PiSessions,
    OmpSessions,
}

#[derive(Debug, Clone, Copy)]
struct TranscriptProfile {
    tool: Tool,
    backend: TranscriptBackend,
    discovery: TranscriptDiscovery,
}

static TRANSCRIPT_PROFILES: &[TranscriptProfile] = &[
    TranscriptProfile {
        tool: Tool::Claude,
        backend: TranscriptBackend::ClaudeJsonl,
        discovery: TranscriptDiscovery::ClaudeProjects,
    },
    TranscriptProfile {
        tool: Tool::Gemini,
        backend: TranscriptBackend::GeminiJson,
        discovery: TranscriptDiscovery::GeminiTree,
    },
    TranscriptProfile {
        tool: Tool::Codex,
        backend: TranscriptBackend::CodexJsonl,
        discovery: TranscriptDiscovery::CodexSessions,
    },
    TranscriptProfile {
        tool: Tool::OpenCode,
        backend: TranscriptBackend::OpenCodeSqlite,
        discovery: TranscriptDiscovery::OpenCodeDatabase,
    },
    TranscriptProfile {
        tool: Tool::Kilo,
        backend: TranscriptBackend::OpenCodeSqlite,
        discovery: TranscriptDiscovery::KiloDatabase,
    },
    TranscriptProfile {
        tool: Tool::Pi,
        backend: TranscriptBackend::PiJsonl,
        discovery: TranscriptDiscovery::PiSessions,
    },
    TranscriptProfile {
        tool: Tool::Omp,
        backend: TranscriptBackend::PiJsonl,
        discovery: TranscriptDiscovery::OmpSessions,
    },
    TranscriptProfile {
        tool: Tool::Antigravity,
        backend: TranscriptBackend::ClaudeJsonl,
        discovery: TranscriptDiscovery::GeminiTree,
    },
    TranscriptProfile {
        tool: Tool::Cursor,
        backend: TranscriptBackend::CursorJsonl,
        discovery: TranscriptDiscovery::CursorProjects,
    },
    TranscriptProfile {
        tool: Tool::Kimi,
        backend: TranscriptBackend::KimiWireJsonl,
        discovery: TranscriptDiscovery::KimiSessions,
    },
    TranscriptProfile {
        tool: Tool::Copilot,
        backend: TranscriptBackend::CopilotJsonl,
        discovery: TranscriptDiscovery::CopilotSessionState,
    },
];

fn profile_for_tool(tool: Tool) -> Option<&'static TranscriptProfile> {
    TRANSCRIPT_PROFILES
        .iter()
        .find(|profile| profile.tool == tool)
}

/// Resolve a canonical tool to its transcript parser backend.
pub fn backend_for_tool(tool: Tool) -> Option<TranscriptBackend> {
    profile_for_tool(tool).map(|profile| profile.backend)
}

/// Released tools with transcript support, in canonical integration order.
pub fn transcript_tools() -> Vec<Tool> {
    crate::integration_spec::ALL
        .iter()
        .filter(|spec| spec.released && profile_for_tool(spec.tool).is_some())
        .map(|spec| spec.tool)
        .collect()
}

/// Canonical names accepted by `transcript search --agent`.
pub fn transcript_tool_names() -> Vec<&'static str> {
    transcript_tools()
        .into_iter()
        .map(|tool| tool.as_str())
        .collect()
}

/// Parse an exact canonical name or declared alias for transcript filtering.
pub fn parse_tool_filter(value: &str) -> Result<Tool, String> {
    let tool = value.parse::<Tool>().map_err(|_| {
        format!(
            "Unknown transcript agent '{}'. Valid options: {}",
            value,
            transcript_tool_names().join(", ")
        )
    })?;
    if profile_for_tool(tool).is_none() {
        return Err(format!("Tool '{}' has no transcript profile", value));
    }
    Ok(tool)
}

/// Options for reading a transcript.
pub struct ReadOptions {
    pub last: usize,
    pub detailed: bool,
    /// Required by OpenCode-family (SQLite) parsers.
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

/// Read exchanges from a transcript at `path` using the selected backend.
pub fn read(
    path: &Path,
    backend: TranscriptBackend,
    opts: &ReadOptions,
) -> Result<Vec<Exchange>, String> {
    if !path.exists() {
        return Err(format!("Transcript not found: {}", path.display()));
    }

    let mut exchanges = match backend {
        TranscriptBackend::ClaudeJsonl => {
            claude::parse_claude_jsonl(path, opts.last, opts.detailed)
        }
        TranscriptBackend::GeminiJson => gemini::parse_gemini_json(path, opts.last),
        TranscriptBackend::CodexJsonl => codex::parse_codex_jsonl(path, opts.last, opts.detailed),
        TranscriptBackend::CursorJsonl => {
            cursor::parse_cursor_jsonl(path, opts.last, opts.detailed)
        }
        TranscriptBackend::KimiWireJsonl => {
            kimi::parse_kimi_wire_jsonl(path, opts.last, opts.detailed)
        }
        TranscriptBackend::CopilotJsonl => {
            copilot::parse_copilot_jsonl(path, opts.last, opts.detailed)
        }
        TranscriptBackend::PiJsonl => pi::parse_pi_jsonl(path, opts.last, opts.detailed),
        TranscriptBackend::OpenCodeSqlite => {
            let sid = opts.session_id.as_deref().unwrap_or("");
            if sid.is_empty() {
                return Err("OpenCode transcript requires a session_id".to_string());
            }
            opencode::parse_opencode_sqlite(path, sid, opts.last)
        }
    }?;

    if backend == TranscriptBackend::CodexJsonl
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

/// Detect canonical tool identity from a transcript path.
///
/// Specific path signatures are checked before broader directory fallbacks so
/// unknown JSON, JSONL, and SQLite files remain unknown instead of silently
/// selecting an unrelated parser.
pub fn detect_tool_from_path(path: &str) -> Option<Tool> {
    // Normalize separators so signatures work for persisted Windows paths too.
    let lower = path.to_ascii_lowercase().replace('\\', "/");
    let file_name = lower.rsplit('/').next().unwrap_or(&lower);

    // Prefer format- or product-specific signatures before broad directory
    // fallbacks. Unknown generic JSON/JSONL/DB files stay unknown rather than
    // being silently assigned a parser.
    if lower.contains("antigravity") || lower.contains("/agy/") || lower.contains("/agy-") {
        Some(Tool::Antigravity)
    } else if lower.contains("/agent-transcripts/") {
        Some(Tool::Cursor)
    } else if lower.contains("/.copilot/session-state/")
        || (lower.contains("/session-state/") && file_name == "events.jsonl")
    {
        Some(Tool::Copilot)
    } else if lower.contains("/.omp/agent/sessions/") {
        Some(Tool::Omp)
    } else if lower.contains("/.pi/agent/sessions/")
        || lower.contains("/.pi/sessions/")
        || lower.contains("pi_coding_agent_session")
    {
        // Pi session files are bare `<uuid>.jsonl` with no content signature, so
        // path detection only recognizes the default session tree. A custom
        // `PI_CODING_AGENT_SESSION_DIR` (pi's `--session-dir` env equivalent)
        // outside `.pi/` stays `unknown` here — indistinguishable from
        // Claude/Codex JSONL by path alone. Both consumers of that override
        // handle it without this fallback: resume/fork key on persisted agent
        // identity, and `transcript search --all` attributes by search-root
        // provenance (see `commands::transcript::attribute_disk_match`).
        Some(Tool::Pi)
    } else if lower.contains("/.kimi-code/sessions/") || lower.ends_with("/agents/main/wire.jsonl")
    {
        Some(Tool::Kimi)
    } else if lower.contains("/.codex/sessions/")
        || (file_name.starts_with("rollout-") && file_name.ends_with(".jsonl"))
    {
        Some(Tool::Codex)
    } else if file_name == "opencode.db" || lower.contains("/opencode/") {
        Some(Tool::OpenCode)
    } else if file_name == "kilo.db" || lower.contains("/kilo/") {
        Some(Tool::Kilo)
    } else if lower.contains("/.gemini/tmp/")
        && lower.contains("/chats/")
        && file_name.starts_with("session-")
        && file_name.ends_with(".json")
    {
        Some(Tool::Gemini)
    } else if lower.contains("/.claude/") || lower.contains("/projects/") {
        // The generic projects segment supports custom CLAUDE_CONFIG_DIR roots;
        // cursor is checked first because its paths also contain /projects/.
        Some(Tool::Claude)
    } else {
        None
    }
}

/// Return a stable display/filter name for a transcript path.
pub fn agent_name_from_path(path: &str) -> &'static str {
    detect_tool_from_path(path)
        .map(|tool| tool.as_str())
        .unwrap_or("unknown")
}

/// Resolve canonical tool identity from persisted agent text, with path
/// inference only as a compatibility aid when identity is absent or unknown.
pub fn tool_from_agent_or_path(agent: &str, path: &str) -> Result<Tool, String> {
    let parsed = if agent == "claude-pty" {
        Some(Tool::Claude)
    } else {
        agent.parse::<Tool>().ok()
    };
    parsed
        .or_else(|| detect_tool_from_path(path))
        .ok_or_else(|| {
            format!(
                "Unable to determine transcript parser for agent '{}' and path '{}'",
                agent, path
            )
        })
}

/// Resolve the parser backend from persisted identity and/or path.
pub fn backend_from_agent_or_path(agent: &str, path: &str) -> Result<TranscriptBackend, String> {
    let tool = tool_from_agent_or_path(agent, path)?;
    backend_for_tool(tool).ok_or_else(|| format!("Tool '{}' has no transcript backend", tool))
}

fn env_or_default_dir(env_var: &str, default: PathBuf) -> PathBuf {
    std::env::var(env_var)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default)
}

/// Canonical Claude project transcript root.
///
/// Resume/fork lookup and disk-wide transcript search deliberately share this
/// resolver so environment overrides cannot drift between workflows.
pub(crate) fn claude_projects_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    env_or_default_dir("CLAUDE_CONFIG_DIR", home.join(".claude")).join("projects")
}

/// Filesystem roots searched by `transcript search --all` for a tool.
/// Database-backed profiles return no roots and expose a database path through
/// [`database_search_path`] instead.
pub fn disk_search_roots(tool: Tool) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let Some(profile) = profile_for_tool(tool) else {
        return Vec::new();
    };
    match profile.discovery {
        TranscriptDiscovery::ClaudeProjects => {
            vec![env_or_default_dir("CLAUDE_CONFIG_DIR", home.join(".claude")).join("projects")]
        }
        TranscriptDiscovery::GeminiTree => {
            let root = std::env::var("GEMINI_CLI_HOME")
                .ok()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|path| path.join(".gemini"))
                .unwrap_or_else(|| home.join(".gemini"));
            vec![root]
        }
        TranscriptDiscovery::CodexSessions => {
            vec![env_or_default_dir("CODEX_HOME", home.join(".codex")).join("sessions")]
        }
        TranscriptDiscovery::CursorProjects => vec![home.join(".cursor").join("projects")],
        TranscriptDiscovery::KimiSessions => {
            vec![env_or_default_dir("KIMI_CODE_HOME", home.join(".kimi-code")).join("sessions")]
        }
        TranscriptDiscovery::CopilotSessionState => {
            vec![env_or_default_dir("COPILOT_HOME", home.join(".copilot")).join("session-state")]
        }
        TranscriptDiscovery::PiSessions => {
            let mut roots = Vec::new();
            if let Ok(path) = std::env::var("PI_CODING_AGENT_SESSION_DIR")
                && !path.is_empty()
            {
                roots.push(PathBuf::from(path));
            }
            roots.push(home.join(".pi").join("agent").join("sessions"));
            roots
        }
        TranscriptDiscovery::OmpSessions => {
            let mut roots = Vec::new();
            if let Ok(path) = std::env::var("PI_CODING_AGENT_SESSION_DIR")
                && !path.is_empty()
            {
                roots.push(PathBuf::from(path));
            }
            if let Ok(path) = std::env::var("XDG_DATA_HOME")
                && !path.is_empty()
            {
                roots.push(PathBuf::from(path).join("omp").join("sessions"));
            }
            // When the launcher isolates OMP config via PI_CODING_AGENT_DIR,
            // sessions live under <dir>/sessions.
            if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR")
                && !dir.is_empty()
            {
                roots.push(PathBuf::from(dir).join("sessions"));
            }
            roots.push(home.join(".omp").join("agent").join("sessions"));
            roots
        }
        TranscriptDiscovery::OpenCodeDatabase | TranscriptDiscovery::KiloDatabase => Vec::new(),
    }
}

/// Existing database source for a database-backed transcript profile.
pub(crate) fn database_search_path(tool: Tool) -> Option<PathBuf> {
    match profile_for_tool(tool)?.discovery {
        TranscriptDiscovery::OpenCodeDatabase => opencode::get_opencode_db_path(),
        TranscriptDiscovery::KiloDatabase => opencode::get_kilo_db_path(),
        _ => None,
    }
}

/// Search a database-backed transcript profile. Callers pass the path returned
/// by [`database_search_path`], keeping family-specific SQL out of command code.
pub(crate) fn search_database_sessions(
    tool: Tool,
    db_path: &Path,
    pattern: &str,
    limit: usize,
) -> Result<Vec<TranscriptSearchMatch>, String> {
    match profile_for_tool(tool).map(|profile| profile.discovery) {
        Some(TranscriptDiscovery::OpenCodeDatabase) => {
            opencode::search_opencode_sessions(db_path, pattern, limit)
        }
        Some(TranscriptDiscovery::KiloDatabase) => {
            opencode::search_kilo_sessions(db_path, pattern, limit)
        }
        _ => Err(format!("Tool '{}' is not database-backed", tool)),
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
    let backend = backend_from_agent_or_path(q.agent, q.path)?;
    let opts = ReadOptions {
        last: q.last,
        detailed: q.detailed,
        session_id: q.session_id.map(|s| s.to_string()),
        allow_codex_retry: true,
    };
    let exchanges = read(Path::new(q.path), backend, &opts)?;
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
    let backend = backend_from_agent_or_path(q.agent, q.path)?;
    let opts = ReadOptions {
        last: q.last,
        detailed: q.detailed,
        session_id: q.session_id.map(|s| s.to_string()),
        allow_codex_retry: true,
    };
    let exchanges = read(Path::new(q.path), backend, &opts)?;
    Ok(format_exchanges(&exchanges, instance, full, q.detailed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_detection_routes_specific_jsonl_formats() {
        assert_eq!(
            detect_tool_from_path("/h/.cursor/projects/r/agent-transcripts/u/u.jsonl"),
            Some(Tool::Cursor)
        );
        assert_eq!(
            detect_tool_from_path("/h/.copilot/session-state/u/events.jsonl"),
            Some(Tool::Copilot)
        );
        assert_eq!(
            detect_tool_from_path("/h/.pi/agent/sessions/r/u.jsonl"),
            Some(Tool::Pi)
        );
        assert_eq!(
            detect_tool_from_path("/h/.omp/agent/sessions/r/u.jsonl"),
            Some(Tool::Omp)
        );
        assert_eq!(
            detect_tool_from_path("/h/.kimi-code/sessions/wd/u/agents/main/wire.jsonl"),
            Some(Tool::Kimi)
        );
        assert_eq!(
            detect_tool_from_path("/h/.gemini/tmp/project/chats/session-1-abc.json"),
            Some(Tool::Gemini)
        );
        assert_eq!(
            detect_tool_from_path("/h/.claude/projects/r/u.jsonl"),
            Some(Tool::Claude)
        );
        assert_eq!(detect_tool_from_path("/h/.gemini/settings.json"), None);
        assert_eq!(detect_tool_from_path("/tmp/session.jsonl"), None);
        assert_eq!(detect_tool_from_path("/tmp/session.json"), None);
        assert_eq!(detect_tool_from_path("/tmp/random.db"), None);
    }

    #[test]
    fn identity_selects_shared_backends_without_duplicate_tool_enum() {
        assert_eq!(
            backend_for_tool(Tool::Antigravity),
            Some(TranscriptBackend::ClaudeJsonl)
        );
        assert_eq!(
            backend_for_tool(Tool::Kilo),
            Some(TranscriptBackend::OpenCodeSqlite)
        );
    }

    #[test]
    fn unknown_agent_and_ambiguous_path_is_an_error() {
        let err = backend_from_agent_or_path("future-tool", "/tmp/session.jsonl").unwrap_err();
        assert!(err.contains("future-tool"));
        assert!(err.contains("session.jsonl"));
    }

    #[test]
    fn legacy_claude_pty_agent_maps_to_claude_backend() {
        assert_eq!(
            backend_from_agent_or_path("claude-pty", "/h/.claude/projects/r/u.jsonl").unwrap(),
            TranscriptBackend::ClaudeJsonl
        );
    }

    #[test]
    fn exact_filter_accepts_declared_aliases_and_rejects_substrings() {
        assert_eq!(parse_tool_filter("pi-agent").unwrap(), Tool::Pi);
        assert_eq!(parse_tool_filter("agy").unwrap(), Tool::Antigravity);
        assert!(parse_tool_filter("cop").is_err());
    }

    #[test]
    fn profiles_are_unique_and_cover_every_released_tool() {
        let mut seen = Vec::new();
        for profile in TRANSCRIPT_PROFILES {
            assert!(
                !seen.contains(&profile.tool),
                "duplicate profile for {}",
                profile.tool
            );
            seen.push(profile.tool);
        }
        for spec in crate::integration_spec::ALL {
            if spec.released {
                assert!(
                    profile_for_tool(spec.tool).is_some(),
                    "missing transcript profile for {}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn every_transcript_tool_has_a_disk_or_database_discovery_source() {
        for tool in transcript_tools() {
            let roots = disk_search_roots(tool);
            if matches!(tool, Tool::OpenCode | Tool::Kilo) {
                assert!(
                    roots.is_empty(),
                    "database tool {tool} should not expose disk roots"
                );
            } else {
                assert!(!roots.is_empty(), "missing disk discovery roots for {tool}");
            }
        }
    }

    #[test]
    fn omp_disk_roots_include_pi_coding_agent_dir_sessions() {
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        unsafe {
            std::env::set_var("PI_CODING_AGENT_DIR", "/tmp/test-omp-agent");
        }

        let roots = disk_search_roots(Tool::Omp);
        assert!(
            roots
                .iter()
                .any(|r| r == &PathBuf::from("/tmp/test-omp-agent/sessions")),
            "OMP roots must include PI_CODING_AGENT_DIR/sessions, got {roots:?}"
        );
    }

    #[test]
    fn omp_disk_roots_include_xdg_data_home_sessions() {
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        unsafe {
            std::env::set_var("XDG_DATA_HOME", "/tmp/test-omp-xdg");
        }

        let roots = disk_search_roots(Tool::Omp);
        assert!(
            roots
                .iter()
                .any(|r| r == &PathBuf::from("/tmp/test-omp-xdg/omp/sessions")),
            "OMP roots must include XDG_DATA_HOME/omp/sessions, got {roots:?}"
        );
    }
}
