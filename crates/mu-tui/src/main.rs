//! mu-tui — terminal UI for `mu serve`.
//!
//! Status: **scaffold (Tier 0, mock data)**. Renders the Command Center
//! view from `mu-ui-mockups-2026-05-10.md` with hard-coded session data,
//! supports F1–F9 mode-switching (most modes are placeholders), `q` to
//! quit, and a `:` command-palette stub. The point of the scaffold is to
//! prove the layout/keybind shape so the wire-integration can follow in
//! a focused next change rather than as a sprawling first commit.
//!
//! Next slices (not in this scaffold):
//!   - JSON-RPC over stdio to a spawned `mu serve` (port the Python
//!     mu_client.py defensive layer to Rust)
//!   - Subscribe to `session.text_delta` / `session.tool_call_*` etc.
//!     and render the firehose live
//!   - Render `session.provider_status` (mu-035) into the per-session
//!     "phase:" line — the mockup already assumes this signal
//!   - Implement Session Tree, Session Detail, Context Inspector,
//!     Usage Dashboard, Router, Tools views
//!   - Command palette with `:` prefix, parser, and routing

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
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
    },
    Frame, Terminal,
};
use serde_json::json;

use crate::mu_client::{Message as MuMessage, MuClient};

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
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Self::Idle => Style::default().fg(Color::Yellow),
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
    // bottom. 0 = pinned to bottom (auto-follow on new content);
    // >0 = user scrolled up. Reset to 0 with End / `0`.
    transcript_scroll_offset: u16,
    // F8 events explorer scroll state. Same semantics as transcript
    // scroll: 0 = pinned to bottom of firehose, >0 = scrolled up.
    events_scroll_offset: u16,
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
            streaming_text: std::collections::HashMap::new(),
            transcript_events_by_sid: std::collections::HashMap::new(),
            transcript_scroll_offset: 0,
            events_scroll_offset: 0,
            events_filter: String::new(),
            latest_usage_history: None,
            latest_usage_history_at: None,
            mu,
            default_provider,
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

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.input_mode {
            InputMode::Command => self.on_key_command(code),
            InputMode::SendPrompt => self.on_key_send_prompt(code, mods),
            InputMode::Normal => self.on_key_normal(code, mods),
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
        // F3 picker is modal — when open, eat all keys here.
        // j/k move selection (which also live-previews the transcript
        // pane underneath via the existing selected_session state).
        // Enter commits; Esc / F3-again cancels.
        if self.session_picker_open {
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
            return;
        }
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
            (KeyCode::F(1), _) => self.mode = ViewMode::CommandCenter,
            (KeyCode::F(2), _) => self.mode = ViewMode::SessionTree,
            (KeyCode::F(3), _) => {
                // First press: enter SessionDetail mode.
                // Subsequent press while already there: pop the
                // session picker (modal overlay). Third press / Esc
                // closes the picker.
                if matches!(self.mode, ViewMode::SessionDetail) {
                    self.open_session_picker();
                } else {
                    self.mode = ViewMode::SessionDetail;
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
            (KeyCode::F(4), _) => self.mode = ViewMode::Context,
            (KeyCode::F(5), _) => {
                self.mode = ViewMode::Usage;
                // Eager refresh on mode entry — the user shouldn't
                // have to wait for the next tick (~250ms) to see
                // populated data after pressing F5.
                self.refresh_usage_history();
            }
            (KeyCode::F(6), _) => self.mode = ViewMode::Tools,
            (KeyCode::F(7), _) => self.mode = ViewMode::Router,
            (KeyCode::F(8), _) => {
                self.mode = ViewMode::Events;
                self.events_scroll_offset = 0; // pinned to bottom on entry
            }
            (KeyCode::F(9), _) => self.mode = ViewMode::Mailbox,
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
    Some(SessionRow {
        short_id: sid.chars().take(12).collect(),
        title: format!("{provider_kind} / {model}"),
        status,
        model: format!("{provider_kind} / {model}"),
        cost_usd: 0.0, // populated when usage→cost mapping lands
        tokens_kilo,
        phase,
        session_id: Some(sid),
    })
}

fn handle_notification(
    sessions: &mut [SessionRow],
    firehose: &mut Vec<String>,
    latest_status: &mut std::collections::HashMap<String, ProviderStatusSnapshot>,
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
            let rid = params
                .get("request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            firehose.push(format!(
                "[{sid}] !! input_required ({rid}) — TUI approval flow TBD"
            ));
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
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let (used, budget) = app.cost_budget;
    let dot_style = if app.connected() {
        Style::default().fg(Color::Green)
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
                Color::Yellow
            } else {
                Color::Green
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
            Span::styled("✓", Style::default().fg(Color::Green))
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
            // provider/model line we had before.
            let detail = Line::from(vec![
                Span::raw("    "),
                Span::styled(s.phase.clone(), Style::default().fg(Color::Cyan)),
                Span::raw(format!("   ${:.2}  ", s.cost_usd)),
                Span::raw(format!("{}k tok", s.tokens_kilo)),
            ]);
            ListItem::new(vec![header, detail])
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
                    "awaiting_first_token" => ("● awaiting first token  ", Color::Yellow),
                    "thinking" => ("● thinking  ", Color::Yellow),
                    "tool_executing" => ("● tool executing  ", Color::Magenta),
                    "awaiting_tool_result" => ("● awaiting tool result  ", Color::Magenta),
                    "streaming" | "idle" => return None,
                    _ => ("● working  ", Color::Cyan),
                };
                let suffix = snap
                    .tool_call_id
                    .as_deref()
                    .map(|cid| format!(" (call {cid})"))
                    .unwrap_or_default();
                return Some(Line::from(vec![
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
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:.1}s", elapsed.as_secs_f32()),
                        Style::default().fg(Color::Yellow),
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
            SessionStatus::Running => Style::default().fg(Color::Green),
            SessionStatus::Done => Style::default().fg(Color::DarkGray),
            SessionStatus::Idle => Style::default().fg(Color::Yellow),
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
        render_transcript_lines(events, streaming_partial)
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
    let max_top = total_lines.saturating_sub(inner_height);
    // Same clamp as F8: don't let the stored offset exceed max
    // scrollable range. Without this, the title shows "scrolled up 130"
    // while the view is pinned to the top, and the user has to scroll
    // down past the phantom offset before motion resumes.
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
                push_block(&mut lines, "you", Color::Cyan, content);
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
                        );
                    }
                } else {
                    push_block(&mut lines, "assistant", Color::White, &text);
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
                lines.push(Line::from(Span::styled(
                    format!("─── done · {stop} · {elapsed} ───"),
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
            }
            "error" => {
                let msg = payload.get("message").and_then(|v| v.as_str()).unwrap_or("");
                push_block(&mut lines, "ERROR", Color::Red, msg);
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
            push_block(&mut lines, "assistant (streaming…)", Color::Yellow, partial);
        }
    }

    lines
}

fn push_block(out: &mut Vec<Line<'static>>, label: &str, color: Color, body: &str) {
    out.push(Line::from(Span::styled(
        format!("┌─ {label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    for raw_line in body.lines() {
        // Wrap is handled by the outer Paragraph widget; just indent.
        out.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(color)),
            Span::raw(raw_line.to_string()),
        ]));
    }
    out.push(Line::from(Span::styled(
        "└─".to_string(),
        Style::default().fg(color),
    )));
    out.push(Line::from(""));
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
    let max_top = total.saturating_sub(inner_height);
    // Clamp stored offset to the actual maximum here, so the title
    // shows a value the view can actually reflect and subsequent
    // PageDown presses don't have to "burn off" phantom offset
    // before they move anything.
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
        .style(Style::default().fg(Color::Yellow))
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
        InputMode::Command => Style::default().fg(Color::Black).bg(Color::Yellow),
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

    /// Default provider kind (used for `n` → create_session).
    #[arg(long, default_value = "anthropic_api")]
    provider: String,

    /// Default provider model (used for `n` → create_session).
    #[arg(long, default_value = "claude-haiku-4-5-20251001")]
    model: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Try to spawn a live mu serve; if it fails, fall through to
    // mock-data scaffold mode (so the TUI is still demoable).
    let mu = match &cli.mu_binary {
        Some(bin) => {
            let cwd = cli
                .mu_cwd
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let tools_refs: Vec<&str> = cli.tools.iter().map(String::as_str).collect();
            match MuClient::spawn(&bin.to_string_lossy(), &tools_refs, &cwd, cli.bash_yolo) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("warning: failed to spawn mu serve: {e}; running in mock-data mode");
                    None
                }
            }
        }
        None => None,
    };

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        // mu-1jq: set terminal title to μ - <username>. Honored by zellij
        // (per-pane), kitty, tmux, foot, alacritty, etc. via OSC 0.
        SetTitle(mu_terminal_title()),
    )?;
    // mu-wd2: opt into the Kitty Keyboard Protocol so terminals that
    // support it (Kitty, Foot, WezTerm, modern Konsole, alacritty
    // with the right config) forward modifiers on keys that the
    // legacy xterm encoding drops — most notably Shift-Enter,
    // Ctrl-Enter, and friends. The escape sequence is a no-op on
    // terminals that don't support it (they ignore the CSI), so we
    // don't gate on a supports-check; the Pop on shutdown is
    // similarly benign.
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run(&mut terminal, mu, (cli.provider, cli.model));

    // Cleanup
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        // mu-1jq: empty title resets the terminal/pane title to default.
        SetTitle(""),
    )?;
    terminal.show_cursor()?;

    res
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

fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    mu: Option<MuClient>,
    default_provider: (String, String),
) -> Result<()> {
    let mut app = App::new(mu, default_provider);
    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

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
            break;
        }
    }
    Ok(())
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
) -> io::Result<()> {
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
    // thinks was on screen before the handoff.
    terminal.clear()?;

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
