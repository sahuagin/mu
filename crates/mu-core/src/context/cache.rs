//! Cache-boundary strategy — the [`CacheStrategy`] trait and a
//! foundational [`NoCacheStrategy`] no-op impl.
//!
//! Per `specs/architecture/event-sourced-context.md` lines 564-612
//! ("Cache-boundary alignment" + "Pluggable cache and provider
//! strategies"), prompt-cache boundaries are derived from span
//! retention/stability metadata rather than annotated at the message
//! layer. A [`CacheStrategy`] inspects the rope (where the truth
//! about stability lives) and produces [`CacheBoundary`] markers that
//! a subsequent pass annotates onto the rendered [`ProviderMessages`].
//!
//! ## Why two methods?
//!
//! - `boundaries(rope) -> Vec<CacheBoundary>` is pure inspection. It
//!   answers: "given the rope, where would cache prefixes fall?"
//!   This is the A/B-testable surface: same rope, different
//!   strategies, compare boundary sets.
//! - `annotate(&mut messages, &boundaries)` is the apply step. Once
//!   a renderer has produced `ProviderMessages`, the strategy
//!   converts boundary positions into provider-shaped cache markers
//!   on the messages.
//!
//! Splitting these two lets boundary placement be cached and
//! re-applied to different renders (e.g., agent-view + operator-
//! view of the same rope share a boundary set even if the messages
//! differ).
//!
//! ## Scope (mu-ktq foundational half)
//!
//! IN:
//! - [`CacheStrategy`] trait
//! - [`CacheBoundary`] struct (message-index marker)
//! - [`NoCacheStrategy`] — empty boundaries, no-op annotate; the
//!   correct strategy for providers without cache support (OpenAI,
//!   FauxProvider) and a useful baseline for tests.
//!
//! DEFERRED (separate beads):
//! - `AnthropicCacheStrategy` — places boundary at the first volatile
//!   retained span; emits [`CacheMarker::Ephemeral`] at the last
//!   stable position. Needs design thought re: how it composes with
//!   existing mu-i6j `cache_control` work.
//!
//! [`CacheMarker::Ephemeral`]: super::renderer::CacheMarker::Ephemeral

use serde::{Deserialize, Serialize};

use super::renderer::ProviderMessages;
use super::rope::RetainedRope;

/// A single cache-boundary marker — a position in
/// [`ProviderMessages`] where a provider-specific cache directive
/// should be emitted.
///
/// `message_index` points at the message that is the *last cacheable
/// item in the prefix*. In Anthropic-style annotation, the
/// `cache_control: ephemeral` marker is attached to that message,
/// telling the provider to cache the prefix up to and including it.
///
/// The boundary is intentionally minimal — message-level rather than
/// content-block-level. The full design (spec lines 167-228) will
/// extend boundaries to point at `(message_index, block_index,
/// byte_or_token_range)` once content blocks are first-class. For
/// the stub, message-level is sufficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheBoundary {
    /// Index into `ProviderMessages.messages` of the last message
    /// that should be included in the cached prefix.
    pub message_index: usize,
}

impl CacheBoundary {
    /// New boundary at the given message index.
    pub fn at(message_index: usize) -> Self {
        Self { message_index }
    }
}

/// Strategy for placing prompt-cache boundaries in rendered messages.
///
/// Per spec proposal (lines 600-603):
/// ```text
/// trait CacheStrategy
///   fn boundaries(rope: &RetainedRope) -> Vec<CacheBoundary>
///   fn annotate(messages: &mut ProviderMessages, boundaries: &[CacheBoundary])
/// ```
///
/// Idiomatic Rust adds `&self` to both methods so per-impl
/// configuration (e.g., maximum boundary count, minimum prefix size)
/// can live on the strategy value. This does NOT change the spec's
/// semantics.
pub trait CacheStrategy: Send + Sync {
    /// Compute cache-boundary positions for the given rope.
    ///
    /// Pure inspection — no side effects, no rendering required. The
    /// returned boundaries are message-indexed and apply to any
    /// rendering of the *same rope shape* (i.e., spans-to-messages
    /// must preserve index correspondence).
    fn boundaries(&self, rope: &RetainedRope) -> Vec<CacheBoundary>;

    /// Apply boundary markers to already-rendered messages.
    ///
    /// Implementations attach provider-specific cache markers to the
    /// messages at the given boundary positions. Out-of-range
    /// indices MUST be tolerated silently (clamping or skipping is
    /// fine; panicking is not) so that boundaries computed from an
    /// older rope shape don't crash a re-render.
    fn annotate(&self, messages: &mut ProviderMessages, boundaries: &[CacheBoundary]);
}

/// No-op cache strategy.
///
/// - `boundaries` returns an empty `Vec`.
/// - `annotate` is a no-op.
///
/// Correct for providers without cache support (current OpenAI API,
/// the FauxProvider) and a baseline for tests of code that should
/// work whether or not caching is in play.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCacheStrategy;

impl NoCacheStrategy {
    pub fn new() -> Self {
        Self
    }
}

impl CacheStrategy for NoCacheStrategy {
    fn boundaries(&self, _rope: &RetainedRope) -> Vec<CacheBoundary> {
        Vec::new()
    }

    fn annotate(&self, _messages: &mut ProviderMessages, _boundaries: &[CacheBoundary]) {
        // No-op by design.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::renderer::{FauxProviderRenderer, ProjectionTarget, ProviderRenderer};
    use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};

    fn sample_rope() -> RetainedRope {
        RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new("u1", SpanKind::User, "hi", RetentionClass::Hot),
            Span::new("a1", SpanKind::Assistant, "hello", RetentionClass::Hot),
            Span::new(
                "u2",
                SpanKind::User,
                "what time is it",
                RetentionClass::Warm,
            ),
        ])
    }

    #[test]
    fn no_cache_returns_empty_boundaries() {
        let rope = sample_rope();
        let boundaries = NoCacheStrategy::new().boundaries(&rope);
        assert!(boundaries.is_empty());
    }

    #[test]
    fn no_cache_annotate_is_no_op() {
        let rope = sample_rope();
        let mut rendered = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        let before = rendered.clone();
        // Even if (somehow) the caller passes a non-empty boundary
        // list, NoCacheStrategy must not mutate.
        let bogus = vec![CacheBoundary::at(0), CacheBoundary::at(2)];
        NoCacheStrategy::new().annotate(&mut rendered, &bogus);
        assert_eq!(
            rendered, before,
            "NoCacheStrategy must not mutate ProviderMessages"
        );
    }

    #[test]
    fn cache_boundary_at_helper() {
        let b = CacheBoundary::at(3);
        assert_eq!(b.message_index, 3);
    }

    #[test]
    fn strategy_trait_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn CacheStrategy>();
    }

    /// Compose renderer + strategy end-to-end against a stub rope.
    /// With NoCacheStrategy the round-trip is identity on messages.
    #[test]
    fn compose_renderer_and_no_cache_is_identity() {
        let rope = sample_rope();
        let renderer = FauxProviderRenderer::new();
        let strategy = NoCacheStrategy::new();

        let mut rendered = renderer.render(&rope, ProjectionTarget::AgentView);
        let snapshot = rendered.clone();
        let boundaries = strategy.boundaries(&rope);
        strategy.annotate(&mut rendered, &boundaries);

        assert!(boundaries.is_empty());
        assert_eq!(rendered, snapshot);
    }
}
