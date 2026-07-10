//! Delta accumulation: fold a sequence of [`ChatCompletionChunk`]s into one
//! completed message.
//!
//! Chat Completions streams *partial state*, not semantic events: text
//! arrives as content fragments, tool calls as by-`index` fragments whose
//! `arguments` string concatenates across chunks, and usage/finish_reason
//! land on whichever chunk the server chose. This module owns that folding
//! logic — the exact behavior promoted from the hand-rolled StreamState in
//! `mu-ai/src/providers/openrouter.rs`:
//!
//! - tool calls keyed by `index`, **first-seen order preserved** (the wire's
//!   `index` is a correlation key, not a position guarantee);
//! - `id`/`name` latest-wins, `arguments` appended;
//! - `usage` latest-non-`None`-wins (final dedicated usage chunk, or
//!   attached to the last content chunk — both server styles observed);
//! - `finish_reason` latest-wins.
//!
//! The consumer drives SSE framing and calls [`ChatAccumulator::push`] per
//! parsed chunk, then [`ChatAccumulator::finish`] at `[DONE]`/EOF.

use std::collections::HashMap;

use crate::response::{ChatCompletionChunk, Usage};

/// Incremental fold over streamed chunks.
#[derive(Debug, Default)]
pub struct ChatAccumulator {
    text: String,
    reasoning: String,
    calls: HashMap<u32, ToolCallBuilder>,
    call_order: Vec<u32>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

/// A completed tool call: `arguments` is the concatenated raw JSON string
/// exactly as the wire delivered it — parsing/validating it is consumer
/// policy (mu, for one, degrades unparseable arguments to `{}` with a
/// warning rather than failing the turn).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallComplete {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// The folded result of one streamed completion.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompletedChat {
    /// Concatenated `delta.content`.
    pub text: String,
    /// Concatenated `delta.reasoning` (either wire spelling).
    pub reasoning: String,
    /// Completed tool calls in first-seen order.
    pub tool_calls: Vec<ToolCallComplete>,
    /// Last `finish_reason` seen (`stop`, `tool_calls`, `length`, …).
    pub finish_reason: Option<String>,
    /// Last usage seen.
    pub usage: Option<Usage>,
}

impl ChatAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one chunk. Returns what this chunk *streamed* — the new text and
    /// reasoning fragments — so a live consumer can forward deltas without
    /// re-deriving them (tool-call fragments accumulate silently; they only
    /// become visible as completed calls in [`finish`](Self::finish)).
    pub fn push(&mut self, chunk: &ChatCompletionChunk) -> PushedDeltas {
        let mut pushed = PushedDeltas::default();
        if let Some(u) = chunk.usage.as_ref() {
            self.usage = Some(u.clone());
        }
        for choice in &chunk.choices {
            if let Some(content) = choice.delta.content.as_deref() {
                if !content.is_empty() {
                    self.text.push_str(content);
                    pushed
                        .text
                        .get_or_insert_with(String::new)
                        .push_str(content);
                }
            }
            if let Some(reasoning) = choice.delta.reasoning.as_deref() {
                if !reasoning.is_empty() {
                    self.reasoning.push_str(reasoning);
                    pushed
                        .reasoning
                        .get_or_insert_with(String::new)
                        .push_str(reasoning);
                }
            }
            if let Some(deltas) = choice.delta.tool_calls.as_ref() {
                for d in deltas {
                    let entry = self.calls.entry(d.index).or_insert_with(|| {
                        self.call_order.push(d.index);
                        ToolCallBuilder::default()
                    });
                    if let Some(id) = d.id.as_ref() {
                        entry.id = id.clone();
                    }
                    if let Some(f) = d.function.as_ref() {
                        if let Some(name) = f.name.as_ref() {
                            entry.name = name.clone();
                        }
                        if let Some(args) = f.arguments.as_ref() {
                            entry.arguments.push_str(args);
                        }
                    }
                }
            }
            if let Some(reason) = choice.finish_reason.as_ref() {
                self.finish_reason = Some(reason.clone());
            }
        }
        pushed
    }

    /// Consume the accumulator into the completed message.
    pub fn finish(mut self) -> CompletedChat {
        let tool_calls = self
            .call_order
            .iter()
            .filter_map(|idx| self.calls.remove(idx))
            .map(|b| ToolCallComplete {
                id: b.id,
                name: b.name,
                arguments: b.arguments,
            })
            .collect();
        CompletedChat {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

/// What one [`ChatAccumulator::push`] streamed: concatenated new text and
/// reasoning fragments across the chunk's choices (`None` = none arrived).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PushedDeltas {
    pub text: Option<String>,
    pub reasoning: Option<String>,
}

/// One-shot convenience: fold an entire chunk sequence.
pub fn accumulate<'a>(chunks: impl IntoIterator<Item = &'a ChatCompletionChunk>) -> CompletedChat {
    let mut acc = ChatAccumulator::new();
    for c in chunks {
        acc.push(c);
    }
    acc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(s: &str) -> ChatCompletionChunk {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn text_then_finish_then_usage() {
        let chunks = [
            chunk(r#"{"choices":[{"delta":{"content":"hel"}}]}"#),
            chunk(r#"{"choices":[{"delta":{"content":"lo"}}]}"#),
            chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            chunk(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#),
        ];
        let done = accumulate(&chunks);
        assert_eq!(done.text, "hello");
        assert_eq!(done.finish_reason.as_deref(), Some("stop"));
        assert_eq!(done.usage.unwrap().prompt_tokens, Some(10));
        assert!(done.tool_calls.is_empty());
    }

    #[test]
    fn tool_call_fragments_assemble_in_first_seen_order() {
        let chunks = [
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"grep","arguments":"{\"pat"}}]}}]}"#,
            ),
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":"{}"}}]}}]}"#,
            ),
            chunk(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"tern\":\"x\"}"}}]}}]}"#,
            ),
            chunk(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
        ];
        let done = accumulate(&chunks);
        // index 1 was seen FIRST → it leads, matching the promoted behavior.
        assert_eq!(done.tool_calls.len(), 2);
        assert_eq!(done.tool_calls[0].id, "call_b");
        assert_eq!(done.tool_calls[0].name, "grep");
        assert_eq!(done.tool_calls[0].arguments, r#"{"pattern":"x"}"#);
        assert_eq!(done.tool_calls[1].id, "call_a");
        assert_eq!(done.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn reasoning_accumulates_separately_from_text() {
        let chunks = [
            chunk(r#"{"choices":[{"delta":{"reasoning":"think"}}]}"#),
            chunk(r#"{"choices":[{"delta":{"reasoning_content":"ing"}}]}"#),
            chunk(r#"{"choices":[{"delta":{"content":"answer"}}]}"#),
        ];
        let done = accumulate(&chunks);
        assert_eq!(done.reasoning, "thinking");
        assert_eq!(done.text, "answer");
    }

    #[test]
    fn push_reports_streamed_deltas() {
        let mut acc = ChatAccumulator::new();
        let p1 = acc.push(&chunk(r#"{"choices":[{"delta":{"content":"a"}}]}"#));
        assert_eq!(p1.text.as_deref(), Some("a"));
        assert_eq!(p1.reasoning, None);
        let p2 = acc.push(&chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"n","arguments":"{}"}}]}}]}"#,
        ));
        assert_eq!(p2.text, None);
        assert_eq!(p2.reasoning, None);
    }

    #[test]
    fn usage_latest_non_none_wins() {
        let chunks = [
            chunk(r#"{"choices":[{"delta":{"content":"x"}}],"usage":{"prompt_tokens":1}}"#),
            chunk(r#"{"choices":[{"delta":{"content":"y"}}]}"#),
            chunk(r#"{"choices":[],"usage":{"prompt_tokens":99}}"#),
        ];
        let done = accumulate(&chunks);
        assert_eq!(done.usage.unwrap().prompt_tokens, Some(99));
    }
}
