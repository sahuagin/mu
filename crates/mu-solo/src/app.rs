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
use crate::viewport::DynamicViewport;

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
        "anthropic_api" | "anthropic_oauth" => {
            &["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5"]
        }
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

    /// True if any item carries visible content (replaces the old
    /// `ask_had_output` flag for the live turn).
    fn has_output(&self) -> bool {
        self.items.iter().any(|it| match it {
            render::TurnItem::Text(s) => !s.is_empty(),
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
        "faux" => "faux".into(),
        _ => lc,
    }
}

#[allow(clippy::incompatible_msrv)]
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
    /// Which session owns the currently-streaming turn. Set when an ask
    /// is fired (Main on /send, Btw on /btw); kept in sync with
    /// `live_turn.route` and used by `/cancel` to route to the right
    /// session. None when no turn is in flight.
    streaming_route: Option<TurnRoute>,
    /// The in-flight assistant turn as a structured model (mu-d04a).
    /// Built by `handle_notification`, rendered live in the viewport each
    /// frame, committed to scrollback on done/error. None when idle.
    live_turn: Option<Turn>,
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

/// Provider-status-driven session phase for the status line. Tracks
/// the daemon's ProviderStatusKind via `session.provider_status`
/// notifications. Falls back to inference from other notifications
/// when provider_status isn't available (e.g. faux provider).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SessionPhase {
    #[default]
    Idle,
    AwaitingFirstToken,
    Streaming,
    ToolExecuting,
}

impl SessionPhase {
    fn icon(self) -> &'static str {
        match self {
            Self::Idle => "○",
            Self::AwaitingFirstToken => "◉",
            Self::Streaming => "●",
            Self::ToolExecuting => "⚙",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::AwaitingFirstToken => "thinking",
            Self::Streaming => "streaming",
            Self::ToolExecuting => "tool",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Idle => Color::DarkGray,
            Self::AwaitingFirstToken => Color::Cyan,
            Self::Streaming => Color::Green,
            Self::ToolExecuting => Color::Yellow,
        }
    }
}

impl App {
    /// Spawn `mu serve`, authenticate, create a session, and return an
    /// App ready to run.
    ///
    /// `effort` is parsed via [`EffortLevel::parse`]; invalid values
    /// surface as an error so a typo in `solo.toml` doesn't silently
    /// fall back to Medium. `focus_mode` seeds the /focus toggle.
    #[allow(clippy::too_many_arguments)]
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
            anyhow!("invalid effort {effort:?} (valid: low|medium|high|xhigh|max)")
        })?;
        let mut client = Client::spawn(mu_binary, cwd, bash_yolo, tools)?;

        let search_dirs = skills::default_search_dirs(Some(cwd));
        let skills = skills::discover(&search_dirs);
        if !skills.is_empty() {
            tracing::info!(count = skills.len(), "discovered skills");
        }

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
            sidecar_session_id: None,
            streaming_route: None,
            live_turn: None,
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
        })
    }

    /// Run the async event loop. Returns Ok(()) on clean exit.
    ///
    /// Uses `tokio::select!` to multiplex four event sources:
    /// - Keyboard/paste events via crossterm's `EventStream`
    /// - Daemon notifications via tokio mpsc (from the reader thread)
    /// - MCP session status via tokio mpsc (from rmcp client task)
    /// - Periodic render tick for elapsed-time display updates
    pub async fn run(&mut self) -> Result<()> {
        let mut vp = DynamicViewport::new(VIEWPORT_HEIGHT).context("DynamicViewport::new")?;
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
            // Drain any buffered notifications before render.
            self.drain_buffered_notifications(&mut vp)?;
            self.render_viewport(&mut vp)?;

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
                        Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                            if self.handle_key(&mut vp, key)? {
                                break;
                            }
                        }
                        Some(Ok(Event::Paste(text))) => {
                            self.paste_count += 1;
                            self.prompt.insert_paste(&text, self.paste_count);
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
    fn render_viewport(&mut self, vp: &mut DynamicViewport) -> Result<()> {
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
        let preview_rows = preview.len();

        let desired_height =
            (preview_rows as u16 + layout.lines.len() as u16 + 4 + menu_rows as u16) // +preview +separator +prompt +separator +status +info
                .clamp(VIEWPORT_HEIGHT, MAX_VIEWPORT_HEIGHT);
        if desired_height != vp.area().height {
            vp.set_height(desired_height)?;
        }

        let area = vp.area();
        let vp_w = area.width as usize;
        let vp_wrap = vp_w.saturating_sub(4);
        let vp_layout = self.prompt.visual_layout(vp_wrap);
        let max_prompt_rows = (area.height as usize).saturating_sub(4 + menu_rows + preview_rows);
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
            Message::Response { .. } => {}
        }
        Ok(())
    }

    /// Drain notifications that were buffered in the client during
    /// synchronous request() calls (before the async loop started).
    fn drain_buffered_notifications(&mut self, vp: &mut DynamicViewport) -> Result<()> {
        while let Some(msg) = self.client.try_recv_notification() {
            self.handle_message(vp, msg)?;
        }
        Ok(())
    }

    /// Handle one keypress. Returns Ok(true) to exit the loop.
    fn handle_key(&mut self, vp: &mut DynamicViewport, key: KeyEvent) -> Result<bool> {
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
                                        "/btw" | "/effort" | "/provider" | "/model" | "/focus"
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
                self.prompt.insert_char('/');
                let items = self.build_slash_menu_items();
                let max_visible = vp.area().height.saturating_sub(3) as usize;
                self.inline_menu = Some(InlineMenu::new(items, max_visible.max(5)));
            }
            (_, KeyCode::Char(c)) => self.prompt.insert_char(c),
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
                        self.emit_status_lines(vp)?;
                        return Ok(false);
                    }
                    "/help" => {
                        self.emit_help_lines(vp)?;
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
                    "/cancel" if tail.is_empty() => {
                        self.cmd_cancel(vp)?;
                        return Ok(false);
                    }
                    "/clear" if tail.is_empty() => {
                        self.cmd_clear(vp)?;
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

    /// Send a user prompt: emit the "you" block to scrollback, then
    /// fire `session.ask`.
    fn send_prompt(&mut self, vp: &mut DynamicViewport, text: &str) -> Result<()> {
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

        let _ = self.client.request(
            "ask_session",
            serde_json::json!({
                "session_id": self.session_id,
                "user_message": wire_text,
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
        let lines = render::block_lines(route.you_label(), route.you_color(), msg, wrap_width);
        let h = lines.len() as u16;
        vp.insert_before(h, |buf| {
            let p = Paragraph::new(lines);
            ratatui::widgets::Widget::render(p, buf.area, buf);
        })?;

        // Route this turn to the sidecar and start a fresh live turn.
        self.streaming_route = Some(route);
        self.live_turn = Some(Turn::new(route));

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
    fn emit_status_lines(&self, vp: &mut DynamicViewport) -> Result<()> {
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
        ];
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
            }
            Err(e) => {
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
            }
            Err(e) => {
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
    fn cmd_clear(&mut self, _vp: &mut DynamicViewport) -> Result<()> {
        // TODO: implement viewport clear
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
            MenuItem::new("/provider ›", "Select provider"),
            MenuItem::new("/model ›", "Select model"),
            MenuItem::new(
                "/btw",
                "Side question via sidecar (main history unaffected)",
            ),
            MenuItem::new("/cancel", "Abort the in-flight provider call"),
            MenuItem::new("/clear", "Clear the visible scrollback"),
            MenuItem::new("/q", "Leave the session"),
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

    fn emit_help_lines(&self, vp: &mut DynamicViewport) -> Result<()> {
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
            Line::from("  /provider [name]   list-picker (bare) or set directly"),
            Line::from("  /model [name]      list-picker (bare) or set directly"),
            Line::from("  /btw <message>     side question via sidecar (main history unaffected)"),
            Line::from("  /cancel            abort the in-flight provider call"),
            Line::from("  /clear             clear the visible scrollback"),
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
        let not_idle = phase != SessionPhase::Idle;

        let phase_text = if phase == SessionPhase::Idle {
            format!("{} {}", phase.icon(), phase.label())
        } else if self.phase_elapsed_ms > 0 {
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
        let (in_tok, out_tok, cache_read, cache_creation, cost, ctx_pct, ctx_window) =
            if let Some(ref s) = self.mcp_status {
                (
                    s.input_tokens,
                    s.output_tokens,
                    s.cache_read_tokens.unwrap_or(0),
                    s.cache_creation_tokens.unwrap_or(0),
                    s.cost_usd,
                    s.context_pressure_pct,
                    s.context_window_size,
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
            if let (Some(pct), Some(window)) = (ctx_pct, ctx_window) {
                let used = (pct / 100.0 * window as f64) as u64;
                let cs = format!(" {}/{}", format_tokens(used), format_tokens(window));
                metrics_text_len += cs.len();
                let ctx_style = if pct >= 90.0 {
                    Style::default().fg(Color::Red)
                } else if pct >= 70.0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    dim
                };
                spans.push(Span::styled(cs, ctx_style));
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
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
            .unwrap_or_else(|_| {
                // Last resort: short hostname from sysctl on FreeBSD
                std::process::Command::new("hostname")
                    .arg("-s")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "?".into())
            });
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
            .unwrap_or_else(|| "?".into());

        let left = format!("  {user}@{host}:{cwd}");

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
            Span::styled(left, Style::default().fg(Color::DarkGray)),
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
        self.session_phase = match status.phase.as_str() {
            "idle" => SessionPhase::Idle,
            "awaiting_first_token" => SessionPhase::AwaitingFirstToken,
            "streaming" => SessionPhase::Streaming,
            "thinking" => SessionPhase::AwaitingFirstToken,
            "tool_executing" | "awaiting_tool_result" => SessionPhase::ToolExecuting,
            _ => self.session_phase,
        };
        self.phase_elapsed_ms = status.phase_elapsed_ms;
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
            "session.tool_call_started" => {
                // Titlecase + primary-arg extraction happen here (build
                // time) so the renderer stays pure.
                let name = params
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let primary_arg = extract_primary_arg(name, params.get("arguments"));
                let display_name = titlecase_tool(name);
                let route = self.streaming_route.unwrap_or(TurnRoute::Main);
                self.live_turn
                    .get_or_insert_with(|| Turn::new(route))
                    .items
                    .push(render::TurnItem::ToolCall {
                        display_name,
                        primary_arg,
                    });
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
                let kind = params
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("idle");
                let elapsed = params
                    .get("elapsed_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let new_phase = match kind {
                    "awaiting_first_token" => SessionPhase::AwaitingFirstToken,
                    "streaming" => SessionPhase::Streaming,
                    "tool_executing" | "awaiting_tool_result" => SessionPhase::ToolExecuting,
                    "idle" => SessionPhase::Idle,
                    _ => self.session_phase,
                };
                self.session_phase = new_phase;
                self.phase_elapsed_ms = elapsed;
            }
            "session.done" | "session.error" => {
                self.session_phase = SessionPhase::Idle;
                self.phase_elapsed_ms = 0;

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
                        let mut lines = render::render_turn(
                            t.route.header_label(),
                            t.route.color(),
                            &t.items,
                            wrap_width,
                            preview_lines,
                        );
                        lines.extend(render::turn_closer(t.route.color()));
                        let h = lines.len() as u16;
                        vp.insert_before(h, |buf| {
                            let p = Paragraph::new(lines);
                            ratatui::widgets::Widget::render(p, buf.area, buf);
                        })?;
                    }
                    _ => {
                        // No visible output (empty turn or none), and any
                        // error has no turn to attach to: stand-alone block.
                        let lines: Vec<Line<'static>> = if let Some(msg) = error_msg.as_deref() {
                            // Truncate ridiculously long messages to keep
                            // the scrollback usable; the full text lives
                            // in the events JSONL if you need it.
                            let short: String = msg.chars().take(400).collect();
                            vec![
                                Line::from(""),
                                Line::from(Span::styled(
                                    "× turn ended with error".to_string(),
                                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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
                        vp.insert_before(h, |buf| {
                            let p = Paragraph::new(lines);
                            ratatui::widgets::Widget::render(p, buf.area, buf);
                        })?;
                    }
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

/// Minimum viewport height (separator + prompt + separator + status + info).
const VIEWPORT_HEIGHT: u16 = 5;
/// Maximum viewport height — cap to prevent eating the entire screen.
const MAX_VIEWPORT_HEIGHT: u16 = 20;
