//! Tool registry — `Arc<dyn Tool>` catalog whose schemas are spans
//! in the [`crate::context::RetainedRope`] (mu-nat).
//!
//! Per `specs/architecture/event-sourced-context.md` lines 543-563:
//! "Tool schemas — registered tools' descriptions and parameter
//! schemas are spans. The active tool set IS a retained pointer
//! set over tool-schema spans. Capability attenuation, subagent
//! dispatch, and `--tools` filtering all become pointer-set
//! operations."
//!
//! [`ToolRegistry::register`] adds the tool to the catalog AND
//! emits a [`crate::context::RopeEvent::ToolSchemaRegistered`] event
//! into the rope (with the tool's schema as the span content).
//! [`ToolRegistry::attenuate_with`] is the pointer-set filter:
//! given a [`crate::capability::Capability`], it returns the subset
//! of registered tools the capability permits. The corresponding
//! rope-level operation is [`crate::context::RetainedRope::filter_tools`].

use std::collections::HashMap;
use std::sync::Arc;

use crate::agent::Tool;
use crate::capability::Capability;
use crate::context::{RetainedRope, RetentionClass, Span, SpanKind};

/// Build the rope-local span id for a registered tool's schema.
/// Stable across (un)register cycles — useful for provenance.
pub fn tool_schema_span_id(tool_name: &str) -> String {
    format!("tool-schema:{tool_name}")
}

/// Catalog of registered tools.
///
/// The registry holds `Arc<dyn Tool>` directly (so dispatch is a
/// cheap clone) and maintains a name→tool map. Schemas live in the
/// rope; the registry is the source of truth for "what tool object
/// is dispatched when the model emits a tool call."
#[derive(Default)]
pub struct ToolRegistry {
    /// `tool_name -> Arc<dyn Tool>`.
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.tools.keys().map(String::as_str).collect();
        f.debug_struct("ToolRegistry")
            .field("tools", &names)
            .finish()
    }
}

impl ToolRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool: store it in the catalog AND emit a
    /// [`crate::context::RopeEvent::ToolSchemaRegistered`] event into
    /// `rope` with the tool's schema (description + input_schema)
    /// as the span content.
    ///
    /// If a tool with the same name was already registered, the new
    /// one replaces it in the catalog and a fresh schema span is
    /// emitted (the old one stays in the rope's span list, with its
    /// own provenance — callers who want a clean replace should
    /// call [`Self::unregister`] first).
    pub fn register(&mut self, tool: Arc<dyn Tool>, rope: &mut RetainedRope) {
        let spec = tool.spec();
        let tool_name = spec.name.clone();
        let span = Span::new(
            tool_schema_span_id(&tool_name),
            SpanKind::ToolSchema,
            serialize_schema(&spec),
            RetentionClass::Hot,
        );
        rope.register_tool_schema(tool_name.clone(), span);
        self.tools.insert(tool_name, tool);
    }

    /// Unregister a tool: remove it from the catalog AND emit a
    /// [`crate::context::RopeEvent::ToolSchemaUnregistered`] event
    /// into `rope`. Returns the removed [`Tool`] if any.
    pub fn unregister(
        &mut self,
        tool_name: &str,
        rope: &mut RetainedRope,
    ) -> Option<Arc<dyn Tool>> {
        rope.unregister_tool_schema(tool_name);
        self.tools.remove(tool_name)
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Iterate registered tool names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Iterate registered tools.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.values()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// True iff there are no registered tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Capability attenuation — return the subset of registered
    /// tools whose name is permitted by `cap`. This is the
    /// pointer-set filter described in spec lines 561-563
    /// ("Capability changes are span-set changes"). The result is
    /// what a subagent / delegate would see as its tool set.
    ///
    /// Implementation note: this mirrors the predicate semantics of
    /// [`RetainedRope::filter_tools`] but produces `Arc<dyn Tool>`
    /// clones (so the caller has dispatchable handles, not just
    /// schema spans).
    pub fn attenuate_with(&self, cap: &Capability) -> Vec<Arc<dyn Tool>> {
        self.tools
            .iter()
            .filter(|(name, _)| cap.check_allow(name).is_allowed())
            .map(|(_, tool)| Arc::clone(tool))
            .collect()
    }

    /// The names attenuation accepts under `cap`. Useful for
    /// debug / wire-protocol surfaces where the full tool object is
    /// not needed.
    pub fn attenuated_names<'a>(&'a self, cap: &Capability) -> Vec<&'a str> {
        self.tools
            .keys()
            .filter(|name| cap.check_allow(name).is_allowed())
            .map(String::as_str)
            .collect()
    }

    /// Project the attenuation onto the rope: return the slice of
    /// tool-schema spans whose corresponding tool name is permitted
    /// by `cap`. This is the rope-level view of the same filter,
    /// suitable for rendering an attenuated tool set into provider
    /// messages via [`crate::context::ProviderRenderer`].
    ///
    /// Spans returned are borrowed from `rope` — substrate
    /// unchanged, immutable projection. Spans whose corresponding
    /// tool is no longer in the registry (e.g., unregistered after
    /// the schema was emitted) are also filtered out, so the view
    /// reflects the current registry + cap intersection.
    pub fn filter_tool_spans<'r>(&self, rope: &'r RetainedRope, cap: &Capability) -> Vec<&'r Span> {
        rope.filter_tools(|span| {
            let Some(name) = span.id.strip_prefix("tool-schema:") else {
                return false;
            };
            self.tools.contains_key(name) && cap.check_allow(name).is_allowed()
        })
    }
}

/// Serialize a [`crate::agent::ToolSpec`] into the span content. The
/// shape is `{name, description, input_schema}` — the schema-as-span
/// representation per spec line 543. Skips serde-failure surfaces
/// (the schema is JSON-serializable by construction); on the
/// unhappy path the span content carries a parse-error message
/// rather than the schema. Renderers can still surface the span;
/// callers debugging registration get a textual hint.
fn serialize_schema(spec: &crate::agent::ToolSpec) -> String {
    let payload = serde_json::json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    });
    serde_json::to_string(&payload).unwrap_or_else(|e| format!("<schema-serialize-error: {e}>"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Tool, ToolResult, ToolSpec};
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashSet;
    use tokio::sync::oneshot;

    /// Test fixture: a no-op tool whose spec carries a recognizable
    /// name + description, so registration → schema span → filter
    /// flows can be asserted end-to-end.
    struct StubTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(
                self.name,
                format!("stub tool: {}", self.name),
                json!({"type": "object", "properties": {}, "required": []}),
            )
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _cancel_rx: oneshot::Receiver<()>,
        ) -> ToolResult {
            ToolResult {
                content: format!("stub-result:{}", self.name),
                is_error: false,
            }
        }
    }

    fn stub(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(StubTool { name })
    }

    fn allow(names: &[&str]) -> Capability {
        Capability {
            allowed_tools: Some(names.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn register_adds_tool_and_schema_span() {
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        assert_eq!(reg.len(), 1);
        assert!(reg.get("read").is_some());
        assert_eq!(rope.len(), 1);
        assert_eq!(rope.spans()[0].kind, SpanKind::ToolSchema);
        assert_eq!(rope.spans()[0].id, "tool-schema:read");
        // Content carries the serialized schema.
        assert!(rope.spans()[0].content.contains("\"name\":\"read\""));
    }

    #[test]
    fn unregister_removes_tool_and_span() {
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        reg.register(stub("write"), &mut rope);
        assert_eq!(reg.len(), 2);

        let removed = reg.unregister("read", &mut rope).expect("removed");
        assert_eq!(removed.spec().name, "read");
        assert_eq!(reg.len(), 1);
        assert!(reg.get("read").is_none());
        assert_eq!(rope.len(), 1);
        assert_eq!(rope.spans()[0].id, "tool-schema:write");
    }

    #[test]
    fn attenuate_with_unrestricted_cap_returns_all() {
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        reg.register(stub("write"), &mut rope);
        reg.register(stub("bash"), &mut rope);

        let cap = Capability::root(); // None on allowed_tools → all allowed
        let allowed = reg.attenuate_with(&cap);
        assert_eq!(allowed.len(), 3);
    }

    #[test]
    fn attenuate_with_narrowed_cap_returns_subset() {
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        reg.register(stub("write"), &mut rope);
        reg.register(stub("bash"), &mut rope);

        // Only read + bash are in the cap.
        let cap = allow(&["read", "bash"]);
        let allowed = reg.attenuate_with(&cap);
        assert_eq!(allowed.len(), 2);
        let names: HashSet<String> = allowed.iter().map(|t| t.spec().name).collect();
        assert!(names.contains("read"));
        assert!(names.contains("bash"));
        assert!(!names.contains("write"));
    }

    #[test]
    fn attenuated_names_matches_attenuate_with() {
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        reg.register(stub("write"), &mut rope);

        let cap = allow(&["read"]);
        let names: HashSet<&str> = reg.attenuated_names(&cap).into_iter().collect();
        assert_eq!(names, HashSet::from(["read"]));
    }

    #[test]
    fn filter_tool_spans_projects_attenuation_onto_rope() {
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        reg.register(stub("write"), &mut rope);
        reg.register(stub("bash"), &mut rope);

        let cap = allow(&["read", "bash"]);
        let view = reg.filter_tool_spans(&rope, &cap);
        assert_eq!(view.len(), 2);
        let ids: HashSet<&str> = view.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains("tool-schema:read"));
        assert!(ids.contains("tool-schema:bash"));
    }

    #[test]
    fn filter_tool_spans_excludes_stale_schema_spans() {
        // Regression: a schema span emitted but for a tool that's
        // been unregistered must not appear in the attenuated view.
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        reg.unregister("read", &mut rope);
        // The unregister path also strips the span, but defense-in-
        // depth: filter_tool_spans must double-check via the
        // registry, in case a caller manually pushed a stale span.
        rope.push(Span::new(
            "tool-schema:ghost",
            SpanKind::ToolSchema,
            "{}",
            RetentionClass::Hot,
        ));
        let cap = Capability::root();
        let view = reg.filter_tool_spans(&rope, &cap);
        assert!(
            view.is_empty(),
            "stale tool-schema span must not be projected"
        );
    }

    #[test]
    fn attenuation_pointer_set_difference_is_observable() {
        // Spec line 558: capability attenuation produces a pointer-
        // set difference observable via the rope API. Concretely:
        // |registered| - |attenuated| = |denied|.
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        reg.register(stub("write"), &mut rope);
        reg.register(stub("bash"), &mut rope);

        let cap = allow(&["read"]);
        let all: HashSet<String> = reg.names().map(String::from).collect();
        let allowed: HashSet<String> = reg
            .attenuated_names(&cap)
            .into_iter()
            .map(String::from)
            .collect();
        let denied: HashSet<String> = all.difference(&allowed).cloned().collect();
        assert_eq!(
            denied,
            HashSet::from(["write".to_string(), "bash".to_string()])
        );
    }

    #[test]
    fn provenance_resolves_for_registered_tool() {
        let mut reg = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        reg.register(stub("read"), &mut rope);
        let prov = rope.provenance("tool-schema:read").expect("provenance");
        match prov {
            crate::context::RopeEvent::ToolSchemaRegistered { tool_name, .. } => {
                assert_eq!(tool_name, "read");
            }
            other => panic!("expected ToolSchemaRegistered, got {other:?}"),
        }
    }
}
