use std::collections::{BTreeSet, HashMap};

use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::tui::app::App;
use crate::tui::model::*;
use crate::tui::render::text::{fmt_agent, highlight_spans, render_body};
use crate::tui::theme::{Theme, palette};

/// Build agent name → last_event_id map for read-receipt waterline checks.
fn build_waterlines(app: &App) -> HashMap<String, u64> {
    app.data
        .agents
        .iter()
        .chain(app.data.remote_agents.iter())
        .filter_map(|a| a.last_event_id.map(|id| (a.name.clone(), id)))
        .collect()
}

const DISPLAY_LIMIT: usize = 5000; // vertical mode loads up to 5000 from DB

use super::truncate_display;

pub fn render_messages(frame: &mut Frame, area: Rect, app: &App) -> usize {
    if app.ui.show_events {
        let agents = if app.ui.selected.is_empty() {
            None
        } else {
            Some(&app.ui.selected)
        };
        render_events_timeline(frame, area, app, agents)
    } else if let Some(ref cr) = app.ui.command_result {
        render_command_output(frame, area, &cr.output, app.ui.msg_scroll)
    } else if !app.ui.selected.is_empty() {
        render_agent_feed(frame, area, app, &app.ui.selected)
    } else {
        render_all_messages(frame, area, app)
    }
}

// ── Events timeline: all events + messages merged ────────────────────

fn render_events_timeline(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    agents: Option<&BTreeSet<String>>,
) -> usize {
    let mut items: Vec<FeedItem> = Vec::new();
    for ev in &app.data.events {
        if agents.is_none_or(|a| a.contains(&ev.agent)) {
            items.push(FeedItem::Ev(ev));
        }
    }
    for msg in &app.data.messages {
        if agents.is_none_or(|a| {
            a.contains(&msg.sender)
                || msg.recipients.iter().any(|r| a.contains(r))
                || (msg.recipients.is_empty() && !msg.is_system())
        }) {
            items.push(FeedItem::Msg(msg));
        }
    }
    items.sort_by(|a, b| a.time().total_cmp(&b.time()));

    // Keep only the most recent items up to the display limit
    if items.len() > DISPLAY_LIMIT {
        items.drain(..items.len() - DISPLAY_LIMIT);
    }

    // Filter by search query if present
    let query = app.active_search_query();
    if let Some(q) = query {
        items.retain(|item| match item {
            FeedItem::Ev(ev) => event_matches(ev, q),
            FeedItem::Msg(msg) => msg_matches(msg, q),
        });
    }

    if items.is_empty() {
        let msg = if query.is_some() {
            "  No matches"
        } else {
            "  No events yet"
        };
        let empty = vec![
            Line::raw(""),
            Line::from(Span::styled(msg, Style::default().fg(palette::FG_DIM))),
        ];
        frame.render_widget(Paragraph::new(empty), area);
        return 0;
    }

    let resolve_name = |name: &str| app.data.resolve_display_name(name);
    let wl = build_waterlines(app);
    let mut lines: Vec<Line> = Vec::new();
    let mut prev_group: Option<(String, String)> = None;
    for item in &items {
        match item {
            FeedItem::Ev(ev) => {
                let time_str = format_time(ev.time);
                let same = prev_group
                    .as_ref()
                    .is_some_and(|(a, t)| a == &ev.agent && t == &time_str);
                prev_group = Some((ev.agent.clone(), time_str.clone()));
                lines.push(event_line(
                    ev,
                    &time_str,
                    same,
                    true,
                    area.width,
                    query,
                    &resolve_name,
                ));
                push_sub_lines(&mut lines, ev);
            }
            FeedItem::Msg(msg) => {
                prev_group = None;
                push_feed_message(
                    &mut lines,
                    msg,
                    area.width as usize,
                    query,
                    &resolve_name,
                    Some(&wl),
                );
            }
        }
    }

    render_scrolled(frame, area, lines, app.ui.msg_scroll)
}

// ── Shared feed rendering helpers ───────────────────────────────────

enum FeedItem<'a> {
    Ev(&'a Event),
    Msg(&'a Message),
}

impl<'a> FeedItem<'a> {
    fn time(&self) -> f64 {
        match self {
            FeedItem::Ev(e) => e.time,
            FeedItem::Msg(m) => m.time,
        }
    }
}

/// Render a single event (tool or activity) as a Line with right-aligned time.
pub(crate) fn event_line(
    ev: &Event,
    time_str: &str,
    same: bool,
    show_agent: bool,
    width: u16,
    query: Option<&str>,
    resolve_name: &dyn Fn(&str) -> String,
) -> Line<'static> {
    let right_time: Vec<Span> = if same {
        vec![Span::raw("       ")] // 5 (time) + 2 (margin)
    } else {
        vec![
            Span::styled(format!(" {}", time_str), Theme::dim()),
            Span::raw("  "),
        ]
    };
    let right_w: usize = right_time.iter().map(|s| s.width()).sum();

    let agent_display = resolve_name(&ev.agent);
    let agent_col_w = unicode_width::UnicodeWidthStr::width(agent_display.as_str()).max(4) + 1;

    match ev.kind {
        EventKind::Tool => {
            let tc = tool_color(&ev.tool);
            let mut spans = vec![Span::raw("  ")];
            if show_agent {
                spans.push(if same {
                    Span::raw(" ".repeat(agent_col_w))
                } else {
                    Span::styled(
                        fmt_agent(&agent_display, agent_col_w),
                        Style::default().fg(palette::FG_DIM),
                    )
                });
            }
            spans.push(Span::styled(
                format!("{} ", ev.tool),
                Style::default().fg(tc),
            ));
            let prefix_w: usize = spans.iter().map(|s| s.width()).sum();
            let margin = 2usize;
            // Truncate detail to fit; time shown only if room remains
            let full_avail = (width as usize).saturating_sub(prefix_w + margin);
            let detail_text = truncate_display(&ev.detail, full_avail);
            spans.extend(highlight_spans(
                vec![Span::styled(detail_text, Style::default().fg(palette::FG))],
                query,
            ));
            let left_w: usize = spans.iter().map(|s| s.width()).sum();
            if left_w + right_w <= width as usize {
                let pad = (width as usize).saturating_sub(left_w + right_w);
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(right_time);
            }
            Line::from(spans)
        }
        EventKind::Activity(kind) => {
            let (icon, color) = match kind {
                ActivityKind::Started => ("\u{25b6}", palette::GREEN),
                ActivityKind::Active => ("\u{25b6}", palette::GREEN),
                ActivityKind::Listening => ("\u{25c9}", palette::CYAN),
                ActivityKind::Stopped => ("\u{25cb}", palette::FG_DIM),
                ActivityKind::Blocked => ("\u{25a0}", palette::RED),
                ActivityKind::StateChange => ("\u{25c6}", palette::YELLOW),
            };
            let detail = if kind == ActivityKind::Active {
                format!("active: {}", ev.detail)
            } else {
                ev.detail.clone()
            };
            let mut spans = vec![Span::raw("  ")];
            if show_agent {
                spans.push(if same {
                    Span::raw(" ".repeat(agent_col_w))
                } else {
                    Span::styled(
                        fmt_agent(&agent_display, agent_col_w),
                        Style::default().fg(palette::FG_DIM),
                    )
                });
            }
            spans.push(Span::styled(
                format!("{} ", icon),
                Style::default().fg(color),
            ));
            let prefix_w: usize = spans.iter().map(|s| s.width()).sum();
            let margin = 2usize;
            let full_avail = (width as usize).saturating_sub(prefix_w + margin);
            let detail_text = truncate_display(&detail, full_avail);
            spans.extend(highlight_spans(
                vec![Span::styled(detail_text, Style::default().fg(color))],
                query,
            ));
            let left_w: usize = spans.iter().map(|s| s.width()).sum();
            if left_w + right_w <= width as usize {
                let pad = (width as usize).saturating_sub(left_w + right_w);
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(right_time);
            }
            Line::from(spans)
        }
    }
}

/// Push sub_lines (stopped snapshot details etc.) as indented dim lines.
/// The last sub_line starting with "hcom " is styled as an actionable command.
fn push_sub_lines(lines: &mut Vec<Line<'static>>, ev: &Event) {
    for sub in &ev.sub_lines {
        let style = if sub.starts_with("hcom ") {
            Style::default().fg(palette::CYAN)
        } else {
            Style::default().fg(palette::FG_DIM)
        };
        lines.push(Line::from(Span::styled(format!("        {}", sub), style)));
    }
}

/// Format a message (header + body) as lines with right-aligned time.
pub(crate) fn format_message(
    msg: &Message,
    width: u16,
    query: Option<&str>,
    resolve_name: &dyn Fn(&str) -> String,
    waterlines: Option<&HashMap<String, u64>>,
) -> Vec<Line<'static>> {
    let mut header_spans: Vec<Span> = vec![Span::raw("  ")];
    push_msg_header(&mut header_spans, msg, query, resolve_name, waterlines);

    // Right-align time
    let time_str = format!(" {}  ", format_time(msg.time));
    let left_w: usize = header_spans.iter().map(|s| s.width()).sum();
    let time_w = Span::raw(&time_str).width();
    let pad = (width as usize).saturating_sub(left_w + time_w);
    header_spans.push(Span::raw(" ".repeat(pad)));
    header_spans.push(Span::styled(time_str, Theme::dim()));

    let mut lines = vec![Line::from(header_spans)];
    if !msg.is_system() {
        lines.extend(render_body(&msg.body, width as usize, query));
    }
    lines
}

/// Render a message (header + body) into a line buffer with surrounding blanks.
fn push_feed_message(
    lines: &mut Vec<Line<'static>>,
    msg: &Message,
    width: usize,
    query: Option<&str>,
    resolve_name: &dyn Fn(&str) -> String,
    waterlines: Option<&HashMap<String, u64>>,
) {
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines.extend(format_message(
        msg,
        width as u16,
        query,
        resolve_name,
        waterlines,
    ));
    lines.push(Line::raw(""));
}

// ── Agent feed: merged tool events + messages ──────────────────────

fn render_agent_feed(frame: &mut Frame, area: Rect, app: &App, agents: &BTreeSet<String>) -> usize {
    let resolve_name = |name: &str| app.data.resolve_display_name(name);
    // Collect relevant items
    let mut items: Vec<FeedItem> = Vec::new();

    for ev in &app.data.events {
        if agents.contains(&ev.agent) {
            items.push(FeedItem::Ev(ev));
        }
    }

    for msg in &app.data.messages {
        if agents.contains(&msg.sender)
            || msg.recipients.iter().any(|r| agents.contains(r))
            || (msg.recipients.is_empty() && !msg.is_system())
        // broadcasts, not lifecycle noise
        {
            items.push(FeedItem::Msg(msg));
        }
    }

    // Merge-sort by time. DataState vecs are pre-sorted so timsort is O(n).
    items.sort_by(|a, b| a.time().total_cmp(&b.time()));

    // Keep only the most recent items up to the display limit
    if items.len() > DISPLAY_LIMIT {
        items.drain(..items.len() - DISPLAY_LIMIT);
    }

    let search_query = app.active_search_query();

    if let Some(query) = search_query {
        items.retain(|item| match item {
            FeedItem::Ev(ev) => event_matches(ev, query),
            FeedItem::Msg(msg) => msg_matches(msg, query),
        });
    }

    if items.is_empty() {
        let msg = if search_query.is_some() {
            "  No matches"
        } else {
            "  No activity yet"
        };
        let empty = vec![
            Line::raw(""),
            Line::from(Span::styled(msg, Style::default().fg(palette::FG_DIM))),
        ];
        frame.render_widget(Paragraph::new(empty), area);
        return 0;
    }

    let show_agent_name = agents.len() > 1;
    let wl = build_waterlines(app);
    let mut lines: Vec<Line> = Vec::new();

    let mut prev_group: Option<(String, String)> = None;
    for item in &items {
        match item {
            FeedItem::Ev(ev) => {
                let time_str = format_time(ev.time);
                let same = prev_group
                    .as_ref()
                    .is_some_and(|(a, t)| a == &ev.agent && t == &time_str);
                prev_group = Some((ev.agent.clone(), time_str.clone()));
                lines.push(event_line(
                    ev,
                    &time_str,
                    same,
                    show_agent_name,
                    area.width,
                    search_query,
                    &resolve_name,
                ));
                push_sub_lines(&mut lines, ev);
            }
            FeedItem::Msg(msg) => {
                prev_group = None;
                push_feed_message(
                    &mut lines,
                    msg,
                    area.width as usize,
                    search_query,
                    &resolve_name,
                    Some(&wl),
                );
            }
        }
    }

    render_scrolled(frame, area, lines, app.ui.msg_scroll)
}

// ── All messages view ──────────────────────────────────────────────

fn render_all_messages(frame: &mut Frame, area: Rect, app: &App) -> usize {
    let resolve_name = |name: &str| app.data.resolve_display_name(name);

    // Use FTS search results when available (vertical mode with active query).
    // search_results messages are already filtered and sorted ascending.
    let (msgs, is_fts_search): (Vec<&Message>, bool) =
        if let Some((ref search_msgs, _)) = app.data.search_results {
            let start = search_msgs.len().saturating_sub(DISPLAY_LIMIT);
            (search_msgs[start..].iter().collect(), true)
        } else {
            let v = app
                .data
                .messages
                .iter()
                .rev()
                .take(DISPLAY_LIMIT)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            (v, false)
        };

    if msgs.is_empty() {
        let label = if is_fts_search {
            "  No matches"
        } else {
            "  No messages yet"
        };
        let empty = vec![
            Line::raw(""),
            Line::from(Span::styled(label, Style::default().fg(palette::FG_DIM))),
        ];
        frame.render_widget(Paragraph::new(empty), area);
        return 0;
    }

    // search_query drives highlight spans; filtering is skipped for FTS results.
    let search_query = app.active_search_query();

    // Selected agent name for highlighting related messages
    let remote_name_buf: String;
    let selected_agent = match app.cursor_target() {
        CursorTarget::Agent(idx) => Some(app.data.agents[idx].name.as_str()),
        CursorTarget::RemoteAgent(idx) => {
            remote_name_buf = app.data.remote_agents[idx].display_name();
            Some(remote_name_buf.as_str())
        }
        CursorTarget::StoppedAgent(idx) => Some(app.data.stopped_agents[idx].name.as_str()),
        _ => None,
    };

    let wl = build_waterlines(app);
    let mut lines: Vec<Line> = Vec::new();

    for msg in &msgs {
        if !is_fts_search {
            if let Some(query) = search_query {
                if !msg_matches(msg, query) {
                    continue;
                }
            }
        }

        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }

        // Check if this message involves the selected agent
        let involves_selected = selected_agent
            .is_some_and(|name| msg.sender == name || msg.recipients.iter().any(|r| r == name));

        // Left margin: dot marker for messages involving selected agent
        let margin: Span = if involves_selected {
            Span::styled(" \u{2502}", Style::default().fg(palette::BLUE))
        } else {
            Span::raw("  ")
        };

        // Header: sender -> recipients + time
        let mut header_spans: Vec<Span> = vec![margin.clone()];
        push_msg_header(
            &mut header_spans,
            msg,
            search_query,
            &resolve_name,
            Some(&wl),
        );

        // Right-align time (with 2-space trailing margin matching left)
        let left_width: usize = header_spans.iter().map(|s| s.width()).sum();
        let time_str = format!(" {}  ", format_time(msg.time));
        let time_w = Span::raw(&time_str).width();
        let pad = (area.width as usize).saturating_sub(left_width + time_w);
        header_spans.push(Span::raw(" ".repeat(pad)));
        header_spans.push(Span::styled(time_str, Theme::dim()));

        lines.push(Line::from(header_spans));

        // Body (skip for system messages since body is in header)
        if !msg.is_system() {
            lines.extend(render_body(&msg.body, area.width as usize, search_query));
        }
    }

    render_scrolled(frame, area, lines, app.ui.msg_scroll)
}

fn push_msg_header(
    spans: &mut Vec<Span<'static>>,
    msg: &Message,
    query: Option<&str>,
    resolve_name: &dyn Fn(&str) -> String,
    waterlines: Option<&HashMap<String, u64>>,
) {
    if msg.is_system() {
        spans.extend(highlight_spans(
            vec![Span::styled(
                msg.body.clone(),
                Style::default().fg(palette::YELLOW),
            )],
            query,
        ));
    } else {
        spans.extend(highlight_spans(
            vec![Span::styled(
                resolve_name(&msg.sender),
                Style::default().fg(palette::FG),
            )],
            query,
        ));
        spans.push(Span::styled(" \u{2192} ", Theme::dim()));

        if msg.recipients.is_empty() {
            spans.push(Span::styled("all", Theme::dim()));
        } else {
            for (i, r) in msg.recipients.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(", ", Theme::dim()));
                }
                spans.extend(highlight_spans(
                    vec![Span::styled(
                        resolve_name(r),
                        Style::default().fg(palette::FG),
                    )],
                    query,
                ));
                if let Some(wl) = waterlines {
                    let has_read = wl.get(r).is_some_and(|&w| w >= msg.event_id);
                    if has_read {
                        spans.push(Span::styled(" \u{2713}", Theme::delivery()));
                    }
                }
            }
        }

        // Intent badge
        if let Some(ref intent) = msg.intent {
            let (label, color) = match intent.as_str() {
                "request" => ("req", palette::ORANGE),
                "ack" => ("ack", palette::FG_DIM),
                _ => (intent.as_str(), palette::FG_DIM),
            };
            spans.push(Span::styled(
                format!(" [{}]", label),
                Style::default().fg(color),
            ));
        }

        // Reply-to reference
        if let Some(id) = msg.reply_to {
            spans.push(Span::styled(
                format!(" \u{21b5}{}", id),
                Style::default().fg(palette::FG_DIM),
            ));
        }
    }
}

fn matches_search(query: &str, text: &str) -> bool {
    text.to_lowercase().contains(&query.to_lowercase())
}

fn msg_matches(msg: &Message, query: &str) -> bool {
    matches_search(query, &msg.body)
        || matches_search(query, &msg.sender)
        || msg.recipients.iter().any(|r| matches_search(query, r))
}

fn event_matches(ev: &Event, query: &str) -> bool {
    matches_search(query, &ev.tool)
        || matches_search(query, &ev.detail)
        || matches_search(query, &ev.agent)
}

/// Resolve tool name to display color.
fn tool_color(tool: &str) -> Color {
    match tool {
        "Edit" | "Write" | "write_file" | "apply_patch" | "replace" => palette::YELLOW,
        "Bash" | "shell" | "run_shell_command" => palette::TEAL,
        "Grep" | "WebSearch" | "grep_search" | "search_file_content" | "google_web_search" => {
            palette::MAGENTA
        }
        _ => palette::BLUE,
    }
}

fn search_counts(app: &App) -> (usize, usize) {
    let query = match app.active_search_query() {
        Some(q) => q,
        None => return (0, 0),
    };

    if !app.ui.selected.is_empty() {
        let mut total = 0usize;
        let mut matched = 0usize;

        for ev in &app.data.events {
            if app.ui.selected.contains(&ev.agent) {
                total += 1;
                if event_matches(ev, query) {
                    matched += 1;
                }
            }
        }
        for msg in &app.data.messages {
            if app.ui.selected.contains(&msg.sender)
                || msg.recipients.iter().any(|r| app.ui.selected.contains(r))
                || (msg.recipients.is_empty() && !msg.is_system())
            {
                total += 1;
                if msg_matches(msg, query) {
                    matched += 1;
                }
            }
        }
        (matched, total)
    } else {
        let total = app.data.messages.len();
        let matched = app
            .data
            .messages
            .iter()
            .filter(|m| msg_matches(m, query))
            .count();
        (matched, total)
    }
}

/// Counts for events timeline: (search_matched, total) respecting agent selection.
/// When no search query, matched == total.
fn events_search_counts(app: &App) -> (usize, usize) {
    let agents = if app.ui.selected.is_empty() {
        None
    } else {
        Some(&app.ui.selected)
    };
    let agent_ev = |e: &&Event| agents.is_none_or(|a| a.contains(&e.agent));
    let agent_msg = |m: &&Message| {
        agents.is_none_or(|a| {
            a.contains(&m.sender)
                || m.recipients.iter().any(|r| a.contains(r))
                || (m.recipients.is_empty() && !m.is_system())
        })
    };
    let total = (app.data.events.iter().filter(agent_ev).count()
        + app.data.messages.iter().filter(agent_msg).count())
    .min(DISPLAY_LIMIT);
    let query = match app.active_search_query() {
        Some(q) => q,
        None => return (total, total),
    };
    let matched = app
        .data
        .events
        .iter()
        .filter(agent_ev)
        .filter(|e| event_matches(e, query))
        .count()
        + app
            .data
            .messages
            .iter()
            .filter(agent_msg)
            .filter(|m| msg_matches(m, query))
            .count();
    (matched.min(total), total)
}

/// Returns a short count string like "[42]" or "[3/42]" for the panel heading and inline separator.
pub fn display_count_str(app: &App) -> String {
    if app.ui.show_events {
        let (matched, total) = events_search_counts(app);
        if app.active_search_query().is_some() {
            format!("[{}/{}]", matched, total)
        } else {
            format!("[{}]", total)
        }
    } else if !app.ui.selected.is_empty() {
        let (matched, total) = search_counts(app);
        if app.active_search_query().is_some() {
            format!("[{}/{}]", matched, total)
        } else {
            format!("[{}]", total)
        }
    } else {
        let total = app.data.messages.len().min(DISPLAY_LIMIT);
        if app.active_search_query().is_some() {
            let (matched, _) = search_counts(app);
            format!("[{}/{}]", matched, total)
        } else {
            format!("[{}]", total)
        }
    }
}

fn render_command_output(
    frame: &mut Frame,
    area: Rect,
    output: &[String],
    msg_scroll: usize,
) -> usize {
    let mut lines: Vec<Line> = Vec::new();
    for line in output {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(line.clone(), Style::default().fg(palette::FG)),
        ]));
    }

    render_scrolled(frame, area, lines, msg_scroll)
}

/// Render lines with bottom-anchored scroll and an auto-hiding scrollbar.
/// Returns `max_scroll` so callers can clamp `msg_scroll`.
fn render_scrolled(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'_>>,
    scroll_from_bottom: usize,
) -> usize {
    let total = lines.len();
    let visible = area.height as usize;
    let max_scroll = total.saturating_sub(visible);
    let effective = scroll_from_bottom.min(max_scroll);
    let scroll_pos = max_scroll - effective;

    let paragraph = Paragraph::new(lines).scroll((scroll_pos.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);

    if total > visible {
        let mut state = ScrollbarState::new(total).position(scroll_pos);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some(" "))
                .thumb_style(Style::default().fg(palette::FG_DIM)),
            area,
            &mut state,
        );
    }

    max_scroll
}
