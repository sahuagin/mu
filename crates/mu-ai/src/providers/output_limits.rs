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
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude-opus-4") {
        16384
    } else if m.starts_with("claude-sonnet-4") || m.starts_with("claude-haiku-4") {
        8192
    } else if m.starts_with("gpt-5") || m.starts_with("o4") || m.starts_with("o3") {
        16384
    } else {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_4_7_gets_16k() {
        assert_eq!(max_tokens_for_model("claude-opus-4-7"), 16384);
        assert_eq!(max_tokens_for_model("claude-opus-4-7-20260301"), 16384);
    }

    #[test]
    fn haiku_4_5_gets_8k() {
        assert_eq!(max_tokens_for_model("claude-haiku-4-5"), 8192);
        assert_eq!(max_tokens_for_model("claude-haiku-4-5-20251001"), 8192);
    }

    #[test]
    fn sonnet_4_6_gets_8k() {
        assert_eq!(max_tokens_for_model("claude-sonnet-4-6"), 8192);
    }

    #[test]
    fn unknown_model_gets_safe_fallback() {
        assert_eq!(max_tokens_for_model("some-future-model-v9"), 4096);
        assert_eq!(max_tokens_for_model("claude-test"), 4096);
        assert_eq!(max_tokens_for_model(""), 4096);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(max_tokens_for_model("Claude-Opus-4-7"), 16384);
        assert_eq!(max_tokens_for_model("CLAUDE-HAIKU-4-5"), 8192);
    }

    #[test]
    fn openai_reasoning_models_get_16k() {
        assert_eq!(max_tokens_for_model("gpt-5"), 16384);
        assert_eq!(max_tokens_for_model("o4-mini"), 16384);
    }
}
