//! mu-anthropic — Anthropic Messages API wire protocol as Rust types.
//!
//! Standalone. Knows nothing about mu (no `mu-core` dependency). The job of
//! this crate is to make Anthropic's wire contract a *type* in both
//! directions: typed structs whose `serde` (de)serialization byte-matches the
//! documented + observed wire JSON. See `INTEGRATION.md` and `PLAN.md` in the
//! crate root.
//!
//! Built in vertical slices, leaf-first. Slice 1 (this commit): [`ContentBlock`]
//! and [`CacheControl`].

mod content;

pub use content::{CacheControl, ContentBlock};
