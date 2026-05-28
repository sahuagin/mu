//! Terrain-check probe for the Rust-owned pty worker spawn (mu-slat
//! Phase 3). De-risks the core unknown: can portable-pty drive
//! `sudo jexec … claude` and deliver keystrokes the TUI accepts?
//!
//! Run: cargo run -p mu-coding --example pty_probe -- <pot-name>
//!
//! What it does:
//!   1. Opens a pty.
//!   2. Spawns `sudo jexec -U tcovert <pot> /bin/sh -c <claude launch>`.
//!   3. Feeds pty output into a vt100 emulator (the "terminal Jody set up").
//!   4. Polls the screen grid until the input prompt (❯) appears.
//!   5. Types a test message char-by-char with human-ish cadence + Enter.
//!   6. Scrapes the screen for the expected response word (BANANA).
//!
//! Prints the rendered screen periodically to stderr so we can watch.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const ROWS: u16 = 50;
const COLS: u16 = 120;

fn main() {
    let pot = std::env::args().nth(1).unwrap_or_else(|| "mu-slat-test".into());
    let model = std::env::var("MU_SPAWN_MODEL").unwrap_or_else(|_| "claude-opus-4-7".into());

    eprintln!("[probe] pot={pot} model={model}");

    // sh body: env scrub + claude launch (no MCP — pure pty round-trip test).
    let body = format!(
        "unset ANTHROPIC_API_KEY ANTHROPIC_BASE_URL CLAUDE_CODE_OAUTH_TOKEN; \
         export HOME=/usr/home/tcovert; \
         export LANG=C.UTF-8; \
         export TERM=xterm-256color; \
         export PATH=/usr/local/pot-bin:/usr/home/tcovert/.cargo/bin:/usr/home/tcovert/.local/bin:$PATH; \
         export CLAUDE_CONFIG_DIR=/usr/home/tcovert/.claude-personal; \
         exec /usr/local/bin/claude --dangerously-skip-permissions --model {model}"
    );

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new("sudo");
    cmd.args(["jexec", "-U", "tcovert", &pot, "/bin/sh", "-c", &body]);
    cmd.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    eprintln!("[probe] child spawned");

    // Drop the slave so the master sees EOF when the child exits.
    drop(pair.slave);

    let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");

    // Reader thread: feed all pty output into the vt100 parser.
    let parser_r = parser.clone();
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut p) = parser_r.lock() {
                        p.process(&buf[..n]);
                    }
                }
                Err(_) => break,
            }
        }
        eprintln!("[probe] reader: pty EOF");
    });

    // Poll for readiness: screen contains the prompt marker ❯.
    eprintln!("[probe] waiting for prompt readiness...");
    let ready_deadline = Instant::now() + Duration::from_secs(45);
    let mut ready = false;
    while Instant::now() < ready_deadline {
        thread::sleep(Duration::from_millis(500));
        let contents = {
            let p = parser.lock().unwrap();
            p.screen().contents()
        };
        if contents.contains('❯') || contents.contains("bypass permissions") {
            ready = true;
            let elapsed = 45 - (ready_deadline - Instant::now()).as_secs();
            eprintln!("[probe] prompt ready after ~{elapsed}s");
            break;
        }
    }

    if !ready {
        eprintln!("[probe] FAIL: prompt never appeared. Final screen:");
        dump_screen(&parser);
        let _ = child.kill();
        return;
    }

    // Settle, then type with cadence.
    thread::sleep(Duration::from_millis(1500));
    let msg = "Reply with exactly one word: BANANA";
    eprintln!("[probe] typing: {msg:?}");
    type_with_cadence(&mut writer, msg);
    // Explicit submit. Try CR first.
    thread::sleep(Duration::from_millis(300));
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    eprintln!("[probe] sent CR (submit)");

    // Watch for response.
    let resp_deadline = Instant::now() + Duration::from_secs(60);
    let mut got_banana = false;
    while Instant::now() < resp_deadline {
        thread::sleep(Duration::from_millis(2000));
        let contents = {
            let p = parser.lock().unwrap();
            p.screen().contents()
        };
        // Look for BANANA appearing OUTSIDE our own typed line — crude:
        // count occurrences; >1 means it echoed in our input AND claude said it.
        let count = contents.matches("BANANA").count();
        eprintln!("[probe] BANANA occurrences on screen: {count}");
        if count >= 2 {
            got_banana = true;
            break;
        }
    }

    eprintln!("[probe] ===== FINAL SCREEN =====");
    dump_screen(&parser);
    eprintln!("[probe] =========================");

    if got_banana {
        eprintln!("[probe] ✓ SUCCESS: claude received input and responded via pty");
    } else {
        eprintln!("[probe] ✗ no clear response — inspect screen above");
    }

    let _ = child.kill();
    let _ = reader_handle.join();
}

/// Type a string char-by-char with human-ish inter-key delays. Slow
/// enough that the TUI never sees it as a single paste burst.
fn type_with_cadence(writer: &mut Box<dyn Write + Send>, s: &str) {
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
        // 40–90ms jitter per keystroke.
        let jitter = 40 + (ch as u64 % 50);
        thread::sleep(Duration::from_millis(jitter));
    }
}

fn dump_screen(parser: &Arc<Mutex<vt100::Parser>>) {
    let p = parser.lock().unwrap();
    let contents = p.screen().contents();
    for line in contents.lines() {
        if !line.trim().is_empty() {
            eprintln!("  | {line}");
        }
    }
}
