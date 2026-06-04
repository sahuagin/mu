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

/// mu-814o: hex length of the rendered-prefix digest. One hash
/// compared across calls — generous 64 bits.
pub const PREFIX_HASH_CHARS: usize = 16;
/// mu-814o: hex length of per-span digests. Matches the mu-kgu.3
/// short-hash convention (8 hex = 32 bits, ample at prefix scale).
pub const SPAN_HASH_CHARS: usize = 8;

/// mu-814o: cache-forensics digest of the cacheable prefix.
///
/// Returns `(prefix_hash, prefix_span_hashes)` for the rope/render
/// pair of one model call:
///
/// - `prefix_hash` — blake3 (truncated to [`PREFIX_HASH_CHARS`] hex)
///   over the RENDERED content (role + flat text, length-framed) of
///   `projection.messages[..=last_boundary]`. Cache validity is a
///   property of wire bytes, so this observes the same layer the
///   provider's cache does. `None` when the strategy placed no
///   boundaries (nothing is cacheable, nothing to diagnose).
/// - `prefix_span_hashes` — one `"<span-id>=<hash>"` entry per ROPE
///   span in the same range (blake3 of `span.content()`, truncated
///   to [`SPAN_HASH_CHARS`] hex). Relies on the [`CacheStrategy`]
///   contract that spans-to-messages preserve index correspondence.
///
/// Diffing consecutive calls turns a full-prefix cache invalidation
/// into a one-diff diagnosis:
/// - a span hash changed → that span mutated under a stable id
///   (the FileLoad-rehydration / recall-regeneration class);
/// - `prefix_hash` changed but every span hash is identical → the
///   renderer's projection of unchanged rope content drifted;
/// - the span-hash list length changed → the boundary itself moved.
pub fn prefix_forensics(
    projection: &ProviderMessages,
    boundaries: &[CacheBoundary],
    rope: &RetainedRope,
) -> (Option<String>, Vec<String>) {
    let Some(last) = boundaries.iter().map(|b| b.message_index).max() else {
        return (None, Vec::new());
    };

    let mut hasher = blake3::Hasher::new();
    for msg in projection.messages.iter().take(last + 1) {
        // Length-framed role + content: unambiguous concatenation.
        let role = format!("{:?}", msg.role());
        hasher.update(&(role.len() as u64).to_le_bytes());
        hasher.update(role.as_bytes());
        let content = msg.content();
        hasher.update(&(content.len() as u64).to_le_bytes());
        hasher.update(content.as_bytes());
    }
    let prefix_hash = hasher.finalize().to_hex()[..PREFIX_HASH_CHARS].to_string();

    let span_hashes = rope
        .spans()
        .iter()
        .take(last + 1)
        .map(|s| {
            let h = blake3::hash(s.content().as_bytes());
            format!("{}={}", s.id(), &h.to_hex()[..SPAN_HASH_CHARS])
        })
        .collect();

    (Some(prefix_hash), span_hashes)
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

    // ── mu-814o: prefix forensics ─────────────────────────────────

    fn forensics_for(rope: &RetainedRope, boundary: usize) -> (Option<String>, Vec<String>) {
        let projection = FauxProviderRenderer::new().render(rope, ProjectionTarget::AgentView);
        prefix_forensics(&projection, &[CacheBoundary::at(boundary)], rope)
    }

    #[test]
    fn no_boundaries_means_no_forensics() {
        let rope = sample_rope();
        let projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        let (hash, spans) = prefix_forensics(&projection, &[], &rope);
        assert!(hash.is_none());
        assert!(spans.is_empty());
    }

    #[test]
    fn forensics_are_deterministic_and_scoped_to_prefix() {
        let rope = sample_rope();
        let (h1, s1) = forensics_for(&rope, 1);
        let (h2, s2) = forensics_for(&rope, 1);
        assert_eq!(h1, h2);
        assert_eq!(s1, s2);
        assert_eq!(h1.as_deref().map(str::len), Some(PREFIX_HASH_CHARS));
        // boundary at 1 → spans 0..=1 hashed, suffix excluded
        assert_eq!(s1.len(), 2);
        assert!(s1[0].starts_with("sys="));
        assert!(s1[1].starts_with("u1="));
        // mutating content AFTER the boundary changes nothing
        let mut spans: Vec<Span> = sample_rope().spans().to_vec();
        spans[3] = Span::new("u2", SpanKind::User, "DIFFERENT", RetentionClass::Warm);
        let (h3, s3) = forensics_for(&RetainedRope::from_spans(spans), 1);
        assert_eq!(h1, h3);
        assert_eq!(s1, s3);
    }

    #[test]
    fn span_mutation_under_stable_id_is_named() {
        // The mu-814o incident class: an early span's content changes
        // while its id stays put. The span hash must name it.
        let (h_before, s_before) = forensics_for(&sample_rope(), 1);
        let mut spans: Vec<Span> = sample_rope().spans().to_vec();
        spans[1] = Span::new(
            "u1",
            SpanKind::User,
            "hi (rehydrated, changed)",
            RetentionClass::Hot,
        );
        let (h_after, s_after) = forensics_for(&RetainedRope::from_spans(spans), 1);

        assert_ne!(h_before, h_after, "prefix hash must flag the invalidation");
        assert_eq!(s_before[0], s_after[0], "untouched span hash stable");
        assert_ne!(s_before[1], s_after[1], "mutated span hash must change");
        assert!(s_after[1].starts_with("u1="), "id names the culprit");
    }

    #[test]
    fn renderer_drift_changes_prefix_hash_but_not_span_hashes() {
        // Same rope, different rendered bytes — the projection-drift
        // suspect. prefix_hash observes the wire layer; span hashes
        // observe the rope. Divergence between the two IS the signal.
        let rope = sample_rope();
        let mut projection = FauxProviderRenderer::new().render(&rope, ProjectionTarget::AgentView);
        let boundaries = [CacheBoundary::at(1)];
        let (h_clean, s_clean) = prefix_forensics(&projection, &boundaries, &rope);

        projection.messages[0].content = "you are mu (drifted projection)".into();
        let (h_drift, s_drift) = prefix_forensics(&projection, &boundaries, &rope);

        assert_ne!(h_clean, h_drift);
        assert_eq!(
            s_clean, s_drift,
            "rope-side hashes must NOT move on render drift"
        );
    }

    #[test]
    fn boundary_move_changes_span_hash_count() {
        let rope = sample_rope();
        let (_, s1) = forensics_for(&rope, 1);
        let (_, s2) = forensics_for(&rope, 2);
        assert_eq!(s1.len(), 2);
        assert_eq!(s2.len(), 3);
        assert_eq!(s1[..], s2[..2], "shared prefix hashes agree");
    }
}
