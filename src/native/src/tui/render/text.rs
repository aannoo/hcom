use ratatui::prelude::*;

use crate::tui::theme::{Theme, palette};

/// Format agent name to exactly `width` display columns, truncating if needed.
pub(crate) fn fmt_agent(name: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let w = UnicodeWidthStr::width(name);
    if w <= width {
        format!("{:<width$}", name, width = width)
    } else {
        let mut s = String::new();
        let mut cur = 0;
        for ch in name.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur + cw > width {
                break;
            }
            s.push(ch);
            cur += cw;
        }
        while cur < width {
            s.push(' ');
            cur += 1;
        }
        s
    }
}

/// Split a span into sub-spans, highlighting substrings matching `query` (case-insensitive).
/// Uses char indices to avoid byte-offset mismatches when lowercase changes byte length.
fn highlight_in_span(span: Span<'static>, query: &str) -> Vec<Span<'static>> {
    let text = span.content.to_string();
    let base_style = span.style;
    let hl_style = Theme::search_match();

    if query.is_empty() || text.is_empty() {
        return vec![span];
    }

    let byte_offsets: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let n = byte_offsets.len() - 1; // char count

    let mut result: Vec<Span<'static>> = Vec::new();
    let mut last = 0usize; // char index
    let mut ci = 0usize;

    'outer: while ci + query_lower.len() <= n {
        for (j, &qc) in query_lower.iter().enumerate() {
            let tc = text[byte_offsets[ci + j]..].chars().next().unwrap();
            if tc.to_lowercase().next().unwrap_or(tc) != qc {
                ci += 1;
                continue 'outer;
            }
        }
        // Match at char index ci..ci+query_lower.len()
        let end = ci + query_lower.len();
        if ci > last {
            result.push(Span::styled(
                text[byte_offsets[last]..byte_offsets[ci]].to_string(),
                base_style,
            ));
        }
        result.push(Span::styled(
            text[byte_offsets[ci]..byte_offsets[end]].to_string(),
            hl_style,
        ));
        last = end;
        ci = end;
    }

    if last < n {
        result.push(Span::styled(
            text[byte_offsets[last]..].to_string(),
            base_style,
        ));
    }
    if result.is_empty() {
        vec![span]
    } else {
        result
    }
}

/// Apply search highlighting to a vec of spans.
pub(crate) fn highlight_spans(
    spans: Vec<Span<'static>>,
    query: Option<&str>,
) -> Vec<Span<'static>> {
    match query {
        Some(q) if !q.is_empty() => spans
            .into_iter()
            .flat_map(|s| highlight_in_span(s, q))
            .collect(),
        _ => spans,
    }
}

/// Push a token onto the current line, breaking character-by-character if it exceeds max_w.
fn push_token(
    token: &str,
    style: Style,
    lines: &mut Vec<Line<'static>>,
    line_spans: &mut Vec<Span<'static>>,
    line_w: &mut usize,
    max_w: usize,
    indent: usize,
) {
    let tw = unicode_width::UnicodeWidthStr::width(token);
    if tw <= max_w {
        // Word fits on a line — just wrap if current line is too full
        if *line_w > 0 && *line_w + tw > max_w {
            lines.push(Line::from(std::mem::take(line_spans)));
            *line_spans = vec![Span::raw(" ".repeat(indent))];
            *line_w = 0;
            // Trim leading whitespace at start of new line
            let trimmed = token.trim_start();
            if !trimmed.is_empty() {
                let tw = unicode_width::UnicodeWidthStr::width(trimmed);
                line_spans.push(Span::styled(trimmed.to_string(), style));
                *line_w += tw;
            }
        } else {
            line_spans.push(Span::styled(token.to_string(), style));
            *line_w += tw;
        }
    } else {
        // Token exceeds max_w — break character by character
        let mut chunk = String::new();
        let mut cw = 0usize;
        for ch in token.chars() {
            let char_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if *line_w + cw + char_w > max_w && (*line_w + cw) > 0 {
                if !chunk.is_empty() {
                    line_spans.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                lines.push(Line::from(std::mem::take(line_spans)));
                *line_spans = vec![Span::raw(" ".repeat(indent))];
                *line_w = 0;
                cw = 0;
            }
            chunk.push(ch);
            cw += char_w;
        }
        if !chunk.is_empty() {
            line_spans.push(Span::styled(chunk, style));
            *line_w += cw;
        }
    }
}

pub(crate) fn render_body(text: &str, width: usize, query: Option<&str>) -> Vec<Line<'static>> {
    let indent = 4usize;
    let max_w = width.saturating_sub(indent);
    if max_w == 0 {
        return vec![Line::raw("")];
    }

    // Parse text into styled segments (handling @mentions)
    let mut segments: Vec<Span<'static>> = Vec::new();
    let mut chars = text.chars().peekable();
    let mut current = String::new();

    while let Some(ch) = chars.next() {
        if ch == '@' {
            if !current.is_empty() {
                segments.push(Span::styled(
                    current.clone(),
                    Style::default().fg(palette::FG),
                ));
                current.clear();
            }
            let mut mention = String::from("@");
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '-' || next == '_' || next == ':' {
                    mention.push(chars.next().unwrap());
                } else {
                    break;
                }
            }
            if mention.len() > 1 {
                segments.push(Span::styled(mention, Theme::mention()));
            } else {
                current.push('@');
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        segments.push(Span::styled(current, Style::default().fg(palette::FG)));
    }

    // Apply search highlighting
    segments = highlight_spans(segments, query);

    // Wrap segments into lines
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut line_spans: Vec<Span<'static>> = vec![Span::raw(" ".repeat(indent))];
    let mut line_w = 0usize;

    for span in segments {
        let span_text: &str = span.content.as_ref();
        let span_style = span.style;

        // Split span content into words, preserving spaces
        let mut word_start = 0;
        for (i, ch) in span_text.char_indices() {
            if ch == ' ' && i > word_start {
                let word = &span_text[word_start..i];
                push_token(
                    word,
                    span_style,
                    &mut lines,
                    &mut line_spans,
                    &mut line_w,
                    max_w,
                    indent,
                );
                word_start = i; // include the space in next chunk
            }
        }
        // Remaining text from word_start
        if word_start < span_text.len() {
            let rest = &span_text[word_start..];
            let rw = unicode_width::UnicodeWidthStr::width(rest);
            if line_w > 0 && line_w + rw > max_w {
                lines.push(Line::from(std::mem::take(&mut line_spans)));
                line_spans = vec![Span::raw(" ".repeat(indent))];
                line_w = 0;
                let trimmed = rest.trim_start();
                if !trimmed.is_empty() {
                    push_token(
                        trimmed,
                        span_style,
                        &mut lines,
                        &mut line_spans,
                        &mut line_w,
                        max_w,
                        indent,
                    );
                }
            } else {
                line_spans.push(Span::styled(rest.to_string(), span_style));
                line_w += rw;
            }
        }
    }

    if line_spans.len() > 1 || line_w > 0 {
        lines.push(Line::from(line_spans));
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![Span::raw(" ".repeat(indent))]));
    }

    // Add dim left border: replace "    " indent with "  │ "
    let indent_str: String = " ".repeat(indent);
    for line in &mut lines {
        if line
            .spans
            .first()
            .is_some_and(|s| s.content.as_ref() == indent_str)
        {
            line.spans[0] = Span::raw("  ");
            line.spans
                .insert(1, Span::styled("│", Style::default().fg(palette::FG_DARK)));
            line.spans.insert(2, Span::raw(" "));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── fmt_agent ─────────────────────────────────────────────────

    #[test]
    fn fmt_agent_pads_short_name() {
        let result = fmt_agent("abc", 6);
        assert_eq!(result, "abc   ");
        assert_eq!(result.len(), 6);
    }

    #[test]
    fn fmt_agent_exact_width() {
        let result = fmt_agent("abcdef", 6);
        assert_eq!(result, "abcdef");
    }

    #[test]
    fn fmt_agent_truncates_long_name() {
        let result = fmt_agent("abcdefgh", 6);
        assert_eq!(result, "abcdef");
    }

    // ── highlight_in_span ─────────────────────────────────────────

    #[test]
    fn highlight_no_match_returns_original() {
        let span = Span::raw("hello world");
        let result = highlight_in_span(span.clone(), "xyz");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "hello world");
    }

    #[test]
    fn highlight_empty_query_returns_original() {
        let span = Span::raw("hello world");
        let result = highlight_in_span(span, "");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn highlight_single_match() {
        let span = Span::raw("hello world");
        let result = highlight_in_span(span, "world");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "hello ");
        assert_eq!(result[1].content, "world");
    }

    #[test]
    fn highlight_match_at_start() {
        let span = Span::raw("hello world");
        let result = highlight_in_span(span, "hello");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "hello");
        assert_eq!(result[1].content, " world");
    }

    #[test]
    fn highlight_multiple_matches() {
        let span = Span::raw("abcabc");
        let result = highlight_in_span(span, "abc");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "abc");
        assert_eq!(result[1].content, "abc");
    }

    #[test]
    fn highlight_case_insensitive() {
        let span = Span::raw("Hello World");
        let result = highlight_in_span(span, "hello");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "Hello");
        assert_eq!(result[1].content, " World");
    }

    #[test]
    fn highlight_entire_string() {
        let span = Span::raw("abc");
        let result = highlight_in_span(span, "abc");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "abc");
    }

    // ── highlight_spans ───────────────────────────────────────────

    #[test]
    fn highlight_spans_none_query_passthrough() {
        let spans = vec![Span::raw("hello")];
        let result = highlight_spans(spans.clone(), None);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn highlight_spans_across_multiple_input_spans() {
        let spans = vec![Span::raw("hello "), Span::raw("world")];
        let result = highlight_spans(spans, Some("world"));
        // First span unchanged, second span highlighted
        assert_eq!(result[0].content, "hello ");
        assert_eq!(result[1].content, "world");
    }

    // ── render_body ───────────────────────────────────────────────

    #[test]
    fn render_body_wraps_long_text() {
        let text = "a ".repeat(30); // 60 chars
        let lines = render_body(&text, 20, None);
        assert!(lines.len() > 1, "should wrap into multiple lines");
    }

    #[test]
    fn render_body_empty_produces_one_line() {
        let lines = render_body("", 40, None);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn render_body_highlights_mention() {
        let lines = render_body("hello @nova", 40, None);
        // Flatten spans to check @nova gets mention style
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert!(all_text.contains("@nova"));
    }
}
