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

use mu_core::agent::deferred_tools::DeferredTools;
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
    /// mu-kex4.6.3: when true, rank semantically (t4c `SemanticRanker` over a
    /// config-resolved embedder) with a lexical fallback on any embedder
    /// failure; when false, lexical only. Sourced from `[index].semantic_discover`
    /// (default false). `discover` is a rare orientation call, so the per-call
    /// embed cost is acceptable when enabled.
    semantic: bool,
    /// mu-t4l5e: the session's deferred-tool handle, when schemas are
    /// deferred. Entries whose schema is still withheld are marked in the
    /// result text so the model knows a call by name loads them.
    deferred: Option<Arc<DeferredTools>>,
}

impl DiscoverTool {
    pub fn new(
        tools: Arc<Vec<Arc<dyn Tool>>>,
        skills: Arc<Vec<LoadedSkill>>,
        capability: Arc<Mutex<Capability>>,
        semantic: bool,
        deferred: Option<Arc<DeferredTools>>,
    ) -> Self {
        Self {
            tools,
            skills,
            capability,
            semantic,
            deferred,
        }
    }
}

#[async_trait]
impl Tool for DiscoverTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "discover",
            "Find a tool or skill by intent. Use this FIRST whenever you need a capability you \
             do not see in your tool list — BEFORE guessing a tool name, writing a workaround, \
             or shelling out via bash; a ready-made capability probably exists and this is how \
             you find it. Given a free-text description of what you want to do, returns the \
             ranked capabilities (tools + skills + host CLIs) available to THIS session that \
             match. Read-only; it lists, it does not run anything.",
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
        // mu-cvm5: explicit read-only opt-in (default now fails closed).
        // discover ranks the session's own tools; read-only projection.
        .read_only()
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

        // mu-kex4.6.3: semantic ranking (opt-in via [index].semantic_discover).
        // `discover` is a rare orientation call, so the per-call embed cost is
        // acceptable when enabled. The embed is a blocking HTTP call, so it runs
        // on a blocking thread. ANY failure (no embedder configured, network /
        // Ollama down, embed error) falls through to the lexical floor below —
        // enabling semantic never breaks discovery.
        if self.semantic {
            // Bind the build result to a `let` so the non-Send `Registry`
            // temporary (it holds `Box<dyn RegistrySource>`) is dropped here,
            // not held across the `.await` below — which would make this
            // future non-Send.
            let built =
                mu_core::t4c_source::build_manifest_for_tools(&self.tools, &cap, &self.skills)
                    .build();
            match built {
                Ok(tree) => {
                    let intent_owned = intent.to_owned();
                    match tokio::task::spawn_blocking(move || {
                        mu_core::t4c_source::discover_view_semantic(&tree, &intent_owned, limit)
                    })
                    .await
                    {
                        Ok(Ok(results)) => {
                            return ToolResult {
                                content: format_results(intent, &results, self.deferred.as_deref()),
                                is_error: false,
                            };
                        }
                        Ok(Err(e)) => tracing::info!(
                            error = %e,
                            "discover: semantic ranking unavailable; using lexical floor"
                        ),
                        Err(e) => tracing::warn!(
                            error = %e,
                            "discover: semantic ranking task failed; using lexical floor"
                        ),
                    }
                }
                Err(e) => {
                    return ToolResult {
                        content: format!("discover: building capability manifest failed: {e}"),
                        is_error: true,
                    };
                }
            }
        }

        // Lexical floor — the default path, and the fallback when semantic
        // ranking is disabled or unavailable. Rebuild the manifest (cheap,
        // in-memory projection) since a semantic attempt consumes the tree.
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
            content: format_results(intent, &results, self.deferred.as_deref()),
            is_error: false,
        }
    }
}

/// Render ranked capabilities as compact, model-readable text. With a
/// deferred handle (mu-t4l5e), a `tool.*` entry whose schema is withheld
/// is marked so the model knows calling it by name is how it loads.
fn format_results(
    intent: &str,
    results: &[mu_core::t4c_source::CapabilityView],
    deferred: Option<&DeferredTools>,
) -> String {
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
        if v.path
            .strip_prefix("tool.")
            .is_some_and(|name| deferred.is_some_and(|d| d.is_withheld(name)))
        {
            out.push_str("  (deferred — call by name to load)");
        }
        if !v.summary.is_empty() {
            out.push_str(&format!("\n    {}", v.summary));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::t4c_source::CapabilityView;

    fn view(path: &str) -> CapabilityView {
        CapabilityView {
            path: path.to_string(),
            summary: format!("summary of {path}"),
            keywords: Vec::new(),
            score: 1.0,
            effects: None,
            allowed_by_session: true,
            disallowed_reason: None,
            source: None,
        }
    }

    /// mu-t4l5e: only a `tool.*` entry whose schema is still withheld gets
    /// the marker — loaded, core, and non-tool paths read as before, and
    /// no handle means no marker anywhere.
    #[test]
    fn format_results_marks_withheld_deferred_tools_only() {
        let deferred = DeferredTools::new(["watch".to_string(), "mailbox".to_string()]);
        deferred.load("mailbox");
        let results = vec![
            view("tool.watch"),
            view("tool.mailbox"),
            view("tool.read"),
            view("skill.watch"),
        ];
        let out = format_results("watch things", &results, Some(&deferred));
        let line = |needle: &str| {
            out.lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no line for {needle} in {out}"))
                .to_string()
        };
        assert!(line("• tool.watch").contains("(deferred — call by name to load)"));
        assert!(
            !line("• tool.mailbox").contains("deferred"),
            "loaded ⇒ plain"
        );
        assert!(!line("• tool.read").contains("deferred"), "core ⇒ plain");
        assert!(
            !line("• skill.watch").contains("deferred"),
            "only tool.* paths map onto the deferred set"
        );
        assert!(!format_results("watch things", &results, None).contains("deferred"));
    }
}
