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
}
