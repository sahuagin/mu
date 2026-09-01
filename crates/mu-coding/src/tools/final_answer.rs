use std::future::Future;
use std::pin::Pin;

use mu_core::agent::{Tool, ToolPolicy, ToolResult, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::oneshot;

/// mu-bm6za: the stop-at-answer protocol tool for one-shot/headless
/// sessions. Completion becomes a protocol event instead of a semantic
/// judgment: the model delivers its answer by calling this tool, and the
/// `ends_turn_on_success` policy (the mu-spk7 park-and-wake mechanism)
/// completes the ask instead of re-invoking the model — no
/// answer-then-overwork tail. The result echoes the answer verbatim so
/// the transcript and any capture carry it as a single authoritative
/// unit, which also removes free-text answer scraping from headless
/// scoring pipelines.
///
/// Not injected by default anywhere: sessions opt in via `--tools
/// ...,final_answer`. Interactive conversations have no final answer and
/// should not carry this tool.
pub struct FinalAnswerTool;

impl FinalAnswerTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FinalAnswerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for FinalAnswerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "final_answer",
            "Deliver the final answer to the task and END THE SESSION. Call this exactly \
             once, by itself (no other tool calls in the same turn), the moment the task is \
             answered — put the complete answer in `answer`. Do not verify further, do not \
             continue working, and do not call any other tool after this one: the session \
             ends here and `answer` is the entire deliverable.",
            json!({
                "type": "object",
                "properties": {
                    "answer": {
                        "type": "string",
                        "description": "The complete final answer to the task, self-contained. \
                                        This exact text is what the caller receives."
                    }
                },
                "required": ["answer"]
            }),
        )
        .with_policy(ToolPolicy {
            ends_turn_on_success: true,
            ..ToolPolicy::read_only()
        })
        .with_verbatim_result()
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
            match arguments.get("answer").and_then(Value::as_str) {
                Some(answer) if !answer.trim().is_empty() => ToolResult {
                    content: answer.to_owned(),
                    is_error: false,
                },
                _ => ToolResult {
                    content: "final_answer requires a non-empty `answer` string — the \
                              session stays open; provide the answer and call it again"
                        .to_owned(),
                    is_error: true,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: Value) -> ToolResult {
        let (_tx, rx) = oneshot::channel();
        futures::executor::block_on(FinalAnswerTool::new().execute(args, rx))
    }

    #[test]
    fn spec_ends_turn_on_success() {
        let spec = FinalAnswerTool::new().spec();
        assert!(spec.policy.ends_turn_on_success);
        assert!(spec.verbatim_result);
    }

    #[test]
    fn echoes_answer_verbatim() {
        let r = run(json!({"answer": "the fleet values are mu and cc"}));
        assert!(!r.is_error);
        assert_eq!(r.content, "the fleet values are mu and cc");
    }

    #[test]
    fn empty_answer_is_error_and_keeps_session_open() {
        for args in [json!({}), json!({"answer": ""}), json!({"answer": "  "})] {
            let r = run(args);
            assert!(
                r.is_error,
                "empty answer must error so ends_turn does not fire"
            );
        }
    }
}
