//! Context rendering substrate — the foundational half of mu-ktq.
//!
//! This module introduces two orthogonal traits over a stubbed
//! retained rope:
//!
//! - [`ProviderRenderer`] — translates a [`RetainedRope`] into
//!   provider-shaped messages, for a chosen [`ProjectionTarget`]
//!   (agent view vs operator view).
//! - [`CacheStrategy`] — derives prompt-cache boundaries from the
//!   rope's stability metadata and annotates rendered messages with
//!   provider-specific cache markers.
//!
//! The two are orthogonal: any renderer can be composed with any
//! strategy. The rope is the controlled variable; renderer and
//! strategy are independently swappable. See
//! `specs/architecture/event-sourced-context.md` lines 564-612 for
//! the architecture motivation and trait signature proposal.
//!
//! ## Scope reminder
//!
//! IN (this module, mu-ktq foundational half):
//! - `ProviderRenderer` + `CacheStrategy` traits
//! - Supporting types: `ProjectionTarget`, `ProviderMessages`,
//!   `ProviderMessage`, `ProviderRole`, `CacheMarker`, `CacheBoundary`
//! - `RetainedRope` + `Span` + `RetentionClass` + `SpanKind` —
//!   **STUB ONLY** (full impl is mu-nat scope)
//! - `NoCacheStrategy` impl (no-op)
//! - `FauxProviderRenderer` impl (one-message-per-span)
//!
//! DEFERRED (separate beads — see `mu-ktq` follow-on):
//! - `AnthropicProviderRenderer` + `AnthropicCacheStrategy`
//! - `OpenAIProviderRenderer`
//! - Full `RetainedRope` (= mu-nat)
//! - Adoption in the live agent loop

pub mod assembly;
pub mod cache;
pub mod capability_hints;
pub mod compaction;
pub mod event;
pub mod recall;
pub mod renderer;
pub mod rope;

pub use assembly::{
    append_messages_to_baseline, assemble_rope, assemble_rope_with_context,
    extract_call_id_from_span_id,
};
pub use cache::{prefix_forensics, CacheBoundary, CacheStrategy, NoCacheStrategy};
pub use compaction::{
    BackgroundCompactionState, CompactionDecision, CompactionPolicy, CompactionQuota,
    CompactionResult, NoCompactionPolicy,
};
pub use event::RopeEvent;
pub use recall::{ProjectContext, RecallProvider, RecallSource, RecalledItem};
pub use renderer::{
    CacheMarker, CacheTtl, FauxProviderRenderer, ProjectionTarget, ProviderMessage,
    ProviderMessages, ProviderRenderer, ProviderRole,
};
pub use rope::{ContextSizes, RetainedRope, RetentionClass, Span, SpanId, SpanKind, SpanText};
