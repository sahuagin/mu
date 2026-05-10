# Spec: `mu ask` — one-shot frontend over `mu serve`

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-005                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

The first user-facing CLI command. After this lands, you can type:

```sh
mu ask "hello"
```

and get the assistant's response printed to your terminal. With
v1's hardcoded FauxProvider::echo, the output will be `hello` — but
the wiring is the production shape. Swapping in a real Provider
(mu-006) will make the same CLI useful for real work, no `mu ask`
changes needed.

This spec is also the first **frontend** in mu's frontend/daemon
split. The architectural shape — frontend spawns `mu serve` as a
subprocess and speaks JSON-RPC over its stdio — is what every later
frontend (`mu tui`, `mu orchestrate`) reuses. Get it right once.

## Scope

- **In:**
  - **`crates/mu-coding/src/ask.rs`** — the `mu ask` mode.
    Spawns `mu serve` as a child via `tokio::process::Command`, opens
    JSON-RPC over the child's stdio, sends `create_session` →
    `ask_session`, drains the stream of notifications until
    `session.done`, prints the assistant's text, exits.
  - **`crates/mu-coding/src/lib.rs`** — `pub mod ask;`.
  - **`crates/mu-coding/src/bin/mu.rs`** — `Command::Ask { prompt }`
    arm calls `mu_coding::ask::run(prompt).await`.
  - **`crates/mu-coding/tests/ask_smoke.rs`** — integration test that
    runs `mu ask "hello"` as a subprocess (via `tokio::process` or
    `std::process`), captures stdout, asserts it contains "hello".
    This requires the `mu` binary to be built first; cargo handles
    that automatically for integration tests.
  - **`crates/mu-coding/src/serve/mod.rs`** — small reuse-friendly
    addition: a public helper to find the `mu` binary path
    (`std::env::current_exe()` for normal use, or
    `env!("CARGO_BIN_EXE_mu")` for tests). Decides which based on a
    `MU_BINARY` env var; default = current_exe.

- **Out:**
  - Streaming output. v1 buffers all `session.text_delta`s and
    prints once at the end. Stream-as-you-go is a small future
    amendment; the wiring already supports it (the deltas arrive
    over the same channel).
  - Tool execution display. With FauxProvider::echo, no tools fire.
    With a real Provider that emits tool calls, v1 will silently
    drop the tool events (they'll be visible in `mu serve`'s output
    but not in `mu ask`'s). Future spec adds tool-event display.
  - Error formatting beyond a `eprintln!`. Pretty errors are a
    later TUI/UX spec.
  - `mu ask --provider <X>` flag. v1 uses whatever provider the
    spawned `mu serve` uses (which is hardcoded to
    `FauxProvider::echo` per mu-004). Future spec adds CLI flags
    that pass through to the daemon.
  - Session reuse across multiple `mu ask` invocations. Each call
    creates a fresh session, runs one turn, closes the session.
    Stateless from the user's perspective.

- **Non-goals:**
  - In-process mode (call `serve_with_io` directly without spawning
    a subprocess). The spec deliberately uses subprocess because
    `mu orchestrate` will need to spawn N daemons; using subprocess
    in `mu ask` proves the same machinery on a smaller scale.
  - Reading the prompt from stdin. v1 takes the prompt as a clap
    arg. Stdin support is a small future amendment.

## Invariants

- **INV-1 (file size):** Each file under 800 lines. `ask.rs` will
  land around 200 LOC; well under.
- **INV-2 (subprocess shape):** `mu ask` spawns `mu serve` via
  `tokio::process::Command::new(<binary path>).arg("serve")`. The
  child's stdin/stdout are piped (not inherited). The child's stderr
  is inherited so server logs reach the user's terminal directly.
- **INV-3 (no token holding):** Same as AGENTS.md. `mu ask` doesn't
  see Provider auth at all — that's the daemon's concern.
- **INV-4 (clean child shutdown):** When `mu ask` exits, the child
  `mu serve` must also exit. The natural mechanism: `mu ask` closes
  its stdin handle to the child; the daemon's stdin sees EOF;
  `serve_stdio` returns; daemon exits. `mu ask` does NOT use
  `Child::kill()` in the success path — only as a fallback if the
  daemon hasn't exited within a timeout (5 seconds).
- **INV-5 (no unsafe, no unwrap/expect/panic outside tests):**
  Standard.
- **INV-6 (no new workspace deps):** Use what's already there.
  `tokio::process` is in `tokio` (full feature, already on).

## Interfaces

### `crates/mu-coding/src/ask.rs`

```rust
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use mu_core::protocol::{
    AskSessionRequest, CreateSessionRequest, CreateSessionResponse,
    PingRequest,
};

/// Run a single `mu ask` invocation.
///
/// 1. Spawns `mu serve` as a child subprocess.
/// 2. Sends `create_session`; reads response; extracts session_id.
/// 3. Sends `ask_session` with the prompt; reads notifications/response
///    until `session.done`.
/// 4. Concatenates `session.text_delta` payloads, prints to stdout.
/// 5. Closes the child's stdin; waits for the child to exit (with
///    a 5-second timeout fallback to `Child::kill`).
pub async fn run(prompt: String) -> Result<()> {
    let mut child = spawn_serve()?;
    let mut stdin = child.stdin.take().context("child stdin")?;
    let stdout = child.stdout.take().context("child stdout")?;
    let mut stdout = BufReader::new(stdout);

    // Use atomic-counter-ish increment for ids. Three requests in this run.
    let mut next_id: u64 = 1;

    // Step 1: create_session.
    let session_id = create_session(&mut stdin, &mut stdout, &mut next_id).await?;

    // Step 2: ask_session.
    let text = ask_and_drain(&mut stdin, &mut stdout, &session_id, &prompt, &mut next_id).await?;

    // Step 3: print + clean shutdown.
    println!("{}", text);

    // Closing stdin signals the daemon to exit. Wait briefly.
    drop(stdin);
    match timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => bail!("mu serve exited with status {status}"),
        Ok(Err(e)) => Err(e).context("waiting for child"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("mu serve did not exit within 5 seconds; killed")
        }
    }
}

fn spawn_serve() -> Result<tokio::process::Child> {
    let binary = std::env::var("MU_BINARY")
        .ok()
        .or_else(|| std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()))
        .ok_or_else(|| anyhow!("could not locate mu binary"))?;
    Command::new(binary)
        .arg("serve")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // stderr inherited — daemon logs go directly to user's terminal
        .spawn()
        .context("failed to spawn mu serve")
}

async fn create_session(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    next_id: &mut u64,
) -> Result<String> {
    let id = *next_id;
    *next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": CreateSessionRequest::METHOD,
        "params": {
            // v1: provider field is required by the spec but ignored
            // by the daemon; pass any valid shape.
            "provider": { "kind": "anthropic_api", "model": "irrelevant" }
        }
    });
    write_line(stdin, &req).await?;

    // Read until we get the response with matching id.
    loop {
        let line = read_line(stdout).await?;
        if line.get("id") == Some(&Value::from(id)) {
            if let Some(error) = line.get("error") {
                bail!("create_session failed: {error}");
            }
            let resp: CreateSessionResponse = serde_json::from_value(
                line.get("result").cloned().unwrap_or(Value::Null),
            )
            .context("parse CreateSessionResponse")?;
            return Ok(resp.session_id);
        }
        // Notification or unrelated line — ignore.
    }
}

async fn ask_and_drain(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    session_id: &str,
    prompt: &str,
    next_id: &mut u64,
) -> Result<String> {
    let id = *next_id;
    *next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": AskSessionRequest::METHOD,
        "params": {
            "session_id": session_id,
            "user_message": prompt,
        }
    });
    write_line(stdin, &req).await?;

    let mut text = String::new();
    let mut got_done = false;
    let mut got_response = false;

    loop {
        let line = read_line(stdout).await?;
        match line.get("method").and_then(Value::as_str) {
            Some("session.text_delta") => {
                if line["params"]["session_id"] == session_id {
                    if let Some(delta) = line["params"]["delta"].as_str() {
                        text.push_str(delta);
                    }
                }
            }
            Some("session.done") => {
                if line["params"]["session_id"] == session_id {
                    got_done = true;
                }
            }
            Some("session.error") => {
                if line["params"]["session_id"] == session_id {
                    bail!(
                        "session error: {}",
                        line["params"]["message"].as_str().unwrap_or("(unknown)")
                    );
                }
            }
            _ => {
                // Could be the ask_session response itself.
                if line.get("id") == Some(&Value::from(id)) {
                    if let Some(error) = line.get("error") {
                        bail!("ask_session failed: {error}");
                    }
                    got_response = true;
                }
            }
        }
        if got_done && got_response {
            return Ok(text);
        }
    }
}

async fn write_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    stdin.write_all(s.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_line(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut line = String::new();
    let n = stdout.read_line(&mut line).await?;
    if n == 0 {
        bail!("mu serve closed stdout unexpectedly");
    }
    serde_json::from_str(line.trim_end()).context("parse JSON line")
}
```

### `crates/mu-coding/src/lib.rs`

Add one line: `pub mod ask;`.

### `crates/mu-coding/src/bin/mu.rs`

Replace the `Command::Ask { .. }` arm:

```rust
Command::Ask { prompt } => mu_coding::ask::run(prompt).await,
```

(The current arm bails with "not implemented".)

## Behaviors

1. **B-1 (echo round-trip):** `mu ask "hello"` prints "hello" and
   exits 0. (FauxProvider::echo returns the user message back.)
2. **B-2 (multi-word prompt):** `mu ask "hello world"` prints
   "hello world".
3. **B-3 (clean child shutdown):** Within 5 seconds of `mu ask`'s
   `println!`, the child `mu serve` process has exited. (Verified
   via the integration test capturing the child's exit status.)
4. **B-4 (empty prompt):** `mu ask ""` prints an empty line (the echo
   of an empty user_message). Exits 0. Edge case worth pinning so a
   regression isn't subtle.

## Acceptance

- New file: `crates/mu-coding/src/ask.rs`
- Modified files:
  - `crates/mu-coding/src/lib.rs` (+1 line)
  - `crates/mu-coding/src/bin/mu.rs` (Ask arm)
- New integration test: `crates/mu-coding/tests/ask_smoke.rs`
- `cargo build` clean.
- `cargo nextest run` passes — every existing test + the four B-1..B-4
  here.
- `mu ask "hello"` (run manually) prints "hello" and exits 0.
- ask.rs under 400 lines.

## Iteration-aware handoff

This is claude-implemented; no sub-agent iteration cap. If implementation
becomes nontrivial (e.g., subprocess plumbing on FreeBSD has a quirk),
fall back to the in-process variant (call `serve_with_io` over duplex
pipes inside the same process) and document the deferral as a future
spec amendment.

## Open questions

- [ ] OQ-1: Should `mu ask` print to stdout immediately as deltas
  arrive, or buffer and print at end? — owner: defer — resolution:
  buffer for v1. Streaming-as-you-go is a one-line change later
  (call `print!(delta)` + `flush` instead of `text.push_str`); change
  doesn't break the contract.
- [ ] OQ-2: Should the `mu serve` child have its stderr piped or
  inherited? — owner: tcovert — resolution: inherited per §INV-2.
  Server logs go to user's terminal — useful in dev, can be quieted
  via `RUST_LOG=warn` later.

## Out-of-circuit warnings

- **OOC-1:** `tokio::process::Child::wait` returns `io::Result<ExitStatus>`,
  but if the child has already exited and been reaped, subsequent
  `wait` calls return `Ok(<status>)` — they don't error. So calling
  `wait` after a successful drop-stdin is safe.
- **OOC-2:** `read_line` returns `n = 0` on EOF. Don't conflate this
  with "blank line received." Empty line ends with a `\n`; EOF
  doesn't.
- **OOC-3:** The integration test runs the actual `mu` binary as a
  subprocess. Cargo provides `env!("CARGO_BIN_EXE_mu")` at compile
  time inside test modules — the path to the just-built binary. Use
  this in `tests/ask_smoke.rs` to set `MU_BINARY` before invoking
  the test command.

## Prior work / context

- mu-001 — protocol types.
- mu-002 — stdio transport.
- mu-003 — agent loop.
- mu-004 — `mu serve` end-to-end.
- task_log entries tagged `mu`.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
