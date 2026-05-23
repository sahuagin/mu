//! Cursor-aware input buffer with visual-line wrapping.
//!
//! Replaces the v0 single-string prompt. Supports:
//! - Insert at cursor, delete before/after cursor
//! - Left/Right/Home/End navigation
//! - Multi-line content (from paste or future Ctrl-Enter)
//! - Visual wrap to a given width for rendering
//! - Cursor position expressed as (visual_row, visual_col)

/// A single input buffer with cursor tracking.
pub struct InputBuffer {
    /// The raw text content. May contain newlines (from paste).
    content: String,
    /// Byte offset of the cursor within `content`. Always on a char
    /// boundary. Range: 0..=content.len().
    cursor: usize,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            cursor: 0,
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Take the content out (for sending), reset to empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.content)
    }

    /// Clear without returning content.
    pub fn clear(&mut self) {
        self.content.clear();
        self.cursor = 0;
    }

    /// Insert a single character at cursor.
    pub fn insert_char(&mut self, c: char) {
        self.content.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert a string at cursor (used for paste).
    pub fn insert_str(&mut self, s: &str) {
        self.content.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Delete one char before cursor (backspace).
    pub fn delete_before(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_char_boundary();
        self.content.drain(prev..self.cursor);
        self.cursor = prev;
    }

    /// Delete one char after cursor (delete key).
    pub fn delete_after(&mut self) {
        if self.cursor >= self.content.len() {
            return;
        }
        let next = self.next_char_boundary();
        self.content.drain(self.cursor..next);
    }

    /// Move cursor one char left.
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_char_boundary();
        }
    }

    /// Move cursor one char right.
    pub fn move_right(&mut self) {
        if self.cursor < self.content.len() {
            self.cursor = self.next_char_boundary();
        }
    }

    /// Move cursor to start of content.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move cursor to end of content.
    pub fn move_end(&mut self) {
        self.cursor = self.content.len();
    }

    /// Move cursor one word left (to start of previous word).
    pub fn move_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let bytes = self.content.as_bytes();
        let mut pos = self.prev_char_boundary();
        // Skip whitespace
        while pos > 0 && bytes[pos - 1].is_ascii_whitespace() {
            pos = prev_boundary(&self.content, pos);
        }
        // Skip word chars
        while pos > 0 && !bytes[pos - 1].is_ascii_whitespace() {
            pos = prev_boundary(&self.content, pos);
        }
        self.cursor = pos;
    }

    /// Move cursor one word right (to end of next word).
    pub fn move_word_right(&mut self) {
        let len = self.content.len();
        if self.cursor >= len {
            return;
        }
        let mut pos = self.next_char_boundary();
        let bytes = self.content.as_bytes();
        // Skip word chars
        while pos < len && !bytes[pos].is_ascii_whitespace() {
            pos = next_boundary(&self.content, pos);
        }
        // Skip whitespace
        while pos < len && bytes[pos].is_ascii_whitespace() {
            pos = next_boundary(&self.content, pos);
        }
        self.cursor = pos;
    }

    /// Compute visual lines given a wrap width. Returns (lines, cursor_row, cursor_col).
    /// Each visual line is a string slice reference into content plus its byte range.
    pub fn visual_layout(&self, wrap_width: usize) -> VisualLayout {
        let wrap_width = wrap_width.max(1);
        let mut lines: Vec<VisualLine> = Vec::new();
        let mut cursor_row: usize = 0;
        let mut cursor_col: usize = 0;
        let mut found_cursor = false;

        for (logical_idx, logical_line) in self.content.split('\n').enumerate() {
            let line_start = if logical_idx == 0 {
                0
            } else {
                // Account for the \n separators before this line
                self.content[..self.content.len()]
                    .match_indices('\n')
                    .nth(logical_idx - 1)
                    .map(|(i, _)| i + 1)
                    .unwrap_or(0)
            };

            if logical_line.is_empty() {
                let row_idx = lines.len();
                lines.push(VisualLine {
                    text: String::new(),
                    byte_start: line_start,
                    byte_end: line_start,
                });
                if !found_cursor && self.cursor == line_start {
                    cursor_row = row_idx;
                    cursor_col = 0;
                    found_cursor = true;
                }
                continue;
            }

            let chars: Vec<char> = logical_line.chars().collect();
            let mut col = 0;
            let mut row_start_byte = line_start;
            let mut row_start_char = 0;

            for (ci, _ch) in chars.iter().enumerate() {
                let char_byte_pos = line_start
                    + logical_line
                        .char_indices()
                        .nth(ci)
                        .map(|(b, _)| b)
                        .unwrap_or(0);

                if col >= wrap_width && col > 0 {
                    let row_text: String = chars[row_start_char..ci].iter().collect();
                    let byte_end = char_byte_pos;
                    let row_idx = lines.len();
                    lines.push(VisualLine {
                        text: row_text,
                        byte_start: row_start_byte,
                        byte_end,
                    });
                    if !found_cursor
                        && self.cursor >= row_start_byte
                        && self.cursor < byte_end
                    {
                        cursor_row = row_idx;
                        cursor_col = self.content[row_start_byte..self.cursor]
                            .chars()
                            .count();
                        found_cursor = true;
                    }
                    row_start_byte = byte_end;
                    row_start_char = ci;
                    col = 0;
                }
                col += 1;

                // Handle cursor at this exact position
                if !found_cursor && self.cursor == char_byte_pos {
                    cursor_row = lines.len();
                    cursor_col = col - 1;
                    found_cursor = true;
                }
            }

            // Flush remaining chars in this logical line
            let row_text: String = chars[row_start_char..].iter().collect();
            let byte_end = line_start + logical_line.len();
            let row_idx = lines.len();
            lines.push(VisualLine {
                text: row_text,
                byte_start: row_start_byte,
                byte_end,
            });
            if !found_cursor && self.cursor >= row_start_byte && self.cursor <= byte_end {
                cursor_row = row_idx;
                cursor_col = self.content[row_start_byte..self.cursor].chars().count();
                found_cursor = true;
            }
        }

        // Edge case: empty content
        if lines.is_empty() {
            lines.push(VisualLine {
                text: String::new(),
                byte_start: 0,
                byte_end: 0,
            });
            cursor_row = 0;
            cursor_col = 0;
        }

        VisualLayout {
            lines,
            cursor_row,
            cursor_col,
        }
    }

    fn prev_char_boundary(&self) -> usize {
        prev_boundary(&self.content, self.cursor)
    }

    fn next_char_boundary(&self) -> usize {
        next_boundary(&self.content, self.cursor)
    }
}

fn prev_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.saturating_sub(1);
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p.min(s.len())
}

/// Result of computing visual layout for the input buffer.
pub struct VisualLayout {
    pub lines: Vec<VisualLine>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

/// One visual (wrapped) line of input.
pub struct VisualLine {
    pub text: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer() {
        let buf = InputBuffer::new();
        assert!(buf.is_empty());
        let layout = buf.visual_layout(80);
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(layout.cursor_row, 0);
        assert_eq!(layout.cursor_col, 0);
    }

    #[test]
    fn insert_and_navigate() {
        let mut buf = InputBuffer::new();
        buf.insert_str("hello world");
        assert_eq!(buf.content(), "hello world");
        buf.move_left();
        buf.move_left();
        buf.insert_char('X');
        assert_eq!(buf.content(), "hello worXld");
    }

    #[test]
    fn backspace_at_cursor() {
        let mut buf = InputBuffer::new();
        buf.insert_str("abc");
        buf.move_left();
        buf.delete_before();
        assert_eq!(buf.content(), "ac");
    }

    #[test]
    fn paste_with_newlines() {
        let mut buf = InputBuffer::new();
        buf.insert_str("line one\nline two\nline three");
        let layout = buf.visual_layout(80);
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(layout.lines[0].text, "line one");
        assert_eq!(layout.lines[1].text, "line two");
        assert_eq!(layout.lines[2].text, "line three");
    }

    #[test]
    fn wraps_long_line() {
        let mut buf = InputBuffer::new();
        buf.insert_str("abcdefghij");
        let layout = buf.visual_layout(5);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.lines[0].text, "abcde");
        assert_eq!(layout.lines[1].text, "fghij");
    }

    #[test]
    fn cursor_position_after_wrap() {
        let mut buf = InputBuffer::new();
        buf.insert_str("abcdefgh");
        // cursor at end = position 8
        let layout = buf.visual_layout(5);
        // "abcde" (row 0) + "fgh" (row 1)
        assert_eq!(layout.cursor_row, 1);
        assert_eq!(layout.cursor_col, 3);
    }

    #[test]
    fn home_end() {
        let mut buf = InputBuffer::new();
        buf.insert_str("hello");
        buf.move_home();
        buf.insert_char('X');
        assert_eq!(buf.content(), "Xhello");
        buf.move_end();
        buf.insert_char('Y');
        assert_eq!(buf.content(), "XhelloY");
    }

    #[test]
    fn take_resets() {
        let mut buf = InputBuffer::new();
        buf.insert_str("content");
        let taken = buf.take();
        assert_eq!(taken, "content");
        assert!(buf.is_empty());
    }
}
