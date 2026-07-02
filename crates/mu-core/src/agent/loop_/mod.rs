//! Queue-driven agent loop.
//!
//! See spec mu-003 for the full design. Briefly:
//!
//! - The loop processes `Action`s from a `VecDeque`.
//! - External callers push `AgentInput` via `AgentLoop::send`; the
//!   loop wraps them as `Action::External` and processes in order.
//! - Long-running actions (`InvokeLlm`, `ExecuteTools`) `select!`
//!   between their own work and `input_rx.recv()`, buffering
//!   `UserMessage`s for later and short-circuiting on `Cancel`.
//! - Termination via no-tool-calls assistant message, iteration cap,
//!   `Cancel`, or unrecoverable error.

// Submodules
mod autonomy;
mod compaction_integration;
mod execute_tools;
mod invoke;

// Re-exports
pub use autonomy::RunMode;
pub use execute_tools::TOOL_HISTORY_WINDOW;

// Internal module imports
use execute_tools::ToolHistory;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::capability::{AutonomyCapability, Capability};
use crate::command_journal::CommandTicket;
use crate::context::rope::SpanText;
use crate::context::{
    CompactionTrigger, ProjectContext, ProjectionTarget, ProviderMessages, RetainedRope,
};

/// mu-kgu.4: default compaction threshold in tokens. Matches the
/// Anthropic API's documented automatic-compaction trigger (150k
/// input tokens) so a session that opts into a real compaction
/// policy without specifying a threshold experiences the same
/// trigger shape as Claude Code's native compaction.
pub const DEFAULT_COMPACTION_THRESHOLD: usize = 150_000;

/// mu-wsgx: structural overhead added to the raw renderer estimate
/// when no feedback anchor is available (first call of a session, or
/// first call after the rope lineage changed). Covers what the rope
/// estimate is structurally blind to: system content sent outside
/// the rope (the effective-prompt time line) and per-message /
/// per-tool-schema request framing.
///
/// Sized from the measured gap on session c76f6949 (sonnet, 61–67
/// calls): linear fit `gap = 7,612 + 0.121 × estimate`. The constant
/// half is this overhead; the multiplicative 12.1% residual is
/// tokenizer bias (chars/4 vs Anthropic BPE), which the feedback
/// anchor absorbs from call 2 onward (median |error| 316 tokens vs
/// 20,982 for the raw estimate) — so it is deliberately NOT applied
/// here. Operator decision on mu-wsgx: no tokenizer dependency;
/// better is the enemy of good.
const ESTIMATE_FALLBACK_OVERHEAD_TOKENS: usize = 8_000;

/// mu-wsgx: feedback anchor — pairs the provider-reported prompt
/// total of the most recent model call (via the stamped
/// [`UsageSemantics::prompt_total`]) with the renderer estimate of
/// the rope that was sent on that same call. The compaction-trigger
/// measure is then `actual + (current_estimate − anchor_estimate)`:
/// exact provider accounting for everything already sent, chars/4
/// only for the (small) delta of new spans — self-calibrating across
/// providers with zero tokenizer dependency.
///
/// [`UsageSemantics::prompt_total`]: crate::agent::capabilities::UsageSemantics::prompt_total
struct FeedbackAnchor {
    /// Exact prompt total the provider reported for the last call.
    actual_prompt_total: u64,
    /// Renderer estimate of the rope sent on that same call.
    rope_estimate: usize,
}

/// mu-wsgx: the compaction-trigger measure. With a valid anchor,
/// predict the next prompt total from provider feedback plus the
/// estimate delta of spans added since. Without one — first call, or
/// the rope SHRANK below the anchor's estimate (a compaction landed
/// between, changing the lineage; the anchor's actual would
/// over-predict) — fall back to the raw estimate plus the structural
/// overhead constant.
fn predicted_prompt_total(anchor: Option<&FeedbackAnchor>, rope_estimate: usize) -> usize {
    match anchor {
        Some(a) if rope_estimate >= a.rope_estimate => {
            a.actual_prompt_total as usize + (rope_estimate - a.rope_estimate)
        }
        _ => rope_estimate + ESTIMATE_FALLBACK_OVERHEAD_TOKENS,
    }
}
use crate::protocol::{
    ApprovalDecision, AutonomousIterationOutcome, AutonomousTerminationReason, AutonomyOptions,
};

use super::provider::Provider;
use super::tool::{Tool, ToolSpec};
use super::types::Usage;
use super::types::{AgentMessage, AssistantMessage, ContentBlock, StopReason, ToolCall};

// Use these types from submodules internally
use compaction_integration::CompactionBaseline;
use execute_tools::{handle_execute_tools, ExecuteToolsExit};
use invoke::handle_invoke_llm;

/// Map of outstanding `session.input_required` prompts, keyed by
/// `request_id`. Owned by the daemon's `Sessions` registry but
/// shared with the AgentLoop so it can both insert pending approvals
/// (before emitting `AgentEvent::InputRequired`) and have its
/// counterpart in the daemon's dispatch handler take entries out
/// when responses arrive.
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>>;

/// Shared handle to the session's `Capability` (mu-033). Wrapped in
/// a `Mutex` so the agent loop can both check it (read) and consume
/// tool-call budget (mutate). The Arc lets the daemon's
/// `Sessions::insert` and the AgentLoop hold the same instance.
pub type SessionCapability = Arc<Mutex<Capability>>;

/// External inputs callers push to a running agent loop.
#[derive(Clone)]
pub enum AgentInput {
    /// Add a message to the conversation. Loop runs the LLM after.
    ///
    /// spec mu-046 WP4: the second field is the optional receipt
    /// ticket minted by the ingest pipeline when this message arrived
    /// as a journaled `ask_session` command. The loop collects tickets
    /// per ask and carries them out on the terminal
    /// [`AgentEvent::Done`], where the forwarder writes the
    /// `CommandSucceeded`/`CommandFailed` receipt into the session's
    /// event log with explicit pairing (no side tables). `None` for
    /// internal/synthetic messages (tools, tests, autonomy
    /// continuations).
    ///
    /// mu-vcbm: the third field is the per-turn reasoning-effort
    /// selection that arrived with this ask (`/effort` →
    /// `AskSessionRequest.effort`). `Some(level)` updates the session's
    /// standing effort STICKILY (it persists for subsequent turns until
    /// changed again); `None` leaves the standing effort untouched.
    /// Synthetic/internal user messages pass `None`.
    UserMessage(AgentMessage, Option<Box<CommandTicket>>, Option<Arc<str>>),
    /// Stop. In-flight provider stream and tool execution are
    /// cancelled; loop returns `Outcome::Cancelled`.
    Cancel,
    /// Narrow-cancel (mu-035 Phase C): abort the current provider
    /// stream / tool dispatch, emit a Done(Aborted) for the ask, but
    /// keep the session alive for subsequent ask_sessions. Distinct
    /// from `Cancel`, which terminates the entire agent loop.
    CancelOutstanding { reason: String },
    /// mu-036 Phase B: transition the session into RunMode::Autonomous
    /// with `goal` + `options`. The daemon's
    /// `handle_start_autonomous` constructs this after checking the
    /// session's `AutonomyCapability::Allowed` (INV-1). The agent
    /// loop re-checks defensively and reads enforced bounds from
    /// the session's `Capability`, not from `options` (INV-2).
    StartAutonomous {
        goal: String,
        options: AutonomyOptions,
    },
    /// mu-036 Phase C (mu-7zn): park the autonomous loop until
    /// `wake_at_unix_ms` (wall-clock). The daemon's
    /// `handle_schedule_wakeup` constructs this after checking the
    /// session's `AutonomyCapability::Allowed { allow_schedule_wakeup:
    /// true }` and resolving `sleep_for_ms` to an absolute time.
    /// Honored only while the session is in `RunMode::Autonomous`; on
    /// wake the loop resumes at iteration N+1 with `reason` as the
    /// motivation (INV-5: no model/tool budget consumed while parked).
    ScheduleWakeup {
        wake_at_unix_ms: u64,
        reason: String,
    },
    /// mu-k56u: replace the provider between turns. The loop swaps
    /// its local provider variable and emits a ProviderSwitched event.
    /// Carries provider_kind + model alongside the provider instance
    /// because the Provider trait doesn't expose the model name.
    SwitchProvider {
        provider: Arc<dyn Provider>,
        provider_kind: Arc<str>,
        model: Arc<str>,
        /// mu-ub6q: the new model's output budget, so the loop re-reserves
        /// compaction headroom on a model switch. Resolved from the route
        /// catalog by the daemon's `handle_set_route`. `0` ⇒ no reservation.
        max_output_tokens: usize,
        /// mu-ub6q: the new model's soft limit. The loop stores it to
        /// `live_context_soft_limit` IN THE SAME handler that applies
        /// `max_output_tokens`, so a switch updates both halves of the
        /// effective trigger atomically (from the loop's view) — a queued
        /// turn can never pair the new reservation with the old soft
        /// limit. `0` ⇒ unset (loop falls back to config/default), as at
        /// creation.
        context_soft_limit: u64,
    },
    /// mu-slat Phase 2: a mailbox message arrived for this session.
    /// Injected by the mailbox.post handler when the target is a live
    /// session. The loop synthesizes a UserMessage and queues InvokeLlm
    /// so the LLM can read and act on it.
    MailboxMessage {
        from_session_id: String,
        message_kind: String,
        subject: String,
        seq: u64,
    },
    /// mu-watch-tool-wakeup-o03p: a watched command (registered via the
    /// `watch` tool) has exited (or timed out and was killed). This is
    /// the EVENT sibling of `ScheduleWakeup`'s TIMER: where the timer
    /// resumes an autonomous run at N+1 after a wall-clock delay, this
    /// wakes the session — autonomous OR idle — the moment a process
    /// it was waiting on finishes. Injected by the watch tool's
    /// background task over the same input channel (the "wakeup
    /// channel", spec mu-036 line 59), NOT a parallel bespoke path. The
    /// loop synthesizes a UserMessage from `note` + `summary` and queues
    /// InvokeLlm so the result lands as the next turn's motivation. A
    /// timed-out / killed watch still sends this (with a killed status)
    /// so silence is impossible: a watch that dies is indistinguishable
    /// from one still running unless it always wakes the model.
    WatchCompleted {
        /// The model-supplied label for the watch (e.g. "CI for PR 42").
        note: String,
        /// Human-readable result: exit status line + output tail.
        summary: String,
    },
    /// mu-dialogue-inbound-wakeup: an inbound dialogue message addressed
    /// to this session arrived on a dialogue/mailbox receive path. This is
    /// the transport-agnostic wake seam: v1 push delivery should feed this
    /// over the same input channel — the "wakeup channel" — as
    /// `WatchCompleted` / `MailboxMessage`. The loop synthesizes a User
    /// message carrying `from` + `content` INLINE and queues InvokeLlm so
    /// the message lands directly as the woken turn's motivation, whether
    /// the session was idle (parked on `input_rx.recv`) or mid-run (queued
    /// behind the current work).
    DialogueMessage {
        /// The peer id that sent the message (e.g. "cc:abcd").
        from: String,
        /// The message body.
        content: String,
    },
}

impl std::fmt::Debug for AgentInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserMessage(m, ticket, effort) => f
                .debug_tuple("UserMessage")
                .field(m)
                .field(&ticket.as_ref().map(|t| t.command_event_id))
                .field(effort)
                .finish(),
            Self::Cancel => write!(f, "Cancel"),
            Self::CancelOutstanding { reason } => f
                .debug_struct("CancelOutstanding")
                .field("reason", reason)
                .finish(),
            Self::StartAutonomous { goal, options } => f
                .debug_struct("StartAutonomous")
                .field("goal", goal)
                .field("options", options)
                .finish(),
            Self::ScheduleWakeup {
                wake_at_unix_ms,
                reason,
            } => f
                .debug_struct("ScheduleWakeup")
                .field("wake_at_unix_ms", wake_at_unix_ms)
                .field("reason", reason)
                .finish(),
            Self::SwitchProvider {
                provider_kind,
                model,
                ..
            } => {
                write!(f, "SwitchProvider({provider_kind}/{model})")
            }
            Self::MailboxMessage {
                from_session_id,
                seq,
                ..
            } => {
                write!(f, "MailboxMessage(from={from_session_id}, seq={seq})")
            }
            Self::WatchCompleted { note, .. } => {
                write!(f, "WatchCompleted(note={note})")
            }
            // Avoid dumping the full message body in Debug output.
            Self::DialogueMessage { from, .. } => {
                write!(f, "DialogueMessage(from={from})")
            }
        }
    }
}

/// Output events emitted by the loop. Mirrors mu-001's `session.*`
/// notifications in shape; mu-coding does the typed-enum → JSON-RPC
/// translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    MessageStart {
        message: AgentMessage,
    },
    TextDelta {
        delta: String,
    },
    /// Streaming reasoning chunk (Anthropic extended thinking, ollama
    /// reasoning models). Mirrors `TextDelta`; the forwarder maps it to a
    /// `session.thinking_delta` notification. Inbound/display only — thinking
    /// is never echoed back TO the model (spec mu-044).
    ThinkingDelta {
        delta: String,
    },
    /// Streaming complete — provider returned its final assistant message with the
    /// final assembled text. Fires before MessageEnd and before session.done,
    /// allowing clients to swap from streaming-text accumulator to finalized
    /// text atomically. The text here matches what will appear in the durable
    /// AssistantMessageEvent. See mu-wk2.
    AssistantTextFinalized {
        text: String,
    },
    /// Reasoning complete — the finalized thinking text from the provider's
    /// assembled `ContentBlock::Thinking` blocks. Mirror of
    /// `AssistantTextFinalized` (mu-wk2) for the thinking channel: lets a
    /// client swap its streaming-thinking accumulator for authoritative
    /// reasoning text. The loop only emits it when the turn actually produced
    /// thinking (non-empty), so text-only turns are unchanged on the wire.
    AssistantThinkingFinalized {
        text: String,
    },
    ToolCallStarted {
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    /// Streaming partial tool call (Anthropic tool_use streamed args). The
    /// provider emits the tool name on the block start and argument fragments
    /// as they arrive; the forwarder maps this to `session.tool_call_delta`.
    /// `ToolCallStarted` remains the authoritative fully-assembled call.
    ToolCallDelta {
        tool_call_id: String,
        name_delta: Option<String>,
        arguments_delta: Option<String>,
    },
    ToolCallCompleted {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
    MessageEnd {
        message: AgentMessage,
    },
    TurnEnd,
    Done {
        stop_reason: StopReason,
        turn_count: u32,
        /// Aggregated token usage across this ask_session's turns.
        /// `None` if no provider in the chain reported usage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Wall time from the first turn's start to this Done emit.
        /// Captures multi-turn tool-use loops; resets per ask_session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
        /// spec mu-046 WP4: tickets of the journaled `ask_session`
        /// commands this Done terminates. The forwarder writes one
        /// session-log receipt per ticket (`CommandSucceeded` on a
        /// normal stop, `CommandFailed` on `Aborted`/`Error`). Empty
        /// for non-command asks; never serialized to the wire (the
        /// wire `session.done` is the separate `DoneEvent`, and the
        /// durable `EventPayload::Done` drops this field too — the
        /// receipt rows carry the correlation instead).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        command_receipts: Vec<CommandTicket>,
    },
    Error {
        message: String,
    },
    /// Catch-all "the agent says something notable" event. See spec
    /// mu-016. Free-form `category`/`theme`. The forwarder
    /// translates to `session.callout` notifications, where this
    /// field becomes the wire-level `kind`.
    ///
    /// (We use `category` here because AgentEvent's serde tag is
    /// already named `kind` — the discriminator. The wire surface
    /// in mu-001's `CalloutEvent` keeps the user-facing `kind` name.)
    Callout {
        category: String,
        title: String,
        /// Either a JSON string (text body) or any structured value.
        /// Body shape is preserved end-to-end; the wire layer (mu-001's
        /// `CalloutBody`) interprets it as Text-or-Structured at
        /// translation time.
        body: serde_json::Value,
        theme: Option<String>,
        context_refs: Vec<String>,
    },
    /// A tool whose policy is `PermissionLevel::Ask` is about to
    /// dispatch; the agent loop is blocked waiting for a
    /// `session.respond_to_input_required` matching `request_id`
    /// before it proceeds. See spec mu-029.
    InputRequired {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
        summary: String,
    },
    /// Prompt assembly snapshot. Emitted by the agent loop BEFORE
    /// each `provider.stream()` call. The forwarder lands it in
    /// the durable event log as `EventPayload::ContextAssembly`.
    /// See spec mu-032 and
    /// `specs/architecture/event-sourced-context.md`.
    ///
    /// mu-fb0: the loop now assembles a `RetainedRope` from the
    /// session state and projects it through the provider's
    /// `ProviderRenderer` + `CacheStrategy` before each stream call.
    /// The optional `renderer` / `cache_strategy` / `span_count` /
    /// `cache_boundary_count` / `first_span_ids` fields carry rope-
    /// derived provenance. All defaults serde-skip so pre-mu-fb0
    /// fixtures remain byte-for-byte stable.
    ContextAssembly {
        model_call_id: u32,
        message_count: u32,
        user_message_count: u32,
        assistant_message_count: u32,
        tool_result_count: u32,
        tool_count: u32,
        /// mu-heqf: total + per-`SpanKind` token estimate of the
        /// rope as rendered for this call (post-compaction when one
        /// ran), under the renderer's own measure — the same scale
        /// the compaction trigger uses. The forwarder lands it in
        /// the durable `ContextAssembly` payload so "what does the
        /// rope hold?" is answerable from the JSONL.
        context_sizes: Option<crate::context::ContextSizes>,
        /// mu-fb0: provider's `renderer().provider_label()`-style tag.
        /// Surfaces which `ProviderRenderer` projected the rope for
        /// this call (e.g., `"anthropic"`, `"faux"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        renderer: Option<String>,
        /// mu-fb0: the cache-strategy identifier in use. Currently
        /// equal to `renderer` (each provider supplies a paired
        /// renderer + strategy), but reported separately so future
        /// A/B-tested strategies over the same renderer are
        /// distinguishable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_strategy: Option<String>,
        /// mu-fb0: total spans in the projected rope (system + tool
        /// schemas + messages). Differs from `message_count`, which
        /// counts conversational turns only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        span_count: Option<u32>,
        /// mu-fb0: number of `CacheBoundary` positions the strategy
        /// placed. 0 for `NoCacheStrategy`; up to 1 for
        /// `AnthropicCacheStrategy` v1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_boundary_count: Option<u32>,
        /// mu-fb0: first N (cap 5) span ids of the rope. Lets
        /// consumers identify which spans entered the prompt without
        /// requiring the full rope dump (per spec line 191 — span
        /// identity + reason_included form the source map).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        first_span_ids: Vec<String>,
        /// mu-814o: blake3 digest (16 hex) of the RENDERED cacheable
        /// prefix — role + content of every message up to the last
        /// cache boundary. A change between consecutive calls with no
        /// compaction = the prefix mutated = full cache invalidation.
        /// `None` when the strategy placed no boundaries.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix_hash: Option<String>,
        /// mu-814o: per-span `"<id>=<blake3 8hex>"` digests of ROPE
        /// content over the same prefix range — names WHICH span
        /// mutated. See `context::cache::prefix_forensics` for the
        /// diagnosis table.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prefix_span_hashes: Vec<String>,
    },
    /// mu-kgu.4: a [`CompactionPolicy`] just produced a new rope
    /// because the pre-render token estimate crossed the configured
    /// threshold. Emitted BEFORE the matching `ContextAssembly` —
    /// the rope `ContextAssembly` reports for that turn is the
    /// POST-compaction rope, so the two events together describe
    /// "what was compacted" and "what was rendered."
    ///
    /// Carries the full per-span audit log (mu-za92): the
    /// [`CompactionDecision`]s say exactly which spans were kept,
    /// dropped (and why), or summarized. Pre-mu-za92 this event
    /// carried only a count and the audit lived solely on the
    /// in-memory rope log — which vanishes on process exit; the
    /// forwarder now lands the decisions in the durable session
    /// event log so "what disappeared and why?" survives restarts.
    ///
    /// [`CompactionPolicy`]: crate::context::CompactionPolicy
    /// [`CompactionDecision`]: crate::context::CompactionDecision
    CompactionAssembly {
        /// Same call counter as the matching [`ContextAssembly`]
        /// event — lets consumers join the two by model_call_id.
        model_call_id: u32,
        /// Short policy identifier reported by
        /// `Provider::compaction_policy().policy_label()` (default:
        /// the trait-object's type-name suffix; concrete policies can
        /// override). Surfaces "which policy ran" in the event stream.
        policy_id: String,
        /// Renderer-estimated token count of the rope BEFORE
        /// compaction. mu-wsgx: the threshold check itself compares
        /// a feedback-predicted prompt total (see
        /// [`predicted_prompt_total`]); this field stays on the
        /// renderer-estimate scale, describing the rope.
        tokens_before: usize,
        /// Renderer-estimated token count of the post-compaction
        /// rope. May exceed `target_tokens` — policies are
        /// best-effort. See [`CompactionPolicy::compact`] doc.
        ///
        /// [`CompactionPolicy::compact`]: crate::context::CompactionPolicy::compact
        tokens_after: usize,
        /// The policy's per-span audit log: kept / dropped(reason) /
        /// summarized / failed(reason), one entry per touched span.
        /// Empty means the policy returned identity (e.g., the
        /// fail-closed path); the loop still emits this event so
        /// the operator sees that compaction was attempted.
        ///
        /// [`CompactionDecision`]: crate::context::CompactionDecision
        decisions: Vec<crate::context::CompactionDecision>,
        /// Wall-clock duration of `policy.compact()` in milliseconds.
        wall_clock_us: u64,
        /// mu-a79g: the feedback-predicted prompt total (mu-wsgx) the
        /// threshold check compared — what made this compaction fire.
        predicted_tokens: usize,
        /// mu-a79g: the soft-limit-derived compaction threshold in
        /// effect for this turn.
        compaction_threshold: usize,
        /// mu-a79g: the mu-ub6q output headroom reserved from the
        /// threshold (`min(max_output, threshold/2)`). The effective
        /// trigger point is `compaction_threshold - output_reserve`;
        /// recording all three makes the event self-describing — a
        /// consumer reconstructs and validates the trigger without
        /// re-deriving the loop's inputs.
        output_reserve: usize,
    },
    /// Provider-call lifecycle marker (mu-035 Phase A). Emitted on
    /// state transitions; Phase B will additionally emit periodic
    /// ticks while in non-streaming waits so a stalled provider
    /// remains visible to a watching client.
    ///
    /// The forwarder translates this to `session.provider_status`
    /// notifications.
    ///
    /// Field is `state` (not `kind`) because the enum's serde tag is
    /// already `kind` (the variant discriminator); reusing the name
    /// causes a serde naming collision.
    ProviderStatus {
        state: crate::protocol::ProviderStatusKind,
        /// Unix milliseconds the session entered this state.
        started_at_unix_ms: u64,
        /// Milliseconds since `started_at_unix_ms` at emit time.
        elapsed_ms: u64,
        /// Cumulative bytes from the provider's stream so far on
        /// this call. None when not meaningful.
        bytes_received: Option<u64>,
        /// Set only when `state` is ToolExecuting or AwaitingToolResult.
        tool_call_id: Option<String>,
    },
    /// mu-036 Phase B: autonomous-mode iteration just started.
    /// `iteration` is 1-indexed across the run; `motivation` is a
    /// one-sentence reason (for iteration 1, the goal itself; for
    /// post-wakeup, the wake reason).
    AutonomousIterationStarted {
        iteration: u32,
        motivation: String,
    },
    /// mu-036 Phase B: autonomous-mode iteration ended. `outcome`
    /// tells the consumer whether the loop continues, exits, errors,
    /// or escalates.
    AutonomousIterationCompleted {
        iteration: u32,
        outcome: AutonomousIterationOutcome,
    },
    /// mu-036 Phase C (mu-7zn): the autonomous loop parked itself via
    /// `session.schedule_wakeup` until `wake_at_unix_ms` (wall-clock).
    /// Durable-only: maps to `EventPayload::AutonomousScheduledWakeup`
    /// in the event log. mu-036's wire-notification surface does not
    /// include a scheduled-wakeup method, so the forwarder emits no
    /// notification for it (the next `AutonomousIterationStarted` on
    /// wake carries `reason` as its motivation, which clients observe).
    AutonomousScheduledWakeup {
        wake_at_unix_ms: u64,
        reason: String,
    },
    /// mu-036 Phase B: autonomous-mode loop terminated. Always the
    /// final autonomy event for a run (INV-7). Session returns to
    /// RunMode::Idle and is addressable via ask_session again.
    AutonomousTerminated {
        reason: AutonomousTerminationReason,
    },
    /// mu-k56u: provider/model switched mid-session. Emitted by the
    /// agent loop after replacing its local provider. The forwarder
    /// translates to `EventPayload::ProviderSwitched`.
    ProviderSwitched {
        old_provider_kind: Arc<str>,
        old_model: Arc<str>,
        new_provider_kind: Arc<str>,
        new_model: Arc<str>,
        /// mu-rf9x: the NEW provider's token-accounting convention,
        /// re-registered at the switch so durable-log readers can
        /// interpret usage records from this point on.
        usage_semantics: crate::agent::capabilities::UsageSemantics,
    },
}

#[derive(Clone)]
pub struct AgentConfig {
    /// Cap on assistant-message turns. The loop emits
    /// `AgentEvent::Done(EndTurn)` and returns `Outcome::IterationCap`
    /// when this is reached. Default 20.
    ///
    /// Set to `None` to disable the iteration cap entirely.
    pub max_turns: Option<u32>,
    /// mu-n48: optional system prompt forwarded to every
    /// `Provider::stream` call in this session. None ⇒ no system
    /// content sent (pre-mu-n48 behavior). When set, providers render
    /// it appropriately (Anthropic top-level `system` field, OpenAI-
    /// style prepended {role:"system"} message), and Anthropic
    /// additionally tags it `cache_control: ephemeral` to amortize
    /// its tokens across asks in the session.
    ///
    /// Backing storage is [`SpanText`] (`Arc<str>` via the mu-yqeq.2
    /// per-type alias) so the daemon pays one allocation when building
    /// the config rather than re-allocating on each rope-build cycle.
    /// The conversion from the wire-layer `Option<String>` happens at
    /// the daemon's session-creation site
    /// ([`crates/mu-coding/src/serve/handlers/session.rs`]).
    pub system_prompt: Option<SpanText>,
    /// mu-kgu.4: per-session token threshold above which the agent
    /// loop dispatches `Provider::compaction_policy().compact(...)`
    /// on the rope before each provider call. `None` uses
    /// [`DEFAULT_COMPACTION_THRESHOLD`] (150k tokens). mu-wsgx: the
    /// check compares a feedback-predicted prompt total — the
    /// provider's exact reported total for the previous call plus the
    /// renderer-estimated delta of spans added since (see
    /// [`predicted_prompt_total`]); before any usage feedback it is
    /// the raw renderer estimate plus a structural-overhead constant.
    /// Policies that don't trigger (e.g. `NoCompactionPolicy`) return
    /// identity and the loop proceeds with the original rope —
    /// compaction failure never blocks a turn.
    pub compaction_threshold: Option<usize>,
    /// mu-ub6q: the model's max output-token budget, reserved as
    /// headroom in the compaction trigger. A request sends the input
    /// AND lets the model generate up to this many tokens on top; both
    /// must fit the context window. The trigger therefore fires when
    /// `predicted_input + max_output_tokens` crosses
    /// [`Self::compaction_threshold`], i.e. while the input still leaves
    /// room for the output — not only when the input alone crosses it.
    /// Without this reservation a soft limit set at (or near) the
    /// model's window is unreachable: input+output overflows and the
    /// provider errors mid-stream before the input-only predicted total
    /// can cross the threshold, so compaction never fires. `0` ⇒ no
    /// reservation (pre-mu-ub6q behavior; tests and unknown routes rely
    /// on this default). This is the *creation-time* seed: the loop
    /// tracks the live value in `current_max_output_tokens` and updates
    /// it on every `SwitchProvider`, so a mid-session model switch
    /// re-reserves the new model's output budget.
    pub max_output_tokens: usize,
    /// mu-phl v0 (bead mu-vm81): pre-built recall context to inject at
    /// session start. Built by the daemon at create-session time
    /// (see `crates/mu-coding/src/serve/handlers/session.rs`) so the
    /// agent loop's hot path stays free of subprocess spawning or
    /// filesystem walks. `None` ⇒ no injection (pre-mu-phl behavior;
    /// tests rely on this default).
    ///
    /// The bundled items land as `MemoryInjection` / `FileLoad` spans
    /// in the stable cacheable prefix of the rope, between the System
    /// span and the ToolSchema spans, via
    /// [`crate::context::assemble_rope_with_context`].
    pub project_context: Option<ProjectContext>,
    /// Override the provider's default compaction policy. When Some,
    /// the agent loop uses this policy instead of
    /// `provider.compaction_policy()`. Wired from daemon config's
    /// `compaction.default_policy` at session creation.
    pub compaction_policy_override: Option<Arc<dyn crate::context::compaction::CompactionPolicy>>,
    /// mu-mh4: pre-seeded conversation history for a RESUMED (forked)
    /// session. Empty for a fresh session (the default). When a session
    /// is born as a fork-at-tail of a dead predecessor (`mu --resume` /
    /// `session.resume`), the daemon projects the predecessor's event
    /// log to its last clean boundary
    /// ([`crate::agent::continuation::project_strict`]) and hands the
    /// resulting [`AgentMessage`] history here, so the resumed loop
    /// starts mid-conversation rather than empty. These messages are NOT
    /// re-logged as events in the new session's log — they live in the
    /// predecessor's log, which the new `SessionCreated` event points
    /// back to via `branched_at_parent_event_id`.
    pub seed_messages: Vec<AgentMessage>,
    /// mu-uz0n: implicit capability discovery. When `Some`, each turn
    /// the loop ranks the last user-role message (operator ask or
    /// autonomous iteration motivation) through the same lexical
    /// ranking the `discover` tool uses and injects the top-N as a
    /// compact transient hint span right after that user span — see
    /// [`crate::context::capability_hints`] for the sizing and cache
    /// discipline. `None` (the default) ⇒ feature off; wired by the
    /// daemon from `[index].discover_injection` at session creation.
    pub discover_hints: Option<crate::context::capability_hints::DiscoverHints>,
    /// mu-vcbm: the session's launch-time reasoning-effort default
    /// (`CreateSessionRequest.effort`). Seeds the loop's standing effort
    /// at session start; subsequent `/effort` changes ride in on
    /// `AgentInput::UserMessage` and override it. `None` ⇒ the provider's
    /// own construction-time default (its `--thinking` value, if any).
    pub effort: Option<Arc<str>>,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("max_turns", &self.max_turns)
            .field("system_prompt", &self.system_prompt.is_some())
            .field("compaction_threshold", &self.compaction_threshold)
            .field("project_context", &self.project_context.is_some())
            .field(
                "compaction_policy_override",
                &self.compaction_policy_override.is_some(),
            )
            .field("discover_hints", &self.discover_hints.is_some())
            .field("effort", &self.effort)
            .finish()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: Some(20),
            system_prompt: None,
            compaction_threshold: None,
            max_output_tokens: 0,
            project_context: None,
            compaction_policy_override: None,
            seed_messages: Vec::new(),
            discover_hints: None,
            effort: None,
        }
    }
}

/// Provider-aware default for [`AgentConfig::max_turns`]. (mu-779s)
///
/// Empirically (2026-05-18 daemon `8c78230c467e1de7`) OpenAI models
/// dispatch noticeably more tool calls than Anthropic models on the
/// same prompt — 20 turns is enough for Anthropic but routinely caps
/// out OpenAI sessions on tool-heavy reads. Per-provider defaults keep
/// the ceiling honest without making operators pass `--max-iterations`
/// every time they touch a non-Anthropic provider.
///
/// `provider_kind` matches the strings produced by
/// `handlers::session::describe_selector` (e.g. `"anthropic_api"`,
/// `"openai_codex"`). Unknown / faux providers fall through to the
/// conservative default.
pub fn default_max_turns_for(provider_kind: &str) -> u32 {
    match provider_kind {
        "anthropic_api" | "anthropic_oauth" => 20,
        "openai_api" | "openai_codex" => 35,
        "openrouter" => 30,
        _ => 20,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done(StopReason),
    IterationCap,
    Cancelled,
    Error(String),
    /// mu-035 Phase C narrow-cancel: the current ask was aborted via
    /// `AgentInput::CancelOutstanding`, but the SESSION is still
    /// alive. The outer run() loop catches this from the inner
    /// handlers, emits a Done(Aborted) event for the ask, resets
    /// per-ask state, and continues to wait for the next ask. Not
    /// returned by run() itself — purely an internal sentinel.
    OutstandingCancelled {
        reason: String,
    },
}

/// Internal action queue. Callers push `AgentInput` via `AgentLoop::send`;
/// the run function wraps `AgentInput::UserMessage` as
/// `Action::External(...)` and pushes it to the queue. Internal state
/// transitions (`InvokeLlm`, `ExecuteTools`, `MaybeFinish`) are private.
#[derive(Debug)]
enum Action {
    External(AgentInput),
    InvokeLlm,
    ExecuteTools(Vec<ToolCall>),
    MaybeFinish,
}

// ============================================================================
// Pure planners
// ============================================================================
//
// Logic between queue-mediated steps gets extracted as pure functions
// here. The async I/O parts of the loop call these to decide what to
// queue / emit, then perform the side effects themselves.
//
// Tests can target the planners directly without async machinery — the
// queue-flow integration is covered by the existing behavior tests
// (B-1..B-7) using mock providers and tools.

/// How many consecutive actionless turns the loop will auto-continue
/// through before giving up and ending the ask. Bounds a provider stuck
/// emitting empties so it can't spin forever (also backstopped by the
/// `max_turns` iteration cap, which every re-invoke counts against).
/// (mu-rb4u)
const MAX_EMPTY_TURN_RETRIES: u32 = 3;

/// A turn is "actionless" when the model returned neither a tool call nor
/// any visible text — e.g. an OpenAI Responses-API reasoning-only
/// completion (`content: []`, `stop_reason: end_turn`, no usage). Such a
/// turn carries no answer and no action; routing it straight to
/// `MaybeFinish` ends the ask and forces the operator to type "continue".
/// The loop auto-continues on it instead (bounded). (mu-rb4u)
fn is_actionless_turn(msg: &AssistantMessage) -> bool {
    msg.content.iter().all(|block| match block {
        ContentBlock::ToolCall(_) => false,
        ContentBlock::Text { text } => text.trim().is_empty(),
        // Reasoning-only output is not, on its own, an actionable turn.
        ContentBlock::Thinking { .. } => true,
    })
}

/// Output of `plan_post_invoke_llm`.
struct PostInvokeLlmPlan {
    /// True iff the loop should emit `AgentEvent::TurnEnd` before
    /// pushing actions. False when tool calls are queued — TurnEnd
    /// then gets emitted by `plan_post_execute_tools` after tools
    /// complete.
    emit_turn_end: bool,
    /// Actions to push to the back of the queue, in order.
    actions: Vec<Action>,
}

/// Decide what to do after the assistant message comes back from a
/// successful `InvokeLlm`. Pure — given the assistant message and any
/// `UserMessage`s that were buffered during the LLM stream, produces
/// the actions to enqueue and whether to emit TurnEnd.
fn plan_post_invoke_llm(
    assistant_msg: &AssistantMessage,
    buffered: Vec<AgentInput>,
) -> PostInvokeLlmPlan {
    let tool_calls: Vec<ToolCall> = assistant_msg
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();

    let mut actions = Vec::new();

    if tool_calls.is_empty() {
        // No tool calls — the ask's turn-chain is complete. TurnEnd
        // here, then ALWAYS MaybeFinish FIRST: the per-ask `Done`
        // terminus is emitted there, and it must land before any
        // buffered UserMessage starts the next ask. (mu-wf5w: the old
        // shape skipped MaybeFinish entirely when buffered UMs
        // existed — "the loop continues naturally" — so the completed
        // ask never emitted `done`. Verified consequence in session
        // 1a7812f064510d91: the client's live block never committed
        // and the whole turn vanished from scrollback. The skip also
        // leaked started_at / turn_count / aggregated_usage into the
        // next ask, inflating its Done and marching turn_count toward
        // IterationCap across asks.)
        actions.push(Action::MaybeFinish);
        for input in buffered {
            actions.push(Action::External(input));
        }
        PostInvokeLlmPlan {
            emit_turn_end: true,
            actions,
        }
    } else {
        // Tool calls — defer TurnEnd until after ExecuteTools.
        actions.push(Action::ExecuteTools(tool_calls));
        for input in buffered {
            actions.push(Action::External(input));
        }
        PostInvokeLlmPlan {
            emit_turn_end: false,
            actions,
        }
    }
}

/// Decide what to enqueue after `ExecuteTools` completes. Pure.
/// Buffered UMs come first so they land in `messages` before the
/// next InvokeLlm runs.
fn plan_post_execute_tools(buffered: Vec<AgentInput>) -> Vec<Action> {
    let mut actions = Vec::with_capacity(buffered.len() + 1);
    for input in buffered {
        actions.push(Action::External(input));
    }
    actions.push(Action::InvokeLlm);
    actions
}

/// Pure dedup check: should we push `InvokeLlm` after processing a
/// UserMessage? Yes unless one is already queued (back-to-back UMs
/// share one LLM call).
fn should_push_invoke_llm(queue: &VecDeque<Action>) -> bool {
    !queue.iter().any(|a| matches!(a, Action::InvokeLlm))
}

/// Handle to a running agent loop.
#[derive(Debug)]
pub struct AgentLoop {
    tx: mpsc::Sender<AgentInput>,
    handle: JoinHandle<Outcome>,
}

/// Inputs to construct an agent loop, bundled so [`AgentLoop::spawn`] and the
/// internal `run` task take one argument instead of nine. Built by the caller
/// (the daemon's session manager) and consumed on spawn.
pub struct SpawnArgs {
    pub provider: Arc<dyn Provider>,
    pub provider_kind: Arc<str>,
    pub model: Arc<str>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub config: AgentConfig,
    pub events: mpsc::Sender<AgentEvent>,
    pub pending_approvals: PendingApprovals,
    pub capability: SessionCapability,
    /// mu-context-limits-wire phase 2: the LIVE context soft limit (=
    /// compaction trigger) in tokens, shared with the daemon so
    /// `session.set_config` can change it mid-session. The loop reads it
    /// at each compaction check; `0` means "unset — fall back to
    /// `config.compaction_threshold`, then `DEFAULT_COMPACTION_THRESHOLD`".
    /// A shared atomic (not an `AgentInput`) so a set during streaming or
    /// a tool call isn't intercepted/lost by the mid-turn input drains.
    pub live_context_soft_limit: Arc<AtomicU64>,
}

impl std::fmt::Debug for SpawnArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `provider`/`tools` are trait objects without Debug; show the
        // identifying, printable fields and elide the rest.
        f.debug_struct("SpawnArgs")
            .field("provider_kind", &self.provider_kind)
            .field("model", &self.model)
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

impl AgentLoop {
    /// Spawn a new agent loop on the current tokio runtime.
    ///
    /// `SpawnArgs::pending_approvals` is the shared registry the loop uses when
    /// dispatching tools with `PermissionLevel::Ask`: it inserts a
    /// fresh oneshot under a generated `request_id`, emits
    /// `AgentEvent::InputRequired`, then awaits the oneshot. The
    /// daemon's dispatch handler for `session.respond_to_input_required`
    /// is responsible for taking the oneshot out and sending the
    /// decision.
    pub fn spawn(args: SpawnArgs) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let handle = tokio::spawn(run(args, rx));
        Self { tx, handle }
    }

    /// Push input. Returns `Err` with the input if the loop has terminated.
    pub async fn send(&self, input: AgentInput) -> Result<(), AgentInput> {
        self.tx.send(input).await.map_err(|e| e.0)
    }

    /// Clone the input sender. Used by the daemon's session manager
    /// to drive the loop without holding the AgentLoop value, so
    /// sync-locked map lookups can clone-and-drop the lock before
    /// awaiting on the send.
    pub fn sender(&self) -> mpsc::Sender<AgentInput> {
        self.tx.clone()
    }

    /// Wait for the loop to finish.
    ///
    /// As of mu-035 Phase A (multi-turn fix), the agent loop no
    /// longer terminates after one ask. It runs until its input
    /// channel closes (all senders dropped) or it receives Cancel.
    /// `join` therefore drops the owned `tx` BEFORE awaiting the
    /// handle, so the loop sees its sole sender close and exits
    /// cleanly. If the daemon's session manager holds a cloned
    /// sender (via `sender()`), the loop will wait for that to
    /// also drop — which is what we want: the session is alive as
    /// long as the daemon has a way to talk to it.
    pub async fn join(self) -> Outcome {
        let Self { tx, handle } = self;
        drop(tx); // close the owned input sender so the loop can exit
        handle
            .await
            .unwrap_or_else(|_| Outcome::Error("loop task panicked".into()))
    }
}

/// Outer loop entry: delegates to [`run_inner`] and then closes out
/// any receipt tickets still pending when the loop terminates without
/// a Done (spec mu-046 WP4, INV-4: every `CommandReceived` should gain
/// exactly one receipt). The main case is `cancel_session` /
/// channel-close mid-ask: `run_inner` returns `Outcome::Cancelled`
/// without emitting a Done, so we emit a synthetic terminal
/// `Done(Aborted)` (or `Done(Error)` for error outcomes) carrying the
/// pending tickets — the forwarder turns each into a `CommandFailed`
/// receipt. Loops with no pending tickets (every pre-mu-046 flow and
/// all direct loop tests) see no behavior change. Asks still queued
/// but never started when the loop dies remain orphans — the legible
/// crash marker INV-4 allows.
async fn run(args: SpawnArgs, input_rx: mpsc::Receiver<AgentInput>) -> Outcome {
    let events = args.events.clone();
    let mut pending_tickets: Vec<CommandTicket> = Vec::new();
    let outcome = run_inner(args, input_rx, &mut pending_tickets).await;
    if !pending_tickets.is_empty() {
        let stop_reason = match &outcome {
            Outcome::Error(_) => StopReason::Error,
            _ => StopReason::Aborted,
        };
        let _ = events
            .send(AgentEvent::Done {
                stop_reason,
                turn_count: 0,
                usage: None,
                elapsed_ms: None,
                command_receipts: std::mem::take(&mut pending_tickets),
            })
            .await;
    }
    outcome
}

async fn run_inner(
    args: SpawnArgs,
    mut input_rx: mpsc::Receiver<AgentInput>,
    pending_tickets: &mut Vec<CommandTicket>,
) -> Outcome {
    let SpawnArgs {
        provider,
        provider_kind,
        model,
        tools,
        config,
        events,
        pending_approvals,
        capability,
        live_context_soft_limit,
    } = args;
    let mut provider = provider;
    let mut current_provider_kind = provider_kind;
    let mut current_model = model;
    // mu-ub6q: the compaction-trigger output reservation for the model
    // currently in force. Seeded from the creation-time config and
    // updated on every `SwitchProvider` so a mid-session model switch
    // re-reserves the new model's output budget. The switch handler
    // updates its companion (the `live_context_soft_limit` atomic) in the
    // same step, so the two halves of the effective trigger never diverge.
    let mut current_max_output_tokens = config.max_output_tokens;
    // mu-vcbm: the session's standing reasoning effort. Seeded from the
    // launch-time default, then updated stickily whenever a UserMessage
    // arrives carrying a `/effort` change. Passed to `Provider::stream`
    // each turn; `None` ⇒ the provider's own construction-time default.
    let mut current_effort: Option<Arc<str>> = config.effort.clone();
    // mu-mh4: a resumed (forked) session starts mid-conversation, seeded
    // with the continuation projection of its predecessor's log; a fresh
    // session starts empty (the default — seed_messages is `Vec::new()`).
    let mut messages: Vec<AgentMessage> = config.seed_messages.clone();
    let mut queue: VecDeque<Action> = VecDeque::new();
    let mut turn_count: u32 = 0;
    // mu-rb4u: consecutive actionless (empty / reasoning-only) turns since
    // the last turn that produced a tool call or visible text. Reset at
    // ask start and on any non-actionless turn; bounds the empty-turn
    // auto-continue at `MAX_EMPTY_TURN_RETRIES`.
    let mut consecutive_empty_turns: u32 = 0;
    let mut mode: RunMode = RunMode::Idle;
    let mut aggregated_usage: Option<Usage> = None;
    let mut last_stop_reason: Option<StopReason> = None;
    let mut started_at: Option<Instant> = None;
    let mut tool_history = ToolHistory::default();
    let mut model_call_id: u32 = 0;

    let session_started_at = Instant::now();

    let mut bg_compaction =
        crate::context::BackgroundCompactionState::new(crate::context::CompactionQuota::default());
    let mut compaction_baseline: Option<CompactionBaseline> = None;
    // mu-uz0n: memoized capability hint for the current intent (the
    // last user-role message). Re-ranked only when the intent changes
    // — i.e. once per ask / autonomous iteration, not per tool round —
    // so the hint content (and rope position) is byte-stable within an
    // ask and the cacheable prefix is never disturbed.
    let mut capability_hint_memo: Option<(String, Option<String>)> = None;
    // mu-wsgx: feedback anchor for the compaction-trigger measure.
    // None until the first provider-reported usage; reset on provider
    // switch (different tokenizer + accounting convention).
    let mut feedback_anchor: Option<FeedbackAnchor> = None;

    let _ = events.send(AgentEvent::AgentStart).await;

    loop {
        while let Ok(input) = input_rx.try_recv() {
            match input {
                AgentInput::Cancel => return Outcome::Cancelled,
                AgentInput::CancelOutstanding { .. } => {}
                AgentInput::SwitchProvider {
                    provider: new,
                    provider_kind: new_kind,
                    model: new_model,
                    max_output_tokens: new_max_output,
                    context_soft_limit: new_soft,
                } => {
                    let old_kind: Arc<str> = Arc::from(current_provider_kind.as_ref());
                    let old_model: Arc<str> = Arc::from(current_model.as_ref());
                    provider = new;
                    current_provider_kind = new_kind.clone();
                    current_model = new_model.clone();
                    current_max_output_tokens = new_max_output;
                    // mu-ub6q: store the new soft limit in the SAME handler
                    // that sets the reservation, so the effective trigger
                    // never pairs a new reservation with the old soft limit.
                    live_context_soft_limit.store(new_soft, Ordering::Relaxed);
                    // mu-wsgx: the old provider's actuals don't
                    // transfer (different tokenizer + accounting).
                    feedback_anchor = None;
                    let _ = events
                        .send(AgentEvent::ProviderSwitched {
                            old_provider_kind: old_kind,
                            old_model,
                            new_provider_kind: new_kind,
                            new_model,
                            // mu-rf9x: re-register the accounting
                            // convention for the provider now in force.
                            usage_semantics: provider.capabilities().usage_semantics,
                        })
                        .await;
                }
                AgentInput::UserMessage(..)
                | AgentInput::StartAutonomous { .. }
                | AgentInput::ScheduleWakeup { .. }
                | AgentInput::WatchCompleted { .. }
                | AgentInput::DialogueMessage { .. }
                | AgentInput::MailboxMessage { .. } => {
                    queue.push_back(Action::External(input));
                }
            }
        }

        let action = if let Some(a) = queue.pop_front() {
            a
        } else {
            match input_rx.recv().await {
                Some(AgentInput::Cancel) => return Outcome::Cancelled,
                Some(AgentInput::CancelOutstanding { .. }) => {
                    continue;
                }
                Some(AgentInput::SwitchProvider {
                    provider: new,
                    provider_kind: new_kind,
                    model: new_model,
                    max_output_tokens: new_max_output,
                    context_soft_limit: new_soft,
                }) => {
                    let old_kind: Arc<str> = Arc::from(current_provider_kind.as_ref());
                    let old_model: Arc<str> = Arc::from(current_model.as_ref());
                    provider = new;
                    current_provider_kind = new_kind.clone();
                    current_model = new_model.clone();
                    current_max_output_tokens = new_max_output;
                    // mu-ub6q: store the new soft limit in the SAME handler
                    // that sets the reservation, so the effective trigger
                    // never pairs a new reservation with the old soft limit.
                    live_context_soft_limit.store(new_soft, Ordering::Relaxed);
                    // mu-wsgx: the old provider's actuals don't
                    // transfer (different tokenizer + accounting).
                    feedback_anchor = None;
                    let _ = events
                        .send(AgentEvent::ProviderSwitched {
                            old_provider_kind: old_kind,
                            old_model,
                            new_provider_kind: new_kind,
                            new_model,
                            // mu-rf9x: re-register the accounting
                            // convention for the provider now in force.
                            usage_semantics: provider.capabilities().usage_semantics,
                        })
                        .await;
                    continue;
                }
                Some(input) => Action::External(input),
                None => break,
            }
        };

        match action {
            Action::External(AgentInput::UserMessage(msg, ticket, effort)) => {
                // spec mu-046 WP4: a journaled ask's ticket joins the
                // current ask's pending set; it is drained into the
                // terminal Done below (back-to-back asks that share
                // one LLM call share one Done — each ticket still
                // gets its own receipt, paired to that Done).
                if let Some(t) = ticket {
                    pending_tickets.push(*t);
                }
                // mu-vcbm: a `/effort` change rides in with the ask and
                // updates the standing effort stickily — it stays in
                // force for this and subsequent turns until changed.
                if let Some(e) = effort {
                    current_effort = Some(e);
                }
                let _ = events
                    .send(AgentEvent::MessageStart {
                        message: msg.clone(),
                    })
                    .await;
                messages.push(msg.clone());
                let _ = events.send(AgentEvent::MessageEnd { message: msg }).await;
                if should_push_invoke_llm(&queue) {
                    queue.push_back(Action::InvokeLlm);
                }
            }
            Action::External(AgentInput::Cancel) => {
                return Outcome::Cancelled;
            }
            Action::External(AgentInput::CancelOutstanding { .. }) => {
                continue;
            }
            Action::External(AgentInput::SwitchProvider { .. }) => {
                // Already handled in try_recv/recv above; should not
                // reach the action queue. No-op if it somehow does.
            }
            Action::External(AgentInput::MailboxMessage {
                from_session_id,
                message_kind,
                subject,
                seq,
            }) => {
                let notification = format!(
                    "[Mailbox] New {message_kind} message (seq {seq}) from session {from_session_id}: {subject}\n\
                     Read it with mu_mailbox_read, then act on it."
                );
                let msg = AgentMessage::User {
                    content: notification,
                };
                let _ = events
                    .send(AgentEvent::MessageStart {
                        message: msg.clone(),
                    })
                    .await;
                messages.push(msg.clone());
                let _ = events.send(AgentEvent::MessageEnd { message: msg }).await;
                if should_push_invoke_llm(&queue) {
                    queue.push_back(Action::InvokeLlm);
                }
            }
            Action::External(AgentInput::WatchCompleted { note, summary }) => {
                // mu-watch-tool-wakeup-o03p: a watched command finished.
                // Inject the result as a User message and run the LLM —
                // the same "external attention wakes an idle session"
                // shape as MailboxMessage, but the result is carried
                // INLINE (no go-read-your-mailbox indirection) so it
                // lands directly as the woken turn's motivation. Works
                // whether the session was idle (parked on input_rx.recv)
                // or mid-autonomous-run (queued behind the current work).
                let notification =
                    format!("[Watch] '{note}' finished.\n{summary}\n\nAct on this result.");
                let msg = AgentMessage::User {
                    content: notification,
                };
                let _ = events
                    .send(AgentEvent::MessageStart {
                        message: msg.clone(),
                    })
                    .await;
                messages.push(msg.clone());
                let _ = events.send(AgentEvent::MessageEnd { message: msg }).await;
                if should_push_invoke_llm(&queue) {
                    queue.push_back(Action::InvokeLlm);
                }
            }
            Action::External(AgentInput::DialogueMessage { from, content }) => {
                // mu-dialogue-inbound-wakeup: an inbound dialogue message
                // arrived for this session. Inject it as a User message and
                // run the LLM — same "external attention wakes a session"
                // shape as WatchCompleted, carried INLINE so the message
                // lands directly as the woken turn's motivation. The model
                // sees who spoke and what they said, plus a nudge that it
                // may reply on the same channel.
                let notification = format!(
                    "[Dialogue] {from}: {content}\n\n\
                     You may reply with dialogue_say if appropriate."
                );
                let msg = AgentMessage::User {
                    content: notification,
                };
                let _ = events
                    .send(AgentEvent::MessageStart {
                        message: msg.clone(),
                    })
                    .await;
                messages.push(msg.clone());
                let _ = events.send(AgentEvent::MessageEnd { message: msg }).await;
                if should_push_invoke_llm(&queue) {
                    queue.push_back(Action::InvokeLlm);
                }
            }
            Action::External(AgentInput::StartAutonomous { goal, options }) => {
                let autonomy_snapshot = capability
                    .lock()
                    .ok()
                    .map(|c| c.autonomy.clone())
                    .unwrap_or(AutonomyCapability::Disallowed);
                let (max_iterations, max_wall_clock_ms, max_total_tool_calls) =
                    match autonomy_snapshot {
                        AutonomyCapability::Allowed {
                            max_iterations,
                            max_wall_clock_ms,
                            max_total_tool_calls_in_autonomy,
                            ..
                        } => (
                            max_iterations,
                            max_wall_clock_ms,
                            max_total_tool_calls_in_autonomy,
                        ),
                        AutonomyCapability::Disallowed => {
                            let _ = events
                                .send(AgentEvent::Callout {
                                    category: "warning".to_owned(),
                                    title: "start_autonomous refused".to_owned(),
                                    body: serde_json::json!({
                                        "reason": "autonomy: Disallowed (INV-1)",
                                    }),
                                    theme: Some("warning".to_owned()),
                                    context_refs: vec!["spec:mu-036".to_owned()],
                                })
                                .await;
                            continue;
                        }
                    };

                let effective_max_iterations = options
                    .max_iterations
                    .map(|o| o.min(max_iterations))
                    .unwrap_or(max_iterations);

                mode = RunMode::Autonomous {
                    iteration: 1,
                    goal: goal.clone(),
                    options: options.clone(),
                    started_at: Instant::now(),
                    tool_calls_consumed: 0,
                };

                if let RunMode::Autonomous {
                    iteration,
                    goal: g,
                    options: opts,
                    started_at,
                    tool_calls_consumed,
                } = &mode
                {
                    let _ = effective_max_iterations;
                    let _ = (iteration, g, opts, started_at, tool_calls_consumed);
                }

                let _ = events
                    .send(AgentEvent::AutonomousIterationStarted {
                        iteration: 1,
                        motivation: format!("Autonomous goal: {goal}"),
                    })
                    .await;

                let goal_msg = AgentMessage::User { content: goal };
                let _ = events
                    .send(AgentEvent::MessageStart {
                        message: goal_msg.clone(),
                    })
                    .await;
                messages.push(goal_msg.clone());
                let _ = events
                    .send(AgentEvent::MessageEnd { message: goal_msg })
                    .await;
                queue.push_back(Action::InvokeLlm);
                let _ = (max_wall_clock_ms, max_total_tool_calls);
            }
            Action::External(AgentInput::ScheduleWakeup {
                wake_at_unix_ms,
                reason,
            }) => {
                // mu-036 Phase C (mu-7zn). schedule_wakeup is only
                // meaningful mid-autonomous-run: the spec frames
                // Sleeping as a sub-state of an autonomous loop ("on
                // wake, return to RunMode::Autonomous"). Outside that
                // context there is no iteration to resume, so decline
                // with a warning callout rather than invent a new
                // "sleep then idle" semantic (spec-boundary discipline).
                // The dispatch handler already gates the capability.
                let (iteration, goal, options, auto_started_at, tool_calls_consumed) = match &mode {
                    RunMode::Autonomous {
                        iteration,
                        goal,
                        options,
                        started_at,
                        tool_calls_consumed,
                    } => (
                        *iteration,
                        goal.clone(),
                        options.clone(),
                        *started_at,
                        *tool_calls_consumed,
                    ),
                    _ => {
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "warning".to_owned(),
                                title: "schedule_wakeup ignored".to_owned(),
                                body: serde_json::json!({
                                    "reason": "session is not in autonomous mode; \
                                               schedule_wakeup is only honored mid-run",
                                }),
                                theme: Some("warning".to_owned()),
                                context_refs: vec!["spec:mu-036".to_owned()],
                            })
                            .await;
                        continue;
                    }
                };

                // Resolve the wall-clock wake target to a monotonic
                // deadline. `wake_at_unix_ms` is already absolute (the
                // daemon resolved any `sleep_for_ms` before sending).
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let sleep_ms = wake_at_unix_ms.saturating_sub(now_ms);
                let wake_at = Instant::now() + Duration::from_millis(sleep_ms);

                // INV-6: bracket the current iteration with a Completed
                // before parking (the iteration's chosen action was "go
                // to sleep"; it continues logically as N+1 on wake).
                let _ = events
                    .send(AgentEvent::AutonomousIterationCompleted {
                        iteration,
                        outcome: AutonomousIterationOutcome::Continue,
                    })
                    .await;
                // Durable park marker (INV-5 / audit trail). No wire
                // notification — not part of mu-036's wire surface.
                let _ = events
                    .send(AgentEvent::AutonomousScheduledWakeup {
                        wake_at_unix_ms,
                        reason: reason.clone(),
                    })
                    .await;

                mode = RunMode::Sleeping {
                    wake_at,
                    reason: reason.clone(),
                    iteration,
                    goal,
                    options,
                    started_at: auto_started_at,
                    tool_calls_consumed,
                };

                // Park: hold a tokio sleep future. While here we make no
                // provider or tool calls (INV-5). A Cancel still
                // terminates promptly; other inputs are buffered and
                // replayed after wake. Long sleeps are the whole point
                // (overnight watchdogs), so the sleep stays cancellable.
                let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at));
                tokio::pin!(sleep);
                let mut buffered: Vec<AgentInput> = Vec::new();
                let cancelled = loop {
                    tokio::select! {
                        _ = &mut sleep => break false,
                        maybe = input_rx.recv() => match maybe {
                            Some(AgentInput::Cancel) => break true,
                            Some(AgentInput::CancelOutstanding { .. }) => {}
                            Some(other) => buffered.push(other),
                            None => break true, // channel closed → shut down
                        }
                    }
                };

                if cancelled {
                    let _ = events
                        .send(AgentEvent::AutonomousTerminated {
                            reason: AutonomousTerminationReason::Cancelled,
                        })
                        .await;
                    return Outcome::Cancelled;
                }

                // Woke naturally. Recover the suspended autonomous
                // context from the Sleeping mode.
                let (iteration, goal, options, auto_started_at, tool_calls_consumed) = match mode {
                    RunMode::Sleeping {
                        iteration,
                        goal,
                        options,
                        started_at,
                        tool_calls_consumed,
                        ..
                    } => (iteration, goal, options, started_at, tool_calls_consumed),
                    _ => unreachable!("mode is Sleeping inside the wakeup handler"),
                };

                // INV-2 / INV-5: re-check bounds on wake. Wall-clock
                // kept accruing while we slept, so a session that slept
                // past max_wall_clock_ms terminates here rather than
                // running another iteration.
                let next_iter = iteration.saturating_add(1);
                let (cap_max_iter, cap_max_wall, cap_max_tools) = {
                    let cap = capability.lock().ok();
                    match cap.as_ref().map(|c| c.autonomy.clone()) {
                        Some(AutonomyCapability::Allowed {
                            max_iterations,
                            max_wall_clock_ms,
                            max_total_tool_calls_in_autonomy,
                            ..
                        }) => (
                            max_iterations,
                            max_wall_clock_ms,
                            max_total_tool_calls_in_autonomy,
                        ),
                        _ => (0, 0, 0),
                    }
                };
                let effective_max_iter = options
                    .max_iterations
                    .map(|o| o.min(cap_max_iter))
                    .unwrap_or(cap_max_iter);
                let elapsed_ms_total = auto_started_at.elapsed().as_millis() as u64;
                let terminal_reason: Option<AutonomousTerminationReason> =
                    if next_iter > effective_max_iter {
                        Some(AutonomousTerminationReason::IterationCap)
                    } else if elapsed_ms_total >= cap_max_wall {
                        Some(AutonomousTerminationReason::WallClockExpired)
                    } else if tool_calls_consumed >= cap_max_tools {
                        Some(AutonomousTerminationReason::ToolCallCapExhausted)
                    } else {
                        None
                    };

                // Replay inputs buffered during the sleep so nothing is
                // lost, whether we resume or terminate.
                for input in buffered {
                    queue.push_back(Action::External(input));
                }

                if let Some(reason_term) = terminal_reason {
                    let _ = events
                        .send(AgentEvent::AutonomousTerminated {
                            reason: reason_term,
                        })
                        .await;
                    mode = RunMode::Idle;
                    let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                    let stop_reason = last_stop_reason.take().unwrap_or(StopReason::EndTurn);
                    let _ = events
                        .send(AgentEvent::Done {
                            stop_reason,
                            turn_count,
                            usage: aggregated_usage.take(),
                            elapsed_ms,
                            command_receipts: std::mem::take(pending_tickets),
                        })
                        .await;
                    started_at = None;
                    turn_count = 0;
                    tool_history.clear();
                    continue;
                }

                // Resume the autonomous run at iteration N+1, injecting
                // the wake reason as the next iteration's motivation
                // (spec §"Wake-up scheduling").
                mode = RunMode::Autonomous {
                    iteration: next_iter,
                    goal,
                    options,
                    started_at: auto_started_at,
                    tool_calls_consumed,
                };
                let _ = events
                    .send(AgentEvent::AutonomousIterationStarted {
                        iteration: next_iter,
                        motivation: reason.clone(),
                    })
                    .await;
                let wake_msg = AgentMessage::User { content: reason };
                let _ = events
                    .send(AgentEvent::MessageStart {
                        message: wake_msg.clone(),
                    })
                    .await;
                messages.push(wake_msg.clone());
                let _ = events
                    .send(AgentEvent::MessageEnd { message: wake_msg })
                    .await;
                queue.push_back(Action::InvokeLlm);
            }
            Action::InvokeLlm => {
                // mu-779s: iteration cap check with progressive warnings
                // and dynamic cap (None = disabled)
                let reserved_turns = 2u32;
                let (warning_threshold, cap_enabled) = if let Some(max) = config.max_turns {
                    (max.saturating_sub(reserved_turns), true)
                } else {
                    (0, false)
                };

                // Emit progressive warnings at 75% and 90% of cap
                // Only warn if the cap is enabled and large enough to have meaningful thresholds
                if cap_enabled && warning_threshold > 0 {
                    let warning_threshold_75 = (warning_threshold as f64 * 0.75).ceil() as u32;
                    let warning_threshold_90 = (warning_threshold as f64 * 0.90).ceil() as u32;

                    if turn_count >= warning_threshold_90 && turn_count < warning_threshold_90 + 1 {
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "warning".to_owned(),
                                title: "iteration cap approaching".to_owned(),
                                body: serde_json::json!({
                                    "turn_count": turn_count,
                                    "warning_threshold": warning_threshold,
                                    "reserved_turns": reserved_turns,
                                    "reason": "90% of turn budget exhausted"
                                }),
                                theme: Some("warning".to_owned()),
                                context_refs: vec!["spec:mu-003".to_owned()],
                            })
                            .await;
                    } else if turn_count >= warning_threshold_75
                        && turn_count < warning_threshold_75 + 1
                    {
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "info".to_owned(),
                                title: "iteration cap approaching".to_owned(),
                                body: serde_json::json!({
                                    "turn_count": turn_count,
                                    "warning_threshold": warning_threshold,
                                    "reserved_turns": reserved_turns,
                                    "reason": "75% of turn budget exhausted"
                                }),
                                theme: Some("info".to_owned()),
                                context_refs: vec!["spec:mu-003".to_owned()],
                            })
                            .await;
                    }
                }

                // Check cap (only if enabled)
                // mu-779s: Some(0) means disable cap (per protocol doc).
                // Only cap if Some(n) where n > 0.
                if config.max_turns.is_some_and(|n| n > 0)
                    && turn_count >= config.max_turns.unwrap_or(0)
                {
                    // mu-779s: distinguish iteration-cap exit from natural
                    // end_turn so downstream consumers (TUI, transcript,
                    // telemetry) can surface "turn budget exhausted" rather
                    // than reporting a normal conversation end.
                    let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                    let _ = events
                        .send(AgentEvent::Done {
                            stop_reason: StopReason::IterationCap,
                            turn_count,
                            usage: aggregated_usage.take(),
                            elapsed_ms,
                            command_receipts: std::mem::take(pending_tickets),
                        })
                        .await;
                    started_at = None;
                    turn_count = 0;
                    tool_history.clear();
                    last_stop_reason = None;
                    queue.clear();
                    continue;
                }
                if started_at.is_none() {
                    started_at = Some(Instant::now());
                    // mu-rb4u: a fresh ask gets a fresh empty-turn budget.
                    consecutive_empty_turns = 0;
                }
                turn_count += 1;
                let _ = events.send(AgentEvent::TurnStart).await;

                let tool_specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();

                let renderer = provider.renderer();
                let cache_strategy = provider.cache_strategy();

                if let Some(Some(complete)) = bg_compaction.try_take().await {
                    {
                        let policy_label = provider.compaction_policy().policy_label().to_owned();
                        let tokens_after = renderer.estimate_tokens(&complete.result.rope);
                        let _ = events
                            .send(AgentEvent::CompactionAssembly {
                                model_call_id: model_call_id + 1,
                                policy_id: policy_label,
                                tokens_before: complete.result.tokens_before,
                                tokens_after,
                                decisions: complete.result.decisions.clone(),
                                wall_clock_us: complete.result.wall_clock_us,
                                // mu-a79g: the trigger captured when this
                                // bg compaction was SPAWNED (a prior turn),
                                // not this turn's values — replayed off the
                                // completion so the event describes the
                                // compaction that actually ran.
                                predicted_tokens: complete.trigger.predicted_tokens,
                                compaction_threshold: complete.trigger.compaction_threshold,
                                output_reserve: complete.trigger.output_reserve,
                            })
                            .await;
                        compaction_baseline = Some(CompactionBaseline {
                            rope: complete.result.rope,
                            messages_at_spawn: complete.messages_at_spawn,
                        });
                    }
                }

                let rope: RetainedRope = match &compaction_baseline {
                    Some(b) => crate::context::append_messages_to_baseline(
                        &b.rope,
                        b.messages_at_spawn,
                        &messages,
                    ),
                    None => crate::context::assemble_rope_with_context(
                        config.system_prompt.as_deref(),
                        config.project_context.as_ref(),
                        &messages,
                        &tool_specs,
                    ),
                };

                // mu-uz0n: implicit capability discovery — rank the
                // current intent against the session's capability
                // surface and inject the top-N as a compact transient
                // span after the last user span. Memoized per intent
                // (see `capability_hint_memo`); covers both assembly
                // paths above since it post-processes the rope.
                let rope: RetainedRope = match &config.discover_hints {
                    Some(hints) => {
                        let intent = messages.iter().rev().find_map(|m| match m {
                            AgentMessage::User { content } => Some(content.as_str()),
                            _ => None,
                        });
                        match intent {
                            Some(intent) => {
                                let stale = capability_hint_memo
                                    .as_ref()
                                    .is_none_or(|(memo_intent, _)| memo_intent != intent);
                                if stale {
                                    let snapshot =
                                        capability.lock().map(|c| c.clone()).unwrap_or_default();
                                    let hint = crate::context::capability_hints::rank_hint(
                                        &tools,
                                        &snapshot,
                                        &hints.skills,
                                        intent,
                                        hints.limit,
                                    );
                                    capability_hint_memo = Some((intent.to_owned(), hint));
                                }
                                match capability_hint_memo
                                    .as_ref()
                                    .and_then(|(_, h)| h.as_deref())
                                {
                                    Some(hint) => {
                                        crate::context::capability_hints::with_hint_after_last_user(
                                            &rope, hint,
                                        )
                                    }
                                    None => rope,
                                }
                            }
                            None => rope,
                        }
                    }
                    None => rope,
                };

                let pre_compaction_tokens = renderer.estimate_tokens(&rope);
                // mu-context-limits-wire phase 2: read the LIVE soft limit
                // from the shared handle (the daemon's session.set_config
                // updates it), not the spawn-time config, so a mid-session
                // change takes effect. `0` ⇒ unset → fall back to the
                // spawn config, then DEFAULT_COMPACTION_THRESHOLD.
                let compaction_threshold = match live_context_soft_limit.load(Ordering::Relaxed) {
                    0 => config
                        .compaction_threshold
                        .unwrap_or(DEFAULT_COMPACTION_THRESHOLD),
                    n => n as usize,
                };
                // mu-wsgx: the trigger compares a feedback-predicted
                // prompt total, not the raw estimate. The incident
                // measure (session c76f6949): raw estimate ran ~15%
                // low (uncounted request framing + chars/4 vs BPE),
                // so the 150K trigger actually fired at ~176K provider
                // tokens — 88% of the window. The predictor anchors on
                // the provider's exact accounting and only estimates
                // the delta.
                let predicted_tokens =
                    predicted_prompt_total(feedback_anchor.as_ref(), pre_compaction_tokens);
                // mu-ub6q: honor the provider's send constraint
                // `input + output ≤ window`. The next request sends
                // `predicted_tokens` of input and lets the model generate
                // up to its output budget on top, so compact while the
                // input still leaves room — fire when
                // `predicted + reserve > threshold`. Without this a soft
                // limit at the window is unreachable: input+output
                // overflows and the provider errors mid-stream before the
                // input-only total can cross `compaction_threshold`, so
                // compaction never runs. (The rope itself can hold more
                // than the window; only what we SEND is constrained.)
                //
                // Cap the reservation at the compaction target
                // (`threshold/2`): some models report an output budget as
                // large as their whole window (e.g. ollama qwen/glm:
                // max_output == context). Reserving that verbatim drives
                // the effective trigger to 0 — compact-every-turn with no
                // input budget, a dead session. The target is the most
                // compaction can recover to, so a reservation beyond it
                // can never be satisfied anyway; clamping there keeps a
                // working budget (effective ≥ threshold/2 > 0) and leaves
                // the common case (max_output ≪ threshold) untouched.
                let target_tokens = compaction_threshold / 2;
                let output_reserve = current_max_output_tokens.min(target_tokens);
                let effective_threshold = compaction_threshold.saturating_sub(output_reserve);
                // mu-a79g: capture the trigger inputs so the emitted
                // CompactionAssembly is self-describing — for the async
                // path these ride through BgCompaction to the later emit
                // turn (the values aren't in scope there).
                let compaction_trigger = CompactionTrigger {
                    predicted_tokens,
                    compaction_threshold,
                    output_reserve,
                };
                let rope = if predicted_tokens > effective_threshold {
                    let policy = config
                        .compaction_policy_override
                        .clone()
                        .unwrap_or_else(|| provider.compaction_policy());
                    // `target_tokens` (the post-compaction goal) is
                    // computed above and shared with the reservation cap.
                    if policy.is_async() && bg_compaction.can_start() {
                        bg_compaction.start(
                            policy.clone(),
                            rope.clone(),
                            target_tokens,
                            messages.len(),
                            compaction_trigger,
                        );
                        rope
                    } else {
                        // mu-mu-solo-loop-terminate-5ek5: the sync
                        // compact runs INLINE in the loop task, so a
                        // panicking policy (e.g. tiktoken over a
                        // pathological span in the 2026-06-07 incident)
                        // would unwind the whole loop — input channel
                        // closed, every later ask -32603. The async
                        // path already tolerates panics (JoinError →
                        // "no compaction this round"); give the inline
                        // path the same contract: panic → continue
                        // with the un-compacted rope, loudly.
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            policy.compact(&rope, target_tokens)
                        })) {
                            Ok(result) => {
                                let _ = events
                                    .send(AgentEvent::CompactionAssembly {
                                        model_call_id: model_call_id + 1,
                                        policy_id: policy.policy_label().to_owned(),
                                        tokens_before: pre_compaction_tokens,
                                        tokens_after: renderer.estimate_tokens(&result.rope),
                                        decisions: result.decisions.clone(),
                                        wall_clock_us: result.wall_clock_us,
                                        // mu-a79g: this turn's trigger,
                                        // in scope on the sync path.
                                        predicted_tokens: compaction_trigger.predicted_tokens,
                                        compaction_threshold: compaction_trigger
                                            .compaction_threshold,
                                        output_reserve: compaction_trigger.output_reserve,
                                    })
                                    .await;
                                compaction_baseline = Some(CompactionBaseline {
                                    rope: result.rope.clone(),
                                    messages_at_spawn: messages.len(),
                                });
                                result.rope
                            }
                            Err(panic) => {
                                let _ = events
                                    .send(AgentEvent::Callout {
                                        category: "warning".to_owned(),
                                        title: "compaction policy panicked".to_owned(),
                                        body: serde_json::json!({
                                            "policy": policy.policy_label(),
                                            "panic": execute_tools::panic_message(
                                                panic.as_ref()
                                            ),
                                        }),
                                        theme: Some("warning".to_owned()),
                                        context_refs: vec![
                                            "bead:mu-mu-solo-loop-terminate-5ek5".to_owned()
                                        ],
                                    })
                                    .await;
                                rope
                            }
                        }
                    }
                } else {
                    rope
                };

                let mut projection: ProviderMessages =
                    renderer.render(&rope, ProjectionTarget::AgentView);
                let cache_boundaries = cache_strategy.boundaries(&rope);
                cache_strategy.annotate(&mut projection, &cache_boundaries);
                // mu-814o: digest the cacheable prefix so a full
                // cache invalidation is diagnosable from the JSONL —
                // which call, which span, or renderer drift.
                let (prefix_hash, prefix_span_hashes) =
                    crate::context::prefix_forensics(&projection, &cache_boundaries, &rope);
                let span_count = rope.len() as u32;
                let cache_boundary_count = cache_boundaries.len() as u32;
                let first_span_ids: Vec<String> = rope
                    .spans()
                    .iter()
                    .take(5)
                    .map(|s| s.id.to_string())
                    .collect();
                let provider_label = provider.provider_label().to_owned();

                model_call_id += 1;
                let (user_count, assistant_count, tool_result_count) =
                    count_message_roles(&messages);
                // mu-heqf: size the rope actually being rendered for
                // this call (post-compaction when one ran) section by
                // section, on the renderer's own token scale.
                let context_sizes = renderer.context_sizes(&rope);
                // mu-wsgx: remember what this call's rope estimated
                // at, so the provider's actual for THIS call can be
                // paired with it when usage arrives below.
                let rope_estimate_sent = context_sizes.total as usize;
                let _ = events
                    .send(AgentEvent::ContextAssembly {
                        model_call_id,
                        message_count: messages.len() as u32,
                        user_message_count: user_count,
                        assistant_message_count: assistant_count,
                        tool_result_count,
                        tool_count: tool_specs.len() as u32,
                        context_sizes: Some(context_sizes),
                        renderer: Some(provider_label.clone()),
                        cache_strategy: Some(provider_label),
                        span_count: Some(span_count),
                        cache_boundary_count: Some(cache_boundary_count),
                        first_span_ids,
                        prefix_hash,
                        prefix_span_hashes,
                    })
                    .await;

                let effective_system_prompt = build_effective_system_prompt(
                    config.system_prompt.as_deref(),
                    &session_started_at,
                );

                match handle_invoke_llm(
                    provider.as_ref(),
                    effective_system_prompt.as_deref(),
                    current_effort.as_deref(),
                    &projection,
                    &tool_specs,
                    &mut input_rx,
                    &events,
                )
                .await
                {
                    Ok((assistant_msg, buffered)) => {
                        if let Some(u) = assistant_msg.usage {
                            aggregated_usage = Some(match aggregated_usage {
                                Some(prev) => prev + u,
                                None => u,
                            });
                            // mu-wsgx: re-anchor the trigger predictor
                            // on this call's exact prompt total. None
                            // when the provider's accounting convention
                            // is unknown AND cache buckets make the
                            // total ambiguous — then the previous
                            // anchor (or fallback) stays in force.
                            if let Some(total) =
                                provider.capabilities().usage_semantics.prompt_total(&u)
                            {
                                feedback_anchor = Some(FeedbackAnchor {
                                    actual_prompt_total: total,
                                    rope_estimate: rope_estimate_sent,
                                });
                            }
                        }
                        // mu-rb4u: a codex/gpt-5.5 reasoning-only
                        // completion arrives as an actionless turn (no tool
                        // call, no text). plan_post_invoke_llm would route
                        // it to MaybeFinish, ending the ask and forcing the
                        // operator to type "continue". Treat it as a
                        // transient hiccup: drop the empty turn (don't
                        // pollute history) and re-invoke — bounded by
                        // MAX_EMPTY_TURN_RETRIES, backstopped by max_turns.
                        // Skipped when a buffered UserMessage is waiting:
                        // that's the operator's own next ask; let it run.
                        if is_actionless_turn(&assistant_msg)
                            && buffered.is_empty()
                            && consecutive_empty_turns < MAX_EMPTY_TURN_RETRIES
                        {
                            consecutive_empty_turns += 1;
                            let _ = events
                                .send(AgentEvent::Callout {
                                    category: "warning".to_owned(),
                                    title: "empty model turn — auto-continuing".to_owned(),
                                    body: serde_json::json!({
                                        "provider": provider.provider_label(),
                                        "stop_reason": format!("{:?}", assistant_msg.stop_reason),
                                        "attempt": consecutive_empty_turns,
                                        "max_attempts": MAX_EMPTY_TURN_RETRIES,
                                        "reason": "model returned no tool call and no text; \
                                                   re-invoking instead of ending the ask",
                                    }),
                                    theme: Some("warning".to_owned()),
                                    context_refs: vec!["bead:mu-rb4u".to_owned()],
                                })
                                .await;
                            queue.push_back(Action::InvokeLlm);
                        } else {
                            // Only the true budget-exhaustion case is a
                            // give-up worth surfacing. An actionless turn
                            // with a buffered UM waiting isn't exhaustion —
                            // we end the ask precisely so the operator's
                            // queued message runs next; no callout needed.
                            if is_actionless_turn(&assistant_msg)
                                && consecutive_empty_turns >= MAX_EMPTY_TURN_RETRIES
                            {
                                let _ = events
                                    .send(AgentEvent::Callout {
                                        category: "warning".to_owned(),
                                        title: "empty model turns persisted — ending ask"
                                            .to_owned(),
                                        body: serde_json::json!({
                                            "provider": provider.provider_label(),
                                            "consecutive_empty_turns": consecutive_empty_turns,
                                            "max_attempts": MAX_EMPTY_TURN_RETRIES,
                                        }),
                                        theme: Some("warning".to_owned()),
                                        context_refs: vec!["bead:mu-rb4u".to_owned()],
                                    })
                                    .await;
                            }
                            consecutive_empty_turns = 0;
                            last_stop_reason = Some(assistant_msg.stop_reason);
                            let assistant = AgentMessage::Assistant(assistant_msg.clone());
                            let _ = events
                                .send(AgentEvent::MessageStart {
                                    message: assistant.clone(),
                                })
                                .await;
                            messages.push(assistant.clone());
                            let _ = events
                                .send(AgentEvent::MessageEnd { message: assistant })
                                .await;

                            let plan = plan_post_invoke_llm(&assistant_msg, buffered);
                            if plan.emit_turn_end {
                                let _ = events.send(AgentEvent::TurnEnd).await;
                            }
                            for action in plan.actions {
                                queue.push_back(action);
                            }
                        }
                    }
                    Err(Outcome::OutstandingCancelled { reason }) => {
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "info".into(),
                                title: "outstanding call cancelled".into(),
                                body: serde_json::json!({ "reason": reason }),
                                theme: Some("info".into()),
                                context_refs: vec!["spec:mu-035".into()],
                            })
                            .await;
                        let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                        let _ = events
                            .send(AgentEvent::Done {
                                stop_reason: StopReason::Aborted,
                                turn_count,
                                usage: aggregated_usage.take(),
                                elapsed_ms,
                                command_receipts: std::mem::take(pending_tickets),
                            })
                            .await;
                        started_at = None;
                        turn_count = 0;
                        tool_history.clear();
                        last_stop_reason = None;
                        queue.clear();
                        continue;
                    }
                    Err(Outcome::Error(m)) => {
                        let _ = events.send(AgentEvent::Error { message: m }).await;
                        let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                        let _ = events
                            .send(AgentEvent::Done {
                                stop_reason: StopReason::Error,
                                turn_count,
                                usage: aggregated_usage.take(),
                                elapsed_ms,
                                command_receipts: std::mem::take(pending_tickets),
                            })
                            .await;
                        started_at = None;
                        turn_count = 0;
                        tool_history.clear();
                        last_stop_reason = None;
                        queue.clear();
                        continue;
                    }
                    Err(outcome) => {
                        return outcome;
                    }
                }
            }
            Action::ExecuteTools(calls) => {
                match handle_execute_tools(
                    &tools,
                    calls,
                    &mut input_rx,
                    &events,
                    &mut tool_history,
                    &pending_approvals,
                    &capability,
                )
                .await
                {
                    Ok(ExecuteToolsExit::Completed {
                        tool_messages,
                        buffered,
                    }) => {
                        if let RunMode::Autonomous {
                            tool_calls_consumed,
                            ..
                        } = &mut mode
                        {
                            *tool_calls_consumed =
                                tool_calls_consumed.saturating_add(tool_messages.len() as u32);
                        }
                        for r in tool_messages {
                            messages.push(r);
                        }
                        let _ = events.send(AgentEvent::TurnEnd).await;
                        for action in plan_post_execute_tools(buffered) {
                            queue.push_back(action);
                        }
                    }
                    Ok(ExecuteToolsExit::OutstandingCancelled {
                        reason,
                        tool_messages,
                    }) => {
                        // Even though the ask is aborted, keep the conversation
                        // history structurally complete: every assistant
                        // tool_call now has a synthetic is_error ToolResult.
                        // Otherwise the next OpenAI/Codex request is an invalid
                        // continuation ("No tool output found for function call").
                        for r in tool_messages {
                            messages.push(r);
                        }
                        let _ = events
                            .send(AgentEvent::Callout {
                                category: "info".into(),
                                title: "outstanding call cancelled".into(),
                                body: serde_json::json!({ "reason": reason }),
                                theme: Some("info".into()),
                                context_refs: vec!["spec:mu-035".into()],
                            })
                            .await;
                        let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                        let _ = events
                            .send(AgentEvent::Done {
                                stop_reason: StopReason::Aborted,
                                turn_count,
                                usage: aggregated_usage.take(),
                                elapsed_ms,
                                command_receipts: std::mem::take(pending_tickets),
                            })
                            .await;
                        started_at = None;
                        turn_count = 0;
                        tool_history.clear();
                        last_stop_reason = None;
                        queue.clear();
                        continue;
                    }
                    Ok(ExecuteToolsExit::Cancelled { tool_messages }) => {
                        for r in tool_messages {
                            messages.push(r);
                        }
                        return Outcome::Cancelled;
                    }
                    Err(outcome) => {
                        if let Outcome::Error(ref m) = outcome {
                            let _ = events.send(AgentEvent::Error { message: m.clone() }).await;
                        }
                        return outcome;
                    }
                }
            }
            Action::MaybeFinish => {
                while let Ok(input) = input_rx.try_recv() {
                    match input {
                        AgentInput::Cancel => return Outcome::Cancelled,
                        AgentInput::CancelOutstanding { .. } => {}
                        AgentInput::SwitchProvider {
                            provider: new,
                            provider_kind: new_kind,
                            model: new_model,
                            max_output_tokens: new_max_output,
                            context_soft_limit: new_soft,
                        } => {
                            let old_kind: Arc<str> = Arc::from(current_provider_kind.as_ref());
                            let old_model: Arc<str> = Arc::from(current_model.as_ref());
                            provider = new;
                            current_provider_kind = new_kind.clone();
                            current_model = new_model.clone();
                            current_max_output_tokens = new_max_output;
                            // mu-ub6q: store the new soft limit in the
                            // same handler that sets the reservation (see
                            // the sibling switch paths above).
                            live_context_soft_limit.store(new_soft, Ordering::Relaxed);
                            let _ = events
                                .send(AgentEvent::ProviderSwitched {
                                    old_provider_kind: old_kind,
                                    old_model,
                                    new_provider_kind: new_kind,
                                    new_model,
                                    // mu-rf9x: re-register the accounting
                                    // convention for the provider now in force.
                                    usage_semantics: provider.capabilities().usage_semantics,
                                })
                                .await;
                        }
                        AgentInput::UserMessage(..)
                        | AgentInput::StartAutonomous { .. }
                        | AgentInput::ScheduleWakeup { .. }
                        | AgentInput::WatchCompleted { .. }
                        | AgentInput::DialogueMessage { .. }
                        | AgentInput::MailboxMessage { .. } => {
                            queue.push_back(Action::External(input));
                        }
                    }
                }

                if let RunMode::Autonomous { .. } = &mode {
                    // Autonomous mode owns its own continuation /
                    // termination semantics: queued input defers the
                    // goal-check to the next MaybeFinish, exactly as
                    // before (a per-iteration continuation must NOT
                    // emit Done).
                    if !queue.is_empty() {
                        continue;
                    }
                    let (
                        current_iteration,
                        current_options,
                        current_started_at,
                        current_tool_calls,
                    ) = match &mode {
                        RunMode::Autonomous {
                            iteration,
                            options,
                            started_at,
                            tool_calls_consumed,
                            ..
                        } => (
                            *iteration,
                            options.clone(),
                            *started_at,
                            *tool_calls_consumed,
                        ),
                        _ => unreachable!(),
                    };

                    let last_assistant_text = messages.iter().rev().find_map(|m| match m {
                        AgentMessage::Assistant(am) => {
                            let mut t = String::new();
                            for b in &am.content {
                                if let ContentBlock::Text { text } = b {
                                    t.push_str(text);
                                }
                            }
                            if t.is_empty() {
                                None
                            } else {
                                Some(t)
                            }
                        }
                        _ => None,
                    });
                    let goal_status = last_assistant_text.as_deref().and_then(extract_goal_status);

                    let (satisfied, reason) = goal_status
                        .clone()
                        .unwrap_or_else(|| (false, "no goal_status marker; continuing".to_owned()));
                    let _ = events
                        .send(AgentEvent::Callout {
                            category: "goal_status".to_owned(),
                            title: format!("iteration {current_iteration} goal-check"),
                            body: serde_json::json!({
                                "satisfied": satisfied,
                                "reason": reason,
                            }),
                            theme: Some("info".to_owned()),
                            context_refs: vec!["spec:mu-036".to_owned()],
                        })
                        .await;

                    let outcome = if satisfied {
                        AutonomousIterationOutcome::GoalMet {
                            detail: reason.clone(),
                        }
                    } else {
                        AutonomousIterationOutcome::Continue
                    };
                    let _ = events
                        .send(AgentEvent::AutonomousIterationCompleted {
                            iteration: current_iteration,
                            outcome: outcome.clone(),
                        })
                        .await;

                    let (cap_max_iter, cap_max_wall, cap_max_tools) = {
                        let cap = capability.lock().ok();
                        match cap.as_ref().map(|c| c.autonomy.clone()) {
                            Some(AutonomyCapability::Allowed {
                                max_iterations,
                                max_wall_clock_ms,
                                max_total_tool_calls_in_autonomy,
                                ..
                            }) => (
                                max_iterations,
                                max_wall_clock_ms,
                                max_total_tool_calls_in_autonomy,
                            ),
                            _ => (0, 0, 0),
                        }
                    };
                    let effective_max_iter = current_options
                        .max_iterations
                        .map(|o| o.min(cap_max_iter))
                        .unwrap_or(cap_max_iter);

                    let elapsed_ms_total = current_started_at.elapsed().as_millis() as u64;

                    let terminal_reason: Option<AutonomousTerminationReason> = if satisfied {
                        Some(AutonomousTerminationReason::GoalMet {
                            detail: reason.clone(),
                        })
                    } else if current_iteration >= effective_max_iter {
                        Some(AutonomousTerminationReason::IterationCap)
                    } else if elapsed_ms_total >= cap_max_wall {
                        Some(AutonomousTerminationReason::WallClockExpired)
                    } else if current_tool_calls >= cap_max_tools {
                        Some(AutonomousTerminationReason::ToolCallCapExhausted)
                    } else {
                        None
                    };

                    if let Some(reason_term) = terminal_reason {
                        let _ = events
                            .send(AgentEvent::AutonomousTerminated {
                                reason: reason_term,
                            })
                            .await;
                        mode = RunMode::Idle;
                        let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                        let stop_reason = last_stop_reason.take().unwrap_or(StopReason::EndTurn);
                        let _ = events
                            .send(AgentEvent::Done {
                                stop_reason,
                                turn_count,
                                usage: aggregated_usage.take(),
                                elapsed_ms,
                                command_receipts: std::mem::take(pending_tickets),
                            })
                            .await;
                        started_at = None;
                        turn_count = 0;
                        tool_history.clear();
                        continue;
                    }

                    let next_iter = current_iteration.saturating_add(1);
                    if let RunMode::Autonomous { iteration, .. } = &mut mode {
                        *iteration = next_iter;
                    }
                    let motivation = format!("iteration {next_iter}: continue toward the goal");
                    let _ = events
                        .send(AgentEvent::AutonomousIterationStarted {
                            iteration: next_iter,
                            motivation: motivation.clone(),
                        })
                        .await;
                    let continuation_msg = AgentMessage::User {
                        content: motivation,
                    };
                    let _ = events
                        .send(AgentEvent::MessageStart {
                            message: continuation_msg.clone(),
                        })
                        .await;
                    messages.push(continuation_msg.clone());
                    let _ = events
                        .send(AgentEvent::MessageEnd {
                            message: continuation_msg,
                        })
                        .await;
                    queue.push_back(Action::InvokeLlm);
                    continue;
                }

                // Idle mode: the ask that queued this MaybeFinish is
                // complete — emit its Done terminus NOW, even when
                // follow-up input is already queued. (mu-wf5w: the old
                // early-`continue` on a non-empty queue suppressed the
                // terminus — see plan_post_invoke_llm.) The queued
                // follow-up then starts a FRESH ask on the next queue
                // pass: per-ask state is reset here, so its Done
                // reports its own turn_count / usage / elapsed.
                let elapsed_ms = started_at.map(|t| t.elapsed().as_millis() as u64);
                let stop_reason = last_stop_reason.take().unwrap_or(StopReason::EndTurn);
                let _ = events
                    .send(AgentEvent::Done {
                        stop_reason,
                        turn_count,
                        usage: aggregated_usage.take(),
                        elapsed_ms,
                        command_receipts: std::mem::take(pending_tickets),
                    })
                    .await;
                started_at = None;
                turn_count = 0;
                tool_history.clear();
            }
        }
    }

    tool_history.clear();
    Outcome::Done(StopReason::EndTurn)
}

/// Count the number of User, Assistant, and ToolResult messages
/// in a slice. Used by the ContextAssembly emit path (mu-032) to
/// summarize the prompt being sent to the provider.
fn count_message_roles(messages: &[AgentMessage]) -> (u32, u32, u32) {
    let mut u = 0u32;
    let mut a = 0u32;
    let mut t = 0u32;
    for m in messages {
        match m {
            AgentMessage::User { .. } => u += 1,
            AgentMessage::Assistant(_) => a += 1,
            AgentMessage::ToolResult { .. } => t += 1,
        }
    }
    (u, a, t)
}

/// mu-036 Phase B: parse the model's iteration-end assistant text for
/// a `goal_status` self-report (SelfReport `GoalCheckMethod`).
///
/// Two accepted shapes (in order of precedence):
/// 1. An embedded JSON object containing `"goal_status"` with a
///    `{satisfied: bool, reason: string}` body.
/// 2. The terse marker substrings `goal_status:satisfied` /
///    `goal_status:not_satisfied` (case-sensitive) — fallback for
///    models / FauxProvider scripts that don't emit JSON.
///
/// Returns `None` when no marker is found (loop continues).
pub(crate) fn extract_goal_status(text: &str) -> Option<(bool, String)> {
    if let Some(idx) = text.find('{') {
        for end in (idx + 1..=text.len()).rev() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[idx..end]) {
                if let Some(gs) = v.get("goal_status") {
                    let satisfied = gs.get("satisfied").and_then(|b| b.as_bool());
                    let reason = gs
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("")
                        .to_owned();
                    if let Some(s) = satisfied {
                        return Some((s, reason));
                    }
                }
                break;
            }
        }
    }
    if text.contains("goal_status:satisfied") {
        return Some((true, "marker: goal_status:satisfied".to_owned()));
    }
    if text.contains("goal_status:not_satisfied") {
        return Some((false, "marker: goal_status:not_satisfied".to_owned()));
    }
    None
}

/// mu-c4cz: append wall-clock time and session elapsed to the system
/// prompt so the model knows when it is and how long it's been running.
fn build_effective_system_prompt(
    base: Option<&str>,
    session_started_at: &Instant,
) -> Option<String> {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    let elapsed = session_started_at.elapsed();
    let elapsed_mins = elapsed.as_secs() / 60;

    let time_line = format!(
        "\n\nCurrent time: {:02}:{:02} UTC. Session has been running for {} minute{}.",
        hours,
        mins,
        elapsed_mins,
        if elapsed_mins == 1 { "" } else { "s" },
    );

    match base {
        Some(s) => Some(format!("{s}{time_line}")),
        None => Some(time_line.trim_start().to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::all)]
mod tests;
