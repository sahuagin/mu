//! Concrete `Provider` implementations.
//!
//! Currently:
//! - `anthropic` — direct API access via `ANTHROPIC_API_KEY`.
//!
//! Future: `openai`, `openrouter`, `anthropic-oauth` (subprocess wrapper
//! around `claude --print`), `openai-oauth` (subprocess wrapper around
//! `codex`), `bedrock`.

pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod openrouter;
pub mod output_limits;
pub mod sse;
pub(crate) mod tool_dialect;
pub mod vllm;

pub use anthropic::AnthropicProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenaiProvider;
pub use openrouter::OpenRouterProvider;
pub use vllm::VllmProvider;
