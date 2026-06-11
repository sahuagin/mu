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
    /// `[recall]` — session-start context injection toggle.
    pub recall: RecallConfig,
    /// `[index]` — in-loop discovery/recall knobs.
    pub index: IndexConfig,
    /// `[mcp]` — outbound MCP client: servers whose tools the daemon
    /// imports at startup (mu-yc6).
    pub mcp: McpConfig,
    /// `[journal]` — command-journal durability + location (spec mu-046).
    pub journal: JournalConfig,
    /// `[routes]` — provider route-catalog discovery knobs.
    pub routes: RoutesConfig,
}

/// `[routes]` section. Startup route-catalog discovery (mu-818c).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutesConfig {
    /// Probe the ollama box at daemon startup to populate the route
    /// catalog. Default `true` (production daemons with an events dir).
    /// Hermetic tests MUST set `false`: the baked-in ollama base is a
    /// private LAN address, unroutable on CI runners, so the probe's
    /// bounded connect timeout stalls boot for its full duration there
    /// (observed as pipeline_smoke response timeouts on GitHub CI).
    pub ollama_discover: bool,
}

impl Default for RoutesConfig {
    fn default() -> Self {
        Self {
            ollama_discover: true,
        }
    }
}

/// `[index]` section. Knobs for the in-loop discovery surface. (Code-index
/// recall itself is imported over MCP — see [`McpConfig`].)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    /// mu-kex4.6.3: rank the in-loop `discover` tool's results semantically
    /// (t4c `SemanticRanker` over an embedder) instead of the `LexicalRanker`
    /// floor. Opt-in (`false` default) because semantic ranking resolves an
    /// embedder (`ConfigEmbedder::from_config` — network/paid by default,
    /// pointable at a local Ollama via `T4C_EMBED_ENDPOINT`); `discover` is a
    /// rare orientation action, so the per-call embed cost is acceptable when
    /// enabled. On any embedder failure the tool falls back to the lexical
    /// floor, so enabling this never breaks discovery. Default `false` keeps
    /// prior behavior and keeps tests offline.
    pub semantic_discover: bool,
}

/// `[journal]` section (spec mu-046). Controls the daemon
/// control-plane command journal
/// ([`crate::command_journal::CommandJournal`]) and the session logs'
/// strict command appends.
///
/// ```toml
/// [journal]
/// fsync = "always"          # "always" | "never" — default always
/// journal_queries = true    # read-only queries are journaled too
/// # dir = "..."             # override location (tests/ephemeral daemons)
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JournalConfig {
    /// `"always"` (default) — `sync_data()` after every command
    /// append, before the command is processed (spec mu-046 INV-1).
    /// `"never"` skips the fsync (tests / ephemeral daemons).
    /// Unrecognized values resolve to `Always` — fail durable, not
    /// fast (see [`JournalConfig::fsync_policy`]).
    pub fsync: String,
    /// Journal read-only queries (`session.list`, `daemon.stats`, …)
    /// too, not just mutating commands. Default `true` — the locked
    /// decision: the audit trail records what was ASKED, not only what
    /// changed. `false` (test/ephemeral daemons) makes the pipeline's
    /// recognized read-only query methods skip the journal entirely —
    /// no `CommandReceived`, no receipt, no fsync on the hot read path
    /// — while still flowing through the same ingest seam, auth gate,
    /// and single-writer consumer (the border doesn't open; it just
    /// stops writing for reads). Mutating commands always journal
    /// regardless of this knob. The query set is the per-method
    /// predicate in `serve/pipeline.rs` (`is_query`).
    #[serde(default = "default_true")]
    pub journal_queries: bool,
    /// Override the journal directory. `None` resolves to the
    /// platform default at runtime (`<state_dir>/journal/`, a sibling
    /// of `events/` so the session-log scanners never see it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            fsync: "always".to_string(),
            journal_queries: true,
            dir: None,
        }
    }
}

impl JournalConfig {
    /// Resolve the `fsync` string to a typed policy. Only an explicit
    /// `"never"` opts out of durability; anything else — including a
    /// typo — is `Always`, because the failure mode of a misread knob
    /// must be "slower" rather than "commands lost on crash".
    pub fn fsync_policy(&self) -> crate::command_journal::FsyncPolicy {
        if self.fsync.trim().eq_ignore_ascii_case("never") {
            crate::command_journal::FsyncPolicy::Never
        } else {
            crate::command_journal::FsyncPolicy::Always
        }
    }
}

/// `[mcp]` section (mu-yc6). Outbound MCP client: at startup the daemon
/// connects to each `[[mcp.servers]]` entry over rmcp Streamable HTTP, lists
/// its tools, and imports them as in-loop tools alongside the built-ins. Same
/// graceful-degradation posture as the rest of this file: an unreachable
/// server logs a warning and contributes nothing — never a startup failure.
///
/// ```toml
/// [[mcp.servers]]
/// name  = "code-index"
/// url   = "http://10.1.1.172:7622/mcp"
/// tools = ["code_recall", "code_status"]  # optional allowlist; omit = all
/// side_effects = "read_only"              # operator trust floor for this server
/// tool_side_effects = { code_recall = "read_only" }  # per-tool override
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Servers to connect to at daemon startup.
    pub servers: Vec<McpServerConfig>,
}

/// One outbound MCP server (`[[mcp.servers]]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Label used in logs and (optionally) tool-name prefixes.
    pub name: String,
    /// Streamable HTTP endpoint, e.g. `"http://10.1.1.172:7622/mcp"`.
    /// The only transport in v1; stdio / unix-socket servers are future work.
    pub url: String,
    /// Import allowlist of remote tool names. `None` imports every tool the
    /// server lists. Discovery != trust: prefer listing daily drivers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Prefix prepended to imported tool names (e.g. `"code_index."`).
    /// `None`/empty imports unprefixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// mu-cvm5 (mu-n25a Phase 4): operator-supplied side-effects
    /// classification for tools imported from this server. MCP carries no
    /// side-effects metadata, so there is NO honest source the runtime can
    /// trust — a remote `delete_everything` tool would otherwise import as
    /// benign `ReadOnly` and free-ride past a restrictive session posture.
    ///
    /// `None` (the default) means UNCLASSIFIED: imported tools fail SAFE and
    /// are treated as the most dangerous class (`Execute`), so a read-only
    /// session refuses them at the dispatch gate. An operator who has
    /// vetted a server declares its trust floor here (e.g. `"read_only"` for
    /// a code-search server) — a deliberate, auditable act, never something
    /// a remote server gets to assert about itself.
    ///
    /// Wire form is flat snake_case (`side_effects = "read_only"`), matching
    /// the `SideEffects` serde elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effects: Option<crate::agent::tool::SideEffects>,
    /// mu-cvm5: per-tool side-effects override, keyed by the tool's
    /// REMOTE name (pre-prefix, as the server lists it). Takes precedence
    /// over the server-wide `side_effects` for the named tool. Lets an
    /// operator trust most of a server at one floor while pinning a few
    /// tools higher/lower (e.g. server `read_only`, but `run_query =
    /// "external"`). Unlisted tools fall back to `side_effects`, then to
    /// the fail-safe `Execute` default.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub tool_side_effects: std::collections::HashMap<String, crate::agent::tool::SideEffects>,
}

/// `[recall]` section. Controls session-start context injection — the
/// `SubprocessRecallProvider` (agent memory) + `ProjectFileRecallProvider`
/// (CLAUDE.md / AGENTS.md hierarchy) that front-load context at session start.
///
/// `enabled = false` (or the `MU_NO_RECALL` env override) turns OFF all
/// front-loaded recall, so the agent discovers context on demand instead — the
/// leaner "discover-on-demand" posture (mu's analog of claude-basic's
/// `CLAUDE_BASIC_MEM` dial). Default `true` preserves existing behavior; it also
/// keeps work context out of unrelated sessions, since nothing is pulled until a
/// task asks for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecallConfig {
    /// Run the session-start recall providers. `false` => no front-load.
    pub enabled: bool,
    /// mu-mu-bare-flag-fxc8: hermetic-session mode. `bare = true` goes
    /// one step beyond `enabled = false`: recall is off AND the
    /// discovery bootstrap is suppressed, so a session with no operator
    /// system prompt gets NO system prompt at all. For gate scripts,
    /// benches, and delegated workers that must not inherit operator
    /// identity or any injected default. Set via `mu ask/serve --bare`
    /// (which also forces `enabled = false`); settable in config for
    /// completeness.
    pub bare: bool,
    /// mu-zk2i: which memory-injection tier the agent-memory provider
    /// requests (`agent memory context --tier <this>`). `"identity"`
    /// (default) injects the small kernel — user-first identity rows +
    /// identity-tagged rules, ~1K tokens — with everything else
    /// reachable via the `memory_recall` tool (mu-oee9). `"full"`
    /// restores the pre-mu-zk2i four-section wall (measured 15.9K
    /// tokens = 21% of post-compaction context, session c76f6949).
    pub tier: String,
    /// mu-recall-bootloader-flag-nxpo: emit a first-position startup-
    /// orientation preamble as the FIRST recall segment, ahead of the memory
    /// and project-file providers. `false` (default) => byte-identical
    /// context to today (the experiment's A condition). Toggled per-session
    /// without editing config via the `MU_RECALL_BOOTLOADER=0/1` env override
    /// — the A/B knob for experiment bootloader-startup-ab-2026-06-06. Gated
    /// by [`Config::recall_enabled`] at the call site, so `--bare` and
    /// `MU_NO_RECALL` suppress it too. See [`DEFAULT_BOOTLOADER_TEXT`].
    pub bootloader: bool,
    /// Override text for the bootloader preamble. `None` (default) =>
    /// [`DEFAULT_BOOTLOADER_TEXT`], the operator-approved v1 wording. Only
    /// consulted when `bootloader` (or the env override) is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootloader_text: Option<String>,
    /// mu-recall-operator-controls-5y6a: toggle for the agent-memory
    /// provider (`SubprocessRecallProvider`) INDEPENDENTLY of the
    /// project-file provider. `true` (default) preserves today's behavior
    /// (inject the `agent memory context` kernel). Set `false` to run with
    /// ONLY the file-based project context (MU.md / AGENTS.md) and no
    /// agent.sqlite-backed injection — without disabling all recall
    /// (`enabled = false`), which would also drop the project files.
    #[serde(default = "default_true")]
    pub memory: bool,
    /// mu-recall-operator-controls-5y6a: path to the memory CLI the
    /// agent-memory provider invokes (`<this> memory context --tier <tier>`).
    /// `None` (default) => the built-in default `~/.local/bin/agent`
    /// (`SubprocessRecallProvider::default`). Set this to point mu at a
    /// different binary rather than relying on the hardcoded operator path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_binary: Option<PathBuf>,
}

/// serde default helper: `true`. A bare `#[serde(default)]` on a `bool`
/// yields `false`, which would silently flip the memory provider OFF for
/// any config that omits the key — so `memory` needs an explicit `true`
/// default.
fn default_true() -> bool {
    true
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bare: false,
            tier: "identity".to_string(),
            bootloader: false,
            bootloader_text: None,
            memory: true,
            memory_binary: None,
        }
    }
}

/// Operator-approved v1 bootloader preamble (bead
/// mu-recall-bootloader-flag-nxpo; experiment bootloader-startup-ab-2026-06-06).
/// Emitted verbatim as the first recall segment when `[recall].bootloader`
/// (or `MU_RECALL_BOOTLOADER=1`) is on and recall is enabled. Used when
/// `[recall].bootloader_text` is unset. Do NOT reword without operator
/// sign-off — the experiment's calibration depends on the exact wording.
pub const DEFAULT_BOOTLOADER_TEXT: &str = "## Bootloader\nStartup orientation, not the current task. Load it silently — do not answer or rehearse it. The next user message is the prompt. This dossier is a continuity artifact from prior sessions: testimony, not memory. Verify before relying on it; don't claim lived recollection.";

/// `[compaction]` section. Maps to the agent loop's threshold-cross
/// policy dispatch (mu-kgu.4). v1 supplies the threshold; mu-kgu.11
/// will consume `judge.ranking` to wire a live judge model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    /// Identifier of the policy to instantiate at session creation.
    ///
    /// - `"heuristic"` (default) — [`crate::context::compaction::heuristic::SpanFamilyDropPolicy`];
    ///   drops low-retention spans when the threshold is crossed.  Best choice when no
    ///   judge model is configured.
    /// - `"hash-and-summary"` — [`crate::context::compaction::hash_summary::HashAndSummaryPolicy`];
    ///   calls a configured judge model (see [`CompactionJudgeConfig`]) to produce a
    ///   content-aware keep-list and summary span.  Falls back to the bench canned judge
    ///   (`KeepHalfJudge`) when `[compaction.judge].ranking` is empty, so no model spend
    ///   is required if you omit judge config.
    /// - `"no-compaction"` — explicit identity (pre-mu-kgu behavior); context is never
    ///   compacted regardless of threshold.
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
            // mu-8bkf operator decision 2026-06-05: "default-and-warn beats
            // nothing-and-warn".  The previous "no-compaction" default meant
            // that a threshold set but no policy configured silently did nothing.
            // "heuristic" is always constructible (no judge needed) and is the
            // right safe-default for operators who set trigger_threshold_tokens
            // without reading the full compaction docs.  Explicit "no-compaction"
            // remains selectable.
            default_policy: "heuristic".to_string(),
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

/// Keys whose values are secret-shaped, matched case-insensitively at
/// every depth of the config tree by [`redact_config`]. `auth.tokens`
/// is the canonical entry. This denylist MUST grow with every new
/// secret-shaped config field — a field added without an entry here
/// would land in the journal in plaintext (spec mu-046 INV-6), so
/// review it whenever a struct in this file gains a credential.
const SECRET_KEY_DENYLIST: &[&str] = &[
    "token", "tokens", "secret", "secrets", "api_key", "api_keys", "password",
];

/// spec mu-046 INV-6: secrets never hit a journal. Strips
/// secret-shaped fields — any key on [`SECRET_KEY_DENYLIST`], at any
/// depth — from a JSON projection of the config, replacing values
/// with `"[REDACTED]"` so the shape stays legible while the bytes are
/// gone. In-place because the caller has already paid for the
/// serialization (`ConfigLoaded` builds the `serde_json::Value`
/// anyway) and a mutate-then-append flow can't accidentally journal
/// the unredacted original.
pub fn redact_config(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if SECRET_KEY_DENYLIST
                    .iter()
                    .any(|d| key.eq_ignore_ascii_case(d))
                {
                    *v = serde_json::Value::String("[REDACTED]".to_string());
                } else {
                    redact_config(v);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for v in items.iter_mut() {
                redact_config(v);
            }
        }
        _ => {}
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
        Self::load_with_sources(paths).0
    }

    /// [`Config::load`] plus source provenance (spec mu-046 WP6): the
    /// second element lists the layers that actually CONTRIBUTED to the
    /// resolved config — `"defaults"` first (always present), then the
    /// path of each file that was present AND parsed. Missing or
    /// malformed files are not listed: provenance records what was
    /// applied, not what was probed. Consumer-side overrides (env, CLI
    /// flags — resolution steps 4–5 in the module doc) append their own
    /// entries at their call sites (e.g. `"env:MU_BEARER_TOKEN"`,
    /// `"cli:--bare"` in `mu serve`'s boot path).
    ///
    /// The sources vec rides into the journal as
    /// `JournalPayload::ConfigLoaded { sources, .. }` (INV-9) — the
    /// durable answer to "where did this daemon's config come from".
    pub fn load_with_sources<P: AsRef<Path>>(paths: &[P]) -> (Self, Vec<String>) {
        let mut sources: Vec<String> = vec!["defaults".to_string()];
        let mut merged: toml::Value = toml::Value::Table(Default::default());
        for p in paths {
            let path = p.as_ref();
            match std::fs::read_to_string(path) {
                Ok(content) => match content.parse::<toml::Value>() {
                    Ok(v) => {
                        deep_merge(&mut merged, v);
                        sources.push(path.display().to_string());
                    }
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
            Ok(c) => (c, sources),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "merged mu config failed deserialization; falling back to all-defaults",
                );
                // The files did NOT contribute to the effective config
                // — provenance must say so.
                (Config::default(), vec!["defaults".to_string()])
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
        Self::load_default_with_sources().0
    }

    /// [`Config::load_default`] plus source provenance — see
    /// [`Config::load_with_sources`].
    pub fn load_default_with_sources() -> (Self, Vec<String>) {
        let mut paths: Vec<PathBuf> = vec![PathBuf::from("/etc/mu/config.toml")];
        if let Some(dir) = dirs::config_dir() {
            paths.push(dir.join("mu").join("config.toml"));
        }
        Self::load_with_sources(&paths)
    }

    /// Whether session-start recall injection should run: the `[recall].enabled`
    /// flag, unless the `MU_NO_RECALL` env var force-disables it. This is the
    /// single switch the serve loop consults before constructing the recall
    /// providers (mu's discover-on-demand dial).
    pub fn recall_enabled(&self) -> bool {
        self.recall.enabled
            && !Self::env_disables_recall(std::env::var("MU_NO_RECALL").ok().as_deref())
    }

    /// Pure parse of the `MU_NO_RECALL` override: a truthy value force-disables
    /// recall regardless of config. Split out so it's testable without touching
    /// process env.
    pub fn env_disables_recall(v: Option<&str>) -> bool {
        matches!(v.map(str::trim), Some("1" | "true" | "yes" | "on"))
    }

    /// Whether the first-position bootloader preamble should be emitted:
    /// the `[recall].bootloader` flag, with the `MU_RECALL_BOOTLOADER` env
    /// var overriding it in BOTH directions (`1` forces on, `0` forces off)
    /// so the experiment's A/B toggles per session without editing config.
    /// Callers still gate on [`recall_enabled`](Self::recall_enabled), so
    /// `--bare` / `MU_NO_RECALL` suppress the bootloader regardless of this.
    pub fn bootloader_enabled(&self) -> bool {
        Self::env_bootloader_override(std::env::var("MU_RECALL_BOOTLOADER").ok().as_deref())
            .unwrap_or(self.recall.bootloader)
    }

    /// Pure parse of the `MU_RECALL_BOOTLOADER` override. Unlike
    /// `MU_NO_RECALL` (disable-only), this is tri-state: a truthy value
    /// forces the bootloader ON, a falsy value forces it OFF, and an absent
    /// or unrecognized value (`None`) defers to config. Split out so it's
    /// testable without touching process env.
    pub fn env_bootloader_override(v: Option<&str>) -> Option<bool> {
        match v.map(str::trim) {
            Some("1" | "true" | "yes" | "on") => Some(true),
            Some("0" | "false" | "no" | "off") => Some(false),
            _ => None,
        }
    }

    /// The resolved bootloader preamble text: the operator-configured
    /// `[recall].bootloader_text` override, or [`DEFAULT_BOOTLOADER_TEXT`]
    /// when unset.
    pub fn bootloader_text(&self) -> &str {
        self.recall
            .bootloader_text
            .as_deref()
            .unwrap_or(DEFAULT_BOOTLOADER_TEXT)
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
    fn default_config_compaction_is_heuristic() {
        // mu-8bkf: default_policy flipped from "no-compaction" to "heuristic"
        // so a threshold-configured daemon runs real compaction out of the box.
        let c = Config::default();
        assert_eq!(c.compaction.default_policy, "heuristic");
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
    fn recall_defaults_on_and_toml_can_disable() {
        // default preserves existing always-inject behavior
        assert!(Config::default().recall.enabled);
        // operators opt into discover-on-demand via TOML
        let c: Config = toml::from_str("[recall]\nenabled = false\n").expect("parse");
        assert!(!c.recall.enabled);
    }

    #[test]
    fn recall_memory_defaults_on_and_toml_can_disable() {
        // mu-recall-operator-controls-5y6a: the agent-memory provider is on by
        // default (preserves today's injection); operators opt out via TOML
        // while keeping project-file recall.
        assert!(Config::default().recall.memory);
        assert_eq!(Config::default().recall.memory_binary, None);
        let c: Config = toml::from_str("[recall]\nmemory = false\n").expect("parse memory toggle");
        assert!(!c.recall.memory);
        // composes with the other axes untouched
        assert!(c.recall.enabled);
    }

    #[test]
    fn recall_memory_stays_on_when_section_present_but_key_omitted() {
        // The `default_true` guard, pinned: a [recall] section that sets some
        // OTHER key but omits `memory` must still default it ON. Without the
        // explicit #[serde(default = "default_true")], a partial section would
        // silently flip memory OFF (a bare bool serde-default is `false`) and
        // disable the agent-memory injection for that operator — green CI, wrong
        // behavior. This test is what a future "simplify to #[serde(default)]"
        // would have to break.
        let c: Config =
            toml::from_str("[recall]\nbootloader = true\n").expect("parse partial recall");
        assert!(
            c.recall.memory,
            "memory must default ON for a [recall] section that omits the key"
        );
    }

    #[test]
    fn recall_memory_binary_toml_sets_path() {
        let c: Config = toml::from_str("[recall]\nmemory_binary = \"/opt/agent\"\n")
            .expect("parse memory_binary");
        assert_eq!(
            c.recall.memory_binary,
            Some(std::path::PathBuf::from("/opt/agent"))
        );
        // default still on so the path is actually used
        assert!(c.recall.memory);
    }

    #[test]
    fn recall_tier_defaults_to_identity_and_toml_can_restore_full() {
        // mu-zk2i: the small kernel is the new default posture.
        assert_eq!(Config::default().recall.tier, "identity");
        let c: Config = toml::from_str("[recall]\ntier = \"full\"\n").expect("parse");
        assert_eq!(c.recall.tier, "full");
        // tier composes with enabled untouched
        assert!(c.recall.enabled);
    }

    #[test]
    fn index_semantic_discover_defaults_off_and_toml_can_set() {
        // mu-kex4.6.3: semantic discover is opt-in, default off (keeps tests offline).
        assert!(!Config::default().index.semantic_discover);
        let c: Config = toml::from_str("[index]\nsemantic_discover = true\n").expect("parse");
        assert!(c.index.semantic_discover);
    }

    #[test]
    fn mcp_servers_default_empty_and_toml_can_configure() {
        assert!(Config::default().mcp.servers.is_empty());
        let c: Config = toml::from_str(
            "[[mcp.servers]]\n\
             name = \"code-index\"\n\
             url = \"http://10.1.1.172:7622/mcp\"\n\
             tools = [\"code_recall\"]\n",
        )
        .expect("parse");
        assert_eq!(c.mcp.servers.len(), 1);
        let s = &c.mcp.servers[0];
        assert_eq!(s.name, "code-index");
        assert_eq!(s.url, "http://10.1.1.172:7622/mcp");
        assert_eq!(s.tools.as_deref(), Some(&["code_recall".to_owned()][..]));
        assert_eq!(s.prefix, None);
        // mu-cvm5: classification fields default to unset (fail-safe upstream).
        assert_eq!(s.side_effects, None);
        assert!(s.tool_side_effects.is_empty());
    }

    #[test]
    fn mcp_server_side_effects_classification_parses() {
        // mu-cvm5 (mu-n25a Phase 4): operator can declare a server-wide
        // side-effects floor and per-tool overrides via TOML.
        use crate::agent::tool::SideEffects;
        let c: Config = toml::from_str(
            "[[mcp.servers]]\n\
             name = \"code-index\"\n\
             url = \"http://10.1.1.172:7622/mcp\"\n\
             side_effects = \"read_only\"\n\
             tool_side_effects = { run_query = \"external\" }\n",
        )
        .expect("parse");
        let s = &c.mcp.servers[0];
        assert_eq!(s.side_effects, Some(SideEffects::ReadOnly));
        assert_eq!(
            s.tool_side_effects.get("run_query").copied(),
            Some(SideEffects::External)
        );
    }

    #[test]
    fn mu_no_recall_env_override_is_truthy_only() {
        // truthy values force-disable; everything else is a no-op
        for v in ["1", "true", "yes", "on", " true "] {
            assert!(Config::env_disables_recall(Some(v)), "{v:?} should disable");
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!Config::env_disables_recall(Some(v)), "{v:?} should not");
        }
        assert!(!Config::env_disables_recall(None));
    }

    #[test]
    fn mu_recall_bootloader_env_override_is_tristate() {
        // truthy forces on, falsy forces off, anything else defers (None)
        for v in ["1", "true", "yes", "on", " true "] {
            assert_eq!(
                Config::env_bootloader_override(Some(v)),
                Some(true),
                "{v:?} should force on"
            );
        }
        for v in ["0", "false", "no", "off", " 0 "] {
            assert_eq!(
                Config::env_bootloader_override(Some(v)),
                Some(false),
                "{v:?} should force off"
            );
        }
        for v in ["", "maybe", "2"] {
            assert_eq!(
                Config::env_bootloader_override(Some(v)),
                None,
                "{v:?} should defer to config"
            );
        }
        assert_eq!(Config::env_bootloader_override(None), None);
    }

    #[test]
    fn bootloader_defaults_off_with_canonical_text() {
        // The experiment's A condition: default config has the bootloader
        // OFF, so default sessions are byte-identical to today.
        let c = Config::default();
        assert!(!c.recall.bootloader);
        assert_eq!(c.recall.bootloader_text, None);
        // None override resolves to the operator-approved v1 wording.
        assert_eq!(c.bootloader_text(), DEFAULT_BOOTLOADER_TEXT);
        assert!(c.bootloader_text().starts_with("## Bootloader\n"));
    }

    #[test]
    fn bootloader_text_override_wins_over_default() {
        let mut c = Config::default();
        c.recall.bootloader_text = Some("custom preamble".to_string());
        assert_eq!(c.bootloader_text(), "custom preamble");
    }

    #[test]
    fn recall_section_parses_bootloader_fields() {
        let c: Config = toml::from_str(
            r#"
            [recall]
            enabled = true
            bootloader = true
            bootloader_text = "hi"
            "#,
        )
        .expect("parse");
        assert!(c.recall.bootloader);
        assert_eq!(c.recall.bootloader_text.as_deref(), Some("hi"));
        // Omitting the [recall] keys leaves the defaults (off / None).
        let d: Config = toml::from_str("[recall]\nenabled = true\n").expect("parse");
        assert!(!d.recall.bootloader);
        assert_eq!(d.recall.bootloader_text, None);
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
        // Untouched fields stay at code defaults (mu-8bkf: now "heuristic").
        assert_eq!(c.compaction.default_policy, "heuristic");
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
    fn journal_defaults_fsync_always_queries_on() {
        // spec mu-046: absent [journal] section → fsync always,
        // journal_queries true — the durable-by-default posture.
        let c: Config = toml::from_str("").expect("empty TOML must parse");
        assert_eq!(c.journal.fsync, "always");
        assert!(c.journal.journal_queries);
        assert_eq!(c.journal.dir, None);
        assert_eq!(
            c.journal.fsync_policy(),
            crate::command_journal::FsyncPolicy::Always
        );
    }

    #[test]
    fn journal_toml_can_opt_out_of_fsync_and_set_dir() {
        let c: Config = toml::from_str(
            "[journal]\nfsync = \"never\"\njournal_queries = false\ndir = \"/tmp/mu-j\"\n",
        )
        .expect("parse");
        assert_eq!(
            c.journal.fsync_policy(),
            crate::command_journal::FsyncPolicy::Never
        );
        assert!(!c.journal.journal_queries);
        assert_eq!(c.journal.dir, Some(PathBuf::from("/tmp/mu-j")));
    }

    #[test]
    fn journal_fsync_typo_fails_durable_not_fast() {
        // An unrecognized fsync value must resolve to Always — the
        // misread-knob failure mode is "slower", never "commands lost".
        let c: Config = toml::from_str("[journal]\nfsync = \"nevr\"\n").expect("parse");
        assert_eq!(
            c.journal.fsync_policy(),
            crate::command_journal::FsyncPolicy::Always
        );
    }

    #[test]
    fn journal_queries_stays_on_when_section_present_but_key_omitted() {
        // The default_true guard, same trap as [recall].memory: a
        // [journal] section that sets another key but omits
        // journal_queries must not silently flip it off.
        let c: Config = toml::from_str("[journal]\nfsync = \"never\"\n").expect("parse");
        assert!(c.journal.journal_queries);
    }

    #[test]
    fn load_with_sources_lists_only_contributing_layers() {
        // spec mu-046 WP6: provenance records what was APPLIED —
        // "defaults" always first, then each file that was present and
        // parsed; missing and malformed files are absent.
        let dir = std::env::temp_dir().join(format!("mu-046-sources-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let present = dir.join("present.toml");
        std::fs::write(&present, "[recall]\nenabled = false\n").unwrap();
        let missing = dir.join("never-written.toml");
        let malformed = dir.join("malformed.toml");
        std::fs::write(&malformed, "not toml ][[[").unwrap();

        let (c, sources) = Config::load_with_sources(&[&present, &missing, &malformed]);
        assert!(!c.recall.enabled, "the present layer applied");
        assert_eq!(
            sources,
            vec!["defaults".to_string(), present.display().to_string()],
            "only defaults + the contributing file are listed"
        );

        // No paths at all: pure defaults.
        let (_, sources) = Config::load_with_sources::<&Path>(&[]);
        assert_eq!(sources, vec!["defaults".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redact_config_strips_auth_tokens() {
        // spec mu-046 INV-6: serialize a config carrying a live token,
        // redact, and assert the token bytes are absent from the
        // serialized output — the same check the boot test (WP6) runs
        // against raw journal bytes.
        let c = Config {
            auth: AuthConfig::Bearer {
                tokens: vec!["super-secret-bearer-token".to_string()],
            },
            ..Default::default()
        };
        let mut v = serde_json::to_value(&c).expect("config serializes");
        redact_config(&mut v);
        let bytes = serde_json::to_string(&v).expect("redacted value serializes");
        assert!(
            !bytes.contains("super-secret-bearer-token"),
            "token leaked through redaction: {bytes}"
        );
        assert!(bytes.contains("[REDACTED]"), "expected marker: {bytes}");
        // Non-secret fields survive: the shape stays legible.
        assert!(bytes.contains("compaction"), "shape lost: {bytes}");
    }

    #[test]
    fn redact_config_strips_denylisted_keys_at_any_depth() {
        let mut v = serde_json::json!({
            "providers": {
                "nested": { "api_key": "sk-live-123", "API_KEY": "sk-live-456" },
                "list": [ { "secret": "hush" }, { "model": "ok" } ],
            },
            "model": "claude-haiku-4-5",
        });
        redact_config(&mut v);
        let bytes = serde_json::to_string(&v).expect("serializes");
        assert!(!bytes.contains("sk-live-123"), "{bytes}");
        assert!(!bytes.contains("sk-live-456"), "case-insensitive: {bytes}");
        assert!(!bytes.contains("hush"), "arrays recursed: {bytes}");
        assert!(
            bytes.contains("claude-haiku-4-5"),
            "non-secret kept: {bytes}"
        );
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
