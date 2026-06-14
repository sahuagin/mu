//! mu-anthropic — Anthropic Messages API wire protocol as Rust types.
//!
//! Standalone. Knows nothing about mu (no `mu-core` dependency). The job of
//! this crate is to make Anthropic's wire contract a *type* in both
//! directions: typed structs whose `serde` (de)serialization byte-matches the
//! documented + observed wire JSON. See `INTEGRATION.md` and `PLAN.md` in the
//! crate root.
//!
//! Built in vertical slices, leaf-first. Slice 1: [`ContentBlock`] / [`CacheControl`]. Slice 2: [`Message`] /
//! [`Content`] / [`Role`]. Slice 3: [`MessagesRequest`] / [`Tool`].
//! Slice 4: response [`ResponseMessage`] / [`Usage`] / [`StopReason`].
//! Slice 5: [`StreamEvent`] (SSE events).

mod content;
mod message;
mod request;
mod response;
mod stream;

pub use content::{CacheControl, ContentBlock};
pub use message::{Content, Message, Role};
pub use request::{MessagesRequest, Tool};
pub use response::{CacheCreation, Message as ResponseMessage, StopReason, Usage};
pub use stream::{BlockDelta, BlockStart, MessageDeltaBody, StreamError, StreamEvent};
