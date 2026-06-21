//! Model-aware `max_tokens` lookup shared by Anthropic and OpenAI-shaped
//! provider request bodies.
//!
//! Anthropic's Messages API requires a `max_tokens` field on every
//! request; OpenAI-shaped APIs accept one. Hardcoding a single value
//! (mu's pre-mu-ql2 default was 4096) caps every response — including
//! Opus 4.7, which natively supports 32768 output tokens — at the
//! smallest-supported-model budget. This module returns a per-model
//! cap so each request gets the right ceiling.
//!
//! The fallback is intentionally conservative: any unknown model name
//! gets 4096 (the prior global default), so adding a new provider or
//! model doesn't silently exceed its server-side maximum.

/// Returns the `max_tokens` budget mu should send for the given
/// provider model identifier.
///
/// Matching is by prefix on the *family* portion of the name, so
/// version suffixes (date stamps, point releases) don't need
/// case-by-case entries. Match-from-most-specific-to-least keeps
/// `claude-haiku-4-5-...` from accidentally hitting a generic
/// `claude-*` rule.
pub fn max_tokens_for_model(model: &str) -> u32 {
    max_tokens_for_model_with_catalog(mu_core::model_catalog::global(), model)
}

/// [`max_tokens_for_model`] against an explicit catalog — the testable seam.
///
/// The public entry point passes the process-global catalog; tests pass a
/// controlled one (e.g. [`mu_core::model_catalog::built_in`]) so a user's
/// `~/.config/mu/models.toml` cannot change the asserted value. (A test that
/// read `global()` broke the build the moment an operator tuned their own
/// per-model `max_output_tokens`. bead mu-nzxa.)
pub fn max_tokens_for_model_with_catalog(
    catalog: &mu_core::model_catalog::ModelCatalogConfig,
    model: &str,
) -> u32 {
    if let Some(v) = catalog.resolve_model(model).max_output_tokens {
        return v;
    }
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude-opus-4") {
        16384
    } else if m.starts_with("claude-sonnet-4") || m.starts_with("claude-haiku-4") {
        8192
    } else if m.starts_with("gpt-5") || m.starts_with("o4") || m.starts_with("o3") {
        16384
    } else if m.starts_with("gpt-oss") || m.starts_with("deepseek-r1") || m.starts_with("qwen3.6:")
    {
        // ollama-served reasoning models: thinking arrives on the
        // reasoning channel and counts against max_tokens. Measured
        // 2026-06-05: gpt-oss:20b reviewing a ~500-line diff burned all
        // 4096 tokens reasoning (finish=length, content EMPTY); at 16384
        // the same prompt finished with room to spare. A 4k cap starves
        // these models of any visible output on non-trivial prompts.
        16384
    } else {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Assert against the compiled-in DEFAULT catalog, never the operator's
    // ~/.config/mu/models.toml — a test must not break when the user tunes
    // their own per-model limits (bead mu-nzxa).
    fn mt(model: &str) -> u32 {
        max_tokens_for_model_with_catalog(&mu_core::model_catalog::built_in(), model)
    }

    #[test]
    fn opus_4_7_gets_16k() {
        assert_eq!(mt("claude-opus-4-7"), 16384);
        assert_eq!(mt("claude-opus-4-7-20260301"), 16384);
    }

    #[test]
    fn haiku_4_5_gets_8k() {
        assert_eq!(mt("claude-haiku-4-5"), 8192);
        assert_eq!(mt("claude-haiku-4-5-20251001"), 8192);
    }

    #[test]
    fn sonnet_4_6_gets_8k() {
        assert_eq!(mt("claude-sonnet-4-6"), 8192);
    }

    #[test]
    fn unknown_model_gets_safe_fallback() {
        assert_eq!(mt("some-future-model-v9"), 4096);
        assert_eq!(mt("claude-test"), 4096);
        assert_eq!(mt(""), 4096);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(mt("Claude-Opus-4-7"), 16384);
        assert_eq!(mt("CLAUDE-HAIKU-4-5"), 8192);
    }

    #[test]
    fn openai_reasoning_models_get_16k() {
        assert_eq!(mt("gpt-5"), 16384);
        assert_eq!(mt("o4-mini"), 16384);
        assert_eq!(mt("deepseek/deepseek-v4-pro"), 16384);
    }

    #[test]
    fn ollama_reasoning_models_get_16k() {
        assert_eq!(mt("gpt-oss:20b"), 16384);
        assert_eq!(mt("deepseek-r1:32b"), 16384);
        assert_eq!(mt("qwen3.6:35b-a3b-q8_0"), 16384);
        // Non-reasoning local models keep the conservative default.
        assert_eq!(mt("qwen3-coder:30b"), 4096);
    }
}
