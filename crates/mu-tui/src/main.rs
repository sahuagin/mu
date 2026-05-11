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
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use serde_json::json;

use crate::mu_client::{Message as MuMessage, MuClient};

// ── Model ───────────────────────────────────────────────────────────

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
            Self::Running => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
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
                                    let delta = params
                                        .get("delta")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
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
        if matches!(self.mode, ViewMode::SessionDetail)
            && self.poll_tick_counter % 2 == 0
        {
            self.refresh_transcript_for_selection();
        }
    }

    fn refresh_transcript_for_selection(&mut self) {
        let Some(idx) = self.selected_session.selected() else { return };
        let Some(sid) = self.sessions.get(idx).and_then(|r| r.session_id.clone())
        else {
            return;
        };
        let Some(mu) = self.mu.as_mut() else { return };
        // No after_event_id — pull the full first page (limit=200).
        // For very long sessions, future work adds pagination/scroll;
        // for daily-driver-today, 200 events covers ~tens of asks.
        let res = mu.request(
            "session.events",
            json!({ "session_id": sid, "limit": 500 }),
        );
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
                .map(|arr| {
                    arr.iter().filter_map(session_row_from_info_value).collect()
                })
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
                    let synthetic_ms = snap.elapsed_ms
                        + snap.received_at.elapsed().as_millis() as u64;
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

    fn refresh_daemon_stats(&mut self) {
        let Some(mu) = self.mu.as_mut() else { return };
        match mu.request("daemon.stats", json!({})) {
            Ok(v) => {
                self.daemon_id = v
                    .get("daemon_id")
                    .and_then(|s| s.as_str())
                    .map(String::from);
                self.daemon_uptime_ms =
                    v.get("uptime_ms").and_then(|x| x.as_u64()).unwrap_or(0);
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
        let (kind, model) = self.default_provider.clone();
        let res = mu.request(
            "create_session",
            json!({ "provider": { "kind": kind, "model": model } }),
        );
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

    fn send_prompt(&mut self) {
        let prompt = std::mem::take(&mut self.prompt_buffer);
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
                let suffix = if prompt.chars().count() > 60 { "…" } else { "" };
                self.firehose
                    .push(format!("→ {sid}: {preview:?}{suffix}"));
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
        let debug = format!("[key] code={code:?} mods={mods:?}");
        match (code, mods) {
            (KeyCode::Esc, _) => {
                self.input_mode = InputMode::Normal;
                self.prompt_buffer.clear();
            }
            // Enter submits (the chat-TUI convention). Alt-Enter or
            // Ctrl-J inserts a newline for multi-line prompts.
            // Ctrl-Enter ALSO submits when the terminal happens to
            // distinguish it from plain Enter.
            (KeyCode::Enter, KeyModifiers::ALT) => {
                self.prompt_buffer.push('\n');
            }
            (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.prompt_buffer.push('\n');
            }
            (KeyCode::Enter, _) => {
                self.input_mode = InputMode::Normal;
                self.send_prompt();
            }
            (KeyCode::Backspace, _) => {
                self.prompt_buffer.pop();
            }
            (KeyCode::Char(c), _) => {
                self.prompt_buffer.push(c);
            }
            _ => {
                // Log unknown keycodes so the user can see what their
                // terminal is sending and we can adjust bindings.
                self.firehose.push(debug);
            }
        }
    }

    fn on_key_normal(&mut self, code: KeyCode, mods: KeyModifiers) {
        match (code, mods) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            (KeyCode::Char(':'), _) => {
                self.input_mode = InputMode::Command;
                self.command_buffer.clear();
            }
            (KeyCode::Char('n'), _) => self.create_session(),
            (KeyCode::Char('i'), _) | (KeyCode::Enter, _) => {
                if self.selected_session.selected().is_some() {
                    self.input_mode = InputMode::SendPrompt;
                    self.prompt_buffer.clear();
                }
            }
            (KeyCode::F(1), _) => self.mode = ViewMode::CommandCenter,
            (KeyCode::F(2), _) => self.mode = ViewMode::SessionTree,
            (KeyCode::F(3), _) => {
                self.mode = ViewMode::SessionDetail;
                self.transcript_scroll_offset = 0; // start pinned to bottom
                self.refresh_transcript_for_selection();
            }
            // Transcript scrolling — only meaningful on F3.
            (KeyCode::PageUp, _) if matches!(self.mode, ViewMode::SessionDetail) => {
                self.transcript_scroll_offset =
                    self.transcript_scroll_offset.saturating_add(10);
            }
            (KeyCode::PageDown, _) if matches!(self.mode, ViewMode::SessionDetail) => {
                self.transcript_scroll_offset =
                    self.transcript_scroll_offset.saturating_sub(10);
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
                self.transcript_scroll_offset =
                    self.transcript_scroll_offset.saturating_add(2);
            }
            (KeyCode::Char('j'), _) if matches!(self.mode, ViewMode::SessionDetail) => {
                self.transcript_scroll_offset =
                    self.transcript_scroll_offset.saturating_sub(2);
            }
            (KeyCode::F(4), _) => self.mode = ViewMode::Context,
            (KeyCode::F(5), _) => self.mode = ViewMode::Usage,
            (KeyCode::F(6), _) => self.mode = ViewMode::Tools,
            (KeyCode::F(7), _) => self.mode = ViewMode::Router,
            (KeyCode::F(8), _) => self.mode = ViewMode::Events,
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
                self.send_prompt();
            }
            "provider" => {
                if rest.len() >= 2 {
                    self.default_provider = (rest[0].to_string(), rest[1].to_string());
                    self.firehose.push(format!(
                        "[ok] default provider set to {}/{}",
                        rest[0], rest[1]
                    ));
                } else {
                    self.firehose
                        .push("[usage] :provider <kind> <model>".into());
                }
            }
            "quit" | "q" => self.quit = true,
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
        .and_then(|u| {
            let i = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
            let o = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
            Some(((i + o) / 1000) as u32)
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
    let sid = params.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
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
            let name = params.get("tool_name").and_then(|v| v.as_str()).unwrap_or("?");
            let tid = params.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("?");
            firehose.push(format!("[{sid}] tool.start {name} ({tid})"));
            if let Some(i) = find(sessions, sid) {
                sessions[i].phase = format!("tool: {name}");
            }
        }
        "session.tool_call_completed" => {
            let tid = params.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("?");
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
            let rid = params.get("request_id").and_then(|v| v.as_str()).unwrap_or("?");
            firehose.push(format!("[{sid}] !! input_required ({rid}) — TUI approval flow TBD"));
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
            let stop = params.get("stop_reason").and_then(|v| v.as_str()).unwrap_or("?");
            let elapsed_ms = params.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            firehose.push(format!("[{sid}] done stop={stop} elapsed={elapsed_ms}ms"));
            if let Some(i) = find(sessions, sid) {
                sessions[i].status = SessionStatus::Done;
                sessions[i].phase = format!("done ({stop})");
                if let Some(usage) = params.get("usage") {
                    let inp = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let out = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    sessions[i].tokens_kilo = ((inp + out) / 1000) as u32;
                }
            }
            // Clear the status snapshot — session is no longer
            // running. The next ask will re-populate.
            latest_status.remove(sid);
        }
        "session.error" => {
            let msg = params.get("message").and_then(|v| v.as_str()).unwrap_or("error");
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
            Constraint::Length(3),       // header
            Constraint::Length(2),       // mode tabs
            Constraint::Min(10),         // main body
            Constraint::Length(10),      // firehose (8 lines content + borders)
            Constraint::Length(1),       // status line
        ])
        .split(area);

    render_header(f, app, chunks[0]);
    render_tabs(f, app, chunks[1]);
    match app.mode {
        ViewMode::CommandCenter => render_command_center(f, app, chunks[2]),
        ViewMode::SessionTree => render_placeholder(f, chunks[2], "Session Tree", "F2"),
        ViewMode::SessionDetail => render_session_detail(f, app, chunks[2]),
        ViewMode::Context => render_placeholder(f, chunks[2], "Context Inspector", "F4"),
        ViewMode::Usage => render_placeholder(f, chunks[2], "Usage / Cache", "F5"),
        ViewMode::Tools => render_placeholder(f, chunks[2], "Tools / MCP / Skills", "F6"),
        ViewMode::Router => render_placeholder(f, chunks[2], "Router / Proxy", "F7"),
        ViewMode::Events => render_placeholder(f, chunks[2], "Event Explorer", "F8"),
        ViewMode::Mailbox => render_placeholder(f, chunks[2], "Mailbox (cooperating sessions)", "F9"),
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
                Span::styled(
                    s.phase.clone(),
                    Style::default().fg(Color::Cyan),
                ),
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
                let synthetic_elapsed_ms = snap.elapsed_ms
                    + snap.received_at.elapsed().as_millis() as u64;
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
    let paragraph = Paragraph::new(detail_text).block(block).wrap(Wrap { trim: false });
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
fn render_session_detail(f: &mut Frame, app: &App, area: Rect) {
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
        Paragraph::new(header_lines)
            .block(Block::default().borders(Borders::ALL).title(" Session Detail (F3) ")),
        chunks[0],
    );

    // Transcript body. Pull cached events from the last
    // session.events poll; degrade to a "(loading…)" placeholder if
    // we haven't fetched yet.
    let body_lines: Vec<Line> = if let Some(events) =
        app.transcript_events_by_sid.get(&sid)
    {
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
    let scroll_y = max_top
        .saturating_sub(app.transcript_scroll_offset as usize)
        .min(max_top) as u16;

    let title = if app.transcript_scroll_offset == 0 {
        " Transcript ".to_string()
    } else {
        format!(
            " Transcript (scrolled up {} · End to bottom · Home to top) ",
            app.transcript_scroll_offset
        )
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let paragraph = Paragraph::new(body_lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    f.render_widget(paragraph, chunks[1]);
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
        let Some(payload) = ev.get("payload") else { continue };
        let kind = payload.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let ts = ev.get("timestamp_unix_ms").and_then(|v| v.as_u64()).unwrap_or(0);
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
                if text.is_empty() {
                    // Fall through to a small debug marker so the
                    // block is visible even with no text (e.g.
                    // tool-only turns).
                    push_block(
                        &mut lines,
                        "assistant",
                        Color::White,
                        "(no text in this turn)",
                    );
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
            "session_created" | "callout" | "context_assembly" | "session_closed" => {
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
                Color::Yellow,
                partial,
            );
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
    let block = Block::default().borders(Borders::ALL).title(format!(" {name} "));
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
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_statusline(f: &mut Frame, app: &App, area: Rect) {
    let content = match app.input_mode {
        InputMode::Command => format!(":{}", app.command_buffer),
        InputMode::SendPrompt => {
            let preview: String = app.prompt_buffer.chars().take(80).collect();
            format!(
                " > {preview}{}    [enter=send · alt-enter or ctrl-j=newline · esc=cancel]",
                if app.prompt_buffer.chars().count() > 80 {
                    "…"
                } else {
                    ""
                }
            )
        }
        InputMode::Normal => {
            let scroll_hint = if matches!(app.mode, ViewMode::SessionDetail) {
                "j/k PgUp/PgDn Home/End scroll · "
            } else {
                "j/k select · "
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
            match MuClient::spawn(
                &bin.to_string_lossy(),
                &tools_refs,
                &cwd,
                cli.bash_yolo,
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

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run(&mut terminal, mu, (cli.provider, cli.model));

    // Cleanup
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    res
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
        if app.quit {
            break;
        }
    }
    Ok(())
}
