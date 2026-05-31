//! `discover` — in-loop capability discovery (mu-onq8).
//!
//! Friction B ("folkloric capabilities"): the agent knows tools/skills
//! *might* exist but can't see them at the point of choice, so it guesses
//! or shells out to `bash` (which the strict allowlist blocks). The
//! `capabilities/discover` RPC (mu-kex4.6.4) already ranks the session's
//! permission-attenuated manifest against a free-text intent — but only
//! over the wire. This exposes that exact path as a first-class agent tool
//! so the model can ask "what do I have for X?" directly.
//!
//! Read-only: it projects and ranks the manifest, it never invokes anything.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{Tool, ToolResult, ToolSpec};
use mu_core::capability::Capability;
use mu_core::skill::loader::LoadedSkill;

/// Default top-k when the caller omits `limit`. Matches the RPC handler's
/// `DEFAULT_LIMIT` so the in-loop tool and `capabilities/discover` agree.
const DEFAULT_LIMIT: usize = 20;

/// Agent-facing capability discovery. Constructed per session with the
/// session's *sibling* tools (everything except itself), the daemon's
/// skills, and the session's capability handle — so ranking is over
/// exactly what this session is permitted to use, live.
pub struct DiscoverTool {
    /// Sibling tools to rank (does not include `discover` itself).
    tools: Arc<Vec<Arc<dyn Tool>>>,
    skills: Arc<Vec<LoadedSkill>>,
    /// Session capability; locked + snapshotted per call so a mid-session
    /// attenuation is reflected. Fails closed to the default capability
    /// on a poisoned lock rather than leaking a stale snapshot.
    capability: Arc<Mutex<Capability>>,
}

impl DiscoverTool {
    pub fn new(
        tools: Arc<Vec<Arc<dyn Tool>>>,
        skills: Arc<Vec<LoadedSkill>>,
        capability: Arc<Mutex<Capability>>,
    ) -> Self {
        Self {
            tools,
            skills,
            capability,
        }
    }
}

#[async_trait]
impl Tool for DiscoverTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "discover",
            "Find a tool or skill by intent. Given a free-text description of what you want to \
             do, returns the ranked capabilities (tools + skills) available to THIS session that \
             match. Use this whenever you need a capability and don't know which tool provides it \
             — instead of guessing, or shelling out via bash. Read-only; it lists, it does not run \
             anything.",
            json!({
                "type": "object",
                "properties": {
                    "intent": {
                        "type": "string",
                        "description": "What you're trying to do, in plain language (e.g. \"search code by symbol\", \"compress a file\")."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Max results to return. Default 20."
                    }
                },
                "required": ["intent"]
            }),
        )
    }

    async fn execute(&self, arguments: Value, _cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let intent = arguments
            .get("intent")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if intent.is_empty() {
            return ToolResult {
                content: "discover: `intent` is required (a free-text description of what you \
                          want to do)."
                    .to_owned(),
                is_error: true,
            };
        }
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .max(1);

        // Snapshot the capability (fail closed on poison), then project +
        // rank the permission-attenuated manifest — the same path as the
        // capabilities/discover RPC.
        let cap = self
            .capability
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default();
        let registry =
            mu_core::t4c_source::build_manifest_for_tools(&self.tools, &cap, &self.skills);
        let tree = match registry.build() {
            Ok(t) => t,
            Err(e) => {
                return ToolResult {
                    content: format!("discover: building capability manifest failed: {e}"),
                    is_error: true,
                };
            }
        };
        let results = mu_core::t4c_source::discover_view(&tree, intent, limit);
        ToolResult {
            content: format_results(intent, &results),
            is_error: false,
        }
    }
}

/// Render ranked capabilities as compact, model-readable text.
fn format_results(intent: &str, results: &[mu_core::t4c_source::CapabilityView]) -> String {
    if results.is_empty() {
        return format!(
            "No capabilities matched \"{intent}\". Try a broader intent, or fall back to your \
             core tools."
        );
    }
    let mut out = format!(
        "Capabilities matching \"{intent}\" (best first, {} shown):\n",
        results.len()
    );
    for v in results {
        out.push_str(&format!("\n• {}  (score {:.2})", v.path, v.score));
        if let Some(src) = &v.source {
            out.push_str(&format!("  [{src}]"));
        }
        if !v.allowed_by_session {
            let why = v.disallowed_reason.as_deref().unwrap_or("not permitted");
            out.push_str(&format!("  [unavailable this session: {why}]"));
        }
        if !v.summary.is_empty() {
            out.push_str(&format!("\n    {}", v.summary));
        }
    }
    out
}
