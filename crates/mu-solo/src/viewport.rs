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
//! that have been pushed into native scrollback and are therefore no
//! longer screen-addressable.  The invariant is:
//!
//!   `scrollback_committed = max(0, history.len() − (viewport.y − gap_rows))`
//!
//! after every `insert_before` call (`gap_rows` — blank rows a
//! chrome-pinned shrink left between transcript and viewport — hold no
//! content); `set_height`'s grow path advances it when its region push
//! feeds resident rows into scrollback.
//!
//! Nothing ever redraws lines above the viewport (mu-8oqp): scrollback
//! lines are unreachable, so any redraw scheme must choose between a
//! blank band, a duplicated band, or an on-screen copy of scrollback
//! content — all three shipped as bugs at some point.  Content above
//! the viewport moves only UP, into scrollback, via the CRLF pattern.
//!
//! ## Emission strategies (mu-solo-zellij-blank-band-ptvm)
//!
//! The escape-sequence emission of `insert_before` is selected ONCE at
//! startup (`EmissionStrategy`, see `detect_emission_strategy`):
//!
//! - **Fast** (default, codex-rs pattern, verified on kitty/xterm):
//!   DECSTBM + `CSI T` push-down when the viewport isn't at the bottom,
//!   then one `?2026`-wrapped burst that newline-scrolls the whole
//!   payload through the top-margin-1 region.
//! - **Conservative** (selected when `$ZELLIJ` is set): zellij's
//!   compositor has been observed to blank-fill instead of moving
//!   content for some margined-scroll bursts — a large turn commit left
//!   a ~viewport-height blank band in scrollback while the renderer
//!   journal showed a contiguous commit (the defect is in
//!   emission × compositor, not history accounting).  The conservative
//!   path avoids every suspect mechanism: no DECSTBM+`CSI T` reverse
//!   scroll (hypothesis a), the payload is emitted in chunks strictly
//!   smaller than the history region with margins reset, cursor
//!   re-homed and output flushed between chunks (hypothesis b), and no
//!   `?2026` synchronized-output brackets (hypothesis c).  Costs some
//!   flicker/speed; buys contiguous scrollback under zellij.
//!
//! `MU_SOLO_FORCE_CONSERVATIVE_RENDER=1|0` overrides auto-detection in
//! either direction for live bisection.

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

/// A rendered viewport cell, reduced to the fields `flush` actually writes.
/// Kept small/cloneable so diff-based flushing can skip unchanged cells instead
/// of repainting the whole viewport on every prompt keypress.
type RenderCell = (String, Color, Color, Modifier);

/// A stored line of history content (what insert_before rendered).
/// Only the line COUNT is load-bearing now (journal offsets, drain and
/// scrollback-committed accounting); nothing redraws the cells since the
/// shrink repaint was removed (mu-8oqp). Slimming this to a counter is a
/// possible follow-up.
#[derive(Clone)]
struct HistoryLine {
    cells: Vec<RenderCell>,
}

/// Cap on retained `history` lines — `insert_before` drains the oldest
/// entries past this. `pub(crate)` so the finalize-mismatch check in
/// `app.rs` can compute the drain-aware expected length
/// `min(before + h, MAX_HISTORY)` instead of false-alarming whenever a
/// drain fires (8hva judge finding).
pub(crate) const MAX_HISTORY: usize = 1000;

/// How `insert_before` emits escape sequences (mu-solo-zellij-blank-band-ptvm).
/// Selected once at startup; see the module docs for the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmissionStrategy {
    /// codex-rs pattern: DECSTBM+`CSI T` push-down, one `?2026`-wrapped
    /// margined-scroll burst. Verified on kitty/xterm.
    Fast,
    /// zellij-safe: no reverse scroll, no sync brackets, chunked
    /// margined-scroll smaller than the history region.
    Conservative,
}

impl EmissionStrategy {
    fn as_str(self) -> &'static str {
        match self {
            EmissionStrategy::Fast => "fast",
            EmissionStrategy::Conservative => "conservative",
        }
    }
}

/// Pure strategy selection — split from env reading so it's unit-testable.
/// `force` is the value of `MU_SOLO_FORCE_CONSERVATIVE_RENDER` (if set);
/// `zellij_set` is whether `$ZELLIJ` exists (zellij exports it in every pane).
/// The force knob wins over auto-detection in both directions so the
/// operator can live-bisect either path under either terminal.
fn select_emission_strategy(
    force: Option<&str>,
    zellij_set: bool,
) -> (EmissionStrategy, &'static str) {
    match force {
        Some("1") => (
            EmissionStrategy::Conservative,
            "forced: MU_SOLO_FORCE_CONSERVATIVE_RENDER=1",
        ),
        Some("0") => (
            EmissionStrategy::Fast,
            "forced: MU_SOLO_FORCE_CONSERVATIVE_RENDER=0",
        ),
        _ => {
            if zellij_set {
                (
                    EmissionStrategy::Conservative,
                    "ZELLIJ env var set (zellij pane detected)",
                )
            } else {
                (
                    EmissionStrategy::Fast,
                    "no multiplexer detected (default codex-rs fast path)",
                )
            }
        }
    }
}

/// Read the environment ONCE and pick the emission strategy.  Called from
/// `DynamicViewport::new` (startup), never per-emission.
pub fn detect_emission_strategy() -> (EmissionStrategy, &'static str) {
    let force = std::env::var("MU_SOLO_FORCE_CONSERVATIVE_RENDER").ok();
    select_emission_strategy(force.as_deref(), std::env::var_os("ZELLIJ").is_some())
}

/// A minimal terminal that manages a dynamically-sized inline viewport.
/// Content above the viewport lives in native terminal scrollback.
pub struct DynamicViewport {
    /// Current viewport area (x, y, width, height).
    viewport: Rect,
    /// Double buffer for diff-based rendering.
    buffers: [Buffer; 2],
    /// Last cell image written by `flush`, aligned with `viewport`. `None`
    /// forces a full repaint (after resize/move/insert_before); otherwise the
    /// prompt hot path writes only cells that changed.
    screen_cache: Vec<Option<RenderCell>>,
    current: usize,
    /// Terminal screen size (columns, rows).
    screen_size: (u16, u16),
    /// History lines rendered above the viewport via insert_before.
    history: Vec<HistoryLine>,
    /// Number of history entries that have been committed to native
    /// terminal scrollback (and are therefore no longer addressable
    /// as screen rows).  Maintained by insert_before and by
    /// set_height's grow-path region push.  Drawing a committed line
    /// to a screen row would double it in the scroll-up view — the
    /// root cause of the mid-message span duplication bug
    /// (mu-solo-scrollback-dup-recommit-8hva).
    scrollback_committed: usize,
    /// Blank rows between the transcript tail and the viewport top, left
    /// by a chrome-pinned shrink (mu-8oqp). The next inserts PAINT into the
    /// gap top-down instead of scrolling; grow expands the viewport up over
    /// it. Always 0 when the transcript is flush against the viewport.
    gap_rows: u16,
    /// Optional renderer journal — appended by the commit paths.
    /// None when journalling is disabled (config knob renderer_journal).
    journal: Option<std::fs::File>,
    /// How insert_before emits escape sequences. Read from the
    /// environment exactly once, in `new` (mu-solo-zellij-blank-band-ptvm).
    strategy: EmissionStrategy,
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

        // Strategy is environment-derived state frozen at startup — one
        // read here, never per-emission (mu-solo-zellij-blank-band-ptvm).
        let (strategy, strategy_reason) = detect_emission_strategy();
        tracing::info!(
            strategy = strategy.as_str(),
            reason = strategy_reason,
            "renderer emission strategy selected"
        );

        let mut vp = Self {
            viewport,
            buffers: [Buffer::empty(viewport), Buffer::empty(viewport)],
            screen_cache: vec![None; viewport.width as usize * viewport.height as usize],
            current: 0,
            screen_size: (cols, rows),
            history: Vec::new(),
            scrollback_committed: 0,
            gap_rows: 0,
            journal,
            strategy,
        };
        // Make the selection visible in the flight recorder so a band
        // report can be correlated with the path that produced it.
        vp.journal_strategy(strategy_reason);
        Ok(vp)
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

    /// Resize the viewport to a new height.
    ///
    /// The chrome stays pinned to the screen bottom in BOTH directions —
    /// a prompt that rides up mid-session was tried before and rejected by
    /// the operator (it makes reading hard to follow).
    ///
    /// Shrinking keeps the transcript exactly where it is: the vacated rows
    /// become a tracked GAP between the transcript tail and the viewport
    /// (`gap_rows`), cleared but never repainted. Every scheme that
    /// repainted that area had to source rows from history that exists only
    /// in native scrollback, which is unreachable — the options were a
    /// blank band (the shipped bug), a duplicated band (closed PR #502), or
    /// an on-screen copy of scrollback lines (the 8hva regression). Content
    /// above the viewport must only ever move UP. The gap is consumed by
    /// `insert_before` painting new lines directly into it, and by the grow
    /// branch expanding up over it.
    ///
    /// Growing consumes the gap first (no terminal motion at all); only the
    /// remainder scrolls the history region up, through the
    /// scrollback-feeding CRLF pattern so exiting rows are preserved.
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
                self.invalidate_screen_cache();
            }
            return Ok(());
        }

        let old_height = self.viewport.height;

        if new_height > old_height {
            // Growing: expand up over the gap first (blank rows, no terminal
            // motion), then down over any free rows below (pre-snap edge
            // case), then a scrollback-feeding push for the remainder.
            let growth = new_height - old_height;
            let (from_gap, after_gap) = grow_split(growth, self.gap_rows);
            self.gap_rows -= from_gap;
            self.viewport.y -= from_gap;
            let viewport_bottom = self.viewport.y + old_height + from_gap;
            let free_below = rows.saturating_sub(viewport_bottom);
            let (_take_below, push_needed) = grow_split(after_gap, free_below);

            if push_needed > 0 {
                let viewport_top = self.viewport.y;
                let push = push_needed.min(viewport_top);
                if push > 0 {
                    let mut stdout = io::stdout();
                    match self.strategy {
                        EmissionStrategy::Fast => {
                            emit_region_push_up_fast(&mut stdout, viewport_top, push)?
                        }
                        EmissionStrategy::Conservative => {
                            emit_region_push_up_conservative(&mut stdout, viewport_top, push)?
                        }
                    }
                    // The exiting top rows entered native scrollback; count
                    // how many of them were resident history lines.
                    let resident = self.history.len().saturating_sub(self.scrollback_committed);
                    self.scrollback_committed +=
                        committed_delta_for_push(push as usize, viewport_top as usize, resident);
                    self.viewport.y -= push;
                }
            }
            self.viewport.height = new_height;
        } else {
            // Shrinking: chrome stays at the bottom; the transcript stays
            // put; the vacated rows in between become gap (see method docs).
            let old_y = self.viewport.y;
            let new_y = rows.saturating_sub(new_height);
            let mut stdout = io::stdout();
            for row in old_y..new_y {
                queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
            }
            self.gap_rows = self.gap_rows.saturating_add(new_y.saturating_sub(old_y));
            self.viewport.y = new_y;
            self.viewport.height = new_height;
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
        self.invalidate_screen_cache();
        stdout.flush()?;
        Ok(())
    }

    /// Clear the viewport area on screen (used before insert_before
    /// to erase the raw prompt before the formatted "you" block replaces it).
    pub fn clear_viewport(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        for row in self.viewport.y..(self.viewport.y + self.viewport.height) {
            queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }
        self.invalidate_screen_cache();
        stdout.flush()
    }

    /// Move the viewport to the bottom of the screen and forget any tracked
    /// gap. Only for moments when nothing resident sits above (startup,
    /// maximize/fullscreen exits — their entry paths pushed the transcript
    /// to scrollback): the rows skipped over are genuinely blank, and the
    /// gap reset makes subsequent inserts scroll normally instead of
    /// painting into dead rows. Do NOT call this mid-conversation — the
    /// chrome-pinned shrink + gap-paint in insert_before own that case
    /// (mu-8oqp).
    pub fn snap_to_bottom(&mut self) -> io::Result<()> {
        self.gap_rows = 0;
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
            self.invalidate_screen_cache();
            stdout.flush()?;
        }
        Ok(())
    }

    /// Render a widget into the viewport's buffer.
    pub fn render<W: Widget>(&mut self, widget: W) {
        let area = self.viewport;
        widget.render(area, self.current_buffer_mut());
    }

    /// Flush the viewport to the terminal. Diff against the last flushed cell
    /// image so prompt edits repaint only changed cells; structural terminal
    /// operations call `invalidate_screen_cache` to force a full repaint when
    /// the viewport moves/resizes or scrollback is inserted.
    pub fn flush(&mut self) -> io::Result<()> {
        self.ensure_screen_cache_shape();
        let area = self.viewport;
        let curr = &self.buffers[self.current];
        let mut changes: Vec<(u16, u16, usize, RenderCell)> = Vec::new();

        for y in 0..area.height {
            for x in 0..area.width {
                let idx = (y as usize) * (area.width as usize) + (x as usize);
                let curr_cell = &curr.content[idx];
                let image = (
                    curr_cell.symbol().to_string(),
                    curr_cell.fg,
                    curr_cell.bg,
                    curr_cell.modifier,
                );
                if self.screen_cache[idx].as_ref() != Some(&image) {
                    changes.push((x, y, idx, image));
                }
            }
        }

        if changes.is_empty() {
            self.current_buffer_mut().reset();
            return Ok(());
        }

        let mut stdout = io::stdout();
        // Begin synchronized output (terminal buffers until end bracket)
        write!(stdout, "\x1b[?2026h")?;
        queue!(stdout, Hide)?;

        for (x, y, idx, image) in changes {
            let (symbol, fg, bg, mods) = image.clone();
            let screen_y = area.y + y;
            let screen_x = area.x + x;
            queue!(stdout, MoveTo(screen_x, screen_y))?;

            // Apply style
            let ct_fg = to_crossterm_color(fg);
            let ct_bg = to_crossterm_color(bg);
            queue!(stdout, SetForegroundColor(ct_fg), SetBackgroundColor(ct_bg))?;

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

            queue!(stdout, Print(&symbol))?;
            queue!(stdout, SetAttribute(Attribute::Reset))?;
            self.screen_cache[idx] = Some(image);
        }

        // End synchronized output (terminal renders atomically)
        write!(stdout, "\x1b[?2026l")?;
        stdout.flush()?;

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

        // If the viewport isn't at the bottom of the screen (free rows left
        // by an in-place shrink, mu-8oqp), push it DOWN first to make room
        // above. The rows this vacates sit DIRECTLY below the transcript —
        // exactly where the first `push_down` new lines belong — so those
        // lines are later painted straight into them (`painted_direct`)
        // instead of being scrolled in at the region bottom. Scrolling the
        // full payload here would sweep the vacated blank rows up into the
        // middle of the transcript and prematurely commit resident lines to
        // scrollback — the band artifact, reintroduced through the side door.
        let viewport_bottom = self.viewport.y + self.viewport.height;
        let push_down = if viewport_bottom < screen_rows {
            let push_down = height.min(screen_rows - viewport_bottom);
            match self.strategy {
                EmissionStrategy::Fast => {
                    // Scroll the viewport region DOWN using reverse index
                    emit_push_down_fast(&mut stdout, self.viewport.y, screen_rows, push_down)?;
                }
                EmissionStrategy::Conservative => {
                    // Hypothesis (a) of mu-solo-zellij-blank-band-ptvm:
                    // zellij may blank-fill a margined reverse scroll
                    // (DECSTBM + CSI T) instead of moving the viewport
                    // image. We don't need the terminal to move anything:
                    // the vacated rows are painted directly below, and the
                    // viewport is invalidated so the next flush repaints its
                    // new position. Just clear the old viewport rows
                    // (prevents stale viewport pixels from scrolling up into
                    // history/scrollback) and relocate the viewport
                    // logically.
                    emit_push_down_conservative(
                        &mut stdout,
                        self.viewport.y,
                        self.viewport.height,
                    )?;
                }
            }
            self.viewport.y += push_down;
            self.buffers[0].resize(self.viewport);
            self.buffers[1].resize(self.viewport);
            // Force full redraw since viewport moved
            self.buffers[1 - self.current].reset();
            self.invalidate_screen_cache();
            push_down
        } else {
            0
        };

        // Gap-paint (mu-8oqp): rows between the transcript tail and the
        // viewport (left by a chrome-pinned shrink) receive the first
        // payload rows by direct paint — no scroll, nothing committed to
        // scrollback for them. Mutually exclusive with push_down: a gap
        // exists only when the viewport is already bottom-anchored.
        let gap_take = if push_down == 0 {
            self.gap_rows.min(height)
        } else {
            0
        };

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

        // Direct-paint rows: either the rows the viewport just vacated by
        // push-down, or the top of the gap (directly under the transcript
        // tail). Plain paints — no scrolling, nothing enters scrollback.
        // Only the remainder scrolls through the region.
        let painted_direct = (push_down.min(height)).max(gap_take);
        if painted_direct > 0 {
            let paint_top = if gap_take > 0 {
                // Top of the gap: directly under the transcript tail.
                let top = viewport_top - self.gap_rows;
                self.gap_rows -= gap_take;
                top
            } else {
                viewport_top - painted_direct
            };
            for i in 0..painted_direct {
                queue!(stdout, MoveTo(0, paint_top + i))?;
                write_buffer_row(&mut stdout, &buf, i)?;
            }
            stdout.flush()?;
        }

        if painted_direct < height {
            match self.strategy {
                EmissionStrategy::Fast => emit_insert_fast(
                    &mut stdout,
                    &buf,
                    painted_direct,
                    viewport_top,
                    self.viewport.x,
                    self.viewport.y,
                )?,
                EmissionStrategy::Conservative => emit_insert_conservative(
                    &mut stdout,
                    &buf,
                    painted_direct,
                    viewport_top,
                    self.viewport.x,
                    self.viewport.y,
                )?,
            }
        }
        self.buffers[1 - self.current].reset();
        self.invalidate_screen_cache();

        // Mirror the emitted rows into in-memory history (identical for both
        // strategies — the strategies differ only in escape-sequence framing,
        // never in content).
        for y in 0..draw_area.height {
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

        // Update scrollback_committed: lines that overflowed past
        // viewport_top went into native scrollback and must not be
        // redrawn.  The invariant after every insert_before is:
        //   scrollback_committed = max(0, history.len() − (viewport.y − gap_rows))
        // (gap rows sit inside the region but hold no content — counting
        // them as resident would undercount committed). Re-derive from the
        // new history length so accumulated rounding never drifts.
        // Monotonic: native scrollback is append-only, so committed can never
        // decrease. The naive re-derive undercounts when the region is not
        // full of content (e.g. the first small insert after a maximize/
        // fullscreen exit snapped the viewport under a blank region) — and a
        // committed undercount is the seed of the historic duplication class
        // (panel finding, 2026-08-03).
        let content_rows = (self.viewport.y - self.gap_rows) as usize;
        self.scrollback_committed = self
            .scrollback_committed
            .max(self.history.len().saturating_sub(content_rows));

        // Cap history to prevent unbounded growth (keep last MAX_HISTORY lines)
        if self.history.len() > MAX_HISTORY {
            let drain = self.history.len() - MAX_HISTORY;
            // Adjust scrollback_committed for the drain so the
            // invariant holds: drained lines were already committed.
            self.scrollback_committed = self.scrollback_committed.saturating_sub(drain);
            self.history.drain(0..drain);
        }

        // Emit one journal line per commit (cheap append, never to the
        // semantic event store). AFTER the drain on purpose: the logged
        // offsets index into `self.history`, and pre-drain values would
        // leave the journal stale exactly when a drain fires (8hva
        // verifier finding).
        let offset_start = self.history.len().saturating_sub(height as usize);
        let offset_end = self.history.len();
        self.journal_commit(offset_start, offset_end, height, "insert_before");

        Ok(())
    }

    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    fn ensure_screen_cache_shape(&mut self) {
        let expected = self.viewport.width as usize * self.viewport.height as usize;
        if self.screen_cache.len() != expected {
            self.screen_cache = vec![None; expected];
        }
    }

    fn invalidate_screen_cache(&mut self) {
        let expected = self.viewport.width as usize * self.viewport.height as usize;
        self.screen_cache = vec![None; expected];
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

    /// Record the startup emission-strategy selection in the journal so a
    /// blank-band report can be correlated with the path that produced it
    /// (mu-solo-zellij-blank-band-ptvm). One line per process start.
    fn journal_strategy(&mut self, reason: &str) {
        let strategy = self.strategy.as_str();
        let Some(ref mut f) = self.journal else {
            return;
        };
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = format!(
            "{{\"ts_ms\":{ts_ms},\"kind\":\"strategy\",\"strategy\":\"{strategy}\",\"reason\":\"{reason}\"}}\n"
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

    fn journal_notify_line(ts_ms: u128, occasion: &str, body_len: usize) -> String {
        serde_json::json!({
            "ts_ms": ts_ms,
            "kind": "notify",
            "trigger": "notify",
            "occasion": occasion,
            "body_len": body_len,
        })
        .to_string()
            + "\n"
    }

    /// Record an OSC-notification emission in the renderer journal. This is a
    /// projection flight-recorder line, not semantic history: when an operator
    /// says "no popup", it distinguishes "mu never emitted" from terminal /
    /// multiplexer suppression. The body itself is intentionally NOT logged.
    pub fn journal_notify(&mut self, occasion: &str, body: &str) {
        let Some(ref mut f) = self.journal else {
            return;
        };
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let body_len = body.chars().count();
        let line = Self::journal_notify_line(ts_ms, occasion, body_len);
        let _ = f.write_all(line.as_bytes());
    }

    /// Return the current history line count.  Used by callers that
    /// want to record pre-commit and post-commit offsets for the journal.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }
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

// ─── insert_before emission paths ─────────────────────────────────────────────
//
// Free functions generic over `W: Write` so tests can capture the exact byte
// stream without a TTY. `insert_before` calls them with stdout.

/// FAST push-down: scroll the region from the viewport top to the screen
/// bottom DOWN by `push_down` rows via DECSTBM + `CSI T` (reverse scroll
/// within margins). Byte-identical to the pre-strategy-split emission.
fn emit_push_down_fast<W: Write>(
    out: &mut W,
    viewport_y: u16,
    screen_rows: u16,
    push_down: u16,
) -> io::Result<()> {
    let region_top = viewport_y + 1; // 1-based
    let region_bottom = screen_rows;
    write!(
        out,
        "\x1b[{};{}r\x1b[{}T\x1b[r",
        region_top, region_bottom, push_down
    )
}

/// CONSERVATIVE push-down: no DECSTBM, no `CSI T` — just clear the rows the
/// viewport currently occupies. The caller relocates the viewport logically;
/// the subsequent history emission repaints the vacated rows and invalidates
/// the viewport cache so the next `flush()` repaints its new position. Clearing
/// prevents stale viewport pixels from being scrolled up into the history
/// region / native scrollback by the chunked emission.
fn emit_push_down_conservative<W: Write>(
    out: &mut W,
    viewport_y: u16,
    viewport_height: u16,
) -> io::Result<()> {
    for row in viewport_y..(viewport_y + viewport_height) {
        queue!(out, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
    }
    Ok(())
}

/// FAST history emission (codex-rs pattern, verified on kitty/xterm):
/// one `?2026`-synchronized burst that sets DECSTBM to the history region
/// (rows 1..=viewport_top, 1-based), parks the cursor on the region's bottom
/// row, and newline-scrolls every payload row through it. Byte-identical to
/// the pre-strategy-split emission — this is load-bearing: bare terminals
/// must not regress (mu-solo-zellij-blank-band-ptvm).
fn emit_insert_fast<W: Write>(
    out: &mut W,
    buf: &Buffer,
    from_row: u16,
    viewport_top: u16,
    viewport_x: u16,
    viewport_y: u16,
) -> io::Result<()> {
    // Begin synchronized output so a multi-line history insert + viewport
    // redraw does not visibly tear on terminals that support the extension.
    write!(out, "\x1b[?2026h")?;
    // ANSI scroll-region coordinates are 1-based and inclusive. The
    // history region is terminal rows 0..viewport_top (exclusive), so the
    // bottom row is `viewport_top` in 1-based coordinates.
    write!(out, "\x1b[1;{}r", viewport_top)?;
    queue!(out, MoveTo(0, viewport_top - 1))?;

    for y in from_row..buf.area.height {
        queue!(out, Print("\r\n"))?;
        write_buffer_row(out, buf, y)?;
    }

    // Reset scroll region and leave cursor in the viewport; the next
    // `flush` repaints the viewport from scratch.
    write!(out, "\x1b[r")?;
    queue!(out, MoveTo(viewport_x, viewport_y))?;
    write!(out, "\x1b[?2026l")?;
    out.flush()
}

/// CONSERVATIVE history emission (mu-solo-zellij-blank-band-ptvm).
///
/// Same content as the fast path — one CRLF-scroll + painted row per payload
/// row through the top-margin-1 history region (this is the only primitive
/// that feeds native scrollback, so it cannot be replaced) — but framed so
/// that none of the bead's three suspect mechanisms is exercised:
///
/// - hypothesis (a): no DECSTBM+`CSI T` anywhere (the push-down variant
///   above clears instead of reverse-scrolling);
/// - hypothesis (b): the payload is split into chunks of at most
///   `viewport_top − 1` rows (strictly smaller than the history region), and
///   between chunks the margins are reset (`CSI r`), the cursor is re-homed
///   to the region's bottom row, and the stream is FLUSHED — so zellij's
///   compositor never has to track a scroll burst larger than the margined
///   region, and gets a settled stream boundary between bursts;
/// - hypothesis (c): no `?2026` synchronized-output brackets.
///
/// Cost: visible flicker on large commits and one extra flush per chunk.
/// That trade is the point — contiguous scrollback beats smooth animation
/// under a multiplexer we can't trust with the fancy protocol.
fn emit_insert_conservative<W: Write>(
    out: &mut W,
    buf: &Buffer,
    from_row: u16,
    viewport_top: u16,
    viewport_x: u16,
    viewport_y: u16,
) -> io::Result<()> {
    // At most history-region-height − 1 rows per chunk; minimum 1 so a
    // single-row history region still makes progress.
    let chunk_rows = viewport_top.saturating_sub(1).max(1);
    let total = buf.area.height;
    let mut y = from_row;
    while y < total {
        let end = (y + chunk_rows).min(total);
        write!(out, "\x1b[1;{}r", viewport_top)?;
        queue!(out, MoveTo(0, viewport_top - 1))?;
        for row in y..end {
            queue!(out, Print("\r\n"))?;
            write_buffer_row(out, buf, row)?;
        }
        // Reset margins and flush BETWEEN chunks — the settled boundary is
        // what distinguishes this from the fast path's single burst.
        write!(out, "\x1b[r")?;
        out.flush()?;
        y = end;
    }
    queue!(out, MoveTo(viewport_x, viewport_y))?;
    out.flush()
}

/// Pure math for `set_height`'s grow: how much of `growth` comes from free
/// rows below the viewport vs pushing the history region up. Split out for
/// unit tests.
fn grow_split(growth: u16, free_below: u16) -> (u16, u16) {
    let take_below = growth.min(free_below);
    (take_below, growth - take_below)
}

/// Pure math: of `pushed` rows that just entered native scrollback from the
/// top of a `region_rows`-tall history region holding `resident` history
/// lines at its bottom, how many were history lines. Rows above the resident
/// tail are pre-session terminal content and don't advance
/// `scrollback_committed`.
fn committed_delta_for_push(pushed: usize, region_rows: usize, resident: usize) -> usize {
    let non_history = region_rows.saturating_sub(resident);
    pushed.saturating_sub(non_history)
}

/// FAST region push: scroll the history region (rows 0..viewport_top) up by
/// `amount` via the top-margin CRLF pattern, so the exiting top rows enter
/// native scrollback and `amount` blank rows open at the region's bottom for
/// the growing viewport to claim. This is `emit_insert_fast` with no payload:
/// the CRLF-at-region-bottom scroll is the only primitive that feeds
/// scrollback — DECSTBM+`CSI S` discards exiting rows, which is how the old
/// grow ate the top of the transcript and why the old shrink needed a repair
/// repaint (mu-8oqp).
fn emit_region_push_up_fast<W: Write>(
    out: &mut W,
    viewport_top: u16,
    amount: u16,
) -> io::Result<()> {
    if amount == 0 || viewport_top == 0 {
        return Ok(());
    }
    write!(out, "\x1b[?2026h")?;
    write!(out, "\x1b[1;{}r", viewport_top)?;
    queue!(out, MoveTo(0, viewport_top - 1))?;
    for _ in 0..amount {
        queue!(out, Print("\r\n"))?;
    }
    write!(out, "\x1b[r")?;
    write!(out, "\x1b[?2026l")?;
    out.flush()
}

/// CONSERVATIVE region push: same scroll as the fast variant but framed like
/// `emit_insert_conservative` — chunks strictly smaller than the region,
/// margins reset and stream flushed between chunks, no `?2026` brackets
/// (mu-solo-zellij-blank-band-ptvm hypotheses a–c).
fn emit_region_push_up_conservative<W: Write>(
    out: &mut W,
    viewport_top: u16,
    amount: u16,
) -> io::Result<()> {
    if amount == 0 || viewport_top == 0 {
        return Ok(());
    }
    let chunk_rows = viewport_top.saturating_sub(1).max(1);
    let mut done = 0u16;
    while done < amount {
        let n = chunk_rows.min(amount - done);
        write!(out, "\x1b[1;{}r", viewport_top)?;
        queue!(out, MoveTo(0, viewport_top - 1))?;
        for _ in 0..n {
            queue!(out, Print("\r\n"))?;
        }
        write!(out, "\x1b[r")?;
        out.flush()?;
        done += n;
    }
    Ok(())
}

/// Convert ratatui Color to crossterm Color.
pub(crate) fn to_crossterm_color(color: Color) -> CtColor {
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
// These tests exercise the scrollback_committed invariant, the grow-split /
// committed-delta arithmetic, and the region-push emission byte streams
// (mu-8oqp).  We cannot instantiate a real DynamicViewport here (no TTY), so
// we test the pure helpers directly; the on-screen behavior is validated by
// tests/pty_scrape.rs under a real pty.

#[cfg(test)]
mod tests {
    /// Compute what scrollback_committed should be after insert_before(n_lines)
    /// when history has `history_len` entries and the viewport top is at
    /// `viewport_top` screen rows.  Mirrors the post-insert update in
    /// `insert_before`.
    fn scrollback_committed_after_insert(
        history_len: usize,
        viewport_top: usize,
        gap_rows: usize,
    ) -> usize {
        history_len.saturating_sub(viewport_top - gap_rows)
    }

    use super::{
        committed_delta_for_push, emit_region_push_up_conservative, emit_region_push_up_fast,
        grow_split,
    };

    // ── scrollback_committed invariant ───────────────────────────────────────

    #[test]
    fn scrollback_committed_zero_when_history_fits_in_viewport() {
        // 5 history lines, 20-row viewport region → nothing overflows.
        assert_eq!(scrollback_committed_after_insert(5, 20, 0), 0);
    }

    #[test]
    fn scrollback_committed_counts_overflow() {
        // 50 lines inserted into a 20-row region → 30 lines to scrollback.
        assert_eq!(scrollback_committed_after_insert(50, 20, 0), 30);
    }

    #[test]
    fn scrollback_committed_saturates_at_zero_for_small_history() {
        // viewport is larger than history — no overflow.
        assert_eq!(scrollback_committed_after_insert(3, 20, 0), 0);
    }

    #[test]
    fn scrollback_committed_exact_fit() {
        // Exactly viewport_top lines — boundary: no overflow.
        assert_eq!(scrollback_committed_after_insert(20, 20, 0), 0);
    }

    #[test]
    fn scrollback_committed_one_over_fit() {
        // One line past the viewport top → one line in scrollback.
        assert_eq!(scrollback_committed_after_insert(21, 20, 0), 1);
    }

    // ── committed monotonicity (mu-8oqp panel finding) ───────────────────────

    #[test]
    fn committed_never_decreases_after_snap_and_small_insert() {
        // Maximize exit: everything committed (len 50, committed 50), snap
        // under a blank region (y 19, gap 0). A 3-line insert's naive
        // re-derive would claim committed = 53 − 19 = 34 — resurrecting 16
        // scrollback lines as "resident". The monotonic clamp keeps 50.
        let committed_before = 50usize;
        let naive = scrollback_committed_after_insert(53, 19, 0);
        assert_eq!(naive, 34);
        assert_eq!(committed_before.max(naive), 50);
    }

    // ── grow arithmetic (mu-8oqp) ────────────────────────────────────────────

    #[test]
    fn grow_split_all_from_free_space() {
        // Post-shrink state: plenty of free rows below — no push needed.
        assert_eq!(grow_split(5, 12), (5, 0));
    }

    #[test]
    fn grow_split_partial_push() {
        // 3 free rows below, need 8 → 3 below + 5 pushed.
        assert_eq!(grow_split(8, 3), (3, 5));
    }

    #[test]
    fn grow_split_no_free_space() {
        // Bottom-anchored viewport: everything comes from the push.
        assert_eq!(grow_split(6, 0), (0, 6));
    }

    #[test]
    fn committed_delta_full_region_resident() {
        // Region full of history: every pushed row was a history line.
        assert_eq!(committed_delta_for_push(5, 20, 20), 5);
    }

    #[test]
    fn committed_delta_absorbed_by_pre_session_rows() {
        // 20-row region, 12 resident history lines → 8 pre-session rows on
        // top. Pushing 5 exits only pre-session content.
        assert_eq!(committed_delta_for_push(5, 20, 12), 0);
    }

    #[test]
    fn committed_delta_straddles_boundary() {
        // 8 pre-session rows, push 11 → 3 history lines enter scrollback.
        assert_eq!(committed_delta_for_push(11, 20, 12), 3);
    }

    // ── region-push emission byte streams ────────────────────────────────────

    #[test]
    fn push_up_fast_exact_bytes() {
        let mut out: Vec<u8> = Vec::new();
        emit_region_push_up_fast(&mut out, 20, 3).unwrap();
        let s = String::from_utf8(out).unwrap();
        // Sync bracket, DECSTBM over the history region, park at region
        // bottom, one CRLF per pushed row, reset margins, close bracket.
        assert_eq!(
            s,
            "\x1b[?2026h\x1b[1;20r\x1b[20;1H\r\n\r\n\r\n\x1b[r\x1b[?2026l"
        );
    }

    #[test]
    fn push_up_fast_zero_amount_is_noop() {
        let mut out: Vec<u8> = Vec::new();
        emit_region_push_up_fast(&mut out, 20, 0).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn push_up_conservative_single_chunk_no_sync_brackets() {
        let mut out: Vec<u8> = Vec::new();
        emit_region_push_up_conservative(&mut out, 20, 3).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b[1;20r\x1b[20;1H\r\n\r\n\r\n\x1b[r");
        assert!(
            !s.contains("\x1b[?2026"),
            "conservative path must not use sync brackets"
        );
        assert!(
            !s.contains('T'),
            "conservative path must not reverse-scroll"
        );
    }

    #[test]
    fn push_up_conservative_chunks_stay_smaller_than_region() {
        // Region of 4 rows → chunk cap 3. Pushing 7 must burst as 3+3+1 with
        // margins reset between bursts (ptvm hypothesis b).
        let mut out: Vec<u8> = Vec::new();
        emit_region_push_up_conservative(&mut out, 4, 7).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s.matches("\x1b[1;4r").count(),
            3,
            "expected 3 margin-set bursts"
        );
        assert_eq!(
            s.matches("\x1b[r").count(),
            3,
            "each burst must reset margins"
        );
        assert_eq!(s.matches("\r\n").count(), 7, "all 7 rows must scroll");
    }

    // ── shrink/grow round trip keeps the committed invariant ─────────────────

    #[test]
    fn collapse_then_grow_accounting_round_trip() {
        // New model (chrome pinned): viewport_top 20, 50 lines inserted →
        // committed 30, resident 20. Preview collapse moves the viewport to
        // the bottom and records the vacated rows as GAP — committed and
        // resident are untouched (nothing above moved).
        let committed = scrollback_committed_after_insert(50, 20, 0);
        assert_eq!(committed, 30);
        // Collapse: y 20 → 34, gap 14. Invariant with the gap counted:
        // committed = 50 − (34 − 14) = 30 — unchanged, as it must be.
        assert_eq!(scrollback_committed_after_insert(50, 34, 14), committed);
        // A 3-line insert gap-paints (no scroll): len 53, gap 11.
        // committed = 53 − (34 − 11) = 30 — still unchanged.
        assert_eq!(scrollback_committed_after_insert(53, 34, 11), committed);
        // Next preview grow by 5 consumes gap only: y 29, gap 6.
        // committed = 53 − (29 − 6) = 30. No push, no commit.
        assert_eq!(scrollback_committed_after_insert(53, 29, 6), committed);
        // Grow past the gap pushes the region: 6 more rows than gap →
        // committed advances by exactly the pushed resident rows.
        let resident = 53 - committed;
        let after_push = committed + committed_delta_for_push(6, 23, resident);
        assert_eq!(after_push, 36);
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

    #[test]
    fn notify_journal_entry_is_valid_json_without_body() {
        let line = super::DynamicViewport::journal_notify_line(42, "session.done", 37);
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap_or_else(|e| {
            panic!("notify journal line not valid JSON: {e}\n  line: {line:?}")
        });
        assert_eq!(v["kind"], "notify");
        assert_eq!(v["trigger"], "notify");
        assert_eq!(v["occasion"], "session.done");
        assert_eq!(v["body_len"], 37);
        assert!(
            v.get("body").is_none(),
            "journal should record notification metadata, not popup text"
        );
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

    // ── emission-strategy tests (mu-solo-zellij-blank-band-ptvm) ─────────────
    //
    // The emission paths are free functions over `W: Write`, so the exact
    // byte stream can be captured into a Vec<u8> without a TTY.

    use super::{
        emit_insert_conservative, emit_insert_fast, emit_push_down_conservative,
        emit_push_down_fast, select_emission_strategy, write_buffer_row, EmissionStrategy,
    };
    use crossterm::cursor::MoveTo;
    use crossterm::queue;
    use crossterm::style::Print;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use std::io::Write;

    /// Build a payload buffer with a distinct marker per row so content
    /// parity checks catch row loss/duplication, not just length.
    fn payload(width: u16, height: u16) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        for y in 0..height {
            buf.set_string(0, y, format!("row{y:04}"), Style::default());
        }
        buf
    }

    /// Parse every CSI sequence in `bytes` into (parameter string, final byte).
    fn parse_csi(bytes: &[u8]) -> Vec<(String, u8)> {
        let mut seqs = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                let start = i;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    seqs.push((
                        String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                        bytes[i],
                    ));
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        seqs
    }

    /// Strip every CSI sequence, leaving printed content (+ CR/LF) only.
    fn strip_csi(bytes: &[u8]) -> String {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1; // skip final byte
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    // ── strategy selection ────────────────────────────────────────────────────

    #[test]
    fn strategy_defaults_to_fast_without_zellij() {
        let (s, _) = select_emission_strategy(None, false);
        assert_eq!(s, EmissionStrategy::Fast);
    }

    #[test]
    fn strategy_is_conservative_under_zellij() {
        let (s, reason) = select_emission_strategy(None, true);
        assert_eq!(s, EmissionStrategy::Conservative);
        assert!(
            reason.contains("ZELLIJ"),
            "reason should name the env var: {reason}"
        );
    }

    #[test]
    fn strategy_force_knob_overrides_detection_both_ways() {
        // Force conservative on a bare terminal.
        let (s, r) = select_emission_strategy(Some("1"), false);
        assert_eq!(s, EmissionStrategy::Conservative);
        assert!(r.contains("forced"));
        // Force fast under zellij (live-bisection knob).
        let (s, r) = select_emission_strategy(Some("0"), true);
        assert_eq!(s, EmissionStrategy::Fast);
        assert!(r.contains("forced"));
        // Unrecognized values fall through to detection.
        let (s, _) = select_emission_strategy(Some("yes"), true);
        assert_eq!(s, EmissionStrategy::Conservative);
        let (s, _) = select_emission_strategy(Some("yes"), false);
        assert_eq!(s, EmissionStrategy::Fast);
    }

    // ── fast path: byte-identical to the pre-split emission ──────────────────

    /// The fast path must not regress bare terminals: its byte stream must be
    /// EXACTLY what `insert_before` inlined before the strategy split. This
    /// test reproduces the original emission code verbatim (modulo writing to
    /// a Vec instead of stdout and pushing history) and compares streams.
    #[test]
    fn fast_path_byte_identical_to_legacy_emission() {
        let viewport_top: u16 = 20;
        let (vx, vy): (u16, u16) = (0, 20);
        let buf = payload(12, 35);

        let mut fast: Vec<u8> = Vec::new();
        emit_insert_fast(&mut fast, &buf, 0, viewport_top, vx, vy).unwrap();

        // Verbatim copy of the pre-split insert_before emission lines
        // (history bookkeeping removed — it never wrote to the stream).
        let mut legacy: Vec<u8> = Vec::new();
        write!(legacy, "\x1b[?2026h").unwrap();
        write!(legacy, "\x1b[1;{}r", viewport_top).unwrap();
        queue!(legacy, MoveTo(0, viewport_top - 1)).unwrap();
        for y in 0..buf.area.height {
            queue!(legacy, Print("\r\n")).unwrap();
            write_buffer_row(&mut legacy, &buf, y).unwrap();
        }
        write!(legacy, "\x1b[r").unwrap();
        queue!(legacy, MoveTo(vx, vy)).unwrap();
        write!(legacy, "\x1b[?2026l").unwrap();
        legacy.flush().unwrap();

        assert_eq!(
            fast, legacy,
            "fast-path emission diverged from the pre-strategy-split byte stream"
        );
    }

    #[test]
    fn fast_push_down_byte_identical_to_legacy_emission() {
        let mut fast: Vec<u8> = Vec::new();
        emit_push_down_fast(&mut fast, 10, 40, 5).unwrap();
        // Original inline: region_top = viewport.y + 1, region_bottom = rows.
        let legacy = format!("\x1b[{};{}r\x1b[{}T\x1b[r", 11, 40, 5).into_bytes();
        assert_eq!(fast, legacy);
    }

    /// Sanity: the fast path really does use the mechanisms the conservative
    /// tests below assert the absence of — proves those assertions have teeth.
    #[test]
    fn fast_path_uses_sync_brackets() {
        let buf = payload(12, 35);
        let mut fast: Vec<u8> = Vec::new();
        emit_insert_fast(&mut fast, &buf, 0, 20, 0, 20).unwrap();
        let s = String::from_utf8_lossy(&fast);
        assert!(s.contains("\x1b[?2026h") && s.contains("\x1b[?2026l"));
    }

    // ── conservative path: suspect mechanisms absent ──────────────────────────

    #[test]
    fn conservative_large_insert_has_no_reverse_scroll_and_no_sync() {
        let viewport_top: u16 = 20;
        // Large commit: well over the history region (the bead's 388-line case
        // in miniature).
        let buf = payload(12, 100);
        let mut out: Vec<u8> = Vec::new();
        emit_insert_conservative(&mut out, &buf, 0, viewport_top, 0, viewport_top).unwrap();

        for (params, fin) in parse_csi(&out) {
            assert_ne!(
                fin, b'T',
                "conservative path emitted CSI {params}T (reverse scroll)"
            );
            assert_ne!(
                fin, b'S',
                "conservative path emitted CSI {params}S (margined SU)"
            );
            assert!(
                !params.contains("2026"),
                "conservative path emitted ?2026 sync bracket (params {params:?})"
            );
        }
    }

    #[test]
    fn conservative_push_down_has_no_decstbm_or_reverse_scroll() {
        let mut out: Vec<u8> = Vec::new();
        emit_push_down_conservative(&mut out, 10, 8).unwrap();
        for (params, fin) in parse_csi(&out) {
            assert_ne!(fin, b'T', "push-down emitted CSI T");
            assert_ne!(fin, b'r', "push-down set scroll margins (CSI {params}r)");
        }
        // It should clear exactly the viewport rows (8 clears).
        let clears = parse_csi(&out).iter().filter(|(_, f)| *f == b'K').count();
        assert_eq!(clears, 8);
    }

    // ── conservative path: chunk bound ────────────────────────────────────────

    #[test]
    fn conservative_chunks_never_exceed_history_region() {
        let viewport_top: u16 = 20;
        let buf = payload(12, 100);
        let mut out: Vec<u8> = Vec::new();
        emit_insert_conservative(&mut out, &buf, 0, viewport_top, 0, viewport_top).unwrap();

        // Each chunk opens with DECSTBM on the history region. Between
        // consecutive openings, the number of scrolled rows (CRLFs) must be
        // at most viewport_top − 1 (strictly smaller than the region).
        let s = String::from_utf8_lossy(&out);
        let marker = format!("\x1b[1;{viewport_top}r");
        let chunks: Vec<&str> = s.split(marker.as_str()).skip(1).collect();
        assert!(
            chunks.len() > 1,
            "100-row insert through a 20-row region must chunk"
        );
        for (i, chunk) in chunks.iter().enumerate() {
            let rows = chunk.matches("\r\n").count();
            assert!(
                rows <= (viewport_top - 1) as usize,
                "chunk {i} scrolled {rows} rows; max is {}",
                viewport_top - 1
            );
            assert!(
                chunk.contains("\x1b[r"),
                "chunk {i} did not reset margins before the next chunk"
            );
        }
        // No content loss across chunking: every row scrolled exactly once.
        let total_rows: usize = chunks.iter().map(|c| c.matches("\r\n").count()).sum();
        assert_eq!(total_rows, 100);
    }

    // ── content parity: chunked emission loses nothing ────────────────────────

    #[test]
    fn conservative_and_fast_emit_identical_content() {
        let viewport_top: u16 = 20;
        let buf = payload(12, 100);

        let mut fast: Vec<u8> = Vec::new();
        emit_insert_fast(&mut fast, &buf, 0, viewport_top, 0, viewport_top).unwrap();
        let mut cons: Vec<u8> = Vec::new();
        emit_insert_conservative(&mut cons, &buf, 0, viewport_top, 0, viewport_top).unwrap();

        // With escape framing stripped, both paths must print exactly the
        // same characters in the same order — the strategies may only differ
        // in framing, never in content.
        assert_eq!(
            strip_csi(&fast),
            strip_csi(&cons),
            "conservative emission altered the printed content"
        );
        // And the content actually contains the distinct row markers.
        let text = strip_csi(&cons);
        assert!(text.contains("row0000") && text.contains("row0099"));
    }
}
