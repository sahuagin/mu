//! `mu ask` — one-shot CLI frontend over `mu serve`.
//!
//! Spawns `mu serve` as a subprocess, speaks JSON-RPC over its
//! stdio, sends `create_session` + `ask_session`, drains notifications
//! until `session.done`, prints the assistant text, exits.
//!
//! See spec mu-005.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use mu_core::protocol::{AskSessionRequest, CreateSessionRequest, CreateSessionResponse};

/// Options for [`run`] — the CLI-flag bundle for a single `mu ask`
/// invocation. Constructed by the CLI binary's argument parser.
#[derive(Debug, Default)]
pub struct AskOptions {
    pub prompt: String,
    pub provider: String,
    pub model: Option<String>,
    pub tools: String,
    pub ephemeral: bool,
    pub thinking: Option<String>,
    pub bash_yolo: bool,
    pub bash_allow: Vec<String>,
    pub bash_prompt: bool,
    /// System prompt for the session. When present, sent as
    /// `CreateSessionRequest.system_prompt` (mu-n48 plumbing); when
    /// None, the daemon default is used. Populated by the CLI from
    /// `--append-system-prompt <FILE>` (file content read by the
    /// binary, not here, so this layer stays I/O-free).
    pub system_prompt: Option<String>,
    /// Hermetic session: forwarded as `--bare` to the spawned
    /// `mu serve` — no recall injection, no discovery bootstrap.
    /// (mu-mu-bare-flag-fxc8)
    pub bare: bool,
    /// mu-779s: cap on assistant-message turns. `None` → use the
    /// provider-aware default (20 for Anthropic, 35 for OpenAI).
    /// `Some(n)` → cap at `n` turns. `Some(0)` → disable entirely.
    /// Forwarded as `CreateSessionRequest.max_turns` to the daemon.
    pub max_turns: Option<u32>,
}

/// Run a single `mu ask` invocation. Flags (`provider`, `model`,
/// `tools`) are forwarded to the spawned `mu serve`.
pub async fn run(opts: AskOptions) -> Result<()> {
    // Map the CLI provider flag to a wire-level selector. This is what
    // gets sent in create_session; the daemon constructs the provider
    // per session from this.
    let selector = crate::serve::selector_from_cli(&opts.provider, opts.model.as_deref())?;

    // mu-fnn: generate a per-spawn bearer token for the trust-on-spawn
    // handshake with the child `mu serve`. The child reads
    // `MU_BEARER_TOKEN` from its env (see `serve::run`) and configures
    // BEARER auth with this single token; we then present the same
    // token in `peer.auth_initiate` before any session.* RPC.
    let bearer_token = generate_bearer_token();

    let mut child = spawn_serve(
        &opts.tools,
        opts.ephemeral,
        opts.thinking.as_deref(),
        opts.bash_yolo,
        &opts.bash_allow,
        opts.bash_prompt,
        opts.bare,
        &bearer_token,
    )?;
    let mut stdin = child.stdin.take().context("child stdin not captured")?;
    let stdout = child.stdout.take().context("child stdout not captured")?;
    let mut stdout = BufReader::new(stdout);

    let mut next_id: u64 = 1;

    // Authenticate before any protected RPC. Failure here is fatal —
    // the gate will reject every subsequent call.
    authenticate(&mut stdin, &mut stdout, &mut next_id, &bearer_token).await?;

    // mu-phl v0 / mu-lfgh: capture the operator's cwd at the entry of
    // the ask path so the daemon's session-start recall (subprocess
    // agent memory + project-file hierarchy) scopes to the operator's
    // actual project. Falls back to `None` if cwd can't be determined
    // (extremely unusual; the daemon resolves its own fallback in
    // build_project_context).
    let invocation_cwd = std::env::current_dir().ok();
    let session_id = create_session(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        &selector,
        opts.system_prompt.as_deref(),
        invocation_cwd,
        opts.max_turns,
    )
    .await?;
    let (text, stop_reason) = ask_and_drain(
        &mut stdin,
        &mut stdout,
        &session_id,
        &opts.prompt,
        &mut next_id,
    )
    .await?;

    println!("{}", text);

    // Closing stdin signals the daemon to exit cleanly.
    drop(stdin);
    match timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) => bail!("mu serve exited with status {status}"),
        Ok(Err(e)) => return Err(e).context("waiting for child"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("mu serve did not exit within 5 seconds; killed")
        }
    }

    // A truncated response must never exit 0 silently (mu-1mvq): the
    // ai-review gate spent a night escalating every PR because ollama
    // truncated oversized prompts to its context window, leaving one
    // token of generation budget — the model emitted a single word,
    // the stream ended *cleanly* with finish_reason=length, and no
    // layer reported anything. The partial text has already been
    // printed above (it is still data); the nonzero exit + stderr
    // line make the truncation legible to scripts and humans.
    match stop_reason.as_deref() {
        Some("max_tokens") => bail!(
            "response truncated (stop_reason=max_tokens): the model hit a token \
             limit — either the output cap, or the prompt filled the model's \
             context window (ollama silently truncates oversized prompts; see \
             mu-1mvq). Output above may be a fragment."
        ),
        Some("degraded_eof") => bail!(
            "response degraded (stop_reason=degraded_eof): the provider stream \
             closed without a terminal stop event (connection drop or upstream \
             truncation). Output above may be a fragment."
        ),
        _ => Ok(()),
    }
}

/// Generate a per-spawn opaque bearer token for the parent↔child
/// handshake. The strength bar is "unguessable across this process
/// lifetime"; SHA-256 + constant-time comparison on the daemon side
/// already absorb timing concerns. 32 hex chars / 128 bits is plenty.
pub(crate) fn generate_bearer_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Run the `peer.auth_initiate` BEARER handshake. Returns Ok on
/// `Accepted`; surfaces a clear error on `Denied` or any non-success.
pub(crate) async fn authenticate(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    next_id: &mut u64,
    token: &str,
) -> Result<()> {
    let id = *next_id;
    *next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "peer.auth_initiate",
        "params": {
            "mechanism": "bearer",
            "initial_response": token,
        },
    });
    write_line(stdin, &req).await?;
    loop {
        let line = read_line(stdout).await?;
        if line.get("id").and_then(|v| v.as_u64()) == Some(id) {
            let outcome = line
                .get("result")
                .and_then(|r| r.get("outcome"))
                .and_then(|v| v.as_str());
            if outcome == Some("accepted") {
                return Ok(());
            }
            bail!("peer.auth_initiate did not accept the spawn-time token: {line}");
        }
        // Skip unrelated notifications.
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the CLI flag bundle 1:1
pub(crate) fn spawn_serve(
    tools: &str,
    ephemeral: bool,
    thinking: Option<&str>,
    bash_yolo: bool,
    bash_allow: &[String],
    bash_prompt: bool,
    bare: bool,
    bearer_token: &str,
) -> Result<tokio::process::Child> {
    // MU_BINARY env override allows integration tests to point at a
    // specific binary path (`env!("CARGO_BIN_EXE_mu")`); production
    // falls back to the current executable.
    let binary = match std::env::var("MU_BINARY") {
        Ok(v) if !v.is_empty() => v,
        _ => std::env::current_exe()
            .context("could not determine current_exe")?
            .to_string_lossy()
            .into_owned(),
    };

    let mut cmd = Command::new(&binary);
    cmd.arg("serve");
    // mu-fnn: hand the child the same BEARER token we'll present at
    // `peer.auth_initiate`. Single source of truth: this string.
    cmd.env("MU_BEARER_TOKEN", bearer_token);
    if !tools.is_empty() {
        cmd.arg("--tools").arg(tools);
    }
    if ephemeral {
        cmd.arg("--ephemeral");
    }
    if let Some(t) = thinking {
        if !t.is_empty() {
            cmd.arg("--thinking").arg(t);
        }
    }
    if bash_yolo {
        cmd.arg("--bash-yolo");
    }
    for entry in bash_allow {
        if !entry.is_empty() {
            cmd.arg("--bash-allow").arg(entry);
        }
    }
    if bash_prompt {
        cmd.arg("--bash-prompt");
    }
    if bare {
        cmd.arg("--bare");
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // stderr inherited — daemon logs go to the user's terminal.
        .spawn()
        .with_context(|| format!("failed to spawn `{binary} serve`"))
}

async fn create_session(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    next_id: &mut u64,
    selector: &mu_core::protocol::ProviderSelector,
    system_prompt: Option<&str>,
    cwd: Option<std::path::PathBuf>,
    max_turns: Option<u32>,
) -> Result<String> {
    let id = *next_id;
    *next_id += 1;
    // Build params from the typed protocol struct so that
    // serde's `skip_serializing_if = "Option::is_none"` on
    // CreateSessionRequest.system_prompt is honored — no
    // explicit null field when unset (mu-x83o).
    //
    // mu-phl v0 / mu-lfgh: cwd is plumbed through from the operator's
    // invocation (set by the `ask()` entry point to
    // std::env::current_dir()) so the daemon-side recall providers
    // (subprocess agent memory + project-file hierarchy) scope to the
    // operator's actual project rather than the daemon's process cwd.
    let body = CreateSessionRequest {
        provider: selector.clone(),
        system_prompt: system_prompt.map(str::to_owned),
        cwd,
        // mu-f1a0: `mu ask` is a batch-shaped one-shot — the 5m
        // default tier is correct (no human gaps to survive).
        cache_ttl: None,
        // mu-7e21: no autonomy grant from `mu ask` yet — a future
        // `--autonomy` flag fills this (operator-deferred; solo.toml
        // is the first frontend knob).
        autonomy: None,
        // mu-n25a: `mu ask` does not restrict side-effects (root default,
        // unrestricted ceiling). solo.toml's `[session] max_side_effects`
        // is the first operator knob.
        max_side_effects: None,
        // mu-779s: per-session max_turns cap. `None` → use provider default.
        // `Some(0)` → disable cap entirely.
        max_turns,
    };
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": CreateSessionRequest::METHOD,
        "params": body,
    });
    write_line(stdin, &req).await?;

    loop {
        let line = read_line(stdout).await?;
        if line.get("id") == Some(&Value::from(id)) {
            if let Some(error) = line.get("error") {
                bail!("create_session failed: {error}");
            }
            let result = line
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("create_session response missing `result`"))?;
            let resp: CreateSessionResponse =
                serde_json::from_value(result).context("parse CreateSessionResponse")?;
            return Ok(resp.session_id);
        }
        // Other notifications (none expected this early) — ignore.
    }
}

/// Send the ask and drain notifications until `session.done`. Returns
/// the assistant text plus the done event's `stop_reason` (None when
/// the daemon omits it — older daemons or malformed events), so the
/// caller can distinguish a complete answer from a truncated one
/// (mu-1mvq).
pub(crate) async fn ask_and_drain(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    session_id: &str,
    prompt: &str,
    next_id: &mut u64,
) -> Result<(String, Option<String>)> {
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

    // Per-turn assembly: deltas stream into `current`; each turn's
    // `session.assistant_text_finalized` (mu-wk2) replaces that turn's
    // accumulated deltas with the authoritative final text. The two can
    // differ — e.g. the ollama provider's text-dialect tool-call rescue
    // (mu-ollama-qwen-tool-dialect-yfl0) strips leaked markup from the
    // final message that already went out as deltas.
    let mut finalized = String::new();
    let mut current = String::new();
    // mu-upk2: reasoning is surfaced on STDERR so stdout stays exactly the
    // answer. Deltas accumulate here; thinking_finalized flushes the block.
    let mut thinking_current = String::new();
    let mut got_done = false;
    let mut got_response = false;
    let mut stop_reason: Option<String> = None;

    loop {
        let line = read_line(stdout).await?;
        match line.get("method").and_then(Value::as_str) {
            Some("session.text_delta") => {
                if line["params"]["session_id"] == session_id {
                    if let Some(delta) = line["params"]["delta"].as_str() {
                        current.push_str(delta);
                    }
                }
            }
            Some("session.assistant_text_finalized") => {
                if line["params"]["session_id"] == session_id {
                    if let Some(text) = line["params"]["text"].as_str() {
                        finalized.push_str(text);
                        current.clear();
                    }
                }
            }
            Some("session.thinking_delta") => {
                if line["params"]["session_id"] == session_id {
                    if let Some(delta) = line["params"]["delta"].as_str() {
                        thinking_current.push_str(delta);
                    }
                }
            }
            Some("session.thinking_finalized") => {
                if line["params"]["session_id"] == session_id {
                    // Reasoning → stderr (stdout is reserved for the answer).
                    let text = line["params"]["text"].as_str().unwrap_or("");
                    let body = if text.is_empty() {
                        thinking_current.as_str()
                    } else {
                        text
                    };
                    if !body.trim().is_empty() {
                        eprintln!("[thinking] {body}");
                    }
                    thinking_current.clear();
                }
            }
            Some("session.tool_call_started") => {
                if line["params"]["session_id"] == session_id {
                    // mu-upk2: surface tool calls on stderr (these were
                    // dropped "in v1"). The streamed session.tool_call_delta
                    // fragments are intentionally NOT echoed here — partial
                    // JSON is noise in a headless answer pipe; the finalized
                    // call below is the useful unit.
                    let name = line["params"]["tool_name"].as_str().unwrap_or("?");
                    let args = &line["params"]["arguments"];
                    eprintln!("[tool] {name} {args}");
                }
            }
            Some("session.tool_call_completed") => {
                if line["params"]["session_id"] == session_id {
                    let kind = line["params"]["outcome"]["kind"].as_str().unwrap_or("?");
                    eprintln!("[tool result: {kind}]");
                }
            }
            Some("session.done") => {
                if line["params"]["session_id"] == session_id {
                    got_done = true;
                    stop_reason = line["params"]["stop_reason"].as_str().map(str::to_owned);
                }
            }
            Some("session.error") => {
                if line["params"]["session_id"] == session_id {
                    let msg = line["params"]["message"].as_str().unwrap_or("(no message)");
                    bail!("session error: {msg}");
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
                // Else: streamed tool-arg deltas (session.tool_call_delta) or
                // other notifications — not surfaced in the headless pipe.
            }
        }
        if got_done && got_response {
            // `current` is normally empty here (every turn finalizes);
            // keep it as a defensive fallback for providers/paths that
            // never emit assistant_text_finalized.
            finalized.push_str(&current);
            return Ok((finalized, stop_reason));
        }
    }
}

pub(crate) async fn write_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    stdin.write_all(s.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

pub(crate) async fn read_line(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut line = String::new();
    let n = stdout.read_line(&mut line).await?;
    if n == 0 {
        bail!("mu serve closed stdout unexpectedly");
    }
    serde_json::from_str(line.trim_end_matches('\n')).context("parse JSON line from daemon")
}
