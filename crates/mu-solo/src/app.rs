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

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::Backend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde_json::Value;

use crate::client::{Client, Message};
use crate::render;

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
}

impl App {
    /// Spawn `mu serve`, authenticate, create a session, and return an
    /// App ready to run.
    pub fn new(
        mu_binary: &str,
        cwd: &std::path::Path,
        provider: &str,
        model: &str,
        bash_yolo: bool,
    ) -> Result<Self> {
        let mut client = Client::spawn(mu_binary, cwd, bash_yolo)?;

        // Normalize provider input → daemon's snake_case wire enum
        // (mirrors mu-tui's accept-anything mapping in create_session).
        let lc = provider.to_lowercase();
        let kind: String = match lc.as_str() {
            "anthropic" | "anthropic-api" | "anthropic_api" | "claude" => "anthropic_api".into(),
            "openai" | "openai-codex" | "openai_codex" | "codex" => "openai_codex".into(),
            "openrouter" | "open-router" | "open_router" => "openrouter".into(),
            "faux" => "faux".into(),
            _ => lc.clone(),
        };

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

        let status = format!(" {provider} · {model} · {session_id} ");

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
                "Type a prompt + Enter to send. /q or Ctrl-C to quit.",
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
                if trimmed == "/q" || trimmed == "/quit" {
                    return Ok(true);
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
            let p = Paragraph::new(lines).wrap(Wrap { trim: false });
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;

        // Reset per-turn streaming state.
        self.streaming_text.clear();
        self.streaming_header_open = false;
        self.streaming_chars_emitted = 0;
        self.pending_close = false;

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
                        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
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
                        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
                        ratatui::widgets::Widget::render(p, buf.area, buf);
                    })?;
                }
                Message::Response { .. } => {} // ignore late responses
            }
        }
        // Phase 2: if streaming ended (pending_close) and we have an
        // open header, flush trailing buffer and emit closer.
        if self.pending_close && self.streaming_header_open {
            let width = terminal.size()?.width as usize;
            let wrap_width = width.saturating_sub(2);
            // Flush any trailing chars past the last newline.
            let total = self.streaming_text.chars().count();
            if total > self.streaming_chars_emitted {
                let remaining: String = self
                    .streaming_text
                    .chars()
                    .skip(self.streaming_chars_emitted)
                    .collect();
                emit_body_chunk(terminal, &remaining, wrap_width)?;
            }
            // Closer.
            let closer = Line::from(Span::styled(
                "└─".to_string(),
                Style::default().fg(Color::White),
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
        // Filter to this session only. (v0 has one session anyway, but
        // be defensive.)
        let sid = params.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        if !sid.is_empty() && sid != self.session_id {
            return Ok(());
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
                self.emit_streaming(terminal, wrap_width)?;
            }
            "session.assistant_text_finalized" => {
                // Replace accumulator with canonical (matches mu-tui's
                // `mu-wk2` invariant: this notification's `text` is
                // what the AssistantMessage will commit).
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    self.streaming_text = text.to_string();
                    self.emit_streaming(terminal, wrap_width)?;
                }
                // In agent loops this fires per-invocation. Trigger
                // the close so the block lands in scrollback before
                // any subsequent tool calls / next invocation render.
                if self.streaming_header_open {
                    self.pending_close = true;
                }
            }
            "session.done" | "session.error" if self.streaming_header_open => {
                self.pending_close = true;
            }
            _ => {} // ignore unhandled notifications for v0
        }
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
        if !self.streaming_header_open {
            let header = Line::from(Span::styled(
                "┌─ assistant ".to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
            terminal.insert_before(1, |buf| {
                ratatui::widgets::Widget::render(Paragraph::new(header), buf.area, buf);
            })?;
            self.streaming_header_open = true;
        }
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
                emit_body_chunk(terminal, &chunk, wrap_width)?;
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
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let inner = wrap_width.saturating_sub(2).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for raw in body.lines() {
        for row in render::wrap_line(raw, inner) {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::White)),
                Span::raw(row),
            ]));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    let h = lines.len() as u16;
    terminal.insert_before(h, |buf| {
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
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
