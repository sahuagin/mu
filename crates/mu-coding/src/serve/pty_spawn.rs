//! mu-slat Phase 3: Rust-owned pty for pot worker spawn.
//!
//! Replaces the `script(1)` + stdin-pipe kickstart hack. We own the
//! pty master directly, feed its output through a vt100 emulator (the
//! "terminal Jody set up" — a headless ANSI state machine that
//! maintains a screen-cell grid), detect when claude's input prompt is
//! actually ready instead of sleeping blind, and deliver the kickstart
//! as keystrokes with human-ish cadence so the TUI's input state
//! machine (bracketed paste, debounce) accepts it intact.
//!
//! Pot lifecycle (clone/start/vnet/DHCP/devfs + MCP config + system
//! prompt files) stays in `mu-spawn` (invoked in setup-only mode). This
//! module takes over only the claude launch + pty I/O.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize};

const ROWS: u16 = 50;
const COLS: u16 = 120;
const READY_TIMEOUT: Duration = Duration::from_secs(45);

/// Inputs for launching claude in a pot under a Rust-owned pty. The pot
/// must already be set up (running, networked, MCP config + system
/// prompt files written) — see `mu-spawn` setup-only mode.
pub(crate) struct PtyWorkerConfig {
    pub pot_name: String,
    pub model: String,
    pub daemon_id: String,
    pub session_id: String,
    pub reply_to: String,
    /// Single-token doorbell typed after the prompt is ready. The task
    /// itself lives in the mailbox; this just wakes claude to read it.
    pub kickstart: String,
}

/// How the pty child ended.
pub(crate) enum PtyExit {
    Exited { success: bool, code: i32 },
    Error { reason: String },
}

/// Live handle to a running pty worker. Holds the vt100 screen for
/// scraping (observability) and a oneshot that fires on child exit.
pub(crate) struct PtyWorker {
    screen: Arc<Mutex<vt100::Parser>>,
    pub exit_rx: tokio::sync::oneshot::Receiver<PtyExit>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl PtyWorker {
    /// Kill the child (used on deadline). Best-effort.
    pub fn kill(&mut self) {
        let _ = self.killer.kill();
    }

    /// Scrape the current rendered screen — what a human would see.
    /// Used for observability (e.g. capturing the last screen on exit).
    pub fn scrape(&self) -> String {
        self.screen
            .lock()
            .map(|p| p.screen().contents())
            .unwrap_or_default()
    }
}

/// Launch claude inside `pot_name` under a Rust-owned pty. Spawns three
/// detached threads: a reader (feeds vt100), a driver (readiness →
/// cadence kickstart), and a waiter (child exit → oneshot). Returns
/// immediately with a handle; the caller monitors `exit_rx`.
pub(crate) fn spawn_pty_worker(config: PtyWorkerConfig) -> Result<PtyWorker, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let body = build_jexec_body(&config);
    let mut cmd = CommandBuilder::new("sudo");
    cmd.args([
        "jexec",
        "-U",
        "tcovert",
        &config.pot_name,
        "/bin/sh",
        "-c",
        &body,
    ]);
    cmd.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn jexec: {e}"))?;
    // Drop the slave so the master sees EOF when the child exits.
    drop(pair.slave);

    let killer = child.clone_killer();

    let screen = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 0)));
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("clone reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take writer: {e}"))?;

    // Reader thread: feed all pty output into the vt100 parser. Holds
    // the master alive so the pty stays open until the child exits.
    let screen_r = screen.clone();
    let master = pair.master;
    thread::spawn(move || {
        let _master = master; // keep pty open for the thread's lifetime
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut p) = screen_r.lock() {
                        p.process(&buf[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Driver thread: wait for the prompt, then type the kickstart with
    // cadence + an explicit submit.
    let screen_d = screen.clone();
    let kickstart = config.kickstart.clone();
    let session_id = config.session_id.clone();
    thread::spawn(move || {
        let mut writer = writer;
        if wait_for_ready(&screen_d, READY_TIMEOUT) {
            // Settle: let the TUI finish its initial render pass.
            thread::sleep(Duration::from_millis(1500));
            type_with_cadence(&mut writer, &kickstart);
            thread::sleep(Duration::from_millis(300));
            // Explicit submit (CR). Proven to submit the turn in the probe.
            let _ = writer.write_all(b"\r");
            let _ = writer.flush();
            tracing::info!(session_id = %session_id, "pty worker: kickstart delivered");
        } else {
            tracing::warn!(session_id = %session_id, "pty worker: prompt never appeared within timeout");
        }
    });

    // Waiter thread: block on child exit, report via oneshot.
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    thread::spawn(move || {
        let mut child = child;
        let exit = match child.wait() {
            Ok(status) => PtyExit::Exited {
                success: status.success(),
                code: status.exit_code() as i32,
            },
            Err(e) => PtyExit::Error {
                reason: e.to_string(),
            },
        };
        let _ = exit_tx.send(exit);
    });

    Ok(PtyWorker {
        screen,
        exit_rx,
        killer,
    })
}

/// Build the `/bin/sh -c` body that launches claude. Reads the MCP
/// config + system prompt from the files `mu-spawn` setup-only wrote to
/// the pot's `/compat/linux/tmp`. The system prompt is read via
/// `$(cat …)` inside the jail so no shell quoting of its contents
/// crosses the boundary (the file-based fix from Phase 1).
fn build_jexec_body(c: &PtyWorkerConfig) -> String {
    let mcp_config = format!("/compat/linux/tmp/mu-mcp-{}.json", c.pot_name);
    let sysprompt = format!("/compat/linux/tmp/mu-system-prompt-{}", c.pot_name);
    format!(
        "unset ANTHROPIC_API_KEY ANTHROPIC_BASE_URL CLAUDE_CODE_OAUTH_TOKEN; \
         export HOME=/usr/home/tcovert; \
         export LANG=C.UTF-8; \
         export TERM=xterm-256color; \
         export PATH=/usr/local/pot-bin:/usr/home/tcovert/.cargo/bin:/usr/home/tcovert/.local/bin:$PATH; \
         export CLAUDE_CONFIG_DIR=/usr/home/tcovert/.claude-personal; \
         export MU_DAEMON_ID='{daemon}'; \
         export MU_SESSION_ID='{session}'; \
         export MU_REPLY_TO='{reply}'; \
         export MU_POT_NAME='{pot}'; \
         exec /usr/local/bin/claude --dangerously-skip-permissions --model {model} \
           --mcp-config '{mcp}' --append-system-prompt \"$(cat '{prompt}')\"",
        daemon = c.daemon_id,
        session = c.session_id,
        reply = c.reply_to,
        pot = c.pot_name,
        model = c.model,
        mcp = mcp_config,
        prompt = sysprompt,
    )
}

/// Poll the vt100 screen until claude's input prompt is visibly ready.
/// Looks for the prompt marker (❯) or the permission-mode footer that
/// only renders once the TUI is fully up. Returns false on timeout.
fn wait_for_ready(screen: &Arc<Mutex<vt100::Parser>>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(500));
        let contents = match screen.lock() {
            Ok(p) => p.screen().contents(),
            Err(_) => continue,
        };
        if contents.contains('❯') || contents.contains("bypass permissions") {
            return true;
        }
    }
    false
}

/// Type a string char-by-char with human-ish inter-key delays. Slow
/// enough that the TUI never sees it as a single paste burst — the
/// lesson from Jody modeling his typing cadence so NASDAQ couldn't tell
/// a program was driving the SOES terminal.
fn type_with_cadence(writer: &mut Box<dyn Write + Send>, s: &str) {
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
        let jitter = 40 + (ch as u64 % 50);
        thread::sleep(Duration::from_millis(jitter));
    }
}
