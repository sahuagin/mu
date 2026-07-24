//! mu-07g0: the session-side mailbox READ affordance. The mailbox always had
//! durable delivery and a wake — but a session had no way to read its own
//! messages (the operator's framing: "we'd never given it a read/retrieve
//! command"). This tool closes that gap: `list` shows the session's mailbox,
//! `read` retrieves one message in full. Per the mailbox design doc, the
//! wake carries only the sender-authored subject (a hint, not the copy) —
//! the receiving model decides whether the body is worth its context and
//! retrieves it here.
//!
//! Per-session (bound to its owner's id in `session_spawn_tools`, like
//! spawn_worker/watch), and holds a WEAK sessions handle — a strong clone
//! here would deadlock shutdown (mu-qc08: the tool lives in its own
//! session's tool list).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{Tool, ToolPolicy, ToolResult, ToolSpec};

use crate::serve::WeakSessions;

pub struct MailboxTool {
    sessions: WeakSessions,
    session_id: String,
    spec: ToolSpec,
}

impl MailboxTool {
    pub fn new(sessions: WeakSessions, session_id: String) -> Self {
        let spec = ToolSpec {
            name: "mailbox".to_string(),
            description: "Read this session's durable mailbox. action=list shows \
                          messages (newest last; seq, from, kind, subject, body \
                          preview); action=read retrieves one message in full by \
                          seq. Wakes carry only the sender's subject line (a hint) — \
                          this tool is how you retrieve the message itself."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "read"],
                               "description": "list (default) or read"},
                    "seq": {"type": "number",
                            "description": "Message seq to read (required for action=read)"},
                    "include_consumed": {"type": "boolean",
                            "description": "list: include already-consumed messages (default true)"}
                }
            }),
            ..ToolSpec::default()
        }
        .with_policy(ToolPolicy::read_only());
        Self {
            sessions,
            session_id,
            spec,
        }
    }
}

#[async_trait]
impl Tool for MailboxTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, arguments: Value, _cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let Some(sessions) = self.sessions.upgrade() else {
            return ToolResult {
                content: "daemon is shutting down".to_string(),
                is_error: true,
            };
        };
        let Some(log) = sessions.event_log(&self.session_id) else {
            return ToolResult {
                content: format!("no event log for session {}", self.session_id),
                is_error: true,
            };
        };
        let include_consumed = arguments
            .get("include_consumed")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let messages = crate::serve::handlers::project_mailbox(&log, None, include_consumed);

        match arguments
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list")
        {
            "read" => {
                let Some(seq) = arguments.get("seq").and_then(Value::as_u64) else {
                    return ToolResult {
                        content: "action=read requires `seq` (see action=list)".to_string(),
                        is_error: true,
                    };
                };
                match messages.iter().find(|m| m.seq == seq) {
                    Some(m) => ToolResult {
                        content: format!(
                            "seq {} · from {} (session {}) · kind {} · consumed: {}\n\
                             subject: {}\n---\n{}",
                            m.seq,
                            m.from_daemon_id,
                            m.from_session_id,
                            m.kind,
                            m.consumed,
                            m.subject,
                            body_text(&m.body),
                        ),
                        is_error: false,
                    },
                    None => ToolResult {
                        content: format!("no message with seq {seq} in this session's mailbox"),
                        is_error: true,
                    },
                }
            }
            "list" => {
                if messages.is_empty() {
                    return ToolResult {
                        content: "mailbox is empty".to_string(),
                        is_error: false,
                    };
                }
                let lines: Vec<String> = messages
                    .iter()
                    .map(|m| {
                        let body = body_text(&m.body);
                        let preview: String = body.chars().take(120).collect();
                        let ellipsis = if body.chars().count() > 120 {
                            "…"
                        } else {
                            ""
                        };
                        format!(
                            "seq {} · from {} · {} · {}{}: {}{}",
                            m.seq,
                            m.from_daemon_id,
                            m.kind,
                            if m.consumed { "[consumed] " } else { "" },
                            m.subject,
                            preview,
                            ellipsis,
                        )
                    })
                    .collect();
                ToolResult {
                    content: lines.join("\n"),
                    is_error: false,
                }
            }
            other => ToolResult {
                content: format!("unknown action `{other}` (list | read)"),
                is_error: true,
            },
        }
    }
}

fn body_text(body: &Value) -> String {
    match body {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Build the per-session mailbox tool (helper for `session_spawn_tools`).
pub fn mailbox_tool(sessions: WeakSessions, session_id: &str) -> Arc<dyn Tool> {
    Arc::new(MailboxTool::new(sessions, session_id.to_string()))
}
