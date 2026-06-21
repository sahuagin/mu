//! App state + main event loop.
//!
//! v0 scope (intentionally minimal):
//! - One session, one provider, one pane.
//! - Prompt input on the bottom inline viewport.
//! - Transcript via `insert_before` into mux scrollback.
//! - Streaming text rendered as it arrives.
//!
//! No multi-window, no command palette yet, no F-key views. Just the
//! send-prompt → see-response loop. Commands (`/model`, `/help`, etc.)
//! land next.

use std::collections::HashMap;
use std::time::Duration;

use crate::mcp_status;
use crate::menu::{InlineMenu, MenuAction, MenuItem};
use anyhow::{anyhow, Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use mu_core::session_status::SessionStatus;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::Value;

use crate::client::{Client, Message};
use crate::input::InputBuffer;
use crate::picker;
use crate::render;
use crate::skills::{self, DiscoveredSkill};
use crate::transcript::{Transcript, TranscriptBlock, TranscriptKind};
use crate::viewport::DynamicViewport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockAction {
    Copy,
    Prompt,
    Maximize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MaximizedBlock {
    index: usize,
    scroll: usize,
}

/// A modal overlay panel (mu-5h9m): a full-viewport bordered box that takes
/// over the screen to show command output — `/help`, `/status` — which would
/// otherwise emit via `insert_before` and be painted over by the fullscreen
/// owned-buffer render the next frame. Painted through the owned buffer like
/// `MaximizedBlock`, so it survives fullscreen. Holds pre-built styled lines
/// (so `/status`'s colors carry through); dismiss with Esc/q.
#[derive(Debug, Clone)]
struct Overlay {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: usize,
}

/// Known providers offered by the `/provider` picker. Free-form
/// `/provider <name>` also works for anything not on this list.
const KNOWN_PROVIDERS: &[&str] = &[
    "openai-codex",
    "anthropic",
    "anthropic-oauth",
    "openai-api",
    "openrouter",
    "vllm",
    "ollama",
    "faux",
];

/// Known models per provider for the `/model` picker. Returns an
/// empty slice for providers we don't have curated lists for; the
/// caller falls back to free-form entry. Strings live as `&'static str`
/// so we can hand them to the picker without allocating.
fn known_models_for(provider: &str) -> &'static [&'static str] {
    match normalize_provider_kind(provider).as_str() {
        "anthropic_api" | "anthropic_oauth" => &[
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        ],
        "openai_codex" => &["gpt-5.5"],
        "openai_api" => &["gpt-4o", "gpt-4-turbo"],
        // OpenRouter model IDs drift faster than mu releases; this is
        // a curated *small* set of currently-valid entries (verified
        // against openrouter.ai/api/v1/models as of 2026-05-23). For
        // the long tail, use `/model <full-id>` to set directly —
        // free-form entry skips the picker. A future commit can
        // populate this list dynamically by querying OpenRouter's
        // /api/v1/models on first /model invocation.
        "openrouter" => &[
            "anthropic/claude-opus-4.7",
            "anthropic/claude-haiku-4-5",
            "openai/gpt-5.5",
            "google/gemini-3.5-flash",
            "x-ai/grok-4.3",
            "meta-llama/llama-4-maverick",
        ],
        "vllm" => &["Qwen/Qwen3-Coder-30B-A3B-Instruct-FP8"],
        "ollama" => &["qwen3-coder:30b", "qwen3.6:35b-a3b-q8_0"],
        "faux" => &["faux"],
        _ => &[],
    }
}

/// Which session's turn is currently being rendered. v0 has two
/// possible owners: the main session (default) or the lazily-created
/// sidecar that holds `/btw` side questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRoute {
    Main,
    Btw,
}

impl TurnRoute {
    /// Header label + body-prefix color for streaming output.
    pub fn header_label(self) -> &'static str {
        match self {
            Self::Main => "assistant",
            Self::Btw => "assistant ⋅ btw",
        }
    }

    pub fn color(self) -> Color {
        match self {
            Self::Main => Color::White,
            Self::Btw => Color::Magenta,
        }
    }

    /// Color + label for the "you" block emitted when the user
    /// submits a prompt.
    pub fn you_label(self) -> &'static str {
        match self {
            Self::Main => "you",
            Self::Btw => "you ⋅ btw",
        }
    }

    pub fn you_color(self) -> Color {
        match self {
            Self::Main => Color::Cyan,
            Self::Btw => Color::Magenta,
        }
    }
}

/// The in-flight assistant turn (mu-d04a Phase 1). Built incrementally by
/// `handle_notification` from wire events and re-rendered from scratch each
/// frame via `render::render_turn`; committed to scrollback (with closer)
/// on `session.done` / `session.error`, then dropped. Phase 1 holds a
/// single turn; Phase 2 makes this a per-session map keyed by session_id
/// so a `/btw` sidecar can stream concurrently.
#[derive(Debug, Clone)]
pub struct Turn {
    pub route: TurnRoute,
    pub items: Vec<render::TurnItem>,
}

impl Turn {
    fn new(route: TurnRoute) -> Self {
        Self {
            route,
            items: Vec::new(),
        }
    }

    /// Append a text delta: extend the trailing `Text` item if the last
    /// item is text, else push a new one. This is what makes streamed
    /// prose accumulate in place instead of committing per-newline.
    fn push_text(&mut self, delta: &str) {
        match self.items.last_mut() {
            Some(render::TurnItem::Text(s)) => s.push_str(delta),
            _ => self.items.push(render::TurnItem::Text(delta.to_string())),
        }
    }

    /// Append a reasoning delta, mirroring [`push_text`](Self::push_text):
    /// extend the trailing `Thinking` item or open a new one. (mu-upk2)
    fn push_thinking(&mut self, delta: &str) {
        match self.items.last_mut() {
            Some(render::TurnItem::Thinking(s)) => s.push_str(delta),
            _ => self
                .items
                .push(render::TurnItem::Thinking(delta.to_string())),
        }
    }

    /// The most recent in-flight `ToolCall` item with this id, if any. Used to
    /// fold streamed `session.tool_call_delta` fragments and the finalizing
    /// `session.tool_call_started` into one item. (mu-upk2)
    fn tool_call_mut(&mut self, id: &str) -> Option<&mut render::TurnItem> {
        self.items.iter_mut().rev().find(|it| match it {
            render::TurnItem::ToolCall { tool_call_id, .. } => tool_call_id == id,
            _ => false,
        })
    }

    /// True if any item carries visible content (replaces the old
    /// `ask_had_output` flag for the live turn).
    fn has_output(&self) -> bool {
        self.items.iter().any(|it| match it {
            render::TurnItem::Text(s) => !s.is_empty(),
            render::TurnItem::Thinking(s) => !s.is_empty(),
            _ => true,
        })
    }
}

/// Normalize a provider string to the daemon's wire enum
/// (`ProviderSelector::kind`, snake_case). Accept the common spellings
/// users type at the CLI. Shared between session create and
/// `session.delegate` (sidecar creation for /btw).
fn normalize_provider_kind(provider: &str) -> String {
    let lc = provider.to_lowercase();
    match lc.as_str() {
        "anthropic" | "anthropic-api" | "anthropic_api" | "claude" => "anthropic_api".into(),
        "openai" | "openai-codex" | "openai_codex" | "codex" => "openai_codex".into(),
        "openrouter" | "open-router" | "open_router" => "openrouter".into(),
        "vllm" | "vllm-openai" | "local-vllm" | "local_vllm" => "vllm".into(),
        "ollama" | "local" => "ollama".into(),
        "faux" => "faux".into(),
        _ => lc,
    }
}

fn truncate_at_word(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let truncated = &s[..s.floor_char_boundary(max)];
    match truncated.rfind(' ') {
        Some(pos) if pos > max / 2 => format!("{}…", &truncated[..pos]),
        _ => format!("{truncated}…"),
    }
}

/// Session-level effort dial. Layer on top of model selection per
/// claude-code-feature-mapping §17. mu-vcbm: live — `fire_ask` carries
/// the selected level on every `ask_session.effort`; the daemon applies
/// it stickily and maps it onto the provider's thinking/reasoning
/// directive. (mu-vcbm slice 2 will config-drive the allowed levels and
/// default, retiring this hardcoded enum.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl EffortLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "low" | "l" => Some(Self::Low),
            "medium" | "med" | "m" => Some(Self::Medium),
            "high" | "h" => Some(Self::High),
            "xhigh" | "x-high" | "extra-high" | "x" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub const ALL: &'static [Self] = &[Self::Low, Self::Medium, Self::High, Self::XHigh, Self::Max];
}

/// User-visible app state. Held across the run loop.
pub struct App {
    client: Client,
    session_id: String,
    /// Provider + model strings (display-only for v0).
    provider: String,
    model: String,
    /// Cursor-aware input buffer. Supports multi-line (paste), cursor
    /// movement, and visual wrapping for the grow-upward prompt.
    prompt: InputBuffer,
    /// Session-wide paste counter for collapse display.
    paste_count: usize,
    /// Fullscreen owned-buffer render mode (mu-5h9m). When true, the whole
    /// transcript is rendered from the in-memory model into an alt-screen
    /// buffer each frame, windowed by `transcript_scroll` — no `insert_before`,
    /// no native scrollback. Opt-in via `MU_SOLO_FULLSCREEN` while it's built
    /// as a parallel mode alongside the inline path.
    fullscreen: bool,
    /// Lines scrolled UP from the bottom of the transcript in fullscreen mode
    /// (0 = stuck to the latest). Ignored in inline mode.
    transcript_scroll: usize,
    /// Daemon ID (per daemon.stats at startup). Surfaced via /status.
    daemon_id: String,
    /// Daemon version string. Surfaced via /status.
    daemon_version: String,
    /// Session-level effort dial (§17). mu-vcbm: sent on every
    /// `ask_session.effort` by `fire_ask`; the daemon applies it stickily.
    effort: EffortLevel,
    /// Focus mode (§16): when true, suppress streaming text_delta
    /// previews and render the assistant block in one shot on
    /// `assistant_text_finalized`. Default off.
    focus_mode: bool,
    /// Optional configured clipboard command for `/copy`, as argv (no shell).
    /// This is an explicit operator escape hatch after native clipboard fails.
    clipboard_command: Option<Vec<String>>,
    /// Sidecar session for `/btw` side questions (§13). Created
    /// lazily on first `/btw` via `session.delegate`. Persists across
    /// /btw calls so follow-ups stay coherent; main session history
    /// is unaffected.
    sidecar_session_id: Option<String>,
    /// Absolute path to the durable event log for the main session.
    /// Used by the renderer-mismatch diagnostic: ContextAssembly
    /// events aren't on the wire (per forwarder.rs:209), so we read
    /// them off disk to detect a silent faux-fallback. None when we
    /// can't resolve the data dir on this platform/user.
    events_file: Option<std::path::PathBuf>,
    /// What the daemon *actually* picked for the renderer / cache /
    /// provider on this session, read from the first ContextAssembly
    /// event. None until the first session.done lets us peek. The
    /// "asked" side is `self.provider` + `self.model`.
    actual_renderer: Option<String>,
    actual_cache_strategy: Option<String>,
    actual_provider_kind: Option<String>,
    actual_model: Option<String>,
    /// Set after we've shown the mismatch warning once; the warning
    /// fires at most once per process to avoid spamming scrollback if
    /// the user keeps sending prompts to a faux session.
    renderer_mismatch_warned: bool,
    /// Which session owns the currently-streaming turn. Set when an ask
    /// is fired (Main on /send, Btw on /btw); kept in sync with
    /// `live_turn.route` and used by `/cancel` to route to the right
    /// session. None when no turn is in flight.
    streaming_route: Option<TurnRoute>,
    /// mu-d3v6: request ids of asks fired via `request_nowait`,
    /// awaiting their end-of-turn responses on the async channel.
    /// Almost always 0 or 1 entries (main ask; /btw can add a second).
    /// Used to surface RPC-level ask errors; success responses are
    /// no-ops (session.done already drove the turn commit).
    pending_ask_ids: std::collections::HashSet<i64>,
    /// The in-flight assistant turn as a structured model (mu-d04a).
    /// Built by `handle_notification`, rendered live in the viewport each
    /// frame, committed to scrollback on done/error. None when idle.
    live_turn: Option<Turn>,
    /// Semantic transcript independent of rendered terminal cells. Copy /
    /// export commands read this, not ratatui scrollback.
    transcript: Transcript,
    /// Selected semantic transcript block. This is a cursor over the record,
    /// not over terminal cells; it survives scrollback repainting and feeds
    /// block copy / prompt-yank / maximize actions.
    selected_block: Option<usize>,
    /// Focused single-block pager. This is another semantic transcript
    /// projection, not a dump into terminal scrollback.
    maximized_block: Option<MaximizedBlock>,
    /// Active modal overlay (slash-command output panel: /help, /status).
    /// None when closed. Takes over the screen until dismissed (Esc/q).
    overlay: Option<Overlay>,
    /// Ephemeral action acknowledgment shown in the info line (e.g. "✓ copied"),
    /// like vim's "N lines yanked". Cleared on the next keystroke (mu-5h9m):
    /// fullscreen actions otherwise succeed silently (insert_before is painted
    /// over), so there was no signal the command did anything.
    flash: Option<String>,
    /// Fold completed tool call+result blocks to one-line summaries in the
    /// fullscreen render (mu-5h9m, /collapse). Default on — the
    /// readable-while-streaming firehose fix. Fullscreen-only; inline
    /// scrollback keeps full results (terminal history can't be re-expanded).
    collapse_tools: bool,
    bash_yolo: bool,
    /// Discovered skills from SKILL.md files on disk.
    skills: HashMap<String, DiscoveredSkill>,
    /// Active inline menu (slash-command picker, etc). None when closed.
    inline_menu: Option<InlineMenu>,
    /// What the inline menu is being used for — determines what
    /// happens on selection.
    menu_context: MenuContext,
    /// Provider-status-driven session phase for the status line.
    session_phase: SessionPhase,
    /// Elapsed ms in the current provider-status phase. Updated by
    /// `session.provider_status` notifications; reset on phase transitions.
    phase_elapsed_ms: u64,
    /// Cumulative token usage across all completed asks (from session.done).
    cumulative_input_tokens: u64,
    cumulative_output_tokens: u64,
    cumulative_cache_read: u64,
    cumulative_cache_creation: u64,
    /// Completed ask count (incremented on session.done).
    ask_count: u32,
    /// MCP status subscription receiver. When connected, receives
    /// SessionStatus pushes from the daemon via the MCP socket.
    /// Falls back to inline accumulation when not connected.
    mcp_status_rx: Option<tokio::sync::mpsc::UnboundedReceiver<SessionStatus>>,
    /// Latest SessionStatus from the MCP subscription.
    mcp_status: Option<SessionStatus>,
    /// mu-solo-scrollback-dup-recommit-8hva: write a renderer journal.
    renderer_journal: bool,
    /// mu-solo-osc-notify-mbmn: desktop notifications (OSC 99) on main-
    /// session turn done/error while the terminal is unfocused.
    notifications: bool,
    /// Terminal focus state, tracked via crossterm FocusGained/
    /// FocusLost events (bin enables EnableFocusChange). Starts true:
    /// mu-solo is foreground at launch, and a terminal that never
    /// reports focus events then never notifies — the conservative
    /// failure mode (silence, not spam).
    terminal_focused: bool,
}

/// What the inline menu is selecting.
#[derive(Default)]
enum MenuContext {
    /// Slash-command picker: selection inserts the command into the prompt.
    #[default]
    SlashCommand,
    /// Effort-level picker: selection applies the effort level directly.
    Effort,
}

/// Session phase for the status line — a PROJECTION of current session
/// state, re-derived on every event, never a sticky last-write (mu-d2hx:
/// a done arm that didn't repaint left "⚙ tool (14.0s)" frozen for
/// minutes and the operator read it as a hang).
///
/// State machine discipline:
/// - firing an ask (`fire_ask` / `/btw`) → `AwaitingFirstToken`
/// - `session.provider_status` (and the MCP status fallback) move
///   between the LIVE states — but only while a turn is in flight, so
///   a stale straggler can't resurrect a spinner after a terminal event
///   (see [`SessionPhase::on_provider_status`])
/// - every terminal arm transitions: `session.done` → `Idle` (or
///   `TurnBudgetExhausted` on `stop_reason == "iteration_cap"`),
///   `session.error` and RPC-level ask failures → `Errored`
///   (see [`SessionPhase::on_turn_end`])
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SessionPhase {
    #[default]
    Idle,
    AwaitingFirstToken,
    Streaming,
    ToolExecuting,
    /// Terminal: the agent loop hit its turn budget
    /// (`StopReason::IterationCap`). Sticky until the next ask so the
    /// operator sees WHY the session stopped — it must never look like
    /// work in progress (mu-d2hx item b).
    TurnBudgetExhausted {
        turn_count: Option<u32>,
    },
    /// Terminal: the turn ended with an error (session.error or an
    /// RPC-level ask failure). Sticky until the next ask.
    Errored,
}

/// The operator-facing copy for an iteration-cap stop (mu-d2hx /
/// mu-779s). `turn_count` comes from the done event when the wire
/// carries it; the copy degrades gracefully when it doesn't.
fn turn_budget_copy(turn_count: Option<u32>) -> String {
    match turn_count {
        Some(n) => format!("turn budget exhausted ({n}) — say continue, or raise the cap"),
        None => "turn budget exhausted — say continue, or raise the cap".to_string(),
    }
}

impl SessionPhase {
    fn icon(self) -> &'static str {
        match self {
            Self::Idle => "○",
            Self::AwaitingFirstToken => "◉",
            Self::Streaming => "●",
            Self::ToolExecuting => "⚙",
            Self::TurnBudgetExhausted { .. } => "■",
            Self::Errored => "×",
        }
    }

    fn label(self) -> String {
        match self {
            Self::Idle => "idle".to_string(),
            Self::AwaitingFirstToken => "thinking".to_string(),
            Self::Streaming => "streaming".to_string(),
            Self::ToolExecuting => "tool".to_string(),
            Self::TurnBudgetExhausted { turn_count } => turn_budget_copy(turn_count),
            Self::Errored => "turn ended with error — see transcript".to_string(),
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Idle => Color::DarkGray,
            Self::AwaitingFirstToken => Color::Cyan,
            Self::Streaming => Color::Green,
            Self::ToolExecuting => Color::Yellow,
            Self::TurnBudgetExhausted { .. } => Color::Magenta,
            Self::Errored => Color::Red,
        }
    }

    /// True for the in-flight states (the only ones that may show a
    /// ticking elapsed counter). Terminal/idle states never animate —
    /// that's what made the frozen spinner read as a hang.
    fn is_live(self) -> bool {
        matches!(
            self,
            Self::AwaitingFirstToken | Self::Streaming | Self::ToolExecuting
        )
    }

    /// Transition for a provider-status update (`session.provider_status`
    /// wire notifications and the MCP `SessionStatus.phase` fallback).
    ///
    /// `in_flight` is "an ask is outstanding" (streaming_route set).
    /// When the turn is over, live-phase claims are IGNORED: provider
    /// status is an ephemeral stream and a straggler that arrives after
    /// the terminal event must not overwrite the done/error repaint —
    /// that resurrection is exactly the mu-d2hx freeze.
    fn on_provider_status(self, kind: &str, in_flight: bool) -> SessionPhase {
        if !in_flight {
            return self;
        }
        match kind {
            "awaiting_first_token" | "thinking" => Self::AwaitingFirstToken,
            "streaming" => Self::Streaming,
            "tool_executing" | "awaiting_tool_result" => Self::ToolExecuting,
            "idle" => Self::Idle,
            // Forward-compat: unknown kinds keep the current phase.
            _ => self,
        }
    }

    /// Terminal transition for a turn-ending event. Every done/error
    /// path MUST route through here so no arm can forget to repaint.
    /// `stop_reason` is the wire string off `session.done` params
    /// (serde snake_case of `StopReason`, e.g. "iteration_cap").
    fn on_turn_end(is_error: bool, stop_reason: Option<&str>, turn_count: Option<u32>) -> Self {
        if is_error {
            Self::Errored
        } else if stop_reason == Some("iteration_cap") {
            Self::TurnBudgetExhausted { turn_count }
        } else {
            Self::Idle
        }
    }
}

/// Startup options for [`App::new`] — bundled so the constructor takes one
/// borrowed struct instead of eight positional args. Borrows from the parsed
/// config/CLI; no allocation at startup.
pub struct AppOptions<'a> {
    pub mu_binary: &'a str,
    pub cwd: &'a std::path::Path,
    pub provider: &'a str,
    pub model: &'a str,
    pub bash_yolo: bool,
    pub tools: &'a str,
    /// mu-upk2: extended-thinking directive forwarded as `mu serve
    /// --thinking <v>`. Empty = off.
    pub thinking: &'a str,
    pub effort: &'a str,
    pub focus_mode: bool,
    /// mu-f1a0: cache TTL tier ("5m" | "1h") for the initial session.
    pub cache_ttl: &'a str,
    pub clipboard_command: Option<&'a [String]>,
    /// mu-solo-scrollback-dup-recommit-8hva: enable the renderer journal.
    /// Written to `~/.local/share/mu/solo/renderer.jsonl`.
    pub renderer_journal: bool,
    /// mu-solo-osc-notify-mbmn: desktop notifications via OSC 99 on
    /// main-session turn done/error while the terminal is unfocused.
    pub notifications: bool,
    /// mu-7e21: autonomy grant forwarded in create_session. None ⇒
    /// field omitted (INV-1 default: disallowed; no autonomy tools).
    pub autonomy: Option<mu_core::capability::AutonomyCapability>,
    /// mu-n25a: side-effects ceiling forwarded in create_session. None ⇒
    /// field omitted (root default: unrestricted, no posture restriction).
    pub max_side_effects: Option<mu_core::agent::tool::SideEffects>,
}

impl App {
    /// Spawn `mu serve`, authenticate, create a session, and return an
    /// App ready to run.
    ///
    /// `effort` is parsed via [`EffortLevel::parse`]; invalid values
    /// surface as an error so a typo in `solo.toml` doesn't silently
    /// fall back to Medium. `focus_mode` seeds the /focus toggle.
    pub fn new(opts: AppOptions) -> Result<Self> {
        let AppOptions {
            mu_binary,
            cwd,
            provider,
            model,
            bash_yolo,
            tools,
            thinking,
            effort,
            focus_mode,
            clipboard_command,
            cache_ttl,
            renderer_journal,
            notifications,
            autonomy,
            max_side_effects,
        } = opts;
        let effort = EffortLevel::parse(effort).ok_or_else(|| {
            anyhow!("invalid effort {effort:?} (valid: low|medium|high|xhigh|max)")
        })?;
        let mut client = Client::spawn(mu_binary, cwd, bash_yolo, tools, thinking)?;

        let search_dirs = skills::default_search_dirs(Some(cwd));
        let skills = skills::discover(&search_dirs);
        if !skills.is_empty() {
            tracing::info!(count = skills.len(), "discovered skills");
        }

        // Normalize provider input → daemon's snake_case wire enum
        // (mirrors mu-tui's accept-anything mapping in create_session).
        let kind = normalize_provider_kind(provider);

        // mu-f1a0: forward the configured cache TTL tier. Omit the
        // field entirely when it isn't one of the wire values so an
        // older daemon (or a typo) degrades to the 5m default rather
        // than failing session creation.
        let mut create_params = serde_json::json!({
            "provider": { "kind": kind, "model": model },
        });
        if matches!(cache_ttl, "5m" | "1h") {
            create_params["cache_ttl"] = serde_json::json!(cache_ttl);
        }
        // mu-7e21: forward the autonomy grant when configured. The
        // type serializes to the capability wire shape directly, so
        // the daemon-side deserialization can't drift from this.
        if let Some(autonomy) = &autonomy {
            match serde_json::to_value(autonomy) {
                Ok(v) => {
                    create_params["autonomy"] = v;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not serialize autonomy grant; omitting");
                }
            }
        }
        // mu-n25a: forward the side-effects ceiling when configured. Like
        // autonomy, omit the field entirely when None so an older daemon
        // (or an unrestricted session) degrades to today's behavior.
        if let Some(max_side_effects) = &max_side_effects {
            match serde_json::to_value(max_side_effects) {
                Ok(v) => {
                    create_params["max_side_effects"] = v;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not serialize max_side_effects; omitting");
                }
            }
        }
        let resp = client
            .request("create_session", create_params)
            .context("create_session failed")?;
        let session_id = resp
            .get("session_id")
            .and_then(|v| v.as_str())
            .context("session.create response missing session_id")?
            .to_string();

        // daemon.stats — query once at startup for the daemon_id /
        // version so /status can surface them. Non-fatal if missing
        // (older daemons may not expose these fields).
        let stats = client
            .request("daemon.stats", serde_json::json!({}))
            .unwrap_or(serde_json::Value::Null);
        let daemon_id = stats
            .get("daemon_id")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)")
            .to_string();
        let daemon_version = stats
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)")
            .to_string();

        // Construct the events log path before moving daemon_id into
        // the struct. dirs::data_dir() returns None only on
        // pathological setups (no $HOME / no equivalent); in that
        // case the diagnostic silently degrades to "(pending)".
        let events_file = dirs::data_dir().map(|p| {
            p.join("mu")
                .join("events")
                .join(&daemon_id)
                .join("session-1.jsonl")
        });

        let mcp_status_rx = Some(mcp_status::spawn_status_subscriber(session_id.clone()));

        Ok(Self {
            client,
            session_id,
            provider: provider.to_string(),
            model: model.to_string(),
            prompt: InputBuffer::new(),
            paste_count: 0,
            daemon_id,
            daemon_version,
            effort,
            focus_mode,
            clipboard_command: clipboard_command.map(<[String]>::to_vec),
            sidecar_session_id: None,
            streaming_route: None,
            pending_ask_ids: std::collections::HashSet::new(),
            live_turn: None,
            transcript: Transcript::new(),
            fullscreen: std::env::var_os("MU_SOLO_FULLSCREEN").is_some(),
            transcript_scroll: 0,
            selected_block: None,
            maximized_block: None,
            overlay: None,
            flash: None,
            collapse_tools: true,
            events_file,
            actual_renderer: None,
            actual_cache_strategy: None,
            actual_provider_kind: None,
            actual_model: None,
            renderer_mismatch_warned: false,
            bash_yolo,
            skills,
            inline_menu: None,
            menu_context: MenuContext::default(),
            session_phase: SessionPhase::default(),
            phase_elapsed_ms: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_cache_read: 0,
            cumulative_cache_creation: 0,
            ask_count: 0,
            mcp_status_rx,
            mcp_status: None,
            renderer_journal,
            notifications,
            terminal_focused: true,
        })
    }

    /// Shut the spawned daemon down, bounded
    /// (mu-mu-solo-loop-terminate-5ek5): stdin-EOF grace, then
    /// SIGKILL + reap. Called by the binary after `run` returns so
    /// quit never orphans a wedged daemon and never waits on one.
    pub fn shutdown_daemon(&mut self) {
        self.client.shutdown(std::time::Duration::from_millis(1500));
    }

    /// Run the async event loop. Returns Ok(()) on clean exit.
    ///
    /// Uses `tokio::select!` to multiplex four event sources:
    /// - Keyboard/paste events via crossterm's `EventStream`
    /// - Daemon notifications via tokio mpsc (from the reader thread)
    /// - MCP session status via tokio mpsc (from rmcp client task)
    /// - Periodic render tick for elapsed-time display updates
    pub async fn run(&mut self) -> Result<()> {
        // Resolve journal path: ~/.local/share/mu/solo/renderer.jsonl.
        // Strictly separate from the semantic event store
        // (~/.local/share/mu/events/).
        let journal_path: Option<std::path::PathBuf> = if self.renderer_journal {
            dirs::data_dir().map(|p| p.join("mu").join("solo").join("renderer.jsonl"))
        } else {
            None
        };
        let mut vp = DynamicViewport::new(VIEWPORT_HEIGHT, journal_path.as_deref())
            .context("DynamicViewport::new")?;
        vp.snap_to_bottom()?;

        // Initial banner — printed once into scrollback.
        let banner_lines = vec![
            Line::from(Span::styled(
                format!("mu-solo · {} · {}", self.provider, self.model),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!(
                    "effort: {} · focus: {} · /help for commands · /q to quit",
                    self.effort.as_str(),
                    if self.focus_mode { "on" } else { "off" }
                ),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
        ];
        vp.insert_before(banner_lines.len() as u16, |buf| {
            let p = Paragraph::new(banner_lines).wrap(Wrap { trim: false });
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;

        // Take the async notification receiver from the client.
        let mut notif_rx = self
            .client
            .take_notification_rx()
            .expect("notification rx already taken");

        let mut event_stream = EventStream::new();
        let mut render_interval = tokio::time::interval(Duration::from_millis(100));
        let mut mcp_rx = self.mcp_status_rx.take();

        loop {
            if self.overlay.is_some() {
                self.render_overlay(&mut vp)?;
            } else if self.fullscreen {
                self.render_fullscreen(&mut vp)?;
            } else {
                self.render_viewport(&mut vp)?;
            }

            tokio::select! {
                biased;

                // Daemon notifications — highest priority so streaming
                // text renders immediately.
                maybe_notif = notif_rx.recv() => {
                    match maybe_notif {
                        Some(msg) => {
                            self.handle_message(&mut vp, msg)?;
                            // Drain any additional queued notifications
                            // so we batch-process bursts and don't
                            // re-render between each text_delta.
                            while let Ok(msg) = notif_rx.try_recv() {
                                self.handle_message(&mut vp, msg)?;
                            }
                        }
                        None => {
                            let width = vp.area().width as usize;
                            let wrap = width.saturating_sub(2);
                            let lines = render::error_block("daemon exited", wrap);
                            let h = lines.len() as u16;
                            vp.insert_before(h, |buf| {
                                let p = Paragraph::new(lines);
                                ratatui::widgets::Widget::render(p, buf.area, buf);
                            })?;
                            break;
                        }
                    }
                }
                // MCP session status — wakes immediately on push from
                // the rmcp client task (no polling).
                Some(status) = async {
                    match mcp_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.apply_mcp_status(status);
                }
                // Keyboard / paste events
                maybe_event = event_stream.next() => {
                    match maybe_event {
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press
                            && self.handle_key(&mut vp, key)? => {
                                break;
                            }
                        Some(Ok(Event::Paste(text))) => {
                            self.paste_count += 1;
                            self.prompt.insert_paste(&text, self.paste_count);
                        }
                        // mu-solo-osc-notify-mbmn: focus tracking for
                        // notification gating (bin enables
                        // EnableFocusChange; terminals that don't
                        // report focus simply never deliver these).
                        Some(Ok(Event::FocusGained)) => {
                            self.terminal_focused = true;
                        }
                        Some(Ok(Event::FocusLost)) => {
                            self.terminal_focused = false;
                        }
                        Some(Err(e)) => {
                            tracing::warn!("crossterm event error: {e}");
                        }
                        None => break,
                        _ => {}
                    }
                }
                // Periodic render tick — updates elapsed time display.
                _ = render_interval.tick() => {}
            }
        }
        Ok(())
    }

    /// Render the viewport (separator + menu + prompt + status line).
    /// Fullscreen owned-buffer render (mu-5h9m): paint the whole transcript
    /// from the in-memory model into a maximized viewport each frame, windowed
    /// by `transcript_scroll`, with the input chrome pinned at the bottom. No
    /// `insert_before`, so the inline scrollback gap/dup class can't occur.
    /// First cut renders the transcript plainly; styled per-block render is a
    /// follow-up. Built as a parallel mode (opt-in via `MU_SOLO_FULLSCREEN`).
    fn render_fullscreen(&mut self, vp: &mut DynamicViewport) -> Result<()> {
        vp.maximize_height()?;
        let area = vp.area();
        let total = area.height as usize;
        let width = area.width as usize;
        // Reserve the 3-col " > " / "   " prompt prefix plus 1 col for the
        // trailing block cursor, matching render_viewport's `w - 4` (mu-5h9m).
        // Wrapping at `width - 1` overflowed the prefix and hard-wrapped the
        // tail of long prompt lines.
        let wrap = width.saturating_sub(4);

        // Bottom chrome: a separator rule + the prompt's visual lines.
        let layout = self.prompt.visual_layout(wrap);
        let mut chrome: Vec<Line<'static>> = Vec::new();
        chrome.push(Line::from("─".repeat(width)));
        // Slash-command dropdown above the prompt (mu-5h9m: was missing in
        // fullscreen). Mirrors render_viewport.
        if let Some(ref menu) = self.inline_menu {
            let (visible, cursor_pos, has_above, has_below) = menu.visible_items();
            if has_above {
                chrome.push(Line::from(Span::styled(
                    "  ↑ more".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            for (vi, (_orig_idx, item)) in visible.iter().enumerate() {
                let is_selected = vi == cursor_pos;
                let name_width = 24.min(width / 3);
                let desc_width = width.saturating_sub(name_width + 4);
                let name_padded = format!("{:<width$}", item.name, width = name_width);
                let desc_trunc = if item.description.len() > desc_width {
                    format!("{}…", &item.description[..desc_width.saturating_sub(1)])
                } else {
                    item.description.clone()
                };
                let (name_style, desc_style) = if is_selected {
                    (
                        Style::default().fg(Color::Black).bg(Color::Cyan),
                        Style::default().fg(Color::DarkGray).bg(Color::Cyan),
                    )
                } else {
                    (
                        Style::default().fg(Color::White),
                        Style::default().fg(Color::DarkGray),
                    )
                };
                chrome.push(Line::from(vec![
                    Span::styled(format!("  {name_padded}"), name_style),
                    Span::styled(format!(" {desc_trunc}"), desc_style),
                ]));
            }
            if has_below {
                chrome.push(Line::from(Span::styled(
                    "  ↓ more".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        // Prompt with a visible (inverted-block) cursor at the caret, since
        // fullscreen hides the terminal cursor (mu-5h9m).
        let cursor_style = Style::default().fg(Color::Black).bg(Color::Cyan);
        if layout.lines.is_empty() {
            chrome.push(Line::from(vec![
                Span::styled(" > ".to_string(), Style::default().fg(Color::Cyan)),
                Span::styled(" ".to_string(), cursor_style),
            ]));
        } else {
            for (row_idx, vline) in layout.lines.iter().enumerate() {
                let prefix = if row_idx == 0 { " > " } else { "   " };
                if row_idx == layout.cursor_row {
                    let before: String = vline.text.chars().take(layout.cursor_col).collect();
                    let after: String = vline.text.chars().skip(layout.cursor_col).collect();
                    let cursor_char = after.chars().next().unwrap_or(' ').to_string();
                    let rest: String = after.chars().skip(1).collect();
                    chrome.push(Line::from(vec![
                        Span::styled(prefix.to_string(), Style::default().fg(Color::Cyan)),
                        Span::raw(before),
                        Span::styled(cursor_char, cursor_style),
                        Span::raw(rest),
                    ]));
                } else {
                    chrome.push(Line::from(vec![
                        Span::styled(prefix.to_string(), Style::default().fg(Color::Cyan)),
                        Span::raw(vline.text.clone()),
                    ]));
                }
            }
        }
        // Status + info lines so streaming/idle is visible (mu-5h9m): without
        // these you can't tell "processing" from "hung".
        chrome.push(self.format_status_line(width));
        chrome.push(self.format_info_line(width));
        let transcript_rows = total.saturating_sub(chrome.len());

        // One styled renderer for the whole transcript: committed blocks and
        // the live turn go through the same block/turn renderers (no plain
        // downgrade), one blank line between blocks (mu-5h9m). Assistant turns
        // keep their structured `items`, so committed turns look identical to
        // the live one.
        let bwrap = (area.width as usize).saturating_sub(2);
        let preview = if self.bash_yolo { 15 } else { 4 };
        let mut tlines: Vec<Line<'static>> = Vec::new();
        for block in self.transcript.blocks() {
            if !tlines.is_empty() {
                tlines.push(Line::from(""));
            }
            match (block.kind, block.items.as_ref()) {
                (TranscriptKind::User, _) => {
                    // Color committed user turns by route — a /btw turn keeps
                    // its sidecar color (ci-aipr finding).
                    let color = block
                        .route
                        .map(|r| r.you_color())
                        .unwrap_or(ratatui::style::Color::Cyan);
                    tlines.extend(render::block_lines(&block.label, color, &block.body, bwrap));
                }
                (TranscriptKind::Assistant, Some(items)) => {
                    // Route color, not hardcoded white — committed /btw turns
                    // stay magenta in fullscreen (ci-aipr finding).
                    let color = block
                        .route
                        .map(|r| r.color())
                        .unwrap_or(ratatui::style::Color::White);
                    tlines.extend(render::render_turn(
                        &block.label,
                        color,
                        items,
                        bwrap,
                        preview,
                        self.collapse_tools,
                    ));
                    // Committed turns get the closer (the live turn stays open).
                    tlines.extend(render::turn_closer(color));
                }
                (TranscriptKind::Assistant, None) | (TranscriptKind::Notice, _) => {
                    tlines.extend(render::assistant_block(&block.body, bwrap))
                }
                (TranscriptKind::Error, _) => {
                    tlines.extend(render::error_block(&block.body, bwrap))
                }
            }
        }
        if let Some(turn) = self.live_turn.as_ref() {
            if !tlines.is_empty() {
                tlines.push(Line::from(""));
            }
            tlines.extend(render::render_turn(
                turn.route.header_label(),
                turn.route.color(),
                &turn.items,
                bwrap,
                preview,
                self.collapse_tools,
            ));
        }

        // Window: bottom-anchored, minus the scroll-up offset.
        let len = tlines.len();
        let max_off = len.saturating_sub(transcript_rows);
        let off = self.transcript_scroll.min(max_off);
        self.transcript_scroll = off;
        let end = len.saturating_sub(off);
        let start = end.saturating_sub(transcript_rows);
        let mut lines: Vec<Line<'static>> = tlines[start..end].to_vec();
        while lines.len() < transcript_rows {
            lines.push(Line::from(""));
        }
        lines.extend(chrome);

        vp.render(Paragraph::new(lines));
        vp.flush()?;
        Ok(())
    }

    fn render_viewport(&mut self, vp: &mut DynamicViewport) -> Result<()> {
        if self.maximized_block.is_some() {
            return self.render_maximized_block(vp);
        }
        let w = vp.area().width as usize;
        let prompt_wrap_width = w.saturating_sub(4);
        let layout = self.prompt.visual_layout(prompt_wrap_width);
        let menu_rows = if let Some(ref menu) = self.inline_menu {
            let (visible, _, has_above, has_below) = menu.visible_items();
            visible.len() + has_above as usize + has_below as usize
        } else {
            0
        };
        // mu-d04a: render the in-flight turn live, above the prompt,
        // tail-truncated so the prompt always stays visible; the full turn
        // lands in scrollback on commit (session.done/error). focus_mode
        // suppresses the preview — the model is still built and committed.
        let preview: Vec<Line<'static>> = match (self.focus_mode, self.live_turn.as_ref()) {
            (false, Some(turn)) => {
                let tool_preview = if self.bash_yolo { 15 } else { 4 };
                let full = render::render_turn(
                    turn.route.header_label(),
                    turn.route.color(),
                    &turn.items,
                    w.saturating_sub(2),
                    tool_preview,
                    false, // inline live preview keeps full results (mu-5h9m)
                );
                // Reserve chrome (2 separators + status + info = 4) + menu +
                // up to 3 prompt rows; the preview gets the rest up to MAX,
                // which guarantees ≥1 (up to 3) prompt rows stay visible.
                let reserve = 4 + menu_rows + layout.lines.len().min(3);
                let budget = (MAX_VIEWPORT_HEIGHT as usize).saturating_sub(reserve);
                render::tail_truncate(full, budget)
            }
            _ => Vec::new(),
        };
        let selection_rows = if self.selected_block.is_some() { 2 } else { 0 };
        let preview_rows = preview.len();

        let desired_prompt_rows = layout.lines.len().max(PROMPT_ROW_SLACK);
        let desired_height = (preview_rows as u16
            + desired_prompt_rows as u16
            + 4
            + menu_rows as u16
            + selection_rows as u16) // +preview +separator +prompt +selection +separator +status +info
            .clamp(VIEWPORT_HEIGHT, MAX_VIEWPORT_HEIGHT);
        if desired_height != vp.area().height {
            vp.set_height(desired_height)?;
        }

        let area = vp.area();
        let vp_w = area.width as usize;
        let vp_wrap = vp_w.saturating_sub(4);
        // Reuse the layout computed above when the viewport width did not
        // change during set_height. Prompt layout runs on every keypress; doing
        // it twice per frame is needless work in the typing hot path.
        let vp_layout = if vp_w == w {
            layout
        } else {
            self.prompt.visual_layout(vp_wrap)
        };
        let max_prompt_rows =
            (area.height as usize).saturating_sub(4 + menu_rows + preview_rows + selection_rows);
        let prompt_rows = vp_layout.lines.len().min(max_prompt_rows);
        let skip = vp_layout.lines.len().saturating_sub(prompt_rows);
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.extend(preview);
        lines.push(Line::from(Span::styled(
            "─".repeat(vp_w),
            Style::default().fg(Color::DarkGray),
        )));
        if let Some(ref menu) = self.inline_menu {
            let (visible, cursor_pos, has_above, has_below) = menu.visible_items();
            if has_above {
                lines.push(Line::from(Span::styled(
                    "  ↑ more".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            for (vi, (_orig_idx, item)) in visible.iter().enumerate() {
                let is_selected = vi == cursor_pos;
                let name_width = 24.min(vp_w / 3);
                let desc_width = vp_w.saturating_sub(name_width + 4);
                let name_padded = format!("{:<width$}", item.name, width = name_width);
                let desc_trunc = if item.description.len() > desc_width {
                    format!("{}…", &item.description[..desc_width.saturating_sub(1)])
                } else {
                    item.description.clone()
                };
                if is_selected {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {name_padded}"),
                            Style::default().fg(Color::Black).bg(Color::Cyan),
                        ),
                        Span::styled(
                            format!(" {desc_trunc}"),
                            Style::default().fg(Color::DarkGray).bg(Color::Cyan),
                        ),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {name_padded}"),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(
                            format!(" {desc_trunc}"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
            if has_below {
                lines.push(Line::from(Span::styled(
                    "  ↓ more".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        for (display_idx, vline) in vp_layout.lines.iter().skip(skip).enumerate() {
            let row_idx = display_idx + skip;
            let prefix = if row_idx == 0 { " > " } else { "   " };
            let is_cursor_row = row_idx == vp_layout.cursor_row;
            if is_cursor_row {
                let before: String = vline.text.chars().take(vp_layout.cursor_col).collect();
                let after: String = vline.text.chars().skip(vp_layout.cursor_col).collect();
                let cursor_char = if after.is_empty() {
                    " ".to_string()
                } else {
                    after.chars().next().unwrap().to_string()
                };
                let rest: String = after.chars().skip(1).collect();
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(Color::Cyan)),
                    Span::raw(before),
                    Span::styled(
                        cursor_char,
                        Style::default().fg(Color::Black).bg(Color::Cyan),
                    ),
                    Span::raw(rest),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(Color::Cyan)),
                    Span::raw(vline.text.clone()),
                ]));
            }
        }
        if let Some(selected) = self.selected_block {
            if let Some(block) = self.transcript.get(selected) {
                let marker = format!(
                    " ◆ block {}/{}: {}",
                    selected + 1,
                    self.transcript.len(),
                    block.label
                );
                let preview = block
                    .body
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("");
                lines.push(Line::from(Span::styled(
                    truncate_at_word(&marker, vp_w.saturating_sub(1)),
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(vec![
                    Span::styled("   ".to_string(), Style::default().fg(Color::Yellow)),
                    Span::styled(
                        truncate_at_word(preview, vp_w.saturating_sub(4)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            } else {
                self.selected_block = None;
            }
        }
        lines.push(Line::from(Span::styled(
            "─".repeat(vp_w),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(self.format_status_line(vp_w));
        lines.push(self.format_info_line(vp_w));
        let para = Paragraph::new(lines);
        vp.render(para);
        vp.flush()?;
        Ok(())
    }

    /// Handle a single message from the daemon notification stream.
    fn handle_message(&mut self, vp: &mut DynamicViewport, msg: Message) -> Result<()> {
        match msg {
            Message::Notification { method, params } => {
                self.handle_notification(vp, &method, &params)?;
            }
            Message::Eof => {
                let width = vp.area().width as usize;
                let wrap = width.saturating_sub(2);
                let lines = render::error_block("mu serve closed stdout — daemon exited", wrap);
                let h = lines.len() as u16;
                vp.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                })?;
                anyhow::bail!("daemon exited unexpectedly");
            }
            Message::ReaderError(e) => {
                let width = vp.area().width as usize;
                let wrap = width.saturating_sub(2);
                let lines = render::error_block(&format!("reader error: {e}"), wrap);
                let h = lines.len() as u16;
                vp.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                })?;
            }
            Message::Response { id, error, .. } => {
                // mu-d3v6: end-of-turn response for an ask fired via
                // request_nowait. Success is a no-op — session.done
                // already committed the turn. An RPC-level error means
                // the daemon refused/aborted the ask (bad session,
                // provider construction failure): no done will come,
                // so surface the error and clear the streaming state
                // the fire site set up.
                if self.pending_ask_ids.remove(&id) {
                    if let Some(err) = error {
                        self.streaming_route = None;
                        self.live_turn = None;
                        // Terminal repaint (mu-d2hx): no session.done will
                        // come for this ask, so THIS arm must transition
                        // the status projection or the spinner freezes.
                        self.session_phase = SessionPhase::on_turn_end(true, None, None);
                        self.phase_elapsed_ms = 0;
                        let width = vp.area().width as usize;
                        let wrap = width.saturating_sub(2);
                        let lines = render::error_block(&format!("ask failed: {err}"), wrap);
                        let h = lines.len() as u16;
                        vp.insert_before(h, |buf| {
                            let p = Paragraph::new(lines);
                            ratatui::widgets::Widget::render(p, buf.area, buf);
                        })?;
                    }
                }
                // Unknown response ids: tolerate silently (defensive —
                // structurally everything else is sync-routed).
            }
        }
        Ok(())
    }

    /// Handle one keypress. Returns Ok(true) to exit the loop.
    fn handle_key(&mut self, vp: &mut DynamicViewport, key: KeyEvent) -> Result<bool> {
        // Clear any prior flash on the next keystroke. A command that sets a
        // flash does so later in this same call, so the new flash survives
        // until the *following* key (vim-style transient ack, mu-5h9m).
        self.flash = None;
        if self.maximized_block.is_some() {
            return self.handle_maximized_key(vp, key);
        }
        if self.overlay.is_some() {
            return self.handle_overlay_key(vp, key);
        }

        // If an inline menu is open, route keys there first.
        if let Some(ref mut menu) = self.inline_menu {
            match menu.handle_key(key) {
                MenuAction::Continue => return Ok(false),
                MenuAction::Select(idx) => {
                    self.inline_menu = None;
                    let ctx = std::mem::take(&mut self.menu_context);
                    match ctx {
                        MenuContext::SlashCommand => {
                            let items = self.build_slash_menu_items();
                            if let Some(item) = items.get(idx) {
                                let raw = item.name.trim_end_matches(" ›");
                                let cmd = if raw.starts_with('/') {
                                    raw.to_string()
                                } else {
                                    format!("/{raw}")
                                };
                                let takes_arg =
                                    matches!(
                                        cmd.as_str(),
                                        "/btw"
                                            | "/effort"
                                            | "/provider"
                                            | "/model"
                                            | "/config"
                                            | "/focus"
                                            | "/collapse"
                                    ) || self.skills.contains_key(cmd.trim_start_matches('/'));
                                self.prompt.clear();
                                for c in cmd.chars() {
                                    self.prompt.insert_char(c);
                                }
                                if takes_arg {
                                    self.prompt.insert_char(' ');
                                    return Ok(false);
                                }
                                let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
                                return self.handle_key(vp, enter);
                            }
                        }
                        MenuContext::Effort => {
                            if let Some(level) = EffortLevel::ALL.get(idx) {
                                self.effort = *level;
                                self.set_flash(format!("effort → {}", level.as_str()));
                            }
                        }
                    }
                    return Ok(false);
                }
                MenuAction::Dismiss => {
                    let filter = menu.filter().to_string();
                    self.inline_menu = None;
                    self.menu_context = MenuContext::default();
                    if filter.is_empty() {
                        self.prompt.clear();
                    } else {
                        // Keep the typed text — the user was typing a
                        // command that didn't match the menu filter.
                        // Prompt already has "/" from the trigger; add
                        // the filter chars.
                        for c in filter.chars() {
                            self.prompt.insert_char(c);
                        }
                    }
                    return Ok(false);
                }
            }
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::ALT, KeyCode::Up) | (KeyModifiers::ALT, KeyCode::Char('k')) => {
                if self.fullscreen {
                    self.transcript_scroll = self.transcript_scroll.saturating_add(1);
                } else {
                    self.move_selected_block(-1);
                }
            }
            (KeyModifiers::ALT, KeyCode::Down) | (KeyModifiers::ALT, KeyCode::Char('j')) => {
                if self.fullscreen {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(1);
                } else {
                    self.move_selected_block(1);
                }
            }
            (KeyModifiers::NONE, KeyCode::PageUp) if self.fullscreen => {
                self.transcript_scroll = self.transcript_scroll.saturating_add(10);
            }
            (KeyModifiers::NONE, KeyCode::PageDown) if self.fullscreen => {
                self.transcript_scroll = self.transcript_scroll.saturating_sub(10);
            }
            // ctrl+s: dump the record into $EDITOR (hx) — keyboard copy-out
            // that works in fullscreen (mu-5h9m), like the zellij `ctrl+s e`.
            (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
                self.open_in_editor(vp)?;
            }
            (_, KeyCode::Char('c')) if self.prompt.is_empty() && self.selected_block.is_some() => {
                self.apply_block_action(vp, BlockAction::Copy)?;
            }
            (_, KeyCode::Char('p')) if self.prompt.is_empty() && self.selected_block.is_some() => {
                self.apply_block_action(vp, BlockAction::Prompt)?;
            }
            (_, KeyCode::Char('m')) if self.prompt.is_empty() && self.selected_block.is_some() => {
                self.apply_block_action(vp, BlockAction::Maximize)?;
            }
            // Plain Enter submits (chat-TUI convention). Any modified
            // Enter — Shift, Alt, Ctrl, Meta — inserts a newline so
            // multi-line prompts work regardless of which terminal-
            // specific binding the user reaches for (mu-tui precedent,
            // mu-solo-shift-enter-62tx). Needs the kitty-keyboard-
            // protocol push in bin/mu-solo.rs for the modifier to
            // survive the terminal layer; Ctrl-J below is the legacy-
            // terminal fallback (0x0A arrives as Ctrl+'j'). This arm
            // must precede the block-action-menu Enter arm: modified
            // Enter ALWAYS means newline, even with a block selected.
            (m, KeyCode::Enter) if !m.is_empty() => {
                self.selected_block = None;
                self.prompt.insert_char('\n');
            }
            (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                self.selected_block = None;
                self.prompt.insert_char('\n');
            }
            (_, KeyCode::Enter) if self.prompt.is_empty() && self.selected_block.is_some() => {
                self.emit_block_action_menu(vp)?;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if !self.prompt.is_empty() {
                    self.prompt.clear();
                } else if self.live_turn.is_some() {
                    // Cancel in-flight turn
                    self.cmd_cancel(vp)?;
                } else {
                    return Ok(true);
                }
            }
            // Esc clears the prompt buffer (conventional "cancel
            // typed input"). It is NOT an exit shortcut — zellij /
            // tmux multiplexer scrollback exits also send Esc, and
            // having Esc quit mu-solo turned that into an accidental
            // session kill. Quit paths are now: /q, /quit, Ctrl-C.
            (_, KeyCode::Esc) => {
                self.prompt.clear();
                self.selected_block = None;
            }
            // Kill line (Ctrl-U) — clear entire prompt
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.prompt.clear();
            }
            // Word-wise movement (Alt+Left/Right) — must precede bare Left/Right
            (KeyModifiers::ALT, KeyCode::Left) => self.prompt.move_word_left(),
            (KeyModifiers::ALT, KeyCode::Right) => self.prompt.move_word_right(),
            // Cursor movement
            (_, KeyCode::Left) => self.prompt.move_left(),
            (_, KeyCode::Right) => self.prompt.move_right(),
            (_, KeyCode::Home) => self.prompt.move_home(),
            (_, KeyCode::End) => self.prompt.move_end(),
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => self.prompt.move_home(),
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => self.prompt.move_end(),
            // Delete
            (_, KeyCode::Backspace) => {
                self.prompt.delete_before();
            }
            (_, KeyCode::Delete) => {
                self.prompt.delete_after();
            }
            (_, KeyCode::Char('/')) if self.prompt.is_empty() => {
                self.selected_block = None;
                self.prompt.insert_char('/');
                let items = self.build_slash_menu_items();
                let max_visible = vp.area().height.saturating_sub(3) as usize;
                self.inline_menu = Some(InlineMenu::new(items, max_visible.max(5)));
            }
            (_, KeyCode::Char(c)) => {
                self.selected_block = None;
                self.prompt.insert_char(c)
            }
            (_, KeyCode::Enter) => {
                let text = self.prompt.take();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Ok(false);
                }
                // Built-in slash commands handled locally — never
                // sent as prompts to the model. Per claude-code
                // convention (memory ff33f770), slash-commands are
                // the operator surface.
                let (head, tail) = trimmed
                    .split_once(char::is_whitespace)
                    .map(|(h, t)| (h, t.trim()))
                    .unwrap_or((trimmed, ""));
                match head {
                    "/q" | "/quit" | "/exit" if tail.is_empty() => return Ok(true),
                    "/status" if tail.is_empty() => {
                        // Fullscreen paints over insert_before'd output, so
                        // route the panel through the owned-buffer overlay
                        // (mu-5h9m); inline mode keeps the scrollback emit.
                        if self.fullscreen {
                            let lines = self.build_status_lines();
                            self.open_overlay("/status", lines);
                        } else {
                            self.emit_status_lines(vp)?;
                        }
                        return Ok(false);
                    }
                    "/help" => {
                        if self.fullscreen {
                            let lines = self.build_help_lines();
                            self.open_overlay("/help", lines);
                        } else {
                            self.emit_help_lines(vp)?;
                        }
                        return Ok(false);
                    }
                    "/effort" => {
                        self.cmd_effort(vp, tail)?;
                        return Ok(false);
                    }
                    "/focus" => {
                        self.cmd_focus(vp, tail)?;
                        return Ok(false);
                    }
                    "/collapse" => {
                        self.cmd_collapse(vp, tail)?;
                        return Ok(false);
                    }
                    "/btw" => {
                        self.cmd_btw(vp, tail)?;
                        return Ok(false);
                    }
                    "/provider" => {
                        self.cmd_provider(vp, tail)?;
                        return Ok(false);
                    }
                    "/model" => {
                        self.cmd_model(vp, tail)?;
                        return Ok(false);
                    }
                    "/config" => {
                        self.cmd_config(vp, tail)?;
                        return Ok(false);
                    }
                    "/cancel" if tail.is_empty() => {
                        self.cmd_cancel(vp)?;
                        return Ok(false);
                    }
                    "/clear" if tail.is_empty() => {
                        self.cmd_clear(vp)?;
                        return Ok(false);
                    }
                    "/transcript" => {
                        self.cmd_transcript(vp, tail)?;
                        return Ok(false);
                    }
                    "/copy" => {
                        self.cmd_copy(vp, tail)?;
                        return Ok(false);
                    }
                    _ if head.starts_with('/') => {
                        let skill_name = &head[1..];
                        if self.skills.contains_key(skill_name) {
                            self.cmd_skill(vp, skill_name, tail)?;
                            return Ok(false);
                        }
                        self.emit_unknown_command(vp, head)?;
                        return Ok(false);
                    }
                    _ => {}
                }
                self.send_prompt(vp, trimmed)?;
            }
            _ => {}
        }
        Ok(false)
    }

    fn move_selected_block(&mut self, delta: isize) {
        let len = self.transcript.len();
        if len == 0 {
            self.selected_block = None;
            return;
        }
        let current = self
            .selected_block
            .unwrap_or(if delta < 0 { len } else { 0 });
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(len - 1)
        };
        self.selected_block = Some(next);
    }

    fn selected_block(&self) -> Option<&TranscriptBlock> {
        self.selected_block.and_then(|idx| self.transcript.get(idx))
    }

    fn apply_block_action(&mut self, vp: &mut DynamicViewport, action: BlockAction) -> Result<()> {
        let Some(block) = self.selected_block().cloned() else {
            return Ok(());
        };
        match action {
            BlockAction::Copy => {
                let outcome =
                    copy_to_clipboard_or_file(&block.body, self.clipboard_command.as_deref())?;
                self.emit_block_notice(vp, "copied selected block".to_string(), outcome)?;
            }
            BlockAction::Prompt => {
                if !self.prompt.is_empty() {
                    self.prompt.insert_char('\n');
                }
                self.prompt.insert_str(&block.body);
            }
            BlockAction::Maximize => {
                if let Some(index) = self.selected_block {
                    self.maximized_block = Some(MaximizedBlock { index, scroll: 0 });
                    vp.clear_viewport()?;
                }
            }
        }
        Ok(())
    }

    fn render_maximized_block(&mut self, vp: &mut DynamicViewport) -> Result<()> {
        vp.maximize_height()?;
        let width = vp.area().width as usize;
        let height = vp.area().height as usize;
        let Some(state) = self.maximized_block else {
            return Ok(());
        };
        let Some(block) = self.transcript.get(state.index) else {
            self.maximized_block = None;
            return Ok(());
        };

        let body_width = width.saturating_sub(2).max(1);
        let mut body_rows: Vec<String> = Vec::new();
        for logical in block.body.lines() {
            if logical.is_empty() {
                body_rows.push(String::new());
            } else {
                body_rows.extend(render::wrap_line(logical, body_width));
            }
        }
        if body_rows.is_empty() {
            body_rows.push(String::new());
        }

        let body_height = height.saturating_sub(3).max(1);
        let max_scroll = body_rows.len().saturating_sub(body_height);
        let scroll = state.scroll.min(max_scroll);
        if scroll != state.scroll {
            self.maximized_block = Some(MaximizedBlock {
                index: state.index,
                scroll,
            });
        }

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
        let title = format!(
            " block {}/{}: {} ",
            state.index + 1,
            self.transcript.len(),
            block.label
        );
        lines.push(Line::from(vec![
            Span::styled("╭".to_string(), Style::default().fg(Color::Yellow)),
            Span::styled(
                truncate_at_word(&title, width.saturating_sub(2)),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        for row in body_rows.iter().skip(scroll).take(body_height) {
            lines.push(Line::from(vec![
                Span::styled("│ ".to_string(), Style::default().fg(Color::Yellow)),
                Span::raw(row.clone()),
            ]));
        }
        while lines.len() < height.saturating_sub(1) {
            lines.push(Line::from(Span::styled(
                "│".to_string(),
                Style::default().fg(Color::Yellow),
            )));
        }

        let footer = format!(
            " ↑/↓ PgUp/PgDn scroll · c copy · p prompt · Esc close · {}/{} ",
            scroll + 1,
            max_scroll + 1
        );
        lines.push(Line::from(vec![
            Span::styled("╰".to_string(), Style::default().fg(Color::Yellow)),
            Span::styled(
                truncate_at_word(&footer, width.saturating_sub(2)),
                Style::default().fg(Color::DarkGray),
            ),
        ]));

        let para = Paragraph::new(lines);
        vp.render(para);
        vp.flush()?;
        Ok(())
    }

    fn handle_maximized_key(&mut self, vp: &mut DynamicViewport, key: KeyEvent) -> Result<bool> {
        let Some(mut state) = self.maximized_block else {
            return Ok(false);
        };
        let page = vp.area().height.saturating_sub(4).max(1) as usize;
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Ok(true),
            (_, KeyCode::Esc) | (_, KeyCode::Char('q')) => {
                self.maximized_block = None;
                vp.set_height(VIEWPORT_HEIGHT)?;
            }
            (_, KeyCode::Up) | (KeyModifiers::ALT, KeyCode::Char('k')) => {
                state.scroll = state.scroll.saturating_sub(1);
                self.maximized_block = Some(state);
            }
            (_, KeyCode::Down) | (KeyModifiers::ALT, KeyCode::Char('j')) => {
                state.scroll = state.scroll.saturating_add(1);
                self.maximized_block = Some(state);
            }
            (_, KeyCode::PageUp) => {
                state.scroll = state.scroll.saturating_sub(page);
                self.maximized_block = Some(state);
            }
            (_, KeyCode::PageDown) | (_, KeyCode::Char(' ')) => {
                state.scroll = state.scroll.saturating_add(page);
                self.maximized_block = Some(state);
            }
            (_, KeyCode::Home) => {
                state.scroll = 0;
                self.maximized_block = Some(state);
            }
            (_, KeyCode::End) => {
                state.scroll = usize::MAX;
                self.maximized_block = Some(state);
            }
            (_, KeyCode::Char('c')) => {
                if let Some(block) = self.transcript.get(state.index) {
                    let outcome =
                        copy_to_clipboard_or_file(&block.body, self.clipboard_command.as_deref())?;
                    self.maximized_block = None;
                    vp.set_height(VIEWPORT_HEIGHT)?;
                    self.emit_block_notice(vp, "copied selected block".to_string(), outcome)?;
                }
            }
            (_, KeyCode::Char('p')) => {
                if let Some(block) = self.transcript.get(state.index) {
                    if !self.prompt.is_empty() {
                        self.prompt.insert_char('\n');
                    }
                    self.prompt.insert_str(&block.body);
                }
                self.maximized_block = None;
                vp.set_height(VIEWPORT_HEIGHT)?;
            }
            _ => {}
        }
        Ok(false)
    }

    /// Open a modal overlay with pre-built styled lines. Used for slash-command
    /// output (/help, /status) that must be visible in fullscreen, where the
    /// owned-buffer render paints over anything `insert_before`'d (mu-5h9m).
    fn open_overlay(&mut self, title: impl Into<String>, lines: Vec<Line<'static>>) {
        self.overlay = Some(Overlay {
            title: title.into(),
            lines,
            scroll: 0,
        });
    }

    /// Set the ephemeral info-line acknowledgment (cleared on the next key).
    fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = Some(msg.into());
    }

    /// Render the active modal overlay: a full-viewport bordered box of the
    /// overlay's pre-built lines, scrollable, painted via the owned buffer (no
    /// `insert_before`). Mirrors `render_maximized_block`; cyan border to
    /// distinguish it from the yellow maximized-block box (mu-5h9m).
    fn render_overlay(&mut self, vp: &mut DynamicViewport) -> Result<()> {
        vp.maximize_height()?;
        let width = vp.area().width as usize;
        let height = vp.area().height as usize;
        if self.overlay.is_none() {
            return Ok(());
        }

        // Clamp scroll to content (body = viewport minus the top/bottom border).
        let body_height = height.saturating_sub(2).max(1);
        let total = self.overlay.as_ref().unwrap().lines.len();
        let max_scroll = total.saturating_sub(body_height);
        let scroll = self.overlay.as_ref().unwrap().scroll.min(max_scroll);
        self.overlay.as_mut().unwrap().scroll = scroll;
        let overlay = self.overlay.as_ref().unwrap();

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
        let title = format!(" {} ", overlay.title);
        lines.push(Line::from(vec![
            Span::styled("╭".to_string(), Style::default().fg(Color::Cyan)),
            Span::styled(
                truncate_at_word(&title, width.saturating_sub(2)),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        // Body rows: prefix each stored line with the box edge, preserving its
        // own styled spans (so /status keeps its colors). Lines wider than the
        // box are clipped by the Paragraph render — status/help rows are short.
        for line in overlay.lines.iter().skip(scroll).take(body_height) {
            let mut spans = vec![Span::styled(
                "│ ".to_string(),
                Style::default().fg(Color::Cyan),
            )];
            spans.extend(line.spans.iter().cloned());
            lines.push(Line::from(spans));
        }
        while lines.len() < height.saturating_sub(1) {
            lines.push(Line::from(Span::styled(
                "│".to_string(),
                Style::default().fg(Color::Cyan),
            )));
        }
        let (footer, footer_style) = if let Some(flash) = self.flash.clone() {
            (
                format!(" {flash} "),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else if max_scroll > 0 {
            (
                format!(
                    " ↑/↓ PgUp/PgDn scroll · c copy · Esc close · {}/{} ",
                    scroll + 1,
                    max_scroll + 1
                ),
                Style::default().fg(Color::DarkGray),
            )
        } else {
            (
                " c copy · Esc close ".to_string(),
                Style::default().fg(Color::DarkGray),
            )
        };
        lines.push(Line::from(vec![
            Span::styled("╰".to_string(), Style::default().fg(Color::Cyan)),
            Span::styled(
                truncate_at_word(&footer, width.saturating_sub(2)),
                footer_style,
            ),
        ]));

        vp.render(Paragraph::new(lines));
        vp.flush()?;
        Ok(())
    }

    /// Key handling while a modal overlay is open: scroll or dismiss. Mirrors
    /// `handle_maximized_key`. Esc/q closes; in inline mode the viewport is
    /// shrunk back to its normal height (fullscreen re-maximizes on the next
    /// frame, so no shrink there).
    fn handle_overlay_key(&mut self, vp: &mut DynamicViewport, key: KeyEvent) -> Result<bool> {
        let Some(mut overlay) = self.overlay.take() else {
            return Ok(false);
        };
        let page = vp.area().height.saturating_sub(3).max(1) as usize;
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Ok(true),
            (_, KeyCode::Esc) | (_, KeyCode::Char('q')) => {
                // Overlay already taken (stays closed).
                if !self.fullscreen {
                    vp.set_height(VIEWPORT_HEIGHT)?;
                }
                return Ok(false);
            }
            (_, KeyCode::Char('c')) => {
                // Copy the panel's text out (e.g. /status session/daemon ids) —
                // ctrl+s and terminal selection don't reach the owned buffer, so
                // the overlay needs its own copy-out (mu-5h9m). Flatten each
                // styled line back to plain text.
                let text = overlay
                    .lines
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // Don't `?` here: the overlay was taken out at the top of this
                // fn and is only restored at the bottom — propagating would leak
                // it (modal silently dies). Report the outcome via flash either
                // way and fall through to the restore (ci-aipr finding).
                match copy_to_clipboard_or_file(text.trim(), self.clipboard_command.as_deref()) {
                    Ok(outcome) => {
                        self.set_flash(format!("✓ copied {} · {outcome}", overlay.title))
                    }
                    Err(e) => self.set_flash(format!("copy failed: {e}")),
                }
            }
            (_, KeyCode::Up) | (KeyModifiers::ALT, KeyCode::Char('k')) => {
                overlay.scroll = overlay.scroll.saturating_sub(1);
            }
            (_, KeyCode::Down) | (KeyModifiers::ALT, KeyCode::Char('j')) => {
                overlay.scroll = overlay.scroll.saturating_add(1);
            }
            (_, KeyCode::PageUp) => {
                overlay.scroll = overlay.scroll.saturating_sub(page);
            }
            (_, KeyCode::PageDown) | (_, KeyCode::Char(' ')) => {
                overlay.scroll = overlay.scroll.saturating_add(page);
            }
            (_, KeyCode::Home) => overlay.scroll = 0,
            (_, KeyCode::End) => overlay.scroll = usize::MAX,
            _ => {}
        }
        self.overlay = Some(overlay);
        Ok(false)
    }

    fn emit_block_action_menu(&self, vp: &mut DynamicViewport) -> Result<()> {
        let Some(block) = self.selected_block() else {
            return Ok(());
        };
        let first = block.body.lines().next().unwrap_or("");
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("selected block: {}", block.label),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("  {}", truncate_at_word(first, 90))),
            Line::from("  c copy · p copy into prompt · m maximize · Esc clear selection"),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    fn emit_block_notice(
        &self,
        vp: &mut DynamicViewport,
        title: String,
        detail: String,
    ) -> Result<()> {
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                title,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("  {detail}")),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// Send a user prompt: emit the "you" block to scrollback, then
    /// fire `session.ask`.
    fn send_prompt(&mut self, vp: &mut DynamicViewport, text: &str) -> Result<()> {
        self.selected_block = None;
        self.transcript
            .push(TranscriptBlock::user(TurnRoute::Main, text.to_string()));
        self.emit_you_block(vp, text)?;
        self.fire_ask(vp, text)
    }

    /// Show the "you" block in scrollback without sending anything.
    fn emit_you_block(&self, vp: &mut DynamicViewport, display_text: &str) -> Result<()> {
        vp.clear_viewport()?;
        let width = vp.area().width as usize;
        let wrap_width = width.saturating_sub(2);
        let lines = render::you_block(display_text, wrap_width);
        let height = lines.len() as u16;
        vp.insert_before(height, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// Reset streaming state, snap viewport, and fire `ask_session`.
    fn fire_ask(&mut self, vp: &mut DynamicViewport, wire_text: &str) -> Result<()> {
        vp.snap_to_bottom()?;
        self.streaming_route = Some(TurnRoute::Main);
        self.live_turn = Some(Turn::new(TurnRoute::Main));
        // Repaint the status projection IMMEDIATELY (mu-d2hx sighting ii:
        // after an error + manual retry the indicators stayed stale until
        // the first provider_status arrived — if it ever did). Firing an
        // ask both clears any terminal notice and starts the new turn's
        // status from a known state.
        self.session_phase = SessionPhase::AwaitingFirstToken;
        self.phase_elapsed_ms = 0;

        // mu-d3v6: fire WITHOUT blocking. ask_session's response only
        // arrives when the turn completes; waiting here parked the
        // event loop for the whole turn (no delta rendering, and turns
        // longer than the RPC timeout spuriously errored). The
        // response is delivered to the select loop as a
        // Message::Response and handled in handle_message.
        // mu-vcbm: carry the current `/effort` dial selection on every
        // ask. The daemon applies it stickily (idempotent when unchanged),
        // so the session's standing effort always tracks the dial.
        let id = self.client.request_nowait(
            "ask_session",
            serde_json::json!({
                "session_id": self.session_id,
                "user_message": wire_text,
                "effort": self.effort.as_str(),
            }),
        )?;
        self.pending_ask_ids.insert(id);
        Ok(())
    }

    /// /btw <message> — fire a side question to a sidecar session
    /// without polluting the main session's history. Lazily creates
    /// the sidecar via `session.delegate` (mu-031) on first use, then
    /// reuses it across subsequent /btw calls so follow-ups thread
    /// coherently in the side conversation.
    ///
    /// v0 constraint: only one in-flight turn at a time across both
    /// routes. If main is streaming, /btw refuses with a hint to wait.
    fn cmd_btw(&mut self, vp: &mut DynamicViewport, msg: &str) -> Result<()> {
        if msg.is_empty() {
            let lines: Vec<Line<'static>> = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "usage: /btw <message>".to_string(),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("  fires a side question to a sidecar session;"),
                Line::from("  main session history is unaffected."),
                Line::from(""),
            ];
            let h = lines.len() as u16;
            vp.insert_before(h, |buf| {
                let p = Paragraph::new(lines);
                ratatui::widgets::Widget::render(p, buf.area, buf);
            })?;
            return Ok(());
        }
        if self.live_turn.is_some() {
            let lines: Vec<Line<'static>> = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "wait — main turn still streaming".to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("  retry /btw once the current response finishes."),
                Line::from(""),
            ];
            let h = lines.len() as u16;
            vp.insert_before(h, |buf| {
                let p = Paragraph::new(lines);
                ratatui::widgets::Widget::render(p, buf.area, buf);
            })?;
            return Ok(());
        }

        // Lazily create the sidecar session via session.delegate.
        // Parent linkage gives audit trail; child has its own event
        // log so its turns never appear in the main session's
        // history. Provider/model mirrors the main session — could
        // become an arg later.
        if self.sidecar_session_id.is_none() {
            let kind = normalize_provider_kind(&self.provider);
            let resp = self
                .client
                .request(
                    "session.delegate",
                    serde_json::json!({
                        "parent_session_id": self.session_id,
                        "provider": { "kind": kind, "model": self.model },
                    }),
                )
                .context("session.delegate failed (sidecar creation)")?;
            let child_id = resp
                .get("child_session_id")
                .and_then(|v| v.as_str())
                .context("session.delegate response missing child_session_id")?
                .to_string();
            self.sidecar_session_id = Some(child_id);
        }
        let sid = self
            .sidecar_session_id
            .as_ref()
            .expect("sidecar_session_id set above")
            .clone();

        // Emit a "you ⋅ btw" block in magenta so it's visually
        // distinct from the main cyan "you" blocks.
        let width = vp.area().width as usize;
        let wrap_width = width.saturating_sub(2);
        let route = TurnRoute::Btw;
        self.transcript
            .push(TranscriptBlock::user(route, msg.to_string()));
        let lines = render::block_lines(route.you_label(), route.you_color(), msg, wrap_width);
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;

        // Route this turn to the sidecar and start a fresh live turn.
        self.streaming_route = Some(route);
        self.live_turn = Some(Turn::new(route));
        // Same status-projection repaint as fire_ask (mu-d2hx): a /btw
        // turn is in flight now; the status must say so immediately.
        self.session_phase = SessionPhase::AwaitingFirstToken;
        self.phase_elapsed_ms = 0;

        // mu-d3v6: non-blocking, same as fire_ask.
        let id = self.client.request_nowait(
            "ask_session",
            serde_json::json!({
                "session_id": sid,
                "user_message": msg,
            }),
        )?;
        self.pending_ask_ids.insert(id);
        Ok(())
    }

    /// /status — print provider, model, session_id, daemon_id, version
    /// to scrollback. Lets the operator find the daemon's events
    /// directory: ~/.local/share/mu/events/{daemon_id}/session-N.jsonl
    fn build_status_lines(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "── /status ─────────────────────────".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("  provider:    {}", self.provider)),
            Line::from(format!("  model:       {}", self.model)),
            {
                // Renderer/cache row: surfaces the daemon's actual
                // resolved provider plumbing so a silent faux-fallback
                // is visible from /status without needing the
                // warning to fire again. Yellow when mismatched.
                let r = self.actual_renderer.as_deref().unwrap_or("(pending)");
                let c = self.actual_cache_strategy.as_deref().unwrap_or("(pending)");
                let text = format!("  renderer:    {r} · cache: {c}");
                if self.is_renderer_mismatch() {
                    Line::from(Span::styled(
                        format!("{text}  ⚠ asked != running"),
                        Style::default().fg(Color::Yellow),
                    ))
                } else {
                    Line::from(text)
                }
            },
            Line::from(format!("  effort:      {}", self.effort.as_str())),
            Line::from(format!(
                "  focus:       {} (suppress streaming preview)",
                if self.focus_mode { "on" } else { "off" }
            )),
            Line::from(format!(
                "  tokens:      {}k in · {}k out (asks: {})",
                self.cumulative_input_tokens / 1000,
                self.cumulative_output_tokens / 1000,
                self.ask_count,
            )),
            {
                let cost = self.compute_cost();
                if cost > 0.0 {
                    Line::from(format!("  cost:        ${cost:.4}"))
                } else {
                    Line::from("  cost:        (unknown — no pricing for this provider/model)")
                }
            },
            Line::from(format!("  session_id:  {}", self.session_id)),
            Line::from(format!(
                "  sidecar:     {} (/btw)",
                self.sidecar_session_id
                    .as_deref()
                    .unwrap_or("(none — created on first /btw)")
            )),
            Line::from(format!("  daemon_id:   {}", self.daemon_id)),
            Line::from(format!("  daemon ver:  {}", self.daemon_version)),
            Line::from(format!(
                "  events:      ~/.local/share/mu/events/{}/session-1.jsonl",
                self.daemon_id
            )),
            Line::from(""),
        ]
    }

    fn emit_status_lines(&self, vp: &mut DynamicViewport) -> Result<()> {
        let lines = self.build_status_lines();
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /effort — show or set the session-level effort dial (§17).
    /// Bare `/effort` prints the current value plus valid choices;
    /// `/effort <level>` sets it. v0 is display-only — the value
    /// surfaces in /status and the banner; once the daemon learns
    /// an effort field on `ask_session`, it will attach here.
    fn cmd_effort(&mut self, vp: &mut DynamicViewport, arg: &str) -> Result<()> {
        let lines: Vec<Line<'static>> = if arg.is_empty() {
            let items: Vec<MenuItem> = EffortLevel::ALL
                .iter()
                .map(|e| {
                    let current = if *e == self.effort { " (current)" } else { "" };
                    MenuItem::new(
                        e.as_str(),
                        format!(
                            "{}{current}",
                            match e {
                                EffortLevel::Low => "Quick, concise responses",
                                EffortLevel::Medium => "Balanced depth and speed",
                                EffortLevel::High => "Thorough, detailed work",
                                EffortLevel::XHigh => "Extra thorough, multi-angle",
                                EffortLevel::Max => "Maximum depth, no shortcuts",
                            }
                        ),
                    )
                })
                .collect();
            let max_visible = vp.area().height.saturating_sub(3) as usize;
            self.inline_menu = Some(InlineMenu::new(items, max_visible.max(5)));
            self.menu_context = MenuContext::Effort;
            return Ok(());
        } else if let Some(level) = EffortLevel::parse(arg) {
            self.effort = level;
            self.set_flash(format!("effort → {}", level.as_str()));
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("effort → {}", level.as_str()),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ]
        } else {
            let choices: Vec<String> = EffortLevel::ALL
                .iter()
                .map(|e| e.as_str().to_string())
                .collect();
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("unknown effort level: {arg:?}"),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("  choices:  {}", choices.join(" · "))),
                Line::from(""),
            ]
        };
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /focus — toggle suppression of streaming text_delta previews
    /// (§16). `/focus` alone toggles; `/focus on|off` sets explicitly.
    /// When on, only the finalized assistant block lands in
    /// scrollback — useful for long autonomous runs where you don't
    /// want to scroll past partial chunks.
    fn cmd_focus(&mut self, vp: &mut DynamicViewport, arg: &str) -> Result<()> {
        let new_value = match arg.trim().to_lowercase().as_str() {
            "" | "toggle" => !self.focus_mode,
            "on" | "true" | "1" | "yes" => true,
            "off" | "false" | "0" | "no" => false,
            other => {
                // Flash so the error is visible in fullscreen (insert_before is
                // painted over there); inline keeps the scrollback notice
                // (ci-aipr finding).
                self.set_flash(format!("unknown focus arg {other:?} — use on|off|toggle"));
                let lines: Vec<Line<'static>> = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("unknown focus arg: {other:?}"),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  usage:    /focus [on|off|toggle]"),
                    Line::from(""),
                ];
                let h = lines.len() as u16;
                vp.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                })?;
                return Ok(());
            }
        };
        self.focus_mode = new_value;
        self.set_flash(format!("focus → {}", if new_value { "on" } else { "off" }));
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("focus → {}", if new_value { "on" } else { "off" }),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /collapse — fold completed tool call+result blocks to one-liners in the
    /// fullscreen render (mu-5h9m). `/collapse` toggles; `/collapse on|off`
    /// sets. Fullscreen-only effect; the flash ack is the visible signal.
    fn cmd_collapse(&mut self, vp: &mut DynamicViewport, arg: &str) -> Result<()> {
        let new_value = match arg.trim().to_lowercase().as_str() {
            "" | "toggle" => !self.collapse_tools,
            "on" | "true" | "1" | "yes" => true,
            "off" | "false" | "0" | "no" => false,
            other => {
                // Fullscreen paints over insert_before, so the error needs the
                // flash to be visible there; inline keeps the scrollback notice
                // (ci-aipr finding).
                self.set_flash(format!(
                    "unknown collapse arg {other:?} — use on|off|toggle"
                ));
                if !self.fullscreen {
                    let lines: Vec<Line<'static>> = vec![
                        Line::from(""),
                        Line::from(Span::styled(
                            format!("unknown collapse arg: {other:?}"),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        )),
                        Line::from("  usage:    /collapse [on|off|toggle]"),
                        Line::from(""),
                    ];
                    let h = lines.len() as u16;
                    vp.insert_before(h, |buf| {
                        let p = Paragraph::new(lines);
                        ratatui::widgets::Widget::render(p, buf.area, buf);
                    })?;
                }
                return Ok(());
            }
        };
        self.collapse_tools = new_value;
        self.set_flash(format!(
            "tools {}",
            if new_value { "collapsed" } else { "expanded" }
        ));
        if !self.fullscreen {
            let lines: Vec<Line<'static>> = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("tools {}", if new_value { "collapsed" } else { "expanded" }),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("  (collapse only affects the fullscreen render)"),
                Line::from(""),
            ];
            let h = lines.len() as u16;
            vp.insert_before(h, |buf| {
                let p = Paragraph::new(lines);
                ratatui::widgets::Widget::render(p, buf.area, buf);
            })?;
        }
        Ok(())
    }

    /// /provider [name] — switch the session's provider. Bare
    /// `/provider` opens a modal picker; `/provider <name>` sets
    /// directly. Sends `session.set_route` to the daemon; the switch
    /// takes effect on the next turn.
    fn cmd_provider(&mut self, vp: &mut DynamicViewport, arg: &str) -> Result<()> {
        let new_provider = if arg.is_empty() {
            let items: Vec<String> = KNOWN_PROVIDERS.iter().map(|s| (*s).to_string()).collect();
            let current = items.iter().position(|s| s == &self.provider).unwrap_or(0);
            match picker::run_picker("/provider", &items, current)? {
                Some(idx) => items[idx].clone(),
                None => return Ok(()),
            }
        } else {
            arg.to_string()
        };

        let kind = normalize_provider_kind(&new_provider);
        let models = known_models_for(&kind);
        let default_model = models
            .first()
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.model.clone());

        match self.send_set_route(vp, &kind, &default_model) {
            Ok(()) => {
                self.provider = new_provider;
                self.model = default_model;
                self.set_flash(format!("provider → {} · {}", self.provider, self.model));
            }
            Err(e) => {
                self.set_flash(format!("provider switch failed: {e}"));
                let lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("provider switch failed: {e}"),
                        Style::default().fg(Color::Red),
                    )),
                    Line::from(""),
                ];
                let h = lines.len() as u16;
                vp.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                })?;
            }
        }
        Ok(())
    }

    /// /model [name] — switch the session's model. Bare `/model` opens
    /// a picker scoped to the current provider; `/model <name>` sets
    /// directly. Sends `session.set_route` to the daemon.
    fn cmd_model(&mut self, vp: &mut DynamicViewport, arg: &str) -> Result<()> {
        let new_model = if arg.is_empty() {
            let kind = normalize_provider_kind(&self.provider);
            let known = known_models_for(&kind);
            if known.is_empty() {
                let lines: Vec<Line<'static>> = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("no curated model list for provider {:?}", self.provider),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from("  use /model <name> to set directly"),
                    Line::from(""),
                ];
                let h = lines.len() as u16;
                vp.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                })?;
                return Ok(());
            }
            let mut items: Vec<String> = Vec::with_capacity(known.len() + 1);
            if !known.iter().any(|m| *m == self.model) {
                items.push(self.model.clone());
            }
            items.extend(known.iter().map(|s| (*s).to_string()));
            let current = items.iter().position(|s| s == &self.model).unwrap_or(0);
            match picker::run_picker("/model", &items, current)? {
                Some(idx) => items[idx].clone(),
                None => return Ok(()),
            }
        } else {
            arg.to_string()
        };

        let kind = normalize_provider_kind(&self.provider);
        match self.send_set_route(vp, &kind, &new_model) {
            Ok(()) => {
                self.model = new_model;
                self.set_flash(format!("model → {}", self.model));
            }
            Err(e) => {
                self.set_flash(format!("model switch failed: {e}"));
                let lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("model switch failed: {e}"),
                        Style::default().fg(Color::Red),
                    )),
                    Line::from(""),
                ];
                let h = lines.len() as u16;
                vp.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                })?;
            }
        }
        Ok(())
    }

    /// Send `session.set_route` to the daemon and emit a success banner
    /// on success. Returns Err with the error message on failure.
    fn send_set_route(
        &mut self,
        vp: &mut DynamicViewport,
        provider_kind: &str,
        model: &str,
    ) -> Result<(), String> {
        let selector = serde_json::json!({
            "kind": provider_kind,
            "model": model,
        });
        let params = serde_json::json!({
            "session_id": self.session_id,
            "provider": selector,
        });
        match self.client.request("session.set_route", params) {
            Ok(_resp) => {
                let lines = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("switched → {provider_kind} / {model}"),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                ];
                let h = lines.len() as u16;
                let _ = vp.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                });
                Ok(())
            }
            Err(e) => Err(format!("{e}")),
        }
    }

    /// /config — read or write session config over the generic
    /// capability-gated config message (mu-context-limits-wire phase 2).
    ///
    ///   /config [get] [key...]      read keys (default: context.soft_limit;
    ///                               `*` for the whole readable config)
    ///   /config set <key> <value>   write one key, e.g.
    ///                               `/config set context.soft_limit 120000`
    ///
    /// The set takes effect live: the daemon updates the running loop's
    /// compaction trigger AND records the change so the context meter
    /// reflects it on the next status tick.
    fn cmd_config(&mut self, vp: &mut DynamicViewport, arg: &str) -> Result<()> {
        let mut parts = arg.split_whitespace();
        let first = parts.next().unwrap_or("get");
        let mut lines: Vec<Line> = vec![Line::from("")];
        match first {
            "set" => {
                let key = match parts.next() {
                    Some(k) => k.to_string(),
                    None => {
                        self.set_flash("usage: /config set <key> <value>");
                        return Ok(());
                    }
                };
                let raw = parts.next().unwrap_or("");
                // Integers parse as JSON numbers (what context.soft_limit
                // wants); anything else is sent as a string and the daemon
                // validates per key.
                let value: serde_json::Value = raw
                    .parse::<u64>()
                    .map(serde_json::Value::from)
                    .unwrap_or_else(|_| serde_json::Value::from(raw));
                let params = serde_json::json!({
                    "session_id": self.session_id,
                    "entries": [{ "key": key, "value": value }],
                });
                match self.client.request("session.set_config", params) {
                    Ok(v) => {
                        let applied = v
                            .get("applied")
                            .and_then(|a| a.as_array())
                            .map(|a| !a.is_empty())
                            .unwrap_or(false);
                        if applied {
                            lines.push(Line::from(Span::styled(
                                format!("config set: {key} = {value}"),
                                Style::default().fg(Color::Green),
                            )));
                            self.set_flash(format!("config set {key}"));
                        }
                        if let Some(rej) = v.get("rejected").and_then(|r| r.as_array()) {
                            for r in rej {
                                let k = r.get("key").and_then(|x| x.as_str()).unwrap_or("?");
                                let reason =
                                    r.get("reason").and_then(|x| x.as_str()).unwrap_or("?");
                                lines.push(Line::from(Span::styled(
                                    format!("rejected {k}: {reason}"),
                                    Style::default().fg(Color::Red),
                                )));
                            }
                        }
                    }
                    Err(e) => lines.push(Line::from(Span::styled(
                        format!("set_config failed: {e}"),
                        Style::default().fg(Color::Red),
                    ))),
                }
            }
            sub => {
                // "get" (explicit) or bare keys; default to the soft limit.
                let mut keys: Vec<String> = parts.map(|s| s.to_string()).collect();
                if sub != "get" {
                    keys.insert(0, sub.to_string());
                }
                if keys.is_empty() {
                    keys.push("context.soft_limit".to_string());
                }
                let params = serde_json::json!({
                    "session_id": self.session_id,
                    "keys": keys,
                });
                match self.client.request("session.get_config", params) {
                    Ok(v) => {
                        lines.push(Line::from(Span::styled(
                            "config:",
                            Style::default().fg(Color::Cyan),
                        )));
                        match v.get("values").and_then(|x| x.as_object()) {
                            Some(map) if !map.is_empty() => {
                                for (k, val) in map {
                                    lines.push(Line::from(format!("  {k} = {val}")));
                                }
                            }
                            _ => lines.push(Line::from("  (no matching keys)")),
                        }
                    }
                    Err(e) => lines.push(Line::from(Span::styled(
                        format!("get_config failed: {e}"),
                        Style::default().fg(Color::Red),
                    ))),
                }
            }
        }
        lines.push(Line::from(""));
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /cancel — abort the in-flight provider call without ending the
    /// session. Maps to `session.cancel_outstanding` (mu-035). Routes
    /// to whichever session owns the current streaming turn (main or
    /// /btw sidecar) so cancelling a side question doesn't kill the
    /// main turn or vice versa. Idempotent — if nothing is in flight,
    /// the daemon returns `canceled: false` and we say so.
    fn cmd_cancel(&mut self, vp: &mut DynamicViewport) -> Result<()> {
        // Pick the session whose turn is currently streaming. Fall
        // back to the main session if nothing is in flight — the
        // daemon will tell us it was idle.
        let sid = match self.streaming_route {
            Some(TurnRoute::Btw) => self
                .sidecar_session_id
                .clone()
                .unwrap_or_else(|| self.session_id.clone()),
            _ => self.session_id.clone(),
        };
        let resp = self
            .client
            .request(
                "session.cancel_outstanding",
                serde_json::json!({
                    "session_id": sid,
                    "reason": "user pressed /cancel",
                }),
            )
            .context("session.cancel_outstanding RPC failed")?;
        let canceled = resp
            .get("canceled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let was_in = resp
            .get("was_in")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let lines: Vec<Line<'static>> = if canceled {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "/cancel — provider call aborted".to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!("  was_in: {was_in}"),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "/cancel — nothing in flight".to_string(),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    format!("  was_in: {was_in}"),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ]
        };
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /clear — clear the visible scrollback. Doesn't touch the
    /// daemon's event log; this is a display-only reset. The inline
    /// viewport redraws on the next tick.
    fn cmd_clear(&mut self, vp: &mut DynamicViewport) -> Result<()> {
        // Drop the in-memory transcript. In fullscreen the owned-buffer render
        // windows `self.transcript`, so this visibly empties the screen the next
        // frame (mu-5h9m). Inline mode also clears the live viewport region;
        // terminal scrollback above it belongs to the multiplexer and is left
        // alone (it's the user's scroll history, not ours to wipe).
        self.transcript.clear();
        self.transcript_scroll = 0;
        self.selected_block = None;
        if !self.fullscreen {
            vp.clear_viewport()?;
        }
        Ok(())
    }

    /// /transcript [PATH] — write the semantic transcript projection to a file.
    /// Bare command writes to a temp file and prints the path. This reads the
    /// in-memory semantic record, not rendered terminal cells.
    /// Dump the record to a temp file and hand the terminal to `$EDITOR`/`hx`,
    /// then take it back. Keyboard copy-out (`ctrl+s`) that survives fullscreen
    /// (mu-5h9m): mirrors the `ctrl+s e` zellij→editor habit, but reads mu's own
    /// record, so it works without a terminal scrollback buffer to dump.
    fn open_in_editor(&mut self, _vp: &mut DynamicViewport) -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "mu-solo-transcript-{}-{}.md",
            std::process::id(),
            self.ask_count
        ));
        std::fs::write(&path, self.transcript.render_all_plain())
            .with_context(|| format!("write transcript to {path:?}"))?;
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "hx".to_string());
        // Hand the terminal to the editor cleanly, then restore EVERY mode set
        // at startup — not just raw mode. Helix (and most modern editors) speak
        // the kitty keyboard protocol and reset terminal keyboard state on exit,
        // which silently drops our DISAMBIGUATE_ESCAPE_CODES push; without
        // re-arming it, modified Enter (Shift-Enter → newline) collapses back to
        // plain CR until restart (mu-5h9m, mu-solo-shift-enter-62tx). Mirror the
        // startup/shutdown sequence in bin/mu-solo.rs: pop our flags before the
        // editor (clean slate, keeps the kitty stack balanced) and re-push after.
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableFocusChange,
            crossterm::event::PopKeyboardEnhancementFlags,
            crossterm::event::DisableBracketedPaste,
        );
        crossterm::terminal::disable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), crossterm::cursor::Show)?;
        let status = std::process::Command::new(&editor).arg(&path).status();
        crossterm::terminal::enable_raw_mode()?;
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::EnableBracketedPaste,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            ),
            crossterm::event::EnableFocusChange,
        );
        crossterm::execute!(std::io::stdout(), crossterm::cursor::Hide)?;
        if let Err(e) = status {
            tracing::warn!("editor '{editor}' spawn failed: {e}");
        }
        Ok(())
    }

    fn cmd_transcript(&mut self, vp: &mut DynamicViewport, arg: &str) -> Result<()> {
        let path = if arg.is_empty() {
            std::env::temp_dir().join(format!(
                "mu-solo-transcript-{}-{}.md",
                std::process::id(),
                self.ask_count
            ))
        } else {
            std::path::PathBuf::from(arg)
        };
        let text = self.transcript.render_all_plain();
        std::fs::write(&path, text).with_context(|| format!("write transcript to {path:?}"))?;
        self.set_flash(format!("✓ transcript → {}", path.display()));
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                "transcript written".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("  {}", path.display())),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /copy [last|assistant|user|all] — copy semantic transcript content.
    fn cmd_copy(&mut self, vp: &mut DynamicViewport, arg: &str) -> Result<()> {
        let selector = if arg.is_empty() { "last" } else { arg };
        let text = match selector {
            "last" => self.transcript.last().map(|b| b.body.clone()),
            "assistant" | "answer" => self
                .transcript
                .last_matching(TranscriptKind::Assistant)
                .map(|b| b.body.clone()),
            "user" | "prompt" => self
                .transcript
                .last_matching(TranscriptKind::User)
                .map(|b| b.body.clone()),
            "all" => Some(self.transcript.render_all_plain()),
            _ => None,
        };
        let Some(text) = text else {
            self.set_flash(format!("nothing to copy ({selector})"));
            let lines: Vec<Line<'static>> = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("nothing to copy for selector {selector:?}"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("  usage: /copy [last|assistant|user|all]"),
                Line::from(""),
            ];
            let h = lines.len() as u16;
            vp.insert_before(h, |buf| {
                let p = Paragraph::new(lines);
                ratatui::widgets::Widget::render(p, buf.area, buf);
            })?;
            return Ok(());
        };

        let outcome = copy_to_clipboard_or_file(&text, self.clipboard_command.as_deref())?;
        self.set_flash(format!("✓ copied {selector} · {outcome}"));
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("copied {selector}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("  {outcome}")),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// Invoke a discovered skill. Injects the skill body as context
    /// by prepending it to the user's message. If no message was
    /// provided, sends just the skill body with a brief preamble.
    fn cmd_skill(&mut self, vp: &mut DynamicViewport, skill_name: &str, tail: &str) -> Result<()> {
        if self.live_turn.is_some() {
            let lines: Vec<Line<'static>> = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "wait — turn still streaming".to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "  retry /{skill_name} once the current response finishes."
                )),
                Line::from(""),
            ];
            let h = lines.len() as u16;
            vp.insert_before(h, |buf| {
                let p = Paragraph::new(lines);
                ratatui::widgets::Widget::render(p, buf.area, buf);
            })?;
            return Ok(());
        }

        let skill = match self.skills.get(skill_name) {
            Some(s) => s.clone(),
            None => {
                self.emit_unknown_command(vp, &format!("/{skill_name}"))?;
                return Ok(());
            }
        };

        // Activation banner — the only visual feedback.
        let banner_lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("  /{skill_name}"),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" — {}", skill.description),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(""),
        ];
        let bh = banner_lines.len() as u16;
        vp.insert_before(bh, |buf| {
            let p = Paragraph::new(banner_lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;

        // Build the wire message: skill body is invisible context,
        // only the user's message (if any) shows in scrollback.
        let injection = skill.injection_text();
        let wire_msg = if tail.is_empty() {
            format!(
                "The user activated the /{skill_name} skill. \
                 Follow the instructions below.\n\n{injection}"
            )
        } else {
            format!("{injection}\n\n---\n\nUser request: {tail}")
        };

        if !tail.is_empty() {
            self.emit_you_block(vp, tail)?;
        }
        self.fire_ask(vp, &wire_msg)?;
        Ok(())
    }

    /// Unknown-command stub. Keeps typos from getting sent to the
    /// model as a prompt (which would burn tokens and confuse the
    /// session). Mirrors claude-code's "Unknown slash command" hint.
    fn emit_unknown_command(&self, vp: &mut DynamicViewport, head: &str) -> Result<()> {
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("unknown command: {head}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::from("  /help for the built-in command list"),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /help — print the built-in command surface to scrollback.
    fn build_slash_menu_items(&self) -> Vec<MenuItem> {
        let mut items = vec![
            MenuItem::new("/status", "Current provider / model / session / daemon"),
            MenuItem::new("/help", "Show help for commands"),
            MenuItem::new("/effort ›", "Select effort level"),
            MenuItem::new("/focus", "Toggle focus mode (suppress streaming preview)"),
            MenuItem::new("/collapse", "Fold tool call+result blocks to one-liners"),
            MenuItem::new("/provider ›", "Select provider"),
            MenuItem::new("/model ›", "Select model"),
            MenuItem::new(
                "/config",
                "Read/set session config (e.g. context.soft_limit)",
            ),
            MenuItem::new(
                "/btw",
                "Side question via sidecar (main history unaffected)",
            ),
            MenuItem::new("/cancel", "Abort the in-flight provider call"),
            MenuItem::new("/clear", "Clear the visible scrollback"),
            MenuItem::new("/transcript", "Write semantic transcript to a file"),
            MenuItem::new("/copy", "Copy last/assistant/user/all semantic content"),
            MenuItem::new("/quit", "Leave the session (/q, /exit)"),
            MenuItem::new("/exit", "Leave the session"),
        ];
        let mut skill_names: Vec<&str> = self.skills.keys().map(|s| s.as_str()).collect();
        skill_names.sort();
        for name in skill_names {
            if let Some(skill) = self.skills.get(name) {
                items.push(MenuItem::new(format!("/{name}"), skill.description.clone()));
            }
        }
        items
    }

    fn build_help_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                "── /help ───────────────────────────".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("  /status            current provider / model / session / daemon"),
            Line::from("  /help              show this list"),
            Line::from("  /effort [LEVEL]    show or set effort: low|medium|high|xhigh|max"),
            Line::from("  /focus [on|off]    toggle focus mode (suppress streaming preview)"),
            Line::from(
                "  /collapse [on|off] fold completed tool blocks to one-liners (fullscreen)",
            ),
            Line::from("  /provider [name]   list-picker (bare) or set directly"),
            Line::from("  /model [name]      list-picker (bare) or set directly"),
            Line::from("  /btw <message>     side question via sidecar (main history unaffected)"),
            Line::from("  /cancel            abort the in-flight provider call"),
            Line::from("  /clear             clear the visible scrollback"),
            Line::from("  /transcript [PATH] write semantic transcript to PATH/tempfile"),
            Line::from("  /copy [WHAT]       copy last|assistant|user|all semantically"),
            Line::from("  Alt-Up/Down or Alt-k/j select previous/next semantic block"),
            Line::from("  c / p / m          copy / copy into prompt / maximize selection"),
            Line::from("  maximized block    ↑/↓ PgUp/PgDn scroll · c copy · p prompt · Esc close"),
            Line::from("  /q, /quit, /exit   leave the session"),
            Line::from(""),
            Line::from("  Esc                clear the current prompt"),
            Line::from("  Ctrl-C             leave the session"),
        ];

        if !self.skills.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── skills ──────────────────────────".to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )));
            let mut names: Vec<&str> = self.skills.keys().map(|s| s.as_str()).collect();
            names.sort();
            for name in names {
                if let Some(skill) = self.skills.get(name) {
                    let desc = truncate_at_word(&skill.description, 50);
                    lines.push(Line::from(format!("  /{name:<18} {desc}")));
                }
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(
            "  Anything else is sent to the model as a prompt.",
        ));
        lines.push(Line::from(""));
        lines
    }

    fn emit_help_lines(&self, vp: &mut DynamicViewport) -> Result<()> {
        let lines = self.build_help_lines();
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// Build the dynamic status line with colored spans. Format:
    /// `◉ thinking (3.2s)  ↑12k ↓3k C8.5k $0.18 6.0%/200k    (openai-codex) gpt-5.5 · medium`
    fn format_status_line(&self, width: usize) -> Line<'static> {
        let phase = self.session_phase;
        let dim = Style::default().fg(Color::DarkGray);
        let not_idle = phase.is_live();

        // Only LIVE phases show the elapsed counter — a terminal state
        // with a frozen "(14.0s)" reads as a hang (mu-d2hx).
        let phase_text = if phase.is_live() && self.phase_elapsed_ms > 0 {
            let secs = self.phase_elapsed_ms as f64 / 1000.0;
            format!("{} {} ({secs:.1}s)", phase.icon(), phase.label())
        } else {
            format!("{} {}", phase.icon(), phase.label())
        };

        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled(" ".to_string(), dim));
        spans.push(Span::styled(
            phase_text.clone(),
            Style::default().fg(phase.color()),
        ));

        // Build metrics from MCP status or inline accumulators
        // Context meter inputs. soft = the budget mu manages against (the
        // denominator + pressure basis), fill = current occupancy, hard =
        // the model's absolute ceiling (informational). See
        // mu_core::session_status for the vocabulary.
        let (
            in_tok,
            out_tok,
            cache_read,
            cache_creation,
            cost,
            ctx_pct,
            ctx_soft,
            ctx_used,
            ctx_hard,
        ) = if let Some(ref s) = self.mcp_status {
            (
                s.input_tokens,
                s.output_tokens,
                s.cache_read_tokens.unwrap_or(0),
                s.cache_creation_tokens.unwrap_or(0),
                s.cost_usd,
                s.context_pressure_pct,
                s.context_soft_limit,
                s.context_used_tokens,
                s.context_hard_limit,
            )
        } else {
            (
                self.cumulative_input_tokens,
                self.cumulative_output_tokens,
                self.cumulative_cache_read,
                self.cumulative_cache_creation,
                self.compute_cost(),
                None,
                None,
                None,
                None,
            )
        };

        let mut metrics_text_len = 0;
        if in_tok > 0 || out_tok > 0 {
            let in_s = format!("  ↑{}", format_tokens(in_tok));
            let out_s = format!(" ↓{}", format_tokens(out_tok));
            metrics_text_len += in_s.len() + out_s.len();
            // Color arrows based on activity: cyan when streaming, dim when idle
            let arrow_style = if not_idle {
                Style::default().fg(Color::Cyan)
            } else {
                dim
            };
            spans.push(Span::styled(in_s, arrow_style));
            spans.push(Span::styled(out_s, arrow_style));

            if cache_read > 0 {
                let cs = format!(" Cr{}", format_tokens(cache_read));
                metrics_text_len += cs.len();
                spans.push(Span::styled(cs, dim));
            }
            if cache_creation > 0 {
                let cs = format!(" Cw{}", format_tokens(cache_creation));
                metrics_text_len += cs.len();
                spans.push(Span::styled(cs, dim));
            }
            if cost > 0.0 {
                let cs = format!(" ${cost:.2}");
                metrics_text_len += cs.len();
                spans.push(Span::styled(cs, dim));
            }
            if let (Some(pct), Some(soft)) = (ctx_pct, ctx_soft) {
                // fill / soft-limit: the meter fills toward the budget mu
                // compacts at. Prefer the reported fill; fall back to
                // pct*soft only if the fill is somehow absent.
                let used = ctx_used.unwrap_or((pct / 100.0 * soft as f64) as u64);
                let cs = format!(" {}/{}", format_tokens(used), format_tokens(soft));
                metrics_text_len += cs.len();
                let ctx_style = if pct >= 90.0 {
                    Style::default().fg(Color::Red)
                } else if pct >= 70.0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    dim
                };
                spans.push(Span::styled(cs, ctx_style));
                // Hard ceiling, shown dim only when it adds information
                // (known and above the soft budget).
                if let Some(hard) = ctx_hard {
                    if hard > soft {
                        let hs = format!(" ⌈{}", format_tokens(hard));
                        metrics_text_len += hs.len();
                        spans.push(Span::styled(hs, dim));
                    }
                }
            }
        }

        let right = format!(
            "({}) {} · {}",
            self.provider,
            self.model,
            self.effort.as_str()
        );

        let left_len = 1 + phase_text.len() + metrics_text_len;
        let gap = width.saturating_sub(left_len + right.len() + 1);
        let padding = " ".repeat(gap.max(1));

        spans.push(Span::styled(padding, dim));
        spans.push(Span::styled(right, dim));

        Line::from(spans)
    }

    /// Bottom info line: user@host:project | model | ctx:%
    fn format_info_line(&self, width: usize) -> Line<'static> {
        let user = std::env::var("USER").unwrap_or_else(|_| "?".into());
        // mu-8stm.1: the hostname is constant for the life of the process —
        // resolve it ONCE via gethostname(3) (the kern.hostname sysctl on BSD,
        // i.e. what `hostname` itself reads) and cache it. The previous
        // $HOSTNAME→$HOST→/etc/hostname→`hostname -s` ladder ran every render
        // frame, and on FreeBSD (no /etc/hostname, $HOSTNAME unset) fell all
        // the way through to fork+exec'ing `hostname` per frame — a syscall
        // storm during any in-flight turn.
        static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let host = HOST.get_or_init(|| {
            gethostname::gethostname()
                .to_string_lossy()
                .split('.')
                .next()
                .unwrap_or("?")
                .to_string()
        });
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
            .unwrap_or_else(|| "?".into());

        // An active flash takes over the left of the info line (green), so a
        // command's "✓ did the thing" is visible even in fullscreen where the
        // insert_before confirmation is painted over (mu-5h9m).
        let (left, left_style) = match self.flash {
            Some(ref flash) => (
                format!("  {flash}"),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            None => (
                format!("  {user}@{host}:{cwd}"),
                Style::default().fg(Color::DarkGray),
            ),
        };

        let (right, right_style) = if let Some(ref status) = self.mcp_status {
            if let Some(pct) = status.context_pressure_pct {
                let style = if pct >= 90.0 {
                    Style::default().fg(Color::Red)
                } else if pct >= 70.0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                (format!("ctx:{pct:.0}%"), style)
            } else {
                (String::new(), Style::default().fg(Color::DarkGray))
            }
        } else {
            (String::new(), Style::default().fg(Color::DarkGray))
        };

        let gap = width.saturating_sub(left.len() + right.len() + 2);
        let padding = " ".repeat(gap.max(1));

        Line::from(vec![
            Span::styled(left, left_style),
            Span::styled(padding, Style::default()),
            Span::styled(right, right_style),
        ])
    }

    /// Inline cost computation (mirrors mu-core pricing.rs). Returns
    /// 0.0 for unknown (provider, model) pairs.
    fn compute_cost(&self) -> f64 {
        let kind = normalize_provider_kind(&self.provider);
        let (in_rate, out_rate) = match kind.as_str() {
            "anthropic_api" | "anthropic_oauth" => {
                if self.model.starts_with("claude-opus-4") {
                    (5.00_f64, 25.00_f64)
                } else if self.model.starts_with("claude-sonnet-4") {
                    (3.00, 15.00)
                } else if self.model.starts_with("claude-haiku-4") {
                    (1.00, 5.00)
                } else {
                    return 0.0;
                }
            }
            _ => return 0.0,
        };
        let inp = self.cumulative_input_tokens as f64;
        let out = self.cumulative_output_tokens as f64;
        let cw = self.cumulative_cache_creation as f64;
        let cr = self.cumulative_cache_read as f64;
        (inp * in_rate + cw * in_rate * 1.25 + cr * in_rate * 0.10 + out * out_rate) / 1_000_000.0
    }

    /// Apply a single MCP status update. Syncs the inline accumulators
    /// so both the status line and /status command reflect the latest data.
    fn apply_mcp_status(&mut self, status: SessionStatus) {
        // Phase is a projection of CURRENT session state (mu-d2hx): the
        // MCP push is an async secondary channel, so its phase claim only
        // applies while a turn is actually in flight. A stale push landing
        // after the terminal done/error repaint must not resurrect a
        // spinner — that sticky last-write was the frozen-status bug.
        let in_flight = self.streaming_route.is_some();
        if in_flight {
            self.session_phase = self
                .session_phase
                .on_provider_status(status.phase.as_str(), in_flight);
            self.phase_elapsed_ms = status.phase_elapsed_ms;
        }
        self.cumulative_input_tokens = status.input_tokens;
        self.cumulative_output_tokens = status.output_tokens;
        self.cumulative_cache_read = status.cache_read_tokens.unwrap_or(0);
        self.cumulative_cache_creation = status.cache_creation_tokens.unwrap_or(0);
        self.ask_count = status.ask_count;
        self.mcp_status = Some(status);
    }

    /// Dispatch a single notification.
    fn handle_notification(
        &mut self,
        vp: &mut DynamicViewport,
        method: &str,
        params: &Value,
    ) -> Result<()> {
        // Route notifications to the right turn (main vs sidecar /btw).
        // Notifications without a session_id (rare; some daemon
        // events) fall through with whatever streaming_route is set.
        let sid = params
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !sid.is_empty() {
            if sid == self.session_id {
                // main turn — route already set by send_prompt
            } else if self.sidecar_session_id.as_deref() == Some(sid) {
                // sidecar turn — route already set by cmd_btw
            } else {
                // unknown session — drop
                return Ok(());
            }
        }
        let width = vp.area().width as usize;
        let wrap_width = width.saturating_sub(2);
        match method {
            "session.text_delta" => {
                // Build the model only; the live preview is painted by
                // render_viewport each tick (focus_mode suppresses the
                // preview there — the model is still built and committed).
                let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Ok(());
                }
                let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                self.live_turn
                    .get_or_insert_with(|| Turn::new(route))
                    .push_text(delta);
            }
            "session.assistant_text_finalized" => {
                // Replace the current segment's streamed text with the
                // canonical text (mu-wk2 invariant: this notification's
                // `text` is what the AssistantMessage commits). In agent
                // loops this fires once per invocation; each segment is a
                // distinct Text item, separated by any intervening tool
                // calls, all inside the one live turn.
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                    let turn = self.live_turn.get_or_insert_with(|| Turn::new(route));
                    match turn.items.last_mut() {
                        Some(render::TurnItem::Text(s)) => *s = text.to_string(),
                        _ => turn.items.push(render::TurnItem::Text(text.to_string())),
                    }
                }
            }
            "session.thinking_delta" => {
                // Reasoning streams exactly like text (mu-upk2). focus_mode
                // suppression is handled in render_viewport, same as text.
                let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Ok(());
                }
                let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                self.live_turn
                    .get_or_insert_with(|| Turn::new(route))
                    .push_thinking(delta);
            }
            "session.thinking_finalized" => {
                // Mirror assistant_text_finalized: replace the streamed
                // reasoning with the canonical text from the assistant
                // message's Thinking blocks. The reasoning item precedes the
                // answer text in the turn, so target the most recent Thinking
                // item rather than the trailing item.
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                    let turn = self.live_turn.get_or_insert_with(|| Turn::new(route));
                    match turn
                        .items
                        .iter_mut()
                        .rev()
                        .find(|it| matches!(it, render::TurnItem::Thinking(_)))
                    {
                        Some(render::TurnItem::Thinking(s)) => *s = text.to_string(),
                        _ => turn
                            .items
                            .push(render::TurnItem::Thinking(text.to_string())),
                    }
                }
            }
            "session.tool_call_delta" => {
                // Live partial tool call (mu-upk2): the tool name arrives on
                // the block start, then arg fragments stream in. Fold both
                // into one in-flight ToolCall item, keyed by tool_call_id;
                // session.tool_call_started finalizes it below.
                let id = params
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if id.is_empty() {
                    return Ok(());
                }
                let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                let turn = self.live_turn.get_or_insert_with(|| Turn::new(route));
                if turn.tool_call_mut(id).is_none() {
                    turn.items.push(render::TurnItem::ToolCall {
                        tool_call_id: id.to_string(),
                        display_name: String::new(),
                        primary_arg: String::new(),
                        arguments: serde_json::Value::Null,
                        partial_args: String::new(),
                    });
                }
                if let Some(render::TurnItem::ToolCall {
                    display_name,
                    partial_args,
                    ..
                }) = turn.tool_call_mut(id)
                {
                    if let Some(name) = params.get("name_delta").and_then(|v| v.as_str()) {
                        if !name.is_empty() {
                            *display_name = titlecase_tool(name);
                        }
                    }
                    if let Some(args) = params.get("arguments_delta").and_then(|v| v.as_str()) {
                        partial_args.push_str(args);
                    }
                }
            }
            "session.tool_call_started" => {
                // Titlecase + primary-arg extraction happen here (build
                // time) so the renderer stays pure. mu-upk2: finalize the
                // in-flight item the streamed deltas already opened (keyed by
                // id) and retain the full typed call (id + arguments); if no
                // deltas streamed (e.g. a provider without tool-arg
                // streaming), open the item now.
                let name = params
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let id = params
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let primary_arg = extract_primary_arg(name, params.get("arguments"));
                let display_name = titlecase_tool(name);
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                let turn = self.live_turn.get_or_insert_with(|| Turn::new(route));
                if turn.tool_call_mut(id).is_some() {
                    if let Some(render::TurnItem::ToolCall {
                        display_name: dn,
                        primary_arg: pa,
                        arguments: ar,
                        partial_args,
                        ..
                    }) = turn.tool_call_mut(id)
                    {
                        *dn = display_name;
                        *pa = primary_arg;
                        *ar = arguments;
                        partial_args.clear();
                    }
                } else {
                    turn.items.push(render::TurnItem::ToolCall {
                        tool_call_id: id.to_string(),
                        display_name,
                        primary_arg,
                        arguments,
                        partial_args: String::new(),
                    });
                }
            }
            "session.tool_call_completed" => {
                let outcome = params.get("outcome");
                let kind = outcome
                    .and_then(|o| o.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let text = match kind.as_str() {
                    "ok" => outcome
                        .and_then(|o| o.get("result"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    "err" => outcome
                        .and_then(|o| o.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    _ => String::new(),
                };
                let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                self.live_turn
                    .get_or_insert_with(|| Turn::new(route))
                    .items
                    .push(render::TurnItem::ToolResult { kind, text });
            }
            "session.provider_status" => {
                // Projection guard (mu-d2hx): provider_status is an
                // ephemeral live stream. Once the turn is over (done/
                // error/RPC failure cleared streaming_route) a straggler
                // must not overwrite the terminal repaint — only apply
                // while a turn is in flight.
                let in_flight = self.streaming_route.is_some();
                if in_flight {
                    let kind = params
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("idle");
                    self.session_phase = self.session_phase.on_provider_status(kind, in_flight);
                    self.phase_elapsed_ms = params
                        .get("elapsed_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            "session.done" | "session.error" => {
                let is_error = method == "session.error";
                // Terminal repaint (mu-d2hx): EVERY done/error transitions
                // the phase projection — leaving the last live phase in
                // place is the frozen "⚙ tool (14.0s)" hang-lookalike.
                // IterationCap gets its own terminal state so a budget
                // stop never looks like work in progress.
                let stop_reason = params.get("stop_reason").and_then(|v| v.as_str());
                let turn_count = params
                    .get("turn_count")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let iteration_cap = !is_error && stop_reason == Some("iteration_cap");
                self.session_phase = SessionPhase::on_turn_end(is_error, stop_reason, turn_count);
                self.phase_elapsed_ms = 0;

                // mu-solo-osc-notify-mbmn: surface main-session turn
                // boundaries as desktop notifications. Sidecar (/btw)
                // turns are background by design and stay silent.
                // mu-solo-notify-pane-focus-jqnp: gate on PANE focus
                // (terminal_focused, fed by zellij-proxied DECSET 1004)
                // and emit o=always so kitty shows it regardless of
                // its window focus. This reverses 56h0's o=invisible
                // hand-off: kitty can't tell zellij panes apart, so it
                // only ever notified on app/tab switches — never on a
                // pane switch within the same kitty window. The layer
                // with pane-level focus knowledge makes the decision.
                if crate::notify::should_notify(self.notifications, self.terminal_focused)
                    && sid == self.session_id
                {
                    if iteration_cap {
                        // mu-d2hx item c: an iteration-cap stop is a
                        // terminal state reached while the operator is
                        // away — say WHY, not just "waiting for input".
                        crate::notify::notify(&format!(
                            "mu ({}): {}",
                            self.model,
                            turn_budget_copy(turn_count)
                        ));
                    } else if method == "session.done" {
                        crate::notify::notify(&format!(
                            "mu ({}) is waiting for your input",
                            self.model
                        ));
                    } else {
                        crate::notify::notify(&format!(
                            "mu ({}): turn ended with an error",
                            self.model
                        ));
                    }
                }

                if method == "session.done" {
                    self.ask_count += 1;
                    if let Some(usage) = params.get("usage") {
                        self.cumulative_input_tokens += usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        self.cumulative_output_tokens += usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        self.cumulative_cache_read += usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        self.cumulative_cache_creation += usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                }

                // One-shot renderer-mismatch diagnostic: after the
                // first turn lands a context_assembly into the events
                // log, scan for it and surface a warning if the
                // daemon silently fell back to faux (or any renderer
                // other than what the user asked for).
                if self.actual_renderer.is_none() {
                    self.try_load_actual_renderer();
                    if self.is_renderer_mismatch() && !self.renderer_mismatch_warned {
                        self.emit_renderer_mismatch_warning(vp)?;
                        // Flash too — the inline warning is painted over in
                        // fullscreen (ci-aipr finding); full detail in /status.
                        self.set_flash("⚠ renderer mismatch — see /status".to_string());
                        self.renderer_mismatch_warned = true;
                    }
                }
                // For session.error: pull the daemon-supplied message
                // so the operator sees WHAT failed instead of just
                // "(turn ended with error)". Per mu-core's ErrorEvent,
                // params.message is a human-readable string explaining
                // the failure (e.g. provider HTTP errors, validation
                // failures, malformed model IDs).
                let error_msg: Option<String> = if method == "session.error" {
                    params
                        .get("message")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                } else {
                    None
                };
                // Commit the in-flight turn to scrollback. Its content
                // streamed into the live viewport region; now it lands as
                // one block (header + items + closer) in arrival order. A
                // `session.error` message attaches as a trailing inline
                // Error item so it reads as part of the turn it killed.
                let preview_lines = if self.bash_yolo { 15 } else { 4 };
                match self.live_turn.take() {
                    Some(mut t) if t.has_output() || error_msg.is_some() => {
                        if let Some(msg) = error_msg.as_deref() {
                            t.items.push(render::TurnItem::Error(msg.to_string()));
                        }
                        // Finalize-mismatch check: compare committed
                        // history lines against the rendered line count
                        // that's about to be inserted. A mismatch here
                        // means the live preview and the final commit
                        // diverged — log to the journal and warn.
                        let history_before = vp.history_len();
                        self.transcript
                            .push(TranscriptBlock::assistant(t.route, &t.items));
                        let mut lines = render::render_turn(
                            t.route.header_label(),
                            t.route.color(),
                            &t.items,
                            wrap_width,
                            preview_lines,
                            false, // inline scrollback commit keeps full results (mu-5h9m)
                        );
                        lines.extend(render::turn_closer(t.route.color()));
                        let h = lines.len() as u16;
                        // Compute committed text length for the mismatch check.
                        let committed_text_len: usize = t
                            .items
                            .iter()
                            .map(|item| {
                                if let render::TurnItem::Text(s) = item {
                                    s.len()
                                } else {
                                    0
                                }
                            })
                            .sum();
                        vp.insert_before(h, |buf| {
                            let p = Paragraph::new(lines);
                            ratatui::widgets::Widget::render(p, buf.area, buf);
                        })?;
                        // Post-insert mismatch check: history should have
                        // grown to exactly min(before + h, MAX_HISTORY) —
                        // the cap-aware form, so a MAX_HISTORY drain does
                        // not false-alarm (8hva judge finding).
                        let history_after = vp.history_len();
                        let expected_after =
                            (history_before + h as usize).min(crate::viewport::MAX_HISTORY);
                        if history_after != expected_after {
                            let actually_committed = history_after.saturating_sub(history_before);
                            vp.journal_finalize_mismatch(actually_committed, committed_text_len);
                        }
                    }
                    _ => {
                        // No visible output (empty turn or none), and any
                        // error has no turn to attach to: stand-alone block.
                        let lines: Vec<Line<'static>> = if let Some(msg) = error_msg.as_deref() {
                            // mu-ka3c: render the FULL message, word-
                            // wrapped (unbroken JSON runs hard-break) —
                            // the old single-line truncation left a 402
                            // provider error unreadable. Also record it
                            // in the semantic transcript so it's visible
                            // in fullscreen and to copy/export.
                            self.transcript.push(TranscriptBlock::error(msg));
                            render::error_notice("turn ended with error", msg, wrap_width)
                        } else {
                            vec![
                                Line::from(Span::styled(
                                    "  (turn ended, no output)".to_string(),
                                    Style::default()
                                        .fg(Color::DarkGray)
                                        .add_modifier(Modifier::ITALIC),
                                )),
                                Line::from(""),
                            ]
                        };
                        let h = lines.len() as u16;
                        vp.insert_before(h, |buf| {
                            let p = Paragraph::new(lines);
                            ratatui::widgets::Widget::render(p, buf.area, buf);
                        })?;
                    }
                }
                // mu-d2hx item b: an iteration-cap stop renders DISTINCTLY
                // in the transcript (in addition to the status line) — the
                // turn budget ran out; the session did not hang and did not
                // finish naturally. Semantic transcript first (fullscreen
                // renders from it; copy/export read it), then scrollback.
                if iteration_cap {
                    let copy = turn_budget_copy(turn_count);
                    self.transcript
                        .push(TranscriptBlock::notice("turn budget", copy.clone()));
                    let mut nlines: Vec<Line<'static>> = vec![Line::from("")];
                    for row in render::wrap_line(&format!("■ {copy}"), wrap_width.max(1)) {
                        nlines.push(Line::from(Span::styled(
                            row,
                            Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::BOLD),
                        )));
                    }
                    nlines.push(Line::from(""));
                    let h = nlines.len() as u16;
                    vp.insert_before(h, |buf| {
                        let p = Paragraph::new(nlines);
                        ratatui::widgets::Widget::render(p, buf.area, buf);
                    })?;
                }
                self.streaming_route = None;
            }
            _ => {} // ignore unhandled notifications for v0
        }
        Ok(())
    }

    /// One-shot read of the durable events log to extract the
    /// *actual* renderer / cache_strategy / provider_kind / model the
    /// daemon resolved for this session. ContextAssembly isn't on the
    /// wire (forwarder.rs:209 — "wire-level exposure is a future
    /// TUI/web-ui feature"), so we scan the JSONL directly. Idempotent:
    /// once populated, subsequent calls noop. Best-effort — missing
    /// file or parse errors are silently treated as "not yet known."
    fn try_load_actual_renderer(&mut self) {
        if self.actual_renderer.is_some() {
            return;
        }
        let Some(path) = self.events_file.as_ref() else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return;
        };
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(p) = v.get("payload") else { continue };
            if p.get("kind").and_then(|k| k.as_str()) != Some("context_assembly") {
                continue;
            }
            // Restrict to OUR session_id when present — defensive in
            // case the file ever holds multiplexed sessions.
            let event_sid = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("");
            if !event_sid.is_empty() && event_sid != self.session_id {
                continue;
            }
            self.actual_renderer = p.get("renderer").and_then(|r| r.as_str()).map(String::from);
            self.actual_cache_strategy = p
                .get("cache_strategy")
                .and_then(|r| r.as_str())
                .map(String::from);
            self.actual_provider_kind = p
                .get("provider_kind")
                .and_then(|r| r.as_str())
                .map(String::from);
            self.actual_model = p.get("model").and_then(|r| r.as_str()).map(String::from);
            return;
        }
    }

    /// True iff the daemon resolved to a renderer / cache strategy
    /// that the user didn't explicitly ask for. Today this means
    /// "faux fallback when the requested provider couldn't be
    /// constructed" — the most common case being expired OAuth on
    /// openai-codex. Returns false if we don't yet have actual data.
    fn is_renderer_mismatch(&self) -> bool {
        let asked = normalize_provider_kind(&self.provider);
        let faux_renderer = self.actual_renderer.as_deref() == Some("faux");
        let faux_cache = self.actual_cache_strategy.as_deref() == Some("faux");
        let asked_faux = asked == "faux";
        (faux_renderer || faux_cache) && !asked_faux
    }

    /// Yellow warning block emitted once after a faux-fallback
    /// detection. Tells the operator what was asked vs. what's
    /// actually running and points at the most likely fix.
    fn emit_renderer_mismatch_warning(&self, vp: &mut DynamicViewport) -> Result<()> {
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                "⚠  renderer mismatch — daemon fell back to faux".to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("  asked:    {}/{}", self.provider, self.model)),
            Line::from(format!(
                "  running:  renderer={} · cache={} · provider_kind={}",
                self.actual_renderer.as_deref().unwrap_or("?"),
                self.actual_cache_strategy.as_deref().unwrap_or("?"),
                self.actual_provider_kind.as_deref().unwrap_or("?"),
            )),
            Line::from(Span::styled(
                "  faux returns empty content — your prompts will get no response.".to_string(),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "  most likely: provider auth missing/expired. Try one of:".to_string(),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "    mu login --provider openai-codex".to_string(),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "    /q  then relaunch with --provider <other>".to_string(),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }
}

fn copy_to_clipboard_or_file(text: &str, configured: Option<&[String]>) -> Result<String> {
    if text.is_empty() {
        return Ok("empty selection".to_string());
    }

    if let Some(outcome) = copy_via_arboard(text) {
        return Ok(outcome);
    }

    if let Some(argv) = configured.filter(|argv| !argv.is_empty()) {
        if run_clipboard_command(argv, text).is_ok() {
            return Ok(format!("{} bytes via {}", text.len(), argv.join(" ")));
        }
    }

    // Unix clipboard command path. Prefer explicit config/env above; v0
    // auto-detects common tools as argv (no shell) and falls back to a file.
    for argv in [
        &["xclip", "-selection", "clipboard"][..],
        &["xsel", "--clipboard", "--input"][..],
        &["wl-copy"][..],
        &["pbcopy"][..],
    ] {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
        if run_clipboard_command(&argv, text).is_ok() {
            return Ok(format!("{} bytes via {}", text.len(), argv.join(" ")));
        }
    }

    let path = std::env::temp_dir().join(format!(
        "mu-solo-copy-{}-{}.txt",
        std::process::id(),
        unix_timestamp_secs()
    ));
    std::fs::write(&path, text).with_context(|| {
        format!(
            "write copy fallback ({} bytes) to {}",
            text.len(),
            path.display()
        )
    })?;
    Ok(format!(
        "clipboard unavailable; wrote {} bytes to {}",
        text.len(),
        path.display()
    ))
}

fn copy_via_arboard(text: &str) -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.set_text(text.to_string()).ok()?;
    Some(format!("{} bytes via native clipboard", text.len()))
}

fn run_clipboard_command(argv: &[String], text: &str) -> Result<()> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("empty clipboard command"))?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawn clipboard command {argv:?}"))?;

    let Some(mut stdin) = child.stdin.take() else {
        anyhow::bail!("clipboard command {argv:?} has no stdin");
    };
    use std::io::Write;
    stdin
        .write_all(text.as_bytes())
        .with_context(|| format!("write {} bytes to clipboard command {argv:?}", text.len()))?;
    drop(stdin);

    let status = child
        .wait()
        .with_context(|| format!("wait for clipboard command {argv:?}"))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("clipboard command {argv:?} exited with {status}");
    }
}

fn unix_timestamp_secs() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

/// Extract the "primary argument" from a tool call's arguments JSON
/// for display in the tool-call header. Returns an empty string if
/// nothing meaningful can be extracted.
fn extract_primary_arg(tool_name: &str, arguments: Option<&Value>) -> String {
    let args = match arguments {
        Some(v) => v,
        None => return String::new(),
    };
    match tool_name {
        "bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "read" | "write" => args
            .get("file_path")
            .or_else(|| args.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "edit" | "str_replace_editor" => args
            .get("file_path")
            .or_else(|| args.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                pattern.to_string()
            } else {
                format!("{pattern}, {path}")
            }
        }
        "glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => {
            // Generic fallback: try common field names
            args.get("command")
                .or_else(|| args.get("file_path"))
                .or_else(|| args.get("path"))
                .or_else(|| args.get("query"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        }
    }
}

/// Title-case a tool name for display: "bash" → "Bash", "read" → "Read".
fn titlecase_tool(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Format token count compactly: 0, 500, 1.2k, 200k, 1.0M
fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1_000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Minimum viewport height (separator + 1 prompt row + separator + status + info).
const VIEWPORT_HEIGHT: u16 = 5;
/// Keep a few prompt rows allocated so crossing a visual-line boundary while
/// typing does not synchronously resize the terminal viewport. The input buffer
/// is local state; starting a new line should be a cheap row repaint, not a
/// scroll-region operation plus full repaint. (mu-mu-solo-typing-lag-8y5g)
const PROMPT_ROW_SLACK: usize = 3;
/// Maximum viewport height — cap to prevent eating the entire screen.
const MAX_VIEWPORT_HEIGHT: u16 = 20;

#[cfg(test)]
mod tests {
    use super::*;

    // ===== status-line state machine (mu-d2hx) =====
    //
    // The status line is a projection of `session_phase`; these tests
    // drive the transition functions the notification handlers delegate
    // to, asserting every ProviderStatusUpdate state and every terminal
    // arm repaints — the frozen "⚙ tool (14.0s)" incident was a done arm
    // that never cleared ToolExecuting.

    /// Live incident (i): IterationCap landed right after a tool result;
    /// the status stayed "⚙ tool". The done(iteration_cap) transition
    /// must replace the tool spinner with the budget-exhausted copy.
    #[test]
    fn tool_executing_plus_done_iteration_cap_shows_budget_copy() {
        let before = SessionPhase::ToolExecuting;
        let after = SessionPhase::on_turn_end(false, Some("iteration_cap"), Some(20));
        assert_ne!(after, before, "done must repaint the tool spinner");
        assert_eq!(
            after,
            SessionPhase::TurnBudgetExhausted {
                turn_count: Some(20)
            }
        );
        assert_eq!(
            after.label(),
            "turn budget exhausted (20) — say continue, or raise the cap"
        );
        // Must NOT look like work in progress.
        assert!(!after.is_live());
        assert_ne!(after.icon(), SessionPhase::ToolExecuting.icon());
        assert_ne!(after.color(), SessionPhase::ToolExecuting.color());
    }

    #[test]
    fn turn_budget_copy_degrades_without_count() {
        assert_eq!(
            turn_budget_copy(None),
            "turn budget exhausted — say continue, or raise the cap"
        );
        assert_eq!(
            turn_budget_copy(Some(7)),
            "turn budget exhausted (7) — say continue, or raise the cap"
        );
    }

    /// Live incident (ii): after a transport-error turn + manual retry,
    /// the indicators did not update during the recovered turn. The
    /// sequence error → ask fired → provider updates must repaint at
    /// every step.
    #[test]
    fn error_then_retry_streams_again() {
        // The turn dies with an error → terminal Errored state.
        let errored = SessionPhase::on_turn_end(true, None, None);
        assert_eq!(errored, SessionPhase::Errored);
        assert!(!errored.is_live());
        // Operator retries: fire_ask seeds AwaitingFirstToken (the
        // immediate repaint), then provider_status drives the live
        // projection for the recovered turn.
        let retrying = SessionPhase::AwaitingFirstToken; // what fire_ask sets
        let streaming = retrying.on_provider_status("streaming", true);
        assert_eq!(streaming, SessionPhase::Streaming);
        let tooling = streaming.on_provider_status("tool_executing", true);
        assert_eq!(tooling, SessionPhase::ToolExecuting);
        // And the recovered turn's clean finish goes back to Idle.
        assert_eq!(
            SessionPhase::on_turn_end(false, Some("end_turn"), Some(3)),
            SessionPhase::Idle
        );
    }

    /// Every ProviderStatusUpdate kind maps to a phase (and unknown
    /// kinds are forward-compatible no-ops).
    #[test]
    fn every_provider_status_kind_maps_to_a_phase() {
        let cases = [
            ("awaiting_first_token", SessionPhase::AwaitingFirstToken),
            ("thinking", SessionPhase::AwaitingFirstToken),
            ("streaming", SessionPhase::Streaming),
            ("tool_executing", SessionPhase::ToolExecuting),
            ("awaiting_tool_result", SessionPhase::ToolExecuting),
            ("idle", SessionPhase::Idle),
        ];
        for (kind, expected) in cases {
            assert_eq!(
                SessionPhase::Idle.on_provider_status(kind, true),
                expected,
                "kind {kind:?} mapped wrong"
            );
        }
        // Unknown kind: keep the current phase, don't reset.
        assert_eq!(
            SessionPhase::Streaming.on_provider_status("future_kind", true),
            SessionPhase::Streaming
        );
    }

    /// A stale provider_status straggler arriving AFTER the terminal
    /// event (turn no longer in flight) must not resurrect a spinner
    /// over the terminal repaint — the sticky-last-write freeze.
    #[test]
    fn stale_provider_status_cannot_resurrect_a_spinner() {
        for terminal in [
            SessionPhase::Idle,
            SessionPhase::TurnBudgetExhausted {
                turn_count: Some(20),
            },
            SessionPhase::Errored,
        ] {
            for kind in [
                "awaiting_first_token",
                "streaming",
                "tool_executing",
                "awaiting_tool_result",
                "idle",
            ] {
                assert_eq!(
                    terminal.on_provider_status(kind, false),
                    terminal,
                    "stale {kind:?} overwrote terminal {terminal:?}"
                );
            }
        }
    }

    /// Every terminal arm's projection: done → Idle, done(iteration_cap)
    /// → TurnBudgetExhausted, error (wire or RPC-level) → Errored.
    #[test]
    fn every_terminal_event_transitions_the_phase() {
        for stop in [None, Some("end_turn"), Some("max_tokens"), Some("aborted")] {
            assert_eq!(
                SessionPhase::on_turn_end(false, stop, None),
                SessionPhase::Idle,
                "done({stop:?}) must land Idle"
            );
        }
        assert_eq!(
            SessionPhase::on_turn_end(false, Some("iteration_cap"), None),
            SessionPhase::TurnBudgetExhausted { turn_count: None }
        );
        // session.error and RPC-level ask failures both route here with
        // is_error = true; stop_reason is irrelevant.
        assert_eq!(
            SessionPhase::on_turn_end(true, Some("iteration_cap"), Some(20)),
            SessionPhase::Errored
        );
    }

    /// Each phase renders distinctly (icon+label pairs are unique) so a
    /// status repaint is always visible.
    #[test]
    fn every_phase_renders_distinctly() {
        let phases = [
            SessionPhase::Idle,
            SessionPhase::AwaitingFirstToken,
            SessionPhase::Streaming,
            SessionPhase::ToolExecuting,
            SessionPhase::TurnBudgetExhausted {
                turn_count: Some(20),
            },
            SessionPhase::Errored,
        ];
        let mut seen = std::collections::HashSet::new();
        for p in phases {
            assert!(
                seen.insert(format!("{} {}", p.icon(), p.label())),
                "duplicate status rendering for {p:?}"
            );
        }
        // Terminal notices never animate.
        assert!(!SessionPhase::TurnBudgetExhausted { turn_count: None }.is_live());
        assert!(!SessionPhase::Errored.is_live());
        assert!(!SessionPhase::Idle.is_live());
    }
}
