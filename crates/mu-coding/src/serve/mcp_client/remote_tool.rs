//! [`RemoteMcpTool`] — one remote MCP tool adapted to mu's `Tool` trait.
//!
//! The agent loop never learns the tool came over the wire: `spec()` serves
//! a `ToolSpec` translated once at import time from the server's `tools/list`
//! entry, and `execute()` delegates to `tools/call` on the shared peer,
//! normalizing the MCP result shape into mu's `ToolResult`.

use std::sync::Arc;

use async_trait::async_trait;
use mu_core::agent::{Tool, ToolResult, ToolSpec};
use rmcp::model::CallToolRequestParams;
use serde_json::Value;
use tokio::sync::oneshot;

use super::McpPeer;

pub struct RemoteMcpTool {
    /// Config-level server label, for error messages.
    server: String,
    /// The tool's name on the remote server (pre-prefix).
    remote_name: String,
    /// Translated-once local spec (possibly prefixed name).
    spec: ToolSpec,
    peer: Arc<McpPeer>,
}

impl RemoteMcpTool {
    pub fn new(
        server: &str,
        prefix: Option<&str>,
        def: &rmcp::model::Tool,
        peer: Arc<McpPeer>,
    ) -> Self {
        Self {
            server: server.to_owned(),
            remote_name: def.name.to_string(),
            spec: translate_spec(prefix, def),
            peer,
        }
    }
}

/// Translate a remote `tools/list` entry into a mu `ToolSpec`. The MCP
/// `inputSchema` is already a JSON Schema object, which is exactly what
/// `ToolSpec` carries — no shape conversion, just an optional name prefix.
fn translate_spec(prefix: Option<&str>, def: &rmcp::model::Tool) -> ToolSpec {
    let local_name = match prefix {
        Some(p) if !p.is_empty() => format!("{p}{}", def.name),
        _ => def.name.to_string(),
    };
    let description = def.description.as_deref().unwrap_or_default().to_owned();
    let schema = Value::Object((*def.input_schema).clone());
    ToolSpec::new(local_name, description, schema)
}

/// Normalize an MCP `CallToolResult` into mu's `ToolResult`: text content
/// parts joined by newlines; `isError` mapped through (absent => success).
/// Non-text parts (images, resources) are skipped — if a result carries
/// only structured content, that is serialized instead so the model gets
/// *something* actionable rather than an empty string.
fn normalize_result(res: &rmcp::model::CallToolResult) -> ToolResult {
    let texts: Vec<&str> = res
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect();
    let content = if texts.is_empty() {
        res.structured_content
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default()
    } else {
        texts.join("\n")
    };
    ToolResult {
        content,
        is_error: res.is_error.unwrap_or(false),
    }
}

#[async_trait]
impl Tool for RemoteMcpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, arguments: Value, cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let mut params = CallToolRequestParams::new(self.remote_name.clone());
        if let Value::Object(map) = arguments {
            params = params.with_arguments(map);
        }
        let call = self.peer.call_tool(params);
        tokio::select! {
            biased;
            _ = cancel_rx => ToolResult {
                content: format!("{} cancelled", self.spec.name),
                is_error: true,
            },
            res = call => match res {
                Ok(r) => normalize_result(&r),
                Err(e) => ToolResult {
                    content: format!(
                        "MCP call {}::{} failed: {e}",
                        self.server, self.remote_name
                    ),
                    is_error: true,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, Content};
    use serde_json::json;

    fn def(name: &str, desc: &str) -> rmcp::model::Tool {
        let schema = match json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }) {
            Value::Object(o) => o,
            _ => unreachable!(),
        };
        rmcp::model::Tool::new(name.to_owned(), desc.to_owned(), Arc::new(schema))
    }

    #[test]
    fn translate_spec_carries_name_description_schema() {
        let spec = translate_spec(None, &def("code_recall", "hybrid retrieval"));
        assert_eq!(spec.name, "code_recall");
        assert_eq!(spec.description, "hybrid retrieval");
        assert_eq!(
            spec.input_schema["properties"]["query"]["type"],
            json!("string")
        );
    }

    #[test]
    fn translate_spec_applies_prefix_when_nonempty() {
        let spec = translate_spec(Some("code_index."), &def("code_status", ""));
        assert_eq!(spec.name, "code_index.code_status");
        // Empty prefix behaves like no prefix.
        let spec = translate_spec(Some(""), &def("code_status", ""));
        assert_eq!(spec.name, "code_status");
    }

    #[test]
    fn normalize_result_joins_text_and_maps_is_error() {
        let ok =
            CallToolResult::success(vec![Content::text("line one"), Content::text("line two")]);
        let r = normalize_result(&ok);
        assert_eq!(r.content, "line one\nline two");
        assert!(!r.is_error);

        let err = CallToolResult::error(vec![Content::text("boom")]);
        let r = normalize_result(&err);
        assert_eq!(r.content, "boom");
        assert!(r.is_error);
    }

    #[test]
    fn normalize_result_empty_content_is_empty_success() {
        let empty = CallToolResult::success(vec![]);
        let r = normalize_result(&empty);
        assert_eq!(r.content, "");
        assert!(!r.is_error);
    }
}
