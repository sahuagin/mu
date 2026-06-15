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
    #[serde(default)]
    pub autonomy: AutonomyConfig,
}

/// mu-7e21: autonomy grant for the solo session, forwarded as
/// `CreateSessionRequest.autonomy`. Default DISABLED — INV-1's
/// opt-in posture reaches all the way to the operator's config file.
/// When enabled, the session's tool list gains `start_autonomous`
/// (and `schedule_wakeup` if allowed), so the model can accept a goal
/// in-band; the bounds below are enforced by the DAEMON at every
/// iteration boundary, never by the model (INV-2).
///
/// ```toml
/// [autonomy]
/// enabled = true
/// max_iterations = 25
/// max_wall_clock_ms = 3600000          # 1h, sleeping included (INV-5)
/// max_total_tool_calls_in_autonomy = 500
/// allow_schedule_wakeup = true
/// allow_delegate_grader = false
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AutonomyConfig {
    /// Master switch. false ⇒ no grant is sent; the session is
    /// created with the INV-1 default (autonomy disallowed) and the
    /// autonomy tools never appear in its tool list.
    pub enabled: bool,
    /// Iteration ceiling for an autonomous run.
    pub max_iterations: u32,
    /// Wall-clock ceiling (ms), sleeping included (INV-5).
    pub max_wall_clock_ms: u64,
    /// Total tool-call ceiling across the autonomous run.
    pub max_total_tool_calls_in_autonomy: u32,
    /// Whether the session may park itself via `schedule_wakeup`.
    pub allow_schedule_wakeup: bool,
    /// Whether the session may use the DelegateGrader goal-check
    /// (spawns/asks a sibling session to grade — non-trivial cost).
    pub allow_delegate_grader: bool,
}

impl Default for AutonomyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_iterations: 25,
            max_wall_clock_ms: 3_600_000,
            max_total_tool_calls_in_autonomy: 500,
            allow_schedule_wakeup: true,
            allow_delegate_grader: false,
        }
    }
}

impl AutonomyConfig {
    /// The wire-shaped grant: None when disabled (field omitted from
    /// create_session entirely — older daemons never see it).
    pub fn to_capability(&self) -> Option<mu_core::capability::AutonomyCapability> {
        if !self.enabled {
            return None;
        }
        Some(mu_core::capability::AutonomyCapability::Allowed {
            max_iterations: self.max_iterations,
            max_wall_clock_ms: self.max_wall_clock_ms,
            max_total_tool_calls_in_autonomy: self.max_total_tool_calls_in_autonomy,
            allow_schedule_wakeup: self.allow_schedule_wakeup,
            allow_delegate_grader: self.allow_delegate_grader,
        })
    }
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
    /// Optional clipboard command fallback for `/copy`, as argv (no shell).
    /// The selected text is written to stdin after the native clipboard
    /// library path fails. Example: `["xclip", "-selection", "clipboard"]`.
    pub clipboard_command: Option<Vec<String>>,
    /// Renderer journal — one JSONL line per scrollback commit written
    /// to `~/.local/share/mu/solo/renderer.jsonl`.  Projection telemetry
    /// only; NEVER written to the semantic event store
    /// (`~/.local/share/mu/events/`).  Default: true (on).
    /// Set to `false` in `[tui]` to disable.
    pub renderer_journal: bool,
    /// Desktop notifications via terminal escape (OSC 99) when a turn
    /// completes or errors while the terminal is unfocused — the
    /// enclosing terminal (kitty/wezterm/iTerm2) raises the popup.
    /// Default: true (on). Set to `false` in `[tui]` to disable.
    pub notifications: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            effort: "medium".into(),
            focus_mode: false,
            clipboard_command: None,
            renderer_journal: true,
            notifications: true,
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
    /// mu-f1a0: prompt-cache TTL tier ("5m" | "1h") forwarded in
    /// create_session. Solo defaults to "1h": interactive sessions
    /// are gap-heavy (74% of the measured baseline's cache writes
    /// were >5min-gap expiry re-writes; 1h would have cut that
    /// session ~20%). Set "5m" for batch-shaped usage. Only the
    /// Anthropic provider consumes it today.
    pub cache_ttl: String,
    /// mu-upk2: extended-thinking directive, forwarded as `mu serve
    /// --thinking <v>`. Empty string (the DEFAULT) = no directive (off).
    /// Accepts effort levels (`minimal`|`low`|`medium`|`high`), a raw token
    /// budget, `adaptive`, or `disabled`. Only the Anthropic provider acts on
    /// it (natively-reasoning ollama models think regardless). Set once in
    /// solo.toml (`[session] thinking = "high"`) — no flag needed each run.
    pub thinking: String,
    /// mu-n25a: the session's side-effects CEILING — the permission
    /// posture an operator's "read only" binds to, forwarded as
    /// `CreateSessionRequest.max_side_effects`. A tool whose declared
    /// side-effects exceed this ceiling is refused at the dispatch
    /// boundary, regardless of its `permission` level — closing the
    /// gap where `write`/`edit`/`watch` (permission: Allow) sailed
    /// through despite an intended read-only session.
    ///
    /// Valid values (ascending danger):
    ///   `read_only` < `mutating` < `external` < `destructive` < `execute`.
    /// Empty string `""` (the DEFAULT) = unrestricted — no ceiling is
    /// sent, the session behaves exactly as before (back-compat). Set
    /// `max_side_effects = "read_only"` for a read-only operator session.
    /// An unrecognized value is a hard config error (so a typo doesn't
    /// silently fall through to unrestricted).
    pub max_side_effects: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            provider: "openai-codex".into(),
            model: "gpt-5.5".into(),
            // mu-oee9: memory_recall is default-on — the small-kernel
            // injection (mu-zk2i) demotes everything but the identity
            // kernel to recall-only; without this tool that tail is
            // unreachable mid-session.
            tools: "read,write,edit,glob,grep,memory_recall,bash".into(),
            bash_yolo: false,
            mu_binary: "./target/release/mu".into(),
            cwd: None,
            cache_ttl: "1h".into(),
            // mu-upk2: thinking off by default ("" = no directive; opt-in).
            thinking: String::new(),
            // mu-n25a: unrestricted by default — opt-in posture, so
            // existing solo configs are unaffected.
            max_side_effects: String::new(),
        }
    }
}

impl SessionConfig {
    /// mu-n25a: parse the configured `max_side_effects` string into the
    /// wire-shaped ceiling. `Ok(None)` = unrestricted (empty string ⇒
    /// the field is omitted from create_session entirely, so an older
    /// daemon never sees it). `Err` on an unrecognized value so a typo
    /// surfaces instead of silently degrading to unrestricted.
    pub fn max_side_effects_capability(&self) -> Result<Option<mu_core::agent::tool::SideEffects>> {
        use mu_core::agent::tool::SideEffects;
        let v = self.max_side_effects.trim();
        if v.is_empty() {
            return Ok(None);
        }
        let parsed = match v {
            "read_only" => SideEffects::ReadOnly,
            "mutating" => SideEffects::Mutating,
            "external" => SideEffects::External,
            "destructive" => SideEffects::Destructive,
            "execute" => SideEffects::Execute,
            other => {
                anyhow::bail!(
                    "invalid [session] max_side_effects {other:?} \
                     (valid: read_only|mutating|external|destructive|execute, \
                     or empty for unrestricted)"
                )
            }
        };
        Ok(Some(parsed))
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
    pub thinking: Option<String>,
    pub cwd: Option<PathBuf>,
    pub effort: Option<String>,
    pub focus_mode: Option<bool>,
    pub clipboard_command: Option<Vec<String>>,
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
    if let Some(v) = &cli.thinking {
        config.session.thinking = v.clone();
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
    if let Some(v) = &cli.clipboard_command {
        config.tui.clipboard_command = Some(v.clone());
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

    // mu-7e21: the grant is None unless explicitly enabled — INV-1's
    // opt-in posture reaches the config layer.
    #[test]
    fn autonomy_disabled_by_default_sends_no_grant() {
        let c = SoloConfig::default();
        assert!(!c.autonomy.enabled);
        assert_eq!(c.autonomy.to_capability(), None);
    }

    // ── mu-n25a: max_side_effects ───────────────────────────────

    #[test]
    fn max_side_effects_unrestricted_by_default() {
        let c = SoloConfig::default();
        assert_eq!(c.session.max_side_effects, "");
        assert_eq!(
            c.session.max_side_effects_capability().expect("ok"),
            None,
            "empty config → no ceiling sent (back-compat)"
        );
    }

    #[test]
    fn max_side_effects_read_only_maps_to_capability() {
        let toml = r#"
            [session]
            max_side_effects = "read_only"
        "#;
        let c: SoloConfig = toml::from_str(toml).expect("parse");
        assert_eq!(
            c.session.max_side_effects_capability().expect("ok"),
            Some(mu_core::agent::tool::SideEffects::ReadOnly)
        );
    }

    #[test]
    fn max_side_effects_all_levels_round_trip() {
        use mu_core::agent::tool::SideEffects;
        for (s, want) in [
            ("read_only", SideEffects::ReadOnly),
            ("mutating", SideEffects::Mutating),
            ("external", SideEffects::External),
            ("destructive", SideEffects::Destructive),
            ("execute", SideEffects::Execute),
        ] {
            let cfg = SessionConfig {
                max_side_effects: s.to_string(),
                ..Default::default()
            };
            assert_eq!(
                cfg.max_side_effects_capability().expect("ok"),
                Some(want),
                "value {s:?} should map to {want:?}"
            );
        }
    }

    #[test]
    fn max_side_effects_invalid_value_is_an_error() {
        let cfg = SessionConfig {
            max_side_effects: "readonly".to_string(), // typo: missing underscore
            ..Default::default()
        };
        let err = cfg
            .max_side_effects_capability()
            .expect_err("typo must be a hard error, not a silent unrestricted");
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn solo_config_round_trips_with_max_side_effects() {
        let mut c = SoloConfig::default();
        c.session.max_side_effects = "read_only".into();
        let s = toml::to_string(&c).expect("serialize");
        let c2: SoloConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(c, c2);
        assert_eq!(c2.session.max_side_effects, "read_only");
    }

    #[test]
    fn autonomy_enabled_maps_bounds_to_capability() {
        let toml = r#"
            [autonomy]
            enabled = true
            max_iterations = 7
            allow_schedule_wakeup = false
        "#;
        let c: SoloConfig = toml::from_str(toml).expect("parse");
        match c.autonomy.to_capability() {
            Some(mu_core::capability::AutonomyCapability::Allowed {
                max_iterations,
                allow_schedule_wakeup,
                max_total_tool_calls_in_autonomy,
                ..
            }) => {
                assert_eq!(max_iterations, 7);
                assert!(!allow_schedule_wakeup);
                // Unspecified bounds inherit the config defaults.
                assert_eq!(max_total_tool_calls_in_autonomy, 500);
            }
            other => panic!("expected Allowed, got {other:?}"),
        }
    }

    #[test]
    fn cli_overrides_replace_some_fields() {
        let mut c = SoloConfig::default();
        assert_eq!(c.session.thinking, "", "thinking defaults off");
        let cli = CliOverrides {
            provider: Some("anthropic".into()),
            bash_yolo: Some(true),
            thinking: Some("high".into()),
            ..Default::default()
        };
        apply_cli_overrides(&mut c, &cli);
        assert_eq!(c.session.provider, "anthropic");
        assert!(c.session.bash_yolo);
        assert_eq!(c.session.thinking, "high");
        // Untouched fields keep their defaults.
        assert_eq!(c.session.model, "gpt-5.5");
    }

    #[test]
    fn thinking_round_trips_through_session_toml() {
        let c: SoloConfig = toml::from_str("[session]\nthinking = \"medium\"\n").expect("parse");
        assert_eq!(c.session.thinking, "medium");
        // Absent ⇒ off.
        let d: SoloConfig = toml::from_str("[session]\nmodel = \"x\"\n").expect("parse");
        assert_eq!(d.session.thinking, "");
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
