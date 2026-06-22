//! mu-openai — OpenAI Responses API wire protocol as Rust types.
//!
//! Standalone. Knows nothing about mu (no `mu-core`/`mu-ai` dependency). The job
//! of this crate is to make OpenAI's **Responses API** wire contract a *type* in
//! both directions: typed structs whose `serde` (de)serialization byte-matches
//! the documented + observed wire JSON. Transport (HTTP/auth/SSE byte framing)
//! and any mu↔wire translation live in the CONSUMER (`mu-ai`), never here — the
//! dependency direction is a strict DAG (`mu-ai → mu-openai`, never back), which
//! is also what makes the crate reusable by external projects.
//!
//! See `AGENTS.md` (the rules) and `INTEGRATION.md` (the seam map) in the crate
//! root. Built in vertical slices, leaf-first, modeling the sibling
//! `mu-anthropic` crate.

mod accumulate;
mod finite;
mod json;
mod request;
mod response;
mod stream;

pub use accumulate::{accumulate, AccumulateError};
pub use finite::{deserialize_option_finite, FiniteF64, NonFinite};
pub use json::{JsonValue, JsonValueError};
pub use request::{
    CreateResponseRequest, FunctionTool, InputContent, InputItem, NamedToolChoice, Reasoning, Tool,
    ToolChoice, ToolChoiceMode,
};
pub use response::{
    IncompleteDetails, OutputContent, OutputItem, Response, ResponseError, ResponseStatus, Usage,
    UsageInputDetails, UsageOutputDetails,
};
pub use stream::ResponseStreamEvent;
