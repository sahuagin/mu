//! `mu` — the coding agent binary.
//!
//! One binary, multiple modes. `mu serve` is the JSON-RPC core daemon;
//! every other subcommand is a frontend that owns one or more daemons.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "mu",
    about = "Coding agent. mu serve is the daemon; everything else is a frontend.",
    version,
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
        /// write, ls.
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
        } => {
            let factory = mu_coding::serve::make_provider_factory(ephemeral, thinking);
            let tool_names = mu_coding::serve::parse_tools_csv(&tools);
            let tool_vec = mu_coding::serve::build_tools(&tool_names)?;
            mu_coding::serve::run(factory, tool_vec).await
        }
        Command::Ask {
            prompt,
            provider,
            model,
            tools,
            ephemeral,
            thinking,
        } => mu_coding::ask::run(prompt, provider, model, tools, ephemeral, thinking).await,
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
        other => anyhow::bail!(
            "unknown provider for login: {other}. Supported: openai-codex."
        ),
    }
}

fn run_logout(provider: &str) -> Result<()> {
    use mu_ai::auth::{FileSystemTokenStore, TokenStore};
    let store = FileSystemTokenStore::default_location()?;
    store.remove(provider)?;
    println!("Removed stored credentials for {provider} (if any).");
    Ok(())
}
