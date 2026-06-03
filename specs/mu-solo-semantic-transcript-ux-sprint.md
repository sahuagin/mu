# mu-solo semantic transcript / block-copy UX sprint

Status: draft sprint plan, first implementation slice in this branch.

## Problem

Fullscreen TUIs make the live screen prettier, but they often make it harder to get data back out. Claude Code fullscreen is the cautionary example: mouse scroll works, but copying more than one screen becomes chunk-copying or an out-of-band dump command. Codex's better pattern is a separate plaintext scrollback/export projection.

mu-solo should not make the screen the buffer. The durable session/event model is the buffer; the screen is one projection over it.

## Core invariant

**The screen is never the buffer.**

mu-solo maintains semantic transcript/block state independent of the ratatui/crossterm rendered cells. Copy/search/export operate on that semantic state, not on terminal selection.

## User-visible goals

1. Plain transcript export
   - Write the current session transcript to a file.
   - Suitable for `less`, `$EDITOR`, grep, attaching to another prompt, or human copy.

2. Semantic copy/yank
   - Copy last block, last assistant answer, last user prompt, or whole session.
   - Prefer configured/system clipboard command; fall back sanely.

3. Block model foundation
   - Store completed user/assistant/system/tool-visible blocks as typed semantic records.
   - This unlocks later block cursor navigation without re-parsing rendered scrollback.

4. Toad-style block UX later
   - Alt-Up/Alt-Down block cursor.
   - Enter block action menu.
   - Copy to clipboard / copy to prompt.
   - Maximize block pager.
   - Select block range and yank.

5. Scroll/live behavior later
   - Mouse/PgUp scroll releases live anchor.
   - New output indicator while scrolled up.
   - End reattaches to live tail.

## Suggested bead breakdown

### P2 epic: mu-solo semantic transcript and block-copy UX

Design and implement mu-solo UX improvements inspired by Toad/Codex/Claude fullscreen lessons: durable semantic transcript projection, block cursor/navigation, copy/yank/export actions, maximized block pager, sane scroll anchoring, and later sidebar/pane polish.

### P2 task: semantic transcript model + export/copy commands

Add an in-memory semantic transcript alongside rendered scrollback. Record user prompts, assistant turns, sidecar `/btw` turns, and local status/help/error blocks where useful. Add:

- `/transcript [PATH]` — write plaintext transcript to PATH or a temp file and print the path.
- `/copy [last|assistant|user|all]` — copy semantic content to clipboard.

Acceptance:

- Copy/export do not scrape terminal cells.
- Whole-session export works even when content is no longer visible.
- Clipboard uses Unix command path (`xclip`/configured later), with file fallback for failures.

### P2 task: block cursor and action menu

Add a selected-block cursor independent of prompt cursor.

Proposed keys:

- Alt-Up / Alt-Down: previous/next block
- Enter: block action menu
- c: copy current block
- p: copy current block to prompt
- m: maximize current block
- Esc: clear block cursor / prompt depending focus

Acceptance:

- Can select and copy a prior answer without mouse selection.
- Can copy a code fence/tool output semantically once block subitems exist.

### P3 task: maximized block pager

Focused single-block view for long assistant/tool output.

Acceptance:

- Scroll/search/copy inside a long block.
- Does not rely on terminal scrollback.

### P3 task: live-scroll anchoring

Make viewport anchoring explicit.

Acceptance:

- Streaming output follows tail only while anchored.
- User scroll releases anchor.
- Status line shows new output below.
- End reattaches.

### P3 task: sidebar/pane polish

Optional sidebar for sessions/workers/mailbox/status after the transcript/block foundation exists.

## First implementation slice in this branch

Implement the P2 transcript/export/copy command foundation. This is deliberately narrow: no pane/layout redesign yet.
