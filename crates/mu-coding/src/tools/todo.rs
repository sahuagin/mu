//! `todo` tool — session-scoped task list for long-task steering.
//! Bead mu-z1ce.
//!
//! The model writes the FULL list on every call (TodoWrite semantics):
//! simpler for the model than diff operations, and self-healing — a
//! confused list is one honest rewrite away from correct. State lives
//! in this tool instance, which is constructed per session (the tool
//! would otherwise leak one session's plan into another); durable
//! history rides the event log via the ToolCallCompleted result like
//! every other tool, so frontends can render progress without a new
//! event variant.
//!
//! This is turn-scale steering, not durable work tracking — beads and
//! agent-memory remain the cross-session trackers. The value here is
//! (a) the model externalizes its plan where the operator can see it,
//! and (b) the rendered list re-enters context each call, anchoring
//! long multi-step work against goal drift.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use mu_core::agent::{
    PermissionLevel, RetryPolicy, SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;

/// Refuse absurd lists — steering state, not a database.
const MAX_ITEMS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    fn marker(&self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Completed => "[x]",
        }
    }
}

#[derive(Debug, Clone)]
struct TodoItem {
    content: String,
    status: TodoStatus,
}

pub struct TodoTool {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoTool {
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }
}

impl Default for TodoTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "todo",
            "Maintain this session's task list for multi-step work. Write the FULL \
             list every call — it replaces the previous one. Use it when a task has \
             3+ steps: add items when work is identified, mark one item in_progress \
             before starting it, mark it completed immediately after finishing. An \
             empty list clears it.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete task list (replaces the previous list).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "Imperative description of the task."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Current state of this task."
                                }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        )
        .with_policy(ToolPolicy {
            // Mutates only this session's own in-memory list — no fs,
            // no exec, no network. Audited-benign; see the
            // policy_invariants rationale in tools/mod.rs.
            side_effects: SideEffects::ReadOnly,
            permission: PermissionLevel::Allow,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: None,
            idempotent: true,
        })
    }

    fn execute<'life0, 'async_trait>(
        &'life0 self,
        arguments: Value,
        _cancel_rx: oneshot::Receiver<()>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let Some(todos) = arguments.get("todos").and_then(Value::as_array) else {
                return ToolResult {
                    content: "todo: missing required `todos` array".to_owned(),
                    is_error: true,
                };
            };
            if todos.len() > MAX_ITEMS {
                return ToolResult {
                    content: format!(
                        "todo: {} items exceeds the {MAX_ITEMS}-item cap — this is \
                         turn-scale steering, not a backlog; track large work in the \
                         durable tracker instead",
                        todos.len()
                    ),
                    is_error: true,
                };
            }

            let mut parsed = Vec::with_capacity(todos.len());
            for (i, t) in todos.iter().enumerate() {
                let content = t
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default();
                if content.is_empty() {
                    return ToolResult {
                        content: format!("todo: item {i} has empty/missing `content`"),
                        is_error: true,
                    };
                }
                let status_raw = t.get("status").and_then(Value::as_str).unwrap_or_default();
                let Some(status) = TodoStatus::parse(status_raw) else {
                    return ToolResult {
                        content: format!(
                            "todo: item {i} has invalid `status` `{status_raw}` \
                             (expected pending|in_progress|completed)"
                        ),
                        is_error: true,
                    };
                };
                parsed.push(TodoItem {
                    content: content.to_owned(),
                    status,
                });
            }

            let rendered = render(&parsed);
            match self.items.lock() {
                Ok(mut items) => *items = parsed,
                Err(poisoned) => *poisoned.into_inner() = parsed,
            }
            ToolResult {
                content: rendered,
                is_error: false,
            }
        })
    }
}

fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "todo list cleared".to_owned();
    }
    let completed = items
        .iter()
        .filter(|i| i.status == TodoStatus::Completed)
        .count();
    let mut out = format!(
        "todo list updated ({completed}/{} completed):\n",
        items.len()
    );
    for item in items {
        out.push_str(&format!("  {} {}\n", item.status.marker(), item.content));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn execute(tool: &TodoTool, args: Value) -> ToolResult {
        let (_tx, rx) = oneshot::channel();
        tool.execute(args, rx).await
    }

    #[test]
    fn spec_describes_todo_tool() {
        let spec = TodoTool::new().spec();
        assert_eq!(spec.name, "todo");
        assert_eq!(spec.input_schema["required"], json!(["todos"]));
        assert_eq!(spec.policy.side_effects, SideEffects::ReadOnly);
    }

    #[tokio::test]
    async fn full_write_replaces_and_renders() {
        let tool = TodoTool::new();
        let r = execute(
            &tool,
            json!({"todos": [
                {"content": "restore tree", "status": "completed"},
                {"content": "hooks engine", "status": "in_progress"},
                {"content": "todo tool", "status": "pending"},
            ]}),
        )
        .await;
        assert!(!r.is_error, "got: {}", r.content);
        assert!(r.content.contains("1/3 completed"));
        assert!(r.content.contains("[x] restore tree"));
        assert!(r.content.contains("[~] hooks engine"));
        assert!(r.content.contains("[ ] todo tool"));

        // Second write fully replaces the first.
        let r = execute(
            &tool,
            json!({"todos": [{"content": "only item", "status": "pending"}]}),
        )
        .await;
        assert!(!r.is_error);
        assert!(r.content.contains("0/1 completed"));
        assert!(!r.content.contains("hooks engine"));
    }

    #[tokio::test]
    async fn empty_list_clears() {
        let tool = TodoTool::new();
        let _ = execute(
            &tool,
            json!({"todos": [{"content": "x", "status": "pending"}]}),
        )
        .await;
        let r = execute(&tool, json!({"todos": []})).await;
        assert!(!r.is_error);
        assert!(r.content.contains("cleared"));
    }

    #[tokio::test]
    async fn invalid_status_is_error() {
        let tool = TodoTool::new();
        let r = execute(
            &tool,
            json!({"todos": [{"content": "x", "status": "done"}]}),
        )
        .await;
        assert!(r.is_error);
        assert!(r.content.contains("invalid `status` `done`"));
    }

    #[tokio::test]
    async fn empty_content_is_error() {
        let tool = TodoTool::new();
        let r = execute(
            &tool,
            json!({"todos": [{"content": "  ", "status": "pending"}]}),
        )
        .await;
        assert!(r.is_error);
        assert!(r.content.contains("empty/missing `content`"));
    }

    #[tokio::test]
    async fn missing_todos_is_error() {
        let tool = TodoTool::new();
        let r = execute(&tool, json!({})).await;
        assert!(r.is_error);
        assert!(r.content.contains("missing required `todos`"));
    }

    #[tokio::test]
    async fn item_cap_is_enforced() {
        let tool = TodoTool::new();
        let items: Vec<Value> = (0..101)
            .map(|i| json!({"content": format!("item {i}"), "status": "pending"}))
            .collect();
        let r = execute(&tool, json!({ "todos": items })).await;
        assert!(r.is_error);
        assert!(r.content.contains("exceeds"));
    }
}
