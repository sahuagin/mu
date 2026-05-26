//! `index_recall` tool — search the code index for symbols and concepts.
//!
//! Delegates to an LSP server (code-index-lsp) via the mu-core LspClient.
//! Registered when the daemon has an active LSP connection; absent otherwise.

use std::sync::Arc;

use async_trait::async_trait;
use mu_core::agent::{Tool, ToolResult, ToolSpec};
use mu_core::lsp_client::LspClient;
use serde_json::{json, Value};
use tokio::sync::oneshot;

pub struct IndexRecallTool {
    lsp: Arc<LspClient>,
}

impl IndexRecallTool {
    pub fn new(lsp: Arc<LspClient>) -> Self {
        Self { lsp }
    }
}

#[async_trait]
impl Tool for IndexRecallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "index_recall",
            "Search the code index for symbols, functions, types, and concepts. \
             Returns ranked results with file locations. Use for codebase \
             orientation, finding where a concept is implemented, or locating \
             related code.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language or symbol name to search for"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results (default 10)",
                        "default": 10,
                        "minimum": 1,
                        "maximum": 50
                    }
                },
                "required": ["query"]
            }),
        )
        .with_display("Code index search")
        .with_when(
            "Investigating a codebase, finding where a concept is implemented, \
             locating symbols or types, orienting in an unfamiliar project.",
        )
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if query.is_empty() {
            return Err("query must be a non-empty string".to_string());
        }
        Ok(())
    }

    async fn execute(&self, arguments: Value, _cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;

        match self.lsp.workspace_symbol(query, limit).await {
            Ok(results) => {
                if results.is_empty() {
                    return ToolResult {
                        content: format!("No results for query: {query}"),
                        is_error: false,
                    };
                }
                let mut out = format!("{} results for \"{}\":\n\n", results.len(), query);
                for r in &results {
                    out.push_str(&format!(
                        "  {} at {}:{}\n",
                        r.name,
                        r.file,
                        r.start_line + 1,
                    ));
                }
                ToolResult {
                    content: out,
                    is_error: false,
                }
            }
            Err(e) => ToolResult {
                content: format!("index_recall error: {e}"),
                is_error: true,
            },
        }
    }
}
