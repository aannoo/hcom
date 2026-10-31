//! Transcript reading: per-tool parsers and a unified read API.
//!
//! Tool-specific adapters normalize JSON, JSONL, and SQLite transcripts into
//! tool-agnostic exchanges and recorded session usage. Canonical [`Tool`](crate::tool::Tool)
//! identity is kept separate from parser backend so aliases and shared formats
//! cannot drift into a second tool registry.

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod devin;
pub mod gemini;
pub mod grok;
pub mod hermes;
pub mod kimi;
pub mod opencode;
pub mod pi;
pub mod shared;

use std::path::{Path, PathBuf};

use crate::tool::Tool;

pub use shared::{
    Exchange, SessionUsage, TokenUsage, ToolUse, Transcript, flag_hint, format_context,
    format_exchanges, format_transcript_exchanges, format_usage, summarize_action,
    summarize_exchange,
};

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
    GrokUpdatesJsonl,
    HermesSqlite,
    DevinAtifJson,
}

impl TranscriptBackend {
    /// One database file holds every session, so a transcript is the
    /// (path, session_id) pair rather than the path alone.
    pub fn is_shared_database(self) -> bool {
        matches!(self, Self::OpenCodeSqlite | Self::HermesSqlite)
    }
}

/// Where `transcript search --all` discovers sessions for a tool.
///
/// Parser format and discovery location are intentionally declared together in
/// [`TranscriptProfile`], preventing support from being added to one workflow
/// while silently omitted from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptDiscovery {
    ClaudeProjects,
    QoderProjects,
    GeminiTree,
    CodexSessions,
    OpenCodeDatabase,
    KiloDatabase,
    CursorProjects,
    KimiSessions,
    CopilotSessionState,
    PiSessions,
    OmpSessions,
    GrokSessions,
    HermesDatabase,
    DevinTranscripts,
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
        tool: Tool::Hermes,
        backend: TranscriptBackend::HermesSqlite,
        discovery: TranscriptDiscovery::HermesDatabase,
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
    TranscriptProfile {
        // Qoder writes Claude-format JSONL under `<config dir>/projects/`.
        tool: Tool::Qoder,
        backend: TranscriptBackend::ClaudeJsonl,
        discovery: TranscriptDiscovery::QoderProjects,
    },
    TranscriptProfile {
        tool: Tool::Devin,
        backend: TranscriptBackend::DevinAtifJson,
        discovery: TranscriptDiscovery::DevinTranscripts,
    },
    TranscriptProfile {
        tool: Tool::Grok,
        backend: TranscriptBackend::GrokUpdatesJsonl,
        discovery: TranscriptDiscovery::GrokSessions,
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

/// Read exchanges and whole-session usage using the selected backend.
pub fn read(
    path: &Path,
    backend: TranscriptBackend,
    opts: &ReadOptions,
) -> Result<Transcript, String> {
    if !path.exists() {
        return Err(format!("Transcript not found: {}", path.display()));
    }

    let mut transcript = match backend {
        TranscriptBackend::ClaudeJsonl => {
            claude::parse_claude_jsonl_with_usage(path, opts.detailed)
        }
        TranscriptBackend::GeminiJson => gemini::parse_gemini_json_with_usage(path),
        TranscriptBackend::CodexJsonl => codex::parse_codex_jsonl_with_usage(path, opts.detailed),
        TranscriptBackend::CursorJsonl => {
            cursor::parse_cursor_jsonl_with_usage(path, opts.detailed)
        }
        TranscriptBackend::KimiWireJsonl => {
            kimi::parse_kimi_wire_jsonl_with_usage(path, opts.detailed)
        }
        TranscriptBackend::CopilotJsonl => {
            copilot::parse_copilot_jsonl_with_usage(path, opts.detailed)
        }
        TranscriptBackend::PiJsonl => pi::parse_pi_jsonl_with_usage(path, opts.detailed),
        TranscriptBackend::GrokUpdatesJsonl => {
            grok::parse_grok_updates_jsonl_with_usage(path, opts.detailed)
        }
        TranscriptBackend::DevinAtifJson => devin::parse_devin_json_with_usage(path),
        TranscriptBackend::OpenCodeSqlite => {
            let sid = opts.session_id.as_deref().unwrap_or("");
            if sid.is_empty() {
                return Err("OpenCode transcript requires a session_id".to_string());
            }
            opencode::parse_opencode_sqlite_with_usage(path, sid)
        }
        TranscriptBackend::HermesSqlite => {
            let sid = opts.session_id.as_deref().unwrap_or("");
            if sid.is_empty() {
                return Err("Hermes transcript requires a session_id".to_string());
            }
            hermes::parse_hermes_sqlite_with_usage(path, sid)
        }
    }?;

    if backend == TranscriptBackend::CodexJsonl && opts.allow_codex_retry {
        for _ in 0..4 {
            if !codex::should_retry_codex_transcript(&transcript.exchanges) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
            transcript = codex::parse_codex_jsonl_with_usage(path, opts.detailed)?;
        }
    }
    transcript.truncate(opts.last);
    Ok(transcript)
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
    } else if file_name == "updates.jsonl" {
        // Only Grok writes `updates.jsonl` (`$GROK_HOME/sessions/<cwd>/<id>/`,
        // and GROK_HOME may be anywhere).
        Some(Tool::Grok)
    } else if file_name == "state.db" && lower.contains("/.hermes/") {
        // The default root and `profiles/<name>/` under it; a custom
        // HERMES_HOME is attributed by persisted agent identity instead.
        Some(Tool::Hermes)
    } else if lower.contains("/agent-transcripts/") {
        Some(Tool::Cursor)
    } else if lower.contains("/.copilot/session-state/")
        || (lower.contains("/session-state/") && file_name == "events.jsonl")
    {
        Some(Tool::Copilot)
    } else if lower.contains("/.qoder/") || under_custom_qoder_root(&lower) {
        Some(Tool::Qoder)
    } else if lower.contains("/devin/cli/transcripts/") && file_name.ends_with(".json") {
        Some(Tool::Devin)
    } else if lower.contains("/.omp/") {
        // Covers the default tree (`/.omp/agent/sessions/`) and named-profile
        // trees (`/.omp/profiles/<name>/agent/sessions/`). XDG and
        // PI_CODING_AGENT_DIR roots have no `.omp` in the path and are attributed
        // by search-root provenance in `attribute_disk_match` instead.
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

/// True for a transcript under a custom `QODER_CONFIG_DIR`. Such a path has no
/// `.qoder` segment and would otherwise fall through to Claude's generic
/// `/projects/` signature. A variable pointed at Claude's own `.claude` dir
/// claims nothing: that tree is Claude's. `lower` is lowercased with `/`
/// separators.
fn under_custom_qoder_root(lower: &str) -> bool {
    let Some(dir) = std::env::var("QODER_CONFIG_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
    else {
        return false;
    };
    let root = dir.to_ascii_lowercase().replace('\\', "/");
    let root = root.trim_end_matches('/');
    if root.ends_with("/.claude") {
        return false;
    }
    lower
        .strip_prefix(root)
        .is_some_and(|rest| rest.starts_with("/projects/"))
}

/// `<QODER_CONFIG_DIR or ~/.qoder>/projects`: where Qoder writes session JSONL.
pub(crate) fn qoder_projects_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    env_or_default_dir("QODER_CONFIG_DIR", home.join(".qoder")).join("projects")
}

/// Devin CLI's data dir: `$XDG_DATA_HOME/devin/cli`, else
/// `~/.local/share/devin/cli` (also on macOS). Holds `transcripts/`,
/// `sessions.db` and `trusted_workspaces.json`.
pub(crate) fn devin_data_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    env_or_default_dir("XDG_DATA_HOME", home.join(".local").join("share"))
        .join("devin")
        .join("cli")
}

/// Where Devin writes the ATIF transcript of a session.
pub(crate) fn devin_transcript_path(session_id: &str) -> PathBuf {
    devin_data_dir()
        .join("transcripts")
        .join(format!("{session_id}.json"))
}

/// Active OMP profile from the environment. `OMP_PROFILE` is canonical and wins;
/// `PI_PROFILE` (legacy) is consulted only when `OMP_PROFILE` is entirely unset.
/// OMP trims the selected value and treats empty/whitespace and the `"default"`
/// sentinel as the implicit default profile.
pub(crate) fn omp_profile_from_env() -> Option<String> {
    let value = match std::env::var("OMP_PROFILE") {
        Ok(value) => Some(value),
        Err(_) => std::env::var("PI_PROFILE").ok(),
    }?;
    let normalized = value.trim();
    (!normalized.is_empty() && normalized != "default").then(|| normalized.to_string())
}

/// The active OMP session root. Mirrors oh-my-pi's `DirResolver`/`getSessionsDir`:
///
/// - `PI_CONFIG_DIR` replaces the default `.omp` config-root name.
/// - named profiles ignore `PI_CODING_AGENT_DIR`;
/// - a default-profile agent-dir override disables XDG selection;
/// - XDG is selected only on supported platforms and only after the applicable
///   app/profile directory exists;
/// - otherwise sessions remain under the config root.
fn omp_active_session_root() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let config_name = std::env::var("PI_CONFIG_DIR")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".omp".to_string());
    let profile = omp_profile_from_env();
    let config_root = match &profile {
        Some(name) => home.join(&config_name).join("profiles").join(name),
        None => home.join(&config_name),
    };
    let default_agent = config_root.join("agent");
    let agent_override = if profile.is_none() {
        std::env::var("PI_CODING_AGENT_DIR")
            .ok()
            .filter(|value| !value.is_empty())
            .map(|value| {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir().unwrap_or_default().join(path)
                }
            })
    } else {
        None
    };
    let agent_dir = agent_override.unwrap_or_else(|| default_agent.clone());
    let is_default_agent = agent_dir == default_agent;

    // Bun reports `process.platform === "linux"` on Android/Termux, so OMP
    // enables its XDG layout there even though Rust's target_os is `android`.
    if cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "android"
    )) && is_default_agent
        && let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        let app_root = PathBuf::from(xdg).join("omp");
        let candidate = match &profile {
            Some(name) => app_root.join("profiles").join(name),
            None => app_root,
        };
        if candidate.exists() {
            return candidate.join("sessions");
        }
    }

    agent_dir.join("sessions")
}

/// Disk root that holds OMP session files. Single source of truth shared by
/// `transcript search --all` and resume/adoption so the two cannot drift.
///
/// Deliberately **excludes** `PI_CODING_AGENT_SESSION_DIR`: OMP (unlike Pi)
/// never reads that variable, so honoring it here would let OMP claim Pi
/// sessions stored under that override.
pub(crate) fn omp_session_roots() -> Vec<PathBuf> {
    vec![omp_active_session_root()]
}

/// The one session root Pi and OMP genuinely share: `PI_CODING_AGENT_DIR/sessions`
/// (both tools read `PI_CODING_AGENT_DIR`). `None` when the var is unset. A match
/// under this root carries no inherent tool provenance — attribute by the path's
/// product marker only (managed configs carry `.pi`/`.omp`).
pub(crate) fn shared_agent_dir_root() -> Option<PathBuf> {
    std::env::var("PI_CODING_AGENT_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(|dir| {
            let path = PathBuf::from(dir);
            let resolved = if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            };
            resolved.join("sessions")
        })
}

/// The active OMP root when it is not the shared `PI_CODING_AGENT_DIR` root.
pub(crate) fn omp_exclusive_roots() -> Vec<PathBuf> {
    let active = omp_active_session_root();
    if shared_agent_dir_root().as_ref() == Some(&active) {
        Vec::new()
    } else {
        vec![active]
    }
}

/// Pi-exclusive session roots (never read by OMP): `PI_CODING_AGENT_SESSION_DIR`
/// (Pi's `--session-dir` env equivalent, which has precedence per pi `main.ts`)
/// and the default `~/.pi/agent/sessions`. Excludes the shared
/// `PI_CODING_AGENT_DIR`.
pub(crate) fn pi_exclusive_roots() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut roots = Vec::new();
    if let Ok(path) = std::env::var("PI_CODING_AGENT_SESSION_DIR")
        && !path.is_empty()
    {
        roots.push(PathBuf::from(path));
    }
    roots.push(home.join(".pi").join("agent").join("sessions"));
    roots
}

/// Disk roots that hold Pi session files, for `transcript search --all`.
///
/// Pi-exclusive roots plus the shared `PI_CODING_AGENT_DIR/sessions` — the
/// latter only when `PI_CODING_AGENT_SESSION_DIR` is absent, since Pi's
/// session-dir override has precedence over `getAgentDir()/sessions` (pi
/// `main.ts`). hcom-managed Pi sessions (launched with
/// `PI_CODING_AGENT_DIR=<root>/.pi`) live under the shared root, so they must be
/// searched here too; attribution of that root is by path marker / provenance.
pub(crate) fn pi_session_roots() -> Vec<PathBuf> {
    let mut roots = pi_exclusive_roots();
    let session_dir_set = std::env::var("PI_CODING_AGENT_SESSION_DIR")
        .ok()
        .is_some_and(|v| !v.is_empty());
    if !session_dir_set && let Some(shared) = shared_agent_dir_root() {
        roots.push(shared);
    }
    roots
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
        TranscriptDiscovery::QoderProjects => {
            vec![qoder_projects_dir()]
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
        TranscriptDiscovery::PiSessions => pi_session_roots(),
        TranscriptDiscovery::OmpSessions => omp_session_roots(),
        TranscriptDiscovery::GrokSessions => {
            vec![crate::transcript::grok::grok_config_dir().join("sessions")]
        }
        TranscriptDiscovery::DevinTranscripts => vec![devin_data_dir().join("transcripts")],
        TranscriptDiscovery::OpenCodeDatabase
        | TranscriptDiscovery::KiloDatabase
        | TranscriptDiscovery::HermesDatabase => Vec::new(),
    }
}

/// Existing database source for a database-backed transcript profile.
pub(crate) fn database_search_path(tool: Tool) -> Option<PathBuf> {
    match profile_for_tool(tool)?.discovery {
        TranscriptDiscovery::OpenCodeDatabase => opencode::get_opencode_db_path(),
        TranscriptDiscovery::KiloDatabase => opencode::get_kilo_db_path(),
        TranscriptDiscovery::HermesDatabase => hermes::get_hermes_db_path(),
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
        Some(TranscriptDiscovery::HermesDatabase) => {
            hermes::search_hermes_sessions(db_path, pattern, limit)
        }
        _ => Err(format!("Tool '{}' is not database-backed", tool)),
    }
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
            detect_tool_from_path("/h/.qoder/projects/r/u.jsonl"),
            Some(Tool::Qoder)
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
            detect_tool_from_path("/h/.hermes/profiles/work/state.db"),
            Some(Tool::Hermes)
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
            if matches!(tool, Tool::OpenCode | Tool::Kilo | Tool::Hermes) {
                assert!(
                    roots.is_empty(),
                    "database tool {tool} should not expose disk roots"
                );
            } else {
                assert!(!roots.is_empty(), "missing disk discovery roots for {tool}");
            }
        }
    }

    // Unix-only: PI_CODING_AGENT_DIR is set to a Unix-style absolute path
    // (`/tmp/...`), which on Windows has no drive letter and resolves against
    // the current drive, so it never matches the expected `PathBuf`.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn omp_disk_roots_include_pi_coding_agent_dir_sessions() {
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        unsafe {
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
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
    #[serial_test::serial]
    fn omp_disk_roots_use_xdg_data_home_only_on_supported_platforms() {
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        let xdg = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(xdg.path().join("omp")).unwrap();
        unsafe {
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::set_var("XDG_DATA_HOME", xdg.path());
        }

        let roots = disk_search_roots(Tool::Omp);
        if cfg!(any(
            target_os = "linux",
            target_os = "macos",
            target_os = "android"
        )) {
            assert_eq!(roots, vec![xdg.path().join("omp").join("sessions")]);
        } else {
            assert_eq!(
                roots,
                vec![
                    dirs::home_dir()
                        .unwrap_or_default()
                        .join(".omp")
                        .join("agent")
                        .join("sessions")
                ]
            );
        }
    }

    // Unix-only: relies on redirecting the home dir via `isolated_test_env`'s
    // $HOME, but on Windows `dirs::home_dir()` queries the OS profile folder
    // directly and ignores it.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn omp_disk_roots_ignore_uninitialized_xdg_root() {
        let (_dir, _hcom, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let xdg = tempfile::tempdir().unwrap();
        unsafe {
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
            std::env::remove_var("PI_CONFIG_DIR");
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::set_var("XDG_DATA_HOME", xdg.path());
        }

        assert_eq!(
            disk_search_roots(Tool::Omp),
            vec![home.join(".omp").join("agent").join("sessions")]
        );
    }

    // Unix-only: relies on redirecting the home dir via `isolated_test_env`'s
    // $HOME, but on Windows `dirs::home_dir()` queries the OS profile folder
    // directly and ignores it.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn omp_disk_roots_honor_pi_config_dir_and_default_profile_sentinels() {
        let (_dir, _hcom, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        unsafe {
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::set_var("PI_CONFIG_DIR", ".custom-omp");
            std::env::set_var("OMP_PROFILE", "  default  ");
            std::env::set_var("PI_PROFILE", "stale-legacy-profile");
        }

        assert_eq!(omp_profile_from_env(), None);
        assert_eq!(
            disk_search_roots(Tool::Omp),
            vec![home.join(".custom-omp").join("agent").join("sessions")]
        );

        unsafe { std::env::set_var("OMP_PROFILE", "   ") }
        assert_eq!(omp_profile_from_env(), None);
    }

    // Unix-only: relies on redirecting the home dir via `isolated_test_env`'s
    // $HOME, but on Windows `dirs::home_dir()` queries the OS profile folder
    // directly and ignores it.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn omp_agent_override_disables_xdg_and_is_ignored_by_named_profiles() {
        let (_dir, _hcom, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let xdg = tempfile::tempdir().unwrap();
        let agent = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(xdg.path().join("omp")).unwrap();
        unsafe {
            std::env::remove_var("PI_CONFIG_DIR");
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
            std::env::set_var("XDG_DATA_HOME", xdg.path());
            std::env::set_var("PI_CODING_AGENT_DIR", agent.path());
        }

        assert_eq!(
            disk_search_roots(Tool::Omp),
            vec![agent.path().join("sessions")]
        );

        unsafe { std::env::set_var("OMP_PROFILE", " work ") }
        assert_eq!(
            disk_search_roots(Tool::Omp),
            vec![
                home.join(".omp")
                    .join("profiles")
                    .join("work")
                    .join("agent")
                    .join("sessions")
            ]
        );
    }

    #[test]
    #[serial_test::serial]
    fn omp_disk_roots_never_include_pi_session_dir_override() {
        // PI_CODING_AGENT_SESSION_DIR is Pi-exclusive: OMP never reads it, so it
        // must not appear as an OMP search root (else OMP claims Pi sessions).
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        unsafe {
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
            std::env::set_var("PI_CODING_AGENT_SESSION_DIR", "/tmp/test-pi-sessions");
        }

        let omp_roots = disk_search_roots(Tool::Omp);
        assert!(
            !omp_roots
                .iter()
                .any(|r| r == &PathBuf::from("/tmp/test-pi-sessions")),
            "OMP must not own PI_CODING_AGENT_SESSION_DIR, got {omp_roots:?}"
        );
        // Pi still owns it.
        assert!(
            disk_search_roots(Tool::Pi)
                .iter()
                .any(|r| r == &PathBuf::from("/tmp/test-pi-sessions")),
        );
    }

    #[test]
    #[serial_test::serial]
    fn omp_disk_roots_use_named_profile_subtree() {
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        unsafe {
            std::env::remove_var("PI_PROFILE");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::remove_var("PI_CODING_AGENT_DIR");
            std::env::set_var("OMP_PROFILE", "work");
        }
        let home = dirs::home_dir().unwrap_or_default();
        let roots = disk_search_roots(Tool::Omp);
        let expected = home
            .join(".omp")
            .join("profiles")
            .join("work")
            .join("agent")
            .join("sessions");
        assert!(
            roots.iter().any(|r| r == &expected),
            "OMP named-profile root missing, got {roots:?}"
        );
        // The default (profile-less) tree must NOT be searched under a profile.
        assert!(
            !roots
                .iter()
                .any(|r| r == &home.join(".omp").join("agent").join("sessions")),
        );
    }

    #[test]
    #[serial_test::serial]
    fn custom_qoder_config_dir_is_not_mistaken_for_claude() {
        let _guard = crate::hooks::test_helpers::EnvGuard::new();
        let path = "/data/qoder-config/projects/-w/u.jsonl";
        unsafe { std::env::remove_var("QODER_CONFIG_DIR") };
        assert_eq!(detect_tool_from_path(path), Some(Tool::Claude));
        unsafe { std::env::set_var("QODER_CONFIG_DIR", "/data/qoder-config/") };
        assert_eq!(detect_tool_from_path(path), Some(Tool::Qoder));
        // A misconfigured root never takes Claude's own tree.
        unsafe { std::env::set_var("QODER_CONFIG_DIR", "/h/.claude") };
        assert_eq!(
            detect_tool_from_path("/h/.claude/projects/-w/u.jsonl"),
            Some(Tool::Claude)
        );
        // A Qoder root nested under `.claude` is still Qoder's.
        unsafe { std::env::set_var("QODER_CONFIG_DIR", "/h/.claude/qoder") };
        assert_eq!(
            detect_tool_from_path("/h/.claude/qoder/projects/-w/u.jsonl"),
            Some(Tool::Qoder)
        );
        unsafe { std::env::set_var("QODER_CONFIG_DIR", "/data/qoder-config/") };
        // Only its own `projects/` tree, not any path sharing the prefix.
        assert_eq!(
            detect_tool_from_path("/data/qoder-config-2/projects/-w/u.jsonl"),
            Some(Tool::Claude)
        );
    }

    /// Qoder writes Claude-format JSONL plus record types Claude lacks
    /// (`workspace-directories`, `runtime-config`, `worktree-state`,
    /// `active-leaf`, `attachment`, `file-history-snapshot`, `last-prompt`);
    /// they must be skipped, not fail the parse. Shapes taken from qodercli 1.1.65.
    #[test]
    fn qoder_transcript_parses_with_claude_backend_and_skips_extra_records() {
        let lines = [
            r#"{"type":"workspace-directories","sessionId":"s1","directories":["/w"]}"#,
            r#"{"type":"runtime-config","sessionId":"s1","model":"m","timestamp":1}"#,
            r#"{"type":"worktree-state","sessionId":"s1","worktreeSession":null}"#,
            r#"{"type":"user","uuid":"u1","timestamp":"2026-10-03T19:58:59.472Z","message":{"role":"user","content":"Run echo hi, then say ok"},"permissionMode":"yolo","origin":{"kind":"human"},"parentUuid":null,"isSidechain":false,"cwd":"/w","sessionId":"s1"}"#,
            r#"{"type":"attachment","attachment":{"type":"skill_listing","content":"- run: x"},"uuid":"a1","timestamp":"2026-10-03T19:58:59.500Z","parentUuid":"u1","isSidechain":false,"cwd":"/w","sessionId":"s1"}"#,
            r#"{"type":"active-leaf","sessionId":"s1","leafUuid":"u1","explicit":false,"timestamp":2}"#,
            r#"{"type":"assistant","uuid":"as1","timestamp":"2026-10-03T19:59:04.524Z","message":{"id":"c1","type":"message","role":"assistant","model":"m","stop_reason":"tool_use","content":[{"type":"tool_use","id":"call_1","name":"Bash","input":{"command":"echo hi","description":"say hi"}}]},"parentUuid":"u1","isSidechain":false,"cwd":"/w","sessionId":"s1"}"#,
            r#"{"type":"user","uuid":"u2","timestamp":"2026-10-03T19:59:04.626Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"hi"}]},"sourceToolAssistantUUID":"as1","toolUseResult":{"stdout":"hi"},"parentUuid":"as1","isSidechain":false,"cwd":"/w","sessionId":"s1"}"#,
            r#"{"type":"file-history-snapshot","messageId":"u1","snapshot":{"messageId":"u1","trackedFileBackups":{}},"isSnapshotUpdate":false}"#,
            r#"{"type":"assistant","uuid":"as2","timestamp":"2026-10-03T19:59:13.954Z","message":{"id":"c2","type":"message","role":"assistant","model":"m","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]},"parentUuid":"u2","isSidechain":false,"cwd":"/w","sessionId":"s1"}"#,
            r#"{"type":"last-prompt","sessionId":"s1","lastPrompt":"Run echo hi, then say ok"}"#,
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        assert_eq!(
            backend_for_tool(Tool::Qoder),
            Some(TranscriptBackend::ClaudeJsonl)
        );
        let opts = ReadOptions {
            last: 10,
            detailed: true,
            ..Default::default()
        };
        let exchanges = read(&path, TranscriptBackend::ClaudeJsonl, &opts)
            .unwrap()
            .exchanges;
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user, "Run echo hi, then say ok");
        assert!(exchanges[0].action.contains("ok"));
        assert_eq!(exchanges[0].tools.len(), 1);
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use serde_json::{Value, json};

    fn read_fixture(backend: TranscriptBackend, entries: &[Value], last: usize) -> Transcript {
        let file = tempfile::NamedTempFile::new().unwrap();
        let content = if backend == TranscriptBackend::GeminiJson {
            json!({"messages": entries}).to_string()
        } else {
            entries
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        std::fs::write(file.path(), content).unwrap();
        read(
            file.path(),
            backend,
            &ReadOptions {
                last,
                allow_codex_retry: false,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn claude_deduplicates_calls_and_keeps_session_total_with_last() {
        let call = json!({"type":"assistant","message":{"id":"a","model":"haiku","content":[{"type":"text","text":"answer"}],"usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens_details":{"thinking_tokens":2}}}});
        let entries = vec![
            json!({"type":"user","message":{"content":"first"}}),
            call.clone(),
            call.clone(),
            call,
            json!({"type":"user","message":{"content":"second"}}),
            json!({"type":"assistant","message":{"id":"b","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":7,"output_tokens":4}}}),
        ];
        let all = read_fixture(TranscriptBackend::ClaudeJsonl, &entries, 100);
        let last = read_fixture(TranscriptBackend::ClaudeJsonl, &entries, 1);
        assert_eq!(all.usage, last.usage);
        assert_eq!(last.exchanges.len(), 1);
        let usage = all.usage.unwrap();
        assert_eq!(
            usage.tokens,
            TokenUsage {
                input: 17,
                output: 7,
                cache_read: 20,
                cache_write: 5,
                reasoning: Some(2)
            }
        );
        assert_eq!(usage.context_tokens, Some(7));
        assert_eq!(all.exchanges[0].usage.unwrap().input, 10);
        assert_eq!(last.exchanges[0].usage.unwrap().input, 7);
    }

    #[test]
    fn codex_uses_cumulative_deltas_and_disjoint_input() {
        let count = |input, cached, output| json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":input,"cached_input_tokens":cached,"cache_write_input_tokens":2,"output_tokens":output,"reasoning_output_tokens":1},"last_token_usage":{"input_tokens":50,"total_tokens":60},"model_context_window":1000}}});
        let entries = vec![
            count(20, 5, 2),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"first"}}),
            count(100, 30, 10),
            count(100, 30, 10),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"done"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"second"}}),
            count(150, 40, 15),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"finished"}}),
        ];
        let all = read_fixture(TranscriptBackend::CodexJsonl, &entries, 100);
        assert_eq!(
            all.usage,
            read_fixture(TranscriptBackend::CodexJsonl, &entries, 1).usage
        );
        let usage = all.usage.unwrap();
        assert_eq!(usage.tokens.input, 108);
        assert_eq!(usage.tokens.output, 15);
        assert_eq!(usage.context_tokens, Some(60));
        assert_eq!(usage.context_window, Some(1000));
        assert_eq!(all.exchanges[0].usage.unwrap().input, 55);
        assert_eq!(all.exchanges[0].usage.unwrap().output, 8);
        assert_eq!(all.exchanges[1].usage.unwrap().input, 40);
    }

    #[test]
    fn pi_keeps_cost_cache_and_zero_usage() {
        let entries = vec![
            json!({"type":"message","message":{"role":"user","content":"hello"}}),
            json!({"type":"message","message":{"role":"assistant","model":"test","content":"done","usage":{"input":10,"output":8,"cacheRead":20,"cacheWrite":5,"reasoning":3,"cost":{"total":0.25}}}}),
            json!({"type":"message","message":{"role":"assistant","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"cost":{"total":0}}}}),
        ];
        let transcript = read_fixture(TranscriptBackend::PiJsonl, &entries, 1);
        let usage = transcript.usage.unwrap();
        assert_eq!(
            usage.tokens,
            TokenUsage {
                input: 10,
                output: 8,
                cache_read: 20,
                cache_write: 5,
                reasoning: Some(3)
            }
        );
        assert_eq!(usage.cost_usd, Some(0.25));
        assert_eq!(usage.context_tokens, Some(0));
        assert_eq!(transcript.exchanges[0].usage, Some(usage.tokens));
    }

    #[test]
    fn gemini_normalizes_cache_and_thoughts() {
        let entries = vec![
            json!({"type":"user","content":"hello"}),
            json!({"type":"gemini","content":"done","tokens":{"input":30,"cached":20,"output":5,"thoughts":7,"tool":4}}),
        ];
        let transcript = read_fixture(TranscriptBackend::GeminiJson, &entries, 1);
        let usage = transcript.usage.unwrap();
        assert_eq!(
            usage.tokens,
            TokenUsage {
                input: 14,
                output: 12,
                cache_read: 20,
                cache_write: 0,
                reasoning: Some(7)
            }
        );
        assert_eq!(usage.context_tokens, Some(30));
        assert_eq!(transcript.exchanges[0].usage, Some(usage.tokens));
    }

    #[test]
    fn codex_prefers_per_response_records_over_totals() {
        // Thread totals also count an earlier rollout segment (1000 input).
        let record = |id: &str, input, thread| json!({"type":"token_usage_record","payload":{"response_id":id,"usage":{"input_tokens":input,"cached_input_tokens":10,"output_tokens":2},"thread_token_usage":{"input_tokens":thread,"cached_input_tokens":20,"output_tokens":4}}});
        // Totals miss the compaction call; records include it.
        let count = json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":2},"last_token_usage":{"total_tokens":80},"model_context_window":1000}}});
        let entries = vec![
            json!({"type":"event_msg","payload":{"type":"user_message","message":"first"}}),
            record("r1", 100, 1100),
            count,
            record("compact", 50, 1150),
            record("compact", 50, 1150),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"done"}}),
        ];
        let t = read_fixture(TranscriptBackend::CodexJsonl, &entries, 100);
        let usage = t.usage.unwrap();
        assert_eq!(usage.tokens.input, 1130);
        assert_eq!(usage.tokens.cache_read, 20);
        assert_eq!(usage.tokens.output, 4);
        assert_eq!(usage.context_tokens, Some(80));
        assert_eq!(usage.context_window, Some(1000));
        let exchange = t.exchanges[0].usage.unwrap();
        assert_eq!((exchange.input, exchange.cache_read), (130, 20));
    }

    #[test]
    fn codex_keeps_window_and_skips_zero_delta_exchanges() {
        let count = |input, window: Option<u64>| {
            let mut info = json!({"total_token_usage":{"input_tokens":input,"output_tokens":1},"last_token_usage":{"total_tokens":input}});
            if let Some(window) = window {
                info["model_context_window"] = json!(window);
            }
            json!({"type":"event_msg","payload":{"type":"token_count","info":info}})
        };
        let entries = vec![
            json!({"type":"event_msg","payload":{"type":"user_message","message":"first"}}),
            count(100, Some(1000)),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"done"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"second"}}),
            count(100, None),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"again"}}),
        ];
        let t = read_fixture(TranscriptBackend::CodexJsonl, &entries, 100);
        let usage = t.usage.unwrap();
        assert_eq!(usage.context_window, Some(1000));
        assert_eq!(usage.context_tokens, Some(100));
        assert!(t.exchanges[0].usage.is_some());
        assert!(
            t.exchanges[1].usage.is_none(),
            "unchanged totals add no exchange usage"
        );
    }

    #[test]
    fn claude_ratio_call_replaces_earlier_token_context() {
        let entries = vec![
            json!({"type":"user","message":{"content":"hello"}}),
            json!({"type":"assistant","message":{"id":"a","content":[{"type":"text","text":"x"}],"usage":{"input_tokens":50,"output_tokens":1}}}),
            json!({"type":"assistant","message":{"id":"b","content":[{"type":"text","text":"y"}],"usage":{"input_tokens":0,"output_tokens":0,"context_usage_ratio":0.4}}}),
        ];
        let usage = read_fixture(TranscriptBackend::ClaudeJsonl, &entries, 1)
            .usage
            .unwrap();
        assert_eq!(usage.context_tokens, None);
        assert_eq!(format_context(&usage).as_deref(), Some("40%"));
    }

    #[test]
    fn missing_usage_is_none() {
        for backend in [
            TranscriptBackend::ClaudeJsonl,
            TranscriptBackend::CodexJsonl,
            TranscriptBackend::PiJsonl,
            TranscriptBackend::GeminiJson,
        ] {
            let entries = match backend {
                TranscriptBackend::ClaudeJsonl => {
                    vec![json!({"type":"user","message":{"content":"hello"}})]
                }
                TranscriptBackend::CodexJsonl => vec![
                    json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
                ],
                TranscriptBackend::PiJsonl => {
                    vec![json!({"type":"message","message":{"role":"user","content":"hello"}})]
                }
                _ => vec![json!({"type":"user","content":"hello"})],
            };
            let transcript = read_fixture(backend, &entries, 1);
            assert!(transcript.usage.is_none());
            assert!(transcript.exchanges[0].usage.is_none());
        }
    }
}
