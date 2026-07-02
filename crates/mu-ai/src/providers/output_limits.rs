//! Model-aware `max_tokens` lookup shared by Anthropic and OpenAI-shaped
//! provider request bodies.
//!
//! Anthropic's Messages API requires a `max_tokens` field on every request;
//! OpenAI-shaped APIs accept one. A single global value would cap every
//! response — including Opus 4, whose extended-output ceiling is 128000 — at
//! the smallest-supported-model budget, so mu resolves a per-model cap.
//!
//! The per-model and per-family caps are DATA, not code: they live in the model
//! catalog (`models.default.toml` `[models.*]` / `[model_rules.*]`, overridable
//! at runtime via `~/.config/mu/models.toml` — no recompile to add or retune a
//! model). The only value hardcoded here is [`DEFAULT_MAX_OUTPUT_TOKENS`], the
//! conservative floor for a model the catalog knows nothing about.

/// Conservative floor for a model absent from the catalog (no `[models.*]`
/// entry and no matching `[model_rules.*]`). Kept low on purpose: overshooting
/// a server-side max errors, so an unknown model degrades to the smallest safe
/// budget rather than guessing high. Every KNOWN cap belongs in the catalog.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;

/// Returns the `max_tokens` budget mu should send for the given provider model
/// identifier, resolved from the model catalog.
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
///
/// Resolution is the catalog's: an exact `[models.*]` entry wins, else the
/// most-specific matching `[model_rules.*]` prefix (so date-stamped and
/// point-release ids inherit their family cap), else the floor. No family
/// values are duplicated here — they're all in the catalog.
pub fn max_tokens_for_model_with_catalog(
    catalog: &mu_core::model_catalog::ModelCatalogConfig,
    model: &str,
) -> u32 {
    catalog
        .resolve_model(model)
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
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
    fn opus_4_7_gets_128k() {
        // opus-4 family ceiling = 128000 (Anthropic-queried). The exact
        // [models.claude_opus_4_7] entry and the [model_rules.claude_opus_4]
        // family fallback both carry it, so the date-stamped variant matches too.
        assert_eq!(mt("claude-opus-4-7"), 128000);
        assert_eq!(mt("claude-opus-4-7-20260301"), 128000);
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
        assert_eq!(mt("Claude-Opus-4-7"), 128000);
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

    #[test]
    fn empty_catalog_falls_back_to_floor() {
        // Caps are data: with no catalog at all, even a known family drops to
        // the floor (there is no family knowledge in code to fall back on).
        // Not a real path — the built-in catalog is mandatory — but this pins
        // the contract so nobody reintroduces a hardcoded per-family ladder.
        let empty = mu_core::model_catalog::ModelCatalogConfig::default();
        assert_eq!(
            max_tokens_for_model_with_catalog(&empty, "claude-opus-4-7"),
            DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(
            max_tokens_for_model_with_catalog(&empty, "gpt-5"),
            DEFAULT_MAX_OUTPUT_TOKENS
        );
    }
}
