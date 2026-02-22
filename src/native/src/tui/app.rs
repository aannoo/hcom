use std::collections::BTreeSet;
use std::io::Stdout;
use std::time::Duration;

use color_eyre::Result;
use crossterm::event::{self, Event as CrosstermEvent, KeyEventKind};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::tui::data::{self, DataSource};
use crate::tui::inline::eject::Ejector;
use crate::tui::model::*;
use crate::tui::render;
use crate::tui::rpc_async::{RpcClient, RpcOp};
pub use crate::tui::state::{Confirm, ConfirmAction, DataState, UiState};

pub struct App {
    pub data: DataState,
    pub ui: UiState,
    pub ejector: Ejector,
    pub(crate) source: Box<dyn DataSource>,
    pub(crate) rpc_client: Option<RpcClient>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let mut source = data::create_data_source();
        let data = source.load();
        let rpc_client = Some(RpcClient::start());

        Self {
            data,
            source,
            rpc_client,
            ejector: Ejector::new(),
            ui: UiState {
                cursor: 0,
                cursor_name: None,
                selected: BTreeSet::new(),
                input: String::new(),
                input_cursor: 0,
                flash: None,
                tick: 0,
                launch: LaunchState::new(),
                should_quit: false,
                switch_viewport: false,
                msg_scroll: 0,
                scroll_max: 0,
                search_filter: None,
                help_open: false,
                help_scroll: 0,
                confirm: None,
                mode: InputMode::Navigate,
                command_result: None,
                view_mode: ViewMode::Inline,
                relay_popup: None,
                relay_text_until: None,
                last_relay_status: None,
                remote_expanded: false,
                stopped_expanded: false,
                show_all_stopped: false,
                show_events: false,
                orphans_expanded: false,
                eject_filter: None,
                inline_filter_changed: false,
                needs_resize: false,
                needs_clear_replay: false,
                overlay: None,
                pending_eject_cmd: false,
            },
        }
    }

    /// Active search query: overlay live input or persisted filter.
    pub fn active_search_query(&self) -> Option<&str> {
        if let Some(ref overlay) = self.ui.overlay {
            if overlay.kind == OverlayKind::Search && !overlay.input.is_empty() {
                return Some(overlay.input.as_str());
            }
        }
        self.ui.search_filter.as_deref()
    }

    pub fn backend_error(&self) -> Option<String> {
        self.source.last_error()
    }

    pub fn pending_rpc_count(&self) -> usize {
        self.rpc_client
            .as_ref()
            .map(RpcClient::pending_count)
            .unwrap_or(0)
    }

    pub fn enqueue_rpc(&mut self, op: RpcOp) -> Result<(), String> {
        match self.rpc_client.as_mut() {
            Some(client) => client.submit(op),
            None => Err("rpc client unavailable".into()),
        }
    }

    pub fn drain_rpc_results(&mut self) -> bool {
        let results = match self.rpc_client.as_mut() {
            Some(client) => client.drain(),
            None => return false,
        };
        if results.is_empty() {
            return false;
        }
        for result in results {
            self.apply_rpc_result(result);
        }
        true
    }

    pub fn cursor_target(&self) -> CursorTarget {
        let live = self.data.agents.len();
        if self.ui.cursor < live {
            return CursorTarget::Agent(self.ui.cursor);
        }
        let mut offset = live;

        if !self.data.remote_agents.is_empty() {
            if self.ui.cursor == offset {
                return CursorTarget::RemoteHeader;
            }
            offset += 1;
            if self.ui.remote_expanded {
                if let Some(idx) = self.ui.cursor.checked_sub(offset) {
                    if idx < self.data.remote_agents.len() {
                        return CursorTarget::RemoteAgent(idx);
                    }
                }
                offset += self.data.remote_agents.len();
            }
        }

        if self.ui.view_mode != ViewMode::Inline && !self.data.stopped_agents.is_empty() {
            if self.ui.cursor == offset {
                return CursorTarget::StoppedHeader;
            }
            offset += 1;
            if self.ui.stopped_expanded {
                if let Some(idx) = self.ui.cursor.checked_sub(offset) {
                    if idx < self.data.stopped_agents.len() {
                        return CursorTarget::StoppedAgent(idx);
                    }
                }
                offset += self.data.stopped_agents.len();
            }
        }

        if !self.data.orphans.is_empty() {
            if self.ui.cursor == offset {
                return CursorTarget::OrphanHeader;
            }
            offset += 1;
            if self.ui.orphans_expanded {
                if let Some(idx) = self.ui.cursor.checked_sub(offset) {
                    if idx < self.data.orphans.len() {
                        return CursorTarget::Orphan(idx);
                    }
                }
            }
        }

        CursorTarget::None
    }

    pub fn total_visible_rows(&self) -> usize {
        let mut n = self.data.agents.len();
        if !self.data.remote_agents.is_empty() {
            n += 1;
            if self.ui.remote_expanded {
                n += self.data.remote_agents.len();
            }
        }
        if self.ui.view_mode != ViewMode::Inline && !self.data.stopped_agents.is_empty() {
            n += 1;
            if self.ui.stopped_expanded {
                n += self.data.stopped_agents.len();
            }
        }
        if !self.data.orphans.is_empty() {
            n += 1;
            if self.ui.orphans_expanded {
                n += self.data.orphans.len();
            }
        }
        n
    }

    /// Combined run loop: works with both inline and fullscreen viewports.
    /// Returns when the user quits or requests a viewport switch.
    pub fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        viewport_height: u16,
    ) -> Result<()> {
        let is_inline = self.ui.view_mode == ViewMode::Inline;
        let mut dirty = true;
        let mut last_reload = std::time::Instant::now();
        let mut resize_cooldown: u8 = 0;

        loop {
            if self.drain_rpc_results() {
                dirty = true;
            }

            // Break before draw to prevent autoresize (its append_lines
            // corrupts scrollback for inline viewports).
            if self.ui.needs_resize || self.ui.should_quit || self.ui.switch_viewport {
                break;
            }

            if dirty && resize_cooldown == 0 {
                if is_inline {
                    crossterm::execute!(std::io::stdout(), BeginSynchronizedUpdate)?;
                    if self.ui.needs_clear_replay {
                        self.ui.needs_clear_replay = false;
                        super::clear_for_resize(viewport_height)?;
                        // We externally cleared terminal content; force a full viewport repaint.
                        terminal.clear()?;
                    }
                    if self.ui.inline_filter_changed {
                        self.ui.inline_filter_changed = false;
                        self.update_search();
                        self.ejector.begin_replay(
                            &self.data,
                            &self.ui.eject_filter,
                            &self.ui.search_filter,
                            crate::tui::inline::eject::ReplayReason::Resize,
                        );
                    }
                    if self.ui.pending_eject_cmd {
                        self.ui.pending_eject_cmd = false;
                        if let Some(ref cr) = self.ui.command_result {
                            self.ejector
                                .eject_command_output(&cr.label, &cr.output, terminal)?;
                        }
                        self.ui.command_result = None;
                    }
                    self.ejector.eject_new(
                        &self.data,
                        &self.ui.eject_filter,
                        &self.ui.search_filter,
                        terminal,
                    )?;
                }

                // Clamp scroll before draw so the render never sees an out-of-bounds value.
                self.ui.msg_scroll = self.ui.msg_scroll.min(self.ui.scroll_max);
                terminal.draw(|frame| render::render(frame, &mut *self))?;

                if is_inline {
                    crossterm::execute!(std::io::stdout(), EndSynchronizedUpdate)?;
                }
                dirty = false;
            }

            // Adaptive poll: 80ms when animating, 500ms when idle
            let rpc_pending = self.pending_rpc_count() > 0;
            let animating = self.ui.flash.is_some()
                || self.ui.confirm.is_some()
                || self
                    .data
                    .agents
                    .iter()
                    .any(|a| a.status == AgentStatus::Launching)
                || self.ejector.is_replaying()
                || rpc_pending;
            let poll_ms = if animating || resize_cooldown > 0 {
                80
            } else {
                500
            };

            if event::poll(Duration::from_millis(poll_ms))? {
                loop {
                    match event::read()? {
                        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                            self.handle_key(key.code, key.modifiers);
                            dirty = true;
                        }
                        CrosstermEvent::Paste(text) => {
                            self.handle_paste(&text);
                            dirty = true;
                        }
                        CrosstermEvent::Mouse(mouse) if !is_inline => {
                            self.handle_mouse(mouse.kind);
                            dirty = true;
                        }
                        CrosstermEvent::Resize(_, _) if is_inline => {
                            resize_cooldown = 2;
                        }
                        _ => {}
                    }
                    if !event::poll(Duration::from_millis(0))? {
                        break;
                    }
                }
            }

            // Debounce resize: wait 2 ticks (160ms) after last resize event
            // before breaking. Suppresses draw during cooldown to prevent
            // autoresize corruption, then breaks to destroy/recreate terminal.
            if resize_cooldown > 0 {
                resize_cooldown -= 1;
                if resize_cooldown == 0 {
                    self.ui.needs_resize = true;
                    // breaks at top of next iteration, before draw
                }
            }

            self.ui.tick += 1;

            // Animations need redraws for spinner frames etc.
            if animating {
                dirty = true;
            }

            // Data reload — refresh faster while launching/pending RPC, slower when idle.
            let reload_ms = if animating { 120 } else { 350 };
            if last_reload.elapsed() >= Duration::from_millis(reload_ms) {
                self.reload_data();
                last_reload = std::time::Instant::now();
                dirty = true;
            }

            if self.ui.flash.as_ref().is_some_and(|f| f.is_expired()) {
                self.ui.flash = None;
                dirty = true;
            }

            if self.ui.confirm.as_ref().is_some_and(|c| c.is_expired()) {
                self.ui.confirm = None;
                dirty = true;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::{Agent, AgentStatus, OrphanProcess, Tool};

    fn dummy_agent(name: &str) -> Agent {
        Agent {
            name: name.into(),
            tool: Tool::Claude,
            status: AgentStatus::Active,
            status_context: String::new(),
            status_detail: String::new(),
            created_at: 0.0,
            status_time: 0.0,
            last_heartbeat: 0.0,
            has_tcp: true,
            directory: String::new(),
            tag: String::new(),
            unread: 0,
            last_event_id: None,
            device_name: None,
            sync_age: None,
            headless: false,
            session_id: None,
            pid: None,
            terminal_preset: None,
        }
    }

    fn dummy_orphan(pid: u32) -> OrphanProcess {
        OrphanProcess {
            pid,
            tool: Tool::Claude,
            names: vec![],
            launched_at: 0.0,
            directory: String::new(),
        }
    }

    struct NullSource;
    impl DataSource for NullSource {
        fn load(&mut self) -> DataState {
            DataState::empty()
        }
        fn load_all_stopped(&mut self) -> Vec<Agent> {
            vec![]
        }
    }

    fn test_app() -> App {
        App {
            data: DataState::empty(),
            ui: UiState {
                cursor: 0,
                cursor_name: None,
                selected: BTreeSet::new(),
                input: String::new(),
                input_cursor: 0,
                flash: None,
                tick: 0,
                launch: LaunchState {
                    tool: Tool::Claude,
                    count: 1,
                    options_cursor: None,
                    tag: String::new(),
                    headless: false,
                    terminal: 0,
                    terminal_presets: vec!["default".into()],
                    editing: None,
                    edit_cursor: 0,
                    edit_snapshot: None,
                },
                should_quit: false,
                switch_viewport: false,
                msg_scroll: 0,
                scroll_max: 0,
                search_filter: None,
                help_open: false,
                help_scroll: 0,
                confirm: None,
                mode: InputMode::Navigate,
                command_result: None,
                view_mode: ViewMode::Vertical,
                relay_popup: None,
                relay_text_until: None,
                last_relay_status: None,
                remote_expanded: false,
                stopped_expanded: false,
                show_all_stopped: false,
                show_events: false,
                orphans_expanded: false,
                eject_filter: None,
                inline_filter_changed: false,
                needs_resize: false,
                needs_clear_replay: false,
                overlay: None,
                pending_eject_cmd: false,
            },
            ejector: Ejector::new(),
            source: Box::new(NullSource),
            rpc_client: None,
        }
    }

    // ── cursor_target: agents only ────────────────────────────────

    #[test]
    fn cursor_target_agents_only() {
        let mut app = test_app();
        app.data.agents = vec![dummy_agent("a"), dummy_agent("b")];
        app.ui.cursor = 0;
        assert_eq!(app.cursor_target(), CursorTarget::Agent(0));
        app.ui.cursor = 1;
        assert_eq!(app.cursor_target(), CursorTarget::Agent(1));
    }

    #[test]
    fn cursor_target_past_all_agents_is_none() {
        let mut app = test_app();
        app.data.agents = vec![dummy_agent("a")];
        app.ui.cursor = 1;
        assert_eq!(app.cursor_target(), CursorTarget::None);
    }

    // ── cursor_target: with remote agents ─────────────────────────

    #[test]
    fn cursor_target_remote_header() {
        let mut app = test_app();
        app.data.agents = vec![dummy_agent("a")];
        let mut remote = dummy_agent("r");
        remote.device_name = Some("BOX".into());
        app.data.remote_agents = vec![remote];
        app.ui.cursor = 1; // after local agents
        assert_eq!(app.cursor_target(), CursorTarget::RemoteHeader);
    }

    #[test]
    fn cursor_target_remote_agent_expanded() {
        let mut app = test_app();
        app.data.agents = vec![dummy_agent("a")];
        let mut remote = dummy_agent("r");
        remote.device_name = Some("BOX".into());
        app.data.remote_agents = vec![remote];
        app.ui.remote_expanded = true;
        app.ui.cursor = 2; // header=1, first remote=2
        assert_eq!(app.cursor_target(), CursorTarget::RemoteAgent(0));
    }

    #[test]
    fn cursor_target_remote_collapsed_skips_agents() {
        let mut app = test_app();
        app.data.agents = vec![dummy_agent("a")];
        let mut remote = dummy_agent("r");
        remote.device_name = Some("BOX".into());
        app.data.remote_agents = vec![remote];
        app.ui.remote_expanded = false;
        app.ui.cursor = 2; // past header, but collapsed
        assert_eq!(app.cursor_target(), CursorTarget::None);
    }

    // ── cursor_target: with stopped agents ────────────────────────

    #[test]
    fn cursor_target_stopped_header_vertical() {
        let mut app = test_app();
        app.ui.view_mode = ViewMode::Vertical;
        app.data.agents = vec![dummy_agent("a")];
        app.data.stopped_agents = vec![dummy_agent("s")];
        app.ui.cursor = 1;
        assert_eq!(app.cursor_target(), CursorTarget::StoppedHeader);
    }

    #[test]
    fn cursor_target_stopped_hidden_in_inline() {
        let mut app = test_app();
        app.ui.view_mode = ViewMode::Inline;
        app.data.agents = vec![dummy_agent("a")];
        app.data.stopped_agents = vec![dummy_agent("s")];
        app.ui.cursor = 1;
        // Inline mode skips stopped section entirely
        assert_eq!(app.cursor_target(), CursorTarget::None);
    }

    // ── cursor_target: with orphans ───────────────────────────────

    #[test]
    fn cursor_target_orphan_header() {
        let mut app = test_app();
        app.data.agents = vec![dummy_agent("a")];
        app.data.orphans = vec![dummy_orphan(42)];
        app.ui.cursor = 1;
        assert_eq!(app.cursor_target(), CursorTarget::OrphanHeader);
    }

    #[test]
    fn cursor_target_orphan_expanded() {
        let mut app = test_app();
        app.data.agents = vec![dummy_agent("a")];
        app.data.orphans = vec![dummy_orphan(42)];
        app.ui.orphans_expanded = true;
        app.ui.cursor = 2; // header=1, first orphan=2
        assert_eq!(app.cursor_target(), CursorTarget::Orphan(0));
    }

    // ── cursor_target: all sections combined ──────────────────────

    #[test]
    fn cursor_target_all_sections_expanded() {
        let mut app = test_app();
        app.ui.view_mode = ViewMode::Vertical;
        app.data.agents = vec![dummy_agent("a"), dummy_agent("b")]; // 0, 1
        let mut remote = dummy_agent("r");
        remote.device_name = Some("BOX".into());
        app.data.remote_agents = vec![remote]; // header=2, agent=3
        app.data.stopped_agents = vec![dummy_agent("s")]; // header=4, agent=5
        app.data.orphans = vec![dummy_orphan(42)]; // header=6, orphan=7
        app.ui.remote_expanded = true;
        app.ui.stopped_expanded = true;
        app.ui.orphans_expanded = true;

        app.ui.cursor = 0;
        assert_eq!(app.cursor_target(), CursorTarget::Agent(0));
        app.ui.cursor = 1;
        assert_eq!(app.cursor_target(), CursorTarget::Agent(1));
        app.ui.cursor = 2;
        assert_eq!(app.cursor_target(), CursorTarget::RemoteHeader);
        app.ui.cursor = 3;
        assert_eq!(app.cursor_target(), CursorTarget::RemoteAgent(0));
        app.ui.cursor = 4;
        assert_eq!(app.cursor_target(), CursorTarget::StoppedHeader);
        app.ui.cursor = 5;
        assert_eq!(app.cursor_target(), CursorTarget::StoppedAgent(0));
        app.ui.cursor = 6;
        assert_eq!(app.cursor_target(), CursorTarget::OrphanHeader);
        app.ui.cursor = 7;
        assert_eq!(app.cursor_target(), CursorTarget::Orphan(0));
        app.ui.cursor = 8;
        assert_eq!(app.cursor_target(), CursorTarget::None);
    }

    // ── total_visible_rows ────────────────────────────────────────

    #[test]
    fn total_visible_rows_empty() {
        let app = test_app();
        assert_eq!(app.total_visible_rows(), 0);
    }

    #[test]
    fn total_visible_rows_all_expanded() {
        let mut app = test_app();
        app.ui.view_mode = ViewMode::Vertical;
        app.data.agents = vec![dummy_agent("a"), dummy_agent("b")];
        let mut remote = dummy_agent("r");
        remote.device_name = Some("BOX".into());
        app.data.remote_agents = vec![remote];
        app.data.stopped_agents = vec![dummy_agent("s")];
        app.data.orphans = vec![dummy_orphan(42)];
        app.ui.remote_expanded = true;
        app.ui.stopped_expanded = true;
        app.ui.orphans_expanded = true;
        // 2 agents + 1 remote header + 1 remote + 1 stopped header + 1 stopped + 1 orphan header + 1 orphan
        assert_eq!(app.total_visible_rows(), 8);
    }

    #[test]
    fn total_visible_rows_all_collapsed() {
        let mut app = test_app();
        app.ui.view_mode = ViewMode::Vertical;
        app.data.agents = vec![dummy_agent("a"), dummy_agent("b")];
        let mut remote = dummy_agent("r");
        remote.device_name = Some("BOX".into());
        app.data.remote_agents = vec![remote];
        app.data.stopped_agents = vec![dummy_agent("s")];
        app.data.orphans = vec![dummy_orphan(42)];
        // 2 agents + 1 remote header + 1 stopped header + 1 orphan header
        assert_eq!(app.total_visible_rows(), 5);
    }
}
