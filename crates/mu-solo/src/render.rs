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
    // Reserve 2 cols for "│ " prefix + 2 cols safety gutter (so
    // ratatui's Paragraph wrap can't re-wrap and strip the prefix).
    let inner = wrap_width.saturating_sub(4).max(1);
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

// ===========================================================================
// Structured turn model (mu-d04a Phase 1).
//
// A turn is the model the renderer paints; `handle_notification` builds it
// from the wire events and `render_turn` turns it into `Line`s. The same
// model renders live (in-flight, tail-truncated, no closer) and on commit
// (full, with closer, inserted into scrollback) — re-rendered from scratch
// each frame, so there is no incremental-emission state to drift (fixes
// mu-304g / mu-926g). `TurnItem` is a closed enum because mu-solo has a
// small, bounded set of in-turn item kinds (cf. codex-rs/tui's open
// `trait HistoryCell`, justified there by ~12 cell types).
// ===========================================================================

/// One renderable item inside an assistant turn. Display-ready: the
/// notification handler does titlecasing / primary-arg extraction so the
/// renderer stays pure and free of wire-format knowledge.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnItem {
    /// Streamed assistant prose. Appended-to in place as deltas arrive.
    Text(String),
    /// A model tool call: `● Name(primary_arg)`.
    ToolCall { display_name: String, primary_arg: String },
    /// A tool result. `kind` is the outcome kind ("ok" | "err" | other);
    /// `text` is the result body (ok) or message (err).
    ToolResult { kind: String, text: String },
    /// An inline error attached to the turn (e.g. `session.error`).
    Error(String),
}

/// Render the body of a turn: header line + each item's lines. Does NOT
/// emit the closer — callers add it via [`turn_closer`] only when
/// committing to scrollback (the live preview is open-ended). `wrap_width`
/// is the caller's `terminal_width - 2`; body wraps to `wrap_width - 4`
/// (2 cols for the `│ ` prefix + 2 safety gutter against ratatui re-wrap),
/// matching the legacy `emit_body_chunk` / `block_lines` budget.
/// `tool_preview_lines` bounds tool-result output (4 normally, 15 in yolo).
pub fn render_turn(
    label: &str,
    color: Color,
    items: &[TurnItem],
    wrap_width: usize,
    tool_preview_lines: usize,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(Span::styled(
        format!("┌─ {label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    for item in items {
        out.extend(render_turn_item(item, color, wrap_width, tool_preview_lines));
    }
    out
}

/// The closer for a committed turn: `└─` plus a trailing blank line.
pub fn turn_closer(color: Color) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled("└─".to_string(), Style::default().fg(color))),
        Line::from(""),
    ]
}

/// Render a single [`TurnItem`] to `│`-prefixed lines. Pure — no I/O, no
/// `self`. `color` is the turn's bar color; `wrap_width` is the caller's
/// `terminal_width - 2`.
pub fn render_turn_item(
    item: &TurnItem,
    color: Color,
    wrap_width: usize,
    tool_preview_lines: usize,
) -> Vec<Line<'static>> {
    match item {
        TurnItem::Text(body) => {
            let inner = wrap_width.saturating_sub(4).max(1);
            let mut lines = Vec::new();
            for raw in body.lines() {
                for row in wrap_line(raw, inner) {
                    lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(color)),
                        Span::raw(row),
                    ]));
                }
            }
            lines
        }
        TurnItem::ToolCall { display_name, primary_arg } => {
            let header_text = if primary_arg.is_empty() {
                format!("● {display_name}")
            } else {
                // "│ ● Name()" overhead = display_name.len() + 5.
                let max_arg_len = wrap_width.saturating_sub(display_name.len() + 5);
                let arg = if primary_arg.chars().count() > max_arg_len {
                    let short: String =
                        primary_arg.chars().take(max_arg_len.saturating_sub(1)).collect();
                    format!("{short}…")
                } else {
                    primary_arg.clone()
                };
                format!("● {display_name}({arg})")
            };
            vec![Line::from(vec![
                Span::styled("│ ", Style::default().fg(color)),
                Span::styled(
                    header_text,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
            ])]
        }
        TurnItem::ToolResult { kind, text } => {
            render_tool_result(kind, text, color, wrap_width, tool_preview_lines)
        }
        TurnItem::Error(msg) => {
            let short: String = msg.chars().take(400).collect();
            vec![Line::from(vec![
                Span::styled("│ ", Style::default().fg(color)),
                Span::styled("× ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::styled(short, Style::default().fg(Color::Red)),
            ])]
        }
    }
}

/// Render a tool result as a bounded `⎿`-connector preview. Mirrors the
/// legacy `emit_tool_call_completed`: ANSI-aware on success, red on error,
/// `… +N lines` overflow marker, `(no output)` / `(kind)` fallbacks.
fn render_tool_result(
    kind: &str,
    text: &str,
    color: Color,
    wrap_width: usize,
    preview_lines: usize,
) -> Vec<Line<'static>> {
    let bar_style = Style::default().fg(color);
    let indent = "│   ";
    match kind {
        "ok" if text.is_empty() => vec![Line::from(vec![
            Span::styled(indent.to_string(), bar_style),
            Span::styled("⎿  (no output)".to_string(), Style::default().fg(Color::DarkGray)),
        ])],
        "ok" => {
            let all: Vec<&str> = text.lines().collect();
            let total = all.len();
            let show = total.min(preview_lines);
            let max_w = wrap_width.saturating_sub(8);
            let mut out: Vec<Line<'static>> = Vec::new();
            for (i, &tl) in all.iter().take(show).enumerate() {
                let truncated: String = tl.chars().take(max_w).collect();
                let prefix = if i == 0 { format!("{indent}⎿  ") } else { format!("{indent}   ") };
                use ansi_to_tui::IntoText;
                let styled = truncated
                    .clone()
                    .into_text()
                    .ok()
                    .and_then(|t| t.into_iter().next())
                    .unwrap_or_else(|| Line::raw(truncated.clone()));
                let mut spans: Vec<Span<'static>> = vec![Span::styled(prefix, bar_style)];
                spans.extend(styled.spans);
                out.push(Line::from(spans));
            }
            if total > show {
                out.push(Line::from(vec![
                    Span::styled(format!("{indent}   "), bar_style),
                    Span::styled(
                        format!("… +{} lines", total - show),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            out
        }
        "err" => {
            let msg = if text.is_empty() { "(unknown error)" } else { text };
            let all: Vec<&str> = msg.lines().collect();
            let total = all.len();
            let show = total.min(preview_lines);
            let max_w = wrap_width.saturating_sub(8);
            let mut out: Vec<Line<'static>> = Vec::new();
            for (i, &tl) in all.iter().take(show).enumerate() {
                let truncated: String = tl.chars().take(max_w).collect();
                let prefix = if i == 0 { format!("{indent}⎿  ") } else { format!("{indent}   ") };
                out.push(Line::from(vec![
                    Span::styled(prefix, bar_style),
                    Span::styled(truncated, Style::default().fg(Color::Red)),
                ]));
            }
            if total > show {
                out.push(Line::from(vec![
                    Span::styled(format!("{indent}   "), bar_style),
                    Span::styled(
                        format!("… +{} lines", total - show),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            out
        }
        other => vec![Line::from(vec![
            Span::styled(indent.to_string(), bar_style),
            Span::styled(format!("⎿  ({other})"), Style::default().fg(Color::DarkGray)),
        ])],
    }
}

/// Tail-truncate rendered lines to at most `max_rows`, keeping the most
/// recent rows (the growing edge) and replacing the elided head with a
/// `⋯ +N earlier rows` marker. Used for the in-flight live preview when a
/// turn is taller than the live region. `max_rows == 0` yields empty.
pub fn tail_truncate(lines: Vec<Line<'static>>, max_rows: usize) -> Vec<Line<'static>> {
    if max_rows == 0 {
        return Vec::new();
    }
    if lines.len() <= max_rows {
        return lines;
    }
    let hidden = lines.len() - (max_rows - 1);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(max_rows);
    out.push(Line::from(Span::styled(
        format!("⋯ +{hidden} earlier rows"),
        Style::default().fg(Color::DarkGray),
    )));
    out.extend(lines.into_iter().skip(hidden));
    out
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

    // ---- structured turn model (mu-d04a) ----

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect::<String>())
            .collect()
    }

    #[test]
    fn render_turn_header_then_items_no_closer() {
        let items = vec![TurnItem::Text("hello".into())];
        let lines = render_turn("assistant", Color::White, &items, 80, 4);
        let p = plain(&lines);
        assert_eq!(p[0], "┌─ assistant ");
        assert_eq!(p[1], "│ hello");
        // No closer in the body render.
        assert!(!p.iter().any(|l| l.starts_with("└─")));
    }

    #[test]
    fn render_tool_call_header_format() {
        let item = TurnItem::ToolCall {
            display_name: "Bash".into(),
            primary_arg: "ls -la".into(),
        };
        let lines = render_turn_item(&item, Color::White, 80, 4);
        assert_eq!(plain(&lines)[0], "│ ● Bash(ls -la)");
    }

    #[test]
    fn render_tool_result_ok_bounds_and_overflow_marker() {
        let body = (1..=10).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let lines = render_turn_item(
            &TurnItem::ToolResult { kind: "ok".into(), text: body },
            Color::White,
            80,
            4,
        );
        let p = plain(&lines);
        assert!(p[0].contains("⎿  line 1"));
        assert_eq!(p.len(), 5); // 4 preview + overflow marker
        assert!(p[4].contains("… +6 lines"));
    }

    #[test]
    fn render_tool_result_no_output() {
        let lines = render_turn_item(
            &TurnItem::ToolResult { kind: "ok".into(), text: String::new() },
            Color::White,
            80,
            4,
        );
        assert!(plain(&lines)[0].contains("(no output)"));
    }

    #[test]
    fn turn_closer_is_bar_and_blank() {
        let lines = turn_closer(Color::White);
        let p = plain(&lines);
        assert_eq!(p, vec!["└─".to_string(), "".to_string()]);
    }

    #[test]
    fn tail_truncate_passthrough_when_short() {
        let full = render_turn("assistant", Color::White, &[TurnItem::Text("a\nb".into())], 80, 4);
        let truncated = tail_truncate(full.clone(), 50);
        assert_eq!(plain(&full), plain(&truncated));
    }

    #[test]
    fn tail_truncate_keeps_tail_with_marker() {
        let body = (1..=20).map(|n| format!("row {n}")).collect::<Vec<_>>().join("\n");
        let full = render_turn("assistant", Color::White, &[TurnItem::Text(body)], 80, 4);
        let max_rows = 6;
        let truncated = tail_truncate(full.clone(), max_rows);
        assert_eq!(truncated.len(), max_rows);
        // First row is the elision marker; the rest are EXACTLY the tail of
        // the full render (the codex "live view == tail of full render"
        // invariant — what keeps mu-304g from recurring).
        let fp = plain(&full);
        let tp = plain(&truncated);
        assert!(tp[0].starts_with("⋯ +"));
        assert_eq!(&tp[1..], &fp[fp.len() - (max_rows - 1)..]);
    }
}
