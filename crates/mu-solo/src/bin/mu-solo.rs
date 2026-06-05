//! mu-solo binary entrypoint.
//!
//! Intentionally thin: load layered config, parse args (sparse
//! overrides), terminal init/teardown, run the App. All real logic
//! lives in the library (`mu_solo::App`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use mu_solo::app::{App, AppOptions};
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
            clipboard_command: None,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = config::load(cli.config.as_deref()).context("failed to load mu-solo config")?;
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
    let mut app = App::new(AppOptions {
        mu_binary: &cfg.session.mu_binary,
        cwd: &cwd,
        provider: &cfg.session.provider,
        model: &cfg.session.model,
        bash_yolo: cfg.session.bash_yolo,
        tools: &cfg.session.tools,
        effort: &cfg.tui.effort,
        focus_mode: cfg.tui.focus_mode,
        cache_ttl: &cfg.session.cache_ttl,
        clipboard_command: cfg.tui.clipboard_command.as_deref(),
        renderer_journal: cfg.tui.renderer_journal,
    })
    .context("App::new failed (is the mu binary path correct?)")?;

    // Enter raw mode + bracketed paste for inline rendering.
    enable_raw_mode().context("enable_raw_mode")?;
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    // mu-solo-shift-enter-62tx: opt into the Kitty Keyboard Protocol so
    // modified Enter (Shift-Enter etc.) reaches the app as a distinct
    // key instead of collapsing to plain CR at the terminal layer.
    // Terminals without the feature silently ignore the push, so this
    // is benign everywhere (mu-tui precedent: enter_terminal_mode).
    let _ = execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );

    let run_result = app.run().await;

    // Always restore the terminal, even on error. Pop the keyboard
    // protocol BEFORE disabling raw mode (mu-tui precedent:
    // leave_terminal_mode); errors ignored — terminals that no-op'd
    // the push no-op the pop too.
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), crossterm::cursor::Show);

    run_result
}
