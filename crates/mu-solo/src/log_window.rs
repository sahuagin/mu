//! Log window — alternate-screen pager over the committed transcript
//! (mu-d04a slice 1, bead mu-d04a.2).
//!
//! The first real alt-screen surface in mu-solo. Enter the alternate screen,
//! render a formatted scrollable projection, leave — the terminal restores
//! the main screen by contract, so the inline surface is never repainted and
//! no artifact class exists at the boundary (contrast: the F3 dump-on-flip
//! replay). Openable MID-TURN: while the pager runs, the main select loop is
//! paused inside the key handler (the established ctrl-s editor-handoff
//! blocking pattern), daemon messages queue in the client channel, and the
//! backlog drains through the normal loop on close. Tool execution is
//! daemon-side and unaffected; only display and input_required responses
//! wait.
//!
//! Projection principle (mu-d04a): the pager re-renders from the transcript
//! at ITS OWN width, at open and on every resize — it never reuses another
//! surface's layout.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::cursor::MoveTo;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{event, execute, queue};
use ratatui::style::Modifier;
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use crate::viewport::to_crossterm_color;

/// Rows reserved for the title (top) and position footer (bottom).
const CHROME_ROWS: u16 = 2;

/// Run the pager until the user closes it (Esc / q / ctrl-o / ctrl-c).
///
/// `render` produces the projection at a given wrap width; it is called at
/// open and again on terminal resize.
pub fn run<F>(render: F) -> io::Result<()>
where
    F: Fn(usize) -> Vec<Line<'static>>,
{
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let result = pager_loop(&mut out, &render);
    // Leave unconditionally — a render error must not strand the terminal
    // in the alternate screen.
    let leave = execute!(out, LeaveAlternateScreen);
    result.and(leave)
}

fn pager_loop<W: Write, F>(out: &mut W, render: &F) -> io::Result<()>
where
    F: Fn(usize) -> Vec<Line<'static>>,
{
    let (mut cols, mut rows) = terminal::size()?;
    let mut lines = render(wrap_width_for(cols));
    // Open at the bottom: the most recent turn is what a mid-run reader
    // wants first.
    let mut scroll = max_scroll(lines.len(), visible_rows(rows));

    loop {
        draw(out, &lines, scroll, cols, rows)?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let page = visible_rows(rows).max(1);
                let bottom = max_scroll(lines.len(), visible_rows(rows));
                match (k.modifiers, k.code) {
                    (_, KeyCode::Esc) | (_, KeyCode::Char('q')) => break,
                    (KeyModifiers::CONTROL, KeyCode::Char('o')) => break,
                    (KeyModifiers::CONTROL, KeyCode::Char('c')) => break,
                    (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
                        scroll = scroll.saturating_sub(1);
                    }
                    (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
                        scroll = (scroll + 1).min(bottom);
                    }
                    (_, KeyCode::PageUp) => scroll = scroll.saturating_sub(page),
                    (_, KeyCode::PageDown) | (_, KeyCode::Char(' ')) => {
                        scroll = (scroll + page).min(bottom);
                    }
                    (_, KeyCode::Home) | (_, KeyCode::Char('g')) => scroll = 0,
                    (_, KeyCode::End) | (_, KeyCode::Char('G')) => scroll = bottom,
                    _ => {}
                }
            }
            Event::Resize(c, r) => {
                cols = c;
                rows = r;
                lines = render(wrap_width_for(cols));
                scroll = scroll.min(max_scroll(lines.len(), visible_rows(rows)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Content wrap width for a terminal `cols` wide: match the inline
/// surface's convention (2-column gutter margin).
pub(crate) fn wrap_width_for(cols: u16) -> usize {
    (cols as usize).saturating_sub(2).max(10)
}

/// Content rows available between title and footer.
pub(crate) fn visible_rows(rows: u16) -> usize {
    rows.saturating_sub(CHROME_ROWS).max(1) as usize
}

/// The scroll offset that puts the last line on the last content row.
pub(crate) fn max_scroll(total_lines: usize, visible: usize) -> usize {
    total_lines.saturating_sub(visible)
}

fn draw<W: Write>(
    out: &mut W,
    lines: &[Line<'static>],
    scroll: usize,
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    let visible = visible_rows(rows);
    let end = (scroll + visible).min(lines.len());

    // Title.
    queue!(out, MoveTo(0, 0), Clear(ClearType::CurrentLine))?;
    queue!(
        out,
        SetAttribute(Attribute::Bold),
        Print(" log "),
        SetAttribute(Attribute::Reset),
        SetAttribute(Attribute::Dim),
        Print(format!(
            "· {} lines · ↑/↓ PgUp/PgDn Home/End scroll · q/Esc/ctrl-o close",
            lines.len()
        )),
        SetAttribute(Attribute::Reset),
    )?;

    // Content.
    for (row, idx) in (scroll..end).enumerate() {
        queue!(
            out,
            MoveTo(0, row as u16 + 1),
            Clear(ClearType::CurrentLine)
        )?;
        print_line(out, &lines[idx], cols as usize)?;
    }
    // Blank any unused rows (short transcript).
    for row in (end - scroll)..visible {
        queue!(
            out,
            MoveTo(0, row as u16 + 1),
            Clear(ClearType::CurrentLine)
        )?;
    }

    // Footer.
    let pct = if lines.len() <= visible {
        100
    } else {
        (end * 100) / lines.len()
    };
    queue!(
        out,
        MoveTo(0, rows.saturating_sub(1)),
        Clear(ClearType::CurrentLine)
    )?;
    queue!(
        out,
        SetAttribute(Attribute::Dim),
        Print(format!(
            " {}–{} of {} ({pct}%)",
            scroll.saturating_add(1).min(lines.len()),
            end,
            lines.len()
        )),
        SetAttribute(Attribute::Reset),
    )?;
    out.flush()
}

/// Write one styled ratatui `Line`, hard-clipped at `max_cols` chars.
/// Lines arrive pre-wrapped at the pager's own wrap width, so the clip is a
/// safety net, not the layout mechanism.
fn print_line<W: Write>(out: &mut W, line: &Line<'static>, max_cols: usize) -> io::Result<()> {
    let mut printed = 0usize;
    for span in &line.spans {
        if printed >= max_cols {
            break;
        }
        let style = span.style;
        if let Some(fg) = style.fg {
            queue!(out, SetForegroundColor(to_crossterm_color(fg)))?;
        }
        if let Some(bg) = style.bg {
            queue!(out, SetBackgroundColor(to_crossterm_color(bg)))?;
        }
        let mods = style.add_modifier;
        if mods.contains(Modifier::BOLD) {
            queue!(out, SetAttribute(Attribute::Bold))?;
        }
        if mods.contains(Modifier::DIM) {
            queue!(out, SetAttribute(Attribute::Dim))?;
        }
        if mods.contains(Modifier::ITALIC) {
            queue!(out, SetAttribute(Attribute::Italic))?;
        }
        if mods.contains(Modifier::UNDERLINED) {
            queue!(out, SetAttribute(Attribute::Underlined))?;
        }
        if mods.contains(Modifier::REVERSED) {
            queue!(out, SetAttribute(Attribute::Reverse))?;
        }
        // Clip by DISPLAY width, not char count — a CJK/wide glyph
        // occupies two columns (panel finding, 2026-08-03).
        let mut content = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if printed + w > max_cols {
                break;
            }
            printed += w;
            content.push(ch);
        }
        queue!(out, Print(content), SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    #[test]
    fn max_scroll_zero_when_content_fits() {
        assert_eq!(max_scroll(10, 20), 0);
    }

    #[test]
    fn max_scroll_bottom_anchors_long_content() {
        // 100 lines, 22 visible → last window starts at 78.
        assert_eq!(max_scroll(100, 22), 78);
    }

    #[test]
    fn visible_rows_reserves_chrome() {
        assert_eq!(visible_rows(24), 22);
        // Degenerate terminal still gets one content row.
        assert_eq!(visible_rows(2), 1);
        assert_eq!(visible_rows(0), 1);
    }

    #[test]
    fn wrap_width_matches_inline_gutter_convention() {
        assert_eq!(wrap_width_for(80), 78);
        // Floor for absurdly narrow terminals.
        assert_eq!(wrap_width_for(5), 10);
    }

    #[test]
    fn print_line_clips_at_budget_and_resets_style() {
        let mut out: Vec<u8> = Vec::new();
        let line = Line::from(vec![
            Span::styled("abcdef", Style::default().fg(Color::Red)),
            Span::raw("ghijkl"),
        ]);
        print_line(&mut out, &line, 8).unwrap();
        let s = String::from_utf8(out).unwrap();
        // 6 chars of first span + 2 of second; the rest clipped.
        assert!(s.contains("abcdef"));
        assert!(s.contains("gh"));
        assert!(!s.contains("ghi"));
        // Style reset emitted after each span.
        assert!(s.contains("\x1b[0m"));
    }

    #[test]
    fn print_line_clips_by_display_width_not_chars() {
        // Four CJK glyphs = 8 columns. Budget 5 fits two glyphs (4 cols);
        // the third would overflow to 6 and must be clipped.
        let mut out: Vec<u8> = Vec::new();
        print_line(&mut out, &Line::from("日本語字"), 5).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("日本"));
        assert!(!s.contains("語"));
    }

    #[test]
    fn print_line_zero_budget_prints_nothing() {
        let mut out: Vec<u8> = Vec::new();
        print_line(&mut out, &Line::from("hello"), 0).unwrap();
        assert!(String::from_utf8(out).unwrap().is_empty());
    }
}
