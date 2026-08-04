//! Screen-scrape validation for the mu-8oqp viewport rework.
//!
//! Runs the `vt-scenario` binary under a real pty and parses everything it
//! emits with the `vt100` crate — an INDEPENDENT terminal model. Two prior
//! fixes in this bug family (mu-solo-zellij-blank-band-ptvm, PR #502) were
//! green on their own bookkeeping tests and wrong on the actual screen; these
//! tests assert on what a terminal would display, not on what the code
//! believes it did.
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
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::pty::{openpty, Winsize};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Run one scenario under a pty; return the visible screen rows (trimmed).
fn scrape(scenario: &str, force_conservative: bool) -> Vec<String> {
    let ws = Winsize {
        ws_row: ROWS,
        ws_col: COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(Some(&ws), None).expect("openpty");
    let slave = pty.slave;
    // Make the pty slave byte-transparent before spawning. The harness feeds
    // the child's escape stream into an independent vt100 parser, so inheriting
    // host-specific cooked output processing would make the test environment,
    // rather than mu-solo, rewrite the bytes under test.
    let mut tio = tcgetattr(&slave).expect("tcgetattr slave");
    cfmakeraw(&mut tio);
    tcsetattr(&slave, SetArg::TCSANOW, &tio).expect("tcsetattr raw slave");
    let mut master = std::fs::File::from(pty.master);

    let exe = env!("CARGO_BIN_EXE_vt-scenario");
    let mut command = Command::new(exe);
    command
        .arg(scenario)
        .stdin(Stdio::from(slave.try_clone().expect("clone slave")))
        .stdout(Stdio::from(slave.try_clone().expect("clone slave")))
        .stderr(Stdio::from(slave))
        .env("TERM", "xterm-256color")
        .env(
            "MU_SOLO_FORCE_CONSERVATIVE_RENDER",
            if force_conservative { "1" } else { "0" },
        );
    // `crossterm::terminal::size()` otherwise consults the developer's outer
    // controlling TTY (zellij/tmux) instead of the 80x24 pty above. Headless
    // CI has no controlling TTY and therefore falls back to stdio. Detach the
    // child into a new session to make local execution follow that same path.
    // SAFETY: between fork and exec the closure calls only `setsid` (which is
    // async-signal-safe) and captures errno on failure; it touches no shared
    // Rust state.
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn vt-scenario");
    // `Command::spawn` borrows its reusable builder. Drop the builder now so
    // the parent's Stdio-owned slave descriptors cannot suppress master EOF.
    drop(command);

    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stream: Vec<u8> = Vec::new();
    let mut answered = 0usize;
    let mut exited = false;
    let mut last_byte_at = Instant::now();

    loop {
        let ready = {
            let mut fds = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];
            poll(&mut fds, PollTimeout::from(50u16)).unwrap_or(0) > 0
        };
        if ready {
            let mut chunk = [0u8; 4096];
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    last_byte_at = Instant::now();
                    stream.extend_from_slice(&chunk[..n]);
                    parser.process(&chunk[..n]);
                    // Answer cursor-position queries (CSI 6n) or crossterm's
                    // cursor::position() blocks inside DynamicViewport::new.
                    let queries = count_occurrences(&stream, b"\x1b[6n");
                    while answered < queries {
                        let (row, col) = parser.screen().cursor_position();
                        let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                        master.write_all(reply.as_bytes()).expect("CPR reply");
                        answered += 1;
                    }
                }
                // pty master read errors when the last slave fd closes.
                Err(_) => break,
            }
        }
        if !exited && child.try_wait().expect("try_wait").is_some() {
            exited = true;
        }
        // After exit, keep draining until the stream has been SILENT for a
        // beat — a fixed post-exit window lost trailing bytes under parallel
        // test load (flaky collapse-idle failures in workspace runs).
        if exited && last_byte_at.elapsed() > Duration::from_millis(750) {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("vt-scenario timed out; bytes so far: {}", stream.len());
        }
    }
    let _ = child.wait();

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

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| w == &needle)
        .count()
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

// Expected windows, traced from the new set_height/insert_before semantics
// at 80x24 with initial height 10 (see vt-scenario for the op sequences):
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
