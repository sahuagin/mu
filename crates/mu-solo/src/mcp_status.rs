//! Lightweight MCP client for session status subscriptions.
//!
//! Connects to mu-serve's MCP Unix socket, subscribes to a session's
//! status resource, and forwards `mu/session_status` notifications
//! through a std::sync::mpsc channel. No tokio, no rmcp SDK — just
//! line-delimited JSON-RPC over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use mu_core::session_status::SessionStatus;
use serde_json::{json, Value};
use tracing::debug;

/// Spawn a background thread that connects to the MCP socket,
/// subscribes to the given session's status, and forwards
/// SessionStatus updates through the returned receiver.
///
/// Best-effort: if the socket doesn't exist or the connection fails,
/// the receiver will simply never produce values. The TUI falls back
/// to its inline accumulation.
pub fn spawn_status_subscriber(
    session_id: String,
) -> mpsc::Receiver<SessionStatus> {
    let (tx, rx) = mpsc::channel();

    thread::Builder::new()
        .name("mu-solo-mcp-status".into())
        .spawn(move || {
            if let Err(e) = run_subscriber(&session_id, &tx) {
                debug!("MCP status subscriber exited: {e:#}");
            }
        })
        .ok();

    rx
}

fn mcp_socket_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("MU_STATE_DIR") {
        return std::path::PathBuf::from(dir).join("mcp.sock");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home)
        .join(".local/share/mu")
        .join("mcp.sock")
}

fn run_subscriber(
    session_id: &str,
    tx: &mpsc::Sender<SessionStatus>,
) -> anyhow::Result<()> {
    let sock_path = mcp_socket_path();

    // Retry connection for up to 5 seconds (daemon may still be starting).
    let stream = {
        let mut attempts = 0;
        loop {
            match UnixStream::connect(&sock_path) {
                Ok(s) => break s,
                Err(e) => {
                    attempts += 1;
                    if attempts >= 10 {
                        anyhow::bail!("connect to {}: {e}", sock_path.display());
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
    };
    stream.set_read_timeout(None)?;

    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    // MCP initialize handshake
    send_jsonrpc(
        &mut writer,
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "mu-solo", "version": env!("CARGO_PKG_VERSION")}
        }),
    )?;
    let _init_resp = read_response(&mut reader)?;

    // notifications/initialized (no id — it's a notification)
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let mut buf = serde_json::to_vec(&notif)?;
    buf.push(b'\n');
    writer.write_all(&buf)?;
    writer.flush()?;

    // Subscribe to session status
    let uri = format!("mu://session/{session_id}/status");
    send_jsonrpc(
        &mut writer,
        2,
        "resources/subscribe",
        json!({ "uri": uri }),
    )?;
    let _sub_resp = read_response(&mut reader)?;

    debug!(session_id, "MCP status subscription active");

    // Read notifications forever
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Look for our custom mu/session_status notification
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if method == "mu/session_status" {
            if let Some(params) = v.get("params") {
                if let Ok(status) = serde_json::from_value::<SessionStatus>(params.clone()) {
                    if tx.send(status).is_err() {
                        break; // receiver dropped
                    }
                }
            }
        }
    }

    Ok(())
}

fn send_jsonrpc(
    writer: &mut UnixStream,
    id: u64,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut buf = serde_json::to_vec(&req)?;
    buf.push(b'\n');
    writer.write_all(&buf)?;
    writer.flush()?;
    Ok(())
}

fn read_response(reader: &mut BufReader<UnixStream>) -> anyhow::Result<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            anyhow::bail!("EOF during MCP handshake");
        }
        let v: Value = serde_json::from_str(line.trim())?;
        // Skip notifications (no id), return responses (have id)
        if v.get("id").is_some() {
            return Ok(v);
        }
    }
}
