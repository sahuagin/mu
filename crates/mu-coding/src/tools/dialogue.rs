//! Per-session identity binding for the mu-dialogue MCP tools.
//!
//! The dialogue server is reached over a *daemon-shared* MCP connection — one
//! pipe carries every session's traffic, and the handshake fires once at daemon
//! startup, so the connection itself cannot announce a per-session identity. The
//! only place a session's id is known at call time is the tool layer (a
//! `Tool::execute` receives no session context), which is exactly how
//! `spawn_worker` / `watch` carry the calling session's id. This wrapper applies
//! the same idiom to outbound dialogue: it binds an argument field to this
//! session's peer id `mu:<daemon_id>:<session_id>` so the model never has to
//! supply — or can spoof — who it is on the channel.
//!
//! - `dialogue_say.from` is **forced**: identity is authoritative.
//! - `dialogue_poll.to` is **defaulted**: the common case is polling your own
//!   inbox (no argument needed), but observing another peer's inbox stays legal.

use std::sync::Arc;

use async_trait::async_trait;
use mu_core::agent::{Tool, ToolResult, ToolSpec};
use serde_json::Value;
use tokio::sync::oneshot;

/// How the bound field is applied.
#[derive(Clone, Copy)]
pub enum DialogueBind {
    /// Always overwrite (identity is authoritative — `dialogue_say.from`).
    Force,
    /// Fill only when the caller omitted it or passed null (`dialogue_poll.to`).
    Default,
}

/// Wraps a dialogue tool, binding one argument field to this session's peer id.
pub struct SessionDialogueTool {
    inner: Arc<dyn Tool>,
    identity: String,
    field: &'static str,
    bind: DialogueBind,
}

impl SessionDialogueTool {
    pub fn new(
        inner: Arc<dyn Tool>,
        identity: String,
        field: &'static str,
        bind: DialogueBind,
    ) -> Self {
        Self {
            inner,
            identity,
            field,
            bind,
        }
    }

    /// Apply the identity binding to a tool-call argument object in place.
    /// Non-object arguments are left untouched — the inner tool will reject
    /// them with its own error.
    fn apply(&self, arguments: &mut Value) {
        let Some(obj) = arguments.as_object_mut() else {
            return;
        };
        let set = match self.bind {
            DialogueBind::Force => true,
            DialogueBind::Default => obj.get(self.field).map(Value::is_null).unwrap_or(true),
        };
        if set {
            obj.insert(self.field.to_string(), Value::String(self.identity.clone()));
        }
    }
}

#[async_trait]
impl Tool for SessionDialogueTool {
    fn spec(&self) -> ToolSpec {
        // Hide the bound field from the model: drop it from `required` (so the
        // agent isn't asked for it), and for a forced field drop it from
        // `properties` too (overriding it is meaningless). A short note is
        // appended to the description so the behaviour is self-explaining.
        let mut spec = self.inner.spec();
        if let Some(schema) = spec.input_schema.as_object_mut() {
            if let Some(Value::Array(required)) = schema.get_mut("required") {
                required.retain(|v| v.as_str() != Some(self.field));
            }
            if matches!(self.bind, DialogueBind::Force) {
                if let Some(Value::Object(props)) = schema.get_mut("properties") {
                    props.remove(self.field);
                }
            }
        }
        let note = match self.bind {
            DialogueBind::Force => format!(
                " `{}` is set automatically to this session's peer id ({}); do not supply it.",
                self.field, self.identity
            ),
            DialogueBind::Default => format!(
                " `{}` defaults to this session's peer id ({}) — omit it to use your own inbox.",
                self.field, self.identity
            ),
        };
        spec.description.push_str(&note);
        spec
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        // Validate against the effective args (binding applied), so a forced
        // field the inner tool requires doesn't trip its pre-flight check.
        let mut effective = arguments.clone();
        self.apply(&mut effective);
        self.inner.validate(&effective)
    }

    async fn execute(&self, mut arguments: Value, cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        self.apply(&mut arguments);
        self.inner.execute(arguments, cancel_rx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A stand-in inner tool that echoes the arguments it was called with, so
    /// tests can assert what binding did.
    struct EchoTool {
        spec: ToolSpec,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            self.spec.clone()
        }
        async fn execute(&self, arguments: Value, _cancel: oneshot::Receiver<()>) -> ToolResult {
            ToolResult {
                content: arguments.to_string(),
                is_error: false,
            }
        }
    }

    fn say_spec() -> ToolSpec {
        ToolSpec {
            name: "dialogue_say".into(),
            description: "send".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string"},
                    "to": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["from", "to", "content"]
            }),
            ..Default::default()
        }
    }

    fn echo(spec: ToolSpec) -> Arc<dyn Tool> {
        Arc::new(EchoTool { spec })
    }

    #[tokio::test]
    async fn force_overwrites_from_and_hides_it_from_schema() {
        let tool = SessionDialogueTool::new(
            echo(say_spec()),
            "mu:d1:s1".into(),
            "from",
            DialogueBind::Force,
        );
        // schema no longer requires or advertises `from`
        let spec = tool.spec();
        let schema = spec.input_schema.as_object().unwrap();
        let required: Vec<_> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(!required.contains(&"from"));
        assert!(!schema["properties"]
            .as_object()
            .unwrap()
            .contains_key("from"));

        // a forged `from` is overwritten with the session identity
        let (_tx, rx) = oneshot::channel();
        let out = tool
            .execute(json!({"from": "cc:evil", "to": "cc", "content": "hi"}), rx)
            .await;
        let sent: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(sent["from"], "mu:d1:s1");
    }

    #[tokio::test]
    async fn default_fills_to_only_when_absent() {
        let poll_spec = ToolSpec {
            name: "dialogue_poll".into(),
            description: "poll".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"to": {"type": "string"}},
                "required": ["to"]
            }),
            ..Default::default()
        };
        let tool = SessionDialogueTool::new(
            echo(poll_spec),
            "mu:d1:s1".into(),
            "to",
            DialogueBind::Default,
        );

        // omitted → filled with identity
        let (_tx, rx) = oneshot::channel();
        let out = tool.execute(json!({}), rx).await;
        let sent: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(sent["to"], "mu:d1:s1");

        // explicitly set → preserved (observing another inbox stays legal)
        let (_tx2, rx2) = oneshot::channel();
        let out2 = tool.execute(json!({"to": "cc:other"}), rx2).await;
        let sent2: Value = serde_json::from_str(&out2.content).unwrap();
        assert_eq!(sent2["to"], "cc:other");
    }
}
