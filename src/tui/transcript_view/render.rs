//! Conversation tab: tab strip, header line and turn list.
use std::time::Instant;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::convert::fill_wake_requests;
use super::markdown;
use super::model::{
    ConversationSnapshot, ConversationTarget, RightTab, ToolSummary, TurnView, remote_account_key,
    resolve_target,
};
use super::prompt::ScreenPrompt;
use crate::tui::app::App;
use crate::tui::model::{Agent, AgentStatus, Message, format_duration_short};
use crate::tui::render::SPINNER_FRAMES;
use crate::tui::render::messages::{ScrollMetrics, render_scrolled};
use crate::tui::render::{fit_spans, truncate_display};
use crate::tui::theme::{Theme, palette};

const TAB_ACTIVITY: &str = "[activity]";
const TAB_CONVERSATION: &str = "[chat]";

fn tabstrip_width() -> usize {
    TAB_ACTIVITY.width() + 1 + TAB_CONVERSATION.width()
}

/// The heading row left of the tab strip.
pub fn heading_area(area: Rect) -> Rect {
    Rect {
        width: area.width.saturating_sub(tabstrip_width() as u16 + 1),
        ..area
    }
}

/// Draw `[activity] [chat]` at the right end of the heading row.
pub fn render_tabstrip(frame: &mut Frame, area: Rect, tab: RightTab) {
    let w = tabstrip_width() as u16;
    if area.width < w {
        return;
    }
    let style = |on: bool| if on { Theme::title() } else { Theme::dim() };
    let line = Line::from(vec![
        Span::styled(TAB_ACTIVITY, style(tab == RightTab::Activity)),
        Span::raw(" "),
        Span::styled(TAB_CONVERSATION, style(tab == RightTab::Conversation)),
    ]);
    let x = area.x + area.width - w;
    frame.render_widget(Paragraph::new(line), Rect::new(x, area.y, w, 1));
}

pub fn render_conversation(frame: &mut Frame, head: Rect, body: Rect, app: &App) -> ScrollMetrics {
    let now = Instant::now();
    let head_w = (head.width as usize).saturating_sub(tabstrip_width() + 1);
    let width = body.width.saturating_sub(1) as usize;
    let target = resolve_target(&app.data, app.cursor_target());
    let live_agent = target_agent(&target);

    let (header, lines): (Vec<Span<'static>>, Vec<Line<'static>>) = match target {
        ConversationTarget::NoAgent => (
            vec![Span::styled("  chat", Theme::dim())],
            status_line("select an agent"),
        ),
        ConversationTarget::Unsupported(agent) | ConversationTarget::LocalNoPath(agent) => (
            header_line(agent, None, &ConversationSnapshot::default(), now, head_w),
            status_line("no conversation"),
        ),
        ConversationTarget::Local { agent, .. } => {
            let snap = app.transcript.local.snapshot(agent);
            let expanded = app.transcript.is_expanded(agent);
            (
                header_line(agent, snap.email, &snap, now, head_w),
                snapshot_lines(&snap, &agent.name, &app.data.messages, expanded, width),
            )
        }
        ConversationTarget::Remote { agent, .. } => {
            let snap = app.transcript.remote.snapshot(agent);
            let email = remote_account_key(agent)
                .and_then(|k| app.data.remote_accounts.get(&k))
                .map(String::as_str);
            let expanded = app.transcript.is_expanded(agent);
            let key = remote_account_key(agent).unwrap_or_else(|| agent.name.clone());
            (
                header_line(agent, email, &snap, now, head_w),
                snapshot_lines(&snap, &key, &app.data.messages, expanded, width),
            )
        }
    };

    let mut lines = lines;
    if let Some(agent) = live_agent
        && let Some(live) = live_line(agent, app.ui.tick, width)
    {
        lines.push(live);
    }
    if let Some((_, prompt)) = app.waiting_prompt() {
        lines.extend(prompt_lines(prompt, width));
    }

    let head_rect = Rect::new(head.x, head.y, head_w as u16, head.height.min(1));
    frame.render_widget(
        Paragraph::new(Line::from(fit_spans(header, head_w))),
        head_rect,
    );
    render_scrolled(frame, body, lines, app.ui.msg_scroll, app.ui.last_msg_total)
}

fn target_agent<'a>(target: &ConversationTarget<'a>) -> Option<&'a Agent> {
    match target {
        ConversationTarget::Local { agent, .. } | ConversationTarget::Remote { agent, .. } => {
            Some(agent)
        }
        _ => None,
    }
}

/// While the agent works: a spinner, what it is doing, and for how long.
/// While it waits on a prompt in its terminal: what it waits on, and that
/// messages are held until then.
pub fn live_line(agent: &Agent, tick: u64, width: usize) -> Option<Line<'static>> {
    if width == 0 {
        return None;
    }
    if agent.status == AgentStatus::Blocked {
        let waiting = match agent.context_display() {
            c if c.is_empty() => "blocked".to_string(),
            c => format!("blocked: {}", c.lines().next().unwrap_or("")),
        };
        return Some(Line::from(Span::styled(
            fit(
                format!(
                    "■ {waiting} · messages wait until answered · {}",
                    agent.age_display()
                ),
                width,
            ),
            Theme::blocked(),
        )));
    }
    if agent.status != AgentStatus::Active {
        return None;
    }
    let frame = SPINNER_FRAMES[(tick as usize / 2) % SPINNER_FRAMES.len()];
    let doing = if agent.status_context.starts_with("tool:") {
        agent.context_display()
    } else {
        "thinking".to_string()
    };
    let age = format!(" · {}", agent.age_display());
    let room = width.saturating_sub(frame.width() + 1 + age.width());
    let doing = truncate_display(doing.lines().next().unwrap_or(""), room.max(1));
    Some(Line::from(Span::styled(
        fit(format!("{frame} {doing}{age}"), width),
        Theme::active(),
    )))
}

/// The prompt waiting on the agent's screen, as numbered options, and the
/// keys that answer it from here.
pub fn prompt_lines(prompt: &ScreenPrompt, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let dim = Theme::dim();
    let mut lines = Vec::new();
    // Wrapped rows line up under the text after `lead`.
    let push =
        |lead: markdown::Segment, body: Vec<markdown::Segment>, lines: &mut Vec<Line<'static>>| {
            let hang = lead.0.width();
            if hang >= width {
                let mut segments = vec![lead];
                segments.extend(body);
                for row in markdown::wrap(&segments, width) {
                    lines.push(Line::from(markdown::spans(row)));
                }
                return;
            }
            for (r, row) in markdown::wrap(&body, width - hang).into_iter().enumerate() {
                let mut spans = if r == 0 {
                    vec![Span::styled(lead.0.clone(), lead.1)]
                } else {
                    vec![Span::raw(" ".repeat(hang))]
                };
                spans.extend(markdown::spans(row));
                lines.push(Line::from(spans));
            }
        };
    let indent = || ("  ".to_string(), Style::default());
    if let Some(tabs) = &prompt.tabs {
        push(indent(), vec![(tabs.clone(), Theme::blocked())], &mut lines);
    }
    for text in &prompt.question {
        push(indent(), vec![(text.clone(), Theme::title())], &mut lines);
    }
    for option in &prompt.options {
        let mark = match option.checked {
            None => "",
            Some(true) => "[x] ",
            Some(false) => "[ ] ",
        };
        let style = if option.needs_typing() {
            dim
        } else {
            Style::default()
        };
        let lead = format!("   {mark}{} ", option.number);
        let under = " ".repeat(lead.width());
        push(
            (lead, Style::default().fg(palette::FG_DIM)),
            vec![(option.label.clone(), style)],
            &mut lines,
        );
        // The screen's own layout: the description sits under the label.
        // A label too long for the screen also continues there.
        if !option.description.is_empty() {
            push(
                (under, Style::default()),
                vec![(option.description.clone(), dim)],
                &mut lines,
            );
        }
    }
    let last = prompt
        .options
        .iter()
        .filter(|o| !o.needs_typing())
        .map(|o| o.number)
        .max()
        .unwrap_or(1);
    let hint = if prompt.notes_in_use() {
        "notes are open · answer in the terminal".to_string()
    } else if prompt.is_multi() {
        format!("1-{last} toggle · enter next · typing answers in the terminal")
    } else {
        format!("1-{last} answer · typing answers in the terminal")
    };
    push(indent(), vec![(hint, dim)], &mut lines);
    lines
}

fn status_line(text: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(format!("  {text}"), Theme::dim()))]
}

/// `agent` is the name messages mention it by.
fn snapshot_lines(
    snap: &ConversationSnapshot,
    agent: &str,
    messages: &[Message],
    full: bool,
    width: usize,
) -> Vec<Line<'static>> {
    match snap.turns {
        None if snap.failure.is_some() => status_line("could not load the conversation"),
        None => status_line("loading…"),
        Some([]) => status_line("no conversation"),
        Some(turns) => {
            let mut turns = turns.to_vec();
            fill_wake_requests(&mut turns, agent, messages);
            body_lines(&turns, full, width)
        }
    }
}

/// `name · tool · email · refresh`. When too wide, email shrinks first, then
/// the tool; name and refresh state are kept.
pub fn header_line(
    agent: &Agent,
    email: Option<&str>,
    snap: &ConversationSnapshot,
    now: Instant,
    width: usize,
) -> Vec<Span<'static>> {
    let name = agent.display_name();
    let mut tool = agent.tool.name().to_string();
    let mut email = email.unwrap_or("—").to_string();
    let refresh = refresh_label(snap, now);
    let sep = " · ";
    let fixed = 2
        + name.width()
        + if refresh.is_empty() {
            0
        } else {
            sep.width() + refresh.width()
        };
    let used = |tool: &str, email: &str| fixed + 2 * sep.width() + tool.width() + email.width();
    if used(&tool, &email) > width {
        let over = used(&tool, &email) - width;
        email = truncate_display(&email, email.width().saturating_sub(over).max(1));
    }
    if used(&tool, &email) > width {
        let over = used(&tool, &email) - width;
        tool = truncate_display(&tool, tool.width().saturating_sub(over).max(1));
    }
    let dot = || Span::styled(sep, Theme::dim());
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(name, Theme::title()),
        dot(),
        Span::styled(tool, Style::default().fg(palette::FG_DIM)),
        dot(),
        Span::styled(email, Style::default().fg(palette::FG_DIM)),
    ];
    if !refresh.is_empty() {
        let style = if snap.failure.is_some() {
            Theme::blocked()
        } else {
            Theme::dim()
        };
        spans.push(dot());
        spans.push(Span::styled(refresh, style));
    }
    spans
}

pub fn refresh_label(snap: &ConversationSnapshot, now: Instant) -> String {
    let ago = |t: Instant| match format_duration_short(now.saturating_duration_since(t).as_secs()) {
        d if d == "now" => "just now".to_string(),
        d => format!("{d} ago"),
    };
    match (snap.failure, snap.last_success) {
        (Some(reason), Some(t)) => format!("as of {} · refresh failed ({reason})", ago(t)),
        (Some(reason), None) => format!("refresh failed ({reason})"),
        (None, Some(t)) => format!("updated {}", ago(t)),
        (None, None) if snap.loading => "loading".to_string(),
        (None, None) => String::new(),
    }
}

fn fit(s: String, width: usize) -> String {
    truncate_display(&s, width)
}

/// Turns, oldest first. Compact: the latest turn is expanded and earlier
/// ones are one line each. Full: every turn with its whole text, wrapped.
pub fn body_lines(turns: &[TurnView], full: bool, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::raw("")];
    }
    let mut lines = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        if full {
            full_turn(turn, width, &mut lines);
        } else if i + 1 == turns.len() {
            expanded_turn(turn, width, &mut lines);
        } else {
            lines.push(Line::from(Span::styled(
                fit(collapsed_turn(turn), width),
                Theme::dim(),
            )));
        }
    }
    lines
}

fn collapsed_turn(turn: &TurnView) -> String {
    let request = if turn.request_summary.is_empty() {
        "(no request)".to_string()
    } else {
        markdown::plain(&turn.request_summary)
    };
    let response = if turn.response_summary.is_empty() {
        "(no response)".to_string()
    } else {
        markdown::plain(&turn.response_summary)
    };
    let tag = match turn.tools.total() {
        Some(0) => String::new(),
        Some(n) => format!("  Tool×{n}"),
        None => "  Tool —".to_string(),
    };
    format!(
        "▸ #{} {} {request} → {response}{tag}",
        turn.position, turn.time
    )
}

fn turn_rule(turn: &TurnView, width: usize, lines: &mut Vec<Line<'static>>) {
    let rule = format!("── #{} · {} ", turn.position, turn.time);
    let fill = width.saturating_sub(rule.width());
    lines.push(Line::from(Span::styled(
        fit(format!("{rule}{}", "─".repeat(fill)), width),
        Theme::dim(),
    )));
}

/// Text lines under a turn marker (`▶ `, `◀ `), Markdown-styled: the marker
/// on the first row, two spaces before the rest. `wrap` folds long lines;
/// otherwise each is cut to `width`. `in_code` is the fence state the
/// first line starts in.
#[allow(clippy::too_many_arguments)]
fn push_text(
    text: &[String],
    marker: &str,
    base: Style,
    width: usize,
    wrap: bool,
    mut in_code: bool,
    more: bool,
    lines: &mut Vec<Line<'static>>,
) {
    let inner = width.saturating_sub(2).max(1);
    let mut first = true;
    let last = text.len().saturating_sub(1);
    let mut i = 0;
    while i < text.len() {
        // A run of `|` rows is laid out as one table when it fits.
        if !in_code && markdown::is_table_row(&text[i]) {
            let end = (i..text.len())
                .find(|&j| !markdown::is_table_row(&text[j]))
                .unwrap_or(text.len());
            if let Some(rows) = markdown::table(&text[i..end], base, inner) {
                for row in rows {
                    let prefix = if first { marker } else { "  " };
                    first = false;
                    let mut spans = vec![Span::styled(prefix.to_string(), base)];
                    spans.extend(markdown::spans(row));
                    lines.push(Line::from(fit_spans(spans, width)));
                }
                i = end;
                continue;
            }
        }
        let raw = &text[i];
        let at_last = i == last;
        i += 1;
        let Some(mut segs) = markdown::line(raw, &mut in_code, base) else {
            continue;
        };
        if more && at_last {
            segs.push(("…".into(), base));
        }
        // Wrapped rows of a bullet line up under its text.
        let hang = match segs.first() {
            Some((lead, _)) if lead.ends_with("• ") => lead.width(),
            _ => 0,
        };
        let rows = if wrap {
            markdown::wrap(&segs, inner.saturating_sub(hang).max(1))
        } else {
            vec![segs]
        };
        for (r, row) in rows.into_iter().enumerate() {
            let prefix = if first { marker } else { "  " };
            first = false;
            let mut spans = vec![Span::styled(prefix.to_string(), base)];
            if r > 0 && hang > 0 {
                spans.push(Span::raw(" ".repeat(hang)));
            }
            spans.extend(markdown::spans(row));
            lines.push(Line::from(fit_spans(spans, width)));
        }
    }
}

fn plain_line(text: &str, style: Style, width: usize, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(Span::styled(
        fit(text.to_string(), width),
        style,
    )));
}

fn full_turn(turn: &TurnView, width: usize, lines: &mut Vec<Line<'static>>) {
    turn_rule(turn, width, lines);
    let req = Style::default().fg(palette::BLUE);
    if turn.request_full.is_empty() {
        plain_line("▶ (no request)", req, width, lines);
    }
    push_text(
        &turn.request_full,
        "▶ ",
        req,
        width,
        true,
        false,
        false,
        lines,
    );
    if let Some(tools) = tools_line(&turn.tools) {
        let dim = Style::default().fg(palette::FG_DIM);
        for row in markdown::wrap(&[(tools, dim)], width.max(1)) {
            lines.push(Line::from(markdown::spans(row)));
        }
    }
    let resp = Style::default().fg(palette::FG);
    if turn.response_full.is_empty() {
        plain_line("◀ (no response)", resp, width, lines);
    }
    push_text(
        &turn.response_full,
        "◀ ",
        resp,
        width,
        true,
        false,
        false,
        lines,
    );
}

fn expanded_turn(turn: &TurnView, width: usize, lines: &mut Vec<Line<'static>>) {
    turn_rule(turn, width, lines);

    let req = Style::default().fg(palette::BLUE);
    if turn.request_lines.is_empty() {
        plain_line("▶ (no request)", req, width, lines);
    }
    push_text(
        &turn.request_lines,
        "▶ ",
        req,
        width,
        false,
        false,
        turn.request_more,
        lines,
    );

    if let Some(tools) = tools_line(&turn.tools) {
        plain_line(&tools, Style::default().fg(palette::FG_DIM), width, lines);
    }

    let resp = Style::default().fg(palette::FG);
    if turn.response_tail.is_empty() {
        plain_line("◀ (no response)", resp, width, lines);
        return;
    }
    let marker = if turn.response_omitted > 0 {
        plain_line(
            &format!("◀ … ({} earlier lines)", turn.response_omitted),
            Theme::dim(),
            width,
            lines,
        );
        "  "
    } else {
        "◀ "
    };
    // A fence opened in the omitted part leaves the tail inside code.
    let omitted_fences = markdown::fence_count(&turn.response_full)
        .saturating_sub(markdown::fence_count(&turn.response_tail));
    let in_code = omitted_fences % 2 == 1;
    push_text(
        &turn.response_tail,
        marker,
        resp,
        width,
        false,
        in_code,
        false,
        lines,
    );
}

fn tools_line(tools: &ToolSummary) -> Option<String> {
    match tools {
        ToolSummary::None => None,
        ToolSummary::Unknown => Some("⚙ —".into()),
        ToolSummary::Some {
            counts,
            files,
            extra_files,
        } => {
            let mut s = format!(
                "⚙ {}",
                counts
                    .iter()
                    .map(|(n, c)| format!("{n}×{c}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            if !files.is_empty() {
                s.push_str(" · ");
                s.push_str(&files.join(", "));
                if *extra_files > 0 {
                    s.push_str(&format!(" +{extra_files}"));
                }
            }
            Some(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn turn(position: usize, req: &str, resp: &str, tools: ToolSummary) -> TurnView {
        TurnView {
            position,
            time: "16:02".into(),
            request_lines: if req.is_empty() {
                vec![]
            } else {
                vec![req.into()]
            },
            request_more: false,
            request_summary: req.into(),
            response_tail: if resp.is_empty() {
                vec![]
            } else {
                vec![resp.into()]
            },
            response_omitted: 0,
            response_summary: resp.into(),
            tools,
            request_full: req.lines().map(String::from).collect(),
            response_full: resp.lines().map(String::from).collect(),
            started_at: None,
            hcom_wake: false,
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn prompt_lists_screen_options() {
        let screen: Vec<String> = "────────────────────────────────
←  ☒ fruit  ☐ drink  ✔ Submit  →

Drink?

❯ 1. [✔] Water
         Still water
  2. [ ] Tea
  3. [ ] Type something
     Submit

Enter to select · Esc to cancel"
            .lines()
            .map(String::from)
            .collect();
        let prompt = super::super::prompt::parse_screen(&screen).unwrap();
        let out = text(&prompt_lines(&prompt, 80));
        assert_eq!(
            out,
            vec![
                "  ☒ fruit  ☐ drink  ✔ Submit",
                "  Drink?",
                "   [x] 1 Water",
                "         Still water",
                "   [ ] 2 Tea",
                "   [ ] 3 Type something",
                "  1-2 toggle · enter next · typing answers in the terminal",
            ]
        );
        for w in 2..20 {
            for line in text(&prompt_lines(&prompt, w)) {
                assert!(line.width() <= w, "{line:?} at {w}");
            }
        }
        assert!(prompt_lines(&prompt, 0).is_empty());
    }

    #[test]
    fn wrapped_option_rows_hang_under_the_label() {
        let screen: Vec<String> = "────────────────────────────────
 Edit file
 format.py
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 1 -old
 1 +new
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 Do you want to make this edit to format.py?
 ❯ 1. Yes
   2. Yes, and switch to accept edits (auto-approve file edits and common file
      commands) for this session (shift+tab)
   3. No

 Esc to cancel · Tab to amend"
            .lines()
            .map(String::from)
            .collect();
        let prompt = super::super::prompt::parse_screen(&screen).unwrap();
        let out = text(&prompt_lines(&prompt, 40));
        let at = out.iter().position(|l| l.starts_with("   2 ")).unwrap();
        assert_eq!(out[at].trim_end(), "   2 Yes, and switch to accept edits");
        // Every row up to option 3 continues under the label.
        let rest: Vec<&String> = out[at + 1..]
            .iter()
            .take_while(|l| !l.starts_with("   3 "))
            .collect();
        assert!(rest.len() >= 2, "{out:?}");
        for line in rest {
            assert!(line.starts_with("     ") && line.width() <= 40, "{line:?}");
        }
    }

    #[test]
    fn latest_turn_expanded_previous_collapsed() {
        let tools = ToolSummary::Some {
            counts: vec![("Bash".into(), 3)],
            files: vec![],
            extra_files: 0,
        };
        let turns = vec![
            turn(12, "analyze it", "feasible", tools.clone()),
            turn(13, "fork", "created", ToolSummary::Unknown),
            turn(14, "run the tests", "6 passed", tools),
        ];
        let out = text(&body_lines(&turns, false, 80));
        assert_eq!(out[0], "▸ #12 16:02 analyze it → feasible  Tool×3");
        assert_eq!(out[1], "▸ #13 16:02 fork → created  Tool —");
        assert!(out[2].starts_with("── #14 · 16:02 "));
        assert_eq!(out[3], "▶ run the tests");
        assert_eq!(out[4], "⚙ Bash×3");
        assert_eq!(out[5], "◀ 6 passed");

        let all = text(&body_lines(&turns, true, 80));
        assert!(all[0].starts_with("── #12"));
    }

    #[test]
    fn empty_request_and_response() {
        let out = text(&body_lines(
            &[turn(1, "", "", ToolSummary::None)],
            false,
            80,
        ));
        assert_eq!(out[1], "▶ (no request)");
        assert_eq!(out[2], "◀ (no response)");
    }

    #[test]
    fn narrow_width_never_panics() {
        let t = turn(
            1,
            "ｗｉｄｅ ask 🦀🦀🦀",
            "ｒｅｐｌｙ 🦀",
            ToolSummary::Unknown,
        );
        for w in 0..12 {
            for l in text(&body_lines(std::slice::from_ref(&t), true, w)) {
                assert!(l.width() <= w.max(1), "{l:?} at {w}");
            }
        }
    }

    #[test]
    fn refresh_labels() {
        let now = Instant::now() + Duration::from_secs(100);
        let t = now - Duration::from_secs(45);
        let mut snap = ConversationSnapshot {
            last_success: Some(t),
            ..Default::default()
        };
        assert_eq!(refresh_label(&snap, now), "updated 45s ago");
        let fresh = ConversationSnapshot {
            last_success: Some(now),
            ..Default::default()
        };
        assert_eq!(refresh_label(&fresh, now), "updated just now");
        snap.failure = Some("offline");
        assert_eq!(
            refresh_label(&snap, now),
            "as of 45s ago · refresh failed (offline)"
        );
        snap.last_success = None;
        assert_eq!(refresh_label(&snap, now), "refresh failed (offline)");
    }

    #[test]
    fn header_shrinks_email_first() {
        let agent = crate::tui::test_helpers::make_test_agent("rina", 0.0);
        let snap = ConversationSnapshot::default();
        let now = Instant::now();
        let joined = |s: Vec<Span>| s.iter().map(|s| s.content.to_string()).collect::<String>();
        let full = joined(header_line(
            &agent,
            Some("someone@example.com"),
            &snap,
            now,
            200,
        ));
        assert!(
            full.contains("rina · claude · someone@example.com"),
            "{full}"
        );
        let none = joined(header_line(&agent, None, &snap, now, 200));
        assert!(none.ends_with("· —"), "{none}");
        let narrow = joined(header_line(
            &agent,
            Some("someone@example.com"),
            &snap,
            now,
            24,
        ));
        assert!(narrow.contains("rina"));
        assert!(narrow.contains('…'));
    }

    #[test]
    fn full_mode_shows_every_turn_wrapped() {
        let mut first = turn(1, "q1", "a1", ToolSummary::None);
        first.response_full = vec!["ＡＢＣＤＥＦ".into(), "".into(), "end".into()];
        let second = turn(2, "q2", "a2", ToolSummary::None);
        let out = text(&body_lines(&[first, second], true, 8));
        assert_eq!(
            out,
            vec![
                "── #1 ·…",
                "▶ q1",
                "◀ ＡＢＣ",
                "  ＤＥＦ",
                "  ",
                "  end",
                "── #2 ·…",
                "▶ q2",
                "◀ a2",
            ]
        );
    }

    #[test]
    fn live_line_while_active_or_blocked() {
        use crate::tui::test_helpers::make_test_agent;
        let mut a = make_test_agent("luna", 0.0);
        a.status = AgentStatus::Listening;
        assert!(live_line(&a, 0, 40).is_none());

        a.status = AgentStatus::Active;
        a.status_context = "tool:Bash".into();
        a.status_detail = "cargo test\nsecond line".into();
        let l = text(&[live_line(&a, 0, 40).unwrap()]).remove(0);
        assert!(l.starts_with("⠋ Bash: cargo test"), "{l}");
        let next = text(&[live_line(&a, 2, 40).unwrap()]).remove(0);
        assert!(next.starts_with("⠙ "), "{next}");

        a.status_context = "prompt".into();
        let l = text(&[live_line(&a, 0, 40).unwrap()]).remove(0);
        assert!(l.contains("thinking"), "{l}");
        for w in 1..12 {
            let l = text(&[live_line(&a, 0, w).unwrap()]).remove(0);
            assert!(l.width() <= w, "{l:?} at {w}");
        }

        // Waiting on a prompt: says so, and that messages are held.
        a.status = AgentStatus::Blocked;
        a.status_context = "approval".into();
        a.status_detail = String::new();
        let line = live_line(&a, 0, 80).unwrap();
        let l = text(std::slice::from_ref(&line)).remove(0);
        assert!(
            l.starts_with("■ blocked: approval · messages wait until answered"),
            "{l}"
        );
        assert_eq!(line.spans[0].style, Theme::blocked());
        for w in 1..12 {
            let l = text(&[live_line(&a, 0, w).unwrap()]).remove(0);
            assert!(l.width() <= w, "{l:?} at {w}");
        }
    }

    #[test]
    fn markdown_in_turns() {
        let mut t = turn(
            1,
            "**request**",
            "## Result\n- `a.rs` changed",
            ToolSummary::None,
        );
        t.response_tail = vec!["## Result".into(), "- `a.rs` changed".into()];
        t.response_summary = "- `a.rs` changed".into();
        let out = text(&body_lines(std::slice::from_ref(&t), false, 40));
        assert_eq!(out[1], "▶ request");
        assert_eq!(out[2], "◀ Result");
        assert_eq!(out[3], "  • a.rs changed");
        // Collapsed summaries drop the markers.
        let out = text(&body_lines(
            &[t.clone(), turn(2, "q", "a", ToolSummary::None)],
            false,
            60,
        ));
        assert!(out[0].contains("request → • a.rs changed"), "{}", out[0]);

        // The tail starts inside a block opened in the omitted part.
        let mut code = turn(3, "q", "", ToolSummary::None);
        code.response_full = vec!["```".into(), "let **x** = 1;".into(), "```".into()];
        code.response_tail = vec!["let **x** = 1;".into(), "```".into()];
        code.response_omitted = 1;
        let out = text(&body_lines(std::slice::from_ref(&code), false, 40));
        assert_eq!(out.last().unwrap(), "  let **x** = 1;");
    }

    #[test]
    fn wrapped_bullet_hangs_under_its_text() {
        let mut t = turn(1, "q", "", ToolSummary::None);
        t.response_full = vec!["- aaa bbb ccc".into()];
        let out = text(&body_lines(std::slice::from_ref(&t), true, 12));
        assert_eq!(out[2..], ["◀ • aaa ", "    bbb ccc"]);
    }
}
