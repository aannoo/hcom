use std::collections::BTreeSet;

use crate::tui::model::{
    Agent, CommandResult, Event, Flash, InputMode, LaunchState, Message, OrphanProcess, Overlay,
    RelayPopupState, ViewMode,
};

#[derive(Clone)]
pub struct DataState {
    pub agents: Vec<Agent>,
    pub remote_agents: Vec<Agent>,
    pub stopped_agents: Vec<Agent>,
    pub orphans: Vec<OrphanProcess>,
    pub messages: Vec<Message>,
    pub events: Vec<Event>,
    pub relay_enabled: bool,
    /// Canonical effective relay state. All UI render branches should switch
    /// on this rather than on `relay_enabled` + raw KV to avoid the false-green
    /// / disabled-but-showing-ok class of bugs. Raw underlying signals (status
    /// KV, last_error, heartbeat age, pid) are intentionally not held here —
    /// they're only meaningful via the derivation, and the JSON output exposes
    /// them under `raw` for forensics.
    pub relay_health: crate::relay::RelayHealth,
    /// FTS search results. `Some` when a text search query is active (both inline and vertical modes).
    pub search_results: Option<(Vec<Message>, Vec<Event>)>,
}

impl DataState {
    pub fn empty() -> Self {
        Self {
            agents: vec![],
            remote_agents: vec![],
            stopped_agents: vec![],
            orphans: vec![],
            messages: vec![],
            events: vec![],
            relay_enabled: false,
            relay_health: crate::relay::RelayHealth::NotConfigured,
            search_results: None,
        }
    }

    /// Resolve a raw agent name to its tag-name display format.
    /// Falls back to the raw name if no matching agent is found.
    pub fn resolve_display_name(&self, name: &str) -> String {
        self.agents
            .iter()
            .chain(self.remote_agents.iter())
            .chain(self.stopped_agents.iter())
            .find(|a| a.name == name)
            .map(|a| a.display_name())
            .unwrap_or_else(|| name.to_string())
    }
}

pub struct UiState {
    pub cursor: usize,
    pub cursor_name: Option<String>,
    pub selected: BTreeSet<String>,
    pub input: String,
    pub input_cursor: usize,
    /// First visible line in multi-line compose input.
    pub input_scroll: usize,
    pub flash: Option<Flash>,
    pub tick: u64,
    pub launch: LaunchState,
    pub should_quit: bool,
    pub switch_viewport: bool,
    pub msg_scroll: usize,
    pub scroll_max: usize,
    pub search_filter: Option<String>,
    pub help_open: bool,
    pub help_scroll: u16,
    pub confirm: Option<Confirm>,
    pub mode: InputMode,
    pub command_result: Option<CommandResult>,
    pub view_mode: ViewMode,
    pub relay_popup: Option<RelayPopupState>,
    pub relay_text_until: Option<std::time::Instant>,
    /// Whether the last observed snapshot's relay health was Connected.
    /// Drives the "relay connected" flash on the not-Connected → Connected
    /// edge. Started as false (we haven't seen any snapshot yet, so the
    /// first Connected snapshot triggers the flash, which is what we want).
    pub last_relay_was_connected: bool,
    pub remote_expanded: bool,
    pub stopped_expanded: bool,
    pub show_all_stopped: bool,
    pub show_events: bool,
    pub orphans_expanded: bool,
    pub eject_filter: Option<BTreeSet<String>>,
    pub inline_filter_changed: bool,
    pub needs_resize: bool,
    pub needs_clear_replay: bool,
    pub overlay: Option<Overlay>,
    pub pending_eject_cmd: bool,
    /// Terminal width, updated each render frame. Used by input handlers for wrap calculations.
    pub term_width: u16,
    /// Filter agents by project name. None = show all.
    pub project_filter: Option<String>,
}

impl UiState {
    /// Set flags to trigger an inline scrollback replay (filter/search change).
    /// No-op in vertical mode.
    pub fn trigger_inline_replay(&mut self) {
        if self.view_mode == ViewMode::Inline {
            self.inline_filter_changed = true;
            self.needs_resize = true;
            self.needs_clear_replay = false;
        }
    }
}

pub struct Confirm {
    pub text: String,
    pub action: ConfirmAction,
    pub selected: bool, // true = yes
    pub expires_at: std::time::Instant,
}

impl Confirm {
    pub fn new(text: String, action: ConfirmAction, default_yes: bool) -> Self {
        Self {
            text,
            action,
            selected: default_yes,
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(10),
        }
    }

    pub fn is_expired(&self) -> bool {
        std::time::Instant::now() >= self.expires_at
    }
}

pub enum ConfirmAction {
    KillAgents(Vec<String>),
    ForkAgents(Vec<String>),
    KillOrphan(u32),
    /// Orphan chooser: selected=false → Kill, selected=true → Recover
    OrphanAction(u32),
}
