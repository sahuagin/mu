//! Render module — single rendering contract.
//!
//! Unlike `mu-tui` which juggles Fullscreen (Paragraph re-render) and
//! Inline (`insert_before` into mux scrollback), mu-solo picks ONE
//! contract: `insert_before` for the transcript (composes with mux
//! scrollback so users can scroll back through long sessions natively).
//! Live input prompt renders inside ratatui's inline viewport.
//!
//! At the v0 stage everything renders as plain text blocks with simple
//! `┌─ label / │ body / └─` borders, matching what claude-code's
//! transcript view does.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Build a single labelled block (header + body lines + closer). The
/// caller emits it to `insert_before`.
pub fn block_lines(label: &str, color: Color, body: &str, wrap_width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(Span::styled(
        format!("┌─ {label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    let inner = wrap_width.saturating_sub(2).max(1);
    for raw in body.lines() {
        for row in wrap_line(raw, inner) {
            out.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(color)),
                Span::raw(row),
            ]));
        }
    }
    out.push(Line::from(Span::styled(
        "└─".to_string(),
        Style::default().fg(color),
    )));
    out.push(Line::from(""));
    out
}

/// Word-aware wrap of `line` to `width` columns. Long words that
/// exceed `width` split mid-character. Char-based width (not
/// grapheme-aware) — over-counts combining marks and under-counts
/// CJK; acceptable for v0.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width || width == 0 {
        return vec![line.to_string()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in line.split_inclusive(' ') {
        let wlen = word.chars().count();
        if cur_len + wlen <= width {
            cur.push_str(word);
            cur_len += wlen;
            continue;
        }
        if !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if wlen <= width {
            cur.push_str(word);
            cur_len = wlen;
        } else {
            for ch in word.chars() {
                if cur_len + 1 > width {
                    rows.push(std::mem::take(&mut cur));
                    cur_len = 0;
                }
                cur.push(ch);
                cur_len += 1;
            }
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
}

/// Build the lines for a "you said X" block. Cyan, matching mu-tui.
pub fn you_block(content: &str, wrap_width: usize) -> Vec<Line<'static>> {
    block_lines("you", Color::Cyan, content, wrap_width)
}

/// Build the lines for an "assistant said X" block. White.
pub fn assistant_block(content: &str, wrap_width: usize) -> Vec<Line<'static>> {
    block_lines("assistant", Color::White, content, wrap_width)
}

/// Build the lines for an "error" block. Red.
pub fn error_block(content: &str, wrap_width: usize) -> Vec<Line<'static>> {
    block_lines("ERROR", Color::Red, content, wrap_width)
}

/// Build a wrapped paragraph for ratatui to render. Wrapping is
/// handled by ratatui's `Wrap { trim: false }`; we just pass the
/// lines through.
pub fn into_paragraph(lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_short_line_one_row() {
        assert_eq!(wrap_line("hello", 80), vec!["hello".to_string()]);
    }

    #[test]
    fn wrap_long_line_breaks_on_words() {
        let rows = wrap_line("this is a longer test sentence", 10);
        // Words split on spaces, never breaking a short word mid-char.
        for row in &rows {
            assert!(row.chars().count() <= 10, "row exceeded width: {row:?}");
        }
    }

    #[test]
    fn wrap_zero_width_passthrough() {
        assert_eq!(wrap_line("abc", 0), vec!["abc".to_string()]);
    }

    #[test]
    fn block_lines_has_header_body_closer() {
        let lines = block_lines("you", Color::Cyan, "hi", 80);
        // Expect: ┌─ you / │ hi / └─ / blank
        assert_eq!(lines.len(), 4);
    }
}
