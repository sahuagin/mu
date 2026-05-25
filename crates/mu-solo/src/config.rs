//! Layered configuration for mu-solo.
//!
//! Sources (highest precedence first):
//!   1. CLI flags
//!   2. Environment variables (`MU_SOLO_*`)
//!   3. `~/.config/mu/solo.toml`
//!   4. Built-in defaults
//!
//! Two namespaces, mirroring tcovert's framing — "local TUI config
//! stays with the TUI, and session/mu config can be sent to mu as a
//! message":
//!
//! - [`TuiConfig`]: lives in the TUI process. `effort`, `focus_mode`,
//!   future theme + key bindings.
//! - [`SessionConfig`]: forwarded to the daemon. Today that's via
//!   `mu serve` CLI flags (`--tools`, `--bash-yolo`) plus
//!   `create_session` params (`provider`, model, cwd). The long-term
//!   plan is a `session.configure` JSON-RPC frame so the TUI can ship
//!   the session-bound block as a single message; until that lands,
//!   the binary splits the struct itself at spawn time.
//!
//! Env var convention: `MU_SOLO_<SECTION>__<KEY>`. Double underscore
//! separates section from key (figment convention). Examples:
//!   `MU_SOLO_TUI__EFFORT=high`
//!   `MU_SOLO_SESSION__MODEL=claude-haiku-4-5`

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

/// Root config. Merged from defaults + TOML + env (in that order),
/// then [`apply_cli_overrides`] applies CLI Options on top.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SoloConfig {
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(default)]
    pub session: SessionConfig,
}

/// TUI-local settings. These never leave the TUI process.
///
/// `#[serde(default)]` lets a partial TOML (e.g. just
/// `[tui]\nfocus_mode = true`) fall through to the Default impl for
/// missing fields rather than erroring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TuiConfig {
    /// Initial value of the `/effort` dial (claude-code-feature-mapping
    /// §17). Display-only in v0 — `ask_session` doesn't carry effort
    /// yet, so the value lives in `App::effort` and surfaces via
    /// /status. Valid values: low, medium, high, xhigh, max.
    pub effort: String,
    /// Whether to start with `/focus` mode on (§16). Default off.
    pub focus_mode: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            effort: "medium".into(),
            focus_mode: false,
        }
    }
}

/// Session-bound settings. Forwarded to the daemon — today via spawn
/// flags + `create_session` params; eventually via a single
/// `session.configure` RPC message.
///
/// `#[serde(default)]` lets a partial TOML fall through to the
/// Default impl for missing fields. Without this, omitting any field
/// in `[session]` would be a hard error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SessionConfig {
    /// Provider for the initial session. Maps to
    /// `CreateSessionRequest.provider.kind` after normalization.
    pub provider: String,
    /// Model id passed alongside the provider.
    pub model: String,
    /// Comma-separated tools registered on the daemon. Forwarded as
    /// `mu serve --tools ...`. Empty string = no tools registered;
    /// the model will then hallucinate tool-call XML as text.
    pub tools: String,
    /// Auto-approve bash invocations (`mu serve --bash-yolo`). Solo
    /// convenience; never enable for sessions whose prompt source
    /// you don't fully trust.
    pub bash_yolo: bool,
    /// Path to the `mu` daemon binary. Strictly a TUI-process
    /// concern (the daemon itself doesn't care), but grouped under
    /// session because it controls which binary the session runs
    /// against.
    pub mu_binary: String,
    /// Working directory passed to the spawned daemon. None = use
    /// the current process cwd at startup time.
    pub cwd: Option<PathBuf>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            provider: "openai-codex".into(),
            model: "gpt-5.5".into(),
            tools: "read,write,edit,glob,grep,bash".into(),
            bash_yolo: false,
            mu_binary: "./target/release/mu".into(),
            cwd: None,
        }
    }
}

/// Resolve the default config file path: `$XDG_CONFIG_HOME/mu/solo.toml`
/// (or `~/.config/mu/solo.toml`). Returns None if no config dir can
/// be determined for this platform/user (e.g. no `$HOME`).
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("mu").join("solo.toml"))
}

/// Load layered config from defaults + TOML + env. CLI overrides are
/// applied separately via [`apply_cli_overrides`] so callers can
/// distinguish "user explicitly set X" from "X happened to match its
/// default" — `Option<T>` at the CLI layer carries that signal.
///
/// Missing TOML file is not an error; the layer is silently skipped.
/// Malformed TOML IS an error so the user notices their typo instead
/// of silently getting defaults.
pub fn load(config_path: Option<&Path>) -> Result<SoloConfig> {
    let mut fig = Figment::from(Serialized::defaults(SoloConfig::default()));
    let path = config_path
        .map(Path::to_path_buf)
        .or_else(default_config_path);
    if let Some(p) = path.as_ref() {
        if p.exists() {
            fig = fig.merge(Toml::file(p));
        }
    }
    // Env: MU_SOLO_TUI__EFFORT=high, MU_SOLO_SESSION__MODEL=opus, etc.
    fig = fig.merge(Env::prefixed("MU_SOLO_").split("__"));
    fig.extract().context("invalid mu-solo config")
}

/// CLI-supplied overrides. Each field is `Option<T>` so we can tell
/// "user set this" from "fall through to TOML/env/default." Apply via
/// [`apply_cli_overrides`].
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub tools: Option<String>,
    pub bash_yolo: Option<bool>,
    pub mu_binary: Option<String>,
    pub cwd: Option<PathBuf>,
    pub effort: Option<String>,
    pub focus_mode: Option<bool>,
}

/// Apply CLI Options on top of an already-loaded config. Some fields
/// override the underlying value; None leaves the lower-precedence
/// layer in place.
pub fn apply_cli_overrides(config: &mut SoloConfig, cli: &CliOverrides) {
    if let Some(v) = &cli.provider {
        config.session.provider = v.clone();
    }
    if let Some(v) = &cli.model {
        config.session.model = v.clone();
    }
    if let Some(v) = &cli.tools {
        config.session.tools = v.clone();
    }
    if let Some(v) = cli.bash_yolo {
        config.session.bash_yolo = v;
    }
    if let Some(v) = &cli.mu_binary {
        config.session.mu_binary = v.clone();
    }
    if let Some(v) = &cli.cwd {
        config.session.cwd = Some(v.clone());
    }
    if let Some(v) = &cli.effort {
        config.tui.effort = v.clone();
    }
    if let Some(v) = cli.focus_mode {
        config.tui.focus_mode = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_serde() {
        let c = SoloConfig::default();
        let s = toml::to_string(&c).expect("serialize");
        let c2: SoloConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(c, c2);
    }

    #[test]
    fn cli_overrides_replace_some_fields() {
        let mut c = SoloConfig::default();
        let cli = CliOverrides {
            provider: Some("anthropic".into()),
            bash_yolo: Some(true),
            ..Default::default()
        };
        apply_cli_overrides(&mut c, &cli);
        assert_eq!(c.session.provider, "anthropic");
        assert!(c.session.bash_yolo);
        // Untouched fields keep their defaults.
        assert_eq!(c.session.model, "gpt-5.5");
    }

    #[test]
    fn toml_partial_inherits_other_defaults() {
        let toml = r#"
            [session]
            model = "claude-haiku-4-5"
        "#;
        let c: SoloConfig = toml::from_str(toml).expect("parse");
        assert_eq!(c.session.model, "claude-haiku-4-5");
        // Other fields default.
        assert_eq!(c.session.provider, "openai-codex");
        assert_eq!(c.tui.effort, "medium");
    }
}
