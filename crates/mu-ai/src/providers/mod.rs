//! Concrete `Provider` implementations.
//!
//! Currently:
//! - `anthropic` — direct API access via `ANTHROPIC_API_KEY`.
//!
//! Future: `openai`, `openrouter`, `anthropic-oauth` (subprocess wrapper
//! around `claude --print`), `openai-oauth` (subprocess wrapper around
//! `codex`), `bedrock`.

pub mod anthropic;
pub mod openai_codex;
pub mod openrouter;
pub mod output_limits;
pub mod sse;

pub use anthropic::AnthropicProvider;
pub use openai_codex::OpenaiCodexProvider;
pub use openrouter::OpenRouterProvider;
