//! Minimal LSP client for connecting to code-index-lsp (or other LSP
//! servers) over TCP. Sends JSON-RPC with Content-Length framing.
//!
//! Phase 1: workspace/symbol queries only. The daemon connects at
//! startup (if a server is available), registers an `index_recall`
//! tool, and routes tool calls through this client.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("connection failed: {0}")]
    Connect(std::io::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("LSP server error: {0}")]
    Server(String),
    #[error("invalid header from LSP server")]
    InvalidHeader,
}

type Result<T> = std::result::Result<T, LspError>;

/// Lightweight LSP client — just enough to do initialize + workspace/symbol.
#[derive(Debug)]
pub struct LspClient {
    reader: Mutex<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    writer: Mutex<tokio::net::tcp::OwnedWriteHalf>,
    next_id: AtomicU64,
    server_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolResult {
    pub name: String,
    pub kind: u32,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
}

impl LspClient {
    /// Connect to an LSP server at the given TCP address and perform
    /// the initialize handshake.
    pub async fn connect(addr: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr).await.map_err(LspError::Connect)?;
        let (read, write) = stream.into_split();
        let mut client = Self {
            reader: Mutex::new(BufReader::new(read)),
            writer: Mutex::new(write),
            next_id: AtomicU64::new(1),
            server_name: String::new(),
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&mut self) -> Result<()> {
        let resp = self
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "rootUri": null,
                    "capabilities": {}
                }),
            )
            .await?;

        if let Some(info) = resp.get("serverInfo") {
            self.server_name = info
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
        }

        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    /// Send a workspace/symbol query and return parsed results.
    pub async fn workspace_symbol(&self, query: &str, limit: usize) -> Result<Vec<SymbolResult>> {
        let resp = self
            .request("workspace/symbol", json!({"query": query}))
            .await?;

        let symbols = resp.as_array().cloned().unwrap_or_default();
        let results: Vec<SymbolResult> = symbols
            .into_iter()
            .take(limit)
            .filter_map(|s| {
                let name = s.get("name")?.as_str()?.to_string();
                let kind = s.get("kind")?.as_u64()? as u32;
                let loc = s.get("location")?;
                let uri = loc.get("uri")?.as_str()?;
                let file = uri.strip_prefix("file://").unwrap_or(uri).to_string();
                let range = loc.get("range")?;
                let start_line = range.get("start")?.get("line")?.as_u64()? as u32;
                let end_line = range.get("end")?.get("line")?.as_u64()? as u32;
                Some(SymbolResult {
                    name,
                    kind,
                    file,
                    start_line,
                    end_line,
                })
            })
            .collect();
        Ok(results)
    }

    /// Send shutdown + exit.
    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.request("shutdown", Value::Null).await;
        self.notify("exit", json!({})).await?;
        Ok(())
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    // ── JSON-RPC transport ─────────────────────────────────────────

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&msg).await?;
        self.recv_response(id).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send(&msg).await
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        let body = serde_json::to_string(msg)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut writer = self.writer.lock().await;
        writer.write_all(header.as_bytes()).await?;
        writer.write_all(body.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn recv_response(&self, expected_id: u64) -> Result<Value> {
        let mut reader = self.reader.lock().await;
        loop {
            // Read Content-Length header
            let mut header_line = String::new();
            reader.read_line(&mut header_line).await?;
            let content_length: usize = header_line
                .trim()
                .strip_prefix("Content-Length: ")
                .and_then(|s| s.parse().ok())
                .ok_or(LspError::InvalidHeader)?;

            // Read empty line separator
            let mut sep = String::new();
            reader.read_line(&mut sep).await?;

            // Read body
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).await?;

            let resp: Value = serde_json::from_slice(&body)?;

            // Skip notifications (no id field)
            if let Some(id) = resp.get("id").and_then(|v| v.as_u64()) {
                if id == expected_id {
                    if let Some(error) = resp.get("error") {
                        return Err(LspError::Server(error.to_string()));
                    }
                    return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            // Not our response — skip (notification or different id)
        }
    }
}
