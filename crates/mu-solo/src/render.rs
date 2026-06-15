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

/// Stand-alone error notice (an error with no turn to attach to): a bold
/// `× <header>` line followed by the FULL message, word-wrapped to the
/// viewport width with unbroken runs (e.g. a provider's JSON error body)
/// hard-broken mid-run. Never truncates — errors are exactly the content
/// the operator must be able to read in full (mu-ka3c: a ~400-char 402
/// provider error rendered truncated and unreadable). Every line carries
/// the error styling.
pub fn error_notice(header: &str, msg: &str, wrap_width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = vec![Line::from("")];
    out.push(Line::from(Span::styled(
        format!("× {header}"),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )));
    // Reserve 2 cols for the "  " indent + 2 cols safety gutter (same
    // budget discipline as block_lines, so ratatui can't re-wrap).
    let inner = wrap_width.saturating_sub(4).max(1);
    for raw in msg.lines() {
        for row in wrap_line(raw, inner) {
            out.push(Line::from(Span::styled(
                format!("  {row}"),
                Style::default().fg(Color::Red),
            )));
        }
    }
    out.push(Line::from(""));
    out
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
    /// Streamed model reasoning (Anthropic extended thinking, ollama
    /// reasoning models). Appended-to in place as deltas arrive; rendered
    /// dimmed to set it apart from the answer prose. (mu-upk2)
    Thinking(String),
    /// A model tool call: `● Name(primary_arg)`.
    ToolCall {
        /// Provider tool-call id — retained so the committed turn carries the
        /// intact typed call, not just a display projection. (mu-upk2)
        tool_call_id: String,
        display_name: String,
        primary_arg: String,
        /// Full structured arguments (lossless). `Null` until the call
        /// finalizes via `session.tool_call_started`. (mu-upk2)
        arguments: serde_json::Value,
        /// Live partial-JSON args accumulated from `session.tool_call_delta`
        /// before finalization; cleared once finalized (empty when the
        /// provider didn't stream tool args). (mu-upk2)
        partial_args: String,
    },
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
    collapse: bool,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(Span::styled(
        format!("┌─ {label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    // When `collapse` is set (fullscreen, mu-5h9m), a completed tool call +
    // its result fold to a single one-line summary (`● Name(arg)  ⎿ ok · N
    // lines`) instead of the call line plus a multi-line result preview — the
    // readable-while-streaming firehose fix. An in-flight tool call (no result
    // item yet) renders normally; text/error items are untouched.
    // Alignment column for collapsed lines: pad each call to the widest call
    // in *this* turn (capped) so the `⎿` summaries line up in a scannable
    // column. Capped at 56 and at `wrap_width - 22` (room for the summary) so a
    // single very-long call can't push the whole column off-screen.
    let call_col = if collapse {
        let mut max_w = 0usize;
        let mut j = 0;
        while j < items.len() {
            if let TurnItem::ToolCall {
                display_name,
                primary_arg,
                ..
            } = &items[j]
            {
                if matches!(items.get(j + 1), Some(TurnItem::ToolResult { .. })) {
                    max_w = max_w.max(tool_call_label(display_name, primary_arg).chars().count());
                    j += 2;
                    continue;
                }
            }
            j += 1;
        }
        max_w.min(56).min(wrap_width.saturating_sub(22)).max(8)
    } else {
        0
    };
    let mut i = 0;
    while i < items.len() {
        if collapse {
            if let TurnItem::ToolCall {
                display_name,
                primary_arg,
                ..
            } = &items[i]
            {
                if let Some(TurnItem::ToolResult { kind, text }) = items.get(i + 1) {
                    out.push(collapsed_tool_line(
                        display_name,
                        primary_arg,
                        kind,
                        text,
                        color,
                        call_col,
                    ));
                    i += 2;
                    continue;
                }
            }
        }
        out.extend(render_turn_item(
            &items[i],
            color,
            wrap_width,
            tool_preview_lines,
        ));
        i += 1;
    }
    out
}

/// The `● Name(arg)` label for a tool call (untruncated). Shared by the
/// collapsed renderer and the per-turn alignment-width pass.
fn tool_call_label(display_name: &str, primary_arg: &str) -> String {
    if primary_arg.is_empty() {
        format!("● {display_name}")
    } else {
        format!("● {display_name}({primary_arg})")
    }
}

/// Render a completed tool call + result as a single collapsed line (style A,
/// mu-5h9m): `│ ● Name(arg)  ⎿ ok · N lines`. Yellow call (matching the
/// expanded form), dim outcome summary, red on error. The call is padded (or
/// truncated with `…`) to `call_col` so the `⎿` summaries align in a column.
fn collapsed_tool_line(
    display_name: &str,
    primary_arg: &str,
    kind: &str,
    text: &str,
    color: Color,
    call_col: usize,
) -> Line<'static> {
    let n = text.lines().count();
    let unit = if n == 1 { "line" } else { "lines" };
    let (summary, summary_style) = match kind {
        "ok" if text.is_empty() => ("ok".to_string(), Style::default().fg(Color::DarkGray)),
        "ok" => (
            format!("ok · {n} {unit}"),
            Style::default().fg(Color::DarkGray),
        ),
        "err" => (
            format!(
                "err · {} {}",
                n.max(1),
                if n <= 1 { "line" } else { "lines" }
            ),
            Style::default().fg(Color::Red),
        ),
        other => (other.to_string(), Style::default().fg(Color::DarkGray)),
    };
    // Pad or truncate the call to the alignment column so every `⎿` lines up.
    let label = tool_call_label(display_name, primary_arg);
    let label_w = label.chars().count();
    let call = if label_w > call_col {
        let short: String = label.chars().take(call_col.saturating_sub(1)).collect();
        format!("{short}…")
    } else {
        format!("{label}{}", " ".repeat(call_col - label_w))
    };
    Line::from(vec![
        Span::styled("│ ", Style::default().fg(color)),
        Span::styled(
            call,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  ⎿ {summary}"), summary_style),
    ])
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
        TurnItem::Thinking(body) => {
            // Reasoning trace, dimmed and prefixed so it reads as the model's
            // private thinking, distinct from the answer. Wrapped like Text.
            let inner = wrap_width.saturating_sub(4).max(1);
            let mut lines = Vec::new();
            let mut first = true;
            for raw in body.lines() {
                for row in wrap_line(raw, inner) {
                    let marker = if first { "✻ " } else { "  " };
                    first = false;
                    lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(color)),
                        Span::styled(
                            format!("{marker}{row}"),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
            }
            lines
        }
        TurnItem::ToolCall {
            display_name,
            primary_arg,
            partial_args,
            ..
        } => {
            let display_name = if display_name.is_empty() {
                "…"
            } else {
                display_name.as_str()
            };
            let header_text = if primary_arg.is_empty() {
                format!("● {display_name}")
            } else {
                // "│ ● Name()" overhead = display_name.len() + 5.
                let max_arg_len = wrap_width.saturating_sub(display_name.len() + 5);
                let arg = if primary_arg.chars().count() > max_arg_len {
                    let short: String = primary_arg
                        .chars()
                        .take(max_arg_len.saturating_sub(1))
                        .collect();
                    format!("{short}…")
                } else {
                    primary_arg.clone()
                };
                format!("● {display_name}({arg})")
            };
            let mut lines = vec![Line::from(vec![
                Span::styled("│ ", Style::default().fg(color)),
                Span::styled(
                    header_text,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ])];
            // While the call is still streaming (no finalized primary_arg yet),
            // show the raw args building up live, dimmed and truncated. (mu-upk2)
            if primary_arg.is_empty() && !partial_args.is_empty() {
                let inner = wrap_width.saturating_sub(6).max(1);
                let preview: String = partial_args.chars().take(inner).collect();
                lines.push(Line::from(vec![
                    Span::styled("│   ", Style::default().fg(color)),
                    Span::styled(preview, Style::default().fg(Color::DarkGray)),
                ]));
            }
            lines
        }
        TurnItem::ToolResult { kind, text } => {
            render_tool_result(kind, text, color, wrap_width, tool_preview_lines)
        }
        TurnItem::Error(msg) => {
            // mu-ka3c: errors must be FULLY readable. Wrap the whole
            // message (hard-breaking unbroken JSON runs) instead of the
            // old truncate-to-one-line, and keep the error styling on
            // every wrapped row. Continuation rows indent under the `×`.
            let body = if msg.is_empty() {
                "(unknown error)"
            } else {
                msg.as_str()
            };
            // "│ " (2) + "× " (2) prefixes + 2 cols safety gutter.
            let inner = wrap_width.saturating_sub(6).max(1);
            let mut lines: Vec<Line<'static>> = Vec::new();
            let mut first = true;
            for raw in body.lines() {
                for row in wrap_line(raw, inner) {
                    let marker = if first { "× " } else { "  " };
                    first = false;
                    lines.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(color)),
                        Span::styled(
                            marker.to_string(),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(row, Style::default().fg(Color::Red)),
                    ]));
                }
            }
            lines
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
            Span::styled(
                "⎿  (no output)".to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ])],
        "ok" => {
            let all: Vec<&str> = text.lines().collect();
            let total = all.len();
            let show = total.min(preview_lines);
            let max_w = wrap_width.saturating_sub(8);
            let mut out: Vec<Line<'static>> = Vec::new();
            for (i, &tl) in all.iter().take(show).enumerate() {
                let truncated: String = tl.chars().take(max_w).collect();
                let prefix = if i == 0 {
                    format!("{indent}⎿  ")
                } else {
                    format!("{indent}   ")
                };
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
            let msg = if text.is_empty() {
                "(unknown error)"
            } else {
                text
            };
            let all: Vec<&str> = msg.lines().collect();
            let total = all.len();
            let show = total.min(preview_lines);
            let max_w = wrap_width.saturating_sub(8).max(1);
            let mut out: Vec<Line<'static>> = Vec::new();
            // mu-ka3c: error lines WRAP (hard-breaking unbroken runs)
            // instead of the ok-arm's char-truncation — a long provider
            // error in a tool result must stay readable. The preview
            // bound still applies to source lines; each shown line may
            // occupy several wrapped rows, all red.
            let mut emitted = 0usize;
            for &tl in all.iter().take(show) {
                for row in wrap_line(tl, max_w) {
                    let prefix = if emitted == 0 {
                        format!("{indent}⎿  ")
                    } else {
                        format!("{indent}   ")
                    };
                    emitted += 1;
                    out.push(Line::from(vec![
                        Span::styled(prefix, bar_style),
                        Span::styled(row, Style::default().fg(Color::Red)),
                    ]));
                }
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
            Span::styled(
                format!("⎿  ({other})"),
                Style::default().fg(Color::DarkGray),
            ),
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

    /// A 400-char unbroken run (a provider's JSON error body has no
    /// spaces) hard-wraps to width-bounded rows with NO truncation —
    /// the mu-ka3c incident shape.
    #[test]
    fn wrap_hard_breaks_unbroken_json_without_loss() {
        let json: String = r#"{"error":{"type":"payment_required","message":"x"},"#
            .chars()
            .cycle()
            .take(400)
            .collect();
        let width = 60;
        let rows = wrap_line(&json, width);
        assert!(rows.len() >= 400 / width, "expected multiple rows");
        for row in &rows {
            assert!(row.chars().count() <= width, "row exceeded width: {row:?}");
        }
        assert_eq!(rows.concat(), json, "wrap must not drop characters");
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
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect()
    }

    /// A finalized ToolCall item. The streaming/lossless fields (id,
    /// arguments, partial_args) don't affect the layout these tests check.
    fn tc(name: &str, arg: &str) -> TurnItem {
        TurnItem::ToolCall {
            tool_call_id: format!("toolu_{name}"),
            display_name: name.into(),
            primary_arg: arg.into(),
            arguments: serde_json::Value::Null,
            partial_args: String::new(),
        }
    }

    #[test]
    fn render_turn_header_then_items_no_closer() {
        let items = vec![TurnItem::Text("hello".into())];
        let lines = render_turn("assistant", Color::White, &items, 80, 4, false);
        let p = plain(&lines);
        assert_eq!(p[0], "┌─ assistant ");
        assert_eq!(p[1], "│ hello");
        // No closer in the body render.
        assert!(!p.iter().any(|l| l.starts_with("└─")));
    }

    #[test]
    fn collapse_folds_completed_tool_to_one_line() {
        // ToolCall + ToolResult → a single `● Name(arg)  ⎿ ok · N lines` line.
        let items = vec![
            tc("Bash", "cat f"),
            TurnItem::ToolResult {
                kind: "ok".into(),
                text: "a\nb\nc".into(),
            },
        ];
        let p = plain(&render_turn("assistant", Color::White, &items, 80, 4, true));
        // header + exactly one folded line, no multi-line ⎿ preview.
        assert_eq!(p.len(), 2, "expected header + 1 folded line, got {p:?}");
        assert_eq!(p[1], "│ ● Bash(cat f)  ⎿ ok · 3 lines");
    }

    #[test]
    fn collapse_leaves_inflight_call_expanded() {
        // A ToolCall with no following ToolResult (in-flight) is not folded.
        let items = vec![tc("Bash", "sleep 1")];
        let p = plain(&render_turn("assistant", Color::White, &items, 80, 4, true));
        assert_eq!(p[1], "│ ● Bash(sleep 1)");
    }

    #[test]
    fn collapse_err_result_summary() {
        let items = vec![
            tc("Bash", "false"),
            TurnItem::ToolResult {
                kind: "err".into(),
                text: "boom".into(),
            },
        ];
        let p = plain(&render_turn("assistant", Color::White, &items, 80, 4, true));
        assert_eq!(p[1], "│ ● Bash(false)  ⎿ err · 1 line");
    }

    #[test]
    fn collapse_aligns_summaries_in_a_column() {
        // Two collapsed calls of different widths → the shorter is padded so
        // both `⎿` summaries start at the same column.
        let items = vec![
            tc("Bash", "ls"),
            TurnItem::ToolResult {
                kind: "ok".into(),
                text: "x".into(),
            },
            tc("Read", "a/longer/path.rs"),
            TurnItem::ToolResult {
                kind: "ok".into(),
                text: "y\nz".into(),
            },
        ];
        let p = plain(&render_turn("assistant", Color::White, &items, 80, 4, true));
        assert_eq!(p.len(), 3, "header + 2 folded lines, got {p:?}");
        let c0 = p[1].chars().position(|c| c == '⎿');
        let c1 = p[2].chars().position(|c| c == '⎿');
        assert_eq!(c0, c1, "⎿ summaries should align: {:?}", &p[1..]);
    }

    #[test]
    fn render_tool_call_header_format() {
        let item = tc("Bash", "ls -la");
        let lines = render_turn_item(&item, Color::White, 80, 4);
        assert_eq!(plain(&lines)[0], "│ ● Bash(ls -la)");
    }

    #[test]
    fn render_thinking_item_is_marked_and_wrapped() {
        let item = TurnItem::Thinking("let me reason\nabout this".into());
        let p = plain(&render_turn_item(&item, Color::White, 80, 4));
        assert_eq!(p[0], "│ ✻ let me reason");
        assert_eq!(p[1], "│   about this");
    }

    #[test]
    fn render_streaming_tool_call_shows_partial_args() {
        // No finalized primary_arg yet, but args are streaming in: show the
        // name header plus a dim partial-args preview line. (mu-upk2)
        let item = TurnItem::ToolCall {
            tool_call_id: "toolu_1".into(),
            display_name: "Read".into(),
            primary_arg: String::new(),
            arguments: serde_json::Value::Null,
            partial_args: "{\"path\":\"Cargo".into(),
        };
        let p = plain(&render_turn_item(&item, Color::White, 80, 4));
        assert_eq!(p[0], "│ ● Read");
        assert_eq!(p[1], "│   {\"path\":\"Cargo");
    }

    #[test]
    fn render_tool_result_ok_bounds_and_overflow_marker() {
        let body = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_turn_item(
            &TurnItem::ToolResult {
                kind: "ok".into(),
                text: body,
            },
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
            &TurnItem::ToolResult {
                kind: "ok".into(),
                text: String::new(),
            },
            Color::White,
            80,
            4,
        );
        assert!(plain(&lines)[0].contains("(no output)"));
    }

    // ---- error wrapping (mu-ka3c) ----

    /// Recover the body text of an error row: everything after the
    /// "│ " bar and the 2-col marker column.
    fn error_row_body(line: &Line<'static>) -> String {
        let full = line
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect::<String>();
        full.chars().skip(4).collect()
    }

    #[test]
    fn turn_item_error_wraps_400_char_json_no_truncation() {
        let json: String = r#"{"type":"error","error":{"type":"payment_required""#
            .chars()
            .cycle()
            .take(400)
            .collect();
        let wrap_width = 80;
        let lines = render_turn_item(&TurnItem::Error(json.clone()), Color::White, wrap_width, 4);
        assert!(
            lines.len() > 1,
            "400 chars at width 80 must wrap to multiple rows"
        );
        let mut reassembled = String::new();
        for line in &lines {
            let row = error_row_body(line);
            assert!(
                4 + row.chars().count() <= wrap_width,
                "row exceeded viewport width: {row:?}"
            );
            reassembled.push_str(&row);
        }
        assert_eq!(reassembled, json, "error body must not be truncated");
    }

    #[test]
    fn turn_item_error_styles_every_wrapped_line_red() {
        let msg = "x".repeat(300);
        let lines = render_turn_item(&TurnItem::Error(msg), Color::White, 60, 4);
        assert!(lines.len() > 1);
        for line in &lines {
            // spans: [bar, marker, body] — marker and body must both be red.
            assert_eq!(line.spans.len(), 3, "expected bar+marker+body spans");
            assert_eq!(line.spans[1].style.fg, Some(Color::Red));
            assert_eq!(line.spans[2].style.fg, Some(Color::Red));
        }
        // First row carries the `× ` marker; continuations indent under it.
        assert_eq!(lines[0].spans[1].content.as_ref(), "× ");
        assert_eq!(lines[1].spans[1].content.as_ref(), "  ");
    }

    #[test]
    fn error_notice_wraps_full_message_and_styles_every_line() {
        let msg: String = "abcdefgh".chars().cycle().take(400).collect();
        let wrap_width = 78;
        let lines = error_notice("turn ended with error", &msg, wrap_width);
        let p = plain(&lines);
        assert_eq!(p[0], "");
        assert_eq!(p[1], "× turn ended with error");
        assert_eq!(p.last().unwrap(), "");
        // Body rows: indented, width-bounded, nothing lost.
        let body: String = p[2..p.len() - 1]
            .iter()
            .map(|l| l.trim_start().to_string())
            .collect();
        assert_eq!(body, msg, "error_notice must not truncate the message");
        for l in &p[2..p.len() - 1] {
            assert!(l.chars().count() <= wrap_width, "row exceeded width: {l:?}");
        }
        // Every non-blank line is styled red.
        for line in lines
            .iter()
            .filter(|l| !plain(&[(*l).clone()])[0].is_empty())
        {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(Color::Red), "non-red span: {span:?}");
            }
        }
    }

    #[test]
    fn tool_result_err_wraps_long_line_instead_of_truncating() {
        let long = "e".repeat(200);
        let wrap_width = 80;
        let lines = render_turn_item(
            &TurnItem::ToolResult {
                kind: "err".into(),
                text: long.clone(),
            },
            Color::White,
            wrap_width,
            4,
        );
        assert!(lines.len() > 1, "200-char err line must wrap, not truncate");
        let reassembled: String = lines
            .iter()
            .map(|l| l.spans.last().unwrap().content.to_string())
            .collect();
        assert_eq!(reassembled, long, "err preview must keep the whole line");
        for line in &lines {
            assert_eq!(line.spans.last().unwrap().style.fg, Some(Color::Red));
        }
    }

    #[test]
    fn turn_closer_is_bar_and_blank() {
        let lines = turn_closer(Color::White);
        let p = plain(&lines);
        assert_eq!(p, vec!["└─".to_string(), "".to_string()]);
    }

    #[test]
    fn tail_truncate_passthrough_when_short() {
        let full = render_turn(
            "assistant",
            Color::White,
            &[TurnItem::Text("a\nb".into())],
            80,
            4,
            false,
        );
        let truncated = tail_truncate(full.clone(), 50);
        assert_eq!(plain(&full), plain(&truncated));
    }

    #[test]
    fn tail_truncate_keeps_tail_with_marker() {
        let body = (1..=20)
            .map(|n| format!("row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let full = render_turn(
            "assistant",
            Color::White,
            &[TurnItem::Text(body)],
            80,
            4,
            false,
        );
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
