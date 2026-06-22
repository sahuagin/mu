//! mu-ai: LLM provider abstraction.
//!
//! Each provider is a struct that implements the `Provider` trait
//! (planned). The trait is async — the LLM ecosystem is async-first
//! and bridging that into a sync surface costs more than it saves.
//!
//! ## Planned providers
//!
//! - `anthropic` — direct API via `ANTHROPIC_API_KEY`.
//! - `anthropic-oauth` — subprocess wrapper around `claude --print`.
//!   We never hold the OAuth token. ToS guardrail.
//! - `openai` — direct API via `OPENAI_API_KEY`.
//! - `openai-oauth` — subprocess wrapper around the `codex` CLI for
//!   ChatGPT Pro / OpenAI Codex OAuth. Same guardrail as above.
//! - `openrouter` — direct API. Routes to many models behind one key.
//!
//! ## Planned shared utilities
//!
//! - `oauth/openai-codex` — PKCE flow if we ever do hold tokens. Not
//!   currently planned; keeping the module reserved for symmetry.
//! - `streaming` — SSE / event-stream parsing common across providers.
//! - `models` — a small registry of model ids + their context windows
//!   and cost-per-token, so frontends can show what they're spending.

#![deny(unsafe_code)]

pub mod auth;
pub mod catalog_probe; // bead context-limit-harden-sync: HTTP probes for `mu models sync`
pub mod context;
pub mod faux;
pub mod providers;
pub use context::{AnthropicCacheStrategy, AnthropicProviderRenderer};
pub use faux::{FauxProvider, FauxResponse};
pub use providers::{
    AnthropicProvider, OllamaProvider, OpenRouterProvider, OpenaiApiProvider, OpenaiCodexProvider,
    VllmProvider,
};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty() {
        assert!(!version().is_empty());
    }
}
