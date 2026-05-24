//! Quick smoke test for DynamicViewport grow/shrink behavior.
//! Run with: cargo run -p mu-solo --example viewport_test

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use mu_solo::viewport::DynamicViewport;

fn main() -> io::Result<()> {
    enable_raw_mode()?;

    // Print some "scrollback" content first
    for i in 1..=10 {
        println!("  scrollback line {i}");
    }

    let mut vp = DynamicViewport::new(3)?;
    let mut height: u16 = 3;
    let mut msg = String::from("Press Up/Down to grow/shrink. q to quit.");

    loop {
        // Render
        let area = vp.area();
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(vec![
            Span::styled(" > ", Style::default().fg(Color::Cyan)),
            Span::raw(msg.clone()),
        ]));
        // Fill remaining with blank or extra prompt lines
        while lines.len() < area.height as usize - 1 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!(" height={height} viewport={:?}", area),
            Style::default().fg(Color::DarkGray),
        )));

        let para = Paragraph::new(lines);
        vp.render(para);
        vp.flush()?;

        // Handle input
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Up => {
                        height = (height + 1).min(20);
                        vp.set_height(height)?;
                        msg = format!("Grew to {height}");
                    }
                    KeyCode::Down => {
                        height = height.saturating_sub(1).max(3);
                        vp.set_height(height)?;
                        msg = format!("Shrunk to {height}");
                    }
                    KeyCode::Char(c) => {
                        msg.push(c);
                    }
                    _ => {}
                }
            }
        }
    }

    drop(vp);
    disable_raw_mode()?;
    println!("\nDone.");
    Ok(())
}
