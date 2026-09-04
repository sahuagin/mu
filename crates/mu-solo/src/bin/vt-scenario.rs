//! Deterministic `DynamicViewport` scenario driver — a miniature in-repo
//! mu-vt-probe for running the mu-8oqp scenarios against a REAL terminal
//! by hand (`cargo run -p mu-solo --bin vt-scenario -- collapse`). The
//! automated check of the same scenarios is `tests/vt_scrape.rs`, which
//! drives the viewport headless and parses its output with vt100; keep the
//! two scenario lists in step.

use mu_solo::viewport::DynamicViewport;
use ratatui::style::Style;

fn insert(vp: &mut DynamicViewport, from: usize, n: usize) -> std::io::Result<()> {
    vp.insert_before(n as u16, |buf| {
        for i in 0..n {
            // Distinct, greppable marker per logical line.
            let label = format!("L{:03} {}", from + i, "·".repeat(24));
            buf.set_string(0, i as u16, label, Style::default());
        }
    })
}

fn main() -> std::io::Result<()> {
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "collapse".into());
    let mut vp = DynamicViewport::new(10, None)?;
    vp.snap_to_bottom()?;
    match scenario.as_str() {
        // The operator's failing session shape (mu-8oqp, 2026-08-03):
        // transcript fills the screen, the live preview grows tall, the turn
        // commits while grown, the preview collapses, the next turn arrives.
        "collapse" => {
            insert(&mut vp, 1, 30)?;
            vp.set_height(20)?;
            insert(&mut vp, 31, 12)?;
            vp.set_height(6)?;
            insert(&mut vp, 43, 3)?;
        }
        // Collapse with NO follow-up insert: the screen must still show a
        // contiguous transcript directly above the (blank) viewport.
        "collapse-idle" => {
            insert(&mut vp, 1, 30)?;
            vp.set_height(20)?;
            insert(&mut vp, 31, 12)?;
            vp.set_height(6)?;
        }
        // Reconverge with an insert SMALLER than the freed space: the gap
        // rows must be painted in place, not scrolled into the transcript.
        "small-insert-after-collapse" => {
            insert(&mut vp, 1, 30)?;
            vp.set_height(20)?;
            insert(&mut vp, 31, 12)?;
            vp.set_height(4)?;
            insert(&mut vp, 43, 2)?;
            insert(&mut vp, 45, 2)?;
        }
        other => panic!("unknown scenario: {other}"),
    }
    Ok(())
}
