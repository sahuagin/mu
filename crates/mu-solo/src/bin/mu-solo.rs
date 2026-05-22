//! mu-solo binary entrypoint.
//!
//! Intentionally thin: arg parsing, terminal init/teardown, run the
//! App. All real logic lives in the library (`mu_solo::App`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use mu_solo::app::{make_inline_terminal, App};

#[derive(Parser, Debug)]
#[command(name = "mu-solo", about = "standalone single-pane chat TUI for mu serve")]
struct Cli {
    /// Path to the `mu` binary (the daemon to spawn).
    #[arg(long, default_value = "./target/release/mu")]
    mu_binary: String,

    /// Working directory for the spawned daemon. Defaults to the
    /// current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Provider to use for the initial session. Default is
    /// openai-codex/gpt-5.5 because that path is subscription-billed
    /// (no per-token cost) for solo development. Override with
    /// --provider anthropic / --model claude-haiku-4-5 etc. when you
    /// want a different provider.
    #[arg(long, default_value = "openai-codex")]
    provider: String,

    /// Model to use for the initial session.
    #[arg(long, default_value = "gpt-5.5")]
    model: String,

    /// Auto-approve bash invocations (convenience for solo dev).
    #[arg(long)]
    bash_yolo: bool,

    /// Comma-separated list of tools to register with the daemon.
    /// Without this, the session has zero tools and the model is
    /// likely to hallucinate tool-call syntax as text. The default
    /// covers the standard coding-agent set; pass an empty string
    /// (`--tools ""`) for a strictly-text session.
    #[arg(long, default_value = "read,write,edit,glob,grep,bash")]
    tools: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = cli
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));

    // Build the app FIRST (spawns daemon, creates session). Errors here
    // shouldn't leave the terminal in a weird state.
    let mut app = App::new(
        &cli.mu_binary,
        &cwd,
        &cli.provider,
        &cli.model,
        cli.bash_yolo,
        &cli.tools,
    )
    .context("App::new failed (is the mu binary path correct?)")?;

    // Enter raw mode for ratatui inline rendering.
    enable_raw_mode().context("enable_raw_mode")?;
    let mut terminal = make_inline_terminal()?;

    let run_result = app.run(&mut terminal);

    // Always restore the terminal, even on error.
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), crossterm::cursor::Show);

    run_result
}
