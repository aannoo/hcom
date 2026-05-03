use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::app::App;
use crate::tui::model::*;
use crate::tui::theme::{Theme, palette};

/// Label column width shared with cursor positioning in mod.rs.
pub(crate) const LABEL_W: usize = 9;

pub fn render_launch_inline(frame: &mut Frame, area: Rect, app: &App) {
    let ls = &app.ui.launch;
    let w = area.width;
    let mut lines: Vec<Line> = vec![
        separator_line(w, Some("launch")),
        field_row_dual("Tool", ls.tool.name(), LaunchField::Tool, "Count", &ls.count.to_string(), LaunchField::Count, ls, w),
        field_row_text("Tag", &ls.tag, LaunchField::Tag, ls, w),
        field_row_text("Project", &ls.project, LaunchField::Project, ls, w),
    ];

    if ls.tool == Tool::Claude {
        let check = if ls.headless { "\u{2713}" } else { " " };
        let sel = ls.options_cursor == Some(LaunchField::Headless);
        lines.push(field_row_toggle("Headless", check, sel));
    }

    lines.push(field_row(
        "Terminal",
        ls.terminal_presets
            .get(ls.terminal)
            .map(|s| s.as_str())
            .unwrap_or("auto"),
        LaunchField::Terminal,
        ls,
        w,
    ));

    frame.render_widget(Paragraph::new(lines), area);
}

fn separator_line(width: u16, label: Option<&str>) -> Line<'static> {
    let margin = 2usize;
    let inner = (width as usize).saturating_sub(margin * 2);

    if let Some(text) = label {
        let prefix = "\u{2500}\u{2500} ";
        let label_str = format!("{} ", text);
        let prefix_w = unicode_width::UnicodeWidthStr::width(prefix);
        let fill_len = inner.saturating_sub(prefix_w + label_str.len());
        let fill = "\u{2500}".repeat(fill_len);

        Line::from(vec![
            Span::raw(" ".repeat(margin)),
            Span::styled(prefix, Theme::separator()),
            Span::styled(label_str, Theme::launch_active()),
            Span::styled(fill, Theme::separator()),
        ])
    } else {
        let fill = "\u{2500}".repeat(inner);
        Line::from(vec![
            Span::raw(" ".repeat(margin)),
            Span::styled(fill, Theme::separator()),
        ])
    }
}

/// Dual selector field with two values side by side, 5 spaces apart.
fn field_row_dual(
    label1: &str, value1: &str, field1: LaunchField,
    label2: &str, value2: &str, field2: LaunchField,
    ls: &LaunchState, width: u16,
) -> Line<'static> {
    let sel = ls.options_cursor == Some(field1) || ls.options_cursor == Some(field2);
    let (cursor, cursor_style) = cursor_span(sel);

    let left = field_inline(label1, value1, field1, ls);
    let right = field_inline(label2, value2, field2, ls);

    let mut spans: Vec<Span> = vec![
        Span::raw("  "),
        Span::styled(cursor, cursor_style),
    ];
    spans.extend(left);
    spans.push(Span::raw("     "));
    spans.extend(right);

    Line::from(super::fit_spans(spans, width as usize))
}

/// Compact inline field part (no margin, no cursor).
fn field_inline(label: &str, value: &str, field: LaunchField, ls: &LaunchState) -> Vec<Span<'static>> {
    let sel = ls.options_cursor == Some(field);
    let arrow = if sel {
        Theme::launch_arrow()
    } else {
        Style::default().fg(palette::FG_DARK)
    };
    let val_style = if sel {
        Theme::launch_active()
    } else {
        Style::default().fg(palette::FG)
    };
    vec![
        Span::styled(
            format!("{:<w$}", label, w = 5),
            if sel { Theme::launch_active() } else { Theme::dim() },
        ),
        Span::styled("\u{25c2}", arrow),
        Span::styled(value.to_string(), val_style),
        Span::styled("\u{25b8}", arrow),
    ]
}

/// Selector field with ◂ value ▸ arrows.
fn field_row(
    label: &str,
    value: &str,
    field: LaunchField,
    ls: &LaunchState,
    width: u16,
) -> Line<'static> {
    let sel = ls.options_cursor == Some(field);
    let style = if sel {
        Theme::launch_active()
    } else {
        Style::default().fg(palette::FG)
    };
    let arrow = if sel {
        Theme::launch_arrow()
    } else {
        Style::default().fg(palette::FG_DARK)
    };
    let (cursor, cursor_style) = cursor_span(sel);

    let spans = vec![
        Span::raw("  "),
        Span::styled(cursor, cursor_style),
        Span::styled(
            format!("{:<w$}", label, w = LABEL_W),
            if sel {
                Theme::launch_active()
            } else {
                Theme::dim()
            },
        ),
        Span::styled("\u{25c2} ", arrow),
        Span::styled(value.to_string(), style),
        Span::styled(" \u{25b8}", arrow),
    ];
    Line::from(super::fit_spans(spans, width as usize))
}

/// Text field with inline editing.
fn field_row_text(
    label: &str,
    value: &str,
    field: LaunchField,
    ls: &LaunchState,
    width: u16,
) -> Line<'static> {
    let sel = ls.options_cursor == Some(field);
    let editing = ls.editing == Some(field);
    let label_style = if sel || editing {
        Theme::launch_active()
    } else {
        Theme::dim()
    };
    let (cursor, cursor_style) = cursor_span(sel || editing);

    if editing {
        let pos = ls.edit_cursor.min(value.len());
        let before = &value[..pos];
        let after = &value[pos..];

        let spans = vec![
            Span::raw("  "),
            Span::styled(cursor, cursor_style),
            Span::styled(format!("{:<w$}", label, w = LABEL_W), label_style),
            Span::styled(before.to_string(), Style::default().fg(palette::FG)),
            Span::styled("\u{2502}", Style::default().fg(palette::BLUE)),
            Span::styled(after.to_string(), Style::default().fg(palette::FG)),
        ];
        Line::from(super::fit_spans(spans, width as usize))
    } else {
        let (display, val_style) = if value.is_empty() {
            (
                "\u{2500}".to_string(),
                Style::default().fg(palette::FG_DARK),
            )
        } else {
            (value.to_string(), Style::default().fg(palette::FG))
        };

        let spans = vec![
            Span::raw("  "),
            Span::styled(cursor, cursor_style),
            Span::styled(format!("{:<w$}", label, w = LABEL_W), label_style),
            Span::styled(display, val_style),
        ];
        Line::from(super::fit_spans(spans, width as usize))
    }
}

/// Toggle field with [✓] checkbox.
fn field_row_toggle(label: &str, check: &str, selected: bool) -> Line<'static> {
    let label_style = if selected {
        Theme::launch_active()
    } else {
        Theme::dim()
    };
    let val_style = if selected {
        Theme::launch_active()
    } else {
        Style::default().fg(palette::FG)
    };
    let (cursor, cursor_style) = cursor_span(selected);

    Line::from(vec![
        Span::raw("  "),
        Span::styled(cursor, cursor_style),
        Span::styled(format!("{:<w$}", label, w = LABEL_W), label_style),
        Span::styled(format!("[{}]", check), val_style),
    ])
}

fn cursor_span(active: bool) -> (&'static str, Style) {
    if active {
        ("\u{276f} ", Theme::cursor())
    } else {
        ("  ", Style::default())
    }
}
