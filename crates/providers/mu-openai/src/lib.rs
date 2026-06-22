//! mu-openai — OpenAI Responses API wire protocol as Rust types.
//!
//! Standalone. Knows nothing about mu (`mu-core`/`mu-ai`). This crate models
//! the OpenAI wire contract and small HTTP clients for the public API and the
//! ChatGPT/Codex Responses-compatible backend. Mu-specific translation lives in
//! `mu-ai`.

mod client;
mod json;
mod request;
mod response;
mod sse;
mod stream;

pub use client::{Auth, Client, ClientError, Endpoint};
pub use json::{JsonValue, JsonValueError};
pub use request::{
    CreateResponseRequest, FunctionTool, InputContent, InputItem, Reasoning, Tool, ToolChoice,
};
pub use response::{
    IncompleteDetails, OutputContent, OutputItem, Response, ResponseError, ResponseStatus, Usage,
    UsageInputDetails, UsageOutputDetails,
};
pub use sse::{SseError, SseEvent, SseStream};
pub use stream::{ResponseStreamEvent, StreamError};
