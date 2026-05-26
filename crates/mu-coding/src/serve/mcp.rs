//! MCP server surface — exposes mu's mailbox as MCP tools over a unix socket.
//!
//! Claude-code connects via `--mcp-config` and can call:
//!   mu_peer_hello, mu_mailbox_post, mu_mailbox_list, mu_mailbox_read, mu_mailbox_consume
//!
//! The MCP framing (initialize, tools/list, tools/call) is handled here;
//! the actual mailbox logic delegates to the existing handlers in
//! `handlers/mailbox.rs`.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

use mu_core::protocol::{Request, Response, JSONRPC_VERSION};
use mu_core::transport::NotificationWriter;

use super::daemon_info::DaemonInfo;
use super::handlers::mailbox::*;
use super::sessions::Sessions;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "mu-mailbox";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Start the MCP unix socket listener. Runs until the listener is dropped.
/// Spawns a task per connection.
pub async fn serve_mcp_socket(
    socket_path: PathBuf,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> anyhow::Result<()> {
    // Remove stale socket from a previous run.
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    // Ensure parent directory exists.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    info!("MCP server listening on {}", socket_path.display());

    loop {
        let (stream, _addr) = listener.accept().await?;
        let sessions = sessions.clone();
        let daemon_info = daemon_info.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_mcp_connection(stream, sessions, daemon_info).await {
                debug!("MCP connection ended: {e:#}");
            }
        });
    }
}

/// Default socket path: $MU_STATE_DIR/mcp.sock or ~/.local/share/mu/mcp.sock
pub fn default_mcp_socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("MU_STATE_DIR") {
        return PathBuf::from(dir).join("mcp.sock");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".local/share/mu")
        .join("mcp.sock")
}

async fn handle_mcp_connection(
    stream: tokio::net::UnixStream,
    sessions: Sessions,
    daemon_info: DaemonInfo,
) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        debug!(req = %line, "mcp incoming");

        let req: McpRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = mcp_error(Value::Null, -32700, &format!("parse error: {e}"));
                write_response(&mut writer, &resp).await?;
                continue;
            }
        };

        let response = dispatch_mcp(&req, &sessions, &daemon_info).await;
        if let Some(resp) = response {
            write_response(&mut writer, &resp).await?;
        }
    }
    Ok(())
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &Value,
) -> anyhow::Result<()> {
    let mut buf = serde_json::to_vec(resp)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

// ─── MCP protocol framing ────────────────────────────────────────────

#[derive(Deserialize)]
struct McpRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn mcp_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn mcp_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_result_content(value: Value) -> Value {
    json!({
        "content": [
            {"type": "text", "text": value.to_string()}
        ]
    })
}

async fn dispatch_mcp(
    req: &McpRequest,
    sessions: &Sessions,
    daemon_info: &DaemonInfo,
) -> Option<Value> {
    let id = req.id.clone().unwrap_or(Value::Null);

    if req.jsonrpc != "2.0" {
        return Some(mcp_error(id, -32600, "invalid jsonrpc version"));
    }

    match req.method.as_str() {
        "initialize" => Some(mcp_ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": SERVER_VERSION
                }
            }),
        )),
        "notifications/initialized" => None,
        "tools/list" => Some(mcp_ok(id, json!({"tools": tools_list()}))),
        "tools/call" => {
            let name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = req.params.get("arguments").cloned().unwrap_or(Value::Null);
            let result = dispatch_tool(name, arguments, sessions, daemon_info).await;
            match result {
                Ok(v) => Some(mcp_ok(id, tool_result_content(v))),
                Err(msg) => Some(mcp_error(id, -32000, &msg)),
            }
        }
        "ping" => Some(mcp_ok(id, json!({}))),
        other => {
            warn!("mcp: unknown method: {other}");
            Some(mcp_error(id, -32601, &format!("method not found: {other}")))
        }
    }
}

// ─── Tool definitions ────────────────────────────────────────────────

fn tools_list() -> Value {
    json!([
        {
            "name": "mu_peer_hello",
            "description": "Request a mailbox peer handle from a target session. Required before posting messages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to_session_id": {"type": "string", "description": "Target session to request a handle from"},
                    "from_daemon_id": {"type": "string", "description": "This daemon's ID"},
                    "from_session_id": {"type": "string", "description": "Requesting session's ID"},
                    "want_method": {"type": "string", "description": "Method to request access to (typically 'mailbox.post')"}
                },
                "required": ["to_session_id", "from_daemon_id", "from_session_id"]
            }
        },
        {
            "name": "mu_mailbox_post",
            "description": "Post a message to a session's mailbox. Requires a peer handle from mu_peer_hello.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to_session_id": {"type": "string", "description": "Recipient session ID"},
                    "peer_handle": {"type": "string", "description": "Peer handle obtained from mu_peer_hello"},
                    "from_daemon_id": {"type": "string", "description": "Sender's daemon ID"},
                    "from_session_id": {"type": "string", "description": "Sender's session ID"},
                    "kind": {"type": "string", "description": "Message kind (e.g. 'task_result', 'fyi')"},
                    "subject": {"type": "string", "description": "Short subject line"},
                    "body": {"description": "Message body (any JSON value)"}
                },
                "required": ["to_session_id", "peer_handle", "from_daemon_id", "from_session_id", "kind", "subject", "body"]
            }
        },
        {
            "name": "mu_mailbox_list",
            "description": "List messages in a session's mailbox (metadata only). Use mu_mailbox_read for full body.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Session whose mailbox to list"},
                    "since_seq": {"type": "number", "description": "Only return messages with seq >= this value"},
                    "include_consumed": {"type": "boolean", "description": "Include already-consumed messages (default false)"}
                },
                "required": ["session_id"]
            }
        },
        {
            "name": "mu_mailbox_read",
            "description": "Read the full body of a single mailbox message by sequence number.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Session whose mailbox to read from"},
                    "seq": {"type": "number", "description": "Sequence number of the message to read"}
                },
                "required": ["session_id", "seq"]
            }
        },
        {
            "name": "mu_mailbox_consume",
            "description": "Mark messages as consumed (acknowledged). Idempotent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Session whose messages to consume"},
                    "seqs": {"type": "array", "items": {"type": "number"}, "description": "Sequence numbers to mark consumed"}
                },
                "required": ["session_id", "seqs"]
            }
        }
    ])
}

// ─── Tool dispatch → existing mailbox handlers ───────────────────────

async fn dispatch_tool(
    name: &str,
    arguments: Value,
    sessions: &Sessions,
    daemon_info: &DaemonInfo,
) -> Result<Value, String> {
    match name {
        "mu_peer_hello" => {
            let to_session_id = str_field(&arguments, "to_session_id")?;
            let from_daemon_id = str_field(&arguments, "from_daemon_id")?;
            let from_session_id = str_field(&arguments, "from_session_id")?;
            let want_method = arguments
                .get("want_method")
                .and_then(|v| v.as_str())
                .unwrap_or("mailbox.post");

            let rpc_params = json!({
                "to_session_id": to_session_id,
                "from": {
                    "daemon_id": from_daemon_id,
                    "session_id": from_session_id,
                    "advertised_capabilities": []
                },
                "want": { "method": want_method }
            });
            let request = make_internal_request("peer.hello", rpc_params);
            let response = handle_peer_hello(request, sessions.clone(), daemon_info.clone());
            extract_result(response)
        }
        "mu_mailbox_post" => {
            let rpc_params = json!({
                "to_session_id": str_field(&arguments, "to_session_id")?,
                "peer_handle": str_field(&arguments, "peer_handle")?,
                "from": {
                    "daemon_id": str_field(&arguments, "from_daemon_id")?,
                    "session_id": str_field(&arguments, "from_session_id")?,
                },
                "kind": str_field(&arguments, "kind")?,
                "subject": str_field(&arguments, "subject")?,
                "body": arguments.get("body").cloned().unwrap_or(Value::Null),
            });
            let request = make_internal_request("mailbox.post", rpc_params);
            let notif = NotificationWriter::sink();
            let response =
                handle_mailbox_post(request, sessions.clone(), notif, daemon_info.clone()).await;
            extract_result(response)
        }
        "mu_mailbox_list" => {
            let rpc_params = json!({
                "session_id": str_field(&arguments, "session_id")?,
                "since_seq": arguments.get("since_seq").cloned(),
                "include_consumed": arguments.get("include_consumed").and_then(|v| v.as_bool()).unwrap_or(false),
            });
            let request = make_internal_request("mailbox.list", rpc_params);
            let response = handle_mailbox_list(request, sessions.clone());
            extract_result(response)
        }
        "mu_mailbox_read" => {
            let seq = arguments
                .get("seq")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "missing required field: seq".to_string())?;
            let rpc_params = json!({
                "session_id": str_field(&arguments, "session_id")?,
                "seq": seq,
            });
            let request = make_internal_request("mailbox.read", rpc_params);
            let response = handle_mailbox_read(request, sessions.clone());
            extract_result(response)
        }
        "mu_mailbox_consume" => {
            let seqs = arguments
                .get("seqs")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect::<Vec<_>>())
                .unwrap_or_default();
            let rpc_params = json!({
                "session_id": str_field(&arguments, "session_id")?,
                "seqs": seqs,
            });
            let request = make_internal_request("mailbox.consume", rpc_params);
            let response = handle_mailbox_consume(request, sessions.clone());
            extract_result(response)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn str_field(args: &Value, field: &str) -> Result<String, String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required field: {field}"))
}

fn make_internal_request(method: &str, params: Value) -> Request<Value> {
    Request {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: json!(1),
        method: method.to_string(),
        params,
    }
}

fn extract_result(response: Response<Value>) -> Result<Value, String> {
    match response {
        Response::Ok { result, .. } => Ok(result),
        Response::Err { error, .. } => Err(format!("{}: {}", error.code, error.message)),
    }
}

