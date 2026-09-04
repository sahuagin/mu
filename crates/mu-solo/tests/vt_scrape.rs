//! Screen validation for the mu-8oqp viewport rework, through an
//! INDEPENDENT terminal model.
//!
//! Two prior fixes in this bug family (mu-solo-zellij-blank-band-ptvm, PR
//! #502) were green on their own bookkeeping tests and wrong on the actual
//! screen; these tests assert on what a terminal would display, not on what
//! the code believes it did. Each scenario drives a headless
//! `DynamicViewport` (fixed 80x24, escape output captured to memory, no
//! terminal queries) and feeds the bytes it emitted to the `vt100` crate.
//!
//! This used to run a `vt-scenario` child under a pty and scrape the master.
//! For what these tests check — the emission of `insert_before` and
//! `set_height` — the pty bought nothing: those paths only write escape
//! sequences. Reading the master, though, raced the child's exit, so under
//! load the tail of the stream was lost and the parsed screen still showed
//! the pre-collapse rows (mu-dsggq). In-process there is no transport and no
//! timing: the bytes are the bytes. What the pty run did cover and this one
//! does not is the production constructor, `DynamicViewport::new`, which
//! queries the terminal size and cursor position (a CSI 6n round trip) and
//! scrolls to make room; `new_headless` skips that. That construction path is
//! exercised by hand with `src/bin/vt-scenario` against a real terminal,
//! which keeps the same scenarios.
//!
//! ## Scope: the visible screen, anchored at row 0
//!
//! vt100 feeds its scrollback only on FULL-SCREEN scrolls
//! (`!scroll_region_active()` in its grid.rs), while mu-solo's emission
//! relies on kitty/xterm treating a TOP-ANCHORED region scroll as
//! scrollback-feeding. Scrollback-side continuity therefore cannot be
//! asserted through this crate and remains the external mu-vt-probe's job.
//! The discriminating visible assertion instead: once the transcript has
//! filled the screen, content is CONTIGUOUS FROM ROW 0 — the historic band
//! artifact blanks the top rows (old shrink repaint), and the historic
//! duplication artifact repeats a marker; both fail these checks. Verified
//! discriminating by running this suite against the pre-fix viewport.rs.

use mu_solo::viewport::{DynamicViewport, EmissionStrategy};
use ratatui::style::Style;

const COLS: u16 = 80;
const ROWS: u16 = 24;
const INITIAL_HEIGHT: u16 = 10;

fn insert(vp: &mut DynamicViewport, from: usize, n: usize) {
    vp.insert_before(n as u16, |buf| {
        for i in 0..n {
            // Distinct, greppable marker per logical line.
            let label = format!("L{:03} {}", from + i, "·".repeat(24));
            buf.set_string(0, i as u16, label, Style::default());
        }
    })
    .expect("insert_before");
}

/// The operator's failing session shapes (mu-8oqp, 2026-08-03). Kept in
/// step with `src/bin/vt-scenario.rs`, which runs the same sequences
/// against a real terminal.
fn run_scenario(vp: &mut DynamicViewport, scenario: &str) {
    match scenario {
        // Transcript fills the screen, the live preview grows tall, the turn
        // commits while grown, the preview collapses, the next turn arrives.
        "collapse" => {
            insert(vp, 1, 30);
            vp.set_height(20).expect("grow");
            insert(vp, 31, 12);
            vp.set_height(6).expect("collapse");
            insert(vp, 43, 3);
        }
        // Collapse with NO follow-up insert: the screen must still show a
        // contiguous transcript directly above the (blank) viewport.
        "collapse-idle" => {
            insert(vp, 1, 30);
            vp.set_height(20).expect("grow");
            insert(vp, 31, 12);
            vp.set_height(6).expect("collapse");
        }
        // Reconverge with an insert SMALLER than the freed space: the gap
        // rows must be painted in place, not scrolled into the transcript.
        "small-insert-after-collapse" => {
            insert(vp, 1, 30);
            vp.set_height(20).expect("grow");
            insert(vp, 31, 12);
            vp.set_height(4).expect("collapse");
            insert(vp, 43, 2);
            insert(vp, 45, 2);
        }
        other => panic!("unknown scenario: {other}"),
    }
}

/// Run one scenario headless; return the visible screen rows (trimmed) as
/// an independent terminal would display them.
fn scrape(scenario: &str, force_conservative: bool) -> Vec<String> {
    let strategy = if force_conservative {
        EmissionStrategy::Conservative
    } else {
        EmissionStrategy::Fast
    };
    let mut vp = DynamicViewport::new_headless(COLS, ROWS, INITIAL_HEIGHT, strategy);
    vp.snap_to_bottom().expect("snap_to_bottom");
    run_scenario(&mut vp, scenario);

    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(&vp.headless_output());
    (0..ROWS)
        .map(|r| {
            (0..COLS)
                .map(|c| {
                    parser
                        .screen()
                        .cell(r, c)
                        .map(|cell| cell.contents())
                        .unwrap_or_default()
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Marker numbers in screen order (top to bottom).
fn markers(rows: &[String]) -> Vec<u32> {
    rows.iter()
        .filter_map(|r| {
            let idx = r.find('L')?;
            r[idx + 1..idx + 4].parse::<u32>().ok()
        })
        .collect()
}

/// The discriminating screen shape after a transcript has filled the screen:
/// markers `first..=last`, contiguous FROM ROW 0, each exactly once, in
/// order, and nothing but blank viewport rows below them.
fn assert_window(rows: &[String], first: u32, last: u32, label: &str) {
    let ms = markers(rows);
    let expected: Vec<u32> = (first..=last).collect();
    assert_eq!(
        ms,
        expected,
        "{label}: expected markers L{first:03}..=L{last:03} contiguous and ordered\nscreen:\n{}",
        rows.join("\n")
    );
    // Band check: the block must start at ROW 0 (the historic artifact blanks
    // the top rows) and be gapless through its last row.
    let n = expected.len();
    for (i, row) in rows.iter().take(n).enumerate() {
        assert!(
            row.contains('L'),
            "{label}: BLANK BAND — row {i} blank inside the transcript block\nscreen:\n{}",
            rows.join("\n")
        );
    }
    // Below the block: only blank viewport rows — anything else is a stale
    // repaint remnant.
    for (i, row) in rows.iter().enumerate().skip(n) {
        assert!(
            !row.contains('L'),
            "{label}: stray marker below the transcript block at row {i}\nscreen:\n{}",
            rows.join("\n")
        );
    }
}

// Expected windows, traced from the set_height/insert_before semantics at
// 80x24 with initial height 10 (see run_scenario for the op sequences):
//
// collapse-idle: after insert(30) y=14 → grow(20) pushes 10 (y=4) →
// insert(12) leaves rows 0..4 = L039..L042 → in-place shrink to 6 keeps
// them. Visible: L039..=L042.
//
// collapse: + insert(3) pushes the viewport down 3 and paints L043..L045
// directly into the vacated rows. Visible: L039..=L045.
//
// small-insert-after-collapse: shrink to 4, then two 2-line inserts, each
// fully gap-painted. Visible: L039..=L046.

#[test]
fn collapse_idle_fast() {
    assert_window(
        &scrape("collapse-idle", false),
        39,
        42,
        "collapse-idle/fast",
    );
}

#[test]
fn collapse_idle_conservative() {
    assert_window(
        &scrape("collapse-idle", true),
        39,
        42,
        "collapse-idle/conservative",
    );
}

#[test]
fn collapse_then_insert_fast() {
    assert_window(&scrape("collapse", false), 39, 45, "collapse/fast");
}

#[test]
fn collapse_then_insert_conservative() {
    assert_window(&scrape("collapse", true), 39, 45, "collapse/conservative");
}

#[test]
fn small_inserts_after_collapse_fast() {
    assert_window(
        &scrape("small-insert-after-collapse", false),
        39,
        46,
        "small-insert/fast",
    );
}

#[test]
fn small_inserts_after_collapse_conservative() {
    assert_window(
        &scrape("small-insert-after-collapse", true),
        39,
        46,
        "small-insert/conservative",
    );
}
