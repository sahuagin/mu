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

use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::Backend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde_json::Value;

use crate::client::{Client, Message};
use crate::picker;
use crate::render;

/// Known providers offered by the `/provider` picker. Free-form
/// `/provider <name>` also works for anything not on this list.
const KNOWN_PROVIDERS: &[&str] = &[
    "openai-codex",
    "anthropic",
    "anthropic-oauth",
    "openai-api",
    "openrouter",
    "faux",
];

/// Known models per provider for the `/model` picker. Returns an
/// empty slice for providers we don't have curated lists for; the
/// caller falls back to free-form entry. Strings live as `&'static str`
/// so we can hand them to the picker without allocating.
fn known_models_for(provider: &str) -> &'static [&'static str] {
    match normalize_provider_kind(provider).as_str() {
        "anthropic_api" | "anthropic_oauth" => &[
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
        "faux" => "faux".into(),
        _ => lc,
    }
}

/// Session-level effort dial. Layer on top of model selection per
/// claude-code-feature-mapping §17. v0 is display-only — `ask_session`'s
/// wire schema doesn't carry effort yet, so this knob exists in the
/// TUI surface ready to attach when the daemon learns the field.
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

    pub const ALL: &'static [Self] = &[
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];
}

/// User-visible app state. Held across the run loop.
pub struct App {
    client: Client,
    session_id: String,
    /// Provider + model strings (display-only for v0).
    provider: String,
    model: String,
    /// Currently typed prompt (single line for v0; multi-line later).
    prompt: String,
    /// Streaming-text accumulator for the in-flight assistant turn.
    /// Cleared on `session.done` / `session.error`.
    streaming_text: String,
    /// Whether the `┌─ assistant ` header has been emitted to scrollback
    /// for the current in-flight turn (cleared when the closer emits).
    streaming_header_open: bool,
    /// Number of chars from `streaming_text` already emitted to
    /// scrollback as body. Char-based (UTF-8 safe).
    streaming_chars_emitted: usize,
    /// Set when streaming_text was just cleared and we still owe a
    /// closer. Phase 2 of the tick emits it.
    pending_close: bool,
    /// Set when an assistant_text_finalized was seen — used to detect
    /// "we already showed equivalent text as preview, don't render
    /// the committed AssistantMessage." Decremented when phase 1 skips
    /// a matching event.
    pending_skip_assistant: usize,
    /// Status line at the bottom (provider/model/cost/etc).
    status: String,
    /// Daemon ID (per daemon.stats at startup). Surfaced via /status.
    daemon_id: String,
    /// Daemon version string. Surfaced via /status.
    daemon_version: String,
    /// Session-level effort dial (§17). Display-only in v0 — attached
    /// to `ask_session` params once the daemon learns the field.
    effort: EffortLevel,
    /// Focus mode (§16): when true, suppress streaming text_delta
    /// previews and render the assistant block in one shot on
    /// `assistant_text_finalized`. Default off.
    focus_mode: bool,
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
    /// Which session owns the currently-streaming turn. Set when an
    /// ask is fired (Main on /send, Btw on /btw); used by
    /// `emit_streaming` and the closer-emit path to pick the right
    /// label + color. None when no turn is in flight.
    streaming_route: Option<TurnRoute>,
}

impl App {
    /// Spawn `mu serve`, authenticate, create a session, and return an
    /// App ready to run.
    ///
    /// `effort` is parsed via [`EffortLevel::parse`]; invalid values
    /// surface as an error so a typo in `solo.toml` doesn't silently
    /// fall back to Medium. `focus_mode` seeds the /focus toggle.
    pub fn new(
        mu_binary: &str,
        cwd: &std::path::Path,
        provider: &str,
        model: &str,
        bash_yolo: bool,
        tools: &str,
        effort: &str,
        focus_mode: bool,
    ) -> Result<Self> {
        let effort = EffortLevel::parse(effort).ok_or_else(|| {
            anyhow!(
                "invalid effort {effort:?} (valid: low|medium|high|xhigh|max)"
            )
        })?;
        let mut client = Client::spawn(mu_binary, cwd, bash_yolo, tools)?;

        // Normalize provider input → daemon's snake_case wire enum
        // (mirrors mu-tui's accept-anything mapping in create_session).
        let kind = normalize_provider_kind(provider);

        let resp = client
            .request(
                "create_session",
                serde_json::json!({
                    "provider": { "kind": kind, "model": model },
                }),
            )
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

        let status = format!(" {provider} · {model} · {session_id} ");
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

        Ok(Self {
            client,
            session_id,
            provider: provider.to_string(),
            model: model.to_string(),
            prompt: String::new(),
            streaming_text: String::new(),
            streaming_header_open: false,
            streaming_chars_emitted: 0,
            pending_close: false,
            pending_skip_assistant: 0,
            status,
            daemon_id,
            daemon_version,
            effort,
            focus_mode,
            sidecar_session_id: None,
            streaming_route: None,
            events_file,
            actual_renderer: None,
            actual_cache_strategy: None,
            actual_provider_kind: None,
            actual_model: None,
            renderer_mismatch_warned: false,
        })
    }

    /// Run the event loop. Returns Ok(()) on clean exit (user pressed q).
    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        // Initial banner — printed once into scrollback.
        let banner_lines = vec![
            Line::from(Span::styled(
                format!("mu-solo · {} · {}", self.provider, self.model),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
        terminal.insert_before(banner_lines.len() as u16, |buf| {
            let p = Paragraph::new(banner_lines).wrap(Wrap { trim: false });
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;

        let tick = Duration::from_millis(100);
        let mut last_tick = Instant::now();

        loop {
            // Drain any notifications that arrived since the last tick.
            self.drain_notifications(terminal)?;

            // Draw the inline viewport (status + input prompt).
            terminal.draw(|f| {
                let area = f.area();
                let lines = vec![
                    // Separator line.
                    Line::from(Span::styled(
                        "─".repeat(area.width as usize),
                        Style::default().fg(Color::DarkGray),
                    )),
                    // Input prompt.
                    Line::from(vec![
                        Span::styled(" > ", Style::default().fg(Color::Cyan)),
                        Span::raw(self.prompt.clone()),
                        Span::styled(
                            "_",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::SLOW_BLINK),
                        ),
                    ]),
                    // Status footer.
                    Line::from(Span::styled(
                        self.status.clone(),
                        Style::default().fg(Color::DarkGray),
                    )),
                ];
                let p = Paragraph::new(lines);
                f.render_widget(p, area);
            })?;

            // Wait for input with a tick budget so notifications drain
            // even when the user isn't typing.
            let elapsed = last_tick.elapsed();
            let wait = tick.saturating_sub(elapsed);
            if event::poll(wait)? {
                if let Event::Key(key) = event::read()? {
                    if self.handle_key(terminal, key)? {
                        break; // user requested exit
                    }
                }
            }
            if last_tick.elapsed() >= tick {
                last_tick = Instant::now();
            }
        }
        Ok(())
    }

    /// Handle one keypress. Returns Ok(true) to exit the loop.
    fn handle_key<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        key: KeyEvent,
    ) -> Result<bool>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => return Ok(true),
            // Esc clears the prompt buffer (conventional "cancel
            // typed input"). It is NOT an exit shortcut — zellij /
            // tmux multiplexer scrollback exits also send Esc, and
            // having Esc quit mu-solo turned that into an accidental
            // session kill. Quit paths are now: /q, /quit, Ctrl-C.
            (_, KeyCode::Esc) => {
                self.prompt.clear();
            }
            (_, KeyCode::Char(c)) => self.prompt.push(c),
            (_, KeyCode::Backspace) => {
                self.prompt.pop();
            }
            (_, KeyCode::Enter) => {
                let text = std::mem::take(&mut self.prompt);
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
                        self.emit_status_lines(terminal)?;
                        return Ok(false);
                    }
                    "/help" if tail.is_empty() => {
                        self.emit_help_lines(terminal)?;
                        return Ok(false);
                    }
                    "/effort" => {
                        self.cmd_effort(terminal, tail)?;
                        return Ok(false);
                    }
                    "/focus" => {
                        self.cmd_focus(terminal, tail)?;
                        return Ok(false);
                    }
                    "/btw" => {
                        self.cmd_btw(terminal, tail)?;
                        return Ok(false);
                    }
                    "/provider" => {
                        self.cmd_provider(terminal, tail)?;
                        return Ok(false);
                    }
                    "/model" => {
                        self.cmd_model(terminal, tail)?;
                        return Ok(false);
                    }
                    "/cancel" if tail.is_empty() => {
                        self.cmd_cancel(terminal)?;
                        return Ok(false);
                    }
                    "/clear" if tail.is_empty() => {
                        self.cmd_clear(terminal)?;
                        return Ok(false);
                    }
                    _ if head.starts_with('/') => {
                        self.emit_unknown_command(terminal, head)?;
                        return Ok(false);
                    }
                    _ => {}
                }
                self.send_prompt(terminal, trimmed)?;
            }
            _ => {}
        }
        Ok(false)
    }

    /// Send a user prompt: emit the "you" block to scrollback, then
    /// fire `session.ask`.
    fn send_prompt<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        text: &str,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let width = terminal.size()?.width as usize;
        let wrap_width = width.saturating_sub(2);
        let lines = render::you_block(text, wrap_width);
        let height = lines.len() as u16;
        terminal.insert_before(height, |buf| {
            // No .wrap() — block_lines already pre-wrapped to a
            // budget that includes safety gutter; double-wrap loses
            // the "│ " prefix on continuation rows.
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;

        // Reset per-turn streaming state.
        self.streaming_text.clear();
        self.streaming_header_open = false;
        self.streaming_chars_emitted = 0;
        self.pending_close = false;
        self.streaming_route = Some(TurnRoute::Main);

        // Fire and forget the request; responses come back as
        // notifications drained in `drain_notifications`. Method per
        // mu's wire protocol is `ask_session` with `user_message`.
        let _ = self.client.request(
            "ask_session",
            serde_json::json!({
                "session_id": self.session_id,
                "user_message": text,
            }),
        )?;
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
    fn cmd_btw<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        msg: &str,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        if msg.is_empty() {
            let lines: Vec<Line<'static>> = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "usage: /btw <message>".to_string(),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                )),
                Line::from("  fires a side question to a sidecar session;"),
                Line::from("  main session history is unaffected."),
                Line::from(""),
            ];
            let h = lines.len() as u16;
            terminal.insert_before(h, |buf| {
                let p = Paragraph::new(lines);
                ratatui::widgets::Widget::render(p, buf.area, buf);
            })?;
            return Ok(());
        }
        if self.streaming_header_open {
            let lines: Vec<Line<'static>> = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "wait — main turn still streaming".to_string(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )),
                Line::from("  retry /btw once the current response finishes."),
                Line::from(""),
            ];
            let h = lines.len() as u16;
            terminal.insert_before(h, |buf| {
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
        let width = terminal.size()?.width as usize;
        let wrap_width = width.saturating_sub(2);
        let route = TurnRoute::Btw;
        let lines = render::block_lines(
            route.you_label(),
            route.you_color(),
            msg,
            wrap_width,
        );
        let h = lines.len() as u16;
        terminal.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;

        // Reset per-turn streaming state and route this turn to btw.
        self.streaming_text.clear();
        self.streaming_header_open = false;
        self.streaming_chars_emitted = 0;
        self.pending_close = false;
        self.streaming_route = Some(route);

        let _ = self.client.request(
            "ask_session",
            serde_json::json!({
                "session_id": sid,
                "user_message": msg,
            }),
        )?;
        Ok(())
    }

    /// /status — print provider, model, session_id, daemon_id, version
    /// to scrollback. Lets the operator find the daemon's events
    /// directory: ~/.local/share/mu/events/{daemon_id}/session-N.jsonl
    fn emit_status_lines<B: Backend>(&self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let lines: Vec<Line<'static>> = vec![
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
        ];
        let h = lines.len() as u16;
        terminal.insert_before(h, |buf| {
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
    fn cmd_effort<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        arg: &str,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let lines: Vec<Line<'static>> = if arg.is_empty() {
            let choices: Vec<String> = EffortLevel::ALL
                .iter()
                .map(|e| e.as_str().to_string())
                .collect();
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "── /effort ─────────────────────────".to_string(),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("  current:  {}", self.effort.as_str())),
                Line::from(format!("  choices:  {}", choices.join(" · "))),
                Line::from("  usage:    /effort <level>"),
                Line::from(""),
            ]
        } else if let Some(level) = EffortLevel::parse(arg) {
            self.effort = level;
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("effort → {}", level.as_str()),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
        terminal.insert_before(h, |buf| {
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
    fn cmd_focus<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        arg: &str,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let new_value = match arg.trim().to_lowercase().as_str() {
            "" | "toggle" => !self.focus_mode,
            "on" | "true" | "1" | "yes" => true,
            "off" | "false" | "0" | "no" => false,
            other => {
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
                terminal.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                })?;
                return Ok(());
            }
        };
        self.focus_mode = new_value;
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("focus → {}", if new_value { "on" } else { "off" }),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        terminal.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /provider [name] — show or change the selected provider. Bare
    /// `/provider` opens a modal picker (KNOWN_PROVIDERS); `/provider
    /// <name>` sets directly without the picker (useful for any
    /// provider not on the curated list).
    ///
    /// v0 stub-first semantics (memory 85cf7400): updates App state +
    /// banner + /status. The currently-running main session stays
    /// bound to its original provider — there's no session.switch_provider
    /// RPC yet, so live-switch isn't wired. The new value DOES take
    /// effect for future /btw sidecars and for the next process restart.
    fn cmd_provider<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        arg: &str,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let new_provider = if arg.is_empty() {
            let items: Vec<String> =
                KNOWN_PROVIDERS.iter().map(|s| (*s).to_string()).collect();
            let current = items.iter().position(|s| s == &self.provider).unwrap_or(0);
            match picker::run_picker("/provider", &items, current)? {
                Some(idx) => items[idx].clone(),
                None => return Ok(()), // cancelled
            }
        } else {
            arg.to_string()
        };

        let changed = new_provider != self.provider;
        self.provider = new_provider.clone();
        // Refresh the bottom-of-viewport status string so the next
        // draw shows the new provider immediately.
        self.status = format!(" {} · {} · {} ", self.provider, self.model, self.session_id);

        let lines: Vec<Line<'static>> = if changed {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("provider → {new_provider}"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "  applies to new sessions (e.g. /btw); current session keeps its bound provider"
                        .to_string(),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("provider unchanged ({new_provider})"),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ]
        };
        let h = lines.len() as u16;
        terminal.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /model [name] — show or change the selected model. Bare
    /// `/model` opens a picker scoped to the current provider; `/model
    /// <name>` sets directly. Same stub-first semantics as /provider:
    /// updates App state, applies to new sessions; the current main
    /// session keeps its bound model.
    fn cmd_model<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        arg: &str,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let new_model = if arg.is_empty() {
            let known = known_models_for(&self.provider);
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
                terminal.insert_before(h, |buf| {
                    let p = Paragraph::new(lines);
                    ratatui::widgets::Widget::render(p, buf.area, buf);
                })?;
                return Ok(());
            }
            // If the current model is not in the known list, prepend
            // it so the picker doesn't pretend it doesn't exist.
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

        let changed = new_model != self.model;
        self.model = new_model.clone();
        self.status = format!(" {} · {} · {} ", self.provider, self.model, self.session_id);

        let lines: Vec<Line<'static>> = if changed {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("model → {new_model}"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "  applies to new sessions (e.g. /btw); current session keeps its bound model"
                        .to_string(),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("model unchanged ({new_model})"),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
            ]
        };
        let h = lines.len() as u16;
        terminal.insert_before(h, |buf| {
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
    fn cmd_cancel<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
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
        let canceled = resp.get("canceled").and_then(|v| v.as_bool()).unwrap_or(false);
        let was_in = resp
            .get("was_in")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let lines: Vec<Line<'static>> = if canceled {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "/cancel — provider call aborted".to_string(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
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
        terminal.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /clear — clear the visible scrollback. Doesn't touch the
    /// daemon's event log; this is a display-only reset. The inline
    /// viewport redraws on the next tick.
    fn cmd_clear<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        terminal.clear()?;
        Ok(())
    }

    /// Unknown-command stub. Keeps typos from getting sent to the
    /// model as a prompt (which would burn tokens and confuse the
    /// session). Mirrors claude-code's "Unknown slash command" hint.
    fn emit_unknown_command<B: Backend>(
        &self,
        terminal: &mut Terminal<B>,
        head: &str,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
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
        terminal.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// /help — print the built-in command surface to scrollback.
    fn emit_help_lines<B: Backend>(&self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let lines: Vec<Line<'static>> = vec![
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
            Line::from("  /provider [name]   list-picker (bare) or set directly"),
            Line::from("  /model [name]      list-picker (bare) or set directly"),
            Line::from("  /btw <message>     side question via sidecar (main history unaffected)"),
            Line::from("  /cancel            abort the in-flight provider call"),
            Line::from("  /clear             clear the visible scrollback"),
            Line::from("  /q, /quit, /exit   leave the session"),
            Line::from(""),
            Line::from("  Esc                clear the current prompt"),
            Line::from("  Ctrl-C             leave the session"),
            Line::from(""),
            Line::from("  Anything else is sent to the model as a prompt."),
            Line::from(""),
        ];
        let h = lines.len() as u16;
        terminal.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// Pull every queued notification, accumulate streaming_text,
    /// emit incremental preview lines via `insert_before`, and handle
    /// session.done / session.error to close the block.
    fn drain_notifications<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        while let Some(msg) = self.client.try_recv_notification() {
            match msg {
                Message::Notification { method, params } => {
                    self.handle_notification(terminal, &method, &params)?;
                }
                Message::Eof => {
                    let width = terminal.size()?.width as usize;
                    let wrap = width.saturating_sub(2);
                    let lines = render::error_block(
                        "mu serve closed stdout — daemon exited",
                        wrap,
                    );
                    let h = lines.len() as u16;
                    terminal.insert_before(h, |buf| {
                        // No .wrap() — pre-wrapped by block_lines.
                        let p = Paragraph::new(lines);
                        ratatui::widgets::Widget::render(p, buf.area, buf);
                    })?;
                    anyhow::bail!("daemon exited unexpectedly");
                }
                Message::ReaderError(e) => {
                    let width = terminal.size()?.width as usize;
                    let wrap = width.saturating_sub(2);
                    let lines = render::error_block(&format!("reader error: {e}"), wrap);
                    let h = lines.len() as u16;
                    terminal.insert_before(h, |buf| {
                        // No .wrap() — pre-wrapped by block_lines.
                        let p = Paragraph::new(lines);
                        ratatui::widgets::Widget::render(p, buf.area, buf);
                    })?;
                }
                Message::Response { .. } => {} // ignore late responses
            }
        }
        // Phase 2: if streaming ended (pending_close) and we have an
        // open header, flush trailing buffer and emit closer. Color
        // matches the route owning the current turn (white for main,
        // magenta for /btw).
        if self.pending_close && self.streaming_header_open {
            let width = terminal.size()?.width as usize;
            let wrap_width = width.saturating_sub(2);
            let route = self.streaming_route.unwrap_or(TurnRoute::Main);
            // Flush any trailing chars past the last newline.
            let total = self.streaming_text.chars().count();
            if total > self.streaming_chars_emitted {
                let remaining: String = self
                    .streaming_text
                    .chars()
                    .skip(self.streaming_chars_emitted)
                    .collect();
                emit_body_chunk(terminal, &remaining, wrap_width, route.color())?;
            }
            // Closer.
            let closer = Line::from(Span::styled(
                "└─".to_string(),
                Style::default().fg(route.color()),
            ));
            terminal.insert_before(2, |buf| {
                let lines = vec![closer, Line::from("")];
                let p = Paragraph::new(lines);
                ratatui::widgets::Widget::render(p, buf.area, buf);
            })?;
            // Mark next AssistantMessage commit for skip (not used in
            // v0 since we render only from streaming; placeholder for
            // when we add committed-event rendering).
            self.pending_skip_assistant += 1;
            // Reset.
            self.streaming_text.clear();
            self.streaming_header_open = false;
            self.streaming_chars_emitted = 0;
            self.pending_close = false;
            self.streaming_route = None;
        }
        Ok(())
    }

    /// Dispatch a single notification.
    fn handle_notification<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        method: &str,
        params: &Value,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        // Route notifications to the right turn (main vs sidecar /btw).
        // Notifications without a session_id (rare; some daemon
        // events) fall through with whatever streaming_route is set.
        let sid = params.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
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
        let width = terminal.size()?.width as usize;
        let wrap_width = width.saturating_sub(2);
        match method {
            "session.text_delta" => {
                let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Ok(());
                }
                self.streaming_text.push_str(delta);
                // Focus mode (§16): accumulate silently; the finalized
                // event renders the whole block in one shot below.
                if !self.focus_mode {
                    self.emit_streaming(terminal, wrap_width)?;
                }
            }
            "session.assistant_text_finalized" => {
                // Replace accumulator with canonical (matches mu-tui's
                // `mu-wk2` invariant: this notification's `text` is
                // what the AssistantMessage will commit).
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    self.streaming_text = text.to_string();
                }
                if self.focus_mode {
                    // Render the entire assistant turn as one block —
                    // bypasses the header/body/closer streaming
                    // pipeline so we don't have to fix up partial
                    // emission state. Route-aware so /btw turns
                    // render as magenta "assistant ⋅ btw" blocks.
                    if !self.streaming_text.is_empty() {
                        let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                        let lines = render::block_lines(
                            route.header_label(),
                            route.color(),
                            &self.streaming_text,
                            wrap_width,
                        );
                        let h = lines.len() as u16;
                        terminal.insert_before(h, |buf| {
                            let p = Paragraph::new(lines);
                            ratatui::widgets::Widget::render(p, buf.area, buf);
                        })?;
                    }
                    self.streaming_text.clear();
                    self.streaming_header_open = false;
                    self.streaming_chars_emitted = 0;
                    self.pending_close = false;
                    self.streaming_route = None;
                } else {
                    if !self.streaming_text.is_empty() {
                        self.emit_streaming(terminal, wrap_width)?;
                    }
                    // In agent loops this fires per-invocation. Trigger
                    // the close so the block lands in scrollback before
                    // any subsequent tool calls / next invocation render.
                    if self.streaming_header_open {
                        self.pending_close = true;
                    }
                }
            }
            "session.tool_call_started" => {
                self.emit_tool_call_started(terminal, params, wrap_width)?;
            }
            "session.tool_call_completed" => {
                self.emit_tool_call_completed(terminal, params, wrap_width)?;
            }
            "session.done" | "session.error" => {
                // One-shot renderer-mismatch diagnostic: after the
                // first turn lands a context_assembly into the events
                // log, scan for it and surface a warning if the
                // daemon silently fell back to faux (or any renderer
                // other than what the user asked for).
                if self.actual_renderer.is_none() {
                    self.try_load_actual_renderer();
                    if self.is_renderer_mismatch() && !self.renderer_mismatch_warned {
                        self.emit_renderer_mismatch_warning(terminal)?;
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
                if self.streaming_header_open {
                    // Emit the error inline before the closer fires so
                    // the operator sees the message attached to the
                    // turn it killed. Otherwise the close happens
                    // silently and the error is invisible.
                    if let Some(msg) = error_msg.as_deref() {
                        self.emit_inline_error(terminal, msg, wrap_width)?;
                    }
                    self.pending_close = true;
                } else {
                    // Turn ended with no text AND no tool calls.
                    // Without this marker the user has no visual proof
                    // the turn ever ended — they sent a prompt, then
                    // staring into silence. For error turns we ALSO
                    // include the daemon-supplied message.
                    let lines: Vec<Line<'static>> = if let Some(msg) = error_msg.as_deref() {
                        // Truncate ridiculously long messages to keep
                        // the scrollback usable; the full text lives
                        // in the events JSONL if you need it.
                        let short: String = msg.chars().take(400).collect();
                        vec![
                            Line::from(""),
                            Line::from(Span::styled(
                                "× turn ended with error".to_string(),
                                Style::default()
                                    .fg(Color::Red)
                                    .add_modifier(Modifier::BOLD),
                            )),
                            Line::from(Span::styled(
                                format!("  {short}"),
                                Style::default().fg(Color::Red),
                            )),
                            Line::from(""),
                        ]
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
                    terminal.insert_before(h, |buf| {
                        let p = Paragraph::new(lines);
                        ratatui::widgets::Widget::render(p, buf.area, buf);
                    })?;
                    // Reset per-turn state so the next turn starts clean.
                    self.streaming_text.clear();
                    self.streaming_chars_emitted = 0;
                    self.streaming_route = None;
                }
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
            self.actual_renderer = p
                .get("renderer")
                .and_then(|r| r.as_str())
                .map(String::from);
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
    fn emit_renderer_mismatch_warning<B: Backend>(
        &self,
        terminal: &mut Terminal<B>,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(Span::styled(
                "⚠  renderer mismatch — daemon fell back to faux".to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "  asked:    {}/{}",
                self.provider, self.model
            )),
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
        terminal.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;
        Ok(())
    }

    /// Open the assistant block header if it isn't already. Shared
    /// between the streaming-text path (`emit_streaming`) and the
    /// tool-call rendering paths so a turn that only fires tool calls
    /// still produces a header + closer pair in scrollback. Header
    /// color and label come from `streaming_route` so /btw turns get
    /// magenta "assistant ⋅ btw" framing.
    fn ensure_streaming_header_open<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        if self.streaming_header_open {
            return Ok(());
        }
        let route = self.streaming_route.unwrap_or(TurnRoute::Main);
        let header = Line::from(Span::styled(
            format!("┌─ {} ", route.header_label()),
            Style::default()
                .fg(route.color())
                .add_modifier(Modifier::BOLD),
        ));
        terminal.insert_before(1, |buf| {
            ratatui::widgets::Widget::render(Paragraph::new(header), buf.area, buf);
        })?;
        self.streaming_header_open = true;
        Ok(())
    }

    /// Render a `session.tool_call_started` notification as a one-line
    /// entry inside the open assistant block:
    ///
    ///     │ ▸ bash
    ///
    /// The prefix bar uses the route color (white for main, magenta
    /// for /btw); the marker + tool name use a dim accent so they
    /// visually recede from any narration that follows. Header is
    /// opened on demand if no text had streamed yet.
    fn emit_tool_call_started<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        params: &Value,
        wrap_width: usize,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let _ = wrap_width; // single-line for now; wrap deferred
        let name = params
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        self.ensure_streaming_header_open(terminal)?;
        let route = self.streaming_route.unwrap_or(TurnRoute::Main);
        let line = Line::from(vec![
            Span::styled("│ ", Style::default().fg(route.color())),
            Span::styled(
                "▸ ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(name.to_string(), Style::default().fg(Color::Yellow)),
        ]);
        terminal.insert_before(1, |buf| {
            ratatui::widgets::Widget::render(Paragraph::new(line), buf.area, buf);
        })?;
        Ok(())
    }

    /// Emit a `× error: <message>` line inside an already-open
    /// assistant block, so when `session.error` lands the operator
    /// sees what failed before the closer fires. Without this the
    /// turn just ends silently and you have to grep the events JSONL
    /// to find out (e.g. "google/gemini-pro is not a valid model ID"
    /// for a /btw with a stale picker entry).
    fn emit_inline_error<B: Backend>(
        &self,
        terminal: &mut Terminal<B>,
        msg: &str,
        _wrap_width: usize,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let route = self.streaming_route.unwrap_or(TurnRoute::Main);
        // Truncate to keep the scrollback usable. Full message lives
        // in the daemon's event log if you need to inspect it.
        let short: String = msg.chars().take(400).collect();
        let line = Line::from(vec![
            Span::styled("│ ", Style::default().fg(route.color())),
            Span::styled(
                "× ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(short, Style::default().fg(Color::Red)),
        ]);
        terminal.insert_before(1, |buf| {
            ratatui::widgets::Widget::render(Paragraph::new(line), buf.area, buf);
        })?;
        Ok(())
    }

    /// Render a `session.tool_call_completed` notification as a
    /// one-line outcome inside the open assistant block:
    ///
    ///     │ ◂ ok    (green)
    ///     │ ◂ err   (red, plus any short error message)
    ///
    /// `params.outcome.kind` is the discriminator (per
    /// `ToolOutcome::{Ok, Err}` in mu-core's events.rs). On Err we
    /// pull a short prefix of `outcome.message` so the operator sees
    /// *why* it failed without leaving the TUI.
    fn emit_tool_call_completed<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        params: &Value,
        wrap_width: usize,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let _ = wrap_width;
        let outcome = params.get("outcome");
        let kind = outcome
            .and_then(|o| o.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let (label, accent, detail) = match kind {
            "ok" => ("ok", Color::Green, String::new()),
            "err" => {
                let msg = outcome
                    .and_then(|o| o.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let short: String = msg.chars().take(80).collect();
                (
                    "err",
                    Color::Red,
                    if short.is_empty() {
                        String::new()
                    } else {
                        format!(" · {short}")
                    },
                )
            }
            other => (other, Color::DarkGray, String::new()),
        };
        self.ensure_streaming_header_open(terminal)?;
        let route = self.streaming_route.unwrap_or(TurnRoute::Main);
        let mut spans = vec![
            Span::styled("│ ", Style::default().fg(route.color())),
            Span::styled(
                "◂ ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(label.to_string(), Style::default().fg(accent)),
        ];
        if !detail.is_empty() {
            spans.push(Span::styled(detail, Style::default().fg(Color::DarkGray)));
        }
        let line = Line::from(spans);
        terminal.insert_before(1, |buf| {
            ratatui::widgets::Widget::render(Paragraph::new(line), buf.area, buf);
        })?;
        Ok(())
    }

    /// Emit the streaming preview header (if not yet) and any new body
    /// lines up to the last `\n`. Mid-line trailing content stays
    /// buffered until the next newline or until pending_close flushes
    /// it.
    fn emit_streaming<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        wrap_width: usize,
    ) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        if self.streaming_text.is_empty() {
            return Ok(());
        }
        self.ensure_streaming_header_open(terminal)?;
        let route = self.streaming_route.unwrap_or(TurnRoute::Main);
        // Emit any new chars up to the last newline.
        let safe_byte_end = self
            .streaming_text
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        if safe_byte_end > 0 {
            let safe_chars = self.streaming_text[..safe_byte_end].chars().count();
            if safe_chars > self.streaming_chars_emitted {
                let chunk: String = self
                    .streaming_text
                    .chars()
                    .skip(self.streaming_chars_emitted)
                    .take(safe_chars - self.streaming_chars_emitted)
                    .collect();
                emit_body_chunk(terminal, &chunk, wrap_width, route.color())?;
                self.streaming_chars_emitted = safe_chars;
            }
        }
        Ok(())
    }
}

fn emit_body_chunk<B: Backend>(
    terminal: &mut Terminal<B>,
    body: &str,
    wrap_width: usize,
    color: Color,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Budget: wrap_width is already (terminal_width - 2). Reserve 2
    // more for the "│ " prefix AND 2 more as a safety gutter so the
    // pre-wrapped content provably fits in the destination width
    // without provoking ratatui's wrap to re-wrap and strip our
    // prefix. 4 columns of margin is the empirical safe number for
    // ratatui 0.30 + crossterm on narrow zellij panes.
    let inner = wrap_width.saturating_sub(4).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for raw in body.lines() {
        for row in render::wrap_line(raw, inner) {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(color)),
                Span::raw(row),
            ]));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    let h = lines.len() as u16;
    terminal.insert_before(h, |buf| {
        // No Wrap option — we pre-wrapped above. Adding ratatui's
        // wrap on top would re-wrap continuations and lose the "│ "
        // prefix on the new visual rows (same hazard mu-tui's
        // push_block / mu-2zs comment documents).
        let p = Paragraph::new(lines);
        ratatui::widgets::Widget::render(p, buf.area, buf);
    })?;
    Ok(())
}

/// Construct a `Terminal<CrosstermBackend>` configured for inline
/// rendering. Used by the binary; exposed here so library consumers
/// can construct their own.
pub fn make_inline_terminal(
) -> Result<Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>> {
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let opts = TerminalOptions {
        viewport: Viewport::Inline(3),
    };
    Terminal::with_options(backend, opts).context("terminal init")
}
