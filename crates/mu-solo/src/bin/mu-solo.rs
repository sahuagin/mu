//! mu-solo binary entrypoint.
//!
//! Intentionally thin: load layered config, parse args (sparse
//! overrides), terminal init/teardown, run the App. All real logic
//! lives in the library (`mu_solo::App`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use mu_solo::app::{make_inline_terminal, App};
use mu_solo::config::{self, CliOverrides};

/// CLI flags. Every override flag is Optional so we can distinguish
/// "user explicitly set X" from "fall through to TOML / env / default."
/// figment merges defaults + ~/.config/mu/solo.toml + env (MU_SOLO_*);
/// these flags override on top.
#[derive(Parser, Debug)]
#[command(
    name = "mu-solo",
    about = "standalone single-pane chat TUI for mu serve"
)]
struct Cli {
    /// Path to an alternate config file. Default:
    /// `$XDG_CONFIG_HOME/mu/solo.toml` (i.e. `~/.config/mu/solo.toml`).
    /// Missing file is fine; malformed TOML is an error.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Path to the `mu` daemon binary. Overrides config.session.mu_binary.
    #[arg(long)]
    mu_binary: Option<String>,

    /// Working directory for the spawned daemon. Overrides
    /// config.session.cwd. Default = current process cwd.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Provider for the initial session. Overrides config.session.provider.
    #[arg(long)]
    provider: Option<String>,

    /// Model id for the initial session. Overrides config.session.model.
    #[arg(long)]
    model: Option<String>,

    /// Auto-approve bash invocations. When passed, sets
    /// config.session.bash_yolo = true. No way to negate from the CLI
    /// in v0 — set bash_yolo = false in solo.toml if you want it off
    /// and the default flips.
    #[arg(long)]
    bash_yolo: bool,

    /// Comma-separated tools to register on the daemon. Overrides
    /// config.session.tools.
    #[arg(long)]
    tools: Option<String>,

    /// Initial /effort value: low|medium|high|xhigh|max. Overrides
    /// config.tui.effort.
    #[arg(long)]
    effort: Option<String>,

    /// Start with /focus mode on. Sets config.tui.focus_mode = true.
    #[arg(long)]
    focus: bool,
}

impl Cli {
    fn to_overrides(&self) -> CliOverrides {
        CliOverrides {
            provider: self.provider.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            // Presence-only flags map to Some(true) when passed,
            // None otherwise — so a lower-precedence layer (TOML)
            // can still set them true if the CLI flag is absent.
            bash_yolo: if self.bash_yolo { Some(true) } else { None },
            mu_binary: self.mu_binary.clone(),
            cwd: self.cwd.clone(),
            effort: self.effort.clone(),
            focus_mode: if self.focus { Some(true) } else { None },
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = config::load(cli.config.as_deref())
        .context("failed to load mu-solo config")?;
    config::apply_cli_overrides(&mut cfg, &cli.to_overrides());

    // Resolve cwd once: None ⇒ current process cwd. Held here (not in
    // the config struct after resolution) because the resolution time
    // is the binary's responsibility — App::new takes a concrete &Path.
    let cwd = cfg
        .session
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));

    // Build the app FIRST (spawns daemon, creates session). Errors
    // here shouldn't leave the terminal in a weird state.
    let mut app = App::new(
        &cfg.session.mu_binary,
        &cwd,
        &cfg.session.provider,
        &cfg.session.model,
        cfg.session.bash_yolo,
        &cfg.session.tools,
        &cfg.tui.effort,
        cfg.tui.focus_mode,
    )
    .context("App::new failed (is the mu binary path correct?)")?;

    // Enter raw mode + bracketed paste for ratatui inline rendering.
    enable_raw_mode().context("enable_raw_mode")?;
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let terminal = make_inline_terminal()?;

    let run_result = app.run(terminal);

    // Always restore the terminal, even on error.
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), crossterm::cursor::Show);

    run_result
}
