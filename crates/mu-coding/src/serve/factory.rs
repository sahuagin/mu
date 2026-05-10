//! Provider and tool factories — map CLI flag values to concrete
//! `Arc<dyn Provider>` / `Vec<Arc<dyn Tool>>` instances.
//!
//! Adding a new provider or tool: extend the match arm here, no
//! changes elsewhere in `mu serve`'s wiring.

use std::sync::Arc;

use anyhow::Result;

use mu_ai::{AnthropicProvider, FauxProvider, OpenRouterProvider, OpenaiCodexProvider};
use mu_core::agent::{Provider, Tool};

use crate::tools::{LsTool, ReadTool, WriteTool};

/// Build a `Provider` from a CLI flag value.
///
/// `model` is provider-specific. Faux ignores it; anthropic-api
/// defaults to `claude-haiku-4-5-20251001` if `model` is None;
/// openai-codex defaults to `gpt-5-codex`.
///
/// `ephemeral` only affects providers that hold rotatable OAuth
/// tokens (currently: openai-codex). When true, refreshed tokens
/// stay in memory and aren't written back to the token store.
pub fn build_provider(
    name: &str,
    model: Option<&str>,
    ephemeral: bool,
) -> Result<Arc<dyn Provider>> {
    match name {
        "faux" => Ok(Arc::new(FauxProvider::echo())),
        "anthropic-api" => {
            let model = model
                .unwrap_or("claude-haiku-4-5-20251001")
                .to_string();
            Ok(Arc::new(AnthropicProvider::from_env(model)?))
        }
        "openai-codex" => {
            // ChatGPT-account auth (chatgpt.com/backend-api) has its
            // own model namespace, distinct from api.openai.com.
            // `gpt-5-codex` is API-tier only; ChatGPT subscribers
            // get `gpt-5.X`/`gpt-5.X-codex` variants. gpt-5.5 has
            // 1M context.
            let model = model.unwrap_or("gpt-5.5").to_string();
            let provider = if ephemeral {
                OpenaiCodexProvider::from_store_ephemeral(model)
            } else {
                OpenaiCodexProvider::from_store(model)
            }
            .map_err(|e| anyhow::anyhow!("openai-codex: {e}"))?;
            Ok(Arc::new(provider))
        }
        "openrouter" => {
            let model = model
                .unwrap_or("anthropic/claude-haiku-4.5")
                .to_string();
            Ok(Arc::new(OpenRouterProvider::from_env(model)?))
        }
        other => anyhow::bail!(
            "unknown provider: {other} (expected: faux, anthropic-api, openai-codex, openrouter)"
        ),
    }
}

/// Build a tools vec from a list of names. Unknown names error.
pub fn build_tools(names: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
    names
        .iter()
        .map(|n| match n.as_str() {
            "read" => Ok(Arc::new(ReadTool::new()) as Arc<dyn Tool>),
            "write" => Ok(Arc::new(WriteTool::new()) as Arc<dyn Tool>),
            "ls" => Ok(Arc::new(LsTool::new()) as Arc<dyn Tool>),
            other => anyhow::bail!("unknown tool: {other} (expected: read, write, ls)"),
        })
        .collect()
}

/// Parse a comma-separated tools list, ignoring empty entries (so
/// `--tools ""` and `--tools "read,"` both behave sanely).
pub fn parse_tools_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tools_csv_handles_empty() {
        assert!(parse_tools_csv("").is_empty());
        assert!(parse_tools_csv(",,").is_empty());
        assert!(parse_tools_csv(" ").is_empty());
    }

    #[test]
    fn parse_tools_csv_trims_and_splits() {
        assert_eq!(parse_tools_csv("read"), vec!["read"]);
        assert_eq!(
            parse_tools_csv("read, write , bash"),
            vec!["read", "write", "bash"]
        );
    }

    #[test]
    fn build_provider_faux() {
        assert!(build_provider("faux", None, false).is_ok());
        assert!(build_provider("faux", Some("ignored"), false).is_ok());
        // ephemeral=true is tolerated for providers that ignore it.
        assert!(build_provider("faux", None, true).is_ok());
    }

    #[test]
    fn build_provider_unknown_errors() {
        // Can't `.unwrap_err()` because Arc<dyn Provider>: !Debug.
        match build_provider("bogus", None, false) {
            Ok(_) => panic!("expected error for unknown provider"),
            Err(e) => assert!(e.to_string().contains("unknown provider")),
        }
    }

    #[test]
    fn build_tools_known_and_unknown() {
        let tools = build_tools(&["read".to_string()]).expect("build_tools(read) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "read");

        let tools =
            build_tools(&["write".to_string()]).expect("build_tools(write) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "write");

        let tools = build_tools(&["read".to_string(), "write".to_string()])
            .expect("build_tools(read,write) should succeed");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].spec().name, "read");
        assert_eq!(tools[1].spec().name, "write");

        let tools = build_tools(&["ls".to_string()]).expect("build_tools(ls) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "ls");

        match build_tools(&["bogus".to_string()]) {
            Ok(_) => panic!("expected error for unknown tool"),
            Err(e) => assert!(e.to_string().contains("unknown tool")),
        }
    }

    #[test]
    fn build_tools_empty() {
        let tools = build_tools(&[]).unwrap();
        assert!(tools.is_empty());
    }
}
