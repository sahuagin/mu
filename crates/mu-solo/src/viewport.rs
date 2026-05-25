//! Dynamic inline viewport — a minimal custom terminal that supports
//! grow/shrink of the viewport area while preserving native scrollback.
//!
//! Inspired by codex-rs/tui/src/custom_terminal.rs (Apache-2.0).
//! Only implements the subset needed for mu-solo: render a viewport of
//! variable height at the bottom of the terminal, scroll the region
//! above it when the viewport grows, and shrink when it contracts.

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
}

impl DynamicViewport {
    /// Create a new viewport starting at the current cursor position.
    /// The initial height is the number of lines to claim at the bottom.
    pub fn new(initial_height: u16) -> io::Result<Self> {
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

        Ok(Self {
            viewport,
            buffers: [Buffer::empty(viewport), Buffer::empty(viewport)],
            current: 0,
            screen_size: (cols, rows),
            history: Vec::new(),
        })
    }

    /// Get the current viewport area for rendering into.
    pub fn area(&self) -> Rect {
        self.viewport
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
            // Shrinking: keep viewport.y the same — blank space goes
            // below. Conversation content above stays undisturbed.
            // Clear the old rows below the new smaller viewport.
            let mut stdout = io::stdout();
            for row in (self.viewport.y + new_height)..(self.viewport.y + old_height) {
                queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
            }
            stdout.flush()?;
            self.viewport.height = new_height;
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

        // Scroll the region above the viewport up to make room
        if viewport_top > 0 {
            let scroll_amount = height.min(viewport_top);
            scroll_region_up(0, viewport_top.saturating_sub(1), scroll_amount)?;
        }
        stdout.flush()?;

        // Draw the new content in the freed space above the viewport
        let draw_area = Rect::new(0, viewport_top.saturating_sub(height), width, height);
        let mut buf = Buffer::empty(draw_area);
        draw_fn(&mut buf);

        // Write the buffer content to the terminal AND store in history
        let mut stdout = io::stdout();
        for y in 0..draw_area.height {
            let mut hline = HistoryLine { cells: Vec::new() };
            for x in 0..draw_area.width {
                let idx = (y as usize) * (draw_area.width as usize) + (x as usize);
                let cell = &buf.content[idx];
                if cell.symbol() != " " || cell.fg != Color::Reset || cell.bg != Color::Reset {
                    queue!(stdout, MoveTo(draw_area.x + x, draw_area.y + y))?;
                    let fg = to_crossterm_color(cell.fg);
                    let bg = to_crossterm_color(cell.bg);
                    queue!(stdout, SetForegroundColor(fg), SetBackgroundColor(bg))?;
                    if cell.modifier.contains(Modifier::BOLD) {
                        queue!(stdout, SetAttribute(Attribute::Bold))?;
                    }
                    queue!(stdout, Print(cell.symbol()), SetAttribute(Attribute::Reset))?;
                }
                hline.cells.push((
                    cell.symbol().to_string(),
                    cell.fg,
                    cell.bg,
                    cell.modifier,
                ));
            }
            self.history.push(hline);
        }
        stdout.flush()?;

        // Cap history to prevent unbounded growth (keep last 1000 lines)
        const MAX_HISTORY: usize = 1000;
        if self.history.len() > MAX_HISTORY {
            let drain = self.history.len() - MAX_HISTORY;
            self.history.drain(0..drain);
        }

        Ok(())
    }

    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }
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
