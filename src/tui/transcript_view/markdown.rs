//! Just enough Markdown for agent text: headings, bullets, **bold**,
//! `code` and fenced blocks. Anything else is shown as written.
use ratatui::prelude::*;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::theme::palette;

/// A run of text in one style.
pub type Segment = (String, Style);

fn code_style() -> Style {
    Style::default().fg(palette::CYAN)
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Fence lines in `lines`; an odd count leaves a code block open.
pub fn fence_count(lines: &[String]) -> usize {
    lines.iter().filter(|l| is_fence(l)).count()
}

/// `#`s and the space after them, when the line is a heading.
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) {
        line[hashes..].strip_prefix(' ')
    } else {
        None
    }
}

/// Leading indent and the item text, when the line is a `-`/`*` bullet.
fn bullet_text(line: &str) -> Option<(&str, &str)> {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    body.strip_prefix("- ")
        .or_else(|| body.strip_prefix("* "))
        .map(|text| (indent, text))
}

/// `**bold**` and `` `code` `` inside one line. Unclosed markers stay as text.
fn inline(text: &str, base: Style) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let marked = if let Some(after) = rest.strip_prefix("**") {
            after.find("**").filter(|&end| end > 0).map(|end| {
                (
                    &after[..end],
                    &after[end + 2..],
                    base.add_modifier(Modifier::BOLD),
                )
            })
        } else if let Some(after) = rest.strip_prefix('`') {
            after
                .find('`')
                .filter(|&end| end > 0)
                .map(|end| (&after[..end], &after[end + 1..], code_style()))
        } else {
            None
        };
        match marked {
            Some((inner, after, style)) => {
                if !plain.is_empty() {
                    out.push((std::mem::take(&mut plain), base));
                }
                out.push((inner.to_string(), style));
                rest = after;
            }
            None => {
                let ch = rest.chars().next().expect("non-empty");
                plain.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    if !plain.is_empty() {
        out.push((plain, base));
    }
    out
}

/// One line styled; `in_code` carries an open fence across lines. `None`
/// for fence lines, which are not shown.
pub fn line(text: &str, in_code: &mut bool, base: Style) -> Option<Vec<Segment>> {
    if is_fence(text) {
        *in_code = !*in_code;
        return None;
    }
    if *in_code {
        return Some(vec![(text.to_string(), code_style())]);
    }
    if let Some(title) = heading_text(text) {
        return Some(inline(title, base.add_modifier(Modifier::BOLD)));
    }
    if let Some((indent, item)) = bullet_text(text) {
        let mut out = vec![(format!("{indent}• "), base)];
        out.extend(inline(item, base));
        return Some(out);
    }
    Some(inline(text, base))
}

/// The line as plain text, for one-line summaries.
pub fn plain(text: &str) -> String {
    let mut in_code = false;
    line(text, &mut in_code, Style::default())
        .map(|segs| segs.into_iter().map(|(s, _)| s).collect())
        .unwrap_or_default()
}

pub fn is_table_row(text: &str) -> bool {
    let t = text.trim();
    t.len() > 1 && t.starts_with('|') && t.ends_with('|')
}

fn table_cells(row: &str) -> Vec<&str> {
    let t = row.trim();
    t[1..t.len() - 1].split('|').map(str::trim).collect()
}

fn is_separator(cells: &[&str]) -> bool {
    cells.iter().all(|c| {
        let c = c.trim_matches(':');
        !c.is_empty() && c.chars().all(|ch| ch == '-')
    })
}

/// `|`-rows as aligned columns, header bold, the `---` row as a rule.
/// `None` when the table does not fit in `width`.
pub fn table(rows: &[String], base: Style, width: usize) -> Option<Vec<Vec<Segment>>> {
    let parsed: Vec<Vec<&str>> = rows.iter().map(|r| table_cells(r)).collect();
    let header_rows = parsed.iter().position(|c| is_separator(c)).unwrap_or(0);
    let cols = parsed.iter().map(Vec::len).max()?;
    let styled: Vec<Option<Vec<Vec<Segment>>>> = parsed
        .iter()
        .enumerate()
        .map(|(i, cells)| {
            if is_separator(cells) {
                return None;
            }
            let style = if i < header_rows {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            };
            Some(cells.iter().map(|c| inline(c, style)).collect())
        })
        .collect();
    let cell_width = |segs: &Vec<Segment>| segs.iter().map(|(s, _)| s.width()).sum::<usize>();
    let mut widths = vec![0; cols];
    for row in styled.iter().flatten() {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell_width(cell));
        }
    }
    let sep = " │ ";
    if widths.iter().sum::<usize>() + sep.width() * (cols - 1) > width {
        return None;
    }
    let rule = Style::default().fg(palette::FG_DIM);
    Some(
        styled
            .iter()
            .map(|row| match row {
                None => vec![(
                    widths
                        .iter()
                        .map(|w| "─".repeat(*w))
                        .collect::<Vec<_>>()
                        .join("─┼─"),
                    rule,
                )],
                Some(cells) => {
                    let mut out: Vec<Segment> = Vec::new();
                    for (i, w) in widths.iter().enumerate() {
                        if i > 0 {
                            out.push((sep.to_string(), rule));
                        }
                        let cell = cells.get(i).cloned().unwrap_or_default();
                        let pad = w - cell_width(&cell);
                        out.extend(cell);
                        if i + 1 < cols {
                            out.push((" ".repeat(pad), base));
                        }
                    }
                    out
                }
            })
            .collect(),
    )
}

pub fn spans(segments: Vec<Segment>) -> Vec<Span<'static>> {
    segments
        .into_iter()
        .filter(|(s, _)| !s.is_empty())
        .map(|(s, style)| Span::styled(s, style))
        .collect()
}

/// Split into rows of at most `width` columns, breaking after the last
/// space when the row has one.
pub fn wrap(segments: &[Segment], width: usize) -> Vec<Vec<Segment>> {
    let mut rows: Vec<Vec<(char, Style)>> = vec![Vec::new()];
    let mut used = 0;
    for (text, style) in segments {
        for ch in text.chars() {
            let w = ch.width().unwrap_or(0);
            let row = rows.last_mut().expect("non-empty");
            if used + w > width && !row.is_empty() {
                let carry = match row.iter().rposition(|(c, _)| *c == ' ') {
                    Some(i) if i > 0 && !ch.is_whitespace() => row.split_off(i + 1),
                    _ => Vec::new(),
                };
                used = carry.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
                rows.push(carry);
            }
            rows.last_mut().expect("non-empty").push((ch, *style));
            used += w;
        }
    }
    rows.into_iter()
        .map(|row| {
            let mut out: Vec<Segment> = Vec::new();
            for (ch, style) in row {
                match out.last_mut() {
                    Some((s, st)) if *st == style => s.push(ch),
                    _ => out.push((ch.to_string(), style)),
                }
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(segs: &[Segment]) -> Vec<&str> {
        segs.iter().map(|(s, _)| s.as_str()).collect()
    }

    #[test]
    fn inline_markers() {
        let base = Style::default();
        let segs = inline("a **b** `c` d", base);
        assert_eq!(texts(&segs), vec!["a ", "b", " ", "c", " d"]);
        assert!(segs[1].1.add_modifier.contains(Modifier::BOLD));
        assert_eq!(segs[3].1, code_style());
        // Unclosed or empty markers stay literal.
        assert_eq!(texts(&inline("x ** y `z", base)), vec!["x ** y `z"]);
        assert_eq!(texts(&inline("****", base)), vec!["****"]);
    }

    #[test]
    fn block_lines() {
        let base = Style::default();
        let mut code = false;
        let h = line("## Title **bold**", &mut code, base).unwrap();
        assert_eq!(texts(&h), vec!["Title ", "bold"]);
        assert!(h[0].1.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            texts(&line("  - item", &mut code, base).unwrap()),
            vec!["  • ", "item"]
        );
        assert_eq!(
            texts(&line("**bold** line", &mut code, base).unwrap()),
            vec!["bold", " line"]
        );
        assert!(line("```rust", &mut code, base).is_none());
        assert!(code);
        let c = line("let **x** = 1;", &mut code, base).unwrap();
        assert_eq!(c, vec![("let **x** = 1;".into(), code_style())]);
        assert!(line("```", &mut code, base).is_none());
        assert!(!code);
        assert_eq!(
            texts(&line("#no-space", &mut code, base).unwrap()),
            vec!["#no-space"]
        );
        assert_eq!(plain("- **a** `b`"), "• a b");
        assert_eq!(fence_count(&["```".into(), "x".into()]), 1);
    }

    #[test]
    fn tables_align() {
        let rows: Vec<String> = [
            "| ＩＤ | Tool |",
            "|---|:--:|",
            "| **ＧＯ** | `git diff` |",
            "| a |",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(rows.iter().all(|r| is_table_row(r)));
        assert!(!is_table_row("| open"));
        let out = table(&rows, Style::default(), 40).unwrap();
        let joined: Vec<String> = out.iter().map(|r| texts(r).concat()).collect();
        assert_eq!(
            joined,
            vec![
                "ＩＤ │ Tool",
                "─────┼─────────",
                "ＧＯ │ git diff",
                "a    │ "
            ]
        );
        assert!(out[0][0].1.add_modifier.contains(Modifier::BOLD));
        assert!(table(&rows, Style::default(), 10).is_none());
    }

    #[test]
    fn wrap_keeps_styles_and_breaks_at_spaces() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let segs = vec![
            ("ab ".to_string(), Style::default()),
            ("cdef".to_string(), bold),
        ];
        let rows = wrap(&segs, 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(texts(&rows[0]), vec!["ab "]);
        assert_eq!(rows[1], vec![("cdef".to_string(), bold)]);
        let rows = wrap(&[("ＡＢＣＤ".to_string(), Style::default())], 4);
        assert_eq!(
            rows.iter().map(|r| texts(r).concat()).collect::<Vec<_>>(),
            vec!["ＡＢ", "ＣＤ"]
        );
        assert_eq!(wrap(&[], 4), vec![Vec::<Segment>::new()]);
    }
}
