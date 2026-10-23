//! Conversation tab in vertical view: the selected agent's recent turns
//! (request, tools used, response), local or over the relay.
pub mod convert;
pub mod local;
pub mod markdown;
pub mod model;
pub mod prompt;
pub mod remote;
pub mod render;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

pub use model::*;

use crate::tui::app::App;
use crate::tui::model::{Agent, CursorTarget, ViewMode};
use local::AccountDir;

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
    /// Agents with an answer on its way: one at a time, so a repeated key
    /// does not land on whatever prompt comes next.
    pub answering: HashSet<String>,
    /// The agent whose conversation is shown: the scroll is its own.
    shown: Option<String>,
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
            data,
            transcript,
            ui,
            ..
        } = self;
        let target = if visible {
            resolve_target(data, cursor)
        } else {
            ConversationTarget::NoAgent
        };
        let shown = match &target {
            ConversationTarget::Local { agent, .. }
            | ConversationTarget::Remote { agent, .. }
            | ConversationTarget::LocalNoPath(agent)
            | ConversationTarget::Unsupported(agent) => Some(agent.display_name()),
            ConversationTarget::NoAgent => None,
        };
        if transcript.shown != shown {
            // Another conversation opens at its latest turn, not at the
            // scroll left in the previous one.
            transcript.shown = shown;
            ui.msg_scroll = 0;
        }
        match target {
            ConversationTarget::Local { agent, .. } => {
                let account = if matches!(cursor, CursorTarget::StoppedAgent(_)) {
                    AccountDir::Unknown
                } else {
                    match data.tool_config_dirs.get(&agent.name) {
                        Some(dir) => AccountDir::Known(dir.as_deref()),
                        None => AccountDir::Unknown,
                    }
                };
                transcript.local.poll(Some((agent, account)), now)
                    | transcript.remote.poll(None, now)
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

    /// The prompt the agent under the cursor waits on, when the
    /// conversation tab shows it.
    pub(crate) fn waiting_prompt(&self) -> Option<(&Agent, &prompt::ScreenPrompt)> {
        if !self.conversation_visible() {
            return None;
        }
        match resolve_target(&self.data, self.cursor_target()) {
            ConversationTarget::Local { agent, .. }
                if agent.status == crate::tui::model::AgentStatus::Blocked =>
            {
                self.transcript
                    .local
                    .snapshot(agent)
                    .prompt
                    .map(|p| (agent, p))
            }
            _ => None,
        }
    }

    /// The local agent under the cursor whose answer is still on its way,
    /// when the conversation tab shows it.
    fn answering_agent(&self) -> Option<String> {
        if !self.conversation_visible() {
            return None;
        }
        match resolve_target(&self.data, self.cursor_target()) {
            ConversationTarget::Local { agent, .. }
                if self.transcript.answering.contains(&agent.name) =>
            {
                Some(agent.name.clone())
            }
            _ => None,
        }
    }

    /// A digit picks an option of the waiting prompt; Enter moves on from a
    /// multi-select question. False when the key is not for the prompt.
    pub(crate) fn transcript_answer_key(&mut self, code: crossterm::event::KeyCode) -> bool {
        use crate::tui::model::Flash;
        use crate::tui::theme::Theme;
        use crossterm::event::KeyCode;
        // The answered prompt is dropped until the screen is read again, so
        // this comes first: otherwise a repeated Enter would compose.
        if matches!(code, KeyCode::Char('1'..='9') | KeyCode::Enter)
            && let Some(name) = self.answering_agent()
        {
            self.ui.flash = Some(Flash::new(
                format!("Still answering {name}: wait for its screen to update"),
                Theme::flash_info(),
            ));
            return true;
        }
        let Some((agent, prompt)) = self.waiting_prompt() else {
            return false;
        };
        let name = agent.name.clone();
        let steps = match code {
            KeyCode::Char(c @ '1'..='9') => {
                let n = (c as u8 - b'0') as usize;
                match prompt.pick(n) {
                    Some(steps) => steps,
                    None => {
                        if prompt.notes_in_use() {
                            self.ui.flash = Some(Flash::new(
                                format!("Notes are open: answer in {name}'s terminal"),
                                Theme::flash_info(),
                            ));
                        } else if prompt.options.iter().any(|o| o.number == n) {
                            self.ui.flash = Some(Flash::new(
                                format!("Option {n} needs typing: answer it in {name}'s terminal"),
                                Theme::flash_info(),
                            ));
                        }
                        return true;
                    }
                }
            }
            KeyCode::Enter => match prompt.next() {
                Some(step) => vec![step],
                None => return false,
            },
            _ => return false,
        };
        let agent = agent.clone();
        match self.enqueue_rpc(crate::tui::rpc_async::RpcOp::AnswerPrompt {
            name: name.clone(),
            steps,
        }) {
            Ok(()) => {
                self.transcript.answering.insert(name);
                self.transcript.local.drop_prompt(&agent);
            }
            Err(e) => {
                self.ui.flash = Some(Flash::new(
                    format!("Answer failed: {e}"),
                    Theme::flash_err(),
                ));
            }
        }
        true
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

    /// Scrolled up in one conversation, then on to another: it opens at its
    /// latest turn. Staying on the same agent keeps the scroll.
    #[test]
    fn another_conversation_opens_at_its_latest_turn() {
        let mut app = vertical_app();
        app.data.agents.push(make_test_agent("nova", 0.0));
        app.transcript.tab = RightTab::Conversation;
        let now = Instant::now();
        app.transcript_poll(now);
        app.ui.msg_scroll = 5;
        app.transcript_poll(now);
        assert_eq!(app.ui.msg_scroll, 5);

        app.ui.cursor = 1;
        assert_eq!(app.cursor_target(), CursorTarget::Agent(1));
        app.transcript_poll(now);
        assert_eq!(app.ui.msg_scroll, 0);
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
    fn enter_composes_to_the_agent_being_read() {
        use crate::tui::model::InputMode;
        let mut app = vertical_app();
        app.data.agents.push(make_test_agent("nova", 0.0));
        app.transcript.tab = RightTab::Conversation;
        app.ui.selected.insert("nova".into());
        app.ui.cursor = 0;
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.ui.mode, InputMode::Compose);
        assert_eq!(app.ui.input, "@luna ", "the selection is not the recipient");
        // `m` still messages the selection.
        app.ui.mode = InputMode::Navigate;
        app.handle_key(KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(app.ui.input, "@nova ");
    }

    #[test]
    fn enter_on_a_header_row_does_not_compose() {
        use crate::tui::model::InputMode;
        let mut app = vertical_app();
        app.data.stopped_agents = vec![make_test_agent("old", 0.0)];
        app.transcript.tab = RightTab::Conversation;
        app.ui.cursor = 1;
        assert_eq!(app.cursor_target(), CursorTarget::StoppedHeader);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.ui.mode, InputMode::Navigate);
        assert!(app.ui.input.is_empty());
    }

    /// `luna` blocked, with a prompt read off its screen.
    fn waiting_app(screen: &str) -> App {
        let mut app = vertical_app();
        app.transcript.tab = RightTab::Conversation;
        let luna = &mut app.data.agents[0];
        luna.status = crate::tui::model::AgentStatus::Blocked;
        luna.status_context = "approval".into();
        luna.transcript_path = Some("/nonexistent/s.jsonl".into());
        let lines: Vec<String> = screen.lines().map(String::from).collect();
        let prompt = prompt::parse_screen(&lines).unwrap();
        app.transcript
            .local
            .set_prompt_for_test(&app.data.agents[0], prompt);
        app
    }

    const QUESTION: &str = "────────────────────────────────
Fruit?

❯ 1. Apple
  2. Pear
  3. Type something.

Enter to select · Esc to cancel";

    const MULTI: &str = "────────────────────────────────
Drink?

❯ 1. [ ] Water
  2. [ ] Tea

Enter to select · Esc to cancel";

    #[test]
    fn digits_answer_a_waiting_prompt() {
        let mut app = waiting_app(QUESTION);
        assert!(app.waiting_prompt().is_some());
        app.handle_key(KeyCode::Char('2'), KeyModifiers::NONE);
        // Sent (offline: no RPC worker, so it reports the failure).
        let flash = app.ui.flash.take().unwrap();
        assert!(flash.text.contains("Answer failed"), "{}", flash.text);

        app.handle_key(KeyCode::Char('3'), KeyModifiers::NONE);
        let flash = app.ui.flash.take().unwrap();
        assert!(flash.text.contains("needs typing"), "{}", flash.text);

        // Not an option: swallowed without a message.
        app.handle_key(KeyCode::Char('7'), KeyModifiers::NONE);
        assert!(app.ui.flash.is_none());

        // Enter still composes on a single-select question.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.ui.mode, crate::tui::model::InputMode::Compose);
    }

    /// SEC-003: while an answer is on its way, the prompt it answered is
    /// gone and a repeated key for the same agent is not sent: it would
    /// otherwise land on whatever prompt the agent shows next.
    #[test]
    fn a_repeated_key_waits_for_the_answer_in_flight() {
        let mut app = waiting_app(QUESTION);
        app.rpc_client = Some(crate::tui::rpc_async::RpcClient::start_with(|_| {
            Err("offline".into())
        }));
        app.handle_key(KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(app.ui.flash.is_none());
        assert!(app.transcript.answering.contains("luna"));
        assert!(
            app.waiting_prompt().is_none(),
            "the answered prompt is dropped"
        );
        let sent = |app: &App| app.rpc_client.as_ref().unwrap().pending_count();
        assert_eq!(sent(&app), 1);

        // A screen read from before the answer shows the same prompt again.
        let lines: Vec<String> = QUESTION.lines().map(String::from).collect();
        app.transcript
            .local
            .set_prompt_for_test(&app.data.agents[0], prompt::parse_screen(&lines).unwrap());
        app.handle_key(KeyCode::Char('2'), KeyModifiers::NONE);
        let flash = app.ui.flash.take().unwrap();
        assert!(
            flash.text.contains("Still answering luna"),
            "{}",
            flash.text
        );
        assert_eq!(sent(&app), 1, "nothing more is sent");

        // The answer comes back failed: the next key may go out, and the
        // screen is read again so the dropped prompt shows up.
        app.transcript.local.take_refresh_request();
        for _ in 0..200 {
            if app.drain_rpc_results() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.transcript.answering.is_empty());
        assert!(app.transcript.local.take_refresh_request());
        app.ui.flash = None;
        app.handle_key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert!(app.ui.flash.is_none());
        assert!(app.transcript.answering.contains("luna"));
    }

    /// Right after an answer no prompt is cached until the screen is read
    /// again: a second key is still held, and Enter does not compose.
    #[test]
    fn a_key_right_after_an_answer_is_held() {
        use crate::tui::model::InputMode;
        for screen in [QUESTION, MULTI] {
            let mut app = waiting_app(screen);
            app.rpc_client = Some(crate::tui::rpc_async::RpcClient::start_with(|_| {
                Err("offline".into())
            }));
            let first = if screen == MULTI {
                KeyCode::Enter
            } else {
                KeyCode::Char('1')
            };
            app.handle_key(first, KeyModifiers::NONE);
            assert!(app.ui.flash.is_none());
            assert!(app.waiting_prompt().is_none());
            let sent = |app: &App| app.rpc_client.as_ref().unwrap().pending_count();
            assert_eq!(sent(&app), 1);

            for key in [KeyCode::Enter, KeyCode::Char('2')] {
                app.handle_key(key, KeyModifiers::NONE);
                assert_eq!(app.ui.mode, InputMode::Navigate, "{key:?} composed");
                assert!(app.ui.input.is_empty());
                let flash = app.ui.flash.take().unwrap();
                assert!(
                    flash.text.contains("Still answering luna"),
                    "{}",
                    flash.text
                );
            }
            assert_eq!(sent(&app), 1, "nothing more is sent");
        }
    }

    #[test]
    fn enter_moves_on_from_multi_select() {
        let mut app = waiting_app(MULTI);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.ui.mode, crate::tui::model::InputMode::Navigate);
        assert!(app.ui.flash.take().unwrap().text.contains("Answer failed"));
    }

    #[test]
    fn digits_are_ordinary_keys_without_a_waiting_prompt() {
        let mut app = waiting_app(QUESTION);
        app.transcript.tab = RightTab::Activity;
        assert!(app.waiting_prompt().is_none());
        app.handle_key(KeyCode::Char('1'), KeyModifiers::NONE);
        assert!(app.ui.flash.is_none());

        app.transcript.tab = RightTab::Conversation;
        app.data.agents[0].status = crate::tui::model::AgentStatus::Listening;
        assert!(app.waiting_prompt().is_none());
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

    fn screen_text(app: &mut App, width: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
        terminal
            .draw(|f| crate::tui::render::render(f, app))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        buf.content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .replace(' ', "")
    }

    #[test]
    fn command_output_shows_on_the_conversation_tab() {
        let mut app = vertical_app();
        app.transcript.tab = RightTab::Conversation;
        app.ui.command_result = Some(crate::tui::model::CommandResult {
            label: "relay status".into(),
            output: vec!["relay-output-line".into()],
        });
        assert!(screen_text(&mut app, 100).contains("relay-output-line"));
    }

    #[test]
    fn activity_heading_stops_before_the_tab_strip() {
        let mut app = vertical_app();
        let long = "x".repeat(80);
        app.ui.selected.insert(long.clone());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|f| crate::tui::render::render(f, &mut app))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .find(|r| r.contains("[activity]"))
            .expect("tab strip row");
        // The heading leaves a gap instead of running under the strip.
        assert!(row.contains(" [activity]"), "{row}");
        assert!(row.contains("xxx"), "{row}");
    }
}
