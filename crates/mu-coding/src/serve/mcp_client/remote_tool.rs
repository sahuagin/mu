//! [`RemoteMcpTool`] — one remote MCP tool adapted to mu's `Tool` trait.
//!
//! The agent loop never learns the tool came over the wire: `spec()` serves
//! a `ToolSpec` translated once at import time from the server's `tools/list`
//! entry, and `execute()` delegates to `tools/call` on the shared peer,
//! normalizing the MCP result shape into mu's `ToolResult`.

use std::sync::Arc;

use async_trait::async_trait;
use mu_core::agent::{SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec};
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
    /// `side_effects` is the operator-supplied classification for THIS tool
    /// (already resolved by the importer: per-tool override → server-wide
    /// floor → fail-safe `Execute`). MCP carries no side-effects metadata,
    /// so this is the only honest source — it MUST be supplied by the caller,
    /// never inferred from the remote `def`. (mu-cvm5 / mu-n25a Phase 4)
    pub fn new(
        server: &str,
        prefix: Option<&str>,
        def: &rmcp::model::Tool,
        side_effects: SideEffects,
        peer: Arc<McpPeer>,
    ) -> Self {
        Self {
            server: server.to_owned(),
            remote_name: def.name.to_string(),
            spec: translate_spec(prefix, def, side_effects),
            peer,
        }
    }
}

/// Translate a remote `tools/list` entry into a mu `ToolSpec`. The MCP
/// `inputSchema` is already a JSON Schema object, which is exactly what
/// `ToolSpec` carries — no shape conversion, just an optional name prefix.
///
/// The `policy.side_effects` is set to `side_effects` (the operator's
/// classification, defaulting to the fail-safe `Execute` upstream). The rest
/// of the policy stays at its safe baseline: the runtime cannot vouch for an
/// imported tool's idempotency or retry-safety either, so it carries the
/// conservative defaults and lets the dispatch-boundary side-effects gate
/// (`Capability::check_side_effects`) do the gating. (mu-cvm5)
fn translate_spec(
    prefix: Option<&str>,
    def: &rmcp::model::Tool,
    side_effects: SideEffects,
) -> ToolSpec {
    let local_name = match prefix {
        Some(p) if !p.is_empty() => format!("{p}{}", def.name),
        _ => def.name.to_string(),
    };
    let description = def.description.as_deref().unwrap_or_default().to_owned();
    let schema = Value::Object((*def.input_schema).clone());
    ToolSpec::new(local_name, description, schema).with_policy(ToolPolicy {
        side_effects,
        ..ToolPolicy::default()
    })
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
        let spec = translate_spec(
            None,
            &def("code_recall", "hybrid retrieval"),
            SideEffects::ReadOnly,
        );
        assert_eq!(spec.name, "code_recall");
        assert_eq!(spec.description, "hybrid retrieval");
        assert_eq!(
            spec.input_schema["properties"]["query"]["type"],
            json!("string")
        );
    }

    #[test]
    fn translate_spec_applies_prefix_when_nonempty() {
        let spec = translate_spec(
            Some("code_index."),
            &def("code_status", ""),
            SideEffects::ReadOnly,
        );
        assert_eq!(spec.name, "code_index.code_status");
        // Empty prefix behaves like no prefix.
        let spec = translate_spec(Some(""), &def("code_status", ""), SideEffects::ReadOnly);
        assert_eq!(spec.name, "code_status");
    }

    #[test]
    fn translate_spec_stamps_classified_side_effects() {
        // mu-cvm5: the operator's classification rides into the ToolSpec's
        // policy so the dispatch-boundary gate can act on it.
        let benign = translate_spec(None, &def("code_recall", ""), SideEffects::ReadOnly);
        assert_eq!(benign.policy.side_effects, SideEffects::ReadOnly);
        // Fail-safe default: an unclassified tool is stamped Execute upstream.
        let danger = translate_spec(None, &def("delete_everything", ""), SideEffects::Execute);
        assert_eq!(danger.policy.side_effects, SideEffects::Execute);
    }

    #[test]
    fn read_only_ceiling_refuses_unclassified_mcp_tool() {
        // mu-cvm5 / mu-n25a Phase 4 ACCEPTANCE: a session capped at a
        // ReadOnly side-effects ceiling REFUSES an imported MCP tool of
        // unknown danger. Unclassified MCP tools import as Execute (the
        // fail-safe), so the dispatch-boundary side-effects gate
        // (Capability::check_side_effects) denies them. Without the
        // fail-safe (old behavior: ReadOnly default) this session would
        // have ALLOWED a remote `delete_everything`.
        use mu_core::capability::{Capability, CapabilityCheck};

        // An unclassified import is stamped Execute upstream.
        let spec = translate_spec(
            None,
            &def("delete_everything", "deletes the world"),
            SideEffects::Execute,
        );
        assert_eq!(spec.policy.side_effects, SideEffects::Execute);

        let read_only_session = Capability {
            max_side_effects: Some(SideEffects::ReadOnly),
            ..Default::default()
        };
        match read_only_session.check_side_effects(spec.policy.side_effects) {
            CapabilityCheck::DeniedSideEffectsExceeded { declared, ceiling } => {
                assert_eq!(declared, SideEffects::Execute);
                assert_eq!(ceiling, SideEffects::ReadOnly);
            }
            other => {
                panic!("read-only session must refuse an unclassified MCP tool, got {other:?}")
            }
        }

        // Contrast: an operator-classified read-only MCP tool passes the
        // same ceiling — classification is the deliberate opt-in.
        let classified = translate_spec(None, &def("code_recall", ""), SideEffects::ReadOnly);
        assert!(read_only_session
            .check_side_effects(classified.policy.side_effects)
            .is_allowed());
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
