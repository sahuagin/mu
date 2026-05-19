# Design: multi-line prompt growing upward (mu-o1y7 phase 3g)

## Summary

Replace the single-row scrolling input in `render_inline_session_detail` (main.rs:2894)
with a multi-row input region that grows upward as the user types newlines (Alt-Enter).
Growth is physical: the ratatui `Viewport::Inline(N)` height increases, pushing the
terminal frame boundary up and naturally extending multiplexer scrollback above it.
No transcript-rendering changes are needed — transcript already lives in scrollback
via `insert_before`.

---

## Data model changes

**One new field on App: `terminal_cols: u16`.**

The resize trigger (computing how many visual rows the prompt needs) requires knowing
terminal width. The run loop already queries `terminal.size()?.width` inside
`emit_transcript_delta_inline` (main.rs:2860); we extend it to also store the width
in `App.terminal_cols` each tick. Default: 80.

No `prompt_vscroll` field is needed. Vertical scroll within the input region is
derived purely from cursor position at render time — the cursor must always be visible,
so the scroll offset is `f(cursor_vrow, cap_rows)`.

`prompt_buffer` and `prompt_cursor` are unchanged. Alt-Enter already inserts `\n`
into `prompt_buffer` (main.rs:1243-1248); `prompt_cursor` already counts in chars.
The new rendering code consumes the same existing fields.

---

## Algorithm: building the visual rows

```
prefix     = " > "   // first visual row only
cont       = "   "   // continuation rows (same width as prefix)
prefix_w   = 3
avail      = max(1, area.width as usize − prefix_w)

logical_lines: Vec<&str> = prompt_buffer.split('\n')

visual_rows: Vec<String> = []
for logical_line in logical_lines:
    chars = logical_line.chars().collect::<Vec<char>>()
    if chars.is_empty():
        visual_rows.push("")          // empty logical line → 1 empty row
    else:
        start = 0
        while start < chars.len():
            end = min(start + avail, chars.len())
            visual_rows.push(chars[start..end].iter().collect())
            start = end

if visual_rows.is_empty():
    visual_rows.push("")              // always at least 1 row
```

Each `visual_rows[i]` is the raw content substring (no prefix). The prefix is
prepended at render time: `" > " + visual_rows[0]`, `"   " + visual_rows[i≥1]`.

**Why ` > ` only on row 0, not on each logical-line start?**
Simpler: the alternative (distinct prefix per logical-line-first-row vs. wrap row)
requires tracking row type through the flat list. Since the bordered region makes
the input area self-evident, per-row prefix distinction adds complexity for no
real UX benefit. Open question §1 revisits the upper separator.

---

## Algorithm: cursor position in (vrow, vcol) terms

Given `cursor_char = app.prompt_cursor.min(prompt_buffer.chars().count())`:

```
chars_seen = 0
cursor_vrow = 0   // absolute visual row (0-indexed from top of buffer)
cursor_vcol = 0   // column within that visual row

for (li, logical_line) in logical_lines.enumerate():
    lc = logical_line.chars().count()

    if cursor_char <= chars_seen + lc:
        // cursor is inside this logical line (or at its end)
        col_in_line = cursor_char − chars_seen

        // visual rows contributed by all preceding logical lines
        preceding_vrows = visual_rows.iter()
                              .take(index_of_first_vrow_for_logical_line(li))
                              .count()
        // OR equivalently: sum ceil(max(1, lc_i) / avail) for each li_i < li

        cursor_vrow = preceding_vrows + col_in_line / avail
        cursor_vcol = col_in_line % avail
        break

    chars_seen += lc + 1     // +1 for the consumed '\n'
```

**Newline boundary**: cursor at the `\n` position lands at `col_in_line = lc`
(one past the last character of this logical line, before the split point).
If `lc % avail == 0` and `lc > 0`, this puts the cursor at column 0 of a
new visual wrap-row within the same logical line — which is visually
"after the last character." Acceptable. If that's undesired, treat `col_in_line == lc`
as "cursor is at logical-line end" and clamp to `vcol = lc % avail`. Either
choice is consistent; the visual difference is only for exact-wrap-width lines.

---

## Algorithm: vertical scroll

```
total_vrows = visual_rows.len()
input_rows  = area.height.saturating_sub(2) as usize   // − separator − footer

vscroll =
    if total_vrows <= input_rows:
        0
    else:
        // keep cursor at or before the last visible row
        let floor = cursor_vrow.saturating_sub(input_rows − 1)
        let ceil  = total_vrows.saturating_sub(input_rows)
        floor.min(ceil)
```

This keeps the cursor on the last visible row when typing at the end (natural typing
behavior), and allows scrolling toward the start when cursor moves backward.

---

## Viewport resize (the actual "grows upward" mechanism)

The ratatui inline viewport is `Viewport::Inline(N)` configured at terminal creation.
To change N, the run loop must rebuild the terminal via the existing
`pending_mode_change` → `RunOutcome::ModeChange` path (main.rs:4232-4234).

**Trigger**: after any keystroke that modifies `prompt_buffer`, compute:

```
total_vrows        = compute_visual_rows(prompt_buffer, terminal_cols).len()
cap_input_rows     = max(1, terminal_rows / 3)   // ~ 1/3 of terminal height
needed_input_rows  = min(total_vrows, cap_input_rows).max(1)
needed_inline_h    = needed_input_rows + 2        // + separator + footer
                     // +3 if upper separator is added (open question §1)

current_inline_h   = match current_mode {
                         Inline(n) => n,
                         _         => 0,   // shouldn't happen in F3 path
                     }

if needed_inline_h != current_inline_h:
    app.pending_mode_change = Some(ViewportMode::Inline(needed_inline_h))
```

**Timing**: the current run loop checks `pending_mode_change` AFTER `terminal.draw`
(main.rs:4232). This means a ≤250 ms lag between keystroke and resize. To halve
this, move the check to BEFORE `terminal.draw` at the start of each iteration —
the resize then happens on the next loop start, before the next render. See open
question §3.

**Terminal height for cap**: `App.terminal_cols` solves width. For height, query
`terminal.size()?.height` in the same run-loop step and store `App.terminal_rows: u16`.

**Collapse on submit**: `on_key` already calls `self.prompt_buffer.clear()` on
Enter/submit (main.rs:921). After clear, `total_vrows=1`, so `needed_inline_h=3`,
and `pending_mode_change` triggers a shrink back to the minimum viewport.

---

## Cursor positioning

Today's call is at **main.rs:3044**:

```rust
if let Some(pos) = cursor_pos {
    f.set_cursor_position(pos);
}
```

For multi-line, `cursor_pos` is set as:

```
cursor_vrow_in_viewport = cursor_vrow − vscroll   // 0-indexed within visible frame
cursor_x = area.x + (prefix_w + cursor_vcol) as u16
cursor_y = area.y + cursor_vrow_in_viewport as u16

// Note: prefix_w is always 3 regardless of which visual row we're on,
// because all rows (first and continuation) have a 3-wide prefix slot.
// cont = "   " has the same width as " > ".

cursor_pos = Some(Position { x: cursor_x, y: cursor_y })
```

The `cursor_y` formula is now dynamic (depends on `vscroll` and `cursor_vrow`),
whereas today it is the fixed `area.y + input_y_offset` (main.rs:2938).

---

## Edge cases

- **Single-line prompt, no wrapping** (regression path): `total_vrows=1`, `vscroll=0`,
  `cursor_vrow=0`, `cursor_vcol=col_in_line`. Layout identical to today's 3-row
  inline viewport (height=3). The ` > ` prefix + content + cursor computation
  produce the same visual result. The only change is that the now-dead blank
  placeholder rows at the top are gone (there are no blank rows in the new layout).

- **Empty prompt, cursor at position 0**: after split on `\n`, `[""]`. `visual_rows=[""]`.
  `cursor_char=0`, `chars_seen=0`, `lc=0`. Condition `0 <= 0+0 = 0` → true.
  `col_in_line=0`, `cursor_vrow=0`, `cursor_vcol=0`. Cursor at `(area.x+prefix_w, area.y)`.

- **Prompt ending with `\n`** (e.g., "abc\n"): split → `["abc", ""]`.
  `visual_rows=["abc", ""]`. Cursor at position 4 (after `\n`):
  `chars_seen=0` for "abc" (lc=3). `4 > 3` → move to next. `chars_seen=4` for "" (lc=0).
  `4 <= 4+0 = 4` → true. `col_in_line=0`, `cursor_vrow=1`, `cursor_vcol=0`.
  The trailing empty row renders (with `"   "` prefix and empty content) and the
  cursor appears there. This is the expected behavior: typing after a final newline
  puts the cursor on a new empty line.

- **Cursor past end of buffer**: `cursor = app.prompt_cursor.min(chars.len())` clamps
  it before the algorithm runs — identical to the current single-line guard at
  main.rs:2918. No special case needed.

- **Wider-than-viewport rows** (logical line wider than `avail`): the avail-chunking
  wraps the row into multiple visual rows. No horizontal scroll in multi-line mode —
  horizontal scroll is the OLD single-line behavior only. The cursor correctly
  lands on the wrapped row via `col_in_line / avail` and `col_in_line % avail`.

- **Viewport too small** (`area.height < 3`): the existing guard at main.rs:2896-2904
  bails with a placeholder line. With an upper separator (open question §1), the
  guard threshold becomes `< 4`. Either way, the guard precedes all other logic and
  is unchanged in structure.

- **Cap hit** (`total_vrows > cap_rows`): `vscroll` activates. The viewport height
  is pinned at `cap_input_rows + 2` (or +3). The prompt scrolls internally. The
  operator sees the most recently typed content; cursor always visible. This matches
  the "Beyond the cap, scroll within the input region" requirement from mu-o1y7.

---

## Test plan

Unit tests for the two pure functions (`compute_visual_rows`, `find_cursor_position`):

1. Empty buffer → `visual_rows=[""]`, cursor at `(vrow=0, vcol=0)`
2. Single char "a", cursor=0 → `visual_rows=["a"]`, cursor at `(0, 0)`
3. Single char "a", cursor=1 → `visual_rows=["a"]`, cursor at `(0, 1)` (after 'a')
4. Single line exactly `avail` wide → 1 visual row, cursor at end = `(0, avail)`
5. Single line `avail+1` wide → 2 visual rows, cursor at end = `(1, 0)`
6. `"abc\nde"`, cursor=0 → vrow=0, vcol=0 (start of "abc")
7. `"abc\nde"`, cursor=3 → vrow=0, vcol=3 (end of "abc", before `\n`)
8. `"abc\nde"`, cursor=4 → vrow=1, vcol=0 (start of "de")
9. `"abc\nde"`, cursor=6 → vrow=1, vcol=2 (end of "de")
10. `"abc\n"`, cursor=4 → vrow=1, vcol=0 (cursor on trailing empty row)
11. Long buffer exceeding cap: `vscroll > 0`, cursor in `[vscroll, vscroll+cap-1]`
12. Long buffer, cursor at top of buffer: `vscroll=0`, cursor visible on row 0
13. Single-line buffer, avail=80, content "hello" → identical to current horizontal path
    (regression check: `visual_rows=["hello"]`, same cursor position)

Integration tests (render output checks):
14. Render with empty buffer: exactly 3 rows of output (input + sep + footer), no blank top rows
15. Render with 2-line buffer: exactly 4 rows (2 input + sep + footer)
16. Render with buffer hitting cap: height capped, vscroll active

---

## What's out of scope

- **Transcript rendering changes**: `emit_transcript_delta_inline` and `render_transcript_lines`
  are unchanged. The transcript already scrolls off the top correctly.
- **Upper separator implementation**: flagged as open question §1; if the operator
  decides to add it, it is a +1 row addition with no algorithmic complexity.
- **User-driven scroll within the input region**: no Ctrl-Up/Ctrl-Down to scroll
  inside the input area. The cursor (moved by existing arrow-key bindings) implicitly
  drives scroll.
- **Color/styling pass**: the multi-line region uses the same colors as the current
  single-line input. A styling iteration comes later per mu-o1y7 notes.
- **F1/F2/F4-F9 viewport changes**: this is F3 Inline mode only.
- **Mouse selection in the input region**: deferred per mu-o1y7 Out-of-scope.
- **Windows ConPTY / non-FreeBSD platforms**: not a target.
- **`$EDITOR` integration changes** (Ctrl-X Ctrl-E, mu-82l): the editor handoff
  operates on `prompt_buffer` directly and is unaffected by rendering changes.

---

## Open questions for the orchestrator

**Q1 — Upper separator**: The mu-o1y7 notes say "Bordered open area with thin ─── rules
above AND below the editable region." Should the topmost row of the inline viewport be a
`─────` separator? This would change the height formula from `needed_input_rows + 2` to
`needed_input_rows + 3` and add a `Line::from("─".repeat(...))` as row 0. Without
guidance I'll implement WITHOUT the upper separator (keeps the formula simpler and
the visual boundary is already implicit at the scrollback/frame edge), but will add
it immediately if the operator confirms it's wanted.

**Q2 — Continuation prefix**: Current design uses `" > "` on row 0, `"   "` on all
subsequent rows. An alternative is `" > "` on the first visual row of each LOGICAL
line, `"   "` on wrapped continuation rows — this visually shows where logical lines
begin. Recommend the simpler approach (row 0 only) unless the operator prefers the
per-logical-line marker.

**Q3 — Resize timing**: Moving `pending_mode_change` check to BEFORE `terminal.draw`
in the run loop (rather than after, as today) halves the visual lag. This is a
2-line code change. Recommend doing it as part of this iteration since it's zero
risk. But it changes the run loop structure, so flagging for explicit approval.

**Q4 — `terminal_rows` storage**: To compute the cap, we need terminal height.
Recommend adding both `terminal_cols: u16` and `terminal_rows: u16` to App,
updated from `terminal.size()?` in the run loop each tick (alongside the existing
`terminal.size()?.width` call in `emit_transcript_delta_inline`). Alternatively,
derive the cap from the current `Inline(N)` and a hard-coded fraction, but that
requires knowing the full terminal height independently of the frame height.
