//! mu-l1z: centralized, layered configuration.
//!
//! Operator-controllable defaults the daemon reads at startup.
//! Replaces the prior pattern of scattered env-vars + code defaults +
//! per-session ad-hoc construction.
//!
//! ## Why this exists
//!
//! Decisions that vary by operator preference shouldn't live in
//! source code. They live in a TOML file, version-controllable,
//! editable without recompilation. Code defaults take over when the
//! file is absent or omits a field, so an operator with no config
//! file gets the same behavior as before this module landed.
//!
//! ## Layered resolution
//!
//! In ascending priority (later overlays earlier):
//!
//! 1. **Code defaults** — `Config::default()`; always present.
//! 2. **Site config** — `/etc/mu/config.toml` (optional; for shared
//!    hosts).
//! 3. **Operator config** — `~/.config/mu/config.toml` (per-user;
//!    the main one).
//! 4. **Per-daemon override** — CLI flags to `mu serve` and
//!    `mu-tui` resolved by their consumers after [`Config::load`].
//! 5. **Per-session override** — RPC params (e.g.
//!    `create_session.provider`) handled at the dispatch layer.
//!
//! Steps 1–3 happen inside [`Config::load`] via deep-merge of
//! [`toml::Value`] trees. Steps 4–5 are consumer-side: each consumer
//! is responsible for resolving its own CLI/RPC overrides against
//! the loaded [`Config`].
//!
//! ## Failure model
//!
//! Loading NEVER panics or surfaces an error to the caller. Missing
//! files are silent skips (operators without a config file are the
//! common case). Malformed TOML emits a [`tracing::warn`] and the
//! file is skipped — the daemon continues with whatever layers it
//! could load. The operator sees the warning in logs; the daemon
//! doesn't refuse to start over a broken config.
//!
//! ## What's NOT in v1
//!
//! - **Live reload** (SIGHUP / file-watch). Config is read once at
//!   startup; daemon restart picks up changes.
//! - **Config-via-RPC** (`config.get` / `config.set`). v1 is
//!   filesystem-only.
//! - **`ConfigCapability` axis** on the `Capability` model. The
//!   bead's "config = capability axis" unification is a follow-up.
//! - **`config_set` events** on the event log. The
//!   "config-changes-as-events" unification is a follow-up.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Top-level mu config. Loaded from TOML; every nested struct is
/// `#[serde(default)]` so partial TOML files compose cleanly with
/// code defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `[compaction]` — policy selection + judge ranking.
    pub compaction: CompactionConfig,
    /// `[providers]` — per-provider auth ranking + proxy settings.
    pub providers: ProvidersConfig,
    /// `[session]` — persistence + state-dir.
    pub session: SessionConfig,
    /// `[ui]` — TUI defaults.
    pub ui: UiConfig,
    /// `[budget]` — soft daily/weekly warning thresholds.
    pub budget: BudgetConfig,
    /// `[auth]` — connect-time SASL-shaped handshake config (mu-7rk).
    pub auth: AuthConfig,
}

/// `[compaction]` section. Maps to the agent loop's threshold-cross
/// policy dispatch (mu-kgu.4). v1 supplies the threshold; mu-kgu.11
/// will consume `judge.ranking` to wire a live judge model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// Identifier of the policy to instantiate at session creation.
    /// `"no-compaction"` keeps pre-mu-kgu behavior; `"heuristic"`
    /// selects [`crate::context::compaction::heuristic`];
    /// `"hash-and-summary"` selects [`crate::context::compaction::hash_summary`].
    pub default_policy: String,
    /// Token threshold above which the agent loop runs
    /// `compaction_policy().compact(...)` between turns. Matches
    /// [`crate::agent::DEFAULT_COMPACTION_THRESHOLD`] when defaulted.
    pub trigger_threshold_tokens: usize,
    /// Judge model preference list (mu-kgu.11 consumer).
    pub judge: CompactionJudgeConfig,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            default_policy: "no-compaction".to_string(),
            trigger_threshold_tokens: crate::agent::DEFAULT_COMPACTION_THRESHOLD,
            judge: CompactionJudgeConfig::default(),
        }
    }
}

/// `[compaction.judge]` section. Defines the ordered preference list
/// the daemon walks when picking a judge model for HashAndSummary
/// compaction. First entry whose `(provider, auth)` is available
/// wins. mu-kgu.11 implements the walk-and-fall-back logic; v1 just
/// stores the data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionJudgeConfig {
    /// Ranked list. Empty `ranking` means "no judge configured"; the
    /// HashAndSummary policy falls back to its hard-coded canned
    /// judge in that case (mu-kgu.3 behavior).
    pub ranking: Vec<JudgeRankingEntry>,
    /// `"index_keep"` (mu-kgu.7 rung-B) or `"hash_keep"` (mu-kgu.3 v1).
    pub output_mode: String,
    /// Wall-clock cap for a single judge call.
    pub timeout_secs: u64,
}

/// One entry in [`CompactionJudgeConfig::ranking`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeRankingEntry {
    /// `"openrouter"`, `"anthropic"`, `"openai"`, etc. — matches the
    /// strings used in [`crate::protocol::ProviderSelector`] discriminants.
    pub provider: String,
    /// Model id passed to the provider (e.g. `"claude-haiku-4-5"`).
    pub model: String,
    /// `"api_key"` or `"oauth"`.
    pub auth: String,
}

/// `[providers]` section. Auth-source ranking + optional proxy.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProvidersConfig {
    /// Preference order for Anthropic auth (e.g. `["oauth", "api_key"]`).
    pub anthropic_auth_ranking: Vec<String>,
    /// Preference order for Openrouter auth.
    pub openrouter_auth_ranking: Vec<String>,
    /// Preference order for OpenAI Codex auth.
    pub codex_auth_ranking: Vec<String>,
    /// Optional proxy used for some providers.
    pub proxy: ProxyConfig,
}

/// `[providers.proxy]` section. When `url` is set, the listed
/// providers route through it. Used for `claude_proxy` (mu's
/// OAuth-mediating local proxy on `http://127.0.0.1:3180`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    /// `None` disables proxying.
    pub url: Option<String>,
    /// Provider names to route through the proxy.
    pub use_for: Vec<String>,
}

/// `[session]` section. Persistence + resume policy + state directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// mu-upb: write event-log JSONL to disk under
    /// `<state_dir>/events/<daemon_id>/<session_id>.jsonl`. Today's
    /// default behavior; `false` disables persistence entirely
    /// (ephemeral / tests).
    pub persist_events_to_disk: bool,
    /// mu-mh4 (future): on daemon restart, reload sessions from
    /// disk-persisted event logs. v1 defaults to `false`; the
    /// read-side machinery for resume lands in mu-mh4.
    pub resume_on_daemon_restart: bool,
    /// Root directory for daemon state (events + future
    /// compacted-snapshot blobs). `None` resolves to the platform
    /// default at runtime (typically `~/.local/share/mu`).
    pub state_dir: Option<PathBuf>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            persist_events_to_disk: true,
            resume_on_daemon_restart: false,
            state_dir: None,
        }
    }
}

/// `[ui]` section. v1 has only `[ui.tui]`; web/inspector frontends
/// would land here later.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub tui: TuiConfig,
}

/// `[ui.tui]` section. Defaults the TUI's `--provider` /
/// `--model` flags consult when not explicitly given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    /// Provider kind used by the TUI's `n` (new session) shortcut.
    /// Matches the `ProviderSelector::*` discriminants
    /// (`"anthropic_api"`, `"openai_codex"`, etc.).
    pub default_provider: String,
    /// Model id used by the TUI's `n` shortcut.
    pub default_model: String,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            default_provider: "anthropic_api".to_string(),
            default_model: "claude-haiku-4-5-20251001".to_string(),
        }
    }
}

/// `[budget]` section. Soft thresholds for operator awareness. Hard
/// caps still come from CLI flags (`--max-budget-usd`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    pub api_key_daily_warn_usd: Option<f64>,
    pub api_key_weekly_warn_usd: Option<f64>,
}

/// `[auth]` section — connect-time SASL-shaped handshake configuration
/// (mu-7rk). v1 carries BEARER state only; future feature-gated
/// mechanisms (GSSAPI, OAUTHBEARER, TLS client cert) extend this enum.
///
/// Wire form is internally-tagged on `kind`:
///
/// ```toml
/// [auth]
/// kind = "bearer"
/// tokens = ["…"]
/// ```
///
/// `Debug` is manually implemented so token bytes never appear in
/// logs/diagnostics (codex review important #5).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthConfig {
    /// RFC 7628 BEARER token allowlist. Empty `tokens` means every
    /// BEARER attempt is rejected — the safe default for a daemon with
    /// no operator-supplied auth config.
    Bearer {
        /// Tokens accepted by the BEARER handler. Stored in plaintext
        /// in the config file — operators are expected to keep the
        /// file readable only by the daemon's user.
        tokens: Vec<String>,
    },
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self::Bearer { tokens: Vec::new() }
    }
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthConfig::Bearer { tokens } => f
                .debug_struct("Bearer")
                .field("tokens", &RedactedTokenList(tokens.len()))
                .finish(),
        }
    }
}

struct RedactedTokenList(usize);

impl fmt::Debug for RedactedTokenList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED; {} entries]", self.0)
    }
}

impl Config {
    /// Layered TOML load. Each path is loaded if present; entries
    /// from later paths overlay earlier ones via deep-merge on the
    /// underlying [`toml::Value`] tree (so a later file's partial
    /// `[compaction]` override doesn't blow away the earlier file's
    /// `[providers]`).
    ///
    /// Missing files: silent skip (the common case for fresh installs).
    /// Malformed TOML: [`tracing::warn`] then skip that file. Never
    /// panics; never returns Err. Worst case is "all files failed to
    /// parse" → equivalent to [`Config::default`].
    pub fn load<P: AsRef<Path>>(paths: &[P]) -> Self {
        let mut merged: toml::Value = toml::Value::Table(Default::default());
        for p in paths {
            let path = p.as_ref();
            match std::fs::read_to_string(path) {
                Ok(content) => match content.parse::<toml::Value>() {
                    Ok(v) => deep_merge(&mut merged, v),
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "ignoring malformed mu config; falling back to defaults",
                        );
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Missing files are normal — operators may have
                    // only site config, only operator config, or
                    // neither. No warning.
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "could not read mu config; falling back to defaults",
                    );
                }
            }
        }
        match merged.try_into::<Config>() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "merged mu config failed deserialization; falling back to all-defaults",
                );
                Config::default()
            }
        }
    }

    /// Standard load order:
    ///
    /// 1. `/etc/mu/config.toml` (site)
    /// 2. `$XDG_CONFIG_HOME/mu/config.toml` or `~/.config/mu/config.toml` (operator)
    ///
    /// Use this from production entry points (`mu serve`, `mu-tui`).
    /// Tests should call [`Config::load`] with explicit paths to
    /// avoid pollution from the developer's real config.
    pub fn load_default() -> Self {
        let mut paths: Vec<PathBuf> = vec![PathBuf::from("/etc/mu/config.toml")];
        if let Some(dir) = dirs::config_dir() {
            paths.push(dir.join("mu").join("config.toml"));
        }
        Self::load(&paths)
    }
}

/// Recursive deep-merge of TOML values. Tables merge field-by-field
/// (recursively); scalars and arrays in `overlay` replace those in
/// `base`. Used by [`Config::load`] to layer site over operator over
/// CLI without losing fields that aren't redefined.
///
/// Arrays are replace-not-append on purpose: the typical case is the
/// judge `ranking` list, where an operator-edited file SHOULD wholesale
/// replace the site default rather than appending unwanted entries.
/// If a future use case needs per-array semantics, the call site can
/// post-process after `Config::load`.
fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                if let Some(existing) = b.get_mut(&k) {
                    deep_merge(existing, v);
                } else {
                    b.insert(k, v);
                }
            }
        }
        // Scalars and arrays in `overlay` replace those in `base`.
        // Arrays specifically: see [`load_array_replace_not_append`]
        // for why replace-not-append is the chosen semantic.
        (b, o) => {
            *b = o;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_pre_l1z_behavior() {
        let c = Config::default();
        assert_eq!(c.compaction.default_policy, "no-compaction");
        assert_eq!(
            c.compaction.trigger_threshold_tokens,
            crate::agent::DEFAULT_COMPACTION_THRESHOLD
        );
        assert!(c.session.persist_events_to_disk);
        assert!(!c.session.resume_on_daemon_restart);
        assert_eq!(c.session.state_dir, None);
        assert_eq!(c.ui.tui.default_provider, "anthropic_api");
        assert_eq!(c.ui.tui.default_model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn empty_toml_parses_to_default() {
        let c: Config = toml::from_str("").expect("empty TOML must parse");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn partial_toml_fills_in_defaults() {
        let toml_str = r#"
            [ui.tui]
            default_provider = "openrouter"
            default_model = "deepseek-chat"
        "#;
        let c: Config = toml::from_str(toml_str).expect("parse");
        // Specified fields use the TOML value.
        assert_eq!(c.ui.tui.default_provider, "openrouter");
        assert_eq!(c.ui.tui.default_model, "deepseek-chat");
        // Untouched fields stay at code defaults.
        assert_eq!(c.compaction.default_policy, "no-compaction");
    }

    #[test]
    fn round_trip_through_toml() {
        let mut c = Config::default();
        c.compaction.default_policy = "hash-and-summary".into();
        c.compaction.trigger_threshold_tokens = 100_000;
        c.compaction.judge.ranking = vec![
            JudgeRankingEntry {
                provider: "openrouter".into(),
                model: "deepseek-chat".into(),
                auth: "api_key".into(),
            },
            JudgeRankingEntry {
                provider: "anthropic".into(),
                model: "claude-haiku-4-5".into(),
                auth: "oauth".into(),
            },
        ];
        c.session.state_dir = Some(PathBuf::from("/tmp/mu-state"));
        c.providers.proxy.url = Some("http://127.0.0.1:3180".into());
        c.providers.proxy.use_for = vec!["anthropic".into()];

        let s = toml::to_string(&c).expect("serialize");
        let parsed: Config = toml::from_str(&s).expect("deserialize");
        assert_eq!(parsed, c);
    }

    #[test]
    fn load_missing_paths_returns_defaults_silently() {
        let nonexistent = std::env::temp_dir().join("definitely-not-here-mu-l1z.toml");
        let c = Config::load(&[&nonexistent]);
        assert_eq!(c, Config::default());
    }

    #[test]
    fn load_malformed_toml_warns_and_returns_defaults() {
        let dir = std::env::temp_dir().join(format!("mu-l1z-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not valid toml ][[[").unwrap();
        let c = Config::load(&[&path]);
        assert_eq!(c, Config::default());
        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_layers_overlay_in_order() {
        let dir = std::env::temp_dir().join(format!("mu-l1z-layered-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Layer 1 (site): sets compaction.default_policy + ui defaults.
        let site = dir.join("site.toml");
        std::fs::write(
            &site,
            r#"
[compaction]
default_policy = "heuristic"

[ui.tui]
default_provider = "openrouter"
default_model = "site/model"
"#,
        )
        .unwrap();

        // Layer 2 (operator): overrides compaction.default_policy +
        // ui.tui.default_model, leaves provider alone.
        let operator = dir.join("operator.toml");
        std::fs::write(
            &operator,
            r#"
[compaction]
default_policy = "hash-and-summary"

[ui.tui]
default_model = "operator/model"
"#,
        )
        .unwrap();

        let c = Config::load(&[&site, &operator]);
        // Operator wins where it specifies.
        assert_eq!(c.compaction.default_policy, "hash-and-summary");
        assert_eq!(c.ui.tui.default_model, "operator/model");
        // Site wins for fields operator didn't redefine.
        assert_eq!(c.ui.tui.default_provider, "openrouter");
        // Untouched stays at code default.
        assert_eq!(
            c.compaction.trigger_threshold_tokens,
            crate::agent::DEFAULT_COMPACTION_THRESHOLD
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_example_config_loads_cleanly() {
        // specs/example-config.toml is the operator-facing reference;
        // it must always parse cleanly through the current schema.
        // A test prevents the example drifting from the code (e.g.,
        // a field rename without an example update would break this).
        let example_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("specs")
            .join("example-config.toml");
        assert!(
            example_path.exists(),
            "expected shipped example config at {}",
            example_path.display()
        );
        let c = Config::load(&[&example_path]);
        // Spot-check that the example's fields landed on the struct.
        assert_eq!(c.compaction.default_policy, "no-compaction");
        assert_eq!(c.compaction.judge.output_mode, "index_keep");
        assert_eq!(c.compaction.judge.ranking.len(), 3);
        assert_eq!(c.compaction.judge.ranking[0].provider, "openrouter");
        assert_eq!(
            c.providers.anthropic_auth_ranking,
            vec!["oauth".to_string(), "api_key".to_string()]
        );
        assert!(c.session.persist_events_to_disk);
        assert_eq!(c.ui.tui.default_provider, "anthropic_api");
    }

    #[test]
    fn auth_config_debug_redacts_tokens() {
        let cfg = AuthConfig::Bearer {
            tokens: vec!["super-secret-token".to_string(), "another".to_string()],
        };
        let s = format!("{cfg:?}");
        assert!(
            !s.contains("super-secret-token"),
            "token leaked in Debug output: {s}",
        );
        assert!(!s.contains("another"), "token leaked in Debug output: {s}",);
        assert!(s.contains("REDACTED"), "expected REDACTED marker: {s}");
        assert!(s.contains("2 entries"), "expected entry count: {s}");
    }

    #[test]
    fn load_array_replace_not_append() {
        // The judge.ranking array should be REPLACED by a later
        // layer, not concatenated. Operators editing their config
        // expect to wholesale redefine; concatenation would surprise.
        let dir = std::env::temp_dir().join(format!("mu-l1z-arrays-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let site = dir.join("site.toml");
        std::fs::write(
            &site,
            r#"
[[compaction.judge.ranking]]
provider = "anthropic"
model = "claude-haiku-4-5"
auth = "oauth"

[[compaction.judge.ranking]]
provider = "anthropic"
model = "claude-haiku-4-5"
auth = "api_key"
"#,
        )
        .unwrap();

        let operator = dir.join("operator.toml");
        std::fs::write(
            &operator,
            r#"
[[compaction.judge.ranking]]
provider = "openrouter"
model = "deepseek-chat"
auth = "api_key"
"#,
        )
        .unwrap();

        let c = Config::load(&[&site, &operator]);
        // Operator's single entry wholesale replaced site's two.
        assert_eq!(c.compaction.judge.ranking.len(), 1);
        assert_eq!(c.compaction.judge.ranking[0].provider, "openrouter");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
