//! mu-tui — terminal UI for `mu serve`.
//!
//! Status: **live JSON-RPC client over stdio against a spawned `mu serve`**.
//! Renders the Command Center, F3 transcript replay (with live streaming
//! text + tool-call/result pairing), F5 usage history percentile table,
//! F8 firehose events explorer, session picker overlay, `$EDITOR` handoff
//! for long prompts (Ctrl-X Ctrl-E / Ctrl-X Ctrl-P), and provider-status
//! throbbers with elapsed-ms interpolation between ticks.
//!
//! Open work tracked in br: `mu-gih` (session.input_required modal),
//! `mu-2za` (typed wire decode via mu-core::protocol), `mu-wk2`
//! (close streaming-to-finalized swap gap), `mu-mvk` (markdown
//! rendering), `mu-u8a` (color polish), `mu-cha` (F2 session tree).
//!
//! Subscribes to `session.text_delta` / `session.tool_call_*` and renders
//! the firehose live. Renders `session.provider_status` (mu-035) into the
//! per-session phase line. F2 Session Tree, F4 Context Inspector, F7
//! Router, F9 Tools/Mailbox remain placeholders; the rest are live.

mod mu_client;

use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use mu_core::protocol::{
    ApprovalDecision, InputRequiredEvent, RespondToInputRequiredRequest,
    RespondToInputRequiredResponse,
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Widget, Wrap,
    },
    Frame, Terminal, TerminalOptions, Viewport,
};
use serde_json::json;
use throbber_widgets_tui::{Throbber, ThrobberState};

use crate::mu_client::{Message as MuMessage, MuClient};

// ── Palette ─────────────────────────────────────────────────────────
// mu-u8a: desaturated replacements for `Color::Yellow` and
// `Color::Green` to match the moonfly / github_dark family the user
// runs in helix. Saturated primaries read as jarring against the
// muted editor palette; soft amber and sage green keep the same
// semantic categories (warning/attention vs. healthy/active) at
// lower volume. `Color::Cyan` (active tab) and `Color::Red` (real
// danger) stay as-is — they're already in family or signal severity.
const MUTED_AMBER: Color = Color::Rgb(204, 153, 102);
const MUTED_GREEN: Color = Color::Rgb(122, 162, 122);

// ── Model ───────────────────────────────────────────────────────────

/// mu-62s: which buffer the next $EDITOR handoff targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum EditorTarget {
    /// mu-82l: the user prompt being typed in SendPrompt mode.
    #[default]
    PromptBuffer,
    /// mu-62s: the session's default system prompt — applies to the
    /// next `n` (create_session) until changed.
    SystemPrompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    CommandCenter, // F1
    SessionTree,   // F2
    SessionDetail, // F3
    Context,       // F4
    Usage,         // F5
    Tools,         // F6
    Router,        // F7
    Events,        // F8
    Mailbox,       // F9
}

impl ViewMode {
    fn name(&self) -> &'static str {
        match self {
            Self::CommandCenter => "command",
            Self::SessionTree => "sessions",
            Self::SessionDetail => "current session",
            Self::Context => "context",
            Self::Usage => "usage",
            Self::Tools => "tools",
            Self::Router => "router",
            Self::Events => "events",
            Self::Mailbox => "mailbox",
        }
    }
}

#[derive(Debug, Clone)]
struct SessionRow {
    short_id: String,
    title: String,
    status: SessionStatus,
    model: String,
    cost_usd: f32,
    tokens_kilo: u32,
    phase: String, // post-mu-035 this comes from session.provider_status
    /// Wire session id. None ⇒ mock-data row (no daemon session behind it).
    session_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum SessionStatus {
    Running, // ●
    Idle,    // ○
    Done,    // ✓
}

impl SessionStatus {
    fn glyph(&self) -> char {
        match self {
            Self::Running => '●',
            Self::Idle => '○',
            Self::Done => '✓',
        }
    }
    fn style(&self) -> Style {
        match self {
            Self::Running => Style::default()
                .fg(MUTED_GREEN)
                .add_modifier(Modifier::BOLD),
            Self::Idle => Style::default().fg(MUTED_AMBER),
            Self::Done => Style::default().fg(Color::DarkGray),
        }
    }
}

/// Latest provider_status notification snapshot from mu-035. The
/// TUI caches these per-session and renders the state + elapsed_ms
/// as the phase line, replacing the session.list-derived heuristic.
#[derive(Debug, Clone)]
struct ProviderStatusSnapshot {
    state: String, // serialized ProviderStatusKind: "awaiting_first_token" | ...
    elapsed_ms: u64,
    /// Bytes received from the provider so far. Held for wire-format parity
    /// with mu-035's provider_status notification; the TUI renders only
    /// state + elapsed_ms today. mu-pex (metrics framework) will consume this.
    #[allow(dead_code)]
    bytes_received: Option<u64>,
    tool_call_id: Option<String>,
    /// When this snapshot was received locally. Used to age out
    /// snapshots that haven't been refreshed (e.g. session went
    /// quiet — daemon went away).
    received_at: Instant,
}

/// mu-gih: one outstanding `session.input_required` prompt captured
/// from the daemon notification. Kept in `App::pending_approvals`
/// until the user approves or denies via the modal — at which point
/// `session.respond_to_input_required` is sent and the entry drops.
#[derive(Debug, Clone)]
struct PendingApproval {
    request_id: String,
    tool_call_id: String,
    tool_name: String,
    /// Raw arguments from the notification. Rendered into the modal
    /// body via `sanitize_arguments_preview` (truncates to ~200 chars
    /// after a single-line JSON projection).
    arguments: serde_json::Value,
    /// Daemon-rendered fallback summary (mu-029) — typically the
    /// capability label / reason ("bash command not on allowlist").
    summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Command,    // `:` palette
    SendPrompt, // typing a prompt for the selected session
}

struct App {
    mode: ViewMode,
    sessions: Vec<SessionRow>,
    selected_session: ListState,
    firehose: Vec<String>,
    input_mode: InputMode,
    command_buffer: String,
    prompt_buffer: String,
    // mu-wd2: cursor position in `prompt_buffer`, measured in CHARS
    // (not bytes) so unicode prompts behave correctly. The cursor sits
    // *between* characters: cursor=0 is before the first char,
    // cursor=N is after the last (where N = char_count(prompt_buffer)).
    prompt_cursor: usize,
    // mu-82l: Ctrl-X is the leader for two-key chords in SendPrompt
    // mode. After Ctrl-X, the next keypress is interpreted as the
    // chord's second key — Ctrl-E currently means "open prompt in
    // \$EDITOR". Any other follow-up key clears the leader and is
    // processed normally.
    leader_ctrl_x: bool,
    // mu-82l: when set, the run() loop runs the editor-handoff
    // sequence after the current tick — it owns the Terminal handle
    // we need for the crossterm suspend/resume dance, which the App
    // doesn't have direct access to.
    pending_editor: bool,
    // mu-62s: target of the next editor-handoff. PromptBuffer is the
    // mu-82l user-prompt path (Ctrl-X Ctrl-E). SystemPrompt is the
    // mu-62s system-prompt path (Ctrl-X Ctrl-P) — same handoff
    // machinery, different destination buffer.
    pending_editor_target: EditorTarget,
    // mu-62s: default system prompt for the next 'n' (create_session).
    // None ⇒ no system prompt sent (mu-n48's behavior). Set via
    // :system_prompt palette command or Ctrl-X Ctrl-P chord.
    default_system_prompt: Option<String>,
    // F3-on-F3 session picker (an overlay shown on top of the
    // SessionDetail transcript when the user presses F3 a second
    // time). j/k move the picker selection — and as a side effect
    // give a live preview of the underlying transcript because the
    // session selection is the same state SessionDetail reads.
    // Enter commits + closes the picker; Esc / F3-again closes
    // *and restores* the selection to what it was when the picker
    // opened (cancel semantics).
    session_picker_open: bool,
    /// Snapshot of selected_session at the moment the picker opens.
    /// Esc restores this; Enter discards it (the commit).
    session_picker_saved_selection: Option<usize>,
    /// mu-gih: per-session FIFO of outstanding `session.input_required`
    /// prompts. Keyed by session_id. v1 surfaces only the head of the
    /// selected session's queue via the modal overlay; entries for
    /// other sessions wait until their session is selected. Entries
    /// drop in two places: (a) the daemon ACK in
    /// `dispatch_decision` (Stage 3 / B1 — only on a confirmed
    /// response, NOT on transport error); (b) session-removal sweep
    /// in `refresh_session_list` (Stage 3 / I4). `VecDeque` for O(1)
    /// `pop_front` (Stage 3 minor).
    pending_approvals:
        std::collections::HashMap<String, std::collections::VecDeque<PendingApproval>>,
    quit: bool,
    last_tick: Instant,
    // Daemon stats — when connected, populated from daemon.stats; in
    // mock-data mode these stay frozen at the constructed defaults.
    daemon_uptime_ms: u64,
    daemon_event_count: u64,
    daemon_total_input_tokens: u64,
    daemon_total_output_tokens: u64,
    daemon_active_session_count: u32,
    daemon_in_flight_calls_count: u32,
    daemon_id: Option<String>,
    cost_budget: (f32, f32), // (used, budget) — still partially mocked v1
    // Throttle for periodic daemon queries: every N ticks.
    poll_tick_counter: u32,
    // Per-session UI state: when the client submitted an ask.
    // Cleared when text_delta arrives or session.done fires. This is
    // intentionally client-side because "when did the user press
    // ctrl-enter" is UI state, not daemon state — and the gap between
    // RPC ack and first token is the most user-confusing silent
    // window. With mu-035 Phase B (periodic session.provider_status
    // notifications), `latest_status` below is the AUTHORITATIVE
    // source for the phase line; `ask_started_at` stays as the
    // client-side belt-and-suspenders during the very first ~1s
    // before the first provider_status tick arrives.
    ask_started_at: std::collections::HashMap<String, Instant>,
    // Latest provider_status notification per session (mu-035).
    // Populated by handle_notification on every session.provider_status
    // tick. The TUI renders { state, elapsed_ms } in the phase line
    // and the right-pane affordance. Cleared on session.done.
    latest_status: std::collections::HashMap<String, ProviderStatusSnapshot>,
    // mu-di4: per-session throbber animation state. Lifetime mirrors
    // `latest_status`: ensured on the first provider_status tick for a
    // sid, advanced one step per App::tick(), dropped when
    // `latest_status` drops it (i.e. on session.done / session.error).
    throbber_states: std::collections::HashMap<String, ThrobberState>,
    // Per-session live streaming-text accumulator. text_delta
    // events are NOT logged (per event_log.rs design doc — "streaming-
    // only events do NOT go in the log"). So to render an in-flight
    // assistant message in the transcript pane, we have to assemble
    // it client-side from notifications. Cleared on session.done /
    // session.error for that sid (the AssistantMessageEvent will be
    // in the next session.events page).
    streaming_text: std::collections::HashMap<String, String>,
    // Transcript cache for F3 view. Populated lazily when F3 is open
    // and a session is selected. Keyed by session_id so switching
    // selection doesn't lose the loaded data.
    transcript_events_by_sid: std::collections::HashMap<String, Vec<serde_json::Value>>,
    // F3 transcript scroll state: number of LINES back from the
    // bottom, ANCHORED across appends. 0 = pinned to bottom
    // (auto-follow on new content); >0 = user scrolled up. Reset
    // to 0 with End / `0`. Anchoring (mu-sod): when new lines are
    // appended while offset > 0, render bumps the offset by the
    // delta so the visible window stays on the same absolute
    // content instead of drifting downward.
    transcript_scroll_offset: u16,
    // mu-sod: previous body_lines length for the F3 transcript
    // pane, used to detect appends between renders so we can
    // anchor `transcript_scroll_offset` against streaming growth.
    prev_transcript_total_lines: usize,
    // F8 events explorer scroll state. Same semantics as transcript
    // scroll: 0 = pinned to bottom of firehose, >0 = scrolled up
    // and ANCHORED across appends (mu-sod).
    events_scroll_offset: u16,
    // mu-sod: previous filtered firehose length for the F8 events
    // pane, used to anchor `events_scroll_offset` across appends.
    prev_events_total_lines: usize,
    // F8 events explorer filter: if set, only show firehose lines
    // matching this substring. Toggleable via `:filter <text>`
    // palette command. Empty = no filter (show all).
    events_filter: String,
    // F5 usage / cache dashboard (mu-xln). Holds the most recent
    // daemon.usage_history response and the wall time it was fetched
    // at. Refreshed lazily — whenever a session.done lands (so any
    // F5-switch picks up post-ask data) and every 4 ticks while F5
    // is the active mode. Polling is event-triggered against the
    // event log, not interval-driven, so the cost is bounded by
    // ask completion rate.
    latest_usage_history: Option<serde_json::Value>,
    latest_usage_history_at: Option<Instant>,
    // Wire integration. None ⇒ scaffold mock-data mode (no live daemon).
    mu: Option<MuClient>,
    /// `provider/model` to use when a new session is created via `n`.
    default_provider: (String, String),
    /// mu-o1y7: signal from in-loop logic that the terminal should be
    /// rebuilt in a different viewport mode. `run` checks + takes() this
    /// each tick and returns `RunOutcome::ModeChange` to its caller, which
    /// owns the actual terminal-rebuild.
    pending_mode_change: Option<ViewportMode>,
    /// mu-o1y7: the viewport mode the terminal is currently rendered
    /// in. Updated by `main` after a successful mode swap. Rendering
    /// uses this to dispatch — F3 in Inline mode draws the footer +
    /// input-only inline viewport; F3 in Fullscreen mode draws the
    /// bordered transcript pane (legacy behavior).
    current_mode: ViewportMode,
    /// mu-o1y7 phase 2c: how many transcript events we've emitted into
    /// terminal scrollback (via `Terminal::insert_before`) for each
    /// session_id. Used to compute the delta to emit each tick when
    /// F3 is in Inline mode. Survives mode swaps so re-entering F3
    /// doesn't duplicate transcript history that's already in scrollback.
    f3_emitted_count_by_sid: std::collections::HashMap<String, usize>,
    /// mu-o1y7 phase 3a: one-line markers that should emit into
    /// scrollback above the inline viewport on the next tick. Used to
    /// surface ephemeral feedback (e.g. "can't send — session is
    /// done") that the user otherwise can't tell happened. Drained by
    /// `run` after each `terminal.draw`. Inline-mode only; firehose is
    /// the alt-screen-mode equivalent.
    pending_inline_markers: Vec<String>,
}

impl App {
    fn new(mu: Option<MuClient>, default_provider: (String, String)) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        let connected = mu.is_some();
        let (sessions, firehose) = if connected {
            (
                Vec::new(),
                vec![format!(
                    "[startup] connected to mu serve; type `n` to create a session, `i` to send a prompt to selected."
                )],
            )
        } else {
            (mock_sessions(), mock_firehose())
        };
        Self {
            mode: ViewMode::CommandCenter,
            sessions,
            selected_session: state,
            firehose,
            input_mode: InputMode::Normal,
            command_buffer: String::new(),
            prompt_buffer: String::new(),
            prompt_cursor: 0,
            leader_ctrl_x: false,
            pending_editor: false,
            pending_editor_target: EditorTarget::default(),
            default_system_prompt: None,
            session_picker_open: false,
            session_picker_saved_selection: None,
            pending_approvals: std::collections::HashMap::new(),
            quit: false,
            last_tick: Instant::now(),
            daemon_uptime_ms: 0,
            daemon_event_count: 0,
            daemon_total_input_tokens: 0,
            daemon_total_output_tokens: 0,
            daemon_active_session_count: 0,
            daemon_in_flight_calls_count: 0,
            daemon_id: None,
            cost_budget: (0.0, 10.0),
            poll_tick_counter: 0,
            ask_started_at: std::collections::HashMap::new(),
            latest_status: std::collections::HashMap::new(),
            throbber_states: std::collections::HashMap::new(),
            streaming_text: std::collections::HashMap::new(),
            transcript_events_by_sid: std::collections::HashMap::new(),
            transcript_scroll_offset: 0,
            prev_transcript_total_lines: 0,
            events_scroll_offset: 0,
            prev_events_total_lines: 0,
            events_filter: String::new(),
            latest_usage_history: None,
            latest_usage_history_at: None,
            mu,
            default_provider,
            pending_mode_change: None,
            current_mode: ViewportMode::Fullscreen,
            f3_emitted_count_by_sid: std::collections::HashMap::new(),
            pending_inline_markers: Vec::new(),
        }
    }

    /// mu-o1y7: switch the visible view, requesting a terminal-mode
    /// rebuild when the transition crosses the F3 boundary. Entering
    /// SessionDetail asks for `Inline(N)` (primary buffer, mux owns
    /// scrollback). Leaving SessionDetail asks for `Fullscreen`
    /// (alt-screen takeover, today's behavior for dashboards). Other
    /// transitions (between non-F3 views) require no terminal-mode
    /// change.
    ///
    /// Idempotent — switching to the view you're already on is a no-op.
    /// Routed through every on_key handler that changes `self.mode`
    /// so the mode-swap signal can't be forgotten.
    fn switch_view(&mut self, new_view: ViewMode) {
        let was_f3 = matches!(self.mode, ViewMode::SessionDetail);
        let will_f3 = matches!(new_view, ViewMode::SessionDetail);
        self.mode = new_view;
        if was_f3 != will_f3 {
            // Inline viewport height: 6 lines is enough for a
            // 1-line top rule + 3-line input region + 1-line bottom
            // rule + 1-line footer. Phase 2c will refine when the
            // real layout lands.
            let target = if will_f3 {
                ViewportMode::Inline(6)
            } else {
                ViewportMode::Fullscreen
            };
            // Only set if it actually differs from the current mode.
            // Avoids a no-op rebuild if (somehow) we're already there.
            if self.current_mode != target {
                self.pending_mode_change = Some(target);
            }
        }
    }

    fn connected(&self) -> bool {
        self.mu.is_some()
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.last_tick = now;
        if !self.connected() {
            return;
        }
        // mu-xln: any session.done observed in this tick's drain
        // means the metrics aggregator's snapshot is stale; refresh
        // after the drain so we don't fight the mutable-borrow on
        // self.mu mid-loop.
        let mut refresh_usage_after_drain = false;
        // 1. Drain incoming notifications into the firehose / per-row phase.
        if let Some(mu) = self.mu.as_mut() {
            for _ in 0..64 {
                match mu.try_recv_notification() {
                    Some(MuMessage::Notification { method, params }) => {
                        // Clear the "awaiting first token" affordance
                        // as soon as we see streaming or terminal
                        // events from the session.
                        let sid = params
                            .get("session_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !sid.is_empty()
                            && (method == "session.text_delta"
                                || method == "session.tool_call_started"
                                || method == "session.done"
                                || method == "session.error")
                        {
                            self.ask_started_at.remove(sid);
                        }
                        // Live transcript accumulator. text_delta is
                        // not in the event log, so the only way to
                        // render an in-flight assistant message is to
                        // assemble the deltas here.
                        if !sid.is_empty() {
                            match method.as_str() {
                                "session.text_delta" => {
                                    let delta =
                                        params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                                    self.streaming_text
                                        .entry(sid.to_string())
                                        .or_default()
                                        .push_str(delta);
                                }
                                "session.assistant_text_finalized" => {
                                    // mu-wk2: swap streaming-text accumulator
                                    // for finalized text atomically. The text
                                    // here matches what will appear in the
                                    // AssistantMessageEvent shortly, preventing
                                    // the visible flicker between streaming
                                    // (yellow, in-memory) and finalized (white,
                                    // durable log) blocks.
                                    let text =
                                        params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                    if !text.is_empty() {
                                        self.streaming_text
                                            .insert(sid.to_string(), text.to_string());
                                    }
                                }
                                "session.done" | "session.error" => {
                                    // AssistantMessageEvent has now
                                    // landed in the event log; the
                                    // next session.events refresh
                                    // (every 2 ticks while on F3,
                                    // i.e. ~500ms) will reflect it.
                                    // Drop the streaming accumulator
                                    // — its content is now part of
                                    // the recorded log. Keep the
                                    // transcript cache so F3 doesn't
                                    // flicker through a "loading…"
                                    // state.
                                    self.streaming_text.remove(sid);
                                    // mu-xln: a Done means metrics
                                    // moved. Mark the usage-history
                                    // cache for refresh after the
                                    // notification drain completes —
                                    // we can't refresh inline because
                                    // we hold a mutable borrow on
                                    // self.mu via try_recv_notification.
                                    refresh_usage_after_drain = true;
                                }
                                _ => {}
                            }
                        }
                        handle_notification(
                            &mut self.sessions,
                            &mut self.firehose,
                            &mut self.latest_status,
                            &mut self.pending_approvals,
                            &method,
                            &params,
                        );
                    }
                    Some(MuMessage::Eof) => {
                        self.firehose.push("[!! mu serve closed stdout]".into());
                        break;
                    }
                    Some(MuMessage::ReaderError(e)) => {
                        self.firehose.push(format!("[!! reader error] {e}"));
                        break;
                    }
                    Some(MuMessage::Response { .. }) => {}
                    None => break,
                }
            }
        }
        // 2. Periodic projection queries. session.list runs every
        //    tick (cheap, local registry, microseconds); daemon.stats
        //    runs every 4 ticks (~1s) to keep header counters fresh
        //    without flooding the dispatch path.
        self.refresh_session_list();
        self.poll_tick_counter = self.poll_tick_counter.wrapping_add(1);
        if self.poll_tick_counter % 4 == 0 {
            self.refresh_daemon_stats();
        }
        // Transcript refresh: only when on the SessionDetail (F3)
        // view and a session is selected. Polls every 2 ticks
        // (~500ms) so live conversation feels responsive without
        // flooding session.events on quiet sessions.
        if matches!(self.mode, ViewMode::SessionDetail) && self.poll_tick_counter % 2 == 0 {
            self.refresh_transcript_for_selection();
        }
        // mu-xln F5: refresh whenever a session.done landed this tick,
        // OR every 4 ticks while F5 is active (so an idle user staring
        // at the dashboard still sees the bucket time-stamp tick over).
        // Outside F5 we skip the interval refresh — event-triggered
        // refresh is the only update path, which is essentially free.
        if refresh_usage_after_drain
            || (matches!(self.mode, ViewMode::Usage) && self.poll_tick_counter % 4 == 0)
        {
            self.refresh_usage_history();
        }
        // mu-di4: keep throbber states in sync with the authoritative
        // `latest_status` map. Ensure-and-tick for every active sid;
        // drop entries whose sid has left `latest_status` (i.e. the
        // session.done handler removed it). The throbber animates at
        // tick_rate (250ms) — ~4 fps with the default BRAILLE_SIX set.
        for sid in self.latest_status.keys() {
            self.throbber_states
                .entry(sid.clone())
                .or_default()
                .calc_next();
        }
        self.throbber_states
            .retain(|sid, _| self.latest_status.contains_key(sid));
    }

    fn refresh_transcript_for_selection(&mut self) {
        let Some(idx) = self.selected_session.selected() else {
            return;
        };
        let Some(sid) = self.sessions.get(idx).and_then(|r| r.session_id.clone()) else {
            return;
        };
        let Some(mu) = self.mu.as_mut() else { return };
        // No after_event_id — pull the full first page (limit=200).
        // For very long sessions, future work adds pagination/scroll;
        // for daily-driver-today, 200 events covers ~tens of asks.
        let res = mu.request("session.events", json!({ "session_id": sid, "limit": 500 }));
        match res {
            Ok(v) => {
                if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
                    self.transcript_events_by_sid
                        .insert(sid.clone(), events.clone());
                }
            }
            Err(e) => {
                self.firehose.push(format!("[!! session.events] {e}"));
            }
        }
    }

    fn refresh_session_list(&mut self) {
        let Some(mu) = self.mu.as_mut() else { return };
        let res = mu.request("session.list", json!({}));
        let mut rows: Vec<SessionRow> = match res {
            Ok(v) => v
                .get("sessions")
                .and_then(|s| s.as_array())
                .map(|arr| arr.iter().filter_map(session_row_from_info_value).collect())
                .unwrap_or_default(),
            Err(e) => {
                self.firehose.push(format!("[!! session.list] {e}"));
                return;
            }
        };
        // Overlay authoritative phase + status from the live
        // session.provider_status snapshot (mu-035). The
        // session.list value is derived from the event log and lags
        // by up to a tick; the live snapshot is current.
        for row in rows.iter_mut() {
            if let Some(sid) = row.session_id.as_deref() {
                if let Some(snap) = self.latest_status.get(sid) {
                    let synthetic_ms =
                        snap.elapsed_ms + snap.received_at.elapsed().as_millis() as u64;
                    let secs = synthetic_ms as f32 / 1000.0;
                    row.phase = match snap.state.as_str() {
                        "awaiting_first_token" => {
                            format!("awaiting first token ({secs:.1}s)")
                        }
                        "thinking" => format!("thinking ({secs:.1}s)"),
                        "streaming" => "streaming".into(),
                        "tool_executing" => format!("tool: executing ({secs:.1}s)"),
                        "awaiting_tool_result" => {
                            format!("awaiting tool result ({secs:.1}s)")
                        }
                        "idle" => "idle".into(),
                        other => other.to_string(),
                    };
                    row.status = match snap.state.as_str() {
                        "idle" | "done" => SessionStatus::Idle,
                        _ => SessionStatus::Running,
                    };
                }
            }
        }

        // Preserve selection by session_id, falling back to first row
        // (or no selection if list is empty).
        let prior_sid = self
            .selected_session
            .selected()
            .and_then(|i| self.sessions.get(i))
            .and_then(|r| r.session_id.clone());
        self.sessions = rows;
        // mu-fqvc: aggregate per-session cost into the header budget.
        // The budget ceiling stays whatever main() set; only `used` is
        // computed from live data.
        self.cost_budget.0 = self.sessions.iter().map(|r| r.cost_usd).sum();
        if let Some(target) = prior_sid {
            if let Some(idx) = self
                .sessions
                .iter()
                .position(|r| r.session_id.as_deref() == Some(target.as_str()))
            {
                self.selected_session.select(Some(idx));
                return;
            }
        }
        if !self.sessions.is_empty() {
            self.selected_session.select(Some(0));
        } else {
            self.selected_session.select(None);
        }
        // Drop ask-tracking for sessions that aren't around anymore.
        let live: std::collections::HashSet<String> = self
            .sessions
            .iter()
            .filter_map(|r| r.session_id.clone())
            .collect();
        self.ask_started_at.retain(|sid, _| live.contains(sid));
        // mu-gih (Stage 3 / I4): drop pending-approval queues for
        // session_ids no longer present.
        prune_pending_approvals_to_live(&mut self.pending_approvals, &live, &mut self.firehose);
    }

    /// Refresh the F5 usage / cache pane (mu-xln). Calls
    /// `daemon.usage_history` and caches the full response. Triggered
    /// on every session.done (in the tick loop's notification drain)
    /// and every 4 ticks while F5 is the active mode. Aggregator is
    /// in-memory and cheap, so failure-mode is just transient stale-
    /// ness — log and keep the prior snapshot.
    fn refresh_usage_history(&mut self) {
        let Some(mu) = self.mu.as_mut() else { return };
        match mu.request("daemon.usage_history", json!({})) {
            Ok(v) => {
                self.latest_usage_history = Some(v);
                self.latest_usage_history_at = Some(Instant::now());
            }
            Err(e) => {
                self.firehose.push(format!("[!! daemon.usage_history] {e}"));
            }
        }
    }

    fn refresh_daemon_stats(&mut self) {
        let Some(mu) = self.mu.as_mut() else { return };
        match mu.request("daemon.stats", json!({})) {
            Ok(v) => {
                self.daemon_id = v
                    .get("daemon_id")
                    .and_then(|s| s.as_str())
                    .map(String::from);
                self.daemon_uptime_ms = v.get("uptime_ms").and_then(|x| x.as_u64()).unwrap_or(0);
                self.daemon_event_count =
                    v.get("total_events").and_then(|x| x.as_u64()).unwrap_or(0);
                self.daemon_total_input_tokens = v
                    .get("total_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                self.daemon_total_output_tokens = v
                    .get("total_output_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                self.daemon_active_session_count = v
                    .get("active_session_count")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0) as u32;
                self.daemon_in_flight_calls_count = v
                    .get("in_flight_calls_count")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0) as u32;
            }
            Err(e) => {
                self.firehose.push(format!("[!! daemon.stats] {e}"));
            }
        }
    }

    fn create_session(&mut self) {
        let Some(mu) = self.mu.as_mut() else {
            self.firehose
                .push("[no daemon] `n` ignored — not connected".into());
            return;
        };
        let (kind_raw, model) = self.default_provider.clone();
        // Normalize common variants → wire-protocol enum names. Mu's
        // ProviderSelector is serde-tagged with snake_case discrimi-
        // nators ("anthropic_api", "openai_codex", "openrouter",
        // "faux"). Users tend to type "openai-codex" / "openai" /
        // "openapi-codex" (typo) / "anthropic" — accept them all.
        let kind = match kind_raw.to_lowercase().as_str() {
            "anthropic" | "anthropic-api" | "anthropic_api" | "claude" => {
                "anthropic_api".to_string()
            }
            "openai" | "openai-codex" | "openai_codex" | "codex" | "openapi-codex"
            | "openapi_codex" | "openapi" => "openai_codex".to_string(),
            "openrouter" | "open-router" | "open_router" => "openrouter".to_string(),
            "faux" => "faux".to_string(),
            other => other.to_string(), // fall through, daemon will reject
        };
        // mu-62s: include system_prompt when set. The wire schema
        // (CreateSessionRequest, mu-n48) skips_serializing_if::is_none
        // so a None default produces the same on-the-wire payload
        // as before — clean back-compat.
        let mut params = json!({ "provider": { "kind": kind, "model": model } });
        if let Some(sp) = &self.default_system_prompt {
            params["system_prompt"] = json!(sp);
        }
        let res = mu.request("create_session", params);
        match res {
            Ok(v) => {
                let sid = v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("?")
                    .to_string();
                self.firehose
                    .push(format!("[ok] create_session → {sid} ({kind}/{model})"));
                // Eagerly refresh so the new session shows up before the
                // next tick. session.list is cheap.
                self.refresh_session_list();
                // Try to select the new row.
                if let Some(idx) = self
                    .sessions
                    .iter()
                    .position(|r| r.session_id.as_deref() == Some(sid.as_str()))
                {
                    self.selected_session.select(Some(idx));
                }
            }
            Err(e) => {
                self.firehose.push(format!("[!! create_session] {e}"));
            }
        }
    }

    /// mu-wd2: byte offset into `prompt_buffer` corresponding to the
    /// current char-cursor. Clamped at the buffer length so callers
    /// can safely use it as a `String::insert` / `String::remove`
    /// index.
    fn prompt_cursor_byte_pos(&self) -> usize {
        self.prompt_buffer
            .char_indices()
            .nth(self.prompt_cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.prompt_buffer.len())
    }

    /// mu-wd2: number of chars in the prompt buffer.
    fn prompt_char_count(&self) -> usize {
        self.prompt_buffer.chars().count()
    }

    /// mu-wd2: reset cursor to start, used when the buffer is cleared.
    fn reset_prompt(&mut self) {
        self.prompt_buffer.clear();
        self.prompt_cursor = 0;
    }

    /// mu-wd2: move the char-cursor one word left (skip-then-cross,
    /// like readline's backward-word). A word boundary is the
    /// transition from non-alphanumeric to alphanumeric.
    fn prompt_move_word_left(&mut self) {
        let chars: Vec<char> = self.prompt_buffer.chars().collect();
        let mut i = self.prompt_cursor;
        // Skip whitespace/punctuation immediately before cursor.
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        // Then skip alphanumeric run.
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        self.prompt_cursor = i;
    }

    /// mu-wd2: mirror of `prompt_move_word_left` for moving right.
    fn prompt_move_word_right(&mut self) {
        let chars: Vec<char> = self.prompt_buffer.chars().collect();
        let mut i = self.prompt_cursor;
        while i < chars.len() && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        self.prompt_cursor = i;
    }

    fn send_prompt(&mut self) {
        let prompt = std::mem::take(&mut self.prompt_buffer);
        self.prompt_cursor = 0;
        if prompt.trim().is_empty() {
            return;
        }
        let Some(idx) = self.selected_session.selected() else {
            self.firehose.push("[no selection] `i`/send ignored".into());
            return;
        };
        let Some(row) = self.sessions.get_mut(idx) else {
            return;
        };
        let Some(sid) = row.session_id.clone() else {
            self.firehose
                .push("[mock session] can't send (no session_id)".into());
            return;
        };
        // mu-o1y7 phase 3a follow-up #2 (2026-05-19): no Done-session
        // block. `SessionStatus::Done` in current mu means "phase is
        // done" — which is also the state a session sits in *between
        // turns* (after one ask completed, before the next). There's
        // no terminal "closed" state distinct from "pause between
        // turns" today; blocking sends on Done would block legitimate
        // session-resume. Daemon decides what to do with stuck or
        // already-finished provider sessions; firehose surfaces any
        // error / no-op.
        // (Architectural gap noted: see bead filed alongside this
        // commit — Monty Python's Mr. Orbiter has the same problem:
        // distinct gestures needed for "still going, just paused" vs
        // "done done.")
        let Some(mu) = self.mu.as_mut() else {
            return;
        };
        let res = mu.request(
            "ask_session",
            json!({ "session_id": sid, "user_message": prompt }),
        );
        match res {
            Ok(_) => {
                // UI state: mark "we sent at this instant" for the
                // right-pane affordance until the first token arrives.
                self.ask_started_at.insert(sid.clone(), Instant::now());
                row.status = SessionStatus::Running;
                row.phase = "sent — awaiting first token".into();
                // Friendlier firehose line — prompt preview, ellipsised.
                let preview: String = prompt
                    .chars()
                    .take(60)
                    .collect::<String>()
                    .replace('\n', " ");
                let suffix = if prompt.chars().count() > 60 {
                    "…"
                } else {
                    ""
                };
                self.firehose.push(format!("→ {sid}: {preview:?}{suffix}"));
            }
            Err(e) => {
                self.firehose.push(format!("[!! ask_session] {e}"));
            }
        }
    }

    /// mu-gih: session_id of the row currently highlighted in the
    /// session list, if any. Mock rows (no `session_id`) return None.
    fn selected_sid(&self) -> Option<String> {
        self.selected_session
            .selected()
            .and_then(|i| self.sessions.get(i))
            .and_then(|r| r.session_id.clone())
    }

    /// mu-gih: head of the selected session's pending-approval queue.
    /// `Some(_)` ⇒ the modal is conceptually open and `on_key_normal`
    /// eats keys for A/D. `None` ⇒ no pending prompt for the selected
    /// session, modal is hidden, normal key handling resumes.
    fn current_pending_approval(&self) -> Option<&PendingApproval> {
        let sid = self.selected_sid()?;
        self.pending_approvals.get(&sid)?.front()
    }

    /// mu-gih: send `session.respond_to_input_required` for the head
    /// of the selected session's queue. The entry is only dropped on
    /// a daemon-acknowledged outcome (RPC `Ok(..)`, whether or not
    /// `accepted` is `true`). An RPC-level error leaves the prompt
    /// queued so the user can retry — see [`dispatch_decision`] for
    /// the precise semantics (Stage 3 / B1).
    fn respond_to_pending_approval(&mut self, approve: bool) {
        let Some(sid) = self.selected_sid() else {
            return;
        };
        // Split-borrow distinct fields so the closure can capture
        // `&mut self.mu` while `dispatch_decision` mutably borrows
        // the queue + firehose. Rust permits disjoint mutable
        // borrows of separate struct fields.
        let pending_approvals = &mut self.pending_approvals;
        let firehose = &mut self.firehose;
        let mu = &mut self.mu;
        dispatch_decision(
            pending_approvals,
            firehose,
            &sid,
            approve,
            |method, payload| match mu.as_mut() {
                Some(client) => client.request(method, payload),
                None => Err(anyhow::anyhow!("no daemon client")),
            },
        );
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        // mu-gih (Stage 3 / B2 + I5): explicit overlay priority,
        // routed BEFORE any `input_mode` dispatch.
        //
        // 1. F3 session picker: when the user is actively picking a
        //    session, all keys go to it. A pending approval is left
        //    in the queue and the modal does NOT render — it pops
        //    into view as soon as the picker closes (I5 option (a):
        //    "suppress modal while picker is open"). This composes
        //    more cleanly than rendering both overlays at once.
        //
        // 2. Pending approval modal: absolute priority over every
        //    input mode. The agent loop is blocked waiting for A/D,
        //    so falling through to Command/SendPrompt would leave
        //    the user typing into the wrong buffer and the daemon
        //    stuck (B2 — pre-Stage-3 only on_key_normal caught
        //    this; A/D in Command/SendPrompt mode bypassed approval
        //    entirely).
        //
        // 3. Otherwise the normal `input_mode` machine runs.
        if self.session_picker_open {
            self.on_key_session_picker(code, mods);
            return;
        }
        if self.current_pending_approval().is_some() {
            match code {
                KeyCode::Char('a') | KeyCode::Char('A') => self.respond_to_pending_approval(true),
                KeyCode::Char('d') | KeyCode::Char('D') => self.respond_to_pending_approval(false),
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    // mu-gih (Stage 3 / I6): edit-and-resubmit is
                    // deferred to v2. Eat the key and explain so the
                    // user isn't left wondering why E does nothing.
                    self.firehose.push(
                        "[approval] [E]dit is not implemented in v1 — \
                         [D]eny and retry from the CLI with --bash-prompt"
                            .into(),
                    );
                }
                _ => {}
            }
            let _ = mods;
            return;
        }
        match self.input_mode {
            InputMode::Command => self.on_key_command(code),
            InputMode::SendPrompt => self.on_key_send_prompt(code, mods),
            InputMode::Normal => self.on_key_normal(code, mods),
        }
    }

    /// mu-gih (Stage 3 / I5): F3 session-picker key handling,
    /// extracted from `on_key_normal` so the overlay priority routing
    /// in `on_key` can dispatch to it directly without leaking F3
    /// state into `on_key_normal`.
    fn on_key_session_picker(&mut self, code: KeyCode, mods: KeyModifiers) {
        match (code, mods) {
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                let n = self.sessions.len().max(1);
                let i = self.selected_session.selected().unwrap_or(0);
                self.selected_session.select(Some((i + 1) % n));
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                let n = self.sessions.len().max(1);
                let i = self.selected_session.selected().unwrap_or(0);
                self.selected_session.select(Some((i + n - 1) % n));
            }
            (KeyCode::Enter, _) => self.close_session_picker(true),
            (KeyCode::Esc, _) | (KeyCode::F(3), _) => self.close_session_picker(false),
            _ => {}
        }
    }

    fn on_key_command(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.command_buffer.clear();
            }
            KeyCode::Enter => {
                let cmd = std::mem::take(&mut self.command_buffer);
                self.input_mode = InputMode::Normal;
                self.dispatch_command(&cmd);
            }
            KeyCode::Backspace => {
                self.command_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.command_buffer.push(c);
            }
            _ => {}
        }
    }

    fn on_key_send_prompt(&mut self, code: KeyCode, mods: KeyModifiers) {
        // Diagnostic: when in SendPrompt mode, log every keycode so
        // we can see what crossterm actually receives. Helps debug
        // terminal-specific binding issues (e.g. Ctrl-Enter often
        // collapses to plain Enter in many terminals).
        // mu-82l + mu-62s: Ctrl-X is the leader for two-key chords
        // in SendPrompt mode. After Ctrl-X, the next keypress is
        // interpreted as the chord's second key:
        //   Ctrl-E → open the user prompt in $EDITOR (mu-82l)
        //   Ctrl-P → open the default system prompt in $EDITOR (mu-62s)
        // Any non-chord follow-up clears the leader and the key is
        // processed normally.
        if self.leader_ctrl_x {
            self.leader_ctrl_x = false;
            match (code, mods) {
                (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                    self.pending_editor = true;
                    self.pending_editor_target = EditorTarget::PromptBuffer;
                    return;
                }
                (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                    self.pending_editor = true;
                    self.pending_editor_target = EditorTarget::SystemPrompt;
                    return;
                }
                _ => {
                    // Fall through: process the key like any other.
                }
            }
        }
        let debug = format!("[key] code={code:?} mods={mods:?}");
        match (code, mods) {
            (KeyCode::Char('x'), KeyModifiers::CONTROL) => {
                self.leader_ctrl_x = true;
            }
            (KeyCode::Esc, _) => {
                self.input_mode = InputMode::Normal;
                self.reset_prompt();
            }
            // Plain Enter submits (chat-TUI convention). Any modified
            // Enter — Alt, Shift, Ctrl, Meta — inserts a newline so
            // multi-line prompts work regardless of which terminal-
            // specific binding the user reaches for. Ctrl-J is the
            // historical newline alternative (LF as char) and stays.
            //
            // Caveat: many terminals strip the Shift modifier on
            // Enter at the terminal layer (xterm, default GNOME
            // Terminal) and send plain `\r` — no app can recover
            // the modifier in that case. Kitty / WezTerm / iTerm2
            // (with config) preserve it. If Shift-Enter still
            // submits, that's a terminal-layer issue, not a mu bug.
            (KeyCode::Enter, m) if !m.is_empty() => {
                let byte = self.prompt_cursor_byte_pos();
                self.prompt_buffer.insert(byte, '\n');
                self.prompt_cursor += 1;
            }
            (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                let byte = self.prompt_cursor_byte_pos();
                self.prompt_buffer.insert(byte, '\n');
                self.prompt_cursor += 1;
            }
            (KeyCode::Enter, _) => {
                self.input_mode = InputMode::Normal;
                self.send_prompt();
            }
            // ── mu-wd2: cursor motion ──────────────────────────────
            (KeyCode::Left, KeyModifiers::CONTROL) => self.prompt_move_word_left(),
            (KeyCode::Left, _) => {
                if self.prompt_cursor > 0 {
                    self.prompt_cursor -= 1;
                }
            }
            (KeyCode::Right, KeyModifiers::CONTROL) => self.prompt_move_word_right(),
            (KeyCode::Right, _) => {
                if self.prompt_cursor < self.prompt_char_count() {
                    self.prompt_cursor += 1;
                }
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.prompt_cursor = 0;
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.prompt_cursor = self.prompt_char_count();
            }
            // ── mu-wd2: deletion ────────────────────────────────────
            (KeyCode::Backspace, _) => {
                if self.prompt_cursor > 0 {
                    self.prompt_cursor -= 1;
                    let byte = self.prompt_cursor_byte_pos();
                    self.prompt_buffer.remove(byte);
                }
            }
            (KeyCode::Delete, _) => {
                if self.prompt_cursor < self.prompt_char_count() {
                    let byte = self.prompt_cursor_byte_pos();
                    self.prompt_buffer.remove(byte);
                }
            }
            // Ctrl-W: delete previous word (readline backward-kill-word).
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                let target = {
                    let saved = self.prompt_cursor;
                    self.prompt_move_word_left();
                    let t = self.prompt_cursor;
                    self.prompt_cursor = saved;
                    t
                };
                if target < self.prompt_cursor {
                    let start_byte = self
                        .prompt_buffer
                        .char_indices()
                        .nth(target)
                        .map(|(b, _)| b)
                        .unwrap_or(0);
                    let end_byte = self.prompt_cursor_byte_pos();
                    self.prompt_buffer.drain(start_byte..end_byte);
                    self.prompt_cursor = target;
                }
            }
            // Ctrl-U: delete from start of line to cursor (readline
            // unix-line-discard). For a single-line buffer this is
            // "kill everything before cursor."
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                let end_byte = self.prompt_cursor_byte_pos();
                self.prompt_buffer.drain(..end_byte);
                self.prompt_cursor = 0;
            }
            // Ctrl-K: delete from cursor to end (readline kill-line).
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                let start_byte = self.prompt_cursor_byte_pos();
                self.prompt_buffer.truncate(start_byte);
            }
            // ── mu-wd2: text insert ─────────────────────────────────
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                let byte = self.prompt_cursor_byte_pos();
                self.prompt_buffer.insert(byte, c);
                self.prompt_cursor += 1;
            }
            _ => {
                // Log unknown keycodes so the user can see what their
                // terminal is sending and we can adjust bindings.
                self.firehose.push(debug);
            }
        }
    }

    /// F3-on-F3 picker: open. Saves the current selection so Esc /
    /// F3-again can restore it on cancel.
    fn open_session_picker(&mut self) {
        if self.sessions.is_empty() {
            self.firehose
                .push("[picker] no sessions yet — press `n` to create one".into());
            return;
        }
        self.session_picker_open = true;
        self.session_picker_saved_selection = self.selected_session.selected();
    }

    /// F3-on-F3 picker: close. When `commit` is true, the current
    /// selection sticks (Enter semantics). When false, restore the
    /// selection to what it was when the picker opened (Esc / F3-
    /// again semantics).
    fn close_session_picker(&mut self, commit: bool) {
        if !commit {
            if let Some(saved) = self.session_picker_saved_selection {
                self.selected_session.select(Some(saved));
            }
        }
        self.session_picker_open = false;
        self.session_picker_saved_selection = None;
        // Force an immediate transcript refresh for the (possibly
        // changed) selection so the F3 pane updates without waiting
        // for the next 500ms tick.
        self.refresh_transcript_for_selection();
    }

    fn on_key_normal(&mut self, code: KeyCode, mods: KeyModifiers) {
        // mu-gih (Stage 3 / B2 + I5): the pending-approval modal and
        // the F3 session picker both have absolute priority and are
        // now routed in `on_key` before this function runs. By the
        // time we land here, neither overlay is active, so the
        // normal-mode keymap below can dispatch unconditionally.
        match (code, mods) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            (KeyCode::Char(':'), _) => {
                self.input_mode = InputMode::Command;
                self.command_buffer.clear();
            }
            (KeyCode::Char('n'), _) => self.create_session(),
            (KeyCode::Char('i'), _) | (KeyCode::Enter, _)
                if self.selected_session.selected().is_some() =>
            {
                self.input_mode = InputMode::SendPrompt;
                self.reset_prompt();
            }
            (KeyCode::F(1), _) => self.switch_view(ViewMode::CommandCenter),
            (KeyCode::F(2), _) => self.switch_view(ViewMode::SessionTree),
            (KeyCode::F(3), _) => {
                // First press: enter SessionDetail mode.
                // Subsequent press while already there: pop the
                // session picker (modal overlay). Third press / Esc
                // closes the picker.
                if matches!(self.mode, ViewMode::SessionDetail) {
                    self.open_session_picker();
                } else {
                    self.switch_view(ViewMode::SessionDetail);
                    self.transcript_scroll_offset = 0;
                    self.refresh_transcript_for_selection();
                }
            }
            // Transcript scrolling — only meaningful on F3.
            (KeyCode::PageUp, _) if matches!(self.mode, ViewMode::SessionDetail) => {
                self.transcript_scroll_offset = self.transcript_scroll_offset.saturating_add(10);
            }
            (KeyCode::PageDown, _) if matches!(self.mode, ViewMode::SessionDetail) => {
                self.transcript_scroll_offset = self.transcript_scroll_offset.saturating_sub(10);
            }
            (KeyCode::Home, _) if matches!(self.mode, ViewMode::SessionDetail) => {
                // Big offset → render scrolls to the top.
                self.transcript_scroll_offset = u16::MAX;
            }
            (KeyCode::End, _) if matches!(self.mode, ViewMode::SessionDetail) => {
                self.transcript_scroll_offset = 0;
            }
            // j/k scroll the transcript in F3, but still navigate the
            // session list in other views.
            (KeyCode::Char('k'), _) if matches!(self.mode, ViewMode::SessionDetail) => {
                self.transcript_scroll_offset = self.transcript_scroll_offset.saturating_add(2);
            }
            (KeyCode::Char('j'), _) if matches!(self.mode, ViewMode::SessionDetail) => {
                self.transcript_scroll_offset = self.transcript_scroll_offset.saturating_sub(2);
            }
            // F8 events-explorer scroll. Same keys as F3 transcript.
            (KeyCode::PageUp, _) if matches!(self.mode, ViewMode::Events) => {
                self.events_scroll_offset = self.events_scroll_offset.saturating_add(10);
            }
            (KeyCode::PageDown, _) if matches!(self.mode, ViewMode::Events) => {
                self.events_scroll_offset = self.events_scroll_offset.saturating_sub(10);
            }
            (KeyCode::Home, _) if matches!(self.mode, ViewMode::Events) => {
                self.events_scroll_offset = u16::MAX;
            }
            (KeyCode::End, _) if matches!(self.mode, ViewMode::Events) => {
                self.events_scroll_offset = 0;
            }
            (KeyCode::Char('k'), _) if matches!(self.mode, ViewMode::Events) => {
                self.events_scroll_offset = self.events_scroll_offset.saturating_add(2);
            }
            (KeyCode::Char('j'), _) if matches!(self.mode, ViewMode::Events) => {
                self.events_scroll_offset = self.events_scroll_offset.saturating_sub(2);
            }
            (KeyCode::F(4), _) => self.switch_view(ViewMode::Context),
            (KeyCode::F(5), _) => {
                self.switch_view(ViewMode::Usage);
                // Eager refresh on mode entry — the user shouldn't
                // have to wait for the next tick (~250ms) to see
                // populated data after pressing F5.
                self.refresh_usage_history();
            }
            (KeyCode::F(6), _) => self.switch_view(ViewMode::Tools),
            (KeyCode::F(7), _) => self.switch_view(ViewMode::Router),
            (KeyCode::F(8), _) => {
                self.switch_view(ViewMode::Events);
                self.events_scroll_offset = 0; // pinned to bottom on entry
            }
            (KeyCode::F(9), _) => self.switch_view(ViewMode::Mailbox),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                let n = self.sessions.len().max(1);
                let i = self.selected_session.selected().unwrap_or(0);
                self.selected_session.select(Some((i + 1) % n));
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                let n = self.sessions.len().max(1);
                let i = self.selected_session.selected().unwrap_or(0);
                self.selected_session.select(Some((i + n - 1) % n));
            }
            _ => {}
        }
    }

    fn dispatch_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return;
        }
        // Tiny parser: just enough to wire the first real verbs.
        let mut parts = cmd.split_whitespace();
        let verb = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();
        match verb {
            "new" => self.create_session(),
            "send" => {
                // Inline single-shot send: `:send <text...>` skips
                // SendPrompt mode for one-liners.
                self.prompt_buffer = rest.join(" ");
                self.prompt_cursor = self.prompt_char_count();
                self.send_prompt();
            }
            // mu-ium: :provider [kind] [model]
            //   :provider                   → show current default
            //   :provider <kind>            → set kind only (model unchanged)
            //   :provider <kind> <model>    → set both (existing combined form)
            //
            // Kind aliases (anthropic/claude/openai/codex/openrouter/etc)
            // are normalized at create_session time, not here — so users
            // can experiment without hitting a "rejected" message
            // before the daemon has a chance to weigh in.
            "provider" => match rest.len() {
                0 => {
                    let (k, m) = &self.default_provider;
                    self.firehose
                        .push(format!("[info] default provider: {k}/{m}"));
                }
                1 => {
                    self.default_provider.0 = rest[0].to_string();
                    let (k, m) = &self.default_provider;
                    self.firehose
                        .push(format!("[ok] provider kind → {k} (model {m} unchanged)"));
                }
                _ => {
                    self.default_provider = (rest[0].to_string(), rest[1..].join(" "));
                    let (k, m) = &self.default_provider;
                    self.firehose
                        .push(format!("[ok] default provider set to {k}/{m}"));
                }
            },
            // mu-ium: :model <id> — set the model leaving kind unchanged.
            // Trim leading dashes so `:model -- claude-haiku-4-5...`
            // doesn't trip clap-style flag confusion (there's no real
            // arg parser here; just a defensive nicety).
            "model" => {
                if rest.is_empty() {
                    let (k, m) = &self.default_provider;
                    self.firehose
                        .push(format!("[info] default model: {m} (provider {k})"));
                } else {
                    self.default_provider.1 = rest.join(" ");
                    let (k, m) = &self.default_provider;
                    self.firehose
                        .push(format!("[ok] model → {m} (provider {k} unchanged)"));
                }
            }
            // mu-62s: system_prompt management.
            //   :system_prompt                 → show current
            //   :system_prompt <inline text>   → set inline
            //   :clear_system_prompt           → unset
            // For multi-line prompts, use Ctrl-X Ctrl-P in input mode
            // to bounce into $EDITOR.
            "system_prompt" | "sp" => {
                if rest.is_empty() {
                    match &self.default_system_prompt {
                        Some(s) => {
                            let preview: String = s.chars().take(80).collect();
                            self.firehose.push(format!(
                                "[info] system_prompt: {}{}",
                                preview,
                                if s.chars().count() > 80 { "…" } else { "" }
                            ));
                        }
                        None => self.firehose.push("[info] no default system_prompt".into()),
                    }
                } else {
                    let text = rest.join(" ");
                    let preview: String = text.chars().take(40).collect();
                    self.default_system_prompt = Some(text);
                    self.firehose
                        .push(format!("[ok] system_prompt set: {preview}…"));
                }
            }
            "clear_system_prompt" | "csp" => {
                self.default_system_prompt = None;
                self.firehose.push("[ok] system_prompt cleared".into());
            }
            "quit" | "q" => self.quit = true,
            "filter" => {
                // :filter <substring>   → only F8 lines containing it
                // :filter               → clear filter
                self.events_filter = rest.join(" ");
                self.events_scroll_offset = 0; // jump back to bottom
                if self.events_filter.is_empty() {
                    self.firehose.push("[ok] filter cleared".into());
                } else {
                    self.firehose
                        .push(format!("[ok] filter set: {:?}", self.events_filter));
                }
            }
            "clear-filter" => {
                self.events_filter.clear();
                self.firehose.push("[ok] filter cleared".into());
            }
            _ => {
                self.firehose.push(format!("[unknown command] :{cmd}"));
            }
        }
    }
}

/// Map one `SessionInfo` JSON object (the shape returned by
/// `session.list`) to a TUI row. Returns None if the payload is
/// malformed (defensive — forward-compat with older daemons that
/// might not include all fields).
fn session_row_from_info_value(v: &serde_json::Value) -> Option<SessionRow> {
    let sid = v.get("session_id")?.as_str()?.to_string();
    let provider_kind = v
        .get("provider_kind")
        .and_then(|s| s.as_str())
        .unwrap_or("?");
    let model = v.get("model").and_then(|s| s.as_str()).unwrap_or("?");
    let status_str = v.get("status").and_then(|s| s.as_str()).unwrap_or("idle");
    let status = match status_str {
        "asking" | "streaming" | "tool_executing" => SessionStatus::Running,
        "awaiting_input_required" => SessionStatus::Idle, // ○ — needs user
        "done" => SessionStatus::Done,
        "errored" => SessionStatus::Idle, // ○ for now — would be nice to have a glyph
        _ => SessionStatus::Idle,
    };
    let phase = match status_str {
        "asking" => "asking (model call pending)".to_string(),
        "streaming" => "streaming".to_string(),
        "tool_executing" => "tool executing".to_string(),
        "awaiting_input_required" => "awaiting approval".to_string(),
        "done" => "done".to_string(),
        "errored" => "error".to_string(),
        "idle" => "idle".to_string(),
        other => other.to_string(),
    };
    let cumulative_usage = v.get("cumulative_usage");
    let tokens_kilo = cumulative_usage
        .map(|u| {
            let i = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
            let o = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
            ((i + o) / 1000) as u32
        })
        .unwrap_or(0);
    // mu-fqvc: per-session cost from cumulative_usage + per-model pricing.
    // Unknown (provider, model) pairs leave cost at 0.0 (best-effort
    // display — don't show a confidently-wrong number).
    let cost_usd = cumulative_usage
        .and_then(|u| {
            let pricing = mu_core::pricing::for_model(provider_kind, model)?;
            let usage = mu_core::agent::types::Usage {
                input_tokens: u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                output_tokens: u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                cache_read_input_tokens: u.get("cache_read_input_tokens").and_then(|x| x.as_u64()),
                cache_creation_input_tokens: u
                    .get("cache_creation_input_tokens")
                    .and_then(|x| x.as_u64()),
                reasoning_tokens: u.get("reasoning_tokens").and_then(|x| x.as_u64()),
            };
            Some(pricing.cost(&usage) as f32)
        })
        .unwrap_or(0.0);
    Some(SessionRow {
        short_id: sid.chars().take(12).collect(),
        title: format!("{provider_kind} / {model}"),
        status,
        model: format!("{provider_kind} / {model}"),
        cost_usd,
        tokens_kilo,
        phase,
        session_id: Some(sid),
    })
}

/// mu-gih (Stage 3 / I1): build the typed
/// `RespondToInputRequiredRequest` for a pending approval. Pulled out
/// so [`dispatch_decision`] and the wire-shape regression tests share
/// the same construction path — drift between the test and the
/// production payload is therefore caught at compile time.
fn build_respond_payload(
    sid: &str,
    item: &PendingApproval,
    approve: bool,
) -> RespondToInputRequiredRequest {
    RespondToInputRequiredRequest {
        session_id: sid.to_string(),
        request_id: item.request_id.clone(),
        decision: if approve {
            ApprovalDecision::Approve
        } else {
            ApprovalDecision::Deny
        },
    }
}

/// mu-gih (Stage 3 / B1, I1): peek the head of the selected session's
/// pending-approval queue, send `session.respond_to_input_required`,
/// and drop the entry ONLY on a daemon-acknowledged outcome.
///
/// Outcome semantics:
/// - `Ok(v)` parses as [`RespondToInputRequiredResponse`] with
///   `accepted=true` or `accepted=false`: the daemon either relayed
///   the decision or told us the request_id is no longer valid (it
///   timed out, was already answered, or never existed). Either way
///   the prompt is terminal — pop it.
/// - `Ok(v)` that FAILS to parse as [`RespondToInputRequiredResponse`]:
///   protocol/shape error. We cannot prove the daemon registered the
///   decision, so the daemon side is still waiting. Keep the prompt
///   queued (same as `Err(_)`) and emit a firehose entry naming the
///   parse failure. This is the N1 (Stage 5) regression — previously
///   `unwrap_or(false)` collapsed this into a phantom accepted=false
///   and silently popped the prompt.
/// - `Err(_)` (transport / serialization / disconnect): the daemon
///   side is still waiting. Keep the prompt visible so the user can
///   retry once the channel recovers. Firehose records the failed
///   attempt for audit.
///
/// The function is generic over the send closure so tests can drive
/// both success + failure paths without a real `MuClient`.
fn dispatch_decision<F>(
    pending_approvals: &mut std::collections::HashMap<
        String,
        std::collections::VecDeque<PendingApproval>,
    >,
    firehose: &mut Vec<String>,
    sid: &str,
    approve: bool,
    send: F,
) where
    F: FnOnce(&str, serde_json::Value) -> Result<serde_json::Value>,
{
    // Peek — do NOT pop until the daemon ACKs.
    let head = match pending_approvals.get(sid).and_then(|q| q.front()) {
        Some(p) => p.clone(),
        None => return,
    };
    let label = if approve { "approve" } else { "deny" };
    let req = build_respond_payload(sid, &head, approve);
    let payload = match serde_json::to_value(&req) {
        Ok(v) => v,
        Err(e) => {
            firehose.push(format!(
                "[{sid}] !! {label} input_required ({}) tool={} payload encode failed: {e}",
                head.request_id, head.tool_name
            ));
            return;
        }
    };
    match send(RespondToInputRequiredRequest::METHOD, payload) {
        Ok(v) => match serde_json::from_value::<RespondToInputRequiredResponse>(v) {
            Ok(parsed) => {
                // Both accepted=true (daemon relayed) and accepted=false
                // (daemon has no record — timeout / already answered)
                // are terminal from the daemon's perspective. Pop.
                if let Some(q) = pending_approvals.get_mut(sid) {
                    q.pop_front();
                    if q.is_empty() {
                        pending_approvals.remove(sid);
                    }
                }
                let verb = if approve { "approved" } else { "denied" };
                firehose.push(format!(
                    "[{sid}] {verb} input_required ({}) tool={} accepted={}",
                    head.request_id, head.tool_name, parsed.accepted
                ));
            }
            Err(parse_err) => {
                // Protocol/shape error — the daemon side is still
                // waiting because we cannot prove it received the
                // decision. Recreates the B1 data-loss class otherwise:
                // a malformed Ok must NOT pop the queue. Keep the
                // prompt visible so the user can retry.
                firehose.push(format!(
                    "[{sid}] !! {label} input_required ({}) tool={} response decode failed: {parse_err}",
                    head.request_id, head.tool_name
                ));
            }
        },
        Err(e) => {
            // RPC failed — keep the prompt queued so the user can
            // retry once the daemon recovers. Audit entry names the
            // attempted decision direction AND tool name so the
            // failure context is recoverable from the firehose.
            firehose.push(format!(
                "[{sid}] !! {label} input_required ({}) tool={} rpc failed: {e}",
                head.request_id, head.tool_name
            ));
        }
    }
}

/// mu-gih (Stage 3 / I4): drop pending-approval queues for any
/// session_id no longer in the daemon's live session list. Without
/// this the queue leaks across long sessions and a stale prompt
/// could later resurface in the modal when its row rotates back
/// into view. The daemon-side pending request has its own timeout;
/// the TUI's loss of the entry here is informational, not
/// load-bearing.
fn prune_pending_approvals_to_live(
    pending_approvals: &mut std::collections::HashMap<
        String,
        std::collections::VecDeque<PendingApproval>,
    >,
    live: &std::collections::HashSet<String>,
    firehose: &mut Vec<String>,
) {
    let stale: Vec<String> = pending_approvals
        .keys()
        .filter(|sid| !live.contains(sid.as_str()))
        .cloned()
        .collect();
    for sid in stale {
        if let Some(queue) = pending_approvals.remove(&sid) {
            if !queue.is_empty() {
                firehose.push(format!(
                    "[{sid}] dropped {} pending approval(s) — session no longer present",
                    queue.len()
                ));
            }
        }
    }
}

/// mu-gih (Stage 3 / I7 + I3): typed handler for
/// `session.input_required`. Pulls the field shape from
/// [`InputRequiredEvent`] so a protocol drift fails the compile, and
/// dedupes by `(session_id, request_id)` so a replayed notification
/// refreshes the existing entry in place rather than enqueuing a
/// phantom second prompt.
fn handle_input_required(
    firehose: &mut Vec<String>,
    pending_approvals: &mut std::collections::HashMap<
        String,
        std::collections::VecDeque<PendingApproval>,
    >,
    fallback_sid: &str,
    params: &serde_json::Value,
) {
    let evt: InputRequiredEvent = match serde_json::from_value(params.clone()) {
        Ok(e) => e,
        Err(e) => {
            firehose.push(format!(
                "[{fallback_sid}] !! input_required malformed ({e}) — ignored"
            ));
            return;
        }
    };
    if evt.session_id.is_empty() || evt.request_id.is_empty() {
        firehose.push(format!(
            "[{}] !! input_required missing session_id or request_id — ignored",
            evt.session_id
        ));
        return;
    }
    let queue = pending_approvals.entry(evt.session_id.clone()).or_default();
    if let Some(existing) = queue.iter_mut().find(|p| p.request_id == evt.request_id) {
        existing.tool_call_id = evt.tool_call_id;
        existing.tool_name = evt.tool_name.clone();
        existing.arguments = evt.arguments;
        existing.summary = evt.summary;
        firehose.push(format!(
            "[{}] !! input_required ({}) tool={} (duplicate refreshed)",
            evt.session_id, evt.request_id, evt.tool_name
        ));
    } else {
        firehose.push(format!(
            "[{}] !! input_required ({}) tool={}",
            evt.session_id, evt.request_id, evt.tool_name
        ));
        queue.push_back(PendingApproval {
            request_id: evt.request_id,
            tool_call_id: evt.tool_call_id,
            tool_name: evt.tool_name,
            arguments: evt.arguments,
            summary: evt.summary,
        });
    }
}

fn handle_notification(
    sessions: &mut [SessionRow],
    firehose: &mut Vec<String>,
    latest_status: &mut std::collections::HashMap<String, ProviderStatusSnapshot>,
    pending_approvals: &mut std::collections::HashMap<
        String,
        std::collections::VecDeque<PendingApproval>,
    >,
    method: &str,
    params: &serde_json::Value,
) {
    let sid = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let find = |sessions: &mut [SessionRow], sid: &str| -> Option<usize> {
        sessions
            .iter()
            .position(|r| r.session_id.as_deref() == Some(sid))
    };
    match method {
        "session.text_delta" => {
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(i) = find(sessions, sid) {
                sessions[i].phase = format!("streaming (+{}b)", delta.len());
                sessions[i].status = SessionStatus::Running;
            }
            // Avoid flooding the firehose with every token — only the first 20 chars.
            let snip: String = delta.chars().take(20).collect();
            firehose.push(format!("[{sid}] δ {snip:?}"));
        }
        "session.assistant_text_finalized" => {
            // mu-wk2: finalized text arrives. Mark the session as
            // having finalized text to distinguish from streaming-only.
            let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
            firehose.push(format!("[{sid}] finalized ({} chars)", text.len()));
        }
        "session.tool_call_started" => {
            let name = params
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let tid = params
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            firehose.push(format!("[{sid}] tool.start {name} ({tid})"));
            if let Some(i) = find(sessions, sid) {
                sessions[i].phase = format!("tool: {name}");
            }
        }
        "session.tool_call_completed" => {
            let tid = params
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let outcome = params
                .get("outcome")
                .and_then(|o| o.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            firehose.push(format!("[{sid}] tool.done {tid} {outcome}"));
        }
        "session.callout" => {
            let title = params.get("title").and_then(|v| v.as_str()).unwrap_or("");
            firehose.push(format!("[{sid}] callout: {title}"));
        }
        "session.input_required" => {
            // mu-gih (Stage 3 / I7): decode the notification through
            // the typed `InputRequiredEvent` struct so the field shape
            // (`tool_call_id`, `tool_name`, `arguments`, `summary`) is
            // pinned to the protocol crate at compile time. A
            // malformed/partial notification falls through to the
            // missing-fields branch instead of silently rendering "?"
            // / null at modal-paint time.
            handle_input_required(firehose, pending_approvals, sid, params);
        }
        "session.provider_status" => {
            // mu-035 Phase B: store the latest snapshot per session.
            // The phase line + right-pane affordance read this on
            // every render, so the visible "thinking 3.4s" advances
            // every tick (~1s).
            let state = params
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let elapsed_ms = params
                .get("elapsed_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let bytes_received = params.get("bytes_received").and_then(|v| v.as_u64());
            let tool_call_id = params
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            // Detect TRANSITION (state changed since last snapshot)
            // so we only log to the firehose on state changes — the
            // periodic re-emits would otherwise flood at ~1/sec.
            let is_transition = latest_status
                .get(sid)
                .map(|prev| prev.state != state)
                .unwrap_or(true);
            latest_status.insert(
                sid.to_string(),
                ProviderStatusSnapshot {
                    state: state.clone(),
                    elapsed_ms,
                    bytes_received,
                    tool_call_id,
                    received_at: Instant::now(),
                },
            );
            if is_transition {
                firehose.push(format!("[{sid}] status → {state}"));
            }
        }
        "session.done" => {
            let stop = params
                .get("stop_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let elapsed_ms = params
                .get("elapsed_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            firehose.push(format!("[{sid}] done stop={stop} elapsed={elapsed_ms}ms"));
            if let Some(i) = find(sessions, sid) {
                sessions[i].status = SessionStatus::Done;
                sessions[i].phase = format!("done ({stop})");
                if let Some(usage) = params.get("usage") {
                    let inp = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let out = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    sessions[i].tokens_kilo = ((inp + out) / 1000) as u32;
                }
            }
            // Clear the status snapshot — session is no longer
            // running. The next ask will re-populate.
            latest_status.remove(sid);
        }
        "session.error" => {
            let msg = params
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("error");
            firehose.push(format!("[{sid}] !! error: {msg}"));
            latest_status.remove(sid);
        }
        other => {
            // Forward-compat: unknown methods get logged but don't crash.
            firehose.push(format!("[{sid}] {other}"));
        }
    }
    // Cap firehose length to avoid unbounded growth.
    if firehose.len() > 500 {
        let drop = firehose.len() - 500;
        firehose.drain(..drop);
    }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let s = s % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

// ── Mock data ───────────────────────────────────────────────────────

fn mock_sessions() -> Vec<SessionRow> {
    vec![
        SessionRow {
            short_id: "impl".into(),
            title: "mu-035 implementation".into(),
            status: SessionStatus::Running,
            model: "openai-codex / gpt-5.5".into(),
            cost_usd: 0.38,
            tokens_kilo: 118,
            phase: "awaiting first token (4.2s)".into(),
            session_id: None,
        },
        SessionRow {
            short_id: "design".into(),
            title: "mu-036 autonomous loop spec".into(),
            status: SessionStatus::Running,
            model: "anthropic / haiku-4.5".into(),
            cost_usd: 0.02,
            tokens_kilo: 14,
            phase: "streaming".into(),
            session_id: None,
        },
        SessionRow {
            short_id: "review".into(),
            title: "mu-022 edit tool review".into(),
            status: SessionStatus::Idle,
            model: "openrouter / sonnet-4.6".into(),
            cost_usd: 0.11,
            tokens_kilo: 22,
            phase: "awaiting approval (tool: edit)".into(),
            session_id: None,
        },
        SessionRow {
            short_id: "scout".into(),
            title: "cache ledger probe".into(),
            status: SessionStatus::Done,
            model: "anthropic / haiku-4.5".into(),
            cost_usd: 0.01,
            tokens_kilo: 6,
            phase: "completed".into(),
            session_id: None,
        },
    ]
}

fn mock_firehose() -> Vec<String> {
    vec![
        "00:41 session.created session-7 (impl)".into(),
        "00:41 context.assembly V31 (98k active)".into(),
        "00:41 model.call openai-codex/gpt-5.5".into(),
        "00:42 session.provider_status awaiting_first_token (1s)".into(),
        "00:42 session.text_delta start".into(),
        "00:42 session.tool_call_started bash --bash-yolo cargo check".into(),
        "00:42 session.tool_call_completed ok (exit 0)".into(),
        "00:43 session.provider_status streaming".into(),
    ]
}

// ── Render ──────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // mu-o1y7: F3 in Inline mode renders only the inline viewport
    // (footer + input region — phase 2c wires real content; phase 2b
    // lands a placeholder). Transcript lives in multiplexer scrollback
    // via insert_before (emitted from outside `ui` in phase 2c).
    // Skip the full-screen header/tabs/firehose layout entirely; the
    // inline viewport is sized to fit just the inline content.
    if matches!(app.mode, ViewMode::SessionDetail)
        && matches!(app.current_mode, ViewportMode::Inline(_))
    {
        render_inline_session_detail(f, app, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Length(2),  // mode tabs
            Constraint::Min(10),    // main body
            Constraint::Length(10), // firehose (8 lines content + borders)
            Constraint::Length(1),  // status line
        ])
        .split(area);

    render_header(f, app, chunks[0]);
    render_tabs(f, app, chunks[1]);
    match app.mode {
        ViewMode::CommandCenter => render_command_center(f, app, chunks[2]),
        ViewMode::SessionTree => render_placeholder(f, chunks[2], "Session Tree", "F2"),
        ViewMode::SessionDetail => render_session_detail(f, app, chunks[2]),
        ViewMode::Context => render_placeholder(f, chunks[2], "Context Inspector", "F4"),
        ViewMode::Usage => render_usage(f, app, chunks[2]),
        ViewMode::Tools => render_placeholder(f, chunks[2], "Tools / MCP / Skills", "F6"),
        ViewMode::Router => render_placeholder(f, chunks[2], "Router / Proxy", "F7"),
        ViewMode::Events => render_events_explorer(f, app, chunks[2]),
        // ^ takes &mut for in-render clamping of events_scroll_offset
        ViewMode::Mailbox => {
            render_placeholder(f, chunks[2], "Mailbox (cooperating sessions)", "F9")
        }
    }
    render_firehose(f, app, chunks[3]);
    render_statusline(f, app, chunks[4]);
    // mu-gih: layered LAST so the modal overlays every view, the
    // firehose, and any other popup. No-op when there's no pending
    // approval for the selected session.
    render_approval_modal(f, app, area);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let (used, budget) = app.cost_budget;
    let dot_style = if app.connected() {
        Style::default().fg(MUTED_GREEN)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let id_snip = app
        .daemon_id
        .as_deref()
        .map(|s| s.chars().take(6).collect::<String>())
        .unwrap_or_else(|| "—".into());
    let events_compact = if app.daemon_event_count >= 1000 {
        format!(
            "{}.{}k",
            app.daemon_event_count / 1000,
            (app.daemon_event_count / 100) % 10
        )
    } else {
        format!("{}", app.daemon_event_count)
    };
    // mu-ium: surface the current default (provider, model) so the
    // user can see what `n` will create a session against. Updated
    // via :provider / :model palette commands. Snake-cased kind on
    // the wire is what we display; aliases are normalized at
    // create_session time.
    let (default_kind, default_model) = &app.default_provider;
    // Truncate to keep the header readable on narrow terminals.
    let default_model_snip: String = default_model.chars().take(28).collect();
    let line = Line::from(vec![
        Span::styled("mu", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" — command center  "),
        Span::styled("●  ", dot_style),
        Span::raw(format!("daemon {id_snip}")),
        Span::raw("  uptime "),
        Span::raw(fmt_duration(Duration::from_millis(app.daemon_uptime_ms))),
        Span::raw(format!(
            "  events {events_compact}  active {}/{} sess  in-flight {}  ",
            app.daemon_active_session_count,
            app.sessions.len(),
            app.daemon_in_flight_calls_count
        )),
        Span::raw("budget "),
        Span::styled(
            format!("${used:.2}/${budget:.2}"),
            Style::default().fg(if used / budget > 0.7 {
                MUTED_AMBER
            } else {
                MUTED_GREEN
            }),
        ),
        Span::raw("  next-`n`: "),
        Span::styled(
            format!("{default_kind}/{default_model_snip}"),
            Style::default().fg(Color::Cyan),
        ),
        // mu-62s: indicator for whether the next session will carry
        // a system prompt. Don't dump the contents — they can be
        // long. Show ✓ when set, dimmer · when not. :system_prompt
        // with no args echoes the contents to the firehose.
        Span::raw("  sys:"),
        if app.default_system_prompt.is_some() {
            Span::styled("✓", Style::default().fg(MUTED_GREEN))
        } else {
            Span::styled("·", Style::default().fg(Color::DarkGray))
        },
    ]);
    let block = Block::default().borders(Borders::ALL);
    let paragraph = Paragraph::new(line).block(block);
    f.render_widget(paragraph, area);
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let tabs = [
        (ViewMode::CommandCenter, "F1 command"),
        (ViewMode::SessionTree, "F2 sessions"),
        (ViewMode::SessionDetail, "F3 session"),
        (ViewMode::Context, "F4 context"),
        (ViewMode::Usage, "F5 usage"),
        (ViewMode::Tools, "F6 tools"),
        (ViewMode::Router, "F7 router"),
        (ViewMode::Events, "F8 events"),
        (ViewMode::Mailbox, "F9 mailbox"),
    ];
    let mut spans: Vec<Span> = Vec::new();
    for (i, (m, label)) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if *m == app.mode {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    f.render_widget(paragraph, area);
}

fn render_command_center(f: &mut Frame, app: &mut App, area: Rect) {
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // Left: live sessions list
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            // Header: status glyph + session id + provider/model.
            let header = Line::from(vec![
                Span::styled(format!("{} ", s.status.glyph()), s.status.style()),
                Span::styled(
                    format!("{:<10}", s.short_id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(s.model.clone(), Style::default().fg(Color::DarkGray)),
            ]);
            // Detail: live phase + cost + tokens. Phase is the most
            // valuable thing to glance at — replaces the redundant
            // provider/model line we had before. mu-di4: prepend an
            // animated throbber glyph for sessions with a live
            // provider_status snapshot, so a glance across the list
            // shows which sessions are actively working.
            let mut detail_spans: Vec<Span> = vec![Span::raw("    ")];
            if let Some(state) = s
                .session_id
                .as_deref()
                .and_then(|sid| app.throbber_states.get(sid))
            {
                detail_spans.push(
                    Throbber::default()
                        .throbber_style(Style::default().fg(Color::Cyan))
                        .to_symbol_span(state),
                );
            }
            detail_spans.push(Span::styled(
                s.phase.clone(),
                Style::default().fg(Color::Cyan),
            ));
            detail_spans.push(Span::raw(format!("   ${:.2}  ", s.cost_usd)));
            detail_spans.push(Span::raw(format!("{}k tok", s.tokens_kilo)));
            ListItem::new(vec![header, Line::from(detail_spans)])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Live sessions ");
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray));
    f.render_stateful_widget(list, h[0], &mut app.selected_session);

    // Right: selected session detail
    let selected = app
        .selected_session
        .selected()
        .and_then(|i| app.sessions.get(i));
    let detail_text = if let Some(s) = selected {
        let sid_opt = s.session_id.as_deref();
        // Provider-status affordance — preferentially from mu-035
        // session.provider_status (authoritative server-side timer),
        // falling back to the client-side ask_started_at for the
        // ~1s gap before the first periodic tick arrives.
        //
        // Show different colors/labels per state:
        //   awaiting_first_token → yellow ● awaiting first token Xs
        //   thinking             → yellow ● thinking Xs
        //   tool_executing       → magenta ● tool executing Xs (call N)
        //   streaming            → (no affordance — text_delta is its own signal)
        //   idle / done          → no affordance
        let awaiting_line = sid_opt.and_then(|sid| {
            // Prefer authoritative live status from mu-035.
            if let Some(snap) = app.latest_status.get(sid) {
                // Bump the elapsed by how long since we received this
                // snapshot — so the displayed seconds advance smoothly
                // between ticks rather than jumping every ~1s.
                let synthetic_elapsed_ms =
                    snap.elapsed_ms + snap.received_at.elapsed().as_millis() as u64;
                let secs = synthetic_elapsed_ms as f32 / 1000.0;
                let (label, color) = match snap.state.as_str() {
                    "awaiting_first_token" => ("awaiting first token  ", MUTED_AMBER),
                    "thinking" => ("thinking  ", MUTED_AMBER),
                    "tool_executing" => ("tool executing  ", Color::Magenta),
                    "awaiting_tool_result" => ("awaiting tool result  ", Color::Magenta),
                    // mu-di4 followup: streaming gets its own row + animated
                    // throbber. The original early-return assumed text deltas
                    // ARE the visible feedback; with the throbber, motion
                    // continuity across all active states matters more than
                    // avoiding redundancy. Sage green signals "active output"
                    // vs. soft amber (waiting) and magenta (tool work). Idle
                    // still hides — no work happening.
                    "streaming" => ("streaming  ", MUTED_GREEN),
                    "idle" => return None,
                    _ => ("working  ", Color::Cyan),
                };
                let suffix = snap
                    .tool_call_id
                    .as_deref()
                    .map(|cid| format!(" (call {cid})"))
                    .unwrap_or_default();
                // mu-di4: animated throbber glyph replaces the static
                // `● ` bullet. Falls back to a static `● ` only if the
                // throbber state hasn't been initialized yet for this
                // sid (shouldn't happen — App::tick ensures it — but
                // belt-and-suspenders for an off-by-one tick race).
                let throbber_widget = Throbber::default()
                    .throbber_style(Style::default().fg(color).add_modifier(Modifier::BOLD));
                let throbber_span = match app.throbber_states.get(sid) {
                    Some(state) => throbber_widget.to_symbol_span(state),
                    None => Span::styled(
                        "● ",
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                };
                return Some(Line::from(vec![
                    throbber_span,
                    Span::styled(
                        label,
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{secs:.1}s{suffix}"), Style::default().fg(color)),
                ]));
            }
            // Fallback: client-side ask_started_at — bridges the
            // RPC-ack-to-first-tick gap (< 1s typically).
            app.ask_started_at.get(sid).map(|t| {
                let elapsed = t.elapsed();
                Line::from(vec![
                    Span::styled(
                        "● awaiting first token  ",
                        Style::default()
                            .fg(MUTED_AMBER)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:.1}s", elapsed.as_secs_f32()),
                        Style::default().fg(MUTED_AMBER),
                    ),
                ])
            })
        });

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("session ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                s.short_id.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(s.title.clone()),
        ]));
        lines.push(Line::from(""));
        if let Some(l) = awaiting_line {
            lines.push(l);
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled("phase:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                s.phase.clone(),
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Cyan),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("model:    ", Style::default().fg(Color::DarkGray)),
            Span::raw(s.model.clone()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("cost:     ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("${:.2}", s.cost_usd)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("context:  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}k cumulative", s.tokens_kilo)),
        ]));
        lines
    } else {
        vec![Line::from("(no session selected)")]
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Selected session ");
    let paragraph = Paragraph::new(detail_text)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, h[1]);
}

/// F3 — Session Detail. Renders a chronological transcript of the
/// selected session: user / assistant / tool-call / tool-result
/// blocks from the event log, plus a live "(streaming…)" block when
/// text_delta notifications are arriving but no session.done has
/// landed yet.
///
/// Single-column for v1 (the mockup's right-side event timeline can
/// come later — the firehose already serves that role globally).
/// Compute a centered rectangle within `area`, sized as percentages
/// of width and height. Used for modal overlays (F3 picker today;
/// future approval dialogs, etc).
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// F3-on-F3 session picker overlay. Renders on top of the
/// SessionDetail transcript when `app.session_picker_open` is true.
/// Selection is the existing `selected_session` ListState — moving
/// the picker selection also moves the underlying detail view's
/// idea of "current session," which gives a live preview behind the
/// popup.
fn render_session_picker(f: &mut Frame, app: &mut App, area: Rect) {
    let popup_area = centered_rect(70, 60, area);
    // Clear blanks out whatever's underneath so the popup doesn't
    // bleed transcript text through its body.
    f.render_widget(Clear, popup_area);

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|row| {
            let sid = row.session_id.as_deref().unwrap_or("(mock)");
            let phase = if row.phase.is_empty() {
                "idle"
            } else {
                row.phase.as_str()
            };
            let status_glyph = row.status.glyph().to_string();
            // Pad sid to 14 chars for column alignment — typical
            // mu session_ids are 'session-N' which fits comfortably.
            let line = format!("  {status_glyph}  {sid:14}  {phase}");
            ListItem::new(line)
        })
        .collect();

    let title = format!(
        " F3 picker · {} sess · j/k move · Enter select · Esc/F3 cancel ",
        app.sessions.len()
    );
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .style(Style::default().bg(Color::Black)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, popup_area, &mut app.selected_session);
}

/// mu-gih: render the head of the selected session's pending-approval
/// queue as a modal overlay with [A]pprove / [D]eny actions. Layered
/// LAST in `ui()` so it sits on top of every view (including the
/// firehose strip and any other modal). The agent loop is blocked
/// until the user responds, so the modal eats all input keys (handled
/// in `on_key_normal`).
fn render_approval_modal(f: &mut Frame, app: &App, area: Rect) {
    // mu-gih (Stage 3 / I5): F3 picker takes precedence — the modal
    // is suppressed while the picker is open. The pending approval
    // stays queued and resurfaces as soon as the picker closes.
    if app.session_picker_open {
        return;
    }
    let Some(item) = app.current_pending_approval() else {
        return;
    };
    let popup_area = centered_rect(70, 50, area);
    f.render_widget(Clear, popup_area);

    let args_preview = sanitize_arguments_preview(&item.arguments, 200);
    let summary = if item.summary.is_empty() {
        "(no daemon-rendered summary)".to_string()
    } else {
        item.summary.clone()
    };

    let lines: Vec<Line> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  tool:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                item.tool_name.clone(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  call:    ", Style::default().fg(Color::DarkGray)),
            Span::styled(item.tool_call_id.clone(), Style::default().fg(Color::Gray)),
        ]),
        Line::from(vec![
            Span::styled("  reason:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(summary, Style::default().fg(MUTED_AMBER)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  arguments:",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::raw("    "),
            Span::styled(args_preview, Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::raw("    "),
            Span::styled(
                " [A]pprove ",
                Style::default()
                    .fg(Color::Black)
                    .bg(MUTED_GREEN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                " [D]eny ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            // mu-gih (Stage 3 / I6): explicit "Edit unavailable in
            // v1" affordance. The key is bound in `on_key` to a
            // firehose explanation; rendering it disabled here makes
            // the contract surface visible so users don't expect it
            // to work.
            Span::styled(
                " [E]dit (v2) ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]),
    ];

    // mu-gih (Stage 3 minor): title components are bounded so a long
    // tool_name / request_id doesn't blow out the modal on narrow
    // terminals.
    let title_tool = truncate_for_title(&item.tool_name, 32);
    let title_req = truncate_for_title(&item.request_id, 28);
    let title = format!(" session.input_required · {title_tool} · req {title_req} ");
    let block = Block::default().borders(Borders::ALL).title(title).style(
        Style::default()
            .bg(Color::Black)
            .fg(MUTED_AMBER)
            .add_modifier(Modifier::BOLD),
    );
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, popup_area);
}

/// mu-gih (Stage 3 minor + Stage 5 M1): Unicode-safe title truncator.
/// Returns `s` unchanged when within budget; otherwise truncates so
/// that the final string (including the trailing `…`) is at most
/// `max_chars`. Used for modal titles where ratatui would otherwise
/// clip the raw string at the border (or wrap it onto a second line
/// on very narrow terminals).
fn truncate_for_title(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

/// mu-gih: render `arguments` (a serde_json::Value, typically an
/// object) as a single-line JSON projection truncated to `max_chars`.
/// Both literal newlines AND the JSON-escaped `\n` / `\r` sequences
/// collapse to spaces so multi-line tool arguments (e.g. a bash
/// command containing embedded newlines) fit on the modal's preview
/// row. Unicode-safe truncation.
fn sanitize_arguments_preview(arguments: &serde_json::Value, max_chars: usize) -> String {
    let raw = serde_json::to_string(arguments).unwrap_or_else(|_| "(?)".into());
    // Collapse both forms in one pass: the actual newline byte (rare,
    // since serde_json escapes inside string values) and the
    // `\n` / `\r` two-char sequences serde_json emits.
    let collapsed = raw
        .replace("\\n", " ")
        .replace("\\r", " ")
        .replace(['\n', '\r'], " ");
    if collapsed.chars().count() <= max_chars {
        collapsed
    } else {
        // mu-gih (Stage 5 / M1): reserve one char for the ellipsis so
        // the final string fits inside `max_chars`, not max_chars + 1.
        let truncated: String = collapsed
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        format!("{truncated}…")
    }
}

/// mu-sod: anchor a "lines back from bottom" scroll offset against
/// content appends between renders. When `current_total > prev_total`
/// and the user is scrolled up (offset > 0), the visible window
/// would otherwise drift downward by the delta — the bottom moved,
/// but the offset stayed put. Adding the delta to the offset keeps
/// the visible window pinned to the same absolute content.
///
/// When offset is 0 (auto-follow / pinned to bottom), do NOT bump:
/// the user wants to follow the tail, so let appends pull the view
/// along. When content shrank (`current_total < prev_total`), do
/// NOT bump either — the downstream `max_top` clamp will pull the
/// offset back into range without us double-adjusting here.
///
/// Saturating add caps at `u16::MAX`, consistent with the Home key
/// handler that sets the offset to `u16::MAX` to scroll to top.
fn anchor_scroll_offset(prev_total: usize, current_total: usize, offset: u16) -> u16 {
    if offset == 0 || current_total <= prev_total {
        return offset;
    }
    let delta = current_total - prev_total;
    let delta_u16 = u16::try_from(delta).unwrap_or(u16::MAX);
    offset.saturating_add(delta_u16)
}

/// mu-o1y7 phase 2c: emit any new transcript events for F3's selected
/// session into terminal scrollback via `Terminal::insert_before`.
/// Called once per tick when the terminal is in Inline mode and the
/// active view is SessionDetail. Tracks the per-session emit count in
/// `App.f3_emitted_count_by_sid` so re-entering F3 after a dashboard
/// doesn't re-emit content that's already in scrollback.
///
/// Pre-wraps via `render_transcript_lines` to (terminal.width - 2) so
/// the visual line count matches `lines.len()` exactly — that's the
/// `height` argument `insert_before` needs.
///
/// ToolCall + ToolResult pairing only applies within a single emit
/// batch (typical case: first emit after entering F3 contains both;
/// subsequent ticks see one or the other and render as separate
/// blocks). Acceptable tradeoff — incremental emit is a minor visual
/// degradation vs. the full-batch paired form.
fn emit_transcript_delta_inline<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let selected_sid: Option<String> = app
        .selected_session
        .selected()
        .and_then(|i| app.sessions.get(i))
        .and_then(|r| r.session_id.clone());

    let Some(sid) = selected_sid else {
        return Ok(());
    };

    // Compute the delta to emit + the new emitted-count. Scoped so
    // the immutable borrow on `app.transcript_events_by_sid` is
    // released before the mutable borrow for `f3_emitted_count_by_sid`.
    let (lines_to_emit, new_count) = {
        let events = match app.transcript_events_by_sid.get(&sid) {
            Some(e) => e,
            None => return Ok(()),
        };
        let emitted = *app.f3_emitted_count_by_sid.get(&sid).unwrap_or(&0);
        if events.len() <= emitted {
            return Ok(());
        }
        let new_events: Vec<serde_json::Value> = events[emitted..].to_vec();
        let width = terminal.size()?.width as usize;
        let wrap_width = width.saturating_sub(2);
        let lines = render_transcript_lines(&new_events, None, Some(wrap_width));
        (lines, events.len())
    };

    if lines_to_emit.is_empty() {
        return Ok(());
    }

    let height = lines_to_emit.len() as u16;
    terminal.insert_before(height, |buf| {
        let area = buf.area;
        let paragraph = Paragraph::new(lines_to_emit).wrap(Wrap { trim: false });
        Widget::render(paragraph, area, buf);
    })?;
    app.f3_emitted_count_by_sid.insert(sid, new_count);
    Ok(())
}

/// mu-o1y7 phase 2c+2d: F3 inline-mode render. The terminal is in
/// `Viewport::Inline(N)` so `area` is N lines tall at the bottom of
/// the primary screen buffer. Transcript content lives in mux
/// scrollback (emitted by `emit_transcript_delta_inline` from `run`);
/// this function renders only the inline viewport's content:
///
///   [blank input-growth area — phase 3 grows the prompt upward here]
///   `>` <prompt buffer with cursor>
///   `──────`  thin separator
///   F3 · session-id · model · phase   (footer)
///
/// In Normal input mode the prompt row shows a one-line hint; in
/// SendPrompt mode it shows the editable buffer with a real terminal
/// cursor (via `f.set_cursor_position`).
fn render_inline_session_detail(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height;
    if height < 3 {
        // Viewport too small to render the input + separator + footer
        // shape. Bail with just a footer placeholder.
        let line = Line::from(Span::styled(
            " (viewport too narrow for F3 inline) ",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let footer_y_offset = height - 1;
    let separator_y_offset = height - 2;
    let input_y_offset = height - 3;

    // Compute input-row content + cursor position (only for SendPrompt
    // mode does a real cursor get set; Normal mode shows a hint and
    // Command mode shows the `:cmd` buffer).
    let mut cursor_pos: Option<Position> = None;
    let input_line: Line<'static> = match app.input_mode {
        InputMode::SendPrompt => {
            let chars: Vec<char> = app.prompt_buffer.chars().collect();
            let cursor = app.prompt_cursor.min(chars.len());
            let prefix = " > ";
            let prefix_w = prefix.chars().count();
            // Horizontal scroll so the cursor stays visible on long lines.
            // Same logic as render_statusline; phase 3 will replace with
            // a real multi-line edit region that grows upward.
            let avail = (area.width as usize).saturating_sub(prefix_w + 1).max(1);
            let scroll = if cursor >= avail {
                cursor + 1 - avail
            } else {
                0
            };
            let visible: String = chars
                .iter()
                .skip(scroll)
                .take(avail)
                .collect::<String>()
                .replace('\n', "↵");
            cursor_pos = Some(Position {
                x: area.x + (prefix_w + (cursor - scroll)) as u16,
                y: area.y + input_y_offset,
            });
            Line::from(format!("{prefix}{visible}"))
        }
        InputMode::Command => Line::from(format!(" :{}", app.command_buffer)),
        InputMode::Normal => {
            let hint = if app.selected_session.selected().is_some() {
                " press i to send a prompt · F1 dashboard · F3 picker · q quit"
            } else {
                " (no session selected — F1 to dashboard, n to create one)"
            };
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
        }
    };

    // Separator: thin rule across the viewport width.
    let separator_line = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(Color::DarkGray),
    ));

    // Footer: session metadata + (when no session) a hint to switch
    // back. mu-o1y7 phase 3a: when the F3-on-F3 picker is open, swap
    // to a high-contrast picker-mode footer so the operator can tell
    // they're in a different input context. Otherwise typing j/k feels
    // like nothing's changing (observed 2026-05-19).
    let footer_line: Line<'static> = if app.session_picker_open {
        let selected_sid: String = app
            .selected_session
            .selected()
            .and_then(|i| app.sessions.get(i))
            .map(|r| r.short_id.clone())
            .unwrap_or_else(|| "—".to_string());
        Line::from(vec![
            Span::styled(
                " F3 picker ",
                Style::default()
                    .bg(MUTED_AMBER)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("previewing {selected_sid}"),
                Style::default()
                    .fg(MUTED_AMBER)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " · j/k move · Enter commit · Esc cancel",
                Style::default().fg(Color::Gray),
            ),
        ])
    } else {
        match app
            .selected_session
            .selected()
            .and_then(|i| app.sessions.get(i))
        {
            Some(r) => {
                let id = if r.short_id.is_empty() {
                    "?".to_string()
                } else {
                    r.short_id.clone()
                };
                let phase = if r.phase.is_empty() {
                    "idle".to_string()
                } else {
                    r.phase.clone()
                };
                Line::from(vec![
                    Span::styled(" F3 · ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("session {id}"), Style::default().fg(Color::Cyan)),
                    Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                    Span::styled(r.model.clone(), Style::default().fg(Color::Gray)),
                    Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                    Span::styled(phase, Style::default().fg(MUTED_AMBER)),
                    Span::styled(
                        format!("  ${:.2}", r.cost_usd),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            }
            None => Line::from(Span::styled(
                " F3 · (no session selected)",
                Style::default().fg(Color::DarkGray),
            )),
        }
    };

    // Compose the viewport: blank top region (room for phase 3's
    // upward-growing prompt), then input row, separator, footer.
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height as usize);
    for _ in 0..input_y_offset {
        lines.push(Line::from(""));
    }
    lines.push(input_line);
    let _ = separator_y_offset; // index-only; line already at the right slot
    lines.push(separator_line);
    let _ = footer_y_offset;
    lines.push(footer_line);

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, area);

    if let Some(pos) = cursor_pos {
        f.set_cursor_position(pos);
    }
}

fn render_session_detail(f: &mut Frame, app: &mut App, area: Rect) {
    // Identify the selected session.
    let selected_sid: Option<String> = app
        .selected_session
        .selected()
        .and_then(|i| app.sessions.get(i))
        .and_then(|r| r.session_id.clone());

    let Some(sid) = selected_sid else {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Session Detail (F3) ");
        let body = vec![
            Line::from(""),
            Line::from("  (no session selected)"),
            Line::from(""),
            Line::from(Span::styled(
                "  Press F1 to go back, select a session with j/k, then F3 to view its transcript.",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        f.render_widget(Paragraph::new(body).block(block), area);
        return;
    };

    // Layout: header strip + transcript body.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Header strip: the selected session's identity.
    let row = app
        .sessions
        .iter()
        .find(|r| r.session_id.as_deref() == Some(sid.as_str()));
    let header_lines: Vec<Line> = if let Some(r) = row {
        let phase_style = match r.status {
            SessionStatus::Running => Style::default().fg(MUTED_GREEN),
            SessionStatus::Done => Style::default().fg(Color::DarkGray),
            SessionStatus::Idle => Style::default().fg(MUTED_AMBER),
        };
        vec![Line::from(vec![
            Span::styled("session ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                r.short_id.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(r.model.clone(), Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(r.phase.clone(), phase_style),
        ])]
    } else {
        vec![Line::from(format!("session {sid}"))]
    };
    f.render_widget(
        Paragraph::new(header_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Session Detail (F3) "),
        ),
        chunks[0],
    );

    // Transcript body. Pull cached events from the last
    // session.events poll; degrade to a "(loading…)" placeholder if
    // we haven't fetched yet.
    let body_lines: Vec<Line> = if let Some(events) = app.transcript_events_by_sid.get(&sid) {
        let streaming_partial = app.streaming_text.get(&sid).map(String::as_str);
        // mu-2zs: compute the inner content width so push_block can
        // pre-wrap each row, keeping the `│ ` border on every visual
        // line. Outer block borders take 2 columns; the `│ ` prefix
        // takes 2 more; leave a small visual gutter on the right.
        let wrap_width = (chunks[1].width as usize).saturating_sub(5);
        render_transcript_lines(events, streaming_partial, Some(wrap_width))
    } else {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "  loading transcript…",
                Style::default().fg(Color::DarkGray),
            )),
        ]
    };
    // Scroll: by default pin to bottom (newest content). When the
    // user has scrolled up (transcript_scroll_offset > 0), back off
    // from the bottom by that many lines. Compute the scroll arg
    // ratatui wants — it's "skip N lines from the top before
    // rendering." We approximate visible_lines using the inner
    // height of the chunk; this is a rough estimate because line
    // wrapping (from Wrap { trim: false }) can produce more visual
    // lines than `body_lines.len()`. v1 ignores wrap-expansion; a
    // future slice can use ratatui's `LineComposer` to count
    // post-wrap rows accurately.
    let inner_height = chunks[1].height.saturating_sub(2) as usize; // -2 for borders
    let total_lines = body_lines.len();
    // mu-sod: anchor against append-during-scroll. If total grew
    // since last render AND the user is scrolled up, bump the
    // offset by the delta so the visible window stays on the same
    // absolute lines instead of drifting downward.
    app.transcript_scroll_offset = anchor_scroll_offset(
        app.prev_transcript_total_lines,
        total_lines,
        app.transcript_scroll_offset,
    );
    app.prev_transcript_total_lines = total_lines;
    let max_top = total_lines.saturating_sub(inner_height);
    // Same clamp as F8: don't let the stored offset exceed max
    // scrollable range. Without this, the title shows "scrolled up 130"
    // while the view is pinned to the top, and the user has to scroll
    // down past the phantom offset before motion resumes. This also
    // handles content shrinkage: when total_lines drops, the anchored
    // offset is brought back into range so the user doesn't see a
    // phantom-scrolled state.
    if (app.transcript_scroll_offset as usize) > max_top {
        app.transcript_scroll_offset = max_top as u16;
    }
    let scroll_y = (max_top.saturating_sub(app.transcript_scroll_offset as usize)) as u16;

    let title = if app.transcript_scroll_offset == 0 {
        " Transcript ".to_string()
    } else {
        format!(
            " Transcript (scrolled up {}/{} · End to bottom · Home to top) ",
            app.transcript_scroll_offset, max_top
        )
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(body_lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    f.render_widget(paragraph, chunks[1]);

    // F3-on-F3 picker: render LAST so it overlays both the header
    // strip and the transcript pane. Picker reads / writes the same
    // selected_session ListState that the header / transcript above
    // read for "current session," so the underlying view live-
    // previews each highlighted candidate.
    if app.session_picker_open {
        render_session_picker(f, app, area);
    }
}

/// Build the line buffer for the transcript pane. Renders one block
/// per significant event (user / assistant / tool_call+tool_result /
/// done / error), each with a small header strip and indented body.
///
/// Streaming text (live deltas) is appended as a tentative
/// "assistant (streaming…)" block at the end when present.
fn render_transcript_lines(
    events: &[serde_json::Value],
    streaming_partial: Option<&str>,
    wrap_width: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));

    // Pair ToolCall / ToolResult by call_id so we render them
    // together. Keep a small map.
    let mut tool_results: std::collections::HashMap<String, &serde_json::Value> =
        std::collections::HashMap::new();
    for ev in events {
        if let Some(p) = ev.get("payload") {
            if p.get("kind").and_then(|k| k.as_str()) == Some("tool_result") {
                if let Some(cid) = p.get("call_id").and_then(|v| v.as_str()) {
                    tool_results.insert(cid.to_string(), ev);
                }
            }
        }
    }

    for ev in events {
        let Some(payload) = ev.get("payload") else {
            continue;
        };
        let kind = payload.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let ts = ev
            .get("timestamp_unix_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let _ = ts; // future: show timestamps in a compact column

        match kind {
            "user_message" => {
                let content = payload
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                push_block(&mut lines, "you", Color::Cyan, content, wrap_width);
            }
            "assistant_message_event" => {
                // AssistantMessage.content is Vec<ContentBlock> where
                // ContentBlock is a tagged enum: {type: "text",
                // text: "..."} | {type: "tool_call", ...} |
                // {type: "thinking", ...}. Pull and join all `text`
                // blocks; tool_call blocks are surfaced separately as
                // their own ToolCall events.
                let text = payload
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|block| {
                                let t = block.get("type").and_then(|v| v.as_str())?;
                                if t == "text" {
                                    block.get("text").and_then(|v| v.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                let has_tool_call = payload
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter().any(|block| {
                            block.get("type").and_then(|v| v.as_str())
                                == Some("tool_call")
                        })
                    })
                    .unwrap_or(false);
                if text.is_empty() {
                    // Tool-only turns: the magenta tool block(s) below
                    // are the visible record of what the model did —
                    // skip the empty "assistant" header (mu-ooy). If
                    // text is empty AND no tool_calls are present,
                    // surface a debug marker so the turn stays visible
                    // (shouldn't normally happen).
                    if !has_tool_call {
                        push_block(
                            &mut lines,
                            "assistant",
                            Color::White,
                            "(no text in this turn)",
                            wrap_width,
                        );
                    }
                } else {
                    push_block(&mut lines, "assistant", Color::White, &text, wrap_width);
                }
            }
            "tool_call" => {
                let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let call_id = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("?");
                let args = payload
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let args_str = match serde_json::to_string(&args) {
                    Ok(s) if s.len() <= 200 => s,
                    Ok(s) => format!("{}…", &s[..199.min(s.len() - 1)]),
                    Err(_) => "?".to_string(),
                };
                let mut body = format!("args: {args_str}");
                if let Some(result_ev) = tool_results.get(call_id) {
                    if let Some(result_p) = result_ev.get("payload") {
                        let result_content = result_p
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let is_error = result_p
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let result_snip: String =
                            result_content.chars().take(400).collect();
                        let marker = if is_error { "!! error" } else { "→ ok" };
                        body.push_str(&format!("\n{marker}: {result_snip}"));
                        if result_content.chars().count() > 400 {
                            body.push('…');
                        }
                    }
                }
                push_block(
                    &mut lines,
                    &format!("tool: {name}"),
                    Color::Magenta,
                    &body,
                    wrap_width,
                );
            }
            "tool_result" => {
                // Rendered inline under its ToolCall above. Skip.
            }
            "done" => {
                let stop = payload
                    .get("stop_reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let elapsed = payload
                    .get("elapsed_ms")
                    .and_then(|v| v.as_u64())
                    .map(|m| format!("{m}ms"))
                    .unwrap_or_else(|| "—".into());
                // mu-779s: surface iteration_cap distinctly — the conversation
                // didn't finish naturally; the operator should know they can
                // ask a follow-up or raise --max-iterations.
                let (label, color) = if stop == "iteration_cap" {
                    let turns = payload
                        .get("turn_count")
                        .and_then(|v| v.as_u64())
                        .map(|n| format!(" ({n} turns)"))
                        .unwrap_or_default();
                    (
                        format!(
                            "─── turn budget reached{turns} · ask a follow-up to continue, or restart with --max-iterations · {elapsed} ───"
                        ),
                        Color::Yellow,
                    )
                } else {
                    (
                        format!("─── done · {stop} · {elapsed} ───"),
                        Color::DarkGray,
                    )
                };
                lines.push(Line::from(Span::styled(label, Style::default().fg(color))));
                lines.push(Line::from(""));
            }
            "error" => {
                let msg = payload.get("message").and_then(|v| v.as_str()).unwrap_or("");
                push_block(&mut lines, "ERROR", Color::Red, msg, wrap_width);
            }
            "session_created"
            | "callout"
            | "context_assembly"
            | "session_closed"
            | "provider_status_update"
            // mu-036: autonomous-loop bookkeeping is observability,
            // not transcript content. The firehose surfaces them.
            | "autonomous_iteration_started"
            | "autonomous_iteration_completed"
            | "autonomous_scheduled_wakeup"
            | "autonomous_terminated" => {
                // Sidechannel events — not in the transcript pane.
                // The firehose carries these globally.
            }
            other => {
                lines.push(Line::from(Span::styled(
                    format!("({other})"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    if let Some(partial) = streaming_partial {
        if !partial.is_empty() {
            push_block(
                &mut lines,
                "assistant (streaming…)",
                MUTED_AMBER,
                partial,
                wrap_width,
            );
        }
    }

    lines
}

fn push_block(
    out: &mut Vec<Line<'static>>,
    label: &str,
    color: Color,
    body: &str,
    wrap_width: Option<usize>,
) {
    out.push(Line::from(Span::styled(
        format!("┌─ {label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    for raw_line in body.lines() {
        // mu-2zs: if a wrap width is available, pre-wrap each raw line
        // into multiple visual rows. Without this, the outer Paragraph's
        // Wrap { trim: false } wraps each Line, but the wrapped
        // continuation rows lose the `│ ` prefix — visually the
        // bordered block "escapes" on long content. Pre-wrapping here
        // keeps every visible row anchored inside the box.
        match wrap_width {
            Some(w) if w > 0 => {
                for visual_row in wrap_body_line(raw_line, w) {
                    out.push(Line::from(vec![
                        Span::styled("│ ", Style::default().fg(color)),
                        Span::raw(visual_row),
                    ]));
                }
            }
            _ => {
                out.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(color)),
                    Span::raw(raw_line.to_string()),
                ]));
            }
        }
    }
    out.push(Line::from(Span::styled(
        "└─".to_string(),
        Style::default().fg(color),
    )));
    out.push(Line::from(""));
}

/// mu-2zs: word-aware wrap of `line` to fit `width` columns. Long
/// words that exceed `width` are split mid-word so we always make
/// progress. Returns a Vec of strings, one per visual row.
///
/// Char-based width (not grapheme/unicode-width) for simplicity; this
/// over-counts for combining marks and under-counts for CJK wide
/// characters, but the failure mode is only "wraps one column early
/// or late" which is much milder than the bug it replaces (continuation
/// rows escaping the bordered block entirely).
fn wrap_body_line(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for word in line.split_inclusive(' ') {
        let word_len = word.chars().count();
        if current_len + word_len <= width {
            current.push_str(word);
            current_len += word_len;
            continue;
        }
        // Word doesn't fit on the current row.
        if !current.is_empty() {
            rows.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if word_len <= width {
            current.push_str(word);
            current_len = word_len;
        } else {
            // Single word longer than width — split on char boundaries.
            for ch in word.chars() {
                if current_len + 1 > width {
                    rows.push(std::mem::take(&mut current));
                    current_len = 0;
                }
                current.push(ch);
                current_len += 1;
            }
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// F8 — Events explorer. Full-screen scrollable view of the
/// in-memory firehose buffer (up to 500 lines). Same scroll
/// semantics as F3 transcript: 0 offset = pinned to bottom, >0 =
/// scrolled up.
///
/// Filter via `:filter <substring>` palette command;
/// `:clear-filter` or `:filter` (no arg) clears it.
///
/// Takes `&mut App` so we can clamp `events_scroll_offset` to the
/// maximum scrollable range here. Without that clamp, the stored
/// offset could grow past the actual top (the key handler doesn't
/// know terminal height) and the user would see "scrolled up 130"
/// while the view stayed pinned at the top, then have to scroll
/// down past 74 before motion resumed.
fn render_events_explorer(f: &mut Frame, app: &mut App, area: Rect) {
    let filter = app.events_filter.trim().to_string();
    // Build the filtered line list. Cheap — firehose is capped at 500.
    let lines_owned: Vec<String> = if filter.is_empty() {
        app.firehose.to_vec()
    } else {
        app.firehose
            .iter()
            .filter(|l| l.contains(&filter))
            .cloned()
            .collect()
    };
    let total = lines_owned.len();
    let inner_height = area.height.saturating_sub(2) as usize;
    // mu-sod: anchor F8 events against append-during-scroll. Same
    // shape as the F3 transcript anchor: when total grew since
    // last render AND user is scrolled up, bump offset by the
    // delta so the visible window stays on the same absolute lines.
    app.events_scroll_offset =
        anchor_scroll_offset(app.prev_events_total_lines, total, app.events_scroll_offset);
    app.prev_events_total_lines = total;
    let max_top = total.saturating_sub(inner_height);
    // Clamp stored offset to the actual maximum here, so the title
    // shows a value the view can actually reflect and subsequent
    // PageDown presses don't have to "burn off" phantom offset
    // before they move anything. Also handles content shrinkage
    // (e.g. filter toggled on) so the anchored offset doesn't
    // strand the view past the new top.
    if (app.events_scroll_offset as usize) > max_top {
        app.events_scroll_offset = max_top as u16;
    }
    let scroll_y = (max_top.saturating_sub(app.events_scroll_offset as usize)) as u16;
    let title_suffix = if filter.is_empty() {
        format!("{total} events")
    } else {
        format!(
            "{total} events matching {:?}  (:clear-filter to reset)",
            filter
        )
    };
    let scroll_suffix = if app.events_scroll_offset > 0 {
        format!(
            "  · scrolled up {}/{} · End to bottom · Home to top",
            app.events_scroll_offset, max_top
        )
    } else {
        " · End=bottom · Home=top · :filter to filter".into()
    };
    let title = format!(" Events Explorer (F8) — {title_suffix}{scroll_suffix} ");
    let body_lines: Vec<Line> = lines_owned.iter().map(|s| Line::from(s.as_str())).collect();
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(body_lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    f.render_widget(paragraph, area);
}

/// mu-xln Phase A — render `daemon.usage_history` as a table.
///
/// One row per (provider, model) group (Phase A doesn't expose
/// time-bucketing yet — the request goes out with no `time_bucket_ms`,
/// so each (provider, model) collapses to a single row spanning all
/// in-memory sessions). Columns favor at-a-glance comparison across
/// models over completeness: TTFT and streaming p95 for "is this
/// model slow?", wall p95 for "is this model expensive in
/// round-trips?", token sums + tool count for "how much work?".
fn render_usage(f: &mut Frame, app: &App, area: Rect) {
    let snapshot_age = app
        .latest_usage_history_at
        .map(|t| t.elapsed().as_secs())
        .map(|s| format!("{s}s ago"))
        .unwrap_or_else(|| "never".into());
    let session_total = app
        .latest_usage_history
        .as_ref()
        .and_then(|v| v.get("session_count_total"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let title = format!(
        " F5 · Usage / Cache · {session_total} sessions in scope · snapshot {snapshot_age} "
    );

    let rows_json = app
        .latest_usage_history
        .as_ref()
        .and_then(|v| v.get("rows"))
        .and_then(|v| v.as_array());
    let Some(rows_json) = rows_json else {
        let block = Block::default().title(title).borders(Borders::ALL);
        let p = Paragraph::new(Line::from(Span::styled(
            "No usage data yet — run an ask_session, or wait for the next session.done.",
            Style::default().fg(Color::DarkGray),
        )))
        .block(block)
        .wrap(Wrap { trim: false });
        f.render_widget(p, area);
        return;
    };

    let header_cells = [
        "provider", "model", "sess", "ttft p95", "strm p95", "tool p95", "wall p95", "in tok",
        "out tok", "cache%", "tools", "err",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells)
        .style(Style::default().fg(MUTED_AMBER))
        .height(1);

    let body_rows: Vec<Row> = rows_json
        .iter()
        .map(|row| {
            let provider = row
                .get("provider_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let model = row.get("model").and_then(|v| v.as_str()).unwrap_or("?");
            let sessions = row
                .get("session_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let ttft_p95 = fmt_ms_p95(row.get("ttft_ms"));
            let stream_p95 = fmt_ms_p95(row.get("streaming_ms"));
            let tool_p95 = fmt_ms_p95(row.get("tool_total_ms"));
            let wall_p95 = fmt_ms_p95(row.get("wall_ms"));
            let in_tok = row
                .get("input_tokens_sum")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let out_tok = row
                .get("output_tokens_sum")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cache_read = row
                .get("cache_read_input_tokens_sum")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cache_creation = row
                .get("cache_creation_input_tokens_sum")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            // Cache hit % = cache_read / (input + cache_read + cache_creation).
            // Anthropic surfaces these three as distinct counters; the
            // denominator is "total input tokens charged" (non-cached
            // input + cache-read tokens that did hit + cache-creation
            // tokens that wrote new entries). "—" when no input has
            // flowed yet to avoid a 0/0 NaN.
            let total_input = in_tok + cache_read + cache_creation;
            let cache_pct_str = if total_input == 0 {
                "—".to_string()
            } else {
                let pct = (cache_read as f64 * 100.0) / total_input as f64;
                format!("{pct:.0}%")
            };
            let tools = row
                .get("tool_call_count_sum")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let errors = row.get("error_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let err_style = if errors > 0 {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(provider.to_string()),
                Cell::from(model.to_string()),
                Cell::from(sessions.to_string()),
                Cell::from(ttft_p95),
                Cell::from(stream_p95),
                Cell::from(tool_p95),
                Cell::from(wall_p95),
                Cell::from(format_thousands(in_tok)),
                Cell::from(format_thousands(out_tok)),
                Cell::from(cache_pct_str),
                Cell::from(tools.to_string()),
                Cell::from(errors.to_string()).style(err_style),
            ])
            .height(1)
        })
        .collect();

    let widths = [
        Constraint::Length(14), // provider
        Constraint::Min(18),    // model
        Constraint::Length(5),  // sess
        Constraint::Length(9),  // ttft p95
        Constraint::Length(9),  // strm p95
        Constraint::Length(9),  // tool p95
        Constraint::Length(9),  // wall p95
        Constraint::Length(8),  // in tok
        Constraint::Length(8),  // out tok
        Constraint::Length(6),  // cache%
        Constraint::Length(5),  // tools
        Constraint::Length(4),  // err
    ];
    let table = Table::new(body_rows, widths)
        .header(header)
        .block(Block::default().title(title).borders(Borders::ALL))
        .column_spacing(1);
    f.render_widget(table, area);
}

/// Format the `p95` field of a PercentileStats-shaped value as
/// `"<n>ms"` or `"—"` if the source is `None`/missing.
fn fmt_ms_p95(stats: Option<&serde_json::Value>) -> String {
    stats
        .and_then(|s| s.get("p95"))
        .and_then(|v| v.as_u64())
        .map(|ms| format!("{ms}ms"))
        .unwrap_or_else(|| "—".into())
}

/// Format a u64 with thousands separators (`12345` → `"12,345"`).
fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn render_placeholder(f: &mut Frame, area: Rect, name: &str, fkey: &str) {
    let body = vec![
        Line::from(""),
        Line::from(format!("  {name} view ({fkey})")),
        Line::from(""),
        Line::from(Span::styled(
            "  Not yet implemented in the scaffold.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  Wire integration → projection queries against `mu serve`.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  See mu-ui-mockups-2026-05-10.md for the target design.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {name} "));
    let paragraph = Paragraph::new(body).block(block);
    f.render_widget(paragraph, area);
}

fn render_firehose(f: &mut Frame, app: &App, area: Rect) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let total = app.firehose.len();
    let lines: Vec<Line> = app
        .firehose
        .iter()
        .rev()
        .take(inner_height)
        .rev()
        .map(|s| Line::from(s.as_str()))
        .collect();
    // Surface in the title how much history exists vs is visible —
    // F8 (events explorer, mu-6fv) is the proper place to scroll
    // through the full log. Firehose is the recent-tail strip.
    let title = if total > inner_height {
        format!(" Firehose · last {inner_height} of {total} · F8 for full ")
    } else {
        format!(" Firehose · {total} events ")
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    // Wrap long lines so e.g. full error messages stay visible
    // instead of being cut off at the right border. Wrap-expansion
    // means a 200-char error consumes multiple visible rows; that's
    // the trade we want for readability over density.
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);
}

fn render_statusline(f: &mut Frame, app: &App, area: Rect) {
    // mu-wd2: when in SendPrompt mode, we want a real terminal
    // cursor at the typing position. Hold the (x, y) we want until
    // after render_widget so the cursor lands on top of the painted
    // content.
    let mut cursor_pos: Option<Position> = None;

    let content = match app.input_mode {
        InputMode::Command => format!(":{}", app.command_buffer),
        InputMode::SendPrompt => {
            // mu-wd2 + mu-h04: render with cursor visible and scroll
            // horizontally if the prompt is wider than the
            // statusline. Suppress the inline hint while typing —
            // it was previously fused with the prompt text after
            // four spaces, which looked like the prompt was
            // truncated/obscured for short inputs (mu-h04). The
            // hint comes back in Normal mode in the statusline's
            // bottom bar (the F1 command center also documents the
            // bindings).
            let chars: Vec<char> = app.prompt_buffer.chars().collect();
            let cursor = app.prompt_cursor.min(chars.len());
            let prefix = " > ";
            let prefix_w = prefix.chars().count();
            // Available chars for the prompt window — leave 1 cell
            // of headroom past the cursor so the caret doesn't sit
            // at the very edge.
            let avail = (area.width as usize).saturating_sub(prefix_w + 1).max(1);
            let scroll = if cursor >= avail {
                cursor + 1 - avail
            } else {
                0
            };
            let visible: String = chars
                .iter()
                .skip(scroll)
                .take(avail)
                .collect::<String>()
                // Render newlines as a glyph so multi-line prompts
                // don't break statusline rendering (the statusline
                // is one row; alt-enter inserts \n which we want
                // to surface, not silently swallow).
                .replace('\n', "↵");
            cursor_pos = Some(Position {
                x: area.x + (prefix_w + (cursor - scroll)) as u16,
                y: area.y,
            });
            format!("{prefix}{visible}")
        }
        InputMode::Normal => {
            let scroll_hint = match app.mode {
                ViewMode::SessionDetail => "j/k PgUp/PgDn Home/End scroll · ",
                ViewMode::Events => "j/k PgUp/PgDn Home/End scroll · :filter <text> · ",
                _ => "j/k select · ",
            };
            format!(
                " mode: {}   keys: F1-F9 · {}n new · i/Enter send · : palette · q quit",
                app.mode.name(),
                scroll_hint,
            )
        }
    };
    let style = match app.input_mode {
        InputMode::Command => Style::default().fg(Color::Black).bg(MUTED_AMBER),
        InputMode::SendPrompt => Style::default().fg(Color::Black).bg(Color::Cyan),
        InputMode::Normal => Style::default().fg(Color::Black).bg(Color::Gray),
    };
    let line = Paragraph::new(content).style(style);
    f.render_widget(line, area);
    if let Some(pos) = cursor_pos {
        f.set_cursor_position(pos);
    }
}

// ── Main loop ───────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(about = "mu-tui — terminal UI for `mu serve`")]
struct Cli {
    /// Path to the `mu` binary to spawn. If omitted, runs in mock-data
    /// scaffold mode (no live daemon).
    #[arg(long)]
    mu_binary: Option<PathBuf>,

    /// Working directory passed to mu serve. Defaults to cwd.
    #[arg(long)]
    mu_cwd: Option<PathBuf>,

    /// Tools to enable on the daemon. Comma-separated. Default
    /// `read,glob,grep`.
    #[arg(long, value_delimiter = ',', default_value = "read,glob,grep")]
    tools: Vec<String>,

    /// Pass --bash-yolo to the daemon.
    #[arg(long)]
    bash_yolo: bool,

    /// Pass --bash-prompt to the daemon. Enables per-call approval
    /// gating for non-allowlisted bash commands via the mu-gih modal.
    /// Mutually exclusive with `--bash-yolo` at the daemon level
    /// (yolo bypasses the prompt path).
    #[arg(long)]
    bash_prompt: bool,

    /// Default provider kind (used for `n` → create_session).
    /// When omitted, falls back to `[ui.tui].default_provider` from
    /// `~/.config/mu/config.toml`, then to `"anthropic_api"` (mu-l1z).
    #[arg(long)]
    provider: Option<String>,

    /// Default provider model (used for `n` → create_session).
    /// When omitted, falls back to `[ui.tui].default_model` from
    /// `~/.config/mu/config.toml`, then to
    /// `"claude-haiku-4-5-20251001"` (mu-l1z).
    #[arg(long)]
    model: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // mu-l1z: load operator config so CLI flags can fall through to
    // it. CLI > config > code default (clap's old hard-coded values
    // now live on the Config struct's Default impl, so behavior with
    // no config file is unchanged).
    let config = mu_core::config::Config::load_default();
    let default_provider = cli
        .provider
        .clone()
        .unwrap_or_else(|| config.ui.tui.default_provider.clone());
    let default_model = cli
        .model
        .clone()
        .unwrap_or_else(|| config.ui.tui.default_model.clone());

    // Try to spawn a live mu serve; if it fails, fall through to
    // mock-data scaffold mode (so the TUI is still demoable).
    let mu = match &cli.mu_binary {
        Some(bin) => {
            let cwd = cli
                .mu_cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let tools_refs: Vec<&str> = cli.tools.iter().map(String::as_str).collect();
            match MuClient::spawn(
                &bin.to_string_lossy(),
                &tools_refs,
                &cwd,
                cli.bash_yolo,
                cli.bash_prompt,
            ) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("warning: failed to spawn mu serve: {e}; running in mock-data mode");
                    None
                }
            }
        }
        None => None,
    };

    // mu-o1y7 phase 2a: App is built once in main and lives across
    // terminal-mode rebuilds. The outer loop owns the terminal handle
    // and the current ViewportMode; `run` returns RunOutcome::ModeChange
    // when in-loop logic requests a swap (no callers in phase 2a — F3
    // wiring lands in phase 2b). Default Fullscreen preserves the
    // alt-screen takeover behavior from pre-mu-o1y7.
    let mut app = App::new(mu, (default_provider, default_model));
    let mut mode = ViewportMode::Fullscreen;
    let mut terminal = enter_terminal_mode(mode)?;

    let res = loop {
        match run(&mut terminal, &mut app) {
            Ok(RunOutcome::Exit) => break Ok(()),
            Ok(RunOutcome::ModeChange(new_mode)) => {
                // Mid-flight leave errors during a mode swap are
                // logged-ignored: we still want to attempt the new
                // mode, and a half-torn-down terminal isn't a useful
                // place to bail.
                let _ = leave_terminal_mode(&mut terminal, mode);
                terminal = match enter_terminal_mode(new_mode) {
                    Ok(t) => t,
                    Err(e) => break Err(e.into()),
                };
                mode = new_mode;
                app.current_mode = new_mode;
            }
            Err(e) => break Err(e),
        }
    };

    let _ = leave_terminal_mode(&mut terminal, mode);

    res
}

/// mu-o1y7: terminal viewport mode. `Fullscreen` takes over the
/// alternate screen buffer — today's behavior, where the entire TUI
/// lives in an offscreen buffer the terminal restores on exit. Mux
/// scrollback sees nothing of what mu rendered.
///
/// `Inline(N)` lives in the bottom N lines of the primary screen
/// buffer instead. The viewport stays at the bottom; new transcript
/// content emits above it via `Terminal::insert_before`, scrolling
/// naturally into multiplexer scrollback (zellij mod-s, tmux Ctrl-b
/// `[`). Used by F3 to give claude-code / pi-style chat-UX.
///
/// Phase 1 (this commit) lands the enum + setup helpers so the
/// architecture is in place; mu-tui still uses `Fullscreen` everywhere.
/// Phase 2 wires F3's enter/leave to switch to `Inline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewportMode {
    Fullscreen,
    #[allow(dead_code)] // wired up in mu-o1y7 phase 2
    Inline(u16),
}

/// mu-o1y7: outcome of one `run` iteration. Lets `main` distinguish
/// a clean exit (app.quit) from a request to rebuild the terminal in
/// a different viewport mode. Phase 2a wires the plumbing; nothing
/// inside the App currently sets `pending_mode_change`, so the only
/// outcome in practice is `Exit`.
#[derive(Debug)]
enum RunOutcome {
    Exit,
    #[allow(dead_code)] // wired up in mu-o1y7 phase 2b
    ModeChange(ViewportMode),
}

/// mu-o1y7: enable raw mode, set up the terminal for `mode`, and
/// return a ratatui Terminal handle. Mirrors the setup that lived
/// inline in `main` pre-phase-1 — mu-wd2's Kitty Keyboard Protocol
/// push and mu-1jq's terminal title are preserved across both modes.
///
/// Fullscreen takes the alternate screen + mouse capture. Inline does
/// neither: the primary buffer stays writeable and the terminal /
/// multiplexer owns mouse + scroll.
fn enter_terminal_mode(mode: ViewportMode) -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    match mode {
        ViewportMode::Fullscreen => {
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableMouseCapture,
                SetTitle(mu_terminal_title()),
            )?;
        }
        ViewportMode::Inline(_) => {
            // Inline mode lives in the primary buffer; no alt-screen,
            // and mouse capture is left to the terminal/multiplexer so
            // scrollback + text selection behave normally.
            execute!(stdout, SetTitle(mu_terminal_title()))?;
        }
    }
    // mu-wd2: opt into the Kitty Keyboard Protocol. Same in both modes.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let backend = CrosstermBackend::new(stdout);
    let terminal = match mode {
        ViewportMode::Fullscreen => Terminal::new(backend)?,
        ViewportMode::Inline(height) => Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?,
    };
    Ok(terminal)
}

/// mu-o1y7: tear down whatever `enter_terminal_mode` set up. The mode
/// argument tells us whether to LeaveAlternateScreen + DisableMouseCapture
/// (Fullscreen) or just clear the title (Inline). Errors from the
/// keyboard-protocol pop are ignored — terminals without the feature
/// silently no-op the original push, so the pop is benign either way.
fn leave_terminal_mode(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mode: ViewportMode,
) -> io::Result<()> {
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    disable_raw_mode()?;
    match mode {
        ViewportMode::Fullscreen => {
            execute!(
                terminal.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture,
                SetTitle(""),
            )?;
        }
        ViewportMode::Inline(_) => {
            execute!(terminal.backend_mut(), SetTitle(""))?;
        }
    }
    terminal.show_cursor()?;
    Ok(())
}

/// mu-1jq: build the terminal title string, e.g. `μ - tcovert`. Set
/// via crossterm's `SetTitle` (OSC 0); zellij renders as the pane
/// title, kitty / tmux / foot / alacritty as the window title.
/// `USER` env var with a stable fallback so the title is deterministic
/// even when invoked in environments without `$USER` set.
fn mu_terminal_title() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "agent".to_string());
    format!("μ - {user}")
}

/// mu-o1y7: event loop. App is owned by the caller (main) so it
/// survives terminal-mode rebuilds — sessions, transcript cache,
/// in-flight selection, etc. don't reset when F3 swaps between
/// Fullscreen and Inline viewports. Returns `RunOutcome::Exit` on
/// `app.quit`, or `RunOutcome::ModeChange(new_mode)` when in-loop
/// logic sets `app.pending_mode_change`.
fn run<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<RunOutcome>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui(f, &mut *app))?;

        // mu-o1y7 phase 2c: when F3 is in Inline mode, emit any new
        // transcript events for the selected session into terminal
        // scrollback via `Terminal::insert_before`. Multiplexers (zellij
        // mod-s, tmux Ctrl-b `[`) navigate this; zellij's
        // open-buffer-in-editor captures it. The per-session emit count
        // survives mode swaps so re-entering F3 doesn't duplicate
        // already-emitted content.
        if matches!(app.current_mode, ViewportMode::Inline(_))
            && matches!(app.mode, ViewMode::SessionDetail)
        {
            emit_transcript_delta_inline(terminal, app)?;
        }

        // mu-o1y7 phase 3a: drain any pending inline markers into
        // scrollback. Inline-mode only — alt-screen views already see
        // these via the firehose strip. Drains regardless of view so
        // a marker pushed just before a view switch still lands.
        if matches!(app.current_mode, ViewportMode::Inline(_))
            && !app.pending_inline_markers.is_empty()
        {
            let markers = std::mem::take(&mut app.pending_inline_markers);
            for msg in markers {
                terminal.insert_before(1, |buf| {
                    let line = Line::from(Span::styled(
                        msg,
                        Style::default()
                            .fg(MUTED_AMBER)
                            .add_modifier(Modifier::BOLD),
                    ));
                    let paragraph = Paragraph::new(line);
                    Widget::render(paragraph, buf.area, buf);
                })?;
            }
        }

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);
        if event::poll(timeout)? {
            if let Event::Key(k) = event::read()? {
                app.on_key(k.code, k.modifiers);
            }
        }
        if last_tick.elapsed() >= tick_rate {
            app.tick();
            last_tick = Instant::now();
        }
        // mu-82l: Ctrl-X Ctrl-E → user prompt to $EDITOR.
        // mu-62s: Ctrl-X Ctrl-P → default system prompt to $EDITOR.
        // App can't drive these itself because the terminal handoff
        // needs the Terminal handle we own here.
        if app.pending_editor {
            app.pending_editor = false;
            match app.pending_editor_target {
                EditorTarget::PromptBuffer => {
                    if let Err(e) = open_prompt_in_editor(
                        terminal,
                        &mut app.prompt_buffer,
                        &mut app.prompt_cursor,
                    ) {
                        app.firehose.push(format!("[!! editor handoff] {e}"));
                    }
                }
                EditorTarget::SystemPrompt => {
                    // Materialize the option into a working String so
                    // the editor function (mu-82l's signature) can
                    // use the same path. None ⇒ start with an empty
                    // buffer in the editor.
                    let mut buf = app.default_system_prompt.clone().unwrap_or_default();
                    let mut cursor: usize = buf.chars().count();
                    if let Err(e) = open_prompt_in_editor(terminal, &mut buf, &mut cursor) {
                        app.firehose.push(format!("[!! editor handoff] {e}"));
                    } else {
                        // Empty buffer post-edit ⇒ user effectively
                        // cleared the system prompt; store None to
                        // suppress the wire field entirely (matches
                        // :clear_system_prompt semantics).
                        app.default_system_prompt = if buf.is_empty() { None } else { Some(buf) };
                        let preview: String = app
                            .default_system_prompt
                            .as_deref()
                            .unwrap_or("(cleared)")
                            .chars()
                            .take(40)
                            .collect();
                        app.firehose
                            .push(format!("[ok] system_prompt updated: {preview}…"));
                    }
                }
            }
        }
        if app.quit {
            return Ok(RunOutcome::Exit);
        }
        // mu-o1y7 phase 2a: any in-loop logic that needs the terminal
        // rebuilt in a different viewport mode sets this flag. We
        // honor it AFTER `app.quit` (quit takes precedence) so a quit
        // immediately preceded by a mode-change request still exits
        // cleanly.
        if let Some(new_mode) = app.pending_mode_change.take() {
            return Ok(RunOutcome::ModeChange(new_mode));
        }
    }
}

/// mu-82l: Suspend the TUI, hand the terminal to $EDITOR with the
/// current prompt buffer in a tempfile, resume the TUI on exit and
/// pull the edited content back into the buffer. If the editor exits
/// with a non-zero status (e.g. `:cq` in vi), the buffer is left
/// unchanged — that's the standard "cancel" affordance.
///
/// The terminal-handoff sequence mirrors main()'s setup/teardown in
/// reverse and then forward: pop KKP → disable raw → leave alt screen
/// → spawn editor → enter alt screen → enable raw → re-push KKP →
/// force redraw via terminal.clear(). KKP-unsupporting terminals
/// get their existing behavior because the push/pop are silent
/// no-ops there.
fn open_prompt_in_editor<B: Backend>(
    terminal: &mut Terminal<B>,
    prompt_buffer: &mut String,
    prompt_cursor: &mut usize,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    use std::io::Write as _;

    // 1. Write current buffer to a uniquely-named tempfile. We don't
    //    use the `tempfile` crate to avoid a new dependency — pid +
    //    nanos suffices for uniqueness, and we remove the file
    //    ourselves on the way out (no RAII drop guard).
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("mu-prompt-{pid}-{nanos}.md"));
    {
        let mut f = std::fs::File::create(&path)?;
        f.write_all(prompt_buffer.as_bytes())?;
    }

    // 2. Suspend TUI control of the terminal.
    let mut stdout = io::stdout();
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    disable_raw_mode()?;
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;

    // 3. Spawn $EDITOR (default vi) synchronously. It inherits our
    //    stdin/stdout/stderr — the terminal is now its.
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&path).status();

    // 4. Reclaim the terminal. Order mirrors main()'s startup so the
    //    interactive surface comes back identical.
    enable_raw_mode()?;
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        // mu-1jq: $EDITOR may have set its own title; re-set ours.
        SetTitle(mu_terminal_title()),
    )?;
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    // The clear() forces the next draw() to repaint from a blank
    // canvas rather than trying to diff against whatever ratatui
    // thinks was on screen before the handoff. (ratatui 0.30: Backend::Error
    // is no longer fixed to io::Error, so route through io::Error::other.)
    terminal.clear().map_err(io::Error::other)?;

    // 5. Read back the edited content — but only on a successful
    //    exit. Non-zero status (e.g. `:cq` in vi) means "I changed
    //    my mind" — leave the prompt unchanged.
    let editor_ok = matches!(status, Ok(s) if s.success());
    if editor_ok {
        match std::fs::read_to_string(&path) {
            Ok(new_content) => {
                // Editors typically append a trailing newline on save.
                // Strip exactly one to keep the buffer clean; users
                // who actually want a trailing \n can add it back.
                let stripped = new_content
                    .strip_suffix('\n')
                    .unwrap_or(&new_content)
                    .to_string();
                *prompt_buffer = stripped;
                *prompt_cursor = prompt_buffer.chars().count();
            }
            Err(_) => {
                // Read failure is weird (we just wrote it) but not
                // catastrophic — leave the buffer alone.
            }
        }
    }

    // 6. Cleanup. Errors here are silent — the next run will
    //    overwrite whatever's left.
    let _ = std::fs::remove_file(&path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mu-2zs: short lines should not wrap (returned as a single row).
    #[test]
    fn wrap_short_line_returns_single_row() {
        let rows = wrap_body_line("hello world", 80);
        assert_eq!(rows, vec!["hello world".to_string()]);
    }

    /// mu-2zs: lines longer than width break at word boundaries.
    #[test]
    fn wrap_word_boundary() {
        let rows = wrap_body_line("alpha beta gamma delta", 12);
        assert_eq!(rows.len(), 2);
        // First row is full words up to width; second is the remainder.
        assert!(rows[0].chars().count() <= 12);
        assert!(rows[1].chars().count() <= 12);
        // No words are split across rows.
        let recombined = rows.join("").trim().to_string();
        assert_eq!(recombined, "alpha beta gamma delta");
    }

    /// mu-2zs: a single word longer than width is split mid-character.
    #[test]
    fn wrap_long_word_splits_mid_char() {
        let rows = wrap_body_line("abcdefghijklmnopqrstuv", 8);
        assert!(rows.len() >= 2);
        for r in &rows {
            assert!(r.chars().count() <= 8);
        }
        let recombined: String = rows.concat();
        assert_eq!(recombined, "abcdefghijklmnopqrstuv");
    }

    /// mu-gih: a `session.input_required` notification populates the
    /// per-session pending-approval queue with the right fields and
    /// emits a firehose entry naming the tool.
    #[test]
    fn input_required_notification_enqueues_pending_approval() {
        let mut sessions: Vec<SessionRow> = Vec::new();
        let mut firehose: Vec<String> = Vec::new();
        let mut latest_status = std::collections::HashMap::new();
        let mut pending = std::collections::HashMap::new();
        let params = json!({
            "session_id": "session-7",
            "request_id": "req-abc",
            "tool_call_id": "call-42",
            "tool_name": "bash",
            "arguments": { "command": "rm -rf /" },
            "summary": "bash command not on allowlist",
        });
        handle_notification(
            &mut sessions,
            &mut firehose,
            &mut latest_status,
            &mut pending,
            "session.input_required",
            &params,
        );
        let queue = pending.get("session-7").expect("queue created for sid");
        assert_eq!(queue.len(), 1);
        let item = &queue[0];
        assert_eq!(item.request_id, "req-abc");
        assert_eq!(item.tool_call_id, "call-42");
        assert_eq!(item.tool_name, "bash");
        assert_eq!(item.summary, "bash command not on allowlist");
        assert_eq!(
            item.arguments,
            json!({ "command": "rm -rf /" }),
            "arguments preserved verbatim for sanitize_arguments_preview at render time"
        );
        assert!(
            firehose.iter().any(|l| l.contains("input_required")
                && l.contains("bash")
                && l.contains("req-abc")),
            "firehose entry names tool + request_id, got: {firehose:?}"
        );
    }

    /// mu-gih (Stage 5 / N2 — Path B): a notification with an omitted
    /// required field (here `request_id`) fails the typed
    /// `InputRequiredEvent` deserialization at the protocol-crate
    /// boundary and falls through the *malformed* branch — NOT the
    /// later empty-string check, which is unreachable from omitted
    /// fields because the struct carries no `#[serde(default)]`.
    /// Either way the prompt is dropped (we cannot echo a request_id
    /// we never received) and the firehose names the failure.
    #[test]
    fn input_required_notification_with_omitted_required_field_is_dropped_as_malformed() {
        let mut sessions: Vec<SessionRow> = Vec::new();
        let mut firehose: Vec<String> = Vec::new();
        let mut latest_status = std::collections::HashMap::new();
        let mut pending = std::collections::HashMap::new();
        let params = json!({
            "session_id": "session-9",
            // request_id intentionally omitted — triggers the
            // typed-deserialization failure, not the empty-string
            // branch below.
            "tool_call_id": "call-1",
            "tool_name": "bash",
            "arguments": {},
            "summary": "",
        });
        handle_notification(
            &mut sessions,
            &mut firehose,
            &mut latest_status,
            &mut pending,
            "session.input_required",
            &params,
        );
        assert!(!pending.contains_key("session-9"));
        assert!(
            firehose.iter().any(|l| l.contains("malformed")),
            "firehose should report the typed-deserializer failure as malformed, got: {firehose:?}"
        );
    }

    /// mu-gih (Stage 5 / N2 — Path B): a notification whose required
    /// fields deserialize but are explicitly empty strings hits the
    /// later missing-fields branch. We could not synthesize a daemon
    /// reply for an empty request_id even if the struct happily parsed
    /// one, so drop and audit.
    #[test]
    fn input_required_notification_with_empty_required_field_is_dropped_as_missing() {
        let mut sessions: Vec<SessionRow> = Vec::new();
        let mut firehose: Vec<String> = Vec::new();
        let mut latest_status = std::collections::HashMap::new();
        let mut pending = std::collections::HashMap::new();
        let params = json!({
            "session_id": "session-9",
            "request_id": "",
            "tool_call_id": "call-1",
            "tool_name": "bash",
            "arguments": {},
            "summary": "",
        });
        handle_notification(
            &mut sessions,
            &mut firehose,
            &mut latest_status,
            &mut pending,
            "session.input_required",
            &params,
        );
        assert!(!pending.contains_key("session-9"));
        assert!(
            firehose.iter().any(|l| l.contains("missing")),
            "firehose should report the empty-required-field path as missing, got: {firehose:?}"
        );
    }

    /// mu-gih: arguments preview collapses newlines and truncates at
    /// max_chars (with a trailing ellipsis). Verifies the sanitization
    /// the modal applies before painting.
    #[test]
    fn sanitize_arguments_preview_collapses_newlines_and_truncates() {
        let args = json!({
            "command": "echo line1\nline2\nline3",
        });
        let preview = sanitize_arguments_preview(&args, 200);
        assert!(!preview.contains('\n'));
        assert!(preview.contains("line1 line2 line3"));
    }

    #[test]
    fn sanitize_arguments_preview_truncates_long_inputs() {
        let huge: String = "x".repeat(500);
        let args = json!({ "data": huge });
        let preview = sanitize_arguments_preview(&args, 200);
        // mu-gih (Stage 5 / M1): the budget is a hard cap. The
        // truncator reserves one char for the trailing ellipsis so
        // the final string is at most `max_chars`, not max_chars + 1.
        assert!(
            preview.chars().count() <= 200,
            "preview exceeds budget: got {} chars",
            preview.chars().count()
        );
        assert!(preview.ends_with('…'));
    }

    /// mu-gih (Stage 3 / I2): pin the outgoing RPC payload — both
    /// the method constant AND the JSON field shape. This regression
    /// test catches drift on any of: `session_id`, `request_id`,
    /// `decision` enum casing ("approve" / "deny", NOT "Approve"),
    /// and absence of the stale `approved` boolean from the bead's
    /// prose. Bonus: deserialize the serialized payload back into
    /// `RespondToInputRequiredRequest` and assert field-equality.
    #[test]
    fn respond_to_input_required_payload_shape_approve() {
        let item = PendingApproval {
            request_id: "req-abc".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({}),
            summary: String::new(),
        };
        let req = build_respond_payload("session-42", &item, true);
        let payload = serde_json::to_value(&req).expect("payload serializes");

        // Method constant pinned (unchanged from Stage 1).
        assert_eq!(
            RespondToInputRequiredRequest::METHOD,
            "session.respond_to_input_required",
        );
        // Required keys present.
        assert!(payload.get("session_id").is_some());
        assert!(payload.get("request_id").is_some());
        assert!(payload.get("decision").is_some());
        assert_eq!(payload["session_id"], "session-42");
        assert_eq!(payload["request_id"], "req-abc");
        assert_eq!(payload["decision"], "approve");
        // Stale boolean shape MUST NOT regress.
        assert!(
            payload.get("approved").is_none(),
            "`approved: bool` is the bead's stale prose shape — the wire format is decision: approve/deny"
        );
        // Roundtrip back to the typed struct.
        let decoded: RespondToInputRequiredRequest =
            serde_json::from_value(payload).expect("roundtrip");
        assert_eq!(decoded.session_id, "session-42");
        assert_eq!(decoded.request_id, "req-abc");
        assert_eq!(decoded.decision, ApprovalDecision::Approve);
    }

    /// mu-gih (Stage 3 / I2): same wire-shape pin for the deny path,
    /// since "approve" and "deny" go through separate `serde` enum
    /// arms and could regress independently.
    #[test]
    fn respond_to_input_required_payload_shape_deny() {
        let item = PendingApproval {
            request_id: "req-xyz".into(),
            tool_call_id: "call-2".into(),
            tool_name: "edit".into(),
            arguments: json!({}),
            summary: String::new(),
        };
        let req = build_respond_payload("session-7", &item, false);
        let payload = serde_json::to_value(&req).expect("payload serializes");
        assert_eq!(payload["decision"], "deny");
        assert!(payload.get("approved").is_none());
        let decoded: RespondToInputRequiredRequest =
            serde_json::from_value(payload).expect("roundtrip");
        assert_eq!(decoded.decision, ApprovalDecision::Deny);
    }

    /// mu-gih (Stage 3 / B1): on `Ok(accepted=true)` the prompt is
    /// popped and the firehose records the outcome. Verifies the
    /// happy path of `dispatch_decision` keeps the audit shape
    /// expected by the bead (label + tool name).
    #[test]
    fn dispatch_decision_pops_on_ok_accepted() {
        let mut pending: std::collections::HashMap<
            String,
            std::collections::VecDeque<PendingApproval>,
        > = std::collections::HashMap::new();
        let sid = "session-1".to_string();
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({"command": "ls"}),
            summary: String::new(),
        });
        pending.insert(sid.clone(), q);
        let mut firehose: Vec<String> = Vec::new();
        dispatch_decision(
            &mut pending,
            &mut firehose,
            &sid,
            true,
            |_method, _payload| Ok(json!({ "accepted": true })),
        );
        assert!(
            !pending.contains_key(&sid),
            "queue cleared after the only entry is popped"
        );
        let entry = firehose
            .iter()
            .find(|l| l.contains("approved") && l.contains("accepted=true"))
            .expect("firehose has approve+accepted=true entry");
        assert!(entry.contains("bash"), "audit names tool: {entry:?}");
        assert!(entry.contains("req-1"), "audit names request_id: {entry:?}");
    }

    /// mu-gih (Stage 3 / B1): on `Err(_)` from the RPC, the prompt
    /// stays in the queue and the firehose records the failed
    /// attempt. THIS is the load-bearing regression test — pre-Stage
    /// 3, the entry was popped before the RPC was even sent, so a
    /// transient daemon error would permanently lose the prompt.
    #[test]
    fn dispatch_decision_keeps_queue_on_rpc_error() {
        let mut pending: std::collections::HashMap<
            String,
            std::collections::VecDeque<PendingApproval>,
        > = std::collections::HashMap::new();
        let sid = "session-1".to_string();
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({"command": "ls"}),
            summary: String::new(),
        });
        pending.insert(sid.clone(), q);
        let mut firehose: Vec<String> = Vec::new();
        dispatch_decision(
            &mut pending,
            &mut firehose,
            &sid,
            true,
            |_method, _payload| Err(anyhow::anyhow!("daemon disconnected")),
        );
        assert_eq!(
            pending.get(&sid).map(|q| q.len()).unwrap_or(0),
            1,
            "prompt MUST stay queued on RPC error"
        );
        let entry = firehose
            .iter()
            .find(|l| l.contains("rpc failed"))
            .expect("firehose has rpc-failed entry");
        assert!(
            entry.contains("approve"),
            "audit names attempted decision: {entry:?}"
        );
        assert!(entry.contains("bash"), "audit names tool: {entry:?}");
        assert!(
            entry.contains("daemon disconnected"),
            "audit names the underlying error: {entry:?}"
        );
    }

    /// mu-gih (Stage 3 / B1): on `Ok(accepted=false)` the daemon has
    /// told us the request_id is no longer valid (timeout / already
    /// answered / unknown). The prompt is terminal from the daemon's
    /// perspective — pop it and let the firehose surface the
    /// rejection so the user knows the click landed too late.
    #[test]
    fn dispatch_decision_pops_on_ok_accepted_false() {
        let mut pending: std::collections::HashMap<
            String,
            std::collections::VecDeque<PendingApproval>,
        > = std::collections::HashMap::new();
        let sid = "session-1".to_string();
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-stale".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({}),
            summary: String::new(),
        });
        pending.insert(sid.clone(), q);
        let mut firehose: Vec<String> = Vec::new();
        dispatch_decision(&mut pending, &mut firehose, &sid, false, |_m, _p| {
            Ok(json!({ "accepted": false }))
        });
        assert!(
            !pending.contains_key(&sid),
            "prompt dropped — daemon has no record of it"
        );
        assert!(firehose
            .iter()
            .any(|l| l.contains("denied") && l.contains("accepted=false")));
    }

    /// mu-gih (Stage 5 / N1): the load-bearing parse-error regression
    /// test. A successful RPC whose response body does NOT deserialize
    /// as `RespondToInputRequiredResponse` is a protocol/shape error,
    /// not a daemon-acknowledged invalidation. The daemon side is
    /// still waiting because we cannot prove it relayed the decision,
    /// so the prompt MUST stay queued and the firehose MUST surface
    /// the decode failure. Pre-Stage-5 this was `unwrap_or(false)` —
    /// the prompt was popped under a phantom accepted=false, dropping
    /// the prompt permanently and recreating the B1 data-loss class
    /// under a different error class.
    #[test]
    fn dispatch_decision_keeps_queue_on_malformed_ok_response() {
        let mut pending: std::collections::HashMap<
            String,
            std::collections::VecDeque<PendingApproval>,
        > = std::collections::HashMap::new();
        let sid = "session-1".to_string();
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({"command": "ls"}),
            summary: String::new(),
        });
        pending.insert(sid.clone(), q);
        let mut firehose: Vec<String> = Vec::new();
        // Returned value parses as JSON but NOT as
        // `RespondToInputRequiredResponse` (wrong type on `accepted`).
        dispatch_decision(
            &mut pending,
            &mut firehose,
            &sid,
            true,
            |_method, _payload| Ok(json!({ "accepted": "not_a_bool" })),
        );
        assert_eq!(
            pending.get(&sid).map(|q| q.len()).unwrap_or(0),
            1,
            "prompt MUST stay queued when the response shape is unparseable"
        );
        let entry = firehose
            .iter()
            .find(|l| l.contains("response decode failed"))
            .expect("firehose has response-decode-failed entry");
        assert!(
            entry.contains("approve"),
            "audit names attempted decision: {entry:?}"
        );
        assert!(entry.contains("bash"), "audit names tool: {entry:?}");
        assert!(entry.contains("req-1"), "audit names request_id: {entry:?}");
    }

    /// mu-gih (Stage 5 / N1, companion): a structurally-empty / wrong
    /// JSON shape (e.g. a bare number) also triggers the
    /// response-decode-failed path. Belt-and-suspenders on the type
    /// of malformations the regression covers.
    #[test]
    fn dispatch_decision_keeps_queue_on_non_object_ok_response() {
        let mut pending: std::collections::HashMap<
            String,
            std::collections::VecDeque<PendingApproval>,
        > = std::collections::HashMap::new();
        let sid = "session-2".to_string();
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-2".into(),
            tool_call_id: "call-2".into(),
            tool_name: "edit".into(),
            arguments: json!({}),
            summary: String::new(),
        });
        pending.insert(sid.clone(), q);
        let mut firehose: Vec<String> = Vec::new();
        dispatch_decision(&mut pending, &mut firehose, &sid, false, |_m, _p| {
            Ok(json!(42))
        });
        assert_eq!(
            pending.get(&sid).map(|q| q.len()).unwrap_or(0),
            1,
            "prompt MUST stay queued on non-object Ok response"
        );
        assert!(firehose
            .iter()
            .any(|l| l.contains("response decode failed") && l.contains("deny")));
    }

    /// mu-gih (Stage 3 / I3): a duplicate notification with the same
    /// (session_id, request_id) refreshes the existing entry instead
    /// of enqueuing a phantom second prompt. The refresh updates
    /// arguments + summary in case the daemon resent with updated
    /// fields after a reconnect.
    #[test]
    fn input_required_duplicate_refreshes_existing_entry() {
        let mut sessions: Vec<SessionRow> = Vec::new();
        let mut firehose: Vec<String> = Vec::new();
        let mut latest_status = std::collections::HashMap::new();
        let mut pending = std::collections::HashMap::new();
        let first = json!({
            "session_id": "session-1",
            "request_id": "req-dup",
            "tool_call_id": "call-1",
            "tool_name": "bash",
            "arguments": { "command": "ls" },
            "summary": "first",
        });
        let second = json!({
            "session_id": "session-1",
            "request_id": "req-dup",
            "tool_call_id": "call-1",
            "tool_name": "bash",
            "arguments": { "command": "ls -la" },
            "summary": "second",
        });
        handle_notification(
            &mut sessions,
            &mut firehose,
            &mut latest_status,
            &mut pending,
            "session.input_required",
            &first,
        );
        handle_notification(
            &mut sessions,
            &mut firehose,
            &mut latest_status,
            &mut pending,
            "session.input_required",
            &second,
        );
        let queue = pending.get("session-1").expect("queue exists");
        assert_eq!(queue.len(), 1, "duplicate did NOT enqueue a second prompt");
        let item = queue.front().expect("head present");
        assert_eq!(item.summary, "second", "duplicate refreshed summary");
        assert_eq!(
            item.arguments,
            json!({ "command": "ls -la" }),
            "duplicate refreshed arguments"
        );
        assert!(
            firehose.iter().any(|l| l.contains("duplicate refreshed")),
            "firehose surfaces the dedupe: {firehose:?}"
        );
    }

    /// mu-gih (Stage 3 / I3): two distinct request_ids for the same
    /// session DO both land in the queue. This is the negative
    /// control for the dedupe test above — proves the dedupe is
    /// keyed on request_id, not on session_id alone.
    #[test]
    fn input_required_distinct_request_ids_both_enqueue() {
        let mut sessions: Vec<SessionRow> = Vec::new();
        let mut firehose: Vec<String> = Vec::new();
        let mut latest_status = std::collections::HashMap::new();
        let mut pending = std::collections::HashMap::new();
        for rid in ["req-a", "req-b"] {
            let params = json!({
                "session_id": "session-2",
                "request_id": rid,
                "tool_call_id": format!("call-{rid}"),
                "tool_name": "bash",
                "arguments": {},
                "summary": "",
            });
            handle_notification(
                &mut sessions,
                &mut firehose,
                &mut latest_status,
                &mut pending,
                "session.input_required",
                &params,
            );
        }
        let queue = pending.get("session-2").expect("queue exists");
        assert_eq!(queue.len(), 2, "distinct request_ids enqueue separately");
        assert_eq!(queue[0].request_id, "req-a");
        assert_eq!(queue[1].request_id, "req-b");
    }

    /// mu-gih (Stage 3 / I7): a notification with a malformed
    /// `arguments` field (wrong JSON shape, not just missing
    /// scalars) is rejected at the typed-deserialization layer with
    /// a malformed-firehose entry — instead of silently degrading to
    /// `?` / null at render time. Verifies the typed
    /// `InputRequiredEvent` deserializer is on the hot path.
    #[test]
    fn input_required_typed_deserializer_rejects_wrong_arguments_shape() {
        let mut sessions: Vec<SessionRow> = Vec::new();
        let mut firehose: Vec<String> = Vec::new();
        let mut latest_status = std::collections::HashMap::new();
        let mut pending = std::collections::HashMap::new();
        // `arguments` is missing entirely → typed deserialize fails.
        let params = json!({
            "session_id": "session-3",
            "request_id": "req-bad",
            "tool_call_id": "call-1",
            "tool_name": "bash",
            "summary": "",
        });
        handle_notification(
            &mut sessions,
            &mut firehose,
            &mut latest_status,
            &mut pending,
            "session.input_required",
            &params,
        );
        assert!(!pending.contains_key("session-3"));
        assert!(
            firehose.iter().any(|l| l.contains("malformed")),
            "firehose surfaces typed-deserialize failure: {firehose:?}"
        );
    }

    /// mu-gih (Stage 3 / minor + Stage 5 / M1): title truncation
    /// appends an ellipsis past the budget. The final string fits
    /// inside `max_chars` (the ellipsis displaces one source char),
    /// so the modal title is bounded on narrow terminals.
    #[test]
    fn truncate_for_title_appends_ellipsis_past_budget() {
        let s = "a".repeat(50);
        let out = truncate_for_title(&s, 10);
        assert!(
            out.chars().count() <= 10,
            "truncated title exceeds budget: got {} chars",
            out.chars().count()
        );
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_for_title_passthrough_under_budget() {
        let out = truncate_for_title("short", 10);
        assert_eq!(out, "short");
    }

    /// mu-gih (Stage 3 / I4): pending-approval queues for session_ids
    /// not in the live set are dropped, with a firehose entry naming
    /// the count. Queues for live session_ids stay untouched.
    #[test]
    fn prune_pending_approvals_drops_stale_sessions() {
        let mut pending: std::collections::HashMap<
            String,
            std::collections::VecDeque<PendingApproval>,
        > = std::collections::HashMap::new();
        let mut q_alive = std::collections::VecDeque::new();
        q_alive.push_back(PendingApproval {
            request_id: "req-a".into(),
            tool_call_id: "call-a".into(),
            tool_name: "bash".into(),
            arguments: json!({}),
            summary: String::new(),
        });
        let mut q_dead = std::collections::VecDeque::new();
        q_dead.push_back(PendingApproval {
            request_id: "req-b".into(),
            tool_call_id: "call-b".into(),
            tool_name: "edit".into(),
            arguments: json!({}),
            summary: String::new(),
        });
        pending.insert("session-alive".into(), q_alive);
        pending.insert("session-dead".into(), q_dead);
        let mut live = std::collections::HashSet::new();
        live.insert("session-alive".to_string());
        let mut firehose: Vec<String> = Vec::new();
        prune_pending_approvals_to_live(&mut pending, &live, &mut firehose);
        assert!(
            pending.contains_key("session-alive"),
            "live queue preserved"
        );
        assert!(!pending.contains_key("session-dead"), "stale queue dropped");
        assert!(
            firehose
                .iter()
                .any(|l| l.contains("session-dead") && l.contains("no longer present")),
            "firehose surfaces the drop: {firehose:?}"
        );
    }

    /// mu-gih (Stage 3 / I4): an empty queue for a stale session is
    /// dropped silently — no firehose noise for the no-op cleanup.
    #[test]
    fn prune_pending_approvals_silent_on_empty_stale_queue() {
        let mut pending: std::collections::HashMap<
            String,
            std::collections::VecDeque<PendingApproval>,
        > = std::collections::HashMap::new();
        pending.insert("session-dead".into(), std::collections::VecDeque::new());
        let live = std::collections::HashSet::new();
        let mut firehose: Vec<String> = Vec::new();
        prune_pending_approvals_to_live(&mut pending, &live, &mut firehose);
        assert!(!pending.contains_key("session-dead"));
        assert!(
            firehose.is_empty(),
            "no firehose entry for an empty stale queue"
        );
    }

    /// mu-gih (Stage 3 / B2): A and D are routed to the approval
    /// modal even when the user is in `InputMode::Command` (or any
    /// non-Normal mode). Pre-Stage-3, the modal check lived inside
    /// `on_key_normal` and these keys were appended to the command
    /// buffer instead, leaving the agent loop stranded.
    #[test]
    fn on_key_approval_modal_intercepts_in_command_mode() {
        let mut app = App::new(None, ("anthropic".into(), "haiku".into()));
        // Inject a live session row so `selected_sid()` resolves.
        app.sessions = vec![SessionRow {
            short_id: "sid".into(),
            title: "t".into(),
            status: SessionStatus::Running,
            model: "m".into(),
            cost_usd: 0.0,
            tokens_kilo: 0,
            phase: "".into(),
            session_id: Some("session-1".into()),
        }];
        app.selected_session.select(Some(0));
        // Queue a pending approval for that session.
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({}),
            summary: String::new(),
        });
        app.pending_approvals.insert("session-1".into(), q);
        // Put the app into Command mode. Pre-Stage 3, 'A' would land
        // in `command_buffer`. Post-Stage 3, the approval modal
        // intercepts before the input_mode dispatch runs.
        app.input_mode = InputMode::Command;
        app.command_buffer.clear();
        app.firehose.clear();
        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(
            app.command_buffer.is_empty(),
            "approval modal intercepted; 'a' did NOT land in command_buffer (got: {:?})",
            app.command_buffer
        );
        // The RPC fails (no daemon), so the prompt stays queued and
        // the firehose carries the "rpc failed" attempt entry. That
        // proves the approval path ran, not the command path.
        assert_eq!(
            app.pending_approvals.get("session-1").map(|q| q.len()),
            Some(1),
            "prompt stayed queued because RPC failed (no daemon in test)"
        );
        assert!(
            app.firehose
                .iter()
                .any(|l| l.contains("rpc failed") && l.contains("approve")),
            "firehose has the failed-approve audit entry: {:?}",
            app.firehose
        );
    }

    /// mu-gih (Stage 3 / B2): same as above, but for D in SendPrompt
    /// mode. SendPrompt is the most user-visible non-Normal mode —
    /// the user is typing a prompt for the selected session, and an
    /// approval prompt arriving mid-typing should NOT land 'd' in
    /// their prompt buffer.
    #[test]
    fn on_key_approval_modal_intercepts_in_send_prompt_mode() {
        let mut app = App::new(None, ("anthropic".into(), "haiku".into()));
        app.sessions = vec![SessionRow {
            short_id: "sid".into(),
            title: "t".into(),
            status: SessionStatus::Running,
            model: "m".into(),
            cost_usd: 0.0,
            tokens_kilo: 0,
            phase: "".into(),
            session_id: Some("session-1".into()),
        }];
        app.selected_session.select(Some(0));
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({}),
            summary: String::new(),
        });
        app.pending_approvals.insert("session-1".into(), q);
        app.input_mode = InputMode::SendPrompt;
        app.prompt_buffer.clear();
        app.firehose.clear();
        app.on_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            app.prompt_buffer.is_empty(),
            "approval modal intercepted; 'd' did NOT land in prompt_buffer (got: {:?})",
            app.prompt_buffer
        );
        assert!(
            app.firehose
                .iter()
                .any(|l| l.contains("rpc failed") && l.contains("deny")),
            "firehose has the failed-deny audit entry: {:?}",
            app.firehose
        );
    }

    /// mu-gih (Stage 3 / I5): F3 session picker has higher priority
    /// than the approval modal. Pressing 'a' while the picker is open
    /// does NOT trigger approval — it falls through to the picker's
    /// key handler (which ignores it). The prompt stays queued and
    /// resurfaces as soon as the picker closes.
    #[test]
    fn on_key_session_picker_suppresses_approval_modal() {
        let mut app = App::new(None, ("anthropic".into(), "haiku".into()));
        app.sessions = vec![SessionRow {
            short_id: "sid".into(),
            title: "t".into(),
            status: SessionStatus::Running,
            model: "m".into(),
            cost_usd: 0.0,
            tokens_kilo: 0,
            phase: "".into(),
            session_id: Some("session-1".into()),
        }];
        app.selected_session.select(Some(0));
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({}),
            summary: String::new(),
        });
        app.pending_approvals.insert("session-1".into(), q);
        app.session_picker_open = true;
        app.firehose.clear();
        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            app.pending_approvals.get("session-1").map(|q| q.len()),
            Some(1),
            "approval modal did NOT fire while picker is open"
        );
        assert!(
            !app.firehose.iter().any(|l| l.contains("rpc failed")),
            "no approval RPC was attempted (picker had key priority): {:?}",
            app.firehose
        );
    }

    /// mu-gih (Stage 3 / I6): pressing E while a pending approval is
    /// active eats the key with a firehose explanation, instead of
    /// silently doing nothing (which would let users wonder why E
    /// looks like an action button).
    #[test]
    fn on_key_e_in_modal_emits_unavailable_message() {
        let mut app = App::new(None, ("anthropic".into(), "haiku".into()));
        app.sessions = vec![SessionRow {
            short_id: "sid".into(),
            title: "t".into(),
            status: SessionStatus::Running,
            model: "m".into(),
            cost_usd: 0.0,
            tokens_kilo: 0,
            phase: "".into(),
            session_id: Some("session-1".into()),
        }];
        app.selected_session.select(Some(0));
        let mut q = std::collections::VecDeque::new();
        q.push_back(PendingApproval {
            request_id: "req-1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            arguments: json!({}),
            summary: String::new(),
        });
        app.pending_approvals.insert("session-1".into(), q);
        app.firehose.clear();
        app.on_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert!(
            app.firehose
                .iter()
                .any(|l| l.contains("[approval]") && l.to_lowercase().contains("not implemented")),
            "firehose explains Edit is unavailable in v1: {:?}",
            app.firehose
        );
        // Queue is unchanged — E neither approves nor denies.
        assert_eq!(
            app.pending_approvals.get("session-1").map(|q| q.len()),
            Some(1)
        );
    }

    /// mu-2zs: width 0 (degenerate) shouldn't panic; the caller is
    /// expected to disable wrapping via `None` in that case, but this
    /// guards against the regression.
    #[test]
    fn wrap_zero_width_returns_input_unchanged() {
        // wrap_body_line is only invoked when wrap_width > 0; this
        // confirms the loop's invariant doesn't panic if called with 0.
        let rows = wrap_body_line("x", 0);
        // With width 0, no chars fit; the function returns an empty
        // string row plus the single char on its own row. Acceptable
        // pathological behavior.
        assert!(!rows.is_empty());
        // No row exceeds whatever width we'd interpret; the key
        // assertion is "no panic."
        let recombined: String = rows.concat();
        assert_eq!(recombined, "x");
    }

    #[test]
    fn anchor_scroll_offset_bumps_on_grow_when_scrolled_up() {
        // mu-sod: with offset > 0 and total growing by N, the offset
        // should bump by N so the visible window stays on the same
        // absolute content instead of drifting downward by N.
        let prev_total = 50;
        let current_total = 55; // 5 new lines appended
        let offset = 10;
        let anchored = anchor_scroll_offset(prev_total, current_total, offset);
        assert_eq!(anchored, 15, "offset must bump by the delta");
    }

    #[test]
    fn anchor_scroll_offset_no_bump_at_offset_zero() {
        // mu-sod: offset 0 = auto-follow. Even when total grows,
        // offset stays at 0 so the render pins to the new bottom.
        let anchored = anchor_scroll_offset(50, 100, 0);
        assert_eq!(anchored, 0, "auto-follow must not be bumped");
    }

    #[test]
    fn anchor_scroll_offset_no_bump_on_shrink() {
        // mu-sod: when content shrinks (rare, but possible if the
        // session-event buffer is rebuilt smaller), do not bump.
        // The downstream max_top clamp handles bringing offset back
        // into range without us double-adjusting here.
        let anchored = anchor_scroll_offset(100, 50, 10);
        assert_eq!(anchored, 10, "shrinkage must leave offset alone");
    }

    #[test]
    fn anchor_scroll_offset_saturating_at_u16_max() {
        // mu-sod: saturating-add caps at u16::MAX, matching the Home
        // key handler that uses u16::MAX to scroll to top. A massive
        // append should not silently wrap to a tiny offset.
        let anchored = anchor_scroll_offset(0, usize::from(u16::MAX) + 100, u16::MAX - 5);
        assert_eq!(anchored, u16::MAX, "saturating-add must cap at u16::MAX");
    }
}
