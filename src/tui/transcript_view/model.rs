//! Display model for the conversation tab: what a turn looks like once the
//! transcript is parsed, and which agent the cursor points the tab at.
use std::path::PathBuf;
use std::time::Instant;

use crate::transcript::{self, TranscriptBackend};
use crate::tui::model::{Agent, CursorTarget};
use crate::tui::state::DataState;

/// Right-panel tab in vertical view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RightTab {
    #[default]
    Activity,
    Conversation,
}

/// One turn, already cut down for display; the raw transcript is not kept.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnView {
    pub position: usize,
    /// "HH:MM", or "--:--" when the timestamp can't be read.
    pub time: String,
    /// Up to the first three non-empty request lines.
    pub request_lines: Vec<String>,
    pub request_more: bool,
    pub request_summary: String,
    /// The last non-empty lines of the assistant text.
    pub response_tail: Vec<String>,
    pub response_omitted: usize,
    pub response_summary: String,
    pub tools: ToolSummary,
    /// Whole request and response, for the full-text view.
    pub request_full: Vec<String>,
    pub response_full: Vec<String>,
    /// Turn start, Unix seconds.
    pub started_at: Option<f64>,
    /// The request was only the `<hcom>` wake trigger; the message itself
    /// reached the agent through a hook.
    pub hcom_wake: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolSummary {
    /// The source didn't say which tools ran (remote reply without `tools`).
    Unknown,
    /// No tool ran this turn.
    None,
    Some {
        /// Most used first; ties by name.
        counts: Vec<(String, usize)>,
        files: Vec<String>,
        extra_files: usize,
    },
}

impl ToolSummary {
    pub fn total(&self) -> Option<usize> {
        match self {
            Self::Some { counts, .. } => Some(counts.iter().map(|c| c.1).sum()),
            Self::None => Some(0),
            Self::Unknown => None,
        }
    }
}

/// A local transcript and the parser for it. Also the local cache key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalSource {
    pub name: String,
    pub path: PathBuf,
    pub session: Option<String>,
    pub backend: TranscriptBackend,
}

impl LocalSource {
    pub fn for_agent(agent: &Agent) -> Option<Self> {
        if agent.is_remote() {
            return None;
        }
        Some(Self {
            name: agent.name.clone(),
            path: agent.transcript_path.as_ref()?.into(),
            session: agent.session_id.clone(),
            backend: transcript::backend_for_tool(agent.tool.canonical()?)?,
        })
    }
}

/// What the conversation tab shows for the row under the cursor.
pub enum ConversationTarget<'a> {
    NoAgent,
    /// The tool has no transcript parser.
    Unsupported(&'a Agent),
    /// Supported tool, but no transcript path recorded yet.
    LocalNoPath(&'a Agent),
    Local {
        agent: &'a Agent,
    },
    Remote {
        agent: &'a Agent,
    },
}

pub fn resolve_target(data: &DataState, cursor: CursorTarget) -> ConversationTarget<'_> {
    let agent = match cursor {
        CursorTarget::Agent(i) => data.agents.get(i),
        CursorTarget::StoppedAgent(i) => data.stopped_agents.get(i),
        CursorTarget::RemoteAgent(i) => data.remote_agents.get(i),
        _ => None,
    };
    let Some(agent) = agent else {
        return ConversationTarget::NoAgent;
    };
    let supported = agent
        .tool
        .canonical()
        .is_some_and(|tool| transcript::backend_for_tool(tool).is_some());
    if !supported {
        return ConversationTarget::Unsupported(agent);
    }
    if agent.is_remote() {
        return ConversationTarget::Remote { agent };
    }
    match LocalSource::for_agent(agent) {
        Some(_) => ConversationTarget::Local { agent },
        None => ConversationTarget::LocalNoPath(agent),
    }
}

/// What the renderer reads from a worker.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConversationSnapshot<'a> {
    /// `None` until a read has succeeded.
    pub turns: Option<&'a [TurnView]>,
    pub last_success: Option<Instant>,
    /// Why the latest refresh failed; cleared by the next success.
    pub failure: Option<&'a str>,
    pub loading: bool,
    /// Logged-in account; only the local worker knows it.
    pub email: Option<&'a str>,
    /// A choice prompt waiting on the agent's screen; local Claude only.
    pub prompt: Option<&'a super::prompt::ScreenPrompt>,
}

pub fn parse_turn_epoch(ts: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp_millis() as f64 / 1000.0)
}

pub fn format_turn_time(ts: &str) -> String {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) {
        return t.with_timezone(&chrono::Local).format("%H:%M").to_string();
    }
    if ts.len() >= 16 && ts.as_bytes()[10] == b'T' && ts.is_char_boundary(16) {
        return ts[11..16].to_string();
    }
    "--:--".to_string()
}

/// Key for a remote agent: its DB row name, `base:DEVICE`.
pub fn remote_account_key(agent: &Agent) -> Option<String> {
    agent
        .device_name
        .as_ref()
        .map(|d| format!("{}:{}", agent.name, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_time_falls_back() {
        assert_eq!(format_turn_time("garbage"), "--:--");
        assert_eq!(format_turn_time(""), "--:--");
        assert_eq!(format_turn_time("2026-10-22T16:02:11"), "16:02");
    }

    #[test]
    fn tool_total() {
        assert_eq!(ToolSummary::Unknown.total(), None);
        assert_eq!(ToolSummary::None.total(), Some(0));
        let s = ToolSummary::Some {
            counts: vec![("Read".into(), 4), ("Edit".into(), 2)],
            files: vec![],
            extra_files: 0,
        };
        assert_eq!(s.total(), Some(6));
    }
}
