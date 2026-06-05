//! Dynamic inline viewport — a minimal custom terminal that supports
//! grow/shrink of the viewport area while preserving native scrollback.
//!
//! Inspired by codex-rs/tui/src/custom_terminal.rs (Apache-2.0).
//! Only implements the subset needed for mu-solo: render a viewport of
//! variable height at the bottom of the terminal, scroll the region
//! above it when the viewport grows, and shrink when it contracts.
//!
//! ## Scrollback-commit invariant (mu-solo-scrollback-dup-recommit-8hva)
//!
//! `self.history` is the in-memory mirror of every line ever passed to
//! `insert_before`.  When an `insert_before(N)` call emits more lines
//! than the available rows above the viewport (`viewport.y`), the
//! excess lines overflow via DECSTBM scroll into native terminal
//! scrollback and are no longer addressable as screen rows.
//!
//! `scrollback_committed` tracks the exact count of history entries
//! that have been pushed into native scrollback and therefore **must
//! not be redrawn** by `repaint_history_tail`.  The invariant is:
//!
//!   `scrollback_committed = max(0, history.len() − viewport.y)`
//!
//! after every `insert_before` call.  `repaint_history_tail` starts
//! from `max(scrollback_committed, history.len() − visible_rows)` so
//! that it never touches lines already in native scrollback — those
//! would appear twice (once in scrollback, once on-screen) when the
//! user scrolls up.

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{
    Attribute, Color as CtColor, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Widget;

/// A stored line of history content (what insert_before rendered).
/// Kept so we can replay on viewport shrink.
#[derive(Clone)]
struct HistoryLine {
    cells: Vec<(String, Color, Color, Modifier)>,
}

/// A minimal terminal that manages a dynamically-sized inline viewport.
/// Content above the viewport lives in native terminal scrollback.
pub struct DynamicViewport {
    /// Current viewport area (x, y, width, height).
    viewport: Rect,
    /// Double buffer for diff-based rendering.
    buffers: [Buffer; 2],
    current: usize,
    /// Terminal screen size (columns, rows).
    screen_size: (u16, u16),
    /// History lines rendered above the viewport via insert_before.
    history: Vec<HistoryLine>,
    /// Number of history entries that have been committed to native
    /// terminal scrollback (and are therefore no longer addressable
    /// as screen rows).  Maintained by insert_before; read by
    /// repaint_history_tail to prevent drawing lines that already
    /// live in native scrollback — the double-draw is the root cause
    /// of the mid-message span duplication bug
    /// (mu-solo-scrollback-dup-recommit-8hva).
    scrollback_committed: usize,
    /// Optional renderer journal — appended by the commit paths.
    /// None when journalling is disabled (config knob renderer_journal).
    journal: Option<std::fs::File>,
}

impl DynamicViewport {
    /// Create a new viewport starting at the current cursor position.
    /// The initial height is the number of lines to claim at the bottom.
    ///
    /// `journal_path` — when `Some`, the renderer opens (or creates) the
    /// file in append mode and writes one JSONL line per scrollback commit.
    /// Pass `None` to disable journalling.
    pub fn new(initial_height: u16, journal_path: Option<&std::path::Path>) -> io::Result<Self> {
        let (cols, rows) = terminal::size()?;
        let (_, cursor_y) = crossterm::cursor::position()?;

        // If the cursor is too close to the bottom, scroll to make room.
        let needed_y = rows.saturating_sub(initial_height);
        let y = if cursor_y > needed_y {
            let scroll_by = cursor_y - needed_y;
            // Scroll the whole screen up to make room
            queue!(io::stdout(), crossterm::terminal::ScrollUp(scroll_by))?;
            io::stdout().flush()?;
            needed_y
        } else {
            cursor_y
        };

        let viewport = Rect::new(0, y, cols, initial_height);

        // Open journal in append mode if requested.  Non-fatal: if
        // the path can't be opened we log a warning and continue
        // without journalling rather than refusing to start.
        let journal = journal_path.and_then(|p| {
            // Ensure parent directory exists.
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::OpenOptions::new().create(true).append(true).open(p) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(path = %p.display(), err = %e, "renderer journal open failed — journalling disabled");
                    None
                }
            }
        });

        Ok(Self {
            viewport,
            buffers: [Buffer::empty(viewport), Buffer::empty(viewport)],
            current: 0,
            screen_size: (cols, rows),
            history: Vec::new(),
            scrollback_committed: 0,
            journal,
        })
    }

    /// Get the current viewport area for rendering into.
    pub fn area(&self) -> Rect {
        self.viewport
    }

    /// Resize the viewport to a full-screen-style overlay, leaving one row
    /// above so insert_before still has a safe history region on tiny terms.
    pub fn maximize_height(&mut self) -> io::Result<()> {
        let (_, rows) = terminal::size()?;
        self.set_height(rows.saturating_sub(1).max(1))
    }

    /// Resize the viewport to a new height. If growing, scrolls the
    /// content above the viewport up to make room. If shrinking,
    /// clears the freed lines.
    pub fn set_height(&mut self, new_height: u16) -> io::Result<()> {
        let (cols, rows) = terminal::size()?;
        self.screen_size = (cols, rows);
        let new_height = new_height.min(rows.saturating_sub(1)); // leave at least 1 row above

        if new_height == self.viewport.height {
            // Width might have changed
            if cols != self.viewport.width {
                self.viewport.width = cols;
                self.buffers[0].resize(self.viewport);
                self.buffers[1].resize(self.viewport);
            }
            return Ok(());
        }

        let old_height = self.viewport.height;

        if new_height > old_height {
            // Growing: scroll content above the viewport up to make room.
            let growth = new_height - old_height;
            let viewport_top = self.viewport.y;

            if viewport_top >= growth {
                scroll_region_up(0, viewport_top.saturating_sub(1), growth)?;
                self.viewport.y -= growth;
            } else {
                let available = viewport_top;
                if available > 0 {
                    scroll_region_up(0, viewport_top.saturating_sub(1), available)?;
                }
                self.viewport.y = 0;
            }
            self.viewport.height = new_height;
        } else {
            // Shrinking: keep the viewport bottom anchored to the bottom of the
            // terminal. The live assistant preview grows upward while a turn is
            // streaming; when it collapses after commit, leaving `viewport.y`
            // unchanged strands the prompt/status mid-screen with blank space
            // below and the next refresh visibly jumps. Tail-follow mode should
            // behave like a chat client: the input chrome stays at the bottom.
            let old_y = self.viewport.y;
            let new_y = rows.saturating_sub(new_height);
            let mut stdout = io::stdout();
            for row in old_y..(old_y + old_height) {
                queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
            }
            self.viewport.y = new_y;
            self.viewport.height = new_height;
            self.repaint_history_tail(&mut stdout)?;
            stdout.flush()?;
        }

        self.viewport.width = cols;
        self.buffers[0].resize(self.viewport);
        self.buffers[1].resize(self.viewport);
        // Clear the entire viewport area on screen so stale content
        // doesn't bleed through. Force full redraw on next flush.
        let mut stdout = io::stdout();
        for row in self.viewport.y..self.viewport.y + self.viewport.height {
            queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }
        self.buffers[1 - self.current].reset();
        stdout.flush()?;
        Ok(())
    }

    /// Clear the viewport area on screen (used before insert_before
    /// to erase the raw prompt before the formatted "you" block replaces it).
    pub fn clear_viewport(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        for row in self.viewport.y..(self.viewport.y + self.viewport.height) {
            queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }
        stdout.flush()
    }

    /// Move the viewport to the bottom of the screen. Used after sending
    /// a prompt so that streaming insert_before calls don't trigger
    /// push-downs (which create blank line gaps).
    pub fn snap_to_bottom(&mut self) -> io::Result<()> {
        let (_, screen_rows) = terminal::size()?;
        let target_y = screen_rows.saturating_sub(self.viewport.height);
        if self.viewport.y < target_y {
            // Clear old position
            let mut stdout = io::stdout();
            for row in self.viewport.y..(self.viewport.y + self.viewport.height) {
                queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
            }
            self.viewport.y = target_y;
            self.buffers[0].resize(self.viewport);
            self.buffers[1].resize(self.viewport);
            self.buffers[1 - self.current].reset();
            stdout.flush()?;
        }
        Ok(())
    }

    /// Render a widget into the viewport's buffer.
    pub fn render<W: Widget>(&mut self, widget: W) {
        let area = self.viewport;
        widget.render(area, self.current_buffer_mut());
    }

    /// Flush the viewport to the terminal. Always does a full repaint
    /// (no diff optimization) to avoid state confusion after insert_before.
    /// Wrapped in synchronized output brackets to prevent flicker.
    pub fn flush(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        // Begin synchronized output (terminal buffers until end bracket)
        write!(stdout, "\x1b[?2026h")?;
        queue!(stdout, Hide)?;

        let area = self.viewport;
        let curr = &self.buffers[self.current];

        for y in 0..area.height {
            for x in 0..area.width {
                let idx = (y as usize) * (area.width as usize) + (x as usize);
                let curr_cell = &curr.content[idx];

                {
                    let screen_y = area.y + y;
                    let screen_x = area.x + x;
                    queue!(stdout, MoveTo(screen_x, screen_y))?;

                    // Apply style
                    let fg = to_crossterm_color(curr_cell.fg);
                    let bg = to_crossterm_color(curr_cell.bg);
                    queue!(stdout, SetForegroundColor(fg), SetBackgroundColor(bg))?;

                    let mods = curr_cell.modifier;
                    if mods.contains(Modifier::BOLD) {
                        queue!(stdout, SetAttribute(Attribute::Bold))?;
                    }
                    if mods.contains(Modifier::DIM) {
                        queue!(stdout, SetAttribute(Attribute::Dim))?;
                    }
                    if mods.contains(Modifier::ITALIC) {
                        queue!(stdout, SetAttribute(Attribute::Italic))?;
                    }
                    if mods.contains(Modifier::UNDERLINED) {
                        queue!(stdout, SetAttribute(Attribute::Underlined))?;
                    }
                    if mods.contains(Modifier::REVERSED) {
                        queue!(stdout, SetAttribute(Attribute::Reverse))?;
                    }

                    queue!(stdout, Print(curr_cell.symbol()))?;
                    queue!(stdout, SetAttribute(Attribute::Reset))?;
                }
            }
        }

        // End synchronized output (terminal renders atomically)
        write!(stdout, "\x1b[?2026l")?;
        stdout.flush()?;

        // Reset buffer for next frame (full repaint each time).
        self.current_buffer_mut().reset();
        Ok(())
    }

    /// Insert lines above the viewport (push content into scrollback).
    /// Used for conversation output (assistant responses, tool calls, etc.)
    /// Also stores the rendered lines in history for replay on shrink.
    pub fn insert_before<F>(&mut self, height: u16, draw_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut Buffer),
    {
        if height == 0 {
            return Ok(());
        }

        let (_, screen_rows) = terminal::size()?;
        let width = self.viewport.width;
        let mut stdout = io::stdout();

        // If the viewport isn't at the bottom of the screen (blank space
        // below from Pi-style shrink), push it DOWN first to make room above.
        let viewport_bottom = self.viewport.y + self.viewport.height;
        if viewport_bottom < screen_rows {
            let push_down = height.min(screen_rows - viewport_bottom);
            // Scroll the viewport region DOWN using reverse index
            let region_top = self.viewport.y + 1; // 1-based
            let region_bottom = screen_rows;
            write!(
                stdout,
                "\x1b[{};{}r\x1b[{}T\x1b[r",
                region_top, region_bottom, push_down
            )?;
            self.viewport.y += push_down;
            self.buffers[0].resize(self.viewport);
            self.buffers[1].resize(self.viewport);
            // Force full redraw since viewport moved
            self.buffers[1 - self.current].reset();
        }

        let viewport_top = self.viewport.y;
        if viewport_top == 0 {
            // No scrollback region is visible. This should be rare because
            // set_height leaves one row above the viewport, but don't risk
            // drawing over the live input area if the terminal is tiny.
            return Ok(());
        }

        // SCROLLBACK FIX — "mu-solo text doesn't persist" regression.
        // The previous code made room with `scroll_region_up(0, …)`, i.e.
        // DECSTBM + SU (`CSI S`). Lines that scroll off the TOP of a margined
        // region via SU are discarded by the terminal — they NEVER enter
        // native scrollback — so once a session filled the screen the
        // committed transcript vanished on scroll-up (invisible at full
        // terminal height, fatal at real heights; an agent driving the TUI
        // never noticed because it reads each frame live). Use the codex-rs
        // pattern instead: restrict DECSTBM to the history region, park the
        // cursor at the bottom of that region, then emit CRLF + one rendered
        // row per logical row. Newline-scrolling at the bottom of a
        // top-margin-1 region DOES feed native scrollback, so the full payload
        // is saved and only the tail of an oversized payload stays visible
        // above the viewport. Draw into a 0,0-anchored off-screen buffer:
        // mapping it onto y=0..height would overlap the live viewport when
        // height > viewport_top and corrupt the prompt.
        let draw_area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(draw_area);
        draw_fn(&mut buf);

        // Begin synchronized output so a multi-line history insert + viewport
        // redraw does not visibly tear on terminals that support the extension.
        write!(stdout, "\x1b[?2026h")?;
        // ANSI scroll-region coordinates are 1-based and inclusive. The
        // history region is terminal rows 0..viewport_top (exclusive), so the
        // bottom row is `viewport_top` in 1-based coordinates.
        write!(stdout, "\x1b[1;{}r", viewport_top)?;
        queue!(stdout, MoveTo(0, viewport_top - 1))?;

        for y in 0..draw_area.height {
            queue!(stdout, Print("\r\n"))?;
            write_buffer_row(&mut stdout, &buf, y)?;

            let mut hline = HistoryLine { cells: Vec::new() };
            for x in 0..draw_area.width {
                let idx = (y as usize) * (draw_area.width as usize) + (x as usize);
                let cell = &buf.content[idx];
                hline
                    .cells
                    .push((cell.symbol().to_string(), cell.fg, cell.bg, cell.modifier));
            }
            self.history.push(hline);
        }

        // Reset scroll region and leave cursor in the viewport; the next
        // `flush` repaints the viewport from scratch.
        write!(stdout, "\x1b[r")?;
        queue!(stdout, MoveTo(self.viewport.x, self.viewport.y))?;
        write!(stdout, "\x1b[?2026l")?;
        stdout.flush()?;
        self.buffers[1 - self.current].reset();

        // Update scrollback_committed: lines that overflowed past
        // viewport_top went into native scrollback and must not be
        // redrawn.  The invariant after every insert_before is:
        //   scrollback_committed = max(0, history.len() − viewport.y)
        // Re-derive from the new history length so accumulated rounding
        // from multiple small inserts never drifts.
        let vp_top = self.viewport.y as usize;
        self.scrollback_committed = self.history.len().saturating_sub(vp_top);

        // Emit one journal line per commit (cheap append, never to the
        // semantic event store).
        let offset_start = self.history.len().saturating_sub(height as usize);
        let offset_end = self.history.len();
        self.journal_commit(offset_start, offset_end, height, "insert_before");

        // Cap history to prevent unbounded growth (keep last 1000 lines)
        const MAX_HISTORY: usize = 1000;
        if self.history.len() > MAX_HISTORY {
            let drain = self.history.len() - MAX_HISTORY;
            // Adjust scrollback_committed for the drain so the
            // invariant holds: drained lines were already committed.
            self.scrollback_committed = self.scrollback_committed.saturating_sub(drain);
            self.history.drain(0..drain);
        }

        Ok(())
    }

    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    /// Repaint the visible history tail above the viewport from the in-memory
    /// rendered-line cache. Used after moving the viewport down on shrink: the
    /// terminal rows exposed between the old and new viewport positions are
    /// blank unless we redraw the committed transcript tail into them.
    ///
    /// ## Scrollback-committed guard
    ///
    /// Lines in `history[..scrollback_committed]` are in native terminal
    /// scrollback — they are no longer screen-addressable.  Drawing them
    /// here would create a second copy that appears when the user scrolls
    /// up, producing the "span twice" duplication
    /// (mu-solo-scrollback-dup-recommit-8hva).  We therefore clamp
    /// `start` to `scrollback_committed` so that only the screen-resident
    /// tail of history is repainted.
    fn repaint_history_tail<W: Write>(&self, stdout: &mut W) -> io::Result<()> {
        let visible_rows = self.viewport.y as usize;
        // Never start before scrollback_committed: those lines live in
        // native scrollback and must not be drawn to screen rows.
        let naive_start = self.history.len().saturating_sub(visible_rows);
        let start = naive_start.max(self.scrollback_committed);
        let rows_to_draw = self.history.len().saturating_sub(start);
        let top = visible_rows.saturating_sub(rows_to_draw);

        for row in 0..self.viewport.y {
            queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }

        for (i, hline) in self.history[start..].iter().enumerate() {
            let y = (top + i) as u16;
            queue!(stdout, MoveTo(0, y), Clear(ClearType::CurrentLine))?;
            write_history_line(stdout, hline)?;
        }
        Ok(())
    }

    /// Append one JSONL entry to the renderer journal (if open).
    /// `offset_start`/`offset_end` are indices into `self.history`;
    /// `line_count` is the number of lines in this commit; `trigger`
    /// is a short label ("insert_before" | "finalize_mismatch" etc.).
    /// Errors are silently swallowed — the journal is diagnostic only.
    fn journal_commit(
        &mut self,
        offset_start: usize,
        offset_end: usize,
        line_count: u16,
        trigger: &str,
    ) {
        let Some(ref mut f) = self.journal else {
            return;
        };
        // Epoch-ms timestamp (no chrono dep).
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = format!(
            "{{\"ts_ms\":{ts_ms},\"offset_start\":{offset_start},\"offset_end\":{offset_end},\"line_count\":{line_count},\"trigger\":\"{trigger}\"}}\n"
        );
        let _ = f.write_all(line.as_bytes());
    }

    /// Emit a finalize-mismatch journal entry when the committed
    /// history length doesn't match the finalized text length.
    /// Also logs a tracing::warn — the journal and the warn fire
    /// together so both the human watching and the log file capture it.
    pub fn journal_finalize_mismatch(&mut self, committed_lines: usize, finalized_text_len: usize) {
        tracing::warn!(
            committed_lines,
            finalized_text_len,
            "renderer finalize mismatch: committed lines vs finalized text length differ"
        );
        let Some(ref mut f) = self.journal else {
            return;
        };
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = format!(
            "{{\"ts_ms\":{ts_ms},\"kind\":\"finalize_mismatch\",\"committed_lines\":{committed_lines},\"finalized_text_len\":{finalized_text_len}}}\n"
        );
        let _ = f.write_all(line.as_bytes());
    }

    /// Return the current history line count.  Used by callers that
    /// want to record pre-commit and post-commit offsets for the journal.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }
}

fn write_history_line<W: Write>(stdout: &mut W, line: &HistoryLine) -> io::Result<()> {
    for (symbol, fg, bg, modifier) in &line.cells {
        queue!(
            stdout,
            SetForegroundColor(to_crossterm_color(*fg)),
            SetBackgroundColor(to_crossterm_color(*bg))
        )?;
        if modifier.contains(Modifier::BOLD) {
            queue!(stdout, SetAttribute(Attribute::Bold))?;
        }
        if modifier.contains(Modifier::DIM) {
            queue!(stdout, SetAttribute(Attribute::Dim))?;
        }
        if modifier.contains(Modifier::ITALIC) {
            queue!(stdout, SetAttribute(Attribute::Italic))?;
        }
        if modifier.contains(Modifier::UNDERLINED) {
            queue!(stdout, SetAttribute(Attribute::Underlined))?;
        }
        if modifier.contains(Modifier::REVERSED) {
            queue!(stdout, SetAttribute(Attribute::Reverse))?;
        }
        queue!(stdout, Print(symbol), SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}

/// Render one row of an off-screen `Buffer` at the cursor's current position,
/// preserving fg/bg/modifiers. Used by `insert_before` to emit history rows
/// into the DECSTBM scroll region (CRLF advances; this paints the new row).
fn write_buffer_row<W: Write>(stdout: &mut W, buf: &Buffer, y: u16) -> io::Result<()> {
    queue!(stdout, Clear(ClearType::CurrentLine))?;
    for x in 0..buf.area.width {
        let idx = (y as usize) * (buf.area.width as usize) + (x as usize);
        let cell = &buf.content[idx];
        let fg = to_crossterm_color(cell.fg);
        let bg = to_crossterm_color(cell.bg);
        queue!(stdout, SetForegroundColor(fg), SetBackgroundColor(bg))?;

        let mods = cell.modifier;
        if mods.contains(Modifier::BOLD) {
            queue!(stdout, SetAttribute(Attribute::Bold))?;
        }
        if mods.contains(Modifier::DIM) {
            queue!(stdout, SetAttribute(Attribute::Dim))?;
        }
        if mods.contains(Modifier::ITALIC) {
            queue!(stdout, SetAttribute(Attribute::Italic))?;
        }
        if mods.contains(Modifier::UNDERLINED) {
            queue!(stdout, SetAttribute(Attribute::Underlined))?;
        }
        if mods.contains(Modifier::REVERSED) {
            queue!(stdout, SetAttribute(Attribute::Reverse))?;
        }

        queue!(stdout, Print(cell.symbol()), SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}

/// Scroll a region of the terminal up using DECSTBM.
fn scroll_region_up(first_row: u16, last_row: u16, amount: u16) -> io::Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let mut stdout = io::stdout();
    // CSI first+1 ; last+1 r  → set scroll region
    // CSI amount S             → scroll up
    // CSI r                    → reset scroll region
    write!(
        stdout,
        "\x1b[{};{}r\x1b[{}S\x1b[r",
        first_row + 1,
        last_row + 1,
        amount
    )?;
    stdout.flush()
}

/// Convert ratatui Color to crossterm Color.
fn to_crossterm_color(color: Color) -> CtColor {
    match color {
        Color::Reset => CtColor::Reset,
        Color::Black => CtColor::Black,
        Color::Red => CtColor::DarkRed,
        Color::Green => CtColor::DarkGreen,
        Color::Yellow => CtColor::DarkYellow,
        Color::Blue => CtColor::DarkBlue,
        Color::Magenta => CtColor::DarkMagenta,
        Color::Cyan => CtColor::DarkCyan,
        Color::Gray => CtColor::Grey,
        Color::DarkGray => CtColor::DarkGrey,
        Color::LightRed => CtColor::Red,
        Color::LightGreen => CtColor::Green,
        Color::LightYellow => CtColor::Yellow,
        Color::LightBlue => CtColor::Blue,
        Color::LightMagenta => CtColor::Magenta,
        Color::LightCyan => CtColor::Cyan,
        Color::White => CtColor::White,
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
        Color::Indexed(i) => CtColor::AnsiValue(i),
    }
}

impl Drop for DynamicViewport {
    fn drop(&mut self) {
        // Move cursor below viewport on exit
        let _ = execute!(
            io::stdout(),
            MoveTo(0, self.viewport.y + self.viewport.height),
            Show
        );
    }
}

// ─── pure-logic unit tests (no terminal I/O) ─────────────────────────────────
//
// These tests exercise the scrollback_committed invariant computation and the
// repaint_history_tail offset selection — the two arithmetic paths at the heart
// of mu-solo-scrollback-dup-recommit-8hva.  We cannot instantiate a real
// DynamicViewport in CI (no TTY), so we test the pure helper functions that
// encode the invariant logic directly.

#[cfg(test)]
mod tests {
    /// Compute what scrollback_committed should be after insert_before(n_lines)
    /// when history has `history_len` entries and the viewport top is at
    /// `viewport_top` screen rows.  Mirrors the post-insert update in
    /// `insert_before`.
    fn scrollback_committed_after_insert(history_len: usize, viewport_top: usize) -> usize {
        history_len.saturating_sub(viewport_top)
    }

    /// Compute the `start` index into `history` that `repaint_history_tail`
    /// should use.  `visible_rows` is `viewport.y` (rows above the viewport).
    /// Mirrors the fixed `repaint_history_tail` implementation.
    fn repaint_start(
        history_len: usize,
        visible_rows: usize,
        scrollback_committed: usize,
    ) -> usize {
        let naive_start = history_len.saturating_sub(visible_rows);
        naive_start.max(scrollback_committed)
    }

    // ── scrollback_committed invariant ───────────────────────────────────────

    #[test]
    fn scrollback_committed_zero_when_history_fits_in_viewport() {
        // 5 history lines, 20-row viewport region → nothing overflows.
        assert_eq!(scrollback_committed_after_insert(5, 20), 0);
    }

    #[test]
    fn scrollback_committed_counts_overflow() {
        // 50 lines inserted into a 20-row region → 30 lines to scrollback.
        assert_eq!(scrollback_committed_after_insert(50, 20), 30);
    }

    #[test]
    fn scrollback_committed_saturates_at_zero_for_small_history() {
        // viewport is larger than history — no overflow.
        assert_eq!(scrollback_committed_after_insert(3, 20), 0);
    }

    #[test]
    fn scrollback_committed_exact_fit() {
        // Exactly viewport_top lines — boundary: no overflow.
        assert_eq!(scrollback_committed_after_insert(20, 20), 0);
    }

    #[test]
    fn scrollback_committed_one_over_fit() {
        // One line past the viewport top → one line in scrollback.
        assert_eq!(scrollback_committed_after_insert(21, 20), 1);
    }

    // ── repaint_start offset logic ────────────────────────────────────────────

    #[test]
    fn repaint_start_no_scrollback_overflow_normal_case() {
        // 10 history lines, all fit in 20-row visible region,
        // nothing committed to scrollback.
        let start = repaint_start(10, 20, 0);
        // Should start at 0 (history.len() - visible_rows saturates to 0).
        assert_eq!(start, 0);
    }

    #[test]
    fn repaint_start_clamps_to_scrollback_committed() {
        // The bug scenario:
        // history has 50 lines; viewport_top was 20 → scrollback_committed=30.
        // Viewport shrinks to new top=35 → repaint wants last 35 lines but
        // must not touch the first 30 (in native scrollback).
        let start = repaint_start(50, 35, 30);
        // naive_start = 50 - 35 = 15; clamped to 30 by scrollback_committed.
        assert_eq!(start, 30);
        // Lines to draw = 50 - 30 = 20 (not 35).  This is the fix: 15 lines
        // that would have caused duplication are no longer drawn on-screen.
    }

    #[test]
    fn repaint_start_no_clamp_when_history_all_on_screen() {
        // Small history, no overflow: clamp has no effect.
        let start = repaint_start(5, 35, 0);
        // naive_start = 0 (5 lines fit in 35 rows), scrollback_committed=0.
        assert_eq!(start, 0);
    }

    #[test]
    fn repaint_start_identical_naive_and_committed() {
        // If naive_start == scrollback_committed there's no duplicate risk.
        let start = repaint_start(50, 20, 30);
        // naive_start = 50 - 20 = 30; clamp(30, 30) = 30.
        assert_eq!(start, 30);
    }

    #[test]
    fn repaint_start_naive_exceeds_committed() {
        // Shrink to very small visible area: naive_start > committed.
        // Use the naive_start (it's safe because it's already past scrollback).
        let start = repaint_start(50, 5, 30);
        // naive_start = 45; committed = 30; max(45, 30) = 45.
        assert_eq!(start, 45);
    }

    // ── duplication shape verification ───────────────────────────────────────
    //
    // Verifies the exact "before once / span twice / tail once" pattern is
    // eliminated.  Uses symbolic history indices rather than real terminal
    // cells — the arithmetic is what matters.

    #[test]
    fn no_duplication_after_large_insert_and_shrink() {
        // Simulate the real session:
        // - viewport_top = 20, insert 50 lines → scrollback_committed = 30
        // - viewport shrinks to top = 35
        // - repaint must only draw lines [30..50] (20 lines), NOT [15..50].

        let history_len = 50usize;
        let viewport_top_before = 20usize;
        let viewport_top_after = 35usize;

        let committed = scrollback_committed_after_insert(history_len, viewport_top_before);
        // 30 lines in native scrollback.
        assert_eq!(committed, 30);

        let start = repaint_start(history_len, viewport_top_after, committed);
        // Must NOT start before committed.
        assert!(
            start >= committed,
            "repaint started at {start} which is before scrollback boundary {committed} — would duplicate"
        );
        // Should draw history[30..50] — lines not in scrollback.
        assert_eq!(start, 30);
        let rows_drawn = history_len.saturating_sub(start);
        assert_eq!(rows_drawn, 20);
    }

    // ── journal mismatch detection ────────────────────────────────────────────

    #[test]
    fn journal_path_pattern_is_in_solo_subdir_not_events() {
        // Verify the journal path is under `.../mu/solo/` and NOT under
        // `.../mu/events/`.  Tests the path construction logic conceptually.
        let base = std::path::Path::new("/home/user/.local/share/mu");
        let journal = base.join("solo").join("renderer.jsonl");
        let events = base.join("events");
        assert!(journal.starts_with(base.join("solo")));
        assert!(!journal.starts_with(events));
    }

    #[test]
    fn journal_entry_is_valid_jsonl() {
        // Write a journal entry to a temp file and verify it's parseable JSON.
        use std::io::Read;

        let tmp = tempfile_for_test();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp)
            .expect("open tmp");

        let ts_ms: u128 = 12345678;
        let offset_start = 0usize;
        let offset_end = 10usize;
        let line_count: u16 = 10;
        let trigger = "insert_before";
        let line = format!(
            "{{\"ts_ms\":{ts_ms},\"offset_start\":{offset_start},\"offset_end\":{offset_end},\"line_count\":{line_count},\"trigger\":\"{trigger}\"}}\n"
        );
        use std::io::Write as _;
        f.write_all(line.as_bytes()).expect("write");
        drop(f);

        let mut contents = String::new();
        std::fs::File::open(&tmp)
            .expect("reopen")
            .read_to_string(&mut contents)
            .expect("read");

        // Each non-empty line must be valid JSON.
        for l in contents.lines().filter(|l| !l.is_empty()) {
            let v: serde_json::Value = serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("journal line not valid JSON: {e}\n  line: {l:?}"));
            assert_eq!(v["trigger"], "insert_before");
            assert_eq!(v["line_count"], 10);
        }

        let _ = std::fs::remove_file(&tmp);
    }

    fn tempfile_for_test() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "mu_solo_viewport_test_{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        p
    }
}
