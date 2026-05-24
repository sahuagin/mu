//! Cursor-aware input buffer with visual-line wrapping.
//!
//! Replaces the v0 single-string prompt. Supports:
//! - Insert at cursor, delete before/after cursor
//! - Left/Right/Home/End navigation
//! - Multi-line content (from paste or future Ctrl-Enter)
//! - Visual wrap to a given width for rendering
//! - Cursor position expressed as (visual_row, visual_col)
//! - Paste collapse: large pastes render as a one-line placeholder

/// Minimum line count for a paste to be collapsed in the visual display.
const PASTE_COLLAPSE_THRESHOLD: usize = 3;

/// A recorded paste region within the content buffer.
#[derive(Debug, Clone)]
struct PasteRegion {
    /// Byte offset where this paste starts in `content`.
    start: usize,
    /// Byte offset where this paste ends in `content`.
    end: usize,
    /// Session-wide paste counter (1-based).
    number: usize,
    /// Number of lines in the original paste.
    line_count: usize,
}

/// A single input buffer with cursor tracking.
pub struct InputBuffer {
    /// The raw text content. May contain newlines (from paste).
    content: String,
    /// Byte offset of the cursor within `content`. Always on a char
    /// boundary. Range: 0..=content.len().
    cursor: usize,
    /// Paste regions tracked for visual collapse. Sorted by start offset.
    pastes: Vec<PasteRegion>,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            cursor: 0,
            pastes: Vec::new(),
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Take the content out (for sending), reset to empty.
    /// Returns the FULL content with pastes expanded.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.pastes.clear();
        std::mem::take(&mut self.content)
    }

    /// Clear without returning content.
    pub fn clear(&mut self) {
        self.content.clear();
        self.cursor = 0;
        self.pastes.clear();
    }

    /// Insert a single character at cursor.
    pub fn insert_char(&mut self, c: char) {
        self.content.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insert a string at cursor (generic, no paste tracking).
    pub fn insert_str(&mut self, s: &str) {
        self.shift_paste_regions(self.cursor, s.len() as isize);
        self.content.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Insert a paste at cursor, recording it for visual collapse.
    /// `paste_number` is the session-wide 1-based paste counter.
    pub fn insert_paste(&mut self, s: &str, paste_number: usize) {
        let start = self.cursor;
        let line_count = s.lines().count().max(1);
        self.shift_paste_regions(self.cursor, s.len() as isize);
        self.content.insert_str(self.cursor, s);
        self.cursor += s.len();
        let end = self.cursor;
        self.pastes.push(PasteRegion {
            start,
            end,
            number: paste_number,
            line_count,
        });
        self.pastes.sort_by_key(|p| p.start);
    }

    /// Delete one char before cursor (backspace).
    pub fn delete_before(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_char_boundary();
        let deleted = self.cursor - prev;
        self.content.drain(prev..self.cursor);
        self.cursor = prev;
        self.adjust_paste_regions_for_delete(prev, deleted);
    }

    /// Delete one char after cursor (delete key).
    pub fn delete_after(&mut self) {
        if self.cursor >= self.content.len() {
            return;
        }
        let next = self.next_char_boundary();
        let deleted = next - self.cursor;
        self.content.drain(self.cursor..next);
        self.adjust_paste_regions_for_delete(self.cursor, deleted);
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

    /// Shift paste regions that start at or after `pos` by `delta` bytes.
    /// Called before insertions to keep regions aligned.
    fn shift_paste_regions(&mut self, pos: usize, delta: isize) {
        for p in &mut self.pastes {
            if p.start >= pos {
                p.start = (p.start as isize + delta) as usize;
                p.end = (p.end as isize + delta) as usize;
            } else if p.end > pos {
                // Insertion inside a paste region — extend it
                p.end = (p.end as isize + delta) as usize;
            }
        }
    }

    /// Adjust paste regions after a deletion at `pos` of `len` bytes.
    /// Removes regions that become empty or are fully deleted.
    fn adjust_paste_regions_for_delete(&mut self, pos: usize, len: usize) {
        let end = pos + len;
        self.pastes.retain_mut(|p| {
            if p.start >= end {
                // Region is entirely after the deletion
                p.start -= len;
                p.end -= len;
                true
            } else if p.end <= pos {
                // Region is entirely before the deletion
                true
            } else {
                // Region overlaps the deletion — shrink or remove
                if p.start >= pos {
                    p.start = pos;
                }
                if p.end > end {
                    p.end -= len;
                } else {
                    p.end = pos;
                }
                p.end > p.start // keep only if non-empty
            }
        });
    }

    /// Find the paste region (if any) that covers byte offset `pos`.
    #[allow(dead_code)]
    fn paste_at(&self, pos: usize) -> Option<&PasteRegion> {
        self.pastes.iter().find(|p| pos >= p.start && pos < p.end)
    }

    /// Compute visual lines given a wrap width. Returns (lines, cursor_row, cursor_col).
    /// Paste regions above PASTE_COLLAPSE_THRESHOLD lines are rendered as a
    /// single placeholder line `[Pasted text #N +X lines]`.
    pub fn visual_layout(&self, wrap_width: usize) -> VisualLayout {
        let wrap_width = wrap_width.max(1);
        let mut lines: Vec<VisualLine> = Vec::new();
        let mut cursor_row: usize = 0;
        let mut cursor_col: usize = 0;
        let mut found_cursor = false;

        // Build segments: chunks of content that are either normal text
        // or collapsed paste placeholders.
        let segments = self.build_display_segments();

        for seg in &segments {
            match seg {
                DisplaySegment::Text { start, end } => {
                    let text = &self.content[*start..*end];
                    self.layout_text_segment(
                        text,
                        *start,
                        wrap_width,
                        &mut lines,
                        &mut cursor_row,
                        &mut cursor_col,
                        &mut found_cursor,
                    );
                }
                DisplaySegment::CollapsedPaste { start, end, number, line_count } => {
                    let placeholder = format!(
                        "[Pasted text #{} +{} lines]",
                        number, line_count
                    );
                    let row_idx = lines.len();
                    lines.push(VisualLine {
                        text: placeholder,
                        byte_start: *start,
                        byte_end: *end,
                    });
                    // If cursor is inside the collapsed paste, place it
                    // at the end of the placeholder line.
                    if !found_cursor && self.cursor >= *start && self.cursor <= *end {
                        cursor_row = row_idx;
                        cursor_col = 0; // beginning of placeholder
                        found_cursor = true;
                    }
                }
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

    /// Build display segments from content + paste regions.
    fn build_display_segments(&self) -> Vec<DisplaySegment> {
        let mut segments: Vec<DisplaySegment> = Vec::new();
        let mut pos = 0;

        for paste in &self.pastes {
            if paste.line_count < PASTE_COLLAPSE_THRESHOLD {
                continue; // don't collapse short pastes
            }
            if paste.start > pos {
                segments.push(DisplaySegment::Text {
                    start: pos,
                    end: paste.start,
                });
            }
            segments.push(DisplaySegment::CollapsedPaste {
                start: paste.start,
                end: paste.end,
                number: paste.number,
                line_count: paste.line_count,
            });
            pos = paste.end;
        }
        if pos < self.content.len() {
            segments.push(DisplaySegment::Text {
                start: pos,
                end: self.content.len(),
            });
        }
        // If no segments (empty content or only non-collapsible pastes),
        // treat the whole content as one text segment.
        if segments.is_empty() {
            segments.push(DisplaySegment::Text {
                start: 0,
                end: self.content.len(),
            });
        }
        segments
    }

    /// Layout a text segment with word-wrap, updating lines and cursor state.
    fn layout_text_segment(
        &self,
        text: &str,
        byte_offset: usize,
        wrap_width: usize,
        lines: &mut Vec<VisualLine>,
        cursor_row: &mut usize,
        cursor_col: &mut usize,
        found_cursor: &mut bool,
    ) {
        for (logical_idx, logical_line) in text.split('\n').enumerate() {
            let line_start = if logical_idx == 0 {
                byte_offset
            } else {
                byte_offset
                    + text[..text.len()]
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
                if !*found_cursor && self.cursor == line_start {
                    *cursor_row = row_idx;
                    *cursor_col = 0;
                    *found_cursor = true;
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
                    if !*found_cursor
                        && self.cursor >= row_start_byte
                        && self.cursor < byte_end
                    {
                        *cursor_row = row_idx;
                        *cursor_col = self.content[row_start_byte..self.cursor]
                            .chars()
                            .count();
                        *found_cursor = true;
                    }
                    row_start_byte = byte_end;
                    row_start_char = ci;
                    col = 0;
                }
                col += 1;

                if !*found_cursor && self.cursor == char_byte_pos {
                    *cursor_row = lines.len();
                    *cursor_col = col - 1;
                    *found_cursor = true;
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
            if !*found_cursor && self.cursor >= row_start_byte && self.cursor <= byte_end {
                *cursor_row = row_idx;
                *cursor_col = self.content[row_start_byte..self.cursor].chars().count();
                *found_cursor = true;
            }
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

/// A segment of the display content — either normal text or a collapsed paste.
enum DisplaySegment {
    Text { start: usize, end: usize },
    CollapsedPaste { start: usize, end: usize, number: usize, line_count: usize },
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

    #[test]
    fn paste_collapse_large() {
        let mut buf = InputBuffer::new();
        buf.insert_str("before ");
        buf.insert_paste("line1\nline2\nline3\nline4\nline5\n", 1);
        buf.insert_str("after");
        let layout = buf.visual_layout(80);
        // Should have: "before " on line 0, placeholder on line 1, "after" on line 2
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(layout.lines[0].text, "before ");
        assert!(layout.lines[1].text.contains("[Pasted text #1"));
        assert!(layout.lines[1].text.contains("+5 lines"));
        assert_eq!(layout.lines[2].text, "after");
    }

    #[test]
    fn paste_no_collapse_small() {
        let mut buf = InputBuffer::new();
        buf.insert_paste("ab\ncd", 1); // only 2 lines, below threshold
        let layout = buf.visual_layout(80);
        // Should NOT collapse — renders as normal text
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.lines[0].text, "ab");
        assert_eq!(layout.lines[1].text, "cd");
    }

    #[test]
    fn paste_take_returns_full_content() {
        let mut buf = InputBuffer::new();
        buf.insert_str("hello ");
        buf.insert_paste("big\npaste\nwith\nmany\nlines\n", 1);
        buf.insert_str("world");
        let taken = buf.take();
        assert_eq!(taken, "hello big\npaste\nwith\nmany\nlines\nworld");
    }
}
