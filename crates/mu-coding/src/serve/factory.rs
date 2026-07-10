//! Provider and tool factories — map CLI flag values to concrete
//! `Arc<dyn Provider>` / `Vec<Arc<dyn Tool>>` instances.
//!
//! Adding a new provider or tool: extend the match arm here, no
//! changes elsewhere in `mu serve`'s wiring.

use std::sync::Arc;

use anyhow::Result;

use mu_ai::{
    AnthropicProvider, FauxProvider, OllamaProvider, OpenRouterProvider, OpenaiProvider,
    VllmProvider,
};
use mu_core::agent::{Provider, Tool};
use mu_core::context::CacheTtl;
use mu_core::model_catalog::ModelCatalogConfig;
use mu_core::protocol::ProviderSelector;

use crate::tools::{
    AwsReconTool, BashMode, BashTool, EditTool, GlobTool, GrepTool, LsTool, MemoryRecallTool,
    ReadTool, WriteTool,
};

/// Settings that parameterize how the `bash` tool is built.
/// Daemon-level: applies to every session this daemon serves.
#[derive(Debug, Clone, Default)]
pub struct BashSettings {
    /// When true, build the tool in YOLO mode (no allowlist, full
    /// shell, full env). User opt-in via `--bash-yolo`.
    pub yolo: bool,
    /// Strings to merge into the default strict-mode allowlist
    /// (only meaningful when `yolo == false`). Each parsed via
    /// shlex.
    pub extra_allow: Vec<String>,
    /// When true, strict mode requires per-call user approval via
    /// the mu-029 session.input_required flow. Ignored in yolo mode.
    /// User opt-in via `--bash-prompt`.
    pub prompt: bool,
}

impl BashSettings {
    /// Resolve these daemon-level settings into the [`BashMode`] that
    /// gates command execution. PURE — no logging; the `bash` tool's
    /// build path emits the yolo/approval notices. Both the `bash` tool
    /// and the `watch` tool gate commands through this SAME mode, so a
    /// session's watch authority always matches its bash authority
    /// (mu-qnag).
    pub fn resolve_mode(&self) -> BashMode {
        if self.yolo {
            BashMode::Yolo
        } else {
            BashMode::strict_with_extras(&self.extra_allow, self.prompt)
        }
    }
}

/// Factory closure for constructing a provider per session, from
/// a wire-level `ProviderSelector`. Closes over daemon-startup flags
/// (`ephemeral`, `thinking`) that parameterize *how* providers get
/// built, while the selector picks *which* provider.
pub type ProviderFactory =
    Arc<dyn Fn(&ProviderSelector, CacheTtl) -> Result<Arc<dyn Provider>> + Send + Sync>;

/// Construct a `ProviderFactory` from daemon flags. Each session
/// gets its own `Arc<dyn Provider>` built by this closure.
pub fn make_provider_factory(ephemeral: bool, thinking: Option<String>) -> ProviderFactory {
    Arc::new(move |selector: &ProviderSelector, cache_ttl: CacheTtl| {
        build_provider_from_selector(selector, ephemeral, thinking.as_deref(), cache_ttl)
    })
}

/// Construct a single `Provider` from a wire-level `ProviderSelector`.
///
/// This is the per-session construction point. The daemon's startup
/// flags (`ephemeral`, `thinking`) parameterize how the provider is
/// built; the selector picks which provider and which model.
pub fn build_provider_from_selector(
    selector: &ProviderSelector,
    ephemeral: bool,
    thinking: Option<&str>,
    // mu-f1a0: per-session cache TTL tier. Only the Anthropic arm
    // consumes it — other providers have no tiered caching surface.
    cache_ttl: CacheTtl,
) -> Result<Arc<dyn Provider>> {
    match selector {
        // Faux is encoded on the wire as `anthropic_api`-with-a-known
        // sentinel model. We don't actually have a `faux` variant in
        // the protocol — keep it accessible only via in-process tests
        // for now. (Could add later if useful.)
        ProviderSelector::AnthropicApi { model } => {
            // Special-case sentinel models that map to FauxProvider —
            // makes the smoke-test path work without changing the
            // protocol. Any actual claude model id falls through.
            if model == "faux" {
                return Ok(Arc::new(FauxProvider::echo()));
            }
            // mu-upk2: --thinking now enables Anthropic extended thinking
            // (was previously ignored). The provider parses the flag value
            // into an effort level and sends `thinking: {type: adaptive,
            // display: summarized}` + `output_config.effort`.
            let mut provider =
                AnthropicProvider::from_env(model.clone())?.with_cache_ttl(cache_ttl);
            if let Some(t) = thinking {
                if !t.is_empty() {
                    provider = provider.with_thinking_flag(t);
                }
            }
            Ok(Arc::new(provider))
        }
        ProviderSelector::AnthropicOauth { .. } => {
            anyhow::bail!(
                "anthropic_oauth is not yet implemented in mu — \
                 per AGENTS.md it stays subprocess-wrapped via \
                 the claude CLI for the foreseeable future"
            )
        }
        ProviderSelector::OpenaiApi { model } => {
            // Public OpenAI Responses API (direct API key). Same provider
            // struct as the codex/OAuth path, in its API-key mode.
            let provider = OpenaiProvider::from_env(model.clone())
                .map_err(|e| anyhow::anyhow!("openai-api: {e}"))?;
            let provider = match thinking {
                Some(t) if !t.is_empty() => provider.with_thinking(t.to_string()),
                _ => provider,
            };
            Ok(Arc::new(provider))
        }
        ProviderSelector::OpenaiCodex { model } => {
            let provider = if ephemeral {
                OpenaiProvider::from_store_ephemeral(model.clone())
            } else {
                OpenaiProvider::from_store(model.clone())
            }
            .map_err(|e| anyhow::anyhow!("openai-codex: {e}"))?;
            let provider = match thinking {
                Some(t) if !t.is_empty() => provider.with_thinking(t.to_string()),
                _ => provider,
            };
            Ok(Arc::new(provider))
        }
        ProviderSelector::Openrouter { model } => {
            log_thinking_ignored("openrouter", thinking);
            Ok(Arc::new(OpenRouterProvider::from_env(model.clone())?))
        }
        ProviderSelector::Vllm { model } => {
            log_thinking_ignored("vllm", thinking);
            Ok(Arc::new(VllmProvider::from_env(model.clone())?))
        }
        ProviderSelector::Ollama { model } => {
            let mut provider = OllamaProvider::from_env(model.clone())?;
            if let Some(t) = thinking {
                if !t.is_empty() {
                    provider = provider.with_thinking_flag(t);
                }
            }
            Ok(Arc::new(provider))
        }
        // Config-defined provider (mu-v8ye): the selector already carries the
        // resolved (protocol, base_url, api_key), so build the matching wire
        // impl pointed at the endpoint — no env round-trip, no config access.
        ProviderSelector::Configured {
            name,
            protocol,
            base_url,
            api_key,
            model,
        } => match protocol.as_str() {
            "openai-chat" => {
                log_thinking_ignored(name, thinking);
                let provider = OpenRouterProvider::new(api_key.clone(), model.clone())
                    .with_api_base(base_url.clone())
                    .with_api_path("/v1/chat/completions".to_string())
                    // mu-v8ye: label by the configured name so the trait-path
                    // label agrees with the event-path label in session.rs.
                    .with_label(name);
                Ok(Arc::new(provider))
            }
            "anthropic-messages" => {
                let mut provider =
                    OllamaProvider::with_endpoint(base_url.clone(), api_key.clone(), model.clone());
                if let Some(t) = thinking {
                    if !t.is_empty() {
                        provider = provider.with_thinking_flag(t);
                    }
                }
                Ok(Arc::new(provider))
            }
            "openai-responses" => {
                anyhow::bail!(
                    "provider '{name}': protocol 'openai-responses' is OpenAI-hosted \
                     (server-side state / built-in tools) and not yet supported as a \
                     config-defined endpoint; use openai-codex / openai-api"
                )
            }
            other => anyhow::bail!(
                "provider '{name}': unknown protocol '{other}' \
                 (expected: openai-chat, anthropic-messages, openai-responses)"
            ),
        },
    }
}

/// Map a CLI provider flag (`--provider <name>`) to a wire-level
/// `ProviderSelector` with the given model (or each provider's
/// default if `model` is None).
///
/// Used by `mu ask` to translate its CLI surface into what it sends
/// in `create_session`.
pub fn selector_from_cli(name: &str, model: Option<&str>) -> Result<ProviderSelector> {
    match name {
        "faux" => Ok(ProviderSelector::AnthropicApi {
            model: "faux".to_string(),
        }),
        "anthropic-api" => Ok(ProviderSelector::AnthropicApi {
            model: model.unwrap_or("claude-haiku-4-5-20251001").to_string(),
        }),
        "openai-codex" => Ok(ProviderSelector::OpenaiCodex {
            model: model.unwrap_or("gpt-5.5").to_string(),
        }),
        // Public OpenAI Responses API (direct API key).
        "openai" | "openai-api" => Ok(ProviderSelector::OpenaiApi {
            model: model.unwrap_or("gpt-5.5").to_string(),
        }),
        "openrouter" => Ok(ProviderSelector::Openrouter {
            model: model.unwrap_or("anthropic/claude-haiku-4.5").to_string(),
        }),
        "vllm" => Ok(ProviderSelector::Vllm {
            model: model
                .unwrap_or("Qwen/Qwen3-Coder-30B-A3B-Instruct-FP8")
                .to_string(),
        }),
        "ollama" => Ok(ProviderSelector::Ollama {
            model: model.unwrap_or("qwen3-coder:30b").to_string(),
        }),
        // Not a built-in name: try a config-defined `[[providers.endpoints]]`
        // entry (mu-v8ye). Resolves protocol/base_url/api_key here (selection
        // time) so the wire selector is self-contained and the daemon factory
        // needs no config access.
        other => {
            let cfg = mu_core::config::Config::load_default();
            resolve_configured_selector(&cfg.providers, other, model)
        }
    }
}

/// Resolve a config-defined provider name to a self-contained
/// [`ProviderSelector::Configured`] (mu-v8ye): look up the
/// `[[providers.endpoints]]` entry, apply a `<NAME>_BASE_URL` runtime
/// override, and read the API key from `api_key_env` (if any) — so the wire
/// selector carries everything the factory needs.
pub fn resolve_configured_selector(
    providers: &mu_core::config::ProvidersConfig,
    name: &str,
    model: Option<&str>,
) -> Result<ProviderSelector> {
    let Some(ep) = providers.endpoint(name) else {
        let known: Vec<&str> = providers
            .endpoints
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        anyhow::bail!(
            "unknown provider: {name} (built-ins: faux, anthropic-api, openai-codex, \
             openai, openrouter, vllm, ollama; config-defined: {})",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        );
    };
    let model = model.map(str::to_string).ok_or_else(|| {
        anyhow::anyhow!(
            "config-defined provider '{name}' requires an explicit --model (no built-in default)"
        )
    })?;
    // Runtime endpoint override: <NAME>_BASE_URL (upper-cased, non-alnum → '_').
    let env_key = format!("{}_BASE_URL", to_env_prefix(name));
    let base_url = std::env::var(&env_key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ep.base_url.clone());
    let api_key = ep
        .api_key_env
        .as_ref()
        .and_then(|k| std::env::var(k).ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let protocol = match ep.protocol {
        mu_core::config::ProtocolKind::AnthropicMessages => "anthropic-messages",
        mu_core::config::ProtocolKind::OpenaiChat => "openai-chat",
        mu_core::config::ProtocolKind::OpenaiResponses => "openai-responses",
    };
    Ok(ProviderSelector::Configured {
        name: name.to_string(),
        protocol: protocol.to_string(),
        base_url,
        api_key,
        model,
    })
}

/// Upper-case a provider name into an env-var prefix, mapping any non
/// alphanumeric character to `_` (so `card-1` → `CARD_1`).
fn to_env_prefix(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Resolve a possible SELECTION alias before `(provider, model)` reach
/// [`selector_from_cli`]. If `model` names a favorite — its
/// `[favorites.<name>]` table key or one of its `aliases` — the favorite's
/// `{provider, model}` replaces BOTH inputs (a favorite is a complete
/// selection, so it overrides the provider flag too). Otherwise the inputs
/// pass through unchanged.
///
/// This is what lets a short name stand in for a long, typo-prone model tag
/// that then lives in exactly one place (the favorite) instead of being
/// retyped every run. Wiring it as a pre-step here (rather than inside
/// `selector_from_cli`) keeps the wire-level mapping pure. (bead mu-eb98,
/// work item 2)
pub fn resolve_launch_selection(provider: &str, model: Option<&str>) -> (String, Option<String>) {
    resolve_launch_selection_with_catalog(mu_core::model_catalog::global(), provider, model)
}

/// [`resolve_launch_selection`] against an explicit catalog — the testable
/// seam (the public entry point passes the process-global catalog).
fn resolve_launch_selection_with_catalog(
    catalog: &ModelCatalogConfig,
    provider: &str,
    model: Option<&str>,
) -> (String, Option<String>) {
    if let Some(alias) = model {
        if let Some((fav_provider, fav_model)) = catalog.resolve_selection_alias(alias) {
            // The favorite's `provider` may be a catalog provider key/alias;
            // map it to its canonical wire kind so selector_from_cli accepts it.
            let resolved_provider = catalog
                .provider(fav_provider)
                .and_then(|p| p.kind.as_deref())
                .unwrap_or(fav_provider);
            tracing::info!(
                alias,
                provider = resolved_provider,
                model = fav_model,
                "selection alias resolved to favorite"
            );
            return (resolved_provider.to_string(), Some(fav_model.to_string()));
        }
    }
    (provider.to_string(), model.map(str::to_string))
}

fn log_thinking_ignored(provider: &str, thinking: Option<&str>) {
    if let Some(t) = thinking {
        if !t.is_empty() {
            tracing::debug!(
                provider, thinking = %t,
                "--thinking is ignored for this provider (no reasoning surface)"
            );
        }
    }
}

/// Build a tools vec from a list of names. Unknown names error.
///
/// `bash` is the one tool whose construction is parameterized by
/// daemon-level settings (`BashSettings`) rather than being a
/// no-arg `Tool::new()`. Pass `BashSettings::default()` for "off"
/// behavior (yolo=false, no extra allowlist entries).
pub fn build_tools(names: &[String], bash: &BashSettings) -> Result<Vec<Arc<dyn Tool>>> {
    names
        .iter()
        .map(|n| match n.as_str() {
            "read" => Ok(Arc::new(ReadTool::new()) as Arc<dyn Tool>),
            "write" => Ok(Arc::new(WriteTool::new()) as Arc<dyn Tool>),
            "ls" => Ok(Arc::new(LsTool::new()) as Arc<dyn Tool>),
            "edit" => Ok(Arc::new(EditTool::new()) as Arc<dyn Tool>),
            "grep" => Ok(Arc::new(GrepTool::new()) as Arc<dyn Tool>),
            "glob" => Ok(Arc::new(GlobTool::new()) as Arc<dyn Tool>),
            // mu-oee9: recall-on-demand over the operator's memory
            // store — the discoverable tail that the small-kernel
            // injection (mu-zk2i) demotes everything into.
            "memory_recall" => Ok(Arc::new(MemoryRecallTool::new()) as Arc<dyn Tool>),
            "aws_recon" => Ok(
                Arc::new(AwsReconTool::from_env().map_err(|e| anyhow::anyhow!(e))?)
                    as Arc<dyn Tool>,
            ),
            "bash" => {
                // Emit the operator-facing posture notices here (the build
                // path); the mode itself is resolved by the shared
                // `BashSettings::resolve_mode` so the `watch` tool gates
                // through the identical mode (mu-qnag).
                if bash.yolo {
                    tracing::warn!(
                        "bash tool: YOLO MODE active. All allowlist checks bypassed. \
                         Confirm you trust the prompt source."
                    );
                } else if bash.prompt {
                    tracing::info!("bash tool: strict + per-call approval (mu-029) active.");
                }
                Ok(Arc::new(BashTool::new(bash.resolve_mode())) as Arc<dyn Tool>)
            }
            other => anyhow::bail!(
                "unknown tool: {other} (expected: read, write, ls, edit, grep, glob, \
                 memory_recall, aws_recon, bash)"
            ),
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

    /// Test helper: build_tools with default BashSettings (no yolo,
    /// no extra allowlist entries). Keeps test sites tidy.
    fn build_tools_default(names: &[String]) -> Result<Vec<Arc<dyn Tool>>> {
        build_tools(names, &BashSettings::default())
    }

    #[test]
    fn build_from_selector_faux_sentinel() {
        // FauxProvider is reachable via AnthropicApi { model: "faux" }
        // — keeps the protocol simple while preserving the test path.
        let sel = ProviderSelector::AnthropicApi {
            model: "faux".into(),
        };
        assert!(build_provider_from_selector(&sel, false, None, CacheTtl::default()).is_ok());
        // ephemeral / thinking are tolerated even though faux ignores
        // them.
        assert!(
            build_provider_from_selector(&sel, true, Some("high"), CacheTtl::default()).is_ok()
        );
    }

    #[test]
    fn build_from_selector_anthropic_oauth_errors() {
        let sel = ProviderSelector::AnthropicOauth { model: "x".into() };
        match build_provider_from_selector(&sel, false, None, CacheTtl::default()) {
            Ok(_) => panic!("anthropic_oauth should not be implemented"),
            Err(e) => assert!(
                e.to_string().contains("not yet implemented")
                    || e.to_string().contains("anthropic_oauth")
            ),
        }
    }

    #[test]
    fn build_from_selector_openai_api_constructs_or_needs_key() {
        // openai_api is now implemented (public OpenAI Responses API).
        // Construction succeeds when an API key is configured
        // (OPENAI_API_KEY or ~/.config/agent/config.toml); otherwise it
        // fails with a key-missing message — NEVER "not yet implemented".
        let sel = ProviderSelector::OpenaiApi {
            model: "gpt-5".into(),
        };
        match build_provider_from_selector(&sel, false, None, CacheTtl::default()) {
            Ok(_) => {} // a key was available in this environment
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    !msg.contains("not yet implemented"),
                    "openai_api should be implemented now; got: {msg}"
                );
                assert!(
                    msg.contains("API key") || msg.contains("OPENAI_API_KEY"),
                    "error should be about the missing key; got: {msg}"
                );
            }
        }
    }

    #[test]
    fn selector_from_cli_known_providers() {
        let s = selector_from_cli("faux", None).unwrap();
        assert!(matches!(
            s,
            ProviderSelector::AnthropicApi { ref model } if model == "faux"
        ));

        let s = selector_from_cli("anthropic-api", None).unwrap();
        assert!(matches!(s, ProviderSelector::AnthropicApi { .. }));

        let s = selector_from_cli("openai-codex", Some("gpt-5.4")).unwrap();
        assert_eq!(
            s,
            ProviderSelector::OpenaiCodex {
                model: "gpt-5.4".into()
            }
        );

        let s = selector_from_cli("openrouter", None).unwrap();
        match s {
            ProviderSelector::Openrouter { model } => {
                assert!(model.starts_with("anthropic/"))
            }
            _ => panic!("expected Openrouter"),
        }

        let s = selector_from_cli("vllm", None).unwrap();
        assert_eq!(
            s,
            ProviderSelector::Vllm {
                model: "Qwen/Qwen3-Coder-30B-A3B-Instruct-FP8".into()
            }
        );

        // ollama: default model + explicit override (mu-818c).
        let s = selector_from_cli("ollama", None).unwrap();
        assert_eq!(
            s,
            ProviderSelector::Ollama {
                model: "qwen3-coder:30b".into()
            }
        );
        let s = selector_from_cli("ollama", Some("deepseek-r1:32b")).unwrap();
        assert_eq!(
            s,
            ProviderSelector::Ollama {
                model: "deepseek-r1:32b".into()
            }
        );
    }

    #[test]
    fn build_from_selector_ollama_constructs() {
        // Ollama needs no API key; construction must succeed with the
        // baked-in default base (mu-818c).
        let sel = ProviderSelector::Ollama {
            model: "qwen3-coder:30b".into(),
        };
        assert!(build_provider_from_selector(&sel, false, None, CacheTtl::default()).is_ok());
    }

    #[test]
    fn selector_from_cli_unknown_errors() {
        match selector_from_cli("bogus", None) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.to_string().contains("unknown provider")),
        }
    }

    #[test]
    fn configured_selector_resolves_protocol_and_base() {
        use mu_core::config::{ProtocolKind, ProviderEndpoint, ProvidersConfig};
        let providers = ProvidersConfig {
            endpoints: vec![ProviderEndpoint {
                name: "card1".into(),
                protocol: ProtocolKind::OpenaiChat,
                base_url: "http://10.1.1.143:11435".into(),
                api_key_env: None,
            }],
            ..Default::default()
        };
        let s = resolve_configured_selector(&providers, "card1", Some("ornith-q4-r0")).unwrap();
        assert_eq!(
            s,
            ProviderSelector::Configured {
                name: "card1".into(),
                protocol: "openai-chat".into(),
                base_url: "http://10.1.1.143:11435".into(),
                api_key: String::new(),
                model: "ornith-q4-r0".into(),
            }
        );
    }

    #[test]
    fn configured_selector_requires_model() {
        use mu_core::config::{ProtocolKind, ProviderEndpoint, ProvidersConfig};
        let providers = ProvidersConfig {
            endpoints: vec![ProviderEndpoint {
                name: "card1".into(),
                protocol: ProtocolKind::OpenaiChat,
                base_url: "http://x".into(),
                api_key_env: None,
            }],
            ..Default::default()
        };
        assert!(resolve_configured_selector(&providers, "card1", None).is_err());
    }

    #[test]
    fn configured_selector_unknown_name_lists_known() {
        use mu_core::config::{ProtocolKind, ProviderEndpoint, ProvidersConfig};
        let providers = ProvidersConfig {
            endpoints: vec![ProviderEndpoint {
                name: "card1".into(),
                protocol: ProtocolKind::AnthropicMessages,
                base_url: "http://x".into(),
                api_key_env: None,
            }],
            ..Default::default()
        };
        let err = resolve_configured_selector(&providers, "nope", Some("m"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("config-defined: card1"), "got: {err}");
    }

    #[test]
    fn env_prefix_maps_nonalnum_to_underscore() {
        assert_eq!(to_env_prefix("card1"), "CARD1");
        assert_eq!(to_env_prefix("card-1"), "CARD_1");
        assert_eq!(to_env_prefix("ornith.review"), "ORNITH_REVIEW");
    }

    #[test]
    fn selection_alias_rewrites_provider_and_model() {
        use mu_core::model_catalog::{FavoriteConfig, ProviderCatalogConfig};
        let mut catalog = ModelCatalogConfig::default();
        // provider registered under a non-kind key + alias, to exercise the
        // provider-kind mapping (favorite.provider -> canonical wire kind).
        catalog.providers.insert(
            "local".to_string(),
            ProviderCatalogConfig {
                kind: Some("ollama".to_string()),
                aliases: vec!["box".to_string()],
                ..Default::default()
            },
        );
        catalog.favorites.insert(
            "local_reasoner".to_string(),
            FavoriteConfig {
                provider: "local".to_string(), // catalog key, not the wire kind
                model: "qwen3.6:35b-a3b-q8_0".to_string(),
                aliases: vec!["lr".to_string()],
                ..Default::default()
            },
        );

        // A favorite alias as --model rewrites BOTH provider and model, and
        // the favorite's catalog-key provider maps to its wire kind "ollama".
        // The caller-supplied provider ("anthropic-api") is overridden.
        let (p, m) = resolve_launch_selection_with_catalog(&catalog, "anthropic-api", Some("lr"));
        assert_eq!(p, "ollama");
        assert_eq!(m.as_deref(), Some("qwen3.6:35b-a3b-q8_0"));
        // And the resolved pair maps to the right wire selector.
        match selector_from_cli(&p, m.as_deref()).unwrap() {
            ProviderSelector::Ollama { model } => assert_eq!(model, "qwen3.6:35b-a3b-q8_0"),
            other => panic!("expected Ollama selector, got {other:?}"),
        }

        // A non-favorite model passes through untouched (provider preserved).
        let (p2, m2) =
            resolve_launch_selection_with_catalog(&catalog, "ollama", Some("deepseek-r1:32b"));
        assert_eq!(p2, "ollama");
        assert_eq!(m2.as_deref(), Some("deepseek-r1:32b"));

        // No model -> nothing to resolve, passthrough.
        let (p3, m3) = resolve_launch_selection_with_catalog(&catalog, "openai-codex", None);
        assert_eq!(p3, "openai-codex");
        assert_eq!(m3, None);
    }

    #[test]
    fn factory_closure_constructs_per_session() {
        let factory = make_provider_factory(false, None);
        // Two sessions, same kind, different models.
        let sel_a = ProviderSelector::AnthropicApi {
            model: "faux".into(),
        };
        let sel_b = ProviderSelector::AnthropicApi {
            model: "faux".into(),
        };
        let a = factory(&sel_a, CacheTtl::default()).expect("first construction");
        let b = factory(&sel_b, CacheTtl::default()).expect("second construction");
        // Each call returns a fresh Arc; pointer equality is not
        // guaranteed but each should be alive.
        assert!(Arc::strong_count(&a) >= 1);
        assert!(Arc::strong_count(&b) >= 1);
    }

    #[test]
    fn build_tools_known_and_unknown() {
        let tools =
            build_tools_default(&["read".to_string()]).expect("build_tools(read) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "read");

        let tools =
            build_tools_default(&["write".to_string()]).expect("build_tools(write) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "write");

        let tools = build_tools_default(&["read".to_string(), "write".to_string()])
            .expect("build_tools(read,write) should succeed");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].spec().name, "read");
        assert_eq!(tools[1].spec().name, "write");

        let tools =
            build_tools_default(&["ls".to_string()]).expect("build_tools(ls) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "ls");

        let tools =
            build_tools_default(&["edit".to_string()]).expect("build_tools(edit) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "edit");

        let tools =
            build_tools_default(&["grep".to_string()]).expect("build_tools(grep) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "grep");

        let tools =
            build_tools_default(&["glob".to_string()]).expect("build_tools(glob) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "glob");

        // Bash: strict mode by default, yolo by setting.
        let tools = build_tools(&["bash".to_string()], &BashSettings::default())
            .expect("build_tools(bash) should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "bash");
        assert!(tools[0].spec().description.contains("STRICT MODE"));

        let tools = build_tools(
            &["bash".to_string()],
            &BashSettings {
                yolo: true,
                extra_allow: vec![],
                prompt: false,
            },
        )
        .expect("build_tools(bash, yolo) should succeed");
        assert!(tools[0].spec().description.contains("YOLO MODE"));

        // Strict + prompt should give an Ask-permission policy
        // and a description containing "WITH APPROVAL".
        let tools = build_tools(
            &["bash".to_string()],
            &BashSettings {
                yolo: false,
                extra_allow: vec![],
                prompt: true,
            },
        )
        .expect("build_tools(bash, strict+prompt) should succeed");
        let spec = tools[0].spec();
        assert!(spec.description.contains("WITH APPROVAL"));
        assert_eq!(spec.policy.permission, mu_core::agent::PermissionLevel::Ask);

        match build_tools_default(&["bogus".to_string()]) {
            Ok(_) => panic!("expected error for unknown tool"),
            Err(e) => assert!(e.to_string().contains("unknown tool")),
        }
    }

    #[test]
    fn build_tools_empty() {
        let tools = build_tools_default(&[]).unwrap();
        assert!(tools.is_empty());
    }
}
