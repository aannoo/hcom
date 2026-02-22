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
    pub relay_status: Option<String>,
    pub relay_error: Option<String>,
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
            relay_status: None,
            relay_error: None,
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
    /// Previous relay_status for transition detection (None = never seen)
    pub last_relay_status: Option<String>,
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
