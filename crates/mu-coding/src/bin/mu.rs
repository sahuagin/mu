//! `mu` — the coding agent binary.
//!
//! One binary, multiple modes. `mu serve` is the JSON-RPC core daemon;
//! every other subcommand is a frontend that owns one or more daemons.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "mu",
    about = "Coding agent. mu serve is the daemon; everything else is a frontend.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// JSON-RPC core daemon over stdio.
    ///
    /// As of mu-020, the daemon does not take a `--provider` flag —
    /// providers are constructed per-session from each
    /// `create_session.provider` request. `--ephemeral` and
    /// `--thinking` parameterize HOW providers are built across all
    /// sessions on this daemon.
    Serve {
        /// Comma-separated list of tools to enable. Values: read,
        /// write, ls, edit, grep, glob, bash.
        #[arg(long, default_value = "")]
        tools: String,
        /// For OAuth providers (openai-codex): load stored token but
        /// don't persist refreshed tokens back to disk.
        #[arg(long)]
        ephemeral: bool,
        /// Reasoning effort: minimal | low | medium | high. Only
        /// affects providers with a reasoning surface (openai-codex
        /// today); ignored elsewhere.
        #[arg(long)]
        thinking: Option<String>,
        /// Bash tool: YOLO MODE. Bypass allowlist + metachar
        /// rejection + env scrub; spawn via `bash -c`. Only enable
        /// for sessions you fully trust the prompt source of.
        #[arg(long)]
        bash_yolo: bool,
        /// Bash tool: extend the default allowlist with these
        /// commands. Repeatable. Each entry is parsed via shlex
        /// and token-prefix matched against incoming commands.
        /// Ignored when --bash-yolo is set.
        #[arg(long = "bash-allow", value_name = "CMD")]
        bash_allow: Vec<String>,
        /// Bash tool: strict mode also requires per-call user
        /// approval via session.input_required (mu-029). Allowlist
        /// still gates first; approval prompts on every allowlisted
        /// command. Ignored when --bash-yolo is set.
        #[arg(long)]
        bash_prompt: bool,
    },
    /// One-shot ask — spawn the daemon, single roundtrip, exit.
    Ask {
        /// The prompt to send.
        prompt: String,
        /// Provider backend (forwarded to the spawned `mu serve`).
        #[arg(long, default_value = "faux")]
        provider: String,
        /// Model id (forwarded to the spawned `mu serve`).
        #[arg(long)]
        model: Option<String>,
        /// Comma-separated list of tools (forwarded).
        #[arg(long, default_value = "")]
        tools: String,
        /// Forwarded as `--ephemeral` to `mu serve`. See `mu serve --help`.
        #[arg(long)]
        ephemeral: bool,
        /// Forwarded as `--thinking` to `mu serve`. See `mu serve --help`.
        #[arg(long)]
        thinking: Option<String>,
        /// Forwarded as `--bash-yolo` to `mu serve`. See `mu serve --help`.
        #[arg(long)]
        bash_yolo: bool,
        /// Forwarded as `--bash-allow` to `mu serve` (repeatable).
        #[arg(long = "bash-allow", value_name = "CMD")]
        bash_allow: Vec<String>,
        /// Forwarded as `--bash-prompt` to `mu serve`.
        #[arg(long)]
        bash_prompt: bool,
        /// Read FILE and use its contents as the session's system
        /// prompt (sent as `CreateSessionRequest.system_prompt`).
        /// Today this overrides the daemon default rather than
        /// appending to it; flag is named for compatibility with
        /// pi's `--append-system-prompt` so callers (e.g.
        /// `agent-spawn-v2`) can substitute mu for pi without
        /// changing flags. Errors if FILE cannot be read (mu-x83o).
        #[arg(long = "append-system-prompt", value_name = "FILE")]
        append_system_prompt: Option<std::path::PathBuf>,
    },
    /// Interactive terminal UI.
    Tui,
    /// Orchestrator — spawn N daemons and coordinate.
    Orchestrate {
        /// Path to a plan.toml describing the task graph.
        plan: std::path::PathBuf,
    },
    /// Sign in to a provider via OAuth. Persists the token bundle
    /// under `~/.config/mu/auth/<provider>.json` (mode 0600).
    Login {
        /// Provider to sign in to. Values: openai-codex.
        #[arg(long, default_value = "openai-codex")]
        provider: String,
    },
    /// Forget stored OAuth credentials for a provider.
    Logout {
        /// Provider to forget. Values: openai-codex.
        #[arg(long, default_value = "openai-codex")]
        provider: String,
    },
    /// Print the version of each crate (smoke test for the workspace).
    Versions,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        // Logs MUST go to stderr — stdout is the JSON-RPC channel for
        // `mu serve`, so any log line on stdout corrupts the protocol
        // and crashes the parent (`mu ask`). This was caught when
        // adding the --bash-yolo startup WARN — same fix would have
        // been needed eventually for any non-quiet daemon log.
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Versions => {
            println!("mu-core    {}", mu_core::version());
            println!("mu-ai      {}", mu_ai::version());
            println!("mu-coding  {}", mu_coding::version());
            Ok(())
        }
        Command::Serve {
            tools,
            ephemeral,
            thinking,
            bash_yolo,
            bash_allow,
            bash_prompt,
        } => {
            let factory = mu_coding::serve::make_provider_factory(ephemeral, thinking);
            let tool_names = mu_coding::serve::parse_tools_csv(&tools);
            let bash_settings = mu_coding::serve::BashSettings {
                yolo: bash_yolo,
                extra_allow: bash_allow,
                prompt: bash_prompt,
            };
            let tool_vec = mu_coding::serve::build_tools(&tool_names, &bash_settings)?;
            mu_coding::serve::run(factory, tool_vec).await
        }
        Command::Ask {
            prompt,
            provider,
            model,
            tools,
            ephemeral,
            thinking,
            bash_yolo,
            bash_allow,
            bash_prompt,
            append_system_prompt,
        } => {
            let system_prompt = match append_system_prompt {
                Some(path) => Some(std::fs::read_to_string(&path).with_context(|| {
                    format!("--append-system-prompt: reading {}", path.display())
                })?),
                None => None,
            };
            mu_coding::ask::run(mu_coding::ask::AskOptions {
                prompt,
                provider,
                model,
                tools,
                ephemeral,
                thinking,
                bash_yolo,
                bash_allow,
                bash_prompt,
                system_prompt,
            })
            .await
        }
        Command::Login { provider } => run_login(&provider).await,
        Command::Logout { provider } => run_logout(&provider),
        Command::Tui | Command::Orchestrate { .. } => {
            anyhow::bail!(
                "this subcommand is not yet implemented; mu is pre-MVP. \
                 Try `mu serve` or `mu ask <prompt>` for what's working."
            )
        }
    }
}

async fn run_login(provider: &str) -> Result<()> {
    use mu_ai::auth::{FileSystemTokenStore, TokenStore};
    match provider {
        "openai-codex" => {
            let token = mu_ai::auth::openai_codex::login_flow().await?;
            let store = FileSystemTokenStore::default_location()?;
            store.save(provider, &token)?;
            println!("Signed in to {provider}. Token saved.");
            Ok(())
        }
        other => anyhow::bail!("unknown provider for login: {other}. Supported: openai-codex."),
    }
}

fn run_logout(provider: &str) -> Result<()> {
    use mu_ai::auth::{FileSystemTokenStore, TokenStore};
    let store = FileSystemTokenStore::default_location()?;
    store.remove(provider)?;
    println!("Removed stored credentials for {provider} (if any).");
    Ok(())
}
