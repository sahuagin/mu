//! Provider capability registry.
//!
//! Providers don't expose detailed per-parameter capability matrices
//! over the wire, so what mu knows about field-level constraints is
//! folklore until someone hits a limit (verbatim quote: the 8KB cap
//! on codex's `instructions` field came from a 200-OK silent-stream
//! incident, not documentation). This module is where that folklore
//! becomes typed.
//!
//! Design intent:
//! - Provider-level, not per-model, in v0. Most fields are properties
//!   of the wire protocol (system prompt shape, supported roles),
//!   not properties of the underlying model. Per-model fields
//!   (context window) carry an `Option` for "no curated entry."
//! - Conservative defaults via [`ProviderCapabilities::default`] so
//!   any new provider that doesn't override gets safe behavior:
//!   unknown system-prompt shape, no assumed caching, no special
//!   roles.
//! - Source-of-truth for diagnostics and request-body builders.
//!   The 8KB constant in `mu-ai/providers/openai_codex.rs` should
//!   eventually be sourced from `capabilities().system_prompt
//!   .max_bytes()` rather than living as a free constant.
//!
//! Future paths:
//! - Per-model overrides via a nested `model_capabilities(&str) -> ...`
//!   method when context-window-per-model matters.
//! - Async `refresh()` for providers that publish `/v1/models` style
//!   endpoints (OpenRouter does this). Returns updated caps from the
//!   wire. Hardcoded constants stay as the offline-safe defaults.

use serde::{Deserialize, Serialize};

/// What we know about a provider's wire-protocol capabilities.
///
/// All fields are `Option` or sentinels for "unknown" so that adding
/// a new field to this struct is non-breaking — existing providers
/// that don't override the new field default to "we don't know."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Shape and constraints of the provider's system-prompt surface.
    /// See [`SystemPromptCapability`] for the variants.
    pub system_prompt: SystemPromptCapability,

    /// Provider supports prompt caching (Anthropic `cache_control`,
    /// OpenAI implicit prefix caching, etc.). Used by diagnostics
    /// and as a hint for the [`crate::context::CacheStrategy`]
    /// choice — not a substitute for actually consulting the
    /// strategy.
    pub supports_prompt_caching: bool,

    /// Provider supports a `developer` (or equivalent privileged-role)
    /// message type distinct from `system` / `user` / `assistant`.
    /// OpenAI's Responses API has this; Anthropic and the OpenAI
    /// chat-completions schema do not.
    pub supports_developer_role: bool,

    /// Maximum tools registrable per request. `None` when we haven't
    /// observed a limit (most providers; if one ever lands the value
    /// gets pinned here).
    pub max_tools: Option<usize>,

    /// Default per-model context window in tokens, when the provider
    /// has a single dominant model or a reasonable floor. Per-model
    /// detail is intentionally not modeled in v0 — add a
    /// `model_capabilities(model: &str)` method if/when needed.
    pub context_window_tokens: Option<u32>,

    /// mu-rf9x: how to interpret this provider's reported
    /// [`Usage`](crate::agent::Usage) numbers. See [`UsageSemantics`]
    /// — this is the accounting-convention folklore (OpenAI's
    /// `input_tokens` includes cache reads; Anthropic's buckets are
    /// disjoint) that consumers kept re-deriving wrong.
    pub usage_semantics: UsageSemantics,
}

/// mu-rf9x: a provider's token-usage accounting convention.
///
/// Providers report a [`Usage`](crate::agent::Usage) with the same
/// field names but different *semantics*: OpenAI-shaped APIs report
/// `input_tokens` as the TOTAL prompt (cache reads are a subset
/// detail); Anthropic reports `input_tokens`, `cache_read`, and
/// `cache_creation` as DISJOINT buckets that sum to the prompt.
/// Consumers that do their own arithmetic on raw `Usage` get one of
/// the two wrong — this struct is the provider's self-declaration so
/// nobody downstream has to guess (three sites guessed wrong on
/// 2026-06-03 alone: the status-bar context display, deep-analyze.py,
/// and a filed-then-invalidated compaction bug).
///
/// Every field is `Option<bool>`; `None` means "we don't know this
/// provider's convention." Readers must not assume — see
/// [`Self::prompt_total`], which returns `None` rather than guessing
/// when the convention is unknown AND the answer would depend on it.
///
/// The declaration is stamped into the durable event log on
/// `SessionCreated` and `ProviderSwitched` events, so log readers can
/// fold the convention-in-force forward through the session without
/// per-event duplication (it only changes when the provider does).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSemantics {
    /// `Usage::input_tokens` already includes cache-read tokens
    /// (`Some(true)`, OpenAI convention) vs. cache reads are an
    /// additional disjoint bucket (`Some(false)`, Anthropic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_in_input: Option<bool>,
    /// Same question for cache-creation (cache-write) tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_in_input: Option<bool>,
    /// `Usage::output_tokens` already includes reasoning tokens
    /// (`Some(true)`, OpenAI Responses convention) vs. reasoning is
    /// billed as an additional bucket (`Some(false)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_in_output: Option<bool>,
}

impl UsageSemantics {
    /// OpenAI-shaped accounting (Responses API, chat completions,
    /// OpenAI-compatible proxies): `input_tokens` is the total
    /// prompt; cache reads and reasoning are subset details.
    pub fn openai_style() -> Self {
        Self {
            cache_read_in_input: Some(true),
            cache_creation_in_input: Some(true),
            reasoning_in_output: Some(true),
        }
    }

    /// Anthropic Messages accounting: `input_tokens`, `cache_read`,
    /// and `cache_creation` are disjoint buckets that sum to the
    /// prompt; thinking is billed inside `output_tokens`.
    pub fn anthropic_style() -> Self {
        Self {
            cache_read_in_input: Some(false),
            cache_creation_in_input: Some(false),
            reasoning_in_output: Some(true),
        }
    }

    /// Total prompt tokens for one call under this convention —
    /// THE number to display as "context size." Returns `None` when
    /// the convention is unknown and the reported fields make the
    /// answer depend on it (cache fields present but additivity
    /// undeclared). When no cache fields were reported, the
    /// convention is moot and `input_tokens` is the answer
    /// regardless.
    pub fn prompt_total(&self, u: &crate::agent::Usage) -> Option<u64> {
        let mut total = u.input_tokens;
        for (flag, bucket) in [
            (self.cache_read_in_input, u.cache_read_input_tokens),
            (self.cache_creation_in_input, u.cache_creation_input_tokens),
        ] {
            match (flag, bucket.unwrap_or(0)) {
                (_, 0) => {}                    // bucket absent/empty: moot
                (Some(true), _) => {}           // already inside input_tokens
                (Some(false), n) => total += n, // disjoint: add it
                (None, _) => return None,       // unknown + material: don't guess
            }
        }
        Some(total)
    }

    /// Freshly-processed (uncached) prompt tokens under this
    /// convention, when computable. The complement of cache reads
    /// within [`Self::prompt_total`].
    pub fn fresh_input(&self, u: &crate::agent::Usage) -> Option<u64> {
        match (
            self.cache_read_in_input,
            u.cache_read_input_tokens.unwrap_or(0),
        ) {
            (_, 0) => Some(u.input_tokens),
            (Some(true), n) => Some(u.input_tokens.saturating_sub(n)),
            (Some(false), _) => Some(u.input_tokens),
            (None, _) => None,
        }
    }
}

/// How a provider exposes the "system prompt" concept on the wire,
/// and what we know about its size constraints.
///
/// This isn't the same axis as "does the provider support system
/// instructions at all" — every modern provider does. What differs
/// is *where* you put them, which determines what limits apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPromptCapability {
    /// Dedicated top-level request field. Examples:
    /// - OpenAI Responses API: `instructions` (silently fails over ~8KB; see codex incident)
    /// - Anthropic Messages API: `system` (no observed limit; supports cache_control)
    ///
    /// `max_bytes = None` means we haven't observed a ceiling — the
    /// field appears to handle arbitrary length. `Some(n)` is the
    /// soft cap above which mu's request builder should overflow
    /// content into the input messages array.
    TopLevelField { max_bytes: Option<usize> },

    /// System content is inlined as a regular message in the input
    /// array (`{role: "system", content: "..."}`). OpenAI chat
    /// completions and most OpenAI-compatible providers (OpenRouter
    /// when proxying to OpenAI-shaped backends) use this shape.
    /// Effective limit is the per-message budget of the backing
    /// model rather than a separate cap on the system slot.
    MessageRole,

    /// We don't have curated information for this provider yet.
    /// Builders should treat this as "be conservative" — don't
    /// assume the provider can handle arbitrary system content
    /// without observation.
    Unknown,
}

impl SystemPromptCapability {
    /// Convenience: the effective byte ceiling we should respect for
    /// the system-prompt slot. `None` when the slot is unbounded or
    /// the shape isn't size-constrained (a `MessageRole` system is
    /// limited by per-message budgets, not a separate system cap).
    pub fn max_bytes(&self) -> Option<usize> {
        match self {
            Self::TopLevelField { max_bytes } => *max_bytes,
            Self::MessageRole => None,
            Self::Unknown => None,
        }
    }
}

impl Default for ProviderCapabilities {
    /// Conservative defaults: "we don't know what this provider
    /// supports." Override per-provider via `Provider::capabilities`.
    fn default() -> Self {
        Self {
            system_prompt: SystemPromptCapability::Unknown,
            supports_prompt_caching: false,
            supports_developer_role: false,
            max_tools: None,
            context_window_tokens: None,
            usage_semantics: UsageSemantics::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_conservative() {
        let c = ProviderCapabilities::default();
        assert!(matches!(c.system_prompt, SystemPromptCapability::Unknown));
        assert!(!c.supports_prompt_caching);
        assert!(!c.supports_developer_role);
        assert!(c.max_tools.is_none());
        assert!(c.context_window_tokens.is_none());
    }

    #[test]
    fn system_prompt_max_bytes_lookup() {
        assert_eq!(
            SystemPromptCapability::TopLevelField {
                max_bytes: Some(8 * 1024)
            }
            .max_bytes(),
            Some(8 * 1024)
        );
        assert_eq!(
            SystemPromptCapability::TopLevelField { max_bytes: None }.max_bytes(),
            None
        );
        assert_eq!(SystemPromptCapability::MessageRole.max_bytes(), None);
        assert_eq!(SystemPromptCapability::Unknown.max_bytes(), None);
    }

    fn usage(
        input: u64,
        cache_read: Option<u64>,
        cache_creation: Option<u64>,
    ) -> crate::agent::Usage {
        crate::agent::Usage {
            input_tokens: input,
            output_tokens: 100,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
            reasoning_tokens: None,
        }
    }

    /// The 2026-06-03 "93k" incident, as a test: codex reported
    /// input=55,577 with cache_read=37,632 (a subset). The buggy
    /// consumer arithmetic displayed 93,209; the declared semantics
    /// give the true prompt total.
    #[test]
    fn openai_style_prompt_total_does_not_double_count_cache() {
        let s = UsageSemantics::openai_style();
        let u = usage(55_577, Some(37_632), None);
        assert_eq!(s.prompt_total(&u), Some(55_577));
        assert_eq!(s.fresh_input(&u), Some(55_577 - 37_632));
    }

    #[test]
    fn anthropic_style_prompt_total_sums_disjoint_buckets() {
        let s = UsageSemantics::anthropic_style();
        let u = usage(1_000, Some(20_000), Some(3_000));
        assert_eq!(s.prompt_total(&u), Some(24_000));
        assert_eq!(s.fresh_input(&u), Some(1_000));
    }

    #[test]
    fn unknown_semantics_refuse_to_guess_when_it_matters() {
        let s = UsageSemantics::default();
        // Cache fields present and material → the answer depends on
        // the convention we don't know. None, not a guess.
        assert_eq!(s.prompt_total(&usage(50_000, Some(10_000), None)), None);
        assert_eq!(s.fresh_input(&usage(50_000, Some(10_000), None)), None);
        // No cache activity → convention is moot; input IS the total.
        assert_eq!(s.prompt_total(&usage(50_000, None, None)), Some(50_000));
        assert_eq!(s.prompt_total(&usage(50_000, Some(0), None)), Some(50_000));
        assert_eq!(s.fresh_input(&usage(50_000, None, None)), Some(50_000));
    }
}
