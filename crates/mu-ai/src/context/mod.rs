//! Provider-specific context rendering — concrete impls of
//! `mu_core::context::{ProviderRenderer, CacheStrategy}`.
//!
//! Per `specs/architecture/event-sourced-context.md` lines 592-612
//! ("Pluggable cache and provider strategies"), the trait surfaces
//! live in `mu_core::context`; concrete provider-shaped impls live
//! here in `mu-ai`. The dependency direction matches `mu_core::agent`
//! (trait) vs `mu_ai::providers` (concrete impls).
//!
//! ## What lives here today
//!
//! - [`anthropic::AnthropicProviderRenderer`] — Anthropic-shaped
//!   renderer (mu-bn4).
//! - [`anthropic::AnthropicCacheStrategy`] — places a single
//!   ephemeral-cache boundary at the last stable+cacheable span
//!   (mu-bn4).
//!
//! ## Coexistence with the live `AgentMessage` path
//!
//! The mu-i6j / mu-n48 `cache_control` work in
//! [`crate::providers::anthropic`] annotates the live-loop
//! `&[AgentMessage]`-shaped wire body directly. The rope-based path
//! in this module is the future shape — same intent (turn the stable
//! prefix into a cache hit) over a different input type
//! ([`RetainedRope`]). Both paths coexist until mu-fb0 wires the
//! rope into the live loop and retires the `AgentMessage`-shaped
//! annotation.
//!
//! [`RetainedRope`]: mu_core::context::RetainedRope

pub mod anthropic;

pub use anthropic::{AnthropicCacheStrategy, AnthropicProviderRenderer};

#[cfg(test)]
mod compaction_cache_tests;
