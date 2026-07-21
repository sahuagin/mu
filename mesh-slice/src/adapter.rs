//! MCP↔NATS edge adapter (mu-wxc4 N10): the ONE place MCP is spoken.
//!
//! CC (and any foreign peer) speaks MCP 1.0 to this adapter; the adapter
//! translates each tool call onto the L1-over-NATS mesh by calling the very
//! same [`CodeIndexProxy`] the fleet uses internally, and renders the typed
//! response back as an MCP tool result. The fleet never speaks MCP; MCP is
//! civilized at the edge, the mesh is native internally. This is the seam
//! that keeps MCP an *adapter*, not the substrate — the correction that
//! sank PR #492.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{json, Map, Value};

use crate::proxy::CodeIndexProxy;

/// The adapter presents the code_index tools CC already knows
/// (`code_recall`, `code_status`) and bridges them to the mesh.
#[derive(Clone)]
pub struct McpNatsAdapter {
    proxy: Arc<CodeIndexProxy>,
}

impl McpNatsAdapter {
    pub fn new(proxy: Arc<CodeIndexProxy>) -> Self {
        Self { proxy }
    }
}

impl ServerHandler for McpNatsAdapter {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    // rmcp's ServerHandler declares these as `fn -> impl Future` (not
    // `async fn`), so implementing them mirrors that shape — same allowance
    // as mu-dialogue's handler.
    #[allow(clippy::manual_async_fn)]
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        async move {
            Ok(ListToolsResult {
                tools: vec![
                    Tool::new(
                        "code_recall",
                        "Hybrid symbol/concept code recall (bridged to the mesh code_index service).",
                        schema(json!({
                            "type": "object",
                            "properties": {
                                "query": {"type": "string"},
                                "limit": {"type": "number"}
                            },
                            "required": ["query"]
                        })),
                    ),
                    Tool::new(
                        "code_status",
                        "code_index health (bridged to the mesh code_index service).",
                        schema(json!({"type": "object", "properties": {}})),
                    ),
                ],
                ..Default::default()
            })
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        let proxy = self.proxy.clone();
        async move {
            let args = request.arguments.unwrap_or_default();
            // Each arm is a pure translation MCP-call → proxy method → JSON.
            // A proxy error (incl. an unauthorized/refused mesh request)
            // becomes an MCP tool error, not a transport failure.
            match request.name.as_ref() {
                "code_recall" => {
                    let query = args
                        .get("query")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let limit = args.get("limit").and_then(Value::as_u64).map(|n| n as u32);
                    match proxy.recall(query, limit).await {
                        Ok(hits) => Ok(ok_json(&hits)),
                        Err(e) => Ok(err_text(e.to_string())),
                    }
                }
                "code_status" => match proxy.status().await {
                    Ok(s) => Ok(ok_json(&s)),
                    Err(e) => Ok(err_text(e.to_string())),
                },
                other => Ok(err_text(format!("unknown tool: {other}"))),
            }
        }
    }
}

fn schema(v: Value) -> Arc<Map<String, Value>> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => Arc::new(Map::new()),
    }
}

fn ok_json<T: serde::Serialize>(value: &T) -> CallToolResult {
    let text =
        serde_json::to_string(value).unwrap_or_else(|e| format!("{{\"encode_error\":\"{e}\"}}"));
    CallToolResult::success(vec![Content::text(text)])
}

fn err_text(msg: String) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg)])
}
