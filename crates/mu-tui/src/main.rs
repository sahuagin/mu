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

use std::{
    io,
    time::{Duration, Instant},
};

use anyhow::Result;
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

struct App {
    mode: ViewMode,
    sessions: Vec<SessionRow>,
    selected_session: ListState,
    firehose: Vec<String>,
    command_mode: bool,
    command_buffer: String,
    quit: bool,
    last_tick: Instant,
    // Mock daemon stats (replaced by daemon.outstanding_calls + projection
    // queries in the next slice).
    daemon_uptime: Duration,
    event_count: u64,
    cost_budget: (f32, f32), // (used, budget)
}

impl App {
    fn new() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            mode: ViewMode::CommandCenter,
            sessions: mock_sessions(),
            selected_session: state,
            firehose: mock_firehose(),
            command_mode: false,
            command_buffer: String::new(),
            quit: false,
            last_tick: Instant::now(),
            daemon_uptime: Duration::from_secs(60 * 60 * 2 + 60 * 58),
            event_count: 18_213,
            cost_budget: (3.42, 10.0),
        }
    }

    fn tick(&mut self) {
        // In the future, this drives live updates from the daemon.
        // Today it just advances the mock-uptime counter so the header
        // looks alive while testing layout.
        let now = Instant::now();
        let dt = now - self.last_tick;
        self.last_tick = now;
        self.daemon_uptime += dt;
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if self.command_mode {
            match code {
                KeyCode::Esc => {
                    self.command_mode = false;
                    self.command_buffer.clear();
                }
                KeyCode::Enter => {
                    // Stub: command parsing/routing will live in a
                    // dedicated module once we have more than one
                    // verb. For now, accept and clear.
                    self.firehose
                        .push(format!("(stub) command: {}", self.command_buffer));
                    self.command_mode = false;
                    self.command_buffer.clear();
                }
                KeyCode::Backspace => {
                    self.command_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.command_buffer.push(c);
                }
                _ => {}
            }
            return;
        }
        match (code, mods) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            (KeyCode::Char(':'), _) => {
                self.command_mode = true;
                self.command_buffer.clear();
            }
            (KeyCode::F(1), _) => self.mode = ViewMode::CommandCenter,
            (KeyCode::F(2), _) => self.mode = ViewMode::SessionTree,
            (KeyCode::F(3), _) => self.mode = ViewMode::SessionDetail,
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
        },
        SessionRow {
            short_id: "design".into(),
            title: "mu-036 autonomous loop spec".into(),
            status: SessionStatus::Running,
            model: "anthropic / haiku-4.5".into(),
            cost_usd: 0.02,
            tokens_kilo: 14,
            phase: "streaming".into(),
        },
        SessionRow {
            short_id: "review".into(),
            title: "mu-022 edit tool review".into(),
            status: SessionStatus::Idle,
            model: "openrouter / sonnet-4.6".into(),
            cost_usd: 0.11,
            tokens_kilo: 22,
            phase: "awaiting approval (tool: edit)".into(),
        },
        SessionRow {
            short_id: "scout".into(),
            title: "cache ledger probe".into(),
            status: SessionStatus::Done,
            model: "anthropic / haiku-4.5".into(),
            cost_usd: 0.01,
            tokens_kilo: 6,
            phase: "completed".into(),
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
            Constraint::Length(5),       // firehose
            Constraint::Length(1),       // status line
        ])
        .split(area);

    render_header(f, app, chunks[0]);
    render_tabs(f, app, chunks[1]);
    match app.mode {
        ViewMode::CommandCenter => render_command_center(f, app, chunks[2]),
        ViewMode::SessionTree => render_placeholder(f, chunks[2], "Session Tree", "F2"),
        ViewMode::SessionDetail => render_placeholder(f, chunks[2], "Session Detail", "F3"),
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
    let line = Line::from(vec![
        Span::styled("mu", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" — command center  "),
        Span::raw("●  uptime "),
        Span::raw(fmt_duration(app.daemon_uptime)),
        Span::raw(format!("  events {}.{}k  ", app.event_count / 1000, (app.event_count / 100) % 10)),
        Span::raw("budget "),
        Span::styled(
            format!("${:.2}/${:.2}", used, budget),
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
            let header = Line::from(vec![
                Span::styled(format!("{} ", s.status.glyph()), s.status.style()),
                Span::styled(
                    format!("{:<7}", s.short_id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(s.title.clone()),
            ]);
            let detail = Line::from(vec![
                Span::raw("    "),
                Span::styled(s.model.clone(), Style::default().fg(Color::DarkGray)),
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
        vec![
            Line::from(vec![
                Span::styled("session ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    s.short_id.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(s.title.clone()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("phase:    ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    s.phase.clone(),
                    Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
                ),
            ]),
            Line::from(vec![
                Span::styled("model:    ", Style::default().fg(Color::DarkGray)),
                Span::raw(s.model.clone()),
            ]),
            Line::from(vec![
                Span::styled("cost:     ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("${:.2}", s.cost_usd)),
            ]),
            Line::from(vec![
                Span::styled("context:  ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{}k active / cached", s.tokens_kilo)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "(post-mu-035: 'phase' comes from session.provider_status; for now mocked)",
                Style::default().fg(Color::DarkGray),
            )),
        ]
    } else {
        vec![Line::from("(no session selected)")]
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Selected session ");
    let paragraph = Paragraph::new(detail_text).block(block).wrap(Wrap { trim: false });
    f.render_widget(paragraph, h[1]);
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
    let lines: Vec<Line> = app
        .firehose
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .rev()
        .map(|s| Line::from(s.as_str()))
        .collect();
    let block = Block::default().borders(Borders::ALL).title(" Firehose ");
    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_statusline(f: &mut Frame, app: &App, area: Rect) {
    let content = if app.command_mode {
        format!(":{}", app.command_buffer)
    } else {
        format!(
            " mode: {}   keys: F1-F9 switch · j/k or ↑↓ select session · : palette · q quit",
            app.mode.name()
        )
    };
    let style = if app.command_mode {
        Style::default().fg(Color::Black).bg(Color::Yellow)
    } else {
        Style::default().fg(Color::Black).bg(Color::Gray)
    };
    let line = Paragraph::new(content).style(style);
    f.render_widget(line, area);
}

// ── Main loop ───────────────────────────────────────────────────────

fn main() -> Result<()> {
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run(&mut terminal);

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

fn run<B: Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    let mut app = App::new();
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
