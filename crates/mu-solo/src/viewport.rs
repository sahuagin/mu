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
            // Shrinking: viewport stays pinned at bottom, top moves down.
            // Freed lines above keep whatever scrollback content is there.
            self.viewport.y += old_height - new_height;
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

    /// Render a widget into the viewport's buffer.
    pub fn render<W: Widget>(&mut self, widget: W) {
        let area = self.viewport;
        widget.render(area, self.current_buffer_mut());
    }

    /// Flush changes to the terminal — diffs against previous frame
    /// and only writes changed cells.
    pub fn flush(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        queue!(stdout, Hide)?;

        let area = self.viewport;
        let prev = &self.buffers[1 - self.current];
        let curr = &self.buffers[self.current];

        for y in 0..area.height {
            for x in 0..area.width {
                let idx = (y as usize) * (area.width as usize) + (x as usize);
                let prev_cell = &prev.content[idx];
                let curr_cell = &curr.content[idx];

                if curr_cell != prev_cell {
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

        queue!(stdout, Show)?;
        stdout.flush()?;

        // Swap buffers
        self.buffers[1 - self.current].reset();
        self.current = 1 - self.current;
        Ok(())
    }

    /// Insert lines above the viewport (push content into scrollback).
    /// Used for conversation output (assistant responses, tool calls, etc.)
    pub fn insert_before<F>(&mut self, height: u16, draw_fn: F) -> io::Result<()>
    where
        F: FnOnce(&mut Buffer),
    {
        if height == 0 {
            return Ok(());
        }

        let viewport_top = self.viewport.y;
        let width = self.viewport.width;

        // Scroll the region above the viewport up to make room
        if viewport_top > 0 {
            let scroll_amount = height.min(viewport_top);
            scroll_region_up(0, viewport_top.saturating_sub(1), scroll_amount)?;
        }

        // Draw the new content in the freed space above the viewport
        let draw_area = Rect::new(0, viewport_top.saturating_sub(height), width, height);
        let mut buf = Buffer::empty(draw_area);
        draw_fn(&mut buf);

        // Write the buffer content to the terminal
        let mut stdout = io::stdout();
        for y in 0..draw_area.height {
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
            }
        }
        stdout.flush()?;

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
