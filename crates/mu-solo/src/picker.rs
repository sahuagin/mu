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
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
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

fn picker_loop(
    out: &mut Stdout,
    title: &str,
    items: &[String],
    initial: usize,
) -> Result<Option<usize>> {
    let mut idx = initial.min(items.len().saturating_sub(1));
    loop {
        draw(out, title, items, idx)?;
        match event::read()? {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match (modifiers, code) {
                (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Ok(None),
                (_, KeyCode::Esc) => return Ok(None),
                (_, KeyCode::Enter) => return Ok(Some(idx)),
                (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
                    idx = idx.saturating_sub(1);
                }
                (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
                    idx = (idx + 1).min(items.len() - 1);
                }
                (_, KeyCode::Home) | (_, KeyCode::Char('g')) => idx = 0,
                (_, KeyCode::End) | (_, KeyCode::Char('G')) => idx = items.len() - 1,
                // Number keys 1-9 jump-select.
                (_, KeyCode::Char(c @ '1'..='9')) => {
                    let pick = c.to_digit(10).unwrap() as usize - 1;
                    if pick < items.len() {
                        idx = pick;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn draw(out: &mut Stdout, title: &str, items: &[String], idx: usize) -> Result<()> {
    queue!(
        out,
        Clear(ClearType::All),
        cursor::MoveTo(0, 0),
        SetForegroundColor(Color::Cyan),
        Print(format!("── {title} ──")),
        ResetColor,
    )?;
    queue!(out, cursor::MoveToNextLine(2))?;
    for (i, item) in items.iter().enumerate() {
        queue!(out, cursor::MoveToColumn(2))?;
        if i == idx {
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
    queue!(
        out,
        cursor::MoveToNextLine(1),
        cursor::MoveToColumn(2),
        SetForegroundColor(Color::DarkGrey),
        Print("↑/↓ or j/k navigate · 1-9 jump · Enter commit · Esc cancel"),
        ResetColor,
    )?;
    out.flush()?;
    Ok(())
}
