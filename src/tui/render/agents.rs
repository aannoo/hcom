use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::tui::app::App;
use crate::tui::model::*;
use crate::tui::theme::{Theme, palette};

/// Shorten a directory path by replacing the home directory prefix with `~`.
fn shorten_dir(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home.as_ref()) {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

/// Indicator color for tab strip entries (blocked > unread > none).
fn tab_indicator_color(agent: &Agent) -> Option<ratatui::style::Color> {
    if agent.is_pty_blocked() {
        Some(palette::ORANGE)
    } else if agent.unread > 0 {
        Some(palette::YELLOW)
    } else {
        None
    }
}

/// Cursor/selection indicator: `❯` if cursor, `●` if selected, blank otherwise.
fn cursor_indicator(is_cursor: bool, is_selected: bool) -> (&'static str, Style) {
    if is_cursor {
        ("\u{276f} ", Theme::cursor())
    } else if is_selected {
        ("\u{25cf} ", Style::default().fg(palette::BLUE))
    } else {
        ("  ", Style::default())
    }
}

/// Agent name style: bold blue if selected, normal bold if not.
fn selected_name_style(is_selected: bool) -> Style {
    if is_selected {
        Style::default()
            .fg(palette::BLUE)
            .add_modifier(Modifier::BOLD)
    } else {
        Theme::agent_name()
    }
}

pub(crate) const SPINNER_FRAMES: &[&str] = &[
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];

/// Pad left spans and append right spans so the result fills `width`.
fn append_right_aligned(left: &mut Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) {
    let right = super::fit_spans(right, width as usize);
    let right_w: usize = right.iter().map(|s| s.width()).sum();
    *left = super::fit_spans(
        std::mem::take(left),
        (width as usize).saturating_sub(right_w),
    );
    let left_w: usize = left.iter().map(|s| s.width()).sum();
    let pad = (width as usize).saturating_sub(left_w + right_w);
    left.push(Span::raw(" ".repeat(pad)));
    left.extend(right);
}

/// Resolve status icon, handling Launching spinner and Adhoc neutral dot.
fn agent_icon(agent: &Agent, tick: u64) -> &'static str {
    if agent.status == AgentStatus::Launching {
        SPINNER_FRAMES[(tick as usize / 2) % SPINNER_FRAMES.len()]
    } else if agent.status != AgentStatus::Listening
        && let Some(icon) = agent.tool.spec().and_then(|spec| spec.adhoc_icon)
    {
        icon
    } else {
        agent.status.icon()
    }
}

/// Compact tool prefix for multi-tool display.
fn tool_prefix_str(tool: &AgentTool) -> String {
    match tool {
        AgentTool::Unknown(raw) => raw.chars().take(4).collect(),
        AgentTool::Known(tool) => tool.spec().tui_prefix.to_string(),
    }
}

fn collect_agent_lines(app: &App, width: u16, max_visible: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    // Show tool prefix only when agents use different tools
    let all_agents: Vec<&Agent> = app
        .data
        .agents
        .iter()
        .chain(app.data.remote_agents.iter())
        .chain(app.data.stopped_agents.iter())
        .collect();
    let tools: std::collections::HashSet<&str> = all_agents.iter().map(|a| a.tool.name()).collect();
    let multi_tool = tools.len() > 1;

    // Compute max display name width for alignment (min 4 to avoid cramping)
    let name_width = all_agents
        .iter()
        .map(|a| UnicodeWidthStr::width(a.display_name().as_str()))
        .max()
        .unwrap_or(4)
        .max(4);

    // Build complete groups first so overflow indicators and detail rows count
    // against the same budget as agent rows.
    for (i, agent) in app.data.agents.iter().enumerate() {
        let is_cursor = app.ui.cursor == i;
        let is_selected = app.ui.selected.contains(&agent.action_name());
        lines.push(compact_agent_line(
            agent,
            is_cursor,
            is_selected,
            width,
            app.ui.tick,
            multi_tool,
        ));
        if is_cursor && is_selected {
            lines.push(detail_line_info(agent, width));
        }
    }
    let mut section_lines = Vec::new();

    // ── Collapsible sections (remote / stopped / orphans) ──
    let ct = app.cursor_target();

    render_collapsible_section(
        &mut section_lines,
        usize::MAX,
        app.data.remote_agents.len(),
        "remote",
        app.ui.remote_expanded,
        matches!(ct, CursorTarget::RemoteHeader),
        width,
        palette::CYAN,
        app.data
            .remote_agents
            .iter()
            .enumerate()
            .map(|(i, ragent)| {
                let ic = matches!(ct, CursorTarget::RemoteAgent(idx) if idx == i);
                let is = app.ui.selected.contains(&ragent.action_name());
                vec![remote_agent_line(
                    ragent,
                    ic,
                    is,
                    width,
                    app.ui.tick,
                    multi_tool,
                )]
            }),
    );

    render_collapsible_section(
        &mut section_lines,
        usize::MAX,
        app.data.stopped_agents.len(),
        "stopped",
        app.ui.stopped_expanded,
        matches!(ct, CursorTarget::StoppedHeader),
        width,
        palette::FG_DIM,
        app.data
            .stopped_agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let ic = matches!(ct, CursorTarget::StoppedAgent(idx) if idx == i);
                let is = app.ui.selected.contains(&agent.action_name());
                let mut item = vec![stopped_agent_line(
                    agent, ic, is, width, multi_tool, name_width,
                )];
                if ic && is {
                    item.push(stopped_detail_line(agent, width));
                }
                item
            }),
    );

    render_collapsible_section(
        &mut section_lines,
        usize::MAX,
        app.data.orphans.len(),
        "orphans",
        app.ui.orphans_expanded,
        matches!(ct, CursorTarget::OrphanHeader),
        width,
        palette::RED,
        app.data.orphans.iter().enumerate().map(|(i, orphan)| {
            let ic = matches!(ct, CursorTarget::Orphan(idx) if idx == i);
            vec![orphan_line(orphan, ic, width)]
        }),
    );

    // Expanded sections get at most half the pane while live agents exist.
    // Each group scrolls independently to keep its cursor in view.
    let section_budget = if lines.is_empty() {
        max_visible
    } else {
        max_visible / 2 + usize::from(max_visible == 1 && app.ui.cursor >= app.data.agents.len())
    }
    .min(section_lines.len());
    let live_budget = max_visible.saturating_sub(section_budget);
    let mut visible = window_agent_rows(lines, live_budget);
    visible.extend(window_agent_rows(
        section_lines,
        max_visible.saturating_sub(visible.len()),
    ));
    visible
}

/// Reserve overflow rows before choosing content. Cursor rows carry the
/// selection background; selected agents without the cursor do not.
fn window_agent_rows(rows: Vec<Line<'static>>, height: usize) -> Vec<Line<'static>> {
    if rows.len() <= height {
        return rows;
    }
    if height == 0 {
        return Vec::new();
    }
    let cursor = rows
        .iter()
        .position(|line| line.style.bg == Some(palette::SELECTION))
        .unwrap_or(0);
    let count = height.saturating_sub(2).max(1);
    let start = compute_scroll(cursor, rows.len(), count);
    let end = (start + count).min(rows.len());
    let mut visible = Vec::new();
    if start > 0 && height > 1 {
        visible.push(Line::from(Span::styled(
            format!("  ↑ {} more", start),
            Theme::dim(),
        )));
    }
    visible.extend(rows[start..end].iter().cloned());
    if end < rows.len() && visible.len() < height {
        visible.push(Line::from(Span::styled(
            format!("  ↓ {} more", rows.len() - end),
            Theme::dim(),
        )));
    }
    visible
}

fn compute_scroll(cursor: usize, total: usize, visible: usize) -> usize {
    if total <= visible {
        return 0;
    }
    let half = visible / 2;
    if cursor <= half {
        0
    } else if cursor + half >= total {
        total - visible
    } else {
        cursor - half
    }
}

/// Truncate spans to fit within max_width, appending '…' if truncated. Pads to fill width.
fn finalize_detail_spans(parts: &mut Vec<Span<'static>>, width: u16) {
    let max = width as usize;
    let mut used: usize = 0;
    for i in 0..parts.len() {
        let sw = parts[i].width();
        if used + sw > max.saturating_sub(1) {
            // This span would overflow — truncate here, add ellipsis
            parts.truncate(i);
            parts.push(Span::styled("\u{2026}", Theme::dim()));
            break;
        }
        used += sw;
    }
    let total: usize = parts.iter().map(|s| s.width()).sum();
    let pad = max.saturating_sub(total);
    parts.push(Span::raw(" ".repeat(pad)));
}

fn detail_line_info(agent: &Agent, width: u16) -> Line<'static> {
    let mut parts: Vec<Span> = vec![
        Span::raw("    "),
        Span::styled("\u{251c} ", Theme::separator()),
        Span::styled(agent.tool.name().to_string(), Theme::dim()),
        Span::styled(" \u{00b7} ", Theme::separator()),
        Span::styled(shorten_dir(&agent.directory), Theme::dim()),
        Span::styled(" \u{00b7} ", Theme::separator()),
        Span::styled(format!("created {}", agent.created_display()), Theme::dim()),
    ];
    if agent.headless {
        parts.push(Span::styled(" \u{00b7} ", Theme::separator()));
        parts.push(Span::styled("[headless]", Theme::dim()));
    }
    if agent.unread > 0 {
        parts.push(Span::styled(" \u{00b7} ", Theme::separator()));
        parts.push(Span::styled(
            format!("{} unread", agent.unread),
            Style::default().fg(palette::YELLOW),
        ));
    }
    if let Some(ref sid) = agent.session_id {
        parts.push(Span::styled(" \u{00b7} ", Theme::separator()));
        let short: String = sid.chars().take(6).collect();
        parts.push(Span::styled(format!("session:{}", short), Theme::dim()));
    }
    if let Some(pid) = agent.pid {
        parts.push(Span::styled(" \u{00b7} ", Theme::separator()));
        parts.push(Span::styled(format!("pid:{}", pid), Theme::dim()));
    }
    if agent.is_pty_blocked() {
        parts.push(Span::styled(" \u{00b7} ", Theme::separator()));
        parts.push(Span::styled("\u{2298} ", Theme::pty_block())); // ⊘
        parts.push(Span::styled(agent.context_display(), Theme::pty_block()));
    }
    finalize_detail_spans(&mut parts, width);
    Line::from(parts).style(Style::default().bg(palette::SELECTION))
}

/// Detail line for stopped agents: exit context, directory, age.
fn stopped_detail_line(agent: &Agent, width: u16) -> Line<'static> {
    let mut parts: Vec<Span> = vec![
        Span::raw("      "),
        Span::styled("\u{2570} ", Theme::separator()),
        Span::styled(agent.tool.name().to_string(), Theme::dim()),
        Span::styled(" \u{00b7} ", Theme::separator()),
        Span::styled(agent.directory.clone(), Theme::dim()),
    ];
    if !agent.status_context.is_empty() {
        parts.push(Span::styled(" \u{00b7} ", Theme::separator()));
        parts.push(Span::styled(agent.context_display(), Theme::dim()));
    }
    finalize_detail_spans(&mut parts, width);
    Line::from(parts).style(Style::default().bg(palette::SELECTION))
}

// ── Collapsible sections ─────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_collapsible_section(
    lines: &mut Vec<Line<'static>>,
    max_height: usize,
    count: usize,
    label: &str,
    expanded: bool,
    header_is_cursor: bool,
    width: u16,
    accent: Color,
    items: impl Iterator<Item = Vec<Line<'static>>>,
) {
    if count == 0 || lines.len() >= max_height {
        return;
    }
    lines.push(collapsible_header(
        count,
        label,
        expanded,
        header_is_cursor,
        width,
        accent,
    ));
    if expanded {
        for item_lines in items {
            for line in item_lines {
                if lines.len() >= max_height {
                    break;
                }
                lines.push(line);
            }
        }
    }
}

fn collapsible_header(
    count: usize,
    label: &str,
    expanded: bool,
    is_cursor: bool,
    width: u16,
    accent: ratatui::style::Color,
) -> Line<'static> {
    let arrow = if expanded { "\u{25be}" } else { "\u{25b8}" }; // ▾ / ▸
    let cursor_str = if is_cursor { "\u{276f} " } else { "  " };
    let cursor_style = if is_cursor {
        Theme::cursor()
    } else {
        Style::default()
    };
    let text = format!("{} {} {}", arrow, count, label);

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(cursor_str.to_string(), cursor_style),
        Span::styled(text, Style::default().fg(accent)),
    ];

    let used: usize = spans.iter().map(|s| s.width()).sum();
    let pad = (width as usize).saturating_sub(used);
    spans.push(Span::raw(" ".repeat(pad)));

    let mut line = Line::from(spans);
    if is_cursor {
        line = line.style(Style::default().bg(palette::SELECTION));
    }
    line
}

fn stopped_agent_line(
    agent: &Agent,
    is_cursor: bool,
    is_selected: bool,
    width: u16,
    multi_tool: bool,
    name_width: usize,
) -> Line<'static> {
    let (cursor_str, cursor_style) = cursor_indicator(is_cursor, is_selected);

    let name_style = if is_selected {
        Style::default()
            .fg(palette::BLUE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette::FG_DIM)
    };

    let dim = Style::default().fg(palette::FG_DARK);
    let display = agent.display_name();
    let display_w = UnicodeWidthStr::width(display.as_str());
    let pad_n = name_width + 2 - display_w.min(name_width + 2);
    let name_padded = format!("{}{}", display, " ".repeat(pad_n));
    let age = agent.age_display();

    let mut left: Vec<Span> = vec![
        Span::raw("    "), // extra indent under header
        Span::styled(cursor_str.to_string(), cursor_style),
    ];
    if multi_tool {
        left.push(Span::styled(tool_prefix_str(&agent.tool), dim));
    }
    left.extend([
        Span::styled("\u{25cb} ", Style::default().fg(palette::FG_DIM)),
        Span::styled(name_padded, name_style),
        Span::styled(agent.context_display(), dim),
    ]);

    // Right: age
    let right = vec![Span::styled(format!("{:>3}", age), dim), Span::raw("  ")];
    append_right_aligned(&mut left, right, width);

    let mut line = Line::from(left);
    if is_cursor {
        line = line.style(Style::default().bg(palette::SELECTION));
    }
    line
}

fn orphan_line(orphan: &OrphanProcess, is_cursor: bool, width: u16) -> Line<'static> {
    let cursor_str = if is_cursor { "\u{276f} " } else { "  " };
    let cursor_style = if is_cursor {
        Theme::cursor()
    } else {
        Style::default()
    };
    let age = orphan.age_display();

    let mut left: Vec<Span> = vec![
        Span::raw("    "), // extra indent under header
        Span::styled(cursor_str.to_string(), cursor_style),
        Span::styled("\u{2716} ", Style::default().fg(palette::RED)), // ✖
        Span::styled(tool_prefix_str(&orphan.tool), Theme::dim()),
        Span::styled(
            format!("{:<6}", orphan.pid),
            Style::default().fg(palette::FG_DIM),
        ),
        Span::styled(
            orphan.names_display(),
            Style::default().fg(palette::FG_DARK),
        ),
    ];

    let right: Vec<Span> = vec![
        Span::styled(format!("{:>3}", age), Style::default().fg(palette::FG_DARK)),
        Span::raw("  "),
    ];

    append_right_aligned(&mut left, right, width);

    let mut line = Line::from(left);
    if is_cursor {
        line = line.style(Style::default().bg(palette::SELECTION));
    }
    line
}

fn remote_agent_line(
    ragent: &Agent,
    is_cursor: bool,
    is_selected: bool,
    width: u16,
    tick: u64,
    multi_tool: bool,
) -> Line<'static> {
    let (cursor_str, cursor_style) = cursor_indicator(is_cursor, is_selected);
    let icon = agent_icon(ragent, tick);
    let name_style = selected_name_style(is_selected);

    let display_name = ragent.display_name();
    let age = ragent.age_display();

    let mut left: Vec<Span> = vec![
        Span::raw("    "), // indent under header
        Span::styled(cursor_str.to_string(), cursor_style),
    ];
    if multi_tool {
        left.push(Span::styled(tool_prefix_str(&ragent.tool), Theme::dim()));
    }
    left.extend([
        Span::styled(icon.to_string(), ragent.status.style()),
        Span::raw(" "),
        Span::styled(display_name, name_style),
        Span::raw("  "),
        Span::styled(ragent.context_display(), Theme::agent_context()),
    ]);

    // Right: sync age + agent age
    let sync = ragent.sync_age.as_deref().unwrap_or("");
    let right: Vec<Span> = vec![
        Span::styled(format!("{}  ", sync), Style::default().fg(palette::FG_DARK)),
        Span::styled(format!("{:>3}", age), Theme::agent_dim()),
        Span::raw("  "),
    ];

    append_right_aligned(&mut left, right, width);

    let mut line = Line::from(left);
    if is_cursor {
        line = line.style(Style::default().bg(palette::SELECTION));
    }
    line
}

/// Compact agent list for vertical (side-by-side) view.
pub fn render_agents_compact(frame: &mut Frame, area: Rect, app: &App) {
    let lines = collect_agent_lines(app, area.width, area.height as usize);

    // Render lines individually so Line::render fills the full row bg
    let buf = frame.buffer_mut();
    for (i, line) in lines.into_iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let row = Rect::new(area.x, area.y + i as u16, area.width, 1);
        line.render(row, buf);
    }
}

/// Compact line for vertical view
fn compact_agent_line(
    agent: &Agent,
    is_cursor: bool,
    is_selected: bool,
    width: u16,
    tick: u64,
    multi_tool: bool,
) -> Line<'static> {
    let icon = agent_icon(agent, tick);

    // Compact uses single-char cursor (no trailing space in indicator)
    let (cursor_str, cursor_style) = if is_cursor {
        ("\u{276f}", Theme::cursor())
    } else if is_selected {
        ("\u{25cf}", Style::default().fg(palette::BLUE))
    } else {
        (" ", Style::default())
    };
    let name_style = selected_name_style(is_selected);

    let age = agent.age_display();

    let mut spans: Vec<Span> = vec![
        Span::styled(cursor_str.to_string(), cursor_style),
        Span::raw(" "),
    ];
    if multi_tool {
        spans.push(Span::styled(tool_prefix_str(&agent.tool), Theme::dim()));
    }
    spans.extend([
        Span::styled(icon.to_string(), agent.status.style()),
        Span::raw(" "),
        Span::styled(agent.display_name(), name_style),
    ]);

    // Right-align unread count + age (fixed-width slots)
    let mut right_spans: Vec<Span> = Vec::new();
    if agent.unread > 0 {
        right_spans.push(Span::styled(
            format!("{:>2}", agent.unread),
            Style::default().fg(palette::YELLOW),
        ));
    } else {
        right_spans.push(Span::raw("  "));
    }
    right_spans.push(Span::styled(format!(" {:>3}", age), Theme::agent_dim()));
    right_spans.push(Span::raw(" "));
    append_right_aligned(&mut spans, right_spans, width);

    let mut line = Line::from(spans);
    if is_cursor {
        line = line.style(Style::default().bg(palette::SELECTION));
    }
    line
}

// ══════════════════════════════════════════════════════════════════════
// Tab Strip layout — alternative inline rendering
// ══════════════════════════════════════════════════════════════════════

pub fn render_agents_tabstrip(frame: &mut Frame, area: Rect, app: &App) {
    if area.height < 2 || area.width < 10 {
        return;
    }

    let width = area.width;

    // Tab strip: 1 row
    let tab_line = build_tab_strip(app, width as usize);
    let tab_area = Rect::new(area.x, area.y, width, 1);
    frame.render_widget(Paragraph::new(tab_line), tab_area);

    // Thin separator: 1 row
    if area.height >= 3 {
        let sep = "\u{254c}".repeat(width as usize);
        let sep_area = Rect::new(area.x, area.y + 1, width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(sep, Theme::separator()))),
            sep_area,
        );
    }

    // Detail pane: remaining rows
    let detail_start = area.y + 2;
    let detail_height = area.height.saturating_sub(2);
    if detail_height == 0 {
        return;
    }
    let detail_area = Rect::new(area.x, detail_start, width, detail_height);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let ct = app.cursor_target();

    match ct {
        CursorTarget::Agent(idx) => {
            build_agent_detail(&app.data.agents[idx], app, &mut lines, width as usize);
        }
        CursorTarget::RemoteAgent(idx) => {
            build_agent_detail(
                &app.data.remote_agents[idx],
                app,
                &mut lines,
                width as usize,
            );
        }
        CursorTarget::RemoteHeader => {
            lines.push(Line::from(Span::styled(
                format!("  {} remote agents", app.data.remote_agents.len()),
                Style::default().fg(palette::CYAN),
            )));
        }
        CursorTarget::OrphanHeader => {
            lines.push(Line::from(Span::styled(
                format!("  {} orphan processes", app.data.orphans.len()),
                Style::default().fg(palette::RED),
            )));
        }
        CursorTarget::Orphan(idx) => {
            build_orphan_detail(&app.data.orphans[idx], &mut lines, width as usize);
        }
        _ => {
            lines.push(Line::from(Span::styled(
                "  No agent selected",
                Theme::dim(),
            )));
        }
    }

    // Pad to fill detail area
    while lines.len() < detail_height as usize {
        lines.push(Line::raw(""));
    }

    frame.render_widget(Paragraph::new(lines), detail_area);
}

struct TabEntry {
    text: String,
    style: Style,
    is_cursor: bool,
    is_selected: bool,
    /// Left-edge indicator color: Some(YELLOW/ORANGE) or None for no indicator.
    indicator: Option<ratatui::style::Color>,
}

fn build_tab_strip(app: &App, width: usize) -> Line<'static> {
    let mut tabs: Vec<TabEntry> = Vec::new();

    // Live agents
    for (i, agent) in app.data.agents.iter().enumerate() {
        let is_cursor = app.ui.cursor == i;
        let is_selected = app.ui.selected.contains(&agent.action_name());
        let icon = agent_icon(agent, app.ui.tick);
        let name = agent.display_name();
        let indicator = tab_indicator_color(agent);
        tabs.push(TabEntry {
            text: format!("{} {}", icon, name),
            style: agent.status.style(),
            is_cursor,
            is_selected,
            indicator,
        });
    }

    // Section headers + items (remote, orphans)
    let mut offset = app.data.agents.len();

    if !app.data.remote_agents.is_empty() {
        let is_cursor = app.ui.cursor == offset;
        let arrow = if app.ui.remote_expanded {
            "\u{25be}"
        } else {
            "\u{25b8}"
        };
        tabs.push(TabEntry {
            text: format!("{} {}r", arrow, app.data.remote_agents.len()),
            style: Style::default().fg(palette::CYAN),
            is_cursor,
            is_selected: false,
            indicator: None,
        });
        offset += 1;
        if app.ui.remote_expanded {
            for (i, ragent) in app.data.remote_agents.iter().enumerate() {
                let is_cursor = app.ui.cursor == offset + i;
                let is_selected = app.ui.selected.contains(&ragent.action_name());
                let icon = agent_icon(ragent, app.ui.tick);
                let indicator = tab_indicator_color(ragent);
                tabs.push(TabEntry {
                    text: format!("{} {}", icon, ragent.display_name()),
                    style: ragent.status.style(),
                    is_cursor,
                    is_selected,
                    indicator,
                });
            }
            offset += app.data.remote_agents.len();
        }
    }

    if !app.data.orphans.is_empty() {
        let is_cursor = app.ui.cursor == offset;
        let arrow = if app.ui.orphans_expanded {
            "\u{25be}"
        } else {
            "\u{25b8}"
        };
        tabs.push(TabEntry {
            text: format!("{} {} orphan", arrow, app.data.orphans.len()),
            style: Style::default().fg(palette::RED),
            is_cursor,
            is_selected: false,
            indicator: None,
        });
        offset += 1;
        if app.ui.orphans_expanded {
            for (i, orphan) in app.data.orphans.iter().enumerate() {
                let is_cursor = app.ui.cursor == offset + i;
                tabs.push(TabEntry {
                    text: format!("\u{2716}{}", orphan.pid),
                    style: Style::default().fg(palette::RED),
                    is_cursor,
                    is_selected: false,
                    indicator: None,
                });
            }
        }
    }

    if tabs.is_empty() {
        return Line::from(Span::styled("  No agents", Theme::dim()));
    }

    // Compute tab widths (text + 1 padding each side)
    let gap = 2usize;
    let tab_widths: Vec<usize> = tabs
        .iter()
        .map(|t| UnicodeWidthStr::width(t.text.as_str()) + 2)
        .collect();

    let cursor_tab = tabs.iter().position(|t| t.is_cursor).unwrap_or(0);
    let (vis_start, vis_end, left_overflow, right_overflow) =
        compute_tab_scroll(&tab_widths, gap, width, cursor_tab);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // Left overflow indicator
    if left_overflow > 0 {
        spans.push(Span::styled(
            format!("\u{2190}{} ", left_overflow),
            Theme::dim(),
        ));
    } else {
        spans.push(Span::raw(" "));
    }

    for (idx, tab) in tabs[vis_start..vis_end].iter().enumerate() {
        if idx > 0 {
            spans.push(Span::raw("  "));
        }

        // Background: cursor takes SELECTION; selected gets strong blue bg; indicator gets tint.
        let bg = if tab.is_cursor && tab.is_selected {
            ratatui::style::Color::Rgb(55, 70, 135) // cursor + selected
        } else if tab.is_cursor {
            palette::SELECTION
        } else if tab.is_selected {
            ratatui::style::Color::Rgb(45, 60, 120)
        } else {
            match tab.indicator {
                Some(c) if c == palette::ORANGE => ratatui::style::Color::Rgb(48, 28, 8),
                Some(_) => ratatui::style::Color::Rgb(46, 42, 14), // yellow tint for unread
                None => ratatui::style::Color::Reset,
            }
        };
        let use_bg = tab.is_cursor || tab.is_selected || tab.indicator.is_some();

        // Left edge: ▏ in indicator/selection color, or plain space
        let ind_span = if let Some(fg) = tab.indicator {
            let s = if use_bg {
                Style::default().fg(fg).bg(bg)
            } else {
                Style::default().fg(fg)
            };
            Span::styled("\u{258f}", s)
        } else if tab.is_selected || use_bg {
            Span::styled(" ", Style::default().bg(bg))
        } else {
            Span::raw(" ")
        };
        spans.push(ind_span);

        // Text + trailing space
        let text_style = if tab.is_cursor {
            if tab.is_selected {
                Style::default()
                    .fg(palette::BLUE)
                    .add_modifier(Modifier::BOLD)
                    .bg(bg)
            } else {
                tab.style.bg(bg)
            }
        } else if tab.is_selected {
            let s = Style::default()
                .fg(palette::BLUE)
                .add_modifier(Modifier::BOLD);
            if use_bg { s.bg(bg) } else { s }
        } else if use_bg {
            tab.style.bg(bg)
        } else {
            tab.style
        };
        spans.push(Span::styled(format!("{} ", tab.text), text_style));
    }

    // Right overflow indicator
    if right_overflow > 0 {
        spans.push(Span::styled(
            format!(" {}\u{2192}", right_overflow),
            Theme::dim(),
        ));
    }

    Line::from(spans)
}

/// Compute visible tab range centered on cursor_idx.
/// Returns (start, end, left_overflow_count, right_overflow_count).
fn compute_tab_scroll(
    widths: &[usize],
    gap: usize,
    avail: usize,
    cursor_idx: usize,
) -> (usize, usize, usize, usize) {
    if widths.is_empty() {
        return (0, 0, 0, 0);
    }

    let total: usize = widths.iter().sum::<usize>() + widths.len().saturating_sub(1) * gap;

    if total <= avail {
        return (0, widths.len(), 0, 0);
    }

    let overflow_w = 5usize;

    let mut start = cursor_idx.min(widths.len() - 1);
    let mut end = start + 1;
    let mut used = widths[start];

    loop {
        let left_reserve = if start > 0 { overflow_w } else { 1 };
        let right_reserve = if end < widths.len() { overflow_w } else { 0 };
        let budget = avail.saturating_sub(left_reserve + right_reserve);

        let mut expanded = false;

        if end < widths.len() {
            let next_w = gap + widths[end];
            if used + next_w <= budget {
                used += next_w;
                end += 1;
                expanded = true;
            }
        }

        if start > 0 {
            let next_w = widths[start - 1] + gap;
            let left_reserve_new = if start - 1 > 0 { overflow_w } else { 1 };
            let right_reserve_cur = if end < widths.len() { overflow_w } else { 0 };
            let budget_new = avail.saturating_sub(left_reserve_new + right_reserve_cur);
            if used + next_w <= budget_new {
                used += next_w;
                start -= 1;
                expanded = true;
            }
        }

        if !expanded {
            break;
        }
    }

    (start, end, start, widths.len() - end)
}

fn build_agent_detail(agent: &Agent, app: &App, lines: &mut Vec<Line<'static>>, width: usize) {
    let w = width as u16;
    let is_selected = app.ui.selected.contains(&agent.action_name());

    // Line 1: icon name · tool · age · context                 created Xm
    let icon = agent_icon(agent, app.ui.tick);
    let age = agent.age_display();
    let created = agent.created_display();
    let name_style = selected_name_style(is_selected);

    let mut left: Vec<Span<'static>> = vec![
        Span::raw("  "),
        Span::styled(icon.to_string(), agent.status.style()),
        Span::raw(" "),
        Span::styled(agent.display_name(), name_style),
        Span::styled(" \u{00b7} ", Theme::separator()),
        Span::styled(agent.tool.name().to_string(), Theme::dim()),
        Span::styled(" \u{00b7} ", Theme::separator()),
        Span::styled(age, Theme::agent_dim()),
    ];
    if let Some((context, fresh)) = app.usage.context(agent)
        && left.iter().map(Span::width).sum::<usize>()
            + 3
            + UnicodeWidthStr::width(context.as_str())
            + UnicodeWidthStr::width(format!("created {}  ", created).as_str())
            <= width
    {
        left.push(Span::styled(" · ", Theme::separator()));
        // Darker until the refresh after selecting this agent lands.
        let style = if fresh {
            Theme::agent_dim()
        } else {
            Theme::separator()
        };
        left.push(Span::styled(context, style));
    }
    let created_label = format!("created {}", created);
    let right: Vec<Span<'static>> = vec![
        Span::styled(created_label, Theme::agent_dim()),
        Span::raw("  "),
    ];
    let right_w: usize = right.iter().map(|s| s.width()).sum();

    // Truncate context to fit within available width; prefix ⊘ when PTY-blocked
    let ctx = agent.context_display();
    let blocked = agent.is_pty_blocked();
    if !ctx.is_empty() || blocked {
        left.push(Span::styled(" \u{00b7} ", Theme::separator()));
        if blocked {
            left.push(Span::styled("\u{2298} ", Theme::pty_block())); // ⊘
        }
        let ctx_style = if blocked {
            Theme::pty_block()
        } else {
            Theme::agent_context()
        };
        let left_w: usize = left.iter().map(|s| s.width()).sum();
        let avail = (w as usize).saturating_sub(left_w + right_w);
        let ctx_w = UnicodeWidthStr::width(ctx.as_str());
        if avail > 1 && ctx_w > avail {
            let mut truncated = String::new();
            let mut tw = 0;
            for c in ctx.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if tw + cw + 1 > avail {
                    break;
                }
                truncated.push(c);
                tw += cw;
            }
            truncated.push('\u{2026}');
            left.push(Span::styled(truncated, ctx_style));
        } else if avail > 0 && !ctx.is_empty() {
            left.push(Span::styled(ctx, ctx_style));
        }
    }

    append_right_aligned(&mut left, right, w);
    lines.push(Line::from(left));

    // Line 2: directory · [headless]/[terminal] · pid · session · unread
    let mut info: Vec<Span<'static>> = vec![
        Span::raw("  "),
        Span::styled(shorten_dir(&agent.directory), Theme::dim()),
    ];
    if agent.headless {
        info.push(Span::styled(" \u{00b7} ", Theme::separator()));
        info.push(Span::styled("[headless]", Theme::dim()));
    } else if let Some(ref preset) = agent.terminal_preset {
        info.push(Span::styled(" \u{00b7} ", Theme::separator()));
        info.push(Span::styled(format!("[{}]", preset), Theme::dim()));
    }
    if let Some(pid) = agent.pid {
        info.push(Span::styled(" \u{00b7} ", Theme::separator()));
        info.push(Span::styled(format!("pid:{}", pid), Theme::dim()));
    }
    if let Some(ref sid) = agent.session_id {
        info.push(Span::styled(" \u{00b7} ", Theme::separator()));
        let short: String = sid.chars().take(6).collect();
        info.push(Span::styled(format!("session:{}", short), Theme::dim()));
    }
    if agent.unread > 0 {
        info.push(Span::styled(" \u{00b7} ", Theme::separator()));
        info.push(Span::styled(
            format!("{} unread", agent.unread),
            Style::default().fg(palette::YELLOW),
        ));
    }
    finalize_detail_spans(&mut info, w);
    lines.push(Line::from(info));

    // Separator line
    let dashes = "\u{254c}".repeat(w.saturating_sub(4) as usize);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(dashes, Theme::separator()),
    ]));
}

fn build_orphan_detail(orphan: &OrphanProcess, lines: &mut Vec<Line<'static>>, _width: usize) {
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("\u{2716} ", Style::default().fg(palette::RED)),
        Span::styled(
            format!("PID {} ", orphan.pid),
            Style::default().fg(palette::FG),
        ),
        Span::styled("\u{00b7} ", Theme::separator()),
        Span::styled(orphan.tool.name().to_string(), Theme::dim()),
        Span::styled(" \u{00b7} ", Theme::separator()),
        Span::styled(orphan.names_display(), Theme::dim()),
    ]));

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(orphan.directory.clone(), Theme::dim()),
        Span::styled(" \u{00b7} ", Theme::separator()),
        Span::styled(orphan.age_display(), Theme::dim()),
    ]));
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn detail_selection_uses_action_identity() {
        crate::config::Config::init();
        let mut app = crate::tui::test_helpers::offline_app();
        let mut agent = crate::tui::test_helpers::make_test_agent("nova", 0.0);
        agent.device_name = Some("BOX".into());
        for (selected, expected) in [("nova", false), ("nova:BOX", true)] {
            app.ui.selected.clear();
            app.ui.selected.insert(selected.into());
            let mut lines = Vec::new();
            build_agent_detail(&agent, &app, &mut lines, 100);
            let name = lines[0]
                .spans
                .iter()
                .find(|span| span.content == "nova:BOX")
                .unwrap();
            assert_eq!(name.style, selected_name_style(expected));
        }
    }

    #[test]
    fn right_badge_survives_long_unicode_middle_fields() {
        for width in 5..30 {
            let mut left = vec![Span::raw("  nova "), Span::raw("界".repeat(100))];
            append_right_aligned(&mut left, vec![Span::raw(" 12m ")], width);
            let line = Line::from(left);
            assert!(line.width() <= width as usize);
            assert!(line.to_string().ends_with(" 12m "));
        }
    }

    #[test]
    fn scrolling_keeps_every_cursor_visible_and_counts_hidden_rows() {
        for height in 1..12 {
            for cursor in 0..20 {
                let rows = (0..20)
                    .map(|i| {
                        let line = Line::raw(format!("agent {i}"));
                        if i == cursor {
                            line.style(Style::default().bg(palette::SELECTION))
                        } else {
                            line
                        }
                    })
                    .collect();
                let visible = window_agent_rows(rows, height);
                assert!(visible.len() <= height);
                assert!(
                    visible
                        .iter()
                        .any(|line| line.to_string() == format!("agent {cursor}"))
                );
                if height >= 3 {
                    let content: Vec<usize> = visible
                        .iter()
                        .filter_map(|line| {
                            line.to_string()
                                .strip_prefix("agent ")
                                .and_then(|s| s.parse().ok())
                        })
                        .collect();
                    let first = *content.first().unwrap();
                    let last = *content.last().unwrap();
                    if first > 0 {
                        assert_eq!(visible[0].to_string(), format!("  ↑ {first} more"));
                    }
                    if last < 19 {
                        assert_eq!(
                            visible.last().unwrap().to_string(),
                            format!("  ↓ {} more", 19 - last)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn expanded_stopped_section_keeps_live_share_and_section_cursor_visible() {
        crate::config::Config::init();
        let mut app = crate::tui::test_helpers::offline_app();
        app.data.agents = (0..10)
            .map(|i| crate::tui::test_helpers::make_test_agent(&format!("live{i}"), 0.0))
            .collect();
        app.data.stopped_agents = (0..100)
            .map(|i| crate::tui::test_helpers::make_test_agent(&format!("stop{i}"), 0.0))
            .collect();
        app.ui.stopped_expanded = true;
        app.ui.view_mode = ViewMode::Vertical;
        for cursor in [0, 9, 10, 11, 110] {
            app.ui.cursor = cursor;
            let lines = collect_agent_lines(&app, 80, 12);
            assert!(lines.len() <= 12);
            assert!(
                lines
                    .iter()
                    .any(|line| line.style.bg == Some(palette::SELECTION))
            );
            assert!(
                lines
                    .iter()
                    .filter(|line| line.to_string().contains("live"))
                    .count()
                    >= 4
            );
        }
    }

    #[test]
    fn context_stays_in_heading_and_is_omitted_when_narrow() {
        crate::config::Config::init();
        let mut app = crate::tui::test_helpers::offline_app();
        let mut agent = crate::tui::test_helpers::make_test_agent("nahe", 3240.0);
        agent.tool = crate::tool::Tool::Qoder.into();
        agent.status_time = agent.created_at;
        agent.transcript_path = Some("/test/session.jsonl".into());
        for context in ["100k/200k", "100k"] {
            app.usage.set_context_for_test(&agent, context);
            let mut lines = Vec::new();
            build_agent_detail(&agent, &app, &mut lines, 120);
            assert_eq!(lines.len(), 3);
            let heading = lines[0].to_string();
            assert!(
                heading.contains(&format!("nahe · qoder · 54m · ctx:{context}")),
                "{heading}"
            );
            assert!(!lines[1].to_string().contains(context));
            let style_of = |lines: &[Line]| {
                lines[0]
                    .spans
                    .iter()
                    .find(|s| s.content == format!("ctx:{context}"))
                    .unwrap()
                    .style
            };
            assert_eq!(style_of(&lines), Theme::agent_dim());
            app.usage.mark_stale_for_test();
            lines.clear();
            build_agent_detail(&agent, &app, &mut lines, 120);
            assert_eq!(style_of(&lines), Theme::separator());
            lines.clear();
            build_agent_detail(&agent, &app, &mut lines, 40);
            assert!(!lines[0].to_string().contains(context));
            assert!(lines[0].to_string().contains("nahe · qoder · 54m"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_preview_truncates_at_character_boundaries() {
        let mut agent = crate::tui::test_helpers::make_test_agent("nova", 60.0);
        agent.session_id = Some("é世🙂abcde".into());
        let line = detail_line_info(&agent, 200).to_string();
        assert!(line.contains("session:é世🙂abc"), "{line}");
        assert!(!line.contains("abcde"), "{line}");
    }
}
