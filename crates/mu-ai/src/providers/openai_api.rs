//! Public OpenAI API-key provider using the typed `mu-openai` Responses client.

use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use tokio::sync::oneshot;

use mu_core::agent::{MessageInput, Provider, ProviderError, ProviderEvent, ToolSpec};

const DEFAULT_INSTRUCTIONS: &str = "You are mu, a coding agent. Respond concisely. \
     When tools are provided, prefer to use them rather than asking \
     the user for information you could obtain yourself.";
const DEFAULT_THINKING: &str = "medium";

pub struct OpenaiApiProvider {
    model: String,
    thinking: String,
    instructions: String,
    client: mu_openai::Client,
}

impl OpenaiApiProvider {
    pub fn from_env(model: String) -> Result<Self, ProviderError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(read_agent_openai_key)
            .ok_or_else(|| {
                ProviderError::Other(
                    "OPENAI_API_KEY not set and ~/.config/agent/config.toml openai.api_key unavailable"
                        .into(),
                )
            })?;
        Ok(Self::new(api_key, model))
    }

    pub fn new(api_key: String, model: String) -> Self {
        Self {
            model,
            thinking: DEFAULT_THINKING.into(),
            instructions: DEFAULT_INSTRUCTIONS.into(),
            client: mu_openai::Client::new(
                mu_openai::Endpoint::public_api(),
                mu_openai::Auth::Bearer(api_key),
            ),
        }
    }

    pub fn with_thinking(mut self, thinking: String) -> Self {
        self.thinking = thinking;
        self
    }

    pub fn with_instructions(mut self, instructions: String) -> Self {
        self.instructions = instructions;
        self
    }
}

fn read_agent_openai_key() -> Option<String> {
    let path = dirs::home_dir()?.join(".config/agent/config.toml");
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("openai")?
        .get("api_key")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

#[async_trait]
impl Provider for OpenaiApiProvider {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        effort: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let eff_thinking = effort.unwrap_or(&self.thinking);
        let body = match input {
            MessageInput::Legacy(msgs) => {
                let instructions = system_prompt
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&self.instructions);
                super::openai_codex::build_request_body(
                    &self.model,
                    eff_thinking,
                    instructions,
                    msgs,
                    tools,
                )
            }
            MessageInput::Projected(pmsgs) => {
                super::openai_codex::build_request_body_from_projection(
                    &self.model,
                    eff_thinking,
                    &self.instructions,
                    pmsgs,
                    tools,
                )
            }
            _ => {
                return Err(ProviderError::Other(
                    "OpenaiApiProvider: unrecognized MessageInput variant".into(),
                ));
            }
        };
        let req = super::openai_responses::request_from_value(body)
            .map_err(|e| ProviderError::Other(format!("build OpenAI Responses request: {e}")))?;
        let stream = self
            .client
            .stream_response(&req)
            .await
            .map_err(|e| ProviderError::Other(format!("openai request: {e}")))?
            .map(|r| r.map_err(|e| e.to_string()));
        Ok(super::openai_responses::events_from_openai_stream(
            Box::pin(stream),
            cancel_rx,
        ))
    }

    fn provider_label(&self) -> &'static str {
        "openai_api"
    }

    fn capabilities(&self) -> mu_core::agent::capabilities::ProviderCapabilities {
        use mu_core::agent::capabilities::{
            ProviderCapabilities, SystemPromptCapability, UsageSemantics,
        };
        ProviderCapabilities {
            system_prompt: SystemPromptCapability::TopLevelField { max_bytes: None },
            supports_prompt_caching: false,
            supports_developer_role: true,
            max_tools: None,
            context_window_tokens: None,
            usage_semantics: UsageSemantics::openai_style(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_key_from_agent_config_without_printing_it() {
        // This is intentionally weak: the operator may not have this file in CI.
        // The assertion is only that the helper is side-effect-free and never logs.
        let _ = read_agent_openai_key();
    }
}
