//! Modal list-picker drawn directly via crossterm.
//!
//! Why crossterm-direct rather than a second ratatui Terminal: the
//! picker is a transient overlay, not part of the inline rendering
//! contract. Using crossterm's `EnterAlternateScreen` saves the main
//! screen state automatically (the terminal emulator does it via
//! escape codes); `LeaveAlternateScreen` restores it on close. We
//! don't fight ratatui's inline viewport, don't reconstruct any
//! Terminal, and the picker UI is ~80 lines instead of plumbing a
//! second ratatui pipeline.
//!
//! Caveat: while the picker blocks on `event::read`, the run loop
//! doesn't drain notifications. The mpsc channel from the daemon's
//! stdout reader queues them up; on picker close, drain_notifications
//! processes the backlog in order. For modal use this is the right
//! behaviour — the operator is choosing, not waiting on output.

use std::io::{stdout, Stdout, Write};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor},
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

/// Run a modal picker over `items`, starting with `initial` highlighted.
/// Returns `Some(idx)` on Enter, `None` on Esc or Ctrl-C.
///
/// Empty `items` returns `None` without entering the alt screen.
pub fn run_picker(title: &str, items: &[String], initial: usize) -> Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, cursor::Hide)?;

    let result = picker_loop(&mut out, title, items, initial);

    // Always leave alt screen even if the loop errored, so the user
    // doesn't get stranded in a half-cleared modal.
    let _ = execute!(out, cursor::Show, LeaveAlternateScreen);
    result
}

/// Recompute the matched indices for `filter` (case-insensitive substring).
/// Empty filter matches everything. Pure so it's unit-testable.
fn refilter(items: &[String], filter: &str, matches: &mut Vec<usize>) {
    matches.clear();
    if filter.is_empty() {
        matches.extend(0..items.len());
        return;
    }
    let needle = filter.to_lowercase();
    for (i, item) in items.iter().enumerate() {
        if item.to_lowercase().contains(&needle) {
            matches.push(i);
        }
    }
}

fn picker_loop(
    out: &mut Stdout,
    title: &str,
    items: &[String],
    initial: usize,
) -> Result<Option<usize>> {
    // Type-to-filter: printable chars narrow the list, arrows move the cursor
    // within the matches, Enter commits the highlighted ORIGINAL index. This
    // is the "works like other pickers" model — no vim nav / digit shortcuts,
    // because those characters now feed the filter.
    let mut filter = String::new();
    let mut matches: Vec<usize> = (0..items.len()).collect();
    // Cursor within `matches`; start on the initial selection (empty filter,
    // so match index == original index).
    let mut idx = initial.min(items.len().saturating_sub(1));
    loop {
        draw(out, title, items, &matches, idx, &filter)?;
        if let Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = event::read()?
        {
            // Under the keyboard-enhancement flags the app pushes, a keypress
            // also emits a Release; ignore it so typing isn't doubled.
            if kind == KeyEventKind::Release {
                continue;
            }
            match (modifiers, code) {
                (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Ok(None),
                (_, KeyCode::Esc) => return Ok(None),
                (_, KeyCode::Enter) => {
                    if let Some(&orig) = matches.get(idx) {
                        return Ok(Some(orig));
                    }
                }
                (_, KeyCode::Up) => idx = idx.saturating_sub(1),
                (_, KeyCode::Down) => {
                    if !matches.is_empty() {
                        idx = (idx + 1).min(matches.len() - 1);
                    }
                }
                (_, KeyCode::Backspace) => {
                    if filter.pop().is_some() {
                        refilter(items, &filter, &mut matches);
                        idx = 0;
                    }
                }
                (m, KeyCode::Char(c))
                    if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
                {
                    filter.push(c);
                    refilter(items, &filter, &mut matches);
                    idx = 0;
                }
                _ => {}
            }
        }
    }
}

fn draw(
    out: &mut Stdout,
    title: &str,
    items: &[String],
    matches: &[usize],
    idx: usize,
    filter: &str,
) -> Result<()> {
    queue!(
        out,
        Clear(ClearType::All),
        cursor::MoveTo(0, 0),
        SetForegroundColor(Color::Cyan),
        Print(format!("── {title} ──")),
        ResetColor,
    )?;
    // Filter line: what's been typed so far.
    queue!(
        out,
        cursor::MoveToNextLine(1),
        cursor::MoveToColumn(2),
        SetForegroundColor(Color::DarkGrey),
        Print("filter: "),
        ResetColor,
        Print(filter),
    )?;
    queue!(out, cursor::MoveToNextLine(2))?;
    if matches.is_empty() {
        queue!(
            out,
            cursor::MoveToColumn(2),
            SetForegroundColor(Color::DarkGrey),
            Print("(no matches)"),
            ResetColor,
            cursor::MoveToNextLine(1),
        )?;
    } else {
        for (row, &orig) in matches.iter().enumerate() {
            let item = &items[orig];
            queue!(out, cursor::MoveToColumn(2))?;
            if row == idx {
                queue!(
                    out,
                    SetForegroundColor(Color::Black),
                    SetBackgroundColor(Color::Cyan),
                    Print(format!(" ▶ {item} ")),
                    ResetColor,
                )?;
            } else {
                queue!(out, Print(format!("   {item}")))?;
            }
            queue!(out, cursor::MoveToNextLine(1))?;
        }
    }
    queue!(
        out,
        cursor::MoveToNextLine(1),
        cursor::MoveToColumn(2),
        SetForegroundColor(Color::DarkGrey),
        Print("type to filter · ↑/↓ select · Enter commit · Esc cancel"),
        ResetColor,
    )?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::refilter;

    fn items() -> Vec<String> {
        ["gpt-5.5", "gpt-oss:20b", "claude-opus-4-8", "qwen3-coder"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn empty_filter_matches_all() {
        let items = items();
        let mut m = Vec::new();
        refilter(&items, "", &mut m);
        assert_eq!(m, vec![0, 1, 2, 3]);
    }

    #[test]
    fn substring_filter_is_case_insensitive() {
        let items = items();
        let mut m = Vec::new();
        refilter(&items, "GPT", &mut m);
        assert_eq!(m, vec![0, 1]);
        refilter(&items, "opus", &mut m);
        assert_eq!(m, vec![2]);
        refilter(&items, "zzz", &mut m);
        assert!(m.is_empty());
    }
}
