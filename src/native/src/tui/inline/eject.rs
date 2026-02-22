use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::*;

use crate::tui::app::DataState;
use crate::tui::model::*;
use crate::tui::render::messages::{event_line, format_message};
use crate::tui::render::text::highlight_spans;
use crate::tui::theme::{Theme, palette};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReplayReason {
    FilterChange,
    Resize,
}

pub struct Ejector {
    last_event_row_id: u64,
    last_msg_id: u64,
    replay_items: VecDeque<EjectItem>,
    replay_lines: VecDeque<Line<'static>>,
    replay_emitted_any: bool,
    replay_lines_per_tick: usize,
    was_replaying: bool,
    replay_reason: ReplayReason,
    /// name → display_name map built from DataState on each replay/eject cycle
    name_map: HashMap<String, String>,
    banner_emitted: bool,
    pending_filter_separator: Option<(Option<BTreeSet<String>>, Option<String>)>,
}

impl Ejector {
    pub fn new() -> Self {
        Self {
            last_event_row_id: 0,
            last_msg_id: 0,
            replay_items: VecDeque::new(),
            replay_lines: VecDeque::new(),
            replay_emitted_any: false,
            replay_lines_per_tick: std::env::var("HCOM_TUI_REPLAY_LINES_PER_TICK")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(200),
            was_replaying: false,
            replay_reason: ReplayReason::Resize,
            name_map: HashMap::new(),
            banner_emitted: false,
            pending_filter_separator: None,
        }
    }

    /// Reset incremental watermarks and replay state.
    fn reset(&mut self) {
        self.last_event_row_id = 0;
        self.last_msg_id = 0;
        self.replay_items.clear();
        self.replay_lines.clear();
        self.replay_emitted_any = false;
        self.was_replaying = false;
        self.banner_emitted = false;
        self.pending_filter_separator = None;
    }

    fn refresh_name_map(&mut self, data: &DataState) {
        self.name_map.clear();
        for agent in data
            .agents
            .iter()
            .chain(data.remote_agents.iter())
            .chain(data.stopped_agents.iter())
        {
            if !agent.tag.is_empty() {
                self.name_map
                    .insert(agent.name.clone(), agent.display_name());
            }
        }
    }

    /// Queue a full replay of currently loaded events/messages.
    /// Replay is emitted gradually (line-bounded) to avoid post-resize stalls.
    pub fn begin_replay(
        &mut self,
        data: &DataState,
        filter: &Option<BTreeSet<String>>,
        text_filter: &Option<String>,
        reason: ReplayReason,
    ) {
        self.reset();
        self.replay_reason = reason;
        self.pending_filter_separator = if reason == ReplayReason::FilterChange {
            Some((filter.clone(), text_filter.clone()))
        } else {
            None
        };
        self.refresh_name_map(data);

        let mut items: Vec<EjectItem> = Vec::new();
        // When FTS search results are available, use them as source (text filter already applied).
        // Fall back to the in-memory window with client-side filtering when no search is active.
        let (ev_src, msg_src, eff_text): (&[Event], &[Message], &Option<String>) =
            if let Some((ref msgs, ref evs)) = data.search_results {
                (evs.as_slice(), msgs.as_slice(), &None)
            } else {
                (
                    data.events.as_slice(),
                    data.messages.as_slice(),
                    text_filter,
                )
            };
        for ev in ev_src
            .iter()
            .filter(|ev| event_matches_filter(ev, filter, eff_text))
        {
            items.push(EjectItem::Ev(ev.clone()));
        }
        for msg in msg_src
            .iter()
            .filter(|msg| message_matches_filter(msg, filter, eff_text))
        {
            items.push(EjectItem::Msg(msg.clone()));
        }
        items.sort_by(|a, b| a.time().total_cmp(&b.time()));
        self.was_replaying = !items.is_empty();
        self.replay_items = items.into();

        // Snapshot watermarks from the full in-memory window so live mode catches all new events.
        self.last_event_row_id = data.events.iter().map(|e| e.row_id).max().unwrap_or(0);
        self.last_msg_id = data.messages.iter().map(|m| m.event_id).max().unwrap_or(0);
    }

    pub fn is_replaying(&self) -> bool {
        !self.replay_items.is_empty() || !self.replay_lines.is_empty()
    }

    /// Eject new events/messages to terminal scrollback via insert_before.
    pub fn eject_new(
        &mut self,
        data: &DataState,
        filter: &Option<BTreeSet<String>>,
        text_filter: &Option<String>,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> io::Result<()> {
        self.refresh_name_map(data);
        if self.is_replaying() {
            let replay_done = self.eject_replay_chunk(text_filter, terminal)?;
            if replay_done && self.was_replaying {
                self.was_replaying = false;
            }
            return Ok(());
        }

        let mut items: Vec<EjectItem> = Vec::new();
        for ev in data.events.iter() {
            if ev.row_id > self.last_event_row_id {
                self.last_event_row_id = ev.row_id;
                if event_matches_filter(ev, filter, text_filter) {
                    items.push(EjectItem::Ev(ev.clone()));
                }
            }
        }
        for msg in data.messages.iter() {
            if msg.event_id > self.last_msg_id {
                self.last_msg_id = msg.event_id;
                if message_matches_filter(msg, filter, text_filter) {
                    items.push(EjectItem::Msg(msg.clone()));
                }
            }
        }

        if items.is_empty() {
            return Ok(());
        }

        items.sort_by(|a, b| a.time().total_cmp(&b.time()));
        self.eject_lines(&items, text_filter, terminal)
    }

    fn eject_replay_chunk(
        &mut self,
        text_filter: &Option<String>,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> io::Result<bool> {
        let width = terminal.size()?.width;
        let mut out_lines: Vec<Line<'static>> = Vec::new();

        if let Some(banner) = self.take_banner_lines(width as usize) {
            out_lines.extend(banner);
        }
        if let Some((filter, text_filter)) = self.pending_filter_separator.take() {
            out_lines.extend(filter_separator_lines(&filter, &text_filter, width));
        }

        // Clone to avoid borrow conflict with self.replay_items mutation
        let nm = self.name_map.clone();
        let resolve_name = move |name: &str| -> String {
            nm.get(name).cloned().unwrap_or_else(|| name.to_string())
        };

        // Fill replay_lines from items until we have enough for one chunk.
        while self.replay_lines.len() < self.replay_lines_per_tick {
            let Some(item) = self.replay_items.pop_front() else {
                break;
            };
            let item_lines = format_item_lines(
                &item,
                width,
                self.replay_emitted_any && matches!(item, EjectItem::Msg(_)),
                &resolve_name,
                text_filter.as_deref(),
            );
            self.replay_lines.extend(item_lines);
            self.replay_emitted_any = true;
        }

        let n = self.replay_lines.len().min(self.replay_lines_per_tick);
        let replay_done = self.replay_items.is_empty() && self.replay_lines.len() <= n;
        if n > 0 {
            out_lines.extend(self.replay_lines.drain(..n));
        }

        if replay_done && self.was_replaying && self.replay_reason == ReplayReason::FilterChange {
            out_lines.extend(live_marker_lines(width));
        }

        if out_lines.is_empty() {
            return Ok(replay_done);
        }
        emit_lines(&out_lines, terminal)?;
        Ok(replay_done)
    }

    fn take_banner_lines(&mut self, width: usize) -> Option<Vec<Line<'static>>> {
        if self.banner_emitted {
            return None;
        }
        self.banner_emitted = true;
        let style = Style::default()
            .fg(palette::ORANGE)
            .add_modifier(Modifier::BOLD);

        let noise = |offset: usize, w: usize| -> String {
            let chars = ['░', '▒', '▓'];
            (0..w).map(|i| chars[(i + offset) % 3]).collect()
        };

        let label = " hcom ";
        let label_w = 6;
        let left_w = width.saturating_sub(label_w) / 2;
        let right_w = width.saturating_sub(label_w + left_w);

        let lines = vec![Line::from(vec![
            Span::styled(noise(0, left_w), style),
            Span::styled(label, style),
            Span::styled(noise(left_w + label_w, right_w), style),
        ])];
        Some(lines)
    }

    /// Eject command output lines into scrollback with a label separator.
    pub fn eject_command_output(
        &self,
        label: &str,
        output: &[String],
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> io::Result<()> {
        let width = terminal.size()?.width;
        let mut lines: Vec<Line> = Vec::new();

        lines.push(Line::raw(""));
        lines.push(separator(&format!("! {}", label), palette::MAGENTA, width));

        for line in output {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(line.clone(), Style::default().fg(palette::FG)),
            ]));
        }
        lines.push(Line::raw(""));

        emit_lines(&lines, terminal)
    }

    fn eject_lines(
        &mut self,
        items: &[EjectItem],
        text_filter: &Option<String>,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> io::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let width = terminal.size()?.width;
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(banner) = self.take_banner_lines(width as usize) {
            lines.extend(banner);
        }
        let resolve_name = |name: &str| -> String {
            self.name_map
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_string())
        };
        let query = text_filter.as_deref();
        let mut item_lines: Vec<Line<'static>> = Vec::new();
        for item in items {
            item_lines.extend(format_item_lines(
                item,
                width,
                !item_lines.is_empty(),
                &resolve_name,
                query,
            ));
        }
        lines.extend(item_lines);

        emit_lines(&lines, terminal)
    }
}

/// Insert lines into scrollback via insert_before.
fn emit_lines(lines: &[Line], terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let height = lines.len() as u16;
    terminal.insert_before(height, |buf| {
        for (i, line) in lines.iter().enumerate() {
            if (i as u16) < buf.area.height {
                let row = Rect::new(buf.area.x, buf.area.y + i as u16, buf.area.width, 1);
                line.render(row, buf);
            }
        }
    })?;
    Ok(())
}

fn filter_separator_lines(
    filter: &Option<BTreeSet<String>>,
    text_filter: &Option<String>,
    width: u16,
) -> Vec<Line<'static>> {
    let label = match (filter, text_filter) {
        (Some(names), Some(query)) => {
            let joined = names.iter().cloned().collect::<Vec<_>>().join(", ");
            format!("filtering: {} \u{00b7} /{}", joined, query)
        }
        (Some(names), None) => {
            let joined = names.iter().cloned().collect::<Vec<_>>().join(", ");
            format!("filtering: {}", joined)
        }
        (None, Some(query)) => format!("/{}", query),
        (None, None) => "all".to_string(),
    };
    let color = if filter.is_none() && text_filter.is_none() {
        palette::FG_DIM
    } else {
        palette::CYAN
    };

    vec![
        Line::raw(""),
        separator(&label, color, width),
        Line::raw(""),
    ]
}

fn live_marker_lines(width: u16) -> Vec<Line<'static>> {
    vec![
        Line::raw(""),
        separator("live", palette::GREEN, width),
        Line::raw(""),
    ]
}

fn format_item_lines(
    item: &EjectItem,
    width: u16,
    pad_message: bool,
    resolve_name: &dyn Fn(&str) -> String,
    query: Option<&str>,
) -> Vec<Line<'static>> {
    match item {
        EjectItem::Ev(ev) => {
            let time_str = format_time(ev.time);
            let mut lines = vec![event_line(
                ev,
                &time_str,
                false,
                true,
                width,
                query,
                resolve_name,
            )];
            for sub in &ev.sub_lines {
                let spans = highlight_spans(
                    vec![Span::styled(
                        format!("        {}", sub),
                        Style::default().fg(palette::FG_DIM),
                    )],
                    query,
                );
                lines.push(Line::from(spans));
            }
            lines
        }
        EjectItem::Msg(msg) => {
            let mut lines = Vec::new();
            if pad_message {
                lines.push(Line::raw(""));
            }
            lines.extend(format_message(msg, width, query, resolve_name, None));
            lines
        }
    }
}

// ── Filter matching ──────────────────────────────────────────────

pub(crate) fn event_matches_filter(
    ev: &Event,
    filter: &Option<BTreeSet<String>>,
    text_filter: &Option<String>,
) -> bool {
    let agent_ok = match filter {
        None => true,
        Some(names) => names.contains(&ev.agent),
    };
    let text_ok = match text_filter {
        None => true,
        Some(query) => {
            let q = query.to_lowercase();
            ev.agent.to_lowercase().contains(&q)
                || ev.tool.to_lowercase().contains(&q)
                || ev.detail.to_lowercase().contains(&q)
                || ev.sub_lines.iter().any(|s| s.to_lowercase().contains(&q))
        }
    };
    agent_ok && text_ok
}

pub(crate) fn message_matches_filter(
    msg: &Message,
    filter: &Option<BTreeSet<String>>,
    text_filter: &Option<String>,
) -> bool {
    let agent_ok = match filter {
        None => true,
        Some(names) => {
            names.contains(&msg.sender)
                || msg.recipients.iter().any(|r| names.contains(r))
                || (msg.recipients.is_empty() && !msg.is_system())
        }
    };
    let text_ok = match text_filter {
        None => true,
        Some(query) => {
            let q = query.to_lowercase();
            msg.body.to_lowercase().contains(&q) || msg.sender.to_lowercase().contains(&q)
        }
    };
    agent_ok && text_ok
}

/// Count events and messages matching the current inline filters.
pub(crate) fn filtered_counts(
    data: &DataState,
    filter: &Option<BTreeSet<String>>,
    text_filter: &Option<String>,
) -> (usize, usize) {
    let (ev_src, msg_src, eff_text): (&[Event], &[Message], &Option<String>) =
        if let Some((ref msgs, ref evs)) = data.search_results {
            (evs.as_slice(), msgs.as_slice(), &None)
        } else {
            (
                data.events.as_slice(),
                data.messages.as_slice(),
                text_filter,
            )
        };
    let ev = ev_src
        .iter()
        .filter(|e| event_matches_filter(e, filter, eff_text))
        .count();
    let msg = msg_src
        .iter()
        .filter(|m| message_matches_filter(m, filter, eff_text))
        .count();
    (ev, msg)
}

// ── Formatting ───────────────────────────────────────────────────

/// Separator line: `  ── label ──────────`
fn separator(label: &str, color: Color, width: u16) -> Line<'static> {
    let prefix = "\u{2500}\u{2500} ";
    let label_display = format!("{} ", label);
    let prefix_w = unicode_width::UnicodeWidthStr::width(prefix);
    let label_w = unicode_width::UnicodeWidthStr::width(label_display.as_str());
    let fill_len = (width as usize).saturating_sub(2 + prefix_w + label_w);

    Line::from(vec![
        Span::raw("  "),
        Span::styled(prefix.to_string(), Theme::separator()),
        Span::styled(label_display, Style::default().fg(color)),
        Span::styled("\u{2500}".repeat(fill_len), Theme::separator()),
    ])
}

enum EjectItem {
    Ev(Event),
    Msg(Message),
}

impl EjectItem {
    fn time(&self) -> f64 {
        match self {
            EjectItem::Ev(e) => e.time,
            EjectItem::Msg(m) => m.time,
        }
    }
}
