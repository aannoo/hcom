//! Conversation tab in vertical view: the selected agent's recent turns
//! (request, tools used, response), local or over the relay.
pub mod convert;
pub mod local;
pub mod markdown;
pub mod model;
pub mod remote;
pub mod render;

use std::collections::HashMap;
use std::time::Instant;

pub use model::*;

use crate::tui::app::App;
use crate::tui::model::{Agent, ViewMode};

#[derive(Default)]
pub struct TranscriptView {
    pub tab: RightTab,
    /// Tab shown on entering vertical view (`HCOM_TUI_TAB`).
    pub default_tab: RightTab,
    /// `c` pressed in inline view: open the tab once vertical view starts.
    pub open_on_switch: bool,
    /// Per agent: full-text view (every turn, whole text).
    pub expanded: HashMap<String, bool>,
    pub local: local::LocalTranscriptWorker,
    pub remote: remote::RemoteTranscriptWorker,
}

/// `HCOM_TUI_TAB=chat` opens vertical view on the conversation tab.
pub fn default_tab_from_env(value: Option<&str>) -> RightTab {
    match value.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("chat") || v.eq_ignore_ascii_case("conversation") => {
            RightTab::Conversation
        }
        _ => RightTab::Activity,
    }
}

impl TranscriptView {
    pub fn on_enter_vertical(&mut self) {
        self.tab = if self.open_on_switch {
            RightTab::Conversation
        } else {
            self.default_tab
        };
        self.open_on_switch = false;
    }

    pub fn is_expanded(&self, agent: &Agent) -> bool {
        self.expanded
            .get(&agent.display_name())
            .copied()
            .unwrap_or(false)
    }

    pub fn has_pending_timer(&self) -> bool {
        self.local.has_pending_timer() || self.remote.has_pending_timer()
    }
}

impl App {
    pub(crate) fn conversation_visible(&self) -> bool {
        self.ui.view_mode == ViewMode::Vertical && self.transcript.tab == RightTab::Conversation
    }

    /// The conversation tab shows a working agent: keep its spinner moving.
    pub(crate) fn transcript_spinning(&self) -> bool {
        self.conversation_visible()
            && matches!(
                resolve_target(&self.data, self.cursor_target()),
                ConversationTarget::Local { agent, .. } | ConversationTarget::Remote { agent, .. }
                    if agent.status == crate::tui::model::AgentStatus::Active
            )
    }

    pub(crate) fn transcript_poll(&mut self, now: Instant) -> bool {
        let cursor = self.cursor_target();
        let visible = self.conversation_visible();
        let App {
            data, transcript, ..
        } = self;
        let target = if visible {
            resolve_target(data, cursor)
        } else {
            ConversationTarget::NoAgent
        };
        match target {
            ConversationTarget::Local { agent, .. } => {
                let dir = data.tool_config_dirs.get(&agent.name).map(String::as_str);
                transcript.local.poll(Some((agent, dir)), now) | transcript.remote.poll(None, now)
            }
            ConversationTarget::Remote { agent, .. } => {
                transcript.local.poll(None, now) | transcript.remote.poll(Some(agent), now)
            }
            _ => transcript.local.poll(None, now) | transcript.remote.poll(None, now),
        }
    }

    /// `c`: toggle activity/conversation; from inline view, switch to vertical first.
    pub(crate) fn transcript_toggle_tab(&mut self) {
        if self.ui.view_mode == ViewMode::Inline {
            self.transcript.open_on_switch = true;
            self.ui.switch_viewport = true;
            return;
        }
        self.transcript.tab = match self.transcript.tab {
            RightTab::Activity => RightTab::Conversation,
            RightTab::Conversation => RightTab::Activity,
        };
        self.ui.msg_scroll = 0;
    }

    /// `R`: refresh the conversation now. Does nothing outside the tab.
    pub(crate) fn transcript_refresh(&mut self, now: Instant) {
        if !self.conversation_visible() {
            return;
        }
        let cursor = self.cursor_target();
        let App {
            data, transcript, ..
        } = self;
        match resolve_target(data, cursor) {
            ConversationTarget::Local { agent, .. } => transcript.local.force_refresh(agent),
            ConversationTarget::Remote { agent, .. } => transcript.remote.force_refresh(agent, now),
            _ => {}
        }
    }

    /// `e`: toggle the full-text view for the agent under the cursor.
    pub(crate) fn transcript_toggle_expand(&mut self) {
        if !self.conversation_visible() {
            return;
        }
        let cursor = self.cursor_target();
        let App {
            data, transcript, ..
        } = self;
        let name = match resolve_target(data, cursor) {
            ConversationTarget::Local { agent, .. }
            | ConversationTarget::Remote { agent, .. }
            | ConversationTarget::Unsupported(agent)
            | ConversationTarget::LocalNoPath(agent) => agent.display_name(),
            ConversationTarget::NoAgent => return,
        };
        let e = transcript.expanded.entry(name).or_insert(false);
        *e = !*e;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_helpers::{make_test_agent, offline_app};
    use crossterm::event::{KeyCode, KeyModifiers};

    fn vertical_app() -> App {
        let mut app = offline_app();
        app.data.agents = vec![make_test_agent("luna", 0.0)];
        app.ui.view_mode = ViewMode::Vertical;
        app
    }

    #[test]
    fn c_toggles_tab_in_vertical() {
        let mut app = vertical_app();
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(app.transcript.tab, RightTab::Conversation);
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(app.transcript.tab, RightTab::Activity);
    }

    #[test]
    fn c_in_inline_switches_to_vertical_conversation() {
        let mut app = offline_app();
        app.ui.view_mode = ViewMode::Inline;
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(app.ui.switch_viewport);
        assert!(app.transcript.open_on_switch);
        app.transcript.on_enter_vertical();
        assert_eq!(app.transcript.tab, RightTab::Conversation);
        // Back to inline and vertical again with `\`: activity.
        app.transcript.on_enter_vertical();
        assert_eq!(app.transcript.tab, RightTab::Activity);
    }

    #[test]
    fn enter_composes_only_on_conversation_tab() {
        use crate::tui::model::InputMode;
        let mut app = vertical_app();
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.ui.mode, InputMode::Navigate);
        assert!(app.ui.selected.contains("luna"));

        let mut app = vertical_app();
        app.transcript.tab = RightTab::Conversation;
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.ui.mode, InputMode::Compose);
        assert_eq!(app.ui.input, "@luna ");
        assert!(app.ui.selected.is_empty());
    }

    #[test]
    fn env_default_tab() {
        assert_eq!(default_tab_from_env(None), RightTab::Activity);
        assert_eq!(default_tab_from_env(Some("chat")), RightTab::Conversation);
        assert_eq!(
            default_tab_from_env(Some(" Conversation ")),
            RightTab::Conversation
        );
        assert_eq!(default_tab_from_env(Some("activity")), RightTab::Activity);

        let mut view = TranscriptView {
            default_tab: RightTab::Conversation,
            ..Default::default()
        };
        view.on_enter_vertical();
        assert_eq!(view.tab, RightTab::Conversation);
    }

    #[test]
    fn expand_is_remembered_per_agent() {
        let mut app = vertical_app();
        app.data.agents.push(make_test_agent("nova", 0.0));
        app.transcript.tab = RightTab::Conversation;
        app.handle_key(KeyCode::Char('e'), KeyModifiers::NONE);
        let luna = app.data.agents[0].clone();
        let nova = app.data.agents[1].clone();
        assert!(app.transcript.is_expanded(&luna));
        assert!(!app.transcript.is_expanded(&nova));
    }

    #[test]
    fn refresh_and_expand_do_nothing_on_activity_tab() {
        let mut app = vertical_app();
        app.handle_key(KeyCode::Char('e'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert!(app.transcript.expanded.is_empty());
        assert_eq!(app.transcript.tab, RightTab::Activity);
    }

    #[test]
    fn no_agents_resolves_to_no_agent() {
        let mut app = offline_app();
        app.ui.view_mode = ViewMode::Vertical;
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(matches!(
            resolve_target(&app.data, app.cursor_target()),
            ConversationTarget::NoAgent
        ));
    }

    #[test]
    fn conversation_tab_renders() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = vertical_app();
        app.transcript.tab = RightTab::Conversation;
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|f| crate::tui::render::render(f, &mut app))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        // Wide glyphs leave a blank cell after them; drop spaces before matching.
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .replace(' ', "");
        assert!(text.contains("[chat]"), "tab strip missing");
        assert!(text.contains("luna"), "header missing");
    }
}
