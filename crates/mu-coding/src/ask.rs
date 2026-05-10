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

/// Run a single `mu ask` invocation. Flags (`provider`, `model`,
/// `tools`) are forwarded to the spawned `mu serve`.
pub async fn run(
    prompt: String,
    provider: String,
    model: Option<String>,
    tools: String,
    ephemeral: bool,
) -> Result<()> {
    let mut child = spawn_serve(&provider, model.as_deref(), &tools, ephemeral)?;
    let mut stdin = child
        .stdin
        .take()
        .context("child stdin not captured")?;
    let stdout = child.stdout.take().context("child stdout not captured")?;
    let mut stdout = BufReader::new(stdout);

    let mut next_id: u64 = 1;

    let session_id = create_session(&mut stdin, &mut stdout, &mut next_id).await?;
    let text = ask_and_drain(
        &mut stdin,
        &mut stdout,
        &session_id,
        &prompt,
        &mut next_id,
    )
    .await?;

    println!("{}", text);

    // Closing stdin signals the daemon to exit cleanly.
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

fn spawn_serve(
    provider: &str,
    model: Option<&str>,
    tools: &str,
    ephemeral: bool,
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
    cmd.arg("serve")
        .arg("--provider")
        .arg(provider);
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    if !tools.is_empty() {
        cmd.arg("--tools").arg(tools);
    }
    if ephemeral {
        cmd.arg("--ephemeral");
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
) -> Result<String> {
    let id = *next_id;
    *next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": CreateSessionRequest::METHOD,
        "params": {
            // v1: the daemon's hardcoded provider is used regardless;
            // the field is required by mu-001's schema, so we send a
            // valid placeholder.
            "provider": { "kind": "anthropic_api", "model": "irrelevant" }
        }
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
                // Else: tool events or other notifications — drop in v1.
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
    serde_json::from_str(line.trim_end_matches('\n')).context("parse JSON line from daemon")
}
