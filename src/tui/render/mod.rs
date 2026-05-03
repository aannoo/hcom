mod agents;
pub mod launch;
pub(crate) mod messages;
pub(crate) mod text;

use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{App, Confirm};
use crate::tui::inline::eject::filtered_counts;
use crate::tui::model::*;
use crate::tui::theme::{Theme, palette};

/// Truncate a string to fit within `max_w` display columns, appending "\u{2026}" if truncated.
pub(crate) fn truncate_display(s: &str, max_w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let w = UnicodeWidthStr::width(s);
    if w <= max_w {
        return s.to_string();
    }
    if max_w == 0 {
        return String::new();
    }
    let target = max_w.saturating_sub(1); // room for "\u{2026}"
    let mut result = String::new();
    let mut cur = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur + cw > target {
            break;
        }
        result.push(ch);
        cur += cw;
    }
    result.push('\u{2026}');
    result
}

/// Truncate spans to fit within `max_w` display columns.
/// Preserves spans left-to-right (highest priority first), drops from the right.
pub(crate) fn fit_spans(spans: Vec<Span<'static>>, max_w: usize) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthChar;
    let total: usize = spans.iter().map(|s| s.width()).sum();
    if total <= max_w {
        return spans;
    }
    let mut result: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let w = span.width();
        if used + w <= max_w {
            result.push(span);
            used += w;
        } else {
            let remaining = max_w.saturating_sub(used);
            if remaining > 1 {
                let text = span.content.to_string();
                let target = remaining - 1; // room for …
                let mut truncated = String::new();
                let mut tw = 0;
                for ch in text.chars() {
                    let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if tw + cw > target {
                        break;
                    }
                    truncated.push(ch);
                    tw += cw;
                }
                if !truncated.is_empty() {
                    result.push(Span::styled(truncated, span.style));
                }
                result.push(Span::styled("\u{2026}", span.style));
            } else if remaining == 1 {
                result.push(Span::styled("\u{2026}", span.style));
            }
            break;
        }
    }
    result
}

fn input_prefix_str(app: &App) -> &'static str {
    if let Some(ref overlay) = app.ui.overlay {
        return match overlay.kind {
            OverlayKind::Search => "SEARCH / ",
            OverlayKind::Command => "CMD ! ",
            OverlayKind::Tag => "TAG # ",
            OverlayKind::Project => "PRJ ! ",
        };
    }
    match app.ui.mode {
        InputMode::Launch => "prompt \u{276f} ",
        _ => "\u{276f} ",
    }
}

fn mode_input_bg(app: &App) -> ratatui::style::Color {
    if let Some(ref overlay) = app.ui.overlay {
        return match overlay.kind {
            OverlayKind::Search => palette::MODE_SEARCH,
            OverlayKind::Command => palette::MODE_CMD,
            OverlayKind::Tag => palette::MODE_TAG,
            OverlayKind::Project => palette::MODE_TAG,
        };
    }
    match app.ui.mode {
        InputMode::Compose => palette::MODE_COMPOSE,
        InputMode::CommandOutput => palette::MODE_CMD,
        _ => palette::SELECTION,
    }
}

pub fn render(frame: &mut Frame, app: &mut App) {
    render_main(frame, app);

    render_command_palette(frame, app);

    if app.ui.help_open {
        render_help(frame, app.ui.help_scroll);
    }

    if let Some(ref confirm) = app.ui.confirm {
        render_confirm(frame, confirm);
    }

    if let Some(ref relay) = app.ui.relay_popup {
        render_relay_popup(frame, relay);
    }
}

fn render_main(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.ui.term_width = area.width;

    if app.data.agents.is_empty()
        && app.data.remote_agents.is_empty()
        && app.data.stopped_agents.is_empty()
        && app.data.orphans.is_empty()
        && app.data.messages.is_empty()
        && app.data.events.is_empty()
        && app.ui.mode != InputMode::Launch
    {
        render_empty(frame, area, app);
        return;
    }

    match app.ui.view_mode {
        ViewMode::Inline => render_inline(frame, app, area),
        ViewMode::Vertical => render_vertical(frame, app, area),
    }
}

fn extra_list_rows(app: &App) -> u16 {
    let mut n = 0u16;
    if !app.data.remote_agents.is_empty() {
        n += 1;
        if app.ui.remote_expanded {
            n += app.data.remote_agents.len() as u16;
        }
    }
    if app.ui.view_mode != ViewMode::Inline && !app.data.stopped_agents.is_empty() {
        n += 1;
        if app.ui.stopped_expanded {
            n += app.data.stopped_agents.len() as u16;
        }
    }
    if !app.data.orphans.is_empty() {
        n += 1;
        if app.ui.orphans_expanded {
            n += app.data.orphans.len() as u16;
        }
    }
    n
}

/// Grapheme position within wrapped text: (byte_offset, visual_line, visual_col).
/// Single source of truth for wrap logic — all wrap functions derive from this.
struct WrapLayout {
    first_avail: usize,
    cont_avail: usize,
}

impl WrapLayout {
    fn new(total_width: u16, prefix_w: usize) -> Self {
        let w = total_width as usize;
        Self {
            first_avail: w.saturating_sub(prefix_w),
            cont_avail: w,
        }
    }

    /// Iterate grapheme positions: yields (byte_offset, line, col, grapheme_str, grapheme_width).
    /// After iteration, final (line, col) is one past the last grapheme.
    fn walk<'a>(&self, text: &'a str) -> WrapWalker<'a> {
        WrapWalker {
            graphemes: text.graphemes(true),
            line: 0,
            col: 0,
            offset: 0,
            cur_avail: self.first_avail,
            cont_avail: self.cont_avail,
        }
    }
}

struct WrapWalker<'a> {
    graphemes: unicode_segmentation::Graphemes<'a>,
    line: usize,
    col: usize,
    offset: usize,
    cur_avail: usize,
    cont_avail: usize,
}

struct WrapPos<'a> {
    offset: usize,
    line: usize,
    col: usize,
    grapheme: &'a str,
    width: usize,
}

impl<'a> Iterator for WrapWalker<'a> {
    type Item = WrapPos<'a>;

    fn next(&mut self) -> Option<WrapPos<'a>> {
        let g = self.graphemes.next()?;
        let gw = UnicodeWidthStr::width(g);
        if self.col + gw > self.cur_avail && self.col > 0 {
            self.line += 1;
            self.cur_avail = self.cont_avail;
            self.col = 0;
        }
        let pos = WrapPos {
            offset: self.offset,
            line: self.line,
            col: self.col,
            grapheme: g,
            width: gw,
        };
        self.col += gw;
        self.offset += g.len();
        Some(pos)
    }
}

/// Number of visual wrapped lines for text with given available width.
/// Prefix width (e.g. "  ❯ " = 4) is subtracted from the first line's width.
/// Continuation lines use the full terminal width (ratatui wraps flush left).
fn wrap_line_count(text: &str, total_width: u16, prefix_w: usize) -> usize {
    if text.is_empty() {
        return 1;
    }
    let layout = WrapLayout::new(total_width, prefix_w);
    if layout.first_avail == 0 {
        return 1;
    }
    let mut last_line = 0;
    for pos in layout.walk(text) {
        last_line = pos.line;
    }
    last_line + 1
}

/// Compute dynamic input area height for compose mode.
/// Returns 3 (default) for non-compose modes, grows with wrapped line count.
fn compose_input_height(app: &App, is_inline: bool, width: u16) -> u16 {
    if app.ui.mode != InputMode::Compose || app.ui.input.is_empty() {
        return 3;
    }
    let wrapped = wrap_line_count(&app.ui.input, width, 4) as u16;
    let max_lines = if is_inline { 4 } else { 8 };
    wrapped.min(max_lines) + 2 // +2 for top/bottom padding rows
}

/// Update input_scroll to keep cursor's wrapped line visible.
fn update_input_scroll(app: &mut App, is_inline: bool, width: u16) {
    if app.ui.mode != InputMode::Compose {
        app.ui.input_scroll = 0;
        return;
    }
    let max_lines = if is_inline { 4 } else { 8 };
    let total_wrapped = wrap_line_count(&app.ui.input, width, 4);
    let visible = max_lines.min(total_wrapped);
    let cursor_wrap_line = cursor_wrap_line(&app.ui.input, app.ui.input_cursor, width, 4);
    if cursor_wrap_line < app.ui.input_scroll {
        app.ui.input_scroll = cursor_wrap_line;
    } else if visible > 0 && cursor_wrap_line >= app.ui.input_scroll + visible {
        app.ui.input_scroll = cursor_wrap_line - visible + 1;
    }
}

/// Which visual wrapped line the cursor is on.
fn cursor_wrap_line(text: &str, cursor: usize, total_width: u16, prefix_w: usize) -> usize {
    let layout = WrapLayout::new(total_width, prefix_w);
    if layout.first_avail == 0 {
        return 0;
    }
    let clamped = cursor.min(text.len());
    let before = &text[..clamped];
    let mut last_line = 0;
    for pos in layout.walk(before) {
        last_line = pos.line;
    }
    last_line
}

/// Break text into visual lines using character-level wrapping.
/// Uses WrapLayout for consistent wrap logic with all other wrap functions.
fn break_into_visual_lines(text: &str, total_width: u16, prefix_w: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let layout = WrapLayout::new(total_width, prefix_w);
    if layout.first_avail == 0 {
        return vec![text.to_string()];
    }

    let mut lines: Vec<String> = vec![String::new()];
    for pos in layout.walk(text) {
        while lines.len() <= pos.line {
            lines.push(String::new());
        }
        lines[pos.line].push_str(pos.grapheme);
    }
    lines
}

/// Column (display width) within the current wrapped line.
fn cursor_wrap_col(text: &str, cursor: usize, total_width: u16, prefix_w: usize) -> usize {
    let layout = WrapLayout::new(total_width, prefix_w);
    if layout.first_avail == 0 {
        return 0;
    }
    let clamped = cursor.min(text.len());
    let before = &text[..clamped];
    let mut col = 0;
    for pos in layout.walk(before) {
        col = pos.col + pos.width;
    }
    col
}

/// Move cursor up one visual wrapped line, preserving display column.
/// No-op if already on the first wrapped line.
pub(crate) fn cursor_wrap_up(text: &str, cursor: &mut usize, total_width: u16, prefix_w: usize) {
    let cur_line = cursor_wrap_line(text, *cursor, total_width, prefix_w);
    if cur_line == 0 {
        return;
    }
    let cur_col = cursor_wrap_col(text, *cursor, total_width, prefix_w);
    let target_line = cur_line - 1;
    let layout = WrapLayout::new(total_width, prefix_w);

    let mut best_offset = 0;
    for pos in layout.walk(text) {
        if pos.line > target_line {
            break;
        }
        if pos.line == target_line {
            if pos.col <= cur_col {
                best_offset = pos.offset;
                if pos.col + pos.width > cur_col {
                    break;
                }
            }
            // After grapheme: update best to end of grapheme
            if pos.col + pos.width <= cur_col {
                best_offset = pos.offset + pos.grapheme.len();
            }
        }
    }
    *cursor = best_offset;
}

/// Move cursor down one visual wrapped line, preserving display column.
/// No-op if already on the last wrapped line.
pub(crate) fn cursor_wrap_down(text: &str, cursor: &mut usize, total_width: u16, prefix_w: usize) {
    let total_lines = wrap_line_count(text, total_width, prefix_w);
    let cur_line = cursor_wrap_line(text, *cursor, total_width, prefix_w);
    if cur_line + 1 >= total_lines {
        return;
    }
    let cur_col = cursor_wrap_col(text, *cursor, total_width, prefix_w);
    let target_line = cur_line + 1;
    let layout = WrapLayout::new(total_width, prefix_w);

    let mut best_offset = 0;
    let mut in_target = false;
    for pos in layout.walk(text) {
        if pos.line > target_line {
            break;
        }
        if pos.line == target_line {
            if !in_target {
                in_target = true;
                best_offset = pos.offset;
            }
            if pos.col <= cur_col {
                best_offset = pos.offset;
                if pos.col + pos.width > cur_col {
                    break;
                }
                best_offset = pos.offset + pos.grapheme.len();
            } else {
                break;
            }
        }
    }
    *cursor = best_offset;
}

fn render_inline(frame: &mut Frame, app: &mut App, area: Rect) {
    let launch_height = if app.ui.mode == InputMode::Launch {
        app.ui.launch.panel_height()
    } else {
        0
    };

    let input_height = compose_input_height(app, true, area.width);

    // Layout: topline + status + sep + agents + [launch] + input + footer
    let chrome = 1 + 1 + 1 + input_height + 1 + launch_height;
    let agent_height = area.height.saturating_sub(chrome);

    let mut constraints = vec![
        Constraint::Length(1),            // top line
        Constraint::Length(1),            // status bar
        Constraint::Length(1),            // separator
        Constraint::Length(agent_height), // agents (fills remaining)
    ];
    if launch_height > 0 {
        constraints.push(Constraint::Length(launch_height));
    }
    constraints.push(Constraint::Length(input_height)); // input
    constraints.push(Constraint::Length(1)); // footer

    let layout = Layout::vertical(constraints).split(area);
    let n = layout.len();

    // Top horizontal line (brighter when filtered)
    let top_rule = "\u{2500}".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            top_rule,
            filter_separator_style(app),
        ))),
        layout[0],
    );

    render_status_bar(frame, layout[1], app);
    render_separator(frame, layout[2], app, area.width);

    if !app.data.agents.is_empty() || extra_list_rows(app) > 0 {
        agents::render_agents_tabstrip(frame, layout[3], app);
    }

    update_input_scroll(app, true, area.width);

    if launch_height > 0 {
        let la = layout[n - 3];
        let input_area = layout[n - 2];
        frame.render_widget(
            Block::default().style(Style::default().bg(mode_input_bg(app))),
            Rect::new(
                area.x,
                la.y,
                area.width,
                input_area.y + input_area.height - la.y,
            ),
        );
        launch::render_launch_inline(frame, la, app);
    } else {
        frame.render_widget(
            Block::default().style(Style::default().bg(mode_input_bg(app))),
            layout[n - 2],
        );
    }

    render_input(frame, layout[n - 2], app);
    render_footer(frame, layout[n - 1], app);

    let launch_rect = if launch_height > 0 {
        Some(layout[n - 3])
    } else {
        None
    };
    position_cursor(frame, app, layout[n - 2], launch_rect, area.width);

    app.ui.scroll_max = 0;
}

fn render_vertical(frame: &mut Frame, app: &mut App, area: Rect) {
    let launch_height = if app.ui.mode == InputMode::Launch {
        app.ui.launch.panel_height()
    } else {
        0
    };

    let mut outer = vec![
        Constraint::Length(1), // status bar
        Constraint::Length(1), // blank
        Constraint::Min(3),    // body (agents left | messages right)
    ];
    if launch_height > 0 {
        outer.push(Constraint::Length(launch_height));
    }
    let input_height = compose_input_height(app, false, area.width);
    outer.push(Constraint::Length(1)); // blank
    outer.push(Constraint::Length(input_height)); // input
    outer.push(Constraint::Length(1)); // footer

    let vlayout = Layout::vertical(outer).split(area);
    let n = vlayout.len();
    let launch_area = if launch_height > 0 {
        Some(vlayout[n - 4])
    } else {
        None
    };

    render_status_bar(frame, vlayout[0], app);

    // Separator line after hcom header (brighter when filtered)
    let sep_line = "\u{2500}".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            sep_line,
            filter_separator_style(app),
        ))),
        vlayout[1],
    );

    // Split body horizontally: agents left, separator, messages right
    let left_width = (area.width / 3).clamp(20, 36);
    let body = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Length(1),
        Constraint::Min(10),
    ])
    .split(vlayout[2]);

    if !app.data.agents.is_empty() || extra_list_rows(app) > 0 {
        agents::render_agents_compact(frame, body[0], app);
    }

    // Vertical separator bar
    let sep_lines: Vec<Line> = (0..body[1].height)
        .map(|_| Line::from(Span::styled("\u{2502}", Theme::separator())))
        .collect();
    frame.render_widget(Paragraph::new(sep_lines), body[1]);

    // Split right panel: 1-line heading + messages content
    let body_right = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(body[2]);

    render_messages_heading(frame, body_right[0], app);
    let max_scroll = messages::render_messages(frame, body_right[1], app);
    app.ui.scroll_max = max_scroll;
    update_input_scroll(app, false, area.width);
    render_bottom_section(
        frame,
        app,
        launch_area,
        vlayout[n - 2],
        vlayout[n - 1],
        area.width,
    );
}

/// Shared bottom section: launch panel bg, input bar, footer, cursor.
fn render_bottom_section(
    frame: &mut Frame,
    app: &App,
    launch_area: Option<Rect>,
    input_area: Rect,
    footer_area: Rect,
    width: u16,
) {
    if let Some(la) = launch_area {
        let bg_top = la.y;
        let bg_bottom = input_area.y + input_area.height;
        let bg_rect = Rect::new(0, bg_top, width, bg_bottom - bg_top);
        frame.render_widget(
            Block::default().style(Style::default().bg(mode_input_bg(app))),
            bg_rect,
        );
        launch::render_launch_inline(frame, la, app);
    } else {
        frame.render_widget(
            Block::default().style(Style::default().bg(mode_input_bg(app))),
            input_area,
        );
    }

    render_input(frame, input_area, app);
    render_footer(frame, footer_area, app);
    position_cursor(frame, app, input_area, launch_area, width);
}

/// Shared cursor positioning for input bar and launch field editing.
fn position_cursor(
    frame: &mut Frame,
    app: &App,
    input_area: Rect,
    launch_area: Option<Rect>,
    width: u16,
) {
    let modal_open = app.ui.help_open
        || app.ui.confirm.is_some()
        || (app.ui.relay_popup.is_some()
            && !app.ui.relay_popup.as_ref().is_some_and(|r| r.editing_token));

    if modal_open {
        return;
    }

    if app.ui.mode == InputMode::Launch && matches!(app.ui.launch.editing, Some(LaunchField::Tag)) {
        if let Some(la) = launch_area {
            let field = app.ui.launch.editing.unwrap();
            let value = app.ui.launch.field_value(field);
            let before = &value[..app.ui.launch.edit_cursor.min(value.len())];
            let cursor_x = 2 + 2 + launch::LABEL_W + UnicodeWidthStr::width(before);
            let row_offset: u16 = 3;
            frame.set_cursor_position(Position::new(
                la.x + (cursor_x as u16).min(width.saturating_sub(1)),
                la.y + row_offset,
            ));
        }
    } else if app.ui.mode == InputMode::Navigate {
        // Navigate: show cursor only when overlay is active
        if let Some(ref overlay) = app.ui.overlay {
            let prefix = input_prefix_str(app);
            let clamped = overlay.cursor.min(overlay.input.len());
            let display_width = UnicodeWidthStr::width(&overlay.input[..clamped]);
            let input_x = 2 + UnicodeWidthStr::width(prefix) + display_width;
            frame.set_cursor_position(Position::new(
                input_area.x + (input_x as u16).min(width.saturating_sub(1)),
                input_area.y + 1,
            ));
        }
        // No overlay → no cursor (Navigate mode has no text input)
    } else if app.ui.mode == InputMode::Compose {
        // Word-wrapped cursor positioning
        let wrap_line = cursor_wrap_line(&app.ui.input, app.ui.input_cursor, width, 4);
        let wrap_col = cursor_wrap_col(&app.ui.input, app.ui.input_cursor, width, 4);
        let prefix_w: usize = if wrap_line == 0 { 4 } else { 0 }; // "  ❯ " on first line, flush left on continuations
        let cursor_x = prefix_w + wrap_col;
        let visible_line = wrap_line.saturating_sub(app.ui.input_scroll);
        let cursor_y = input_area.y + 1 + visible_line as u16;
        frame.set_cursor_position(Position::new(
            input_area.x + (cursor_x as u16).min(width.saturating_sub(1)),
            cursor_y.min(input_area.y + input_area.height.saturating_sub(1)),
        ));
    } else if app.ui.mode != InputMode::Launch || app.ui.launch.options_cursor.is_none() {
        let input_prefix = input_prefix_str(app);
        let clamped = app.ui.input_cursor.min(app.ui.input.len());
        let display_width = UnicodeWidthStr::width(&app.ui.input[..clamped]);
        let input_x = 2 + UnicodeWidthStr::width(input_prefix) + display_width;
        frame.set_cursor_position(Position::new(
            input_area.x + (input_x as u16).min(width.saturating_sub(1)),
            input_area.y + 1,
        ));
    }
}

fn render_empty(frame: &mut Frame, area: Rect, app: &App) {
    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    let bar = Line::from(vec![Span::raw("  "), Span::styled("hcom", Theme::title())]);
    frame.render_widget(Paragraph::new(bar), layout[0]);

    let key = |k: &str| Span::styled(format!("  {:<10}", k), Style::default().fg(palette::FG));
    let lbl = |l: &str| Span::styled(l.to_string(), Theme::dim());

    let mut msg = vec![Line::raw("")];

    if let Some(err) = app.backend_error() {
        msg.push(Line::from(Span::styled(
            "  Backend unavailable",
            Style::default().fg(palette::RED),
        )));
        msg.push(Line::from(Span::styled(
            format!(
                "  {}",
                truncate_display(&err, area.width.saturating_sub(4) as usize)
            ),
            Theme::dim(),
        )));
    } else {
        msg.push(Line::from(Span::styled(
            "  No agents running",
            Style::default().fg(palette::FG_DIM),
        )));
    }

    msg.extend([
        Line::raw(""),
        Line::from(vec![key("tab"), lbl("launch agents")]),
        Line::from(vec![key("!"), lbl("run hcom command")]),
        Line::from(vec![key("ctrl+r"), lbl("relay settings")]),
        Line::from(vec![key("?"), lbl("keyboard shortcuts")]),
    ]);
    frame.render_widget(Paragraph::new(msg), layout[1]);

    let footer = Line::from(vec![
        Span::raw("  "),
        Span::styled("tab", Style::default().fg(palette::FG)),
        Span::styled(" launch ", Theme::dim()),
        Span::styled("\u{00b7} ", Theme::dim()),
        Span::styled("!", Style::default().fg(palette::FG)),
        Span::styled(" command ", Theme::dim()),
        Span::styled("\u{00b7} ", Theme::dim()),
        Span::styled("?", Style::default().fg(palette::FG)),
        Span::styled(" help", Theme::dim()),
    ]);
    frame.render_widget(Paragraph::new(footer), layout[2]);
}

fn render_messages_heading(frame: &mut Frame, area: Rect, app: &App) {
    let selected_names = || {
        app.ui
            .selected
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    let label = if app.ui.show_events && !app.ui.selected.is_empty() {
        format!("events: {}", selected_names())
    } else if app.ui.show_events {
        "events".to_string()
    } else if !app.ui.selected.is_empty() {
        selected_names()
    } else {
        "messages".to_string()
    };

    let count_str = messages::display_count_str(app);

    let has_filter = !app.ui.selected.is_empty() || app.ui.search_filter.is_some();
    let label_style = if has_filter {
        Style::default().fg(palette::BLUE)
    } else {
        Theme::dim()
    };

    let spans = vec![
        Span::raw("  "),
        Span::styled(label, label_style),
        Span::raw("  "),
        Span::styled(count_str, Style::default().fg(palette::FG_DARK)),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let mut active = 0u16;
    let mut listening = 0u16;
    let mut blocked = 0u16;
    let mut launching = 0u16;
    let mut inactive = 0u16;

    for agent in app.data.agents.iter().chain(app.data.remote_agents.iter()) {
        match agent.status {
            AgentStatus::Active => active += 1,
            AgentStatus::Listening => listening += 1,
            AgentStatus::Blocked => blocked += 1,
            AgentStatus::Launching => launching += 1,
            AgentStatus::Inactive => inactive += 1,
        }
    }

    let mut left = vec![Span::raw("  "), Span::styled("hcom", Theme::title())];

    // Filter/count info next to title
    let dim_info = Style::default().fg(palette::FG_DARK);
    if app.ui.view_mode == ViewMode::Inline {
        let has_agent_filter = app.ui.eject_filter.is_some();
        let has_text_filter = app.ui.search_filter.is_some();
        let (ev_count, msg_count) =
            filtered_counts(&app.data, &app.ui.eject_filter, &app.ui.search_filter);
        let matched = ev_count + msg_count;
        let loaded = app.data.events.len() + app.data.messages.len();

        left.push(Span::raw("  "));
        if has_text_filter {
            // FTS search: searched entire DB
            let mut parts: Vec<String> = Vec::new();
            if let Some(ref ef) = app.ui.eject_filter {
                parts.push(ef.iter().cloned().collect::<Vec<_>>().join(", "));
            }
            if let Some(ref sf) = app.ui.search_filter {
                parts.push(format!("/{}", sf));
            }
            parts.push(format!("{} found", matched));
            left.push(Span::styled(
                format!("[{}]", parts.join(" \u{00b7} ")),
                Style::default().fg(palette::YELLOW),
            ));
        } else if has_agent_filter {
            // Agent filter: X matching out of loaded
            let names = app
                .ui
                .eject_filter
                .as_ref()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            left.push(Span::styled(
                format!("[{} \u{00b7} {}/{}]", names, matched, loaded),
                Style::default().fg(palette::BLUE),
            ));
        } else {
            left.push(Span::styled(format!("[last {}]", loaded), dim_info));
        }
    } else if let Some(ref filter) = app.ui.search_filter {
        // Vertical mode: show search filter
        left.push(Span::raw("  "));
        left.push(Span::styled(
            format!("/{}", filter),
            Style::default().fg(palette::CYAN),
        ));
    }

    let mut right: Vec<Span> = Vec::new();

    // Relay indicator (right side, before status counts). Branches off the
    // canonical RelayHealth — same enum the CLI and JSON render from, so
    // the indicator can't disagree with `hcom relay` / `hcom status`.
    use crate::relay::{RelayErrorReason, RelayHealth};
    match &app.data.relay_health {
        RelayHealth::Connected => {
            let show_text = app
                .ui
                .relay_text_until
                .is_some_and(|t| std::time::Instant::now() < t);
            if show_text {
                right.push(Span::styled(
                    "relay connected ",
                    Style::default().fg(palette::GREEN),
                ));
            }
            right.push(Span::styled(
                "\u{21c4}  ",
                Style::default().fg(palette::GREEN),
            ));
        }
        RelayHealth::Error { reason, detail, .. } => {
            // Show short detail only for worker-reported errors — StalePidfile
            // and Ghost are operational anomalies the user can't act on inline.
            if matches!(reason, RelayErrorReason::Reported) {
                if let Some(err) = detail {
                    let truncated: String = err.chars().take(20).collect();
                    right.push(Span::styled(
                        format!("{} ", truncated),
                        Style::default().fg(palette::RED),
                    ));
                }
            }
            right.push(Span::styled(
                "\u{21c4}  ",
                Style::default().fg(palette::RED),
            ));
        }
        RelayHealth::Stale { .. } => {
            right.push(Span::styled(
                "\u{21c4}  ",
                Style::default().fg(palette::RED),
            ));
        }
        RelayHealth::Starting { .. } | RelayHealth::Waiting => {
            right.push(Span::styled(
                "\u{21c4}  ",
                Style::default().fg(palette::FG_DIM),
            ));
        }
        RelayHealth::Disabled => {
            // Disabled means relay_id is set but enabled=false — show a dim
            // arrow so the user knows the toggle exists. (Previously this was
            // gated on raw_status being present, which broke once disable
            // started clearing runtime KV.)
            right.push(Span::styled(
                "\u{21c4}  ",
                Style::default().fg(palette::FG_DIM),
            ));
        }
        RelayHealth::NotConfigured => {}
    }

    let counts = [
        (active, AgentStatus::Active),
        (listening, AgentStatus::Listening),
        (blocked, AgentStatus::Blocked),
        (launching, AgentStatus::Launching),
        (inactive, AgentStatus::Inactive),
    ];
    for (count, status) in counts {
        if count > 0 {
            right.push(Span::styled(format!("{} ", status.icon()), status.style()));
            right.push(Span::styled(format!("{}  ", count), status.style()));
        }
    }

    let left_width: usize = left.iter().map(|s| s.width()).sum();
    let right_width: usize = right.iter().map(|s| s.width()).sum();
    let pad = (area.width as usize).saturating_sub(left_width + right_width);

    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Whether any filter (selection, agent filter, search) is active.
fn has_active_filter(app: &App) -> bool {
    !app.ui.selected.is_empty() || app.ui.eject_filter.is_some() || app.ui.search_filter.is_some()
}

/// Separator style: brighter when a filter is active to visually frame the filtered state.
fn filter_separator_style(app: &App) -> Style {
    if has_active_filter(app) {
        Style::default().fg(palette::FG_DIM)
    } else {
        Theme::separator()
    }
}

fn render_separator(frame: &mut Frame, area: Rect, app: &App, width: u16) {
    let cmd_label: String;
    let flash_label: String;

    // Center label: command > flash (selection shown via tab styling, not here)
    let label_opt: Option<(&str, Style)> = if let Some(ref cr) = app.ui.command_result {
        cmd_label = format!("! {}", cr.label);
        Some((cmd_label.as_str(), Theme::dim()))
    } else if let Some(flash) = &app.ui.flash {
        flash_label = format!("\u{25cf} {}", flash.text);
        Some((flash_label.as_str(), flash.style))
    } else {
        None
    };

    // Right-side scroll position hint
    let right_hint_str: String;
    let right_hint = if app.ui.msg_scroll > 0 {
        let max = app.ui.scroll_max;
        let pos = max.saturating_sub(app.ui.msg_scroll);
        right_hint_str = format!(" \u{2191} {}/{} ", pos, max);
        right_hint_str.as_str()
    } else {
        ""
    };

    let avail = width as usize;
    let right_w = UnicodeWidthStr::width(right_hint);

    let effective_right = if right_w < avail { right_hint } else { "" };
    let effective_right_w = UnicodeWidthStr::width(effective_right);

    let sep_style = filter_separator_style(app);
    let mut spans: Vec<Span> = Vec::new();

    if let Some((label, style)) = label_opt {
        let label_padded = format!(" {} ", label);
        let label_w = UnicodeWidthStr::width(label_padded.as_str());
        let truncated: String;
        let label_display = if label_w + effective_right_w + 2 <= avail {
            label_padded.as_str()
        } else {
            let max_label = avail.saturating_sub(effective_right_w + 2);
            truncated = truncate_display(&label_padded, max_label);
            truncated.as_str()
        };
        let label_display_w = UnicodeWidthStr::width(label_display);
        let fill_total = avail.saturating_sub(label_display_w + effective_right_w);
        let fill_left = fill_total / 2;
        let fill_right = fill_total - fill_left;
        spans.push(Span::styled("\u{2500}".repeat(fill_left), sep_style));
        spans.push(Span::styled(label_display.to_string(), style));
        spans.push(Span::styled("\u{2500}".repeat(fill_right), sep_style));
    } else {
        let fill = avail.saturating_sub(effective_right_w);
        spans.push(Span::styled("\u{2500}".repeat(fill), sep_style));
    }

    if !effective_right.is_empty() {
        let hint_style = if app.active_search_query().is_some() {
            Style::default().fg(palette::CYAN)
        } else {
            Theme::dim()
        };
        spans.push(Span::styled(effective_right.to_string(), hint_style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let line = match app.ui.mode {
        InputMode::CommandOutput => {
            if let Some(ref cr) = app.ui.command_result {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled("! ", Theme::dim()),
                    Span::styled(cr.label.clone(), Theme::dim()),
                ])
            } else {
                Line::raw("")
            }
        }
        InputMode::Launch => {
            let text = if app.ui.input.is_empty() {
                Span::styled(
                    "initial task for agent\u{2026}",
                    Style::default().fg(palette::FG_DARK),
                )
            } else {
                Span::styled(app.ui.input.clone(), Style::default().fg(palette::FG))
            };
            Line::from(vec![
                Span::raw("  "),
                Span::styled("prompt ", Theme::dim()),
                Span::styled("\u{276f} ", Theme::cursor()),
                text,
            ])
        }
        InputMode::Relay => Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{276f} ", Theme::dim()),
        ]),
        InputMode::Compose => {
            // Character-level wrapped compose rendering (matches cursor helpers exactly)
            let visual_lines = break_into_visual_lines(&app.ui.input, area.width, 4);
            let visible_rows = area.height.saturating_sub(2) as usize;
            let scroll = app.ui.input_scroll;

            let mut lines: Vec<Line> = Vec::new();
            for (i, vline) in visual_lines
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible_rows)
            {
                let text_span = Span::styled(vline.clone(), Style::default().fg(palette::FG));
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("\u{276f} ", Theme::cursor()),
                        text_span,
                    ]));
                } else {
                    lines.push(Line::from(text_span));
                }
            }
            if lines.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled("\u{276f} ", Theme::cursor()),
                ]));
            }

            let text_area = Rect::new(area.x, area.y + 1, area.width, (visible_rows as u16).max(1));
            frame.render_widget(Paragraph::new(lines), text_area);

            // Send target hint (only when text fits on one line)
            if !app.ui.input.is_empty() && visual_lines.len() == 1 {
                let target = send_target_display(&app.ui.input);
                let hint = format!("\u{2192} {} ", target);
                let hint_w = UnicodeWidthStr::width(hint.as_str());
                let input_w = 4 + UnicodeWidthStr::width(app.ui.input.as_str());
                if input_w + hint_w + 2 <= area.width as usize {
                    let first_row = Rect::new(area.x, area.y + 1, area.width, 1);
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(hint, Theme::dim())))
                            .alignment(Alignment::Right),
                        first_row,
                    );
                }
            }
            return;
        }
        InputMode::Navigate => {
            if let Some(ref overlay) = app.ui.overlay {
                let (prefix_label, prefix_style) = match overlay.kind {
                    OverlayKind::Search => ("SEARCH ", Style::default().fg(palette::YELLOW)),
                    OverlayKind::Command => ("CMD ", Style::default().fg(palette::MAGENTA)),
                    OverlayKind::Tag => ("TAG ", Style::default().fg(palette::TEAL)),
                    OverlayKind::Project => ("PRJ ", Style::default().fg(palette::TEAL)),
                };
                let prefix_char = match overlay.kind {
                    OverlayKind::Search => "/ ",
                    OverlayKind::Command => "! ",
                    OverlayKind::Tag => "# ",
                    OverlayKind::Project => "! ",
                };
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(prefix_label, prefix_style),
                    Span::styled(prefix_char, Theme::cursor()),
                    Span::styled(overlay.input.clone(), Style::default().fg(palette::FG)),
                ])
            } else {
                // Hints in the input bar — grouped by purpose
                let hk = Style::default().fg(palette::FG_MID);
                let hl = Style::default().fg(palette::FG_DIM);
                let gap = Span::styled("   ", hl);
                let on_orphan = matches!(app.cursor_target(), CursorTarget::Orphan(_));
                if on_orphan {
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("enter", hk),
                        Span::styled(" kill/recover", hl),
                        gap.clone(),
                        Span::styled("/", hk),
                        Span::styled(" search  ", hl),
                        Span::styled("!", hk),
                        Span::styled(" command", hl),
                        gap,
                        Span::styled("tab", hk),
                        Span::styled(" launch", hl),
                    ])
                } else {
                    let dash = Span::styled("  \u{2014}  ", Style::default().fg(palette::FG_DARK));
                    let mut spans = vec![Span::raw("  ")];

                    // State prefix: selected count or search filter
                    if !app.ui.selected.is_empty() {
                        spans.push(Span::styled(
                            format!("{} selected", app.ui.selected.len()),
                            Style::default().fg(palette::FG_MID),
                        ));
                        spans.push(dash.clone());
                        spans.push(Span::styled("esc", hk));
                        spans.push(Span::styled(" clear", hl));
                        spans.push(dash);
                    } else if app.ui.search_filter.is_some() {
                        spans.push(Span::styled("esc", hk));
                        spans.push(Span::styled(" clear search", hl));
                        spans.push(dash);
                    }

                    // Common action hints
                    spans.extend([
                        Span::styled("m", hk),
                        Span::styled(" message  ", hl),
                        Span::styled("t", hk),
                        Span::styled(" tag  ", hl),
                        Span::styled("f", hk),
                        Span::styled(" fork  ", hl),
                        Span::styled("k", hk),
                        Span::styled(" kill", hl),
                        gap.clone(),
                        Span::styled("/", hk),
                        Span::styled(" search  ", hl),
                        Span::styled("!", hk),
                        Span::styled(" command", hl),
                    ]);

                    // Launch hint only when no selection active
                    if app.ui.selected.is_empty() {
                        spans.extend([gap, Span::styled("tab", hk), Span::styled(" launch", hl)]);
                    }

                    Line::from(spans)
                }
            }
        }
    };
    // Render on the middle row of the 3-row area (non-Compose modes; Compose returns early)
    let mid = Rect::new(area.x, area.y + 1, area.width, 1);
    frame.render_widget(Paragraph::new(line), mid);
}

fn send_target_display(input: &str) -> String {
    use crate::tui::input::compose::is_recipient_char;
    let mut recipients = Vec::new();
    for token in input.split_whitespace() {
        if token.starts_with('@') && token.len() > 1 {
            let name: String = token[1..]
                .chars()
                .take_while(|c| is_recipient_char(*c))
                .collect();
            if !name.is_empty() && !recipients.contains(&name) {
                recipients.push(name);
            }
        }
    }
    if recipients.is_empty() {
        "all".to_string()
    } else {
        recipients.join(", ")
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let key = Style::default().fg(palette::FG_DIM);
    let lbl = Style::default().fg(palette::FG_DARK);
    let dot = Style::default().fg(palette::FG_DARK);

    let hints = match app.ui.mode {
        InputMode::Relay => vec![
            Span::raw("  "),
            Span::styled("\u{2191}\u{2193}", key),
            Span::styled(" navigate ", lbl),
            Span::styled("\u{00b7} ", dot),
            Span::styled("enter", key),
            Span::styled(" select ", lbl),
            Span::styled("\u{00b7} ", dot),
            Span::styled("esc", key),
            Span::styled(" close ", lbl),
            Span::styled("\u{00b7} ", dot),
            Span::styled("?", key),
            Span::styled(" help", lbl),
        ],
        InputMode::CommandOutput => vec![
            Span::raw("  "),
            Span::styled("esc", key),
            Span::styled(" back ", lbl),
            Span::styled("\u{00b7} ", dot),
            Span::styled("?", key),
            Span::styled(" help", lbl),
        ],
        InputMode::Launch => {
            if app.ui.launch.editing.is_some() {
                vec![
                    Span::raw("  "),
                    Span::styled("\u{2191}\u{2193}", key),
                    Span::styled(" save & move ", lbl),
                    Span::styled("\u{00b7} ", dot),
                    Span::styled("esc", key),
                    Span::styled(" cancel", lbl),
                ]
            } else if app.ui.launch.options_cursor.is_some() {
                vec![
                    Span::raw("  "),
                    Span::styled("\u{2191}\u{2193}", key),
                    Span::styled(" navigate ", lbl),
                    Span::styled("\u{00b7} ", dot),
                    Span::styled("\u{2190}\u{2192}", key),
                    Span::styled(" adjust ", lbl),
                    Span::styled("\u{00b7} ", dot),
                    Span::styled("esc", key),
                    Span::styled(" back", lbl),
                ]
            } else {
                vec![
                    Span::raw("  "),
                    Span::styled("enter", key),
                    Span::styled(" launch ", lbl),
                    Span::styled("\u{00b7} ", dot),
                    Span::styled("\u{2190}\u{2192}", key),
                    Span::styled(" tool ", lbl),
                    Span::styled("\u{00b7} ", dot),
                    Span::styled("\u{2191}\u{2193}", key),
                    Span::styled(" settings ", lbl),
                    Span::styled("\u{00b7} ", dot),
                    Span::styled("^o", key),
                    Span::styled(" config ", lbl),
                    Span::styled("\u{00b7} ", dot),
                    Span::styled("esc", key),
                    Span::styled(" close", lbl),
                ]
            }
        }
        InputMode::Compose => vec![
            Span::raw("  "),
            Span::styled("enter", key),
            Span::styled(" send ", lbl),
            Span::styled("\u{00b7} ", dot),
            Span::styled("esc", key),
            Span::styled(" cancel ", lbl),
            Span::styled("\u{00b7} ", dot),
            Span::styled("?", key),
            Span::styled(" help", lbl),
        ],
        InputMode::Navigate => {
            if let Some(ref overlay) = app.ui.overlay {
                if overlay.kind == OverlayKind::Command && overlay.palette.is_some() {
                    vec![
                        Span::raw("  "),
                        Span::styled("\u{2191}\u{2193}", key),
                        Span::styled(" browse ", lbl),
                        Span::styled("\u{00b7} ", dot),
                        Span::styled("tab", key),
                        Span::styled(" fill ", lbl),
                        Span::styled("\u{00b7} ", dot),
                        Span::styled("enter", key),
                        Span::styled(" run ", lbl),
                        Span::styled("\u{00b7} ", dot),
                        Span::styled("esc", key),
                        Span::styled(" cancel ", lbl),
                        Span::styled("\u{00b7} ", dot),
                        Span::styled("?", key),
                        Span::styled(" help", lbl),
                    ]
                } else {
                    let verb = match overlay.kind {
                        OverlayKind::Search => " keep ",
                        OverlayKind::Command => " run ",
                        OverlayKind::Tag => " set ",
                        OverlayKind::Project => " set ",
                    };
                    vec![
                        Span::raw("  "),
                        Span::styled("enter", key),
                        Span::styled(verb, lbl),
                        Span::styled("\u{00b7} ", dot),
                        Span::styled("esc", key),
                        Span::styled(" cancel ", lbl),
                        Span::styled("\u{00b7} ", dot),
                        Span::styled("?", key),
                        Span::styled(" help", lbl),
                    ]
                }
            } else {
                // Hints are in the input bar
                vec![
                    Span::raw("  "),
                    Span::styled("?", key),
                    Span::styled(" help", lbl),
                ]
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(fit_spans(hints, area.width as usize))),
        area,
    );
}

fn render_command_palette(frame: &mut Frame, app: &App) {
    let overlay = match app.ui.overlay.as_ref() {
        Some(o) if o.kind == OverlayKind::Command => o,
        _ => return,
    };
    let pal = match overlay.palette.as_ref() {
        Some(p) if !p.filtered.is_empty() => p,
        _ => return,
    };

    let area = frame.area();

    // Cap visible items to terminal height and list size
    let max_visible = 10usize
        .min(pal.filtered.len())
        .min(area.height.saturating_sub(7) as usize);
    if max_visible == 0 {
        return;
    }

    let popup_h = max_visible as u16 + 2; // +2 for border
    let popup_w = 52u16.min(area.width.saturating_sub(2));

    // Anchor above input bar (3 rows) + footer (1 row) = 4 from bottom
    let popup_y = area.height.saturating_sub(4 + popup_h);
    let popup_x = 1u16.min(area.width.saturating_sub(popup_w));
    let popup = Rect::new(area.x + popup_x, area.y + popup_y, popup_w, popup_h);

    frame.render_widget(Clear, popup);

    // Scroll window: keep cursor visible
    let scroll_offset = match pal.cursor {
        Some(c) if c >= max_visible => c - max_visible + 1,
        _ => 0,
    };

    let inner_w = popup_w.saturating_sub(2) as usize; // inside border
    let cmd_col = 26usize.min(inner_w.saturating_sub(4));
    let desc_col = inner_w.saturating_sub(cmd_col + 2);

    let mut lines: Vec<Line> = Vec::with_capacity(max_visible);
    for (vi, &fi) in pal
        .filtered
        .iter()
        .skip(scroll_offset)
        .take(max_visible)
        .enumerate()
    {
        let sg = &pal.all[fi];
        let idx = scroll_offset + vi;
        let is_sel = pal.cursor == Some(idx);

        let marker = if is_sel { "\u{276f} " } else { "  " };
        let marker_style = if is_sel {
            Style::default().fg(palette::CYAN)
        } else {
            Style::default()
        };
        let cmd_style = if is_sel {
            Style::default()
                .fg(palette::FG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette::FG_MID)
        };
        let desc_style = Style::default().fg(palette::FG_DIM);

        let cmd_text = truncate_display(&sg.command, cmd_col);
        let pad = cmd_col.saturating_sub(UnicodeWidthStr::width(cmd_text.as_str()));
        let desc_text = truncate_display(sg.description, desc_col);

        lines.push(Line::from(vec![
            Span::styled(marker, marker_style),
            Span::styled(cmd_text, cmd_style),
            Span::raw(" ".repeat(pad + 2)),
            Span::styled(desc_text, desc_style),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::MAGENTA));

    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_confirm(frame: &mut Frame, confirm: &Confirm) {
    use crate::tui::app::ConfirmAction;

    let area = frame.area();
    let w = ((UnicodeWidthStr::width(confirm.text.as_str()) + 8) as u16)
        .clamp(24, 50)
        .min(area.width.saturating_sub(4));
    let h = 7u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    frame.render_widget(Clear, popup);

    let is_orphan_action = matches!(confirm.action, ConfirmAction::OrphanAction(_));

    // OrphanAction: left=Kill(red), right=Recover(green). Others: left=No, right=Yes.
    let (left_label, right_label, left_accent, right_accent, border_accent) = if is_orphan_action {
        (
            "Kill",
            "Recover",
            palette::RED,
            palette::GREEN,
            palette::BLUE,
        )
    } else {
        let is_destructive = matches!(
            confirm.action,
            ConfirmAction::KillAgents(_) | ConfirmAction::KillOrphan(_)
        );
        let accent = if is_destructive {
            palette::RED
        } else {
            palette::BLUE
        };
        ("No", "Yes", palette::FG, accent, accent)
    };

    let left_style = if !confirm.selected {
        Style::default()
            .fg(left_accent)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        Theme::dim()
    };
    let right_style = if confirm.selected {
        Style::default()
            .fg(right_accent)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        Theme::dim()
    };

    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(&confirm.text, Style::default().fg(palette::FG)),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                if !confirm.selected { "\u{276f} " } else { "  " },
                left_style,
            ),
            Span::styled(left_label, left_style),
            Span::raw("    "),
            Span::styled(
                if confirm.selected { "\u{276f} " } else { "  " },
                right_style,
            ),
            Span::styled(right_label, right_style),
        ]),
    ];

    let title = match confirm.action {
        ConfirmAction::KillAgents(ref names) if names.len() > 1 => " Batch Kill ",
        ConfirmAction::KillAgents(_) | ConfirmAction::KillOrphan(_) => " Kill ",
        ConfirmAction::ForkAgents(ref names) if names.len() > 1 => " Batch Fork ",
        ConfirmAction::ForkAgents(_) => " Fork ",
        ConfirmAction::OrphanAction(_) => " Orphan ",
    };
    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(border_accent)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_accent));

    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, popup);
}

fn render_relay_popup(frame: &mut Frame, relay: &RelayPopupState) {
    let area = frame.area();
    let w = 34u16.min(area.width.saturating_sub(4));
    let h = if relay.editing_token { 10u16 } else { 9u16 };
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    frame.render_widget(Clear, popup);

    let toggle_sel = relay.cursor == 0;
    let (toggle_label, toggle_icon, toggle_color) = if relay.toggling {
        ("...", "\u{25cb}", palette::YELLOW)
    } else if relay.enabled {
        ("enabled", "\u{25cf}", palette::GREEN)
    } else {
        ("disabled", "\u{25cb}", palette::FG_DIM)
    };
    let toggle_style = if toggle_sel {
        Style::default()
            .fg(toggle_color)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        Style::default().fg(toggle_color)
    };
    let toggle_cursor = if toggle_sel { "\u{276f} " } else { "  " };
    let toggle_cursor_style = if toggle_sel {
        Style::default().fg(palette::CYAN)
    } else {
        Style::default()
    };

    let actions = RELAY_ACTIONS;
    let mut lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(toggle_cursor, toggle_cursor_style),
            Span::styled(format!("{} ", toggle_icon), toggle_style),
            Span::styled(toggle_label.to_string(), toggle_style),
        ]),
        Line::raw(""),
    ];

    for (i, action) in actions.iter().enumerate() {
        let sel = relay.cursor == (i as u8) + 1;
        let style = if sel {
            Style::default().fg(palette::CYAN)
        } else {
            Style::default().fg(palette::FG)
        };
        let cursor = if sel { "\u{276f} " } else { "  " };
        let cursor_style = if sel {
            Style::default().fg(palette::CYAN)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(cursor, cursor_style),
            Span::styled(action.to_string(), style),
        ]));
    }

    if relay.editing_token {
        lines.push(Line::raw(""));
        let display = if relay.token_input.is_empty() {
            Span::styled("paste token\u{2026}", Style::default().fg(palette::FG_DARK))
        } else {
            Span::styled(relay.token_input.clone(), Style::default().fg(palette::FG))
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{276f} ", Style::default().fg(palette::CYAN)),
            display,
        ]));
    }

    let block = Block::default()
        .title(Span::styled(" relay ", Theme::title()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::CYAN));

    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, popup);

    // Position cursor in token input
    if relay.editing_token {
        let display_width = unicode_width::UnicodeWidthStr::width(
            &relay.token_input[..relay.token_cursor.min(relay.token_input.len())],
        );
        let cursor_x = popup.x + 2 + 2 + display_width as u16;
        let cursor_y = popup.y + h - 2;
        frame.set_cursor_position(Position::new(
            cursor_x.min(popup.x + popup.width - 1),
            cursor_y,
        ));
    }
}

fn render_help(frame: &mut Frame, help_scroll: u16) {
    let area = frame.area();

    let key = |k: &str| Span::styled(format!("  {:<12}", k), Style::default().fg(palette::FG));
    let desc = |d: &str| Span::styled(d.to_string(), Theme::dim());
    let section = |title: &str| {
        Line::from(Span::styled(
            format!("  {}", title),
            Style::default().fg(palette::FG_MID),
        ))
    };
    let help_lines = vec![
        Line::raw(""),
        section("Navigate"),
        Line::from(vec![
            key("\u{2191}\u{2193} / \u{2190}\u{2192}"),
            desc("move cursor"),
        ]),
        Line::from(vec![key("enter/space"), desc("select + filter scrollback")]),
        Line::from(vec![key("a"), desc("select all")]),
        Line::from(vec![key("b"), desc("broadcast to all")]),
        Line::from(vec![key("\\"), desc("toggle view")]),
        Line::from(vec![key("ctrl+r"), desc("relay settings")]),
        Line::from(vec![key("ctrl+s"), desc("all stopped agents")]),
        Line::from(vec![key("ctrl+d"), desc("quit")]),
        Line::raw(""),
        section("Compose"),
        Line::from(vec![key("enter"), desc("send message")]),
        Line::from(vec![key("esc"), desc("cancel")]),
        Line::raw(""),
    ];

    let total = help_lines.len() as u16;
    // Size popup to content (+ 2 for border), clamped to terminal
    let w = 44u16.min(area.width.saturating_sub(4));
    let h = (total + 2).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    frame.render_widget(Clear, popup);

    let inner_h = h.saturating_sub(2);
    let max_scroll = total.saturating_sub(inner_h);
    let scroll_pos = help_scroll.min(max_scroll);

    let block = Block::default()
        .title(Span::styled(" Keyboard ", Theme::title()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::separator());

    let para = Paragraph::new(help_lines)
        .block(block)
        .scroll((scroll_pos, 0));
    frame.render_widget(para, popup);

    if total > inner_h {
        // Render scrollbar inside the border
        let sb_area = Rect::new(popup.x + popup.width - 1, popup.y + 1, 1, inner_h);
        let mut state = ScrollbarState::new(max_scroll as usize).position(scroll_pos as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("\u{2502}"))
                .track_style(Theme::separator())
                .thumb_style(Style::default().fg(palette::FG_DIM)),
            sb_area,
            &mut state,
        );
    }
}
