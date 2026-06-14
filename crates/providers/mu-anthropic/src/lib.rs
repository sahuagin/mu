//! mu-anthropic — Anthropic Messages API wire protocol as Rust types.
//!
//! Standalone. Knows nothing about mu (no `mu-core` dependency). The job of
//! this crate is to make Anthropic's wire contract a *type* in both
//! directions: typed structs whose `serde` (de)serialization byte-matches the
//! documented + observed wire JSON. See `INTEGRATION.md` and `PLAN.md` in the
//! crate root.
//!
//! Built in vertical slices, leaf-first. Slice 1: [`ContentBlock`] / [`CacheControl`]. Slice 2: [`Message`] /
//! [`Content`] / [`Role`].

mod content;
mod message;

pub use content::{CacheControl, ContentBlock};
pub use message::{Content, Message, Role};
