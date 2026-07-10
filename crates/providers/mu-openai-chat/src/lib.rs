//! mu-openai-chat — OpenAI Chat Completions wire protocol as Rust types.
//!
//! Standalone. Knows nothing about mu (no `mu-core`/`mu-ai` dependency). The
//! job of this crate is to make the OpenAI **Chat Completions** wire contract
//! a *type* in both directions: typed structs whose `serde` (de)serialization
//! matches the wire JSON that OpenAI-compatible servers — OpenRouter, ollama's
//! `/v1/chat/completions`, llama.cpp `llama-server`, vLLM, LM Studio — send
//! and accept. Transport (HTTP/auth/SSE byte framing) and any mu↔wire
//! translation live in the CONSUMER (`mu-ai`), never here — the dependency
//! direction is a strict DAG (`mu-ai → mu-openai-chat`, never back), which is
//! also what makes the crate reusable by external projects.
//!
//! Chat Completions is the *primary* wire for local inference servers (the
//! Responses API — sibling crate `mu-openai` — is OpenAI-hosted machinery:
//! server-side state, built-in tools; local servers offer neither). The two
//! wires are distinct schemas in both directions — messages vs input items,
//! choices vs output items, deltas-by-index chunks vs typed semantic events —
//! so this crate shares no types with `mu-openai` by design (bead mu-v8ye).
//!
//! Types were promoted from the battle-tested hand-rolled implementation in
//! `mu-ai/src/providers/openrouter.rs` (spec mu-017 lineage), byte-matched to
//! observed traffic rather than transcribed from documentation. Built in
//! vertical slices, leaf-first, modeling the sibling `mu-anthropic` /
//! `mu-openai` crates.

mod accumulate;
mod request;
mod response;

pub use accumulate::{accumulate, ChatAccumulator, CompletedChat, PushedDeltas, ToolCallComplete};
pub use request::{
    ChatCompletionRequest, ChatMessage, FunctionDef, FunctionRef, Reasoning, StreamOptions, Tool,
    ToolCallRef,
};
pub use response::{
    ChatChoice, ChatCompletionChunk, ChatDelta, CompletionTokensDetails, FunctionDelta,
    PromptTokensDetails, ToolCallDelta, Usage,
};

/// The sentinel data line that terminates an OpenAI-compatible SSE stream.
/// Servers send `data: [DONE]` (whitespace-tolerant) after the final chunk.
pub const DONE_SENTINEL: &str = "[DONE]";

/// True when an SSE `data:` payload is the stream-termination sentinel
/// rather than a JSON chunk.
pub fn is_done_sentinel(data: &str) -> bool {
    data.trim() == DONE_SENTINEL
}
