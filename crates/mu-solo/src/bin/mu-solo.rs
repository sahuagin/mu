//! mu-solo binary entrypoint.
//!
//! Intentionally thin: load layered config, parse args (sparse
//! overrides), terminal init/teardown, run the App. All real logic
//! lives in the library (`mu_solo::App`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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

    /// Extended-thinking effort forwarded to `mu serve`:
    /// low|medium|high|xhigh|max (alias `minimal` = low; `off`/`none`/
    /// `disabled` to turn off). Anthropic-only. Overrides
    /// config.session.thinking. Prefer setting `[session] thinking` in
    /// solo.toml so you don't pass it each run.
    #[arg(long)]
    thinking: Option<String>,

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
            thinking: self.thinking.clone(),
            cwd: self.cwd.clone(),
            effort: self.effort.clone(),
            focus_mode: if self.focus { Some(true) } else { None },
            clipboard_command: None,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // mu-solo owns the terminal, so its logs go to a FILE (the daemon
    // writes stderr; a TUI can't). Default ~/.local/share/mu/solo/solo.log
    // (override MU_SOLO_LOG), honoring RUST_LOG (default mu_solo=info). In a
    // `release` build debug!/trace! are compiled out; use the `debugrelease`
    // profile + RUST_LOG=mu_solo=debug to see them. Best-effort.
    init_solo_tracing();
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

    // mu-n25a: parse the side-effects ceiling before building the app so
    // a typo in `[session] max_side_effects` is a clean startup error
    // rather than a silent fall-through to unrestricted.
    let max_side_effects = cfg
        .session
        .max_side_effects_capability()
        .context("invalid [session] max_side_effects in solo config")?;

    // Build the app FIRST (spawns daemon, creates session). Errors
    // here shouldn't leave the terminal in a weird state.
    let mut app = App::new(AppOptions {
        mu_binary: &cfg.session.mu_binary,
        cwd: &cwd,
        provider: &cfg.session.provider,
        model: &cfg.session.model,
        bash_yolo: cfg.session.bash_yolo,
        tools: &cfg.session.tools,
        thinking: &cfg.session.thinking,
        effort: &cfg.tui.effort,
        focus_mode: cfg.tui.focus_mode,
        cache_ttl: &cfg.session.cache_ttl,
        clipboard_command: cfg.tui.clipboard_command.as_deref(),
        renderer_journal: cfg.tui.renderer_journal,
        notifications: cfg.tui.notifications,
        // mu-7e21: [autonomy] in solo.toml → create_session grant.
        autonomy: cfg.autonomy.to_capability(),
        // mu-n25a: [session] max_side_effects → create_session ceiling.
        max_side_effects,
    })
    .context("App::new failed (is the mu binary path correct?)")?;

    // Enter raw mode + bracketed paste for inline rendering.
    enable_raw_mode().context("enable_raw_mode")?;
    // mu-mu-solo-loop-terminate-5ek5: restore the terminal on EVERY
    // exit path — including a panic unwinding out of `app.run()`,
    // which previously skipped the restore lines below and left the
    // terminal raw (the worst part of a hung/dying session). Drop
    // runs on unwind; the explicit restore below stays as the normal
    // path and double-restoring is harmless (all calls are no-op
    // idempotent).
    let _restore_guard = TerminalRestoreGuard;
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
    // mu-solo-osc-notify-mbmn: focus reporting gates desktop
    // notifications (notify only when unfocused). Terminals without
    // the feature ignore the request and the app stays silent —
    // terminal_focused starts true and never flips.
    let _ = execute!(std::io::stdout(), EnableFocusChange);

    let run_result = app.run().await;

    // Always restore the terminal, even on error. Pop the keyboard
    // protocol BEFORE disabling raw mode (mu-tui precedent:
    // leave_terminal_mode); errors ignored — terminals that no-op'd
    // the push no-op the pop too.
    let _ = execute!(std::io::stdout(), DisableFocusChange);
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), crossterm::cursor::Show);

    // mu-mu-solo-loop-terminate-5ek5: tear the daemon down AFTER the
    // terminal is restored (restore is the priority on a bad day) —
    // bounded by construction (stdin-EOF grace then SIGKILL), so quit
    // can never hang here even when the daemon is wedged.
    app.shutdown_daemon();

    run_result
}

/// Install a file-writing tracing subscriber for mu-solo. Mirrors the
/// daemon's subscriber (`mu serve`) but writes to a file rather than stderr,
/// because the TUI owns the terminal. Path = `MU_SOLO_LOG` or
/// `~/.local/share/mu/solo/solo.log`; filter from `RUST_LOG`, default
/// `mu_solo=info`. Every failure is swallowed — logging setup must never
/// stop the app from starting.
fn init_solo_tracing() {
    let path = match std::env::var_os("MU_SOLO_LOG") {
        Some(p) => PathBuf::from(p),
        None => match dirs::data_dir() {
            Some(d) => d.join("mu").join("solo").join("solo.log"),
            None => return,
        },
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("mu_solo=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .try_init();
}

/// Restores the terminal when dropped — including on panic unwind.
/// Mirrors the explicit restore sequence in `main`; every operation
/// is idempotent so running both is fine.
struct TerminalRestoreGuard;

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableFocusChange);
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), crossterm::cursor::Show);
    }
}
