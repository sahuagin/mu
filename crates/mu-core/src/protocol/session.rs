//! Session-lifecycle wire types: create / ask / cancel / close / stats,
//! plus the mu-038 projection queries (`session.list`, `session.events`),
//! mu-035 `session.cancel_outstanding`, mu-031 `session.delegate`, and
//! mu-029 `session.respond_to_input_required`.
//!
//! This is the bulk of the JSON-RPC surface — most things a frontend
//! says to the daemon route through one of these types.
//!
//! Extracted from `protocol.rs` per mu-6a8 phase 5 (2026-05-18); re-exported
//! by `protocol::*` so external callers see no API change.

use serde::{Deserialize, Serialize};

use super::{ApprovalDecision, ProviderStatusKind};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub provider: ProviderSelector,
    /// Optional system prompt override. None → daemon default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// mu-phl v0 / mu-045: operator's working directory at the time
    /// of session creation. Used by the daemon to scope recall
    /// providers (`agent memory context --cwd ...`, `./CLAUDE.md`
    /// resolution). None → daemon falls back to its own process cwd
    /// (back-compat with pre-mu-phl clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    /// mu-f1a0: prompt-cache TTL tier for providers with tiered
    /// caching (Anthropic: "5m" default / "1h" extended). None → 5m.
    /// Interactive frontends want "1h" — human gaps >5min dominated
    /// cache-write cost on measured sessions (74% of baseline writes
    /// were expiry re-writes); batch and delegated-worker sessions
    /// stay on "5m" (gap-free, the 2x write premium is pure cost).
    /// The daemon's delegate path pins workers to 5m regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<crate::context::CacheTtl>,
    /// mu-7e21: autonomy grant for this root session. None → the
    /// INV-1 default (`AutonomyCapability::Disallowed`). The grant
    /// flows operator → client → daemon at creation time only; the
    /// model can never widen it (attenuation is intersect-only and no
    /// agent-facing surface writes capability). Granting here also
    /// makes the autonomy tools (`start_autonomous`,
    /// `schedule_wakeup`) appear in the session's tool list — the
    /// tool list stays capability-honest: a session that can't use
    /// the surface doesn't see it; one that can, does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy: Option<crate::capability::AutonomyCapability>,
    /// mu-n25a: the side-effects CEILING for this root session — the
    /// posture an operator's "read only" binds to. `None` → root default
    /// (unrestricted, Execute-equivalent ceiling; back-compat: nobody is
    /// restricted unless they opt in). `Some(SideEffects::ReadOnly)` =>
    /// every tool whose declared side-effects exceed ReadOnly is refused
    /// at dispatch, with NO per-tool opt-in (write/edit/watch/spawn_worker
    /// all gated). Like `autonomy`, the grant flows operator → client →
    /// daemon at creation time only; the model can never widen it
    /// (attenuation is intersect-only, narrower-wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_side_effects: Option<crate::agent::tool::SideEffects>,
    /// mu-779s: cap on assistant-message turns. `None` → use the
    /// provider-aware default (20 for Anthropic, 35 for OpenAI, etc.).
    /// `Some(n)` → cap at `n` turns. `Some(0)` → disable entirely.
    /// Forwarded as `AgentConfig::max_turns` to the agent loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// mu-vcbm: launch-time reasoning-effort default for this session
    /// (e.g. `low` .. `max`, provider-specific vocabulary). Forwarded as
    /// `AgentConfig::effort` to the agent loop, which seeds the session's
    /// standing effort and passes it to `Provider::stream` each turn.
    /// `None` ⇒ the provider's own construction-time default (its
    /// `--thinking` launch value, if any). A live `/effort` change rides
    /// in later on [`AskSessionRequest::effort`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl CreateSessionRequest {
    pub const METHOD: &'static str = "create_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    /// mu-uvuo: the daemon-resolved reasoning-effort vocabulary for the
    /// session's route. The daemon owns the model catalog, so it — not the
    /// frontend — resolves which effort levels a provider/model accepts;
    /// frontends render this set instead of recomputing it locally (a
    /// remote frontend has no catalog at all). Daemons that implement
    /// mu-uvuo always send this field: an empty list is the authoritative
    /// "this route has no effort dial". `None` (absent) ⇒ older daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_effort_levels: Option<Vec<String>>,
    /// mu-uvuo: daemon-resolved default effort for the route, when the
    /// catalog declares one. Frontends snap to this when their current
    /// effort is not in `valid_effort_levels`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// Provider selection at session-create time. Tagged enum so the wire
/// format is `{ "kind": "anthropic_api", "model": "claude-..." }`.
///
/// As of mu-019, `openai_codex` is the canonical name for OAuth-based
/// access to OpenAI via the Codex backend. Earlier protocol drafts
/// used `openai_oauth`; the rename happened when mu started talking
/// to `chatgpt.com/backend-api/codex/responses` directly instead of
/// shelling out to pi.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderSelector {
    AnthropicApi {
        model: String,
    },
    AnthropicOauth {
        model: String,
    },
    OpenaiApi {
        model: String,
    },
    OpenaiCodex {
        model: String,
    },
    Openrouter {
        model: String,
    },
    /// Local vLLM/OpenAI-compatible server. Wire kind `"vllm"`.
    /// Endpoint defaults to `http://127.0.0.1:8000`, overridable via
    /// `VLLM_API_BASE`; path overridable via `VLLM_API_PATH`.
    Vllm {
        model: String,
    },
    /// Local ollama server (OpenAI-compatible). Wire kind `"ollama"`.
    /// Endpoint defaults to the LAN box (`http://10.1.1.143:11434`),
    /// overridable via `OLLAMA_API_BASE`. (bead mu-818c)
    Ollama {
        model: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSessionRequest {
    pub session_id: String,
    pub user_message: String,
    /// mu-vcbm: per-turn reasoning-effort selection (`/effort`). `None`
    /// ⇒ leave the session's standing effort unchanged. `Some(level)`
    /// updates it STICKILY — it applies to this turn and persists for
    /// subsequent turns until changed again. The daemon carries it on
    /// the resulting `AgentInput::UserMessage`; the agent loop maps it
    /// onto the provider's thinking/reasoning directive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl AskSessionRequest {
    pub const METHOD: &'static str = "ask_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSessionResponse {
    /// Acknowledgement that the request was accepted; the actual content
    /// is delivered via `session.*` notifications. Final terminator is
    /// the `session.done` notification.
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelSessionRequest {
    pub session_id: String,
}

impl CancelSessionRequest {
    pub const METHOD: &'static str = "cancel_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelSessionResponse {
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseSessionRequest {
    pub session_id: String,
}

impl CloseSessionRequest {
    pub const METHOD: &'static str = "close_session";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloseSessionResponse {
    pub closed: bool,
}

/// Query a session's running totals (mu-027). The result is a
/// snapshot, derived from the session's durable event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsRequest {
    pub session_id: String,
}

impl SessionStatsRequest {
    pub const METHOD: &'static str = "session.stats";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsResponse {
    pub session_id: String,
    /// Provider kind from the wire protocol (e.g. "openai_codex").
    /// None if no SessionCreated event has been recorded (shouldn't
    /// happen in normal use; defensive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Unix ms of the first event (typically SessionCreated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    /// Unix ms of the most recent event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_unix_ms: Option<u64>,
    /// Total event count in the log.
    pub event_count: u32,
    /// Number of completed ask_session round-trips.
    pub ask_count: u32,
    /// Sum of Done.turn_count across all asks.
    pub total_turn_count: u32,
    /// Number of tool invocations.
    pub tool_call_count: u32,
    /// Sum of Done.elapsed_ms across all asks.
    pub elapsed_total_ms: u64,
    /// Aggregated usage across all asks. None if no Done event
    /// reported usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::agent::Usage>,
}

// ===== mu-038: projection queries (session.list, session.events) =====

/// Filter for `session.list`. All fields optional; default = "all
/// local, no limit." Forward-compat additive: new fields added in
/// future revisions can be ignored by older daemons.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionListFilter {
    /// Include sessions from peer daemons (requires a federating
    /// SessionDiscovery backend like FileBackend or EtcdBackend).
    /// LocalRegistryBackend ignores this flag — it only sees local.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_remote: bool,
    /// Only sessions whose parent_session_id matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Only sessions in the given status. Default = any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatusSummary>,
    /// Only sessions with last_activity_unix_ms >= this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_since_unix_ms: Option<u64>,
    /// Cap response size. 0 or None ⇒ no limit (use cautiously).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Derived summary of where a session is in its lifecycle. Computed
/// from the session's event log (post-mu-035, the live
/// ProviderStatusTracker is authoritative for local sessions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatusSummary {
    /// No ask in flight; last event was Done/SessionClosed or the
    /// log is empty.
    Idle,
    /// User message arrived; model call may or may not have started.
    Asking,
    /// Model is producing text (text_delta-style activity within the
    /// last ~5s).
    Streaming,
    /// A tool call is in flight (started but not yet completed).
    ToolExecuting,
    /// A session.input_required notification is outstanding; the
    /// session is blocked on a client approve/deny.
    AwaitingInputRequired,
    /// Last completed ask ended cleanly.
    Done,
    /// Last event was Error.
    Errored,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    /// Stable per-daemon identifier (UUID generated at startup). Used
    /// by federating discovery backends to disambiguate sessions
    /// across daemons.
    pub daemon_id: String,
    /// True iff this session is in a peer daemon (only ever true with
    /// include_remote + a federating backend).
    pub is_remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub provider_kind: String,
    pub model: String,
    pub status: SessionStatusSummary,
    pub started_at_unix_ms: u64,
    pub last_activity_unix_ms: u64,
    pub ask_count: u32,
    pub tool_call_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_usage: Option<crate::agent::Usage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<SessionListFilter>,
}

impl SessionListRequest {
    pub const METHOD: &'static str = "session.list";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionInfo>,
    pub snapshot_at_unix_ms: u64,
    /// Set when `include_remote=true` and one or more peer daemons
    /// failed to respond. Local results are still included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_peers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventsRequest {
    pub session_id: String,
    /// Resume cursor from a prior page. Returns events with id > this
    /// value. Omit to start from the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_event_id: Option<u64>,
    /// Cap response size. None or 0 ⇒ a sensible default (200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Restrict to specific payload kinds (e.g. ["text_delta",
    /// "tool_call"]). Empty/omitted ⇒ all kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds_filter: Vec<String>,
}

impl SessionEventsRequest {
    pub const METHOD: &'static str = "session.events";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventsResponse {
    /// Already-serialised SessionEvent values (see event_log.rs for
    /// the shape). Returned as serde_json::Value so wire consumers
    /// can decode lazily without depending on mu-core types.
    pub events: Vec<serde_json::Value>,
    /// Cursor for the next page. None when end_of_log is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_event_id: Option<u64>,
    pub end_of_log: bool,
}

// ===== mu-035: session.cancel_outstanding =====

/// Cancel the **outstanding provider call** for a session without
/// ending the session itself (mu-035). The agent loop aborts the
/// in-flight stream and surfaces a CancelOutstanding outcome to the
/// loop's outer driver, which decides what to do next (retry on the
/// same provider, fall over to a different one, surface to a human).
///
/// Distinct from `cancel_session`: that ends the session. This kills
/// just the current provider call; the session is still addressable
/// via `ask_session` immediately after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOutstandingRequest {
    pub session_id: String,
    /// Free-form reason for the cancel. Logged in the event log; not
    /// otherwise interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CancelOutstandingRequest {
    pub const METHOD: &'static str = "session.cancel_outstanding";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOutstandingResponse {
    /// True iff a provider call was actually in flight at the time of
    /// the request. False (with `was_in: Idle`) when the call is a
    /// no-op because nothing was outstanding.
    pub canceled: bool,
    pub was_in: ProviderStatusKind,
}

/// Create a new "child" session that's lineage-aware of `parent_session_id`
/// (mu-031). The child session is fully independent at the runtime
/// level — own agent loop, own event log, own pending-approvals
/// registry — but carries a reference to its parent for audit, and
/// optionally a narrowed `Capability` derived from the parent's
/// (mu-033). v1: the child starts with empty message history;
/// `branched_at_parent_event_id` is recorded for audit/replay but
/// doesn't affect runtime state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateSessionRequest {
    pub parent_session_id: String,
    /// Provider for the child. Independent of the parent's — a child
    /// can use a different provider/model than its parent.
    pub provider: ProviderSelector,
    /// Optional: which event in the parent's log this branched from.
    /// For audit; v1 doesn't act on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_at_parent_event_id: Option<u64>,
    /// Optional capability attenuations (mu-033). The child's
    /// effective capability is the intersection of the parent's
    /// capability with this. Any field omitted is "no further
    /// narrowing on this axis from this request." If absent
    /// entirely, the child inherits the parent's capability
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attenuations: Option<crate::capability::CapabilityAttenuations>,
    /// mu-phl v0 / mu-045: child session's working directory. Same
    /// semantics as [`CreateSessionRequest::cwd`]. None → daemon
    /// fallback (process cwd); see mu-045 for the rationale on why
    /// children do not auto-inherit parent cwd in v0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
}

impl DelegateSessionRequest {
    pub const METHOD: &'static str = "session.delegate";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegateSessionResponse {
    pub child_session_id: String,
}

// ── mu-mh4: session resume (fork-at-tail) ────────────────────────────

/// A parsed reference to a session, addressing it by daemon + session.
///
/// Two textual forms are accepted (PR #206 contract):
///   - `daemon:session`               — the operator-friendly short form
///   - `mu:<daemon>/<session>`        — the canonical fleet-wide form
///
/// Either part MAY be a prefix; resolution against the events directory
/// disambiguates (and refuses ambiguity rather than guessing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub daemon: String,
    pub session: String,
}

impl SessionRef {
    /// Parse a `daemon:session` or `mu:<daemon>/<session>` reference.
    /// Returns a human-readable error naming the accepted forms when
    /// the input matches neither.
    pub fn parse(s: &str) -> Result<SessionRef, String> {
        let s = s.trim();
        // Canonical form: mu:<daemon>/<session>
        if let Some(rest) = s.strip_prefix("mu:") {
            let (daemon, session) = rest.split_once('/').ok_or_else(|| {
                format!("invalid session ref `{s}`: canonical form is `mu:<daemon>/<session>`")
            })?;
            if daemon.is_empty() || session.is_empty() {
                return Err(format!(
                    "invalid session ref `{s}`: both daemon and session are required"
                ));
            }
            return Ok(SessionRef {
                daemon: daemon.to_string(),
                session: session.to_string(),
            });
        }
        // Short form: daemon:session
        let (daemon, session) = s.split_once(':').ok_or_else(|| {
            format!(
                "invalid session ref `{s}`: expected `daemon:session` \
                 or canonical `mu:<daemon>/<session>`"
            )
        })?;
        if daemon.is_empty() || session.is_empty() {
            return Err(format!(
                "invalid session ref `{s}`: both daemon and session are required"
            ));
        }
        Ok(SessionRef {
            daemon: daemon.to_string(),
            session: session.to_string(),
        })
    }

    /// Render in canonical `mu:<daemon>/<session>` form.
    pub fn to_canonical(&self) -> String {
        format!("mu:{}/{}", self.daemon, self.session)
    }
}

impl std::fmt::Display for SessionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_canonical())
    }
}

/// `session.resume` — STRICT fork-at-tail. The daemon resolves the
/// predecessor session, projects its log to the last clean boundary
/// ([`crate::agent::continuation::project_strict`]), and — only if the
/// log is clean — births a fresh live session parented on the dead one,
/// seeded with the continuation history. A ragged log is REFUSED with a
/// precise diagnosis (which record, what's missing) and a `mu --recover`
/// hint; it does NOT silently truncate (that's `session.recover`'s job).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeSessionRequest {
    /// The predecessor session, as `daemon:session` or
    /// `mu:<daemon>/<session>` (both parsed by [`SessionRef::parse`]).
    pub session_ref: String,
    /// Provider for the resumed (live) session. Independent of the
    /// predecessor's — resume can switch providers (e.g. away from the
    /// one that died).
    pub provider: ProviderSelector,
    /// Optional capability attenuations for the resumed session
    /// (mu-033 intersection-only semantics). When present, the resumed
    /// session's capability is the predecessor's ∩ these — so a resume
    /// can ONLY narrow, never widen. This is the hook that lets resume
    /// double as voluntary-least-privilege (fork your own memory into a
    /// read-only child); the RPC does not preclude it. None → the
    /// resumed session gets a fresh root capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attenuations: Option<crate::capability::CapabilityAttenuations>,
    /// Working directory for the resumed session (same semantics as
    /// [`CreateSessionRequest::cwd`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    /// Actor requesting the resume (operator / agent id). Recorded on
    /// the `HeadAttached` event for attribution. None → "operator".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

impl ResumeSessionRequest {
    pub const METHOD: &'static str = "session.resume";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeSessionResponse {
    /// The new live session's id.
    pub session_id: String,
    /// The predecessor session id that was forked from.
    pub predecessor_session_id: String,
    /// The event id in the predecessor's log this forked at (the clean
    /// boundary). None when forked at the empty conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_at_event_id: Option<u64>,
    /// Number of messages seeded into the resumed session from the
    /// continuation projection.
    pub seeded_message_count: usize,
}

/// Respond to an outstanding `session.input_required` notification
/// (mu-029). The daemon blocks the corresponding tool call until
/// the client sends this back. `request_id` identifies which prompt
/// is being answered; `decision` is approve or deny.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RespondToInputRequiredRequest {
    pub session_id: String,
    pub request_id: String,
    pub decision: ApprovalDecision,
}

impl RespondToInputRequiredRequest {
    pub const METHOD: &'static str = "session.respond_to_input_required";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RespondToInputRequiredResponse {
    /// True if the daemon found the pending request and relayed
    /// the decision. False if the request_id was unknown (already
    /// answered, timed out, or never existed).
    pub accepted: bool,
}

/// mu-k56u: switch provider+model on an existing session between turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetRouteRequest {
    pub session_id: String,
    pub provider: ProviderSelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_hash: Option<String>,
}

impl SetRouteRequest {
    pub const METHOD: &'static str = "session.set_route";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetRouteResponse {
    pub provider_kind: String,
    pub model: String,
    /// mu-uvuo: daemon-resolved effort vocabulary for the NEW route (see
    /// [`CreateSessionResponse::valid_effort_levels`]). Sent on every
    /// switch so the frontend's effort dial tracks the route without a
    /// local catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_effort_levels: Option<Vec<String>>,
    /// mu-uvuo: daemon-resolved default effort for the new route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

// ── generic config-plane message (mu-context-limits-wire phase 2) ────
//
// `session.get_config` / `session.set_config` are deliberately GENERIC:
// they carry key→value pairs, not a fixed field per setting. The set of
// addressable keys lives in the daemon handler's registry (the first is
// `context.soft_limit`); adding a key never changes these wire types.
// Both are gated on the session's `ConfigCapability` axis (get needs
// ReadOnly+, set needs ReadWrite). Selective by construction: get returns
// ONLY the requested keys; the daemon never volunteers the whole config.

/// Read named config keys. Each key is a dotted path (e.g.
/// `"context.soft_limit"`). The sentinel `"*"` asks for the entire
/// readable config and must be sent explicitly; an empty `keys`
/// returns nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetConfigRequest {
    pub session_id: String,
    #[serde(default)]
    pub keys: Vec<String>,
}

impl GetConfigRequest {
    pub const METHOD: &'static str = "session.get_config";
    /// Explicit sentinel for "the whole readable config".
    pub const ALL: &'static str = "*";
}

/// The requested keys and their current values — never more than asked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetConfigResponse {
    pub values: std::collections::BTreeMap<String, serde_json::Value>,
}

/// One `key → value` assignment in a [`SetConfigRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigEntry {
    pub key: String,
    pub value: serde_json::Value,
}

/// Write named config keys. Each entry is validated and applied
/// independently; the response reports per-key success/failure (one bad
/// key doesn't sink the others).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetConfigRequest {
    pub session_id: String,
    pub entries: Vec<ConfigEntry>,
}

impl SetConfigRequest {
    pub const METHOD: &'static str = "session.set_config";
}

/// One successfully-applied key, with its effective (possibly
/// normalized) value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigApplied {
    pub key: String,
    pub value: serde_json::Value,
}

/// One rejected key, with a human-readable reason (unknown key,
/// validation failure, read-only key, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigRejected {
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetConfigResponse {
    pub applied: Vec<ConfigApplied>,
    pub rejected: Vec<ConfigRejected>,
}

// ── mu-slat: spawned worker sessions ─────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnWorkerRequest {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // Legacy wire name; now interpreted as optional worker name.
    pub pot_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pot_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

impl SpawnWorkerRequest {
    pub const METHOD: &'static str = "session.spawn_worker";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnWorkerResponse {
    pub session_id: String,
    // Legacy wire name; now interpreted as worker name.
    pub pot_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Spawning,
    Running,
    Done { exit_code: i32, elapsed_ms: u64 },
    Failed { reason: String },
}

#[cfg(test)]
mod session_ref_tests {
    use super::SessionRef;

    #[test]
    fn parses_short_form() {
        let r = SessionRef::parse("daemonABC:session-1").expect("short form");
        assert_eq!(r.daemon, "daemonABC");
        assert_eq!(r.session, "session-1");
        assert_eq!(r.to_canonical(), "mu:daemonABC/session-1");
    }

    #[test]
    fn parses_canonical_form() {
        let r = SessionRef::parse("mu:b533a22e600b31e0/session-1").expect("canonical");
        assert_eq!(r.daemon, "b533a22e600b31e0");
        assert_eq!(r.session, "session-1");
    }

    #[test]
    fn canonical_round_trips() {
        let r = SessionRef::parse("mu:d/s").unwrap();
        assert_eq!(SessionRef::parse(&r.to_canonical()).unwrap(), r);
    }

    #[test]
    fn rejects_missing_separator() {
        let err = SessionRef::parse("justonepart").unwrap_err();
        assert!(err.contains("daemon:session"), "{err}");
        assert!(err.contains("mu:<daemon>/<session>"), "{err}");
    }

    #[test]
    fn rejects_empty_parts() {
        assert!(SessionRef::parse("daemon:").is_err());
        assert!(SessionRef::parse(":session").is_err());
        assert!(SessionRef::parse("mu:/session").is_err());
        assert!(SessionRef::parse("mu:daemon/").is_err());
    }

    #[test]
    fn canonical_takes_precedence_over_colon_split() {
        // "mu:" prefix routes to canonical parsing, so the FIRST slash
        // splits daemon/session — a colon inside is not a separator.
        let r = SessionRef::parse("mu:dae/sess").unwrap();
        assert_eq!(r.daemon, "dae");
        assert_eq!(r.session, "sess");
    }
}
