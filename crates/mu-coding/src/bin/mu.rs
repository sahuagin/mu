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
    Serve {
        /// Provider backend. Values: faux, anthropic-api.
        #[arg(long, default_value = "faux")]
        provider: String,
        /// Model id (provider-specific). For anthropic-api, defaults
        /// to claude-haiku-4-5-20251001 if unset.
        #[arg(long)]
        model: Option<String>,
        /// Comma-separated list of tools to enable. Values: read.
        #[arg(long, default_value = "")]
        tools: String,
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
    },
    /// Interactive terminal UI.
    Tui,
    /// Orchestrator — spawn N daemons and coordinate.
    Orchestrate {
        /// Path to a plan.toml describing the task graph.
        plan: std::path::PathBuf,
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
            provider,
            model,
            tools,
        } => {
            let provider_arc =
                mu_coding::serve::build_provider(&provider, model.as_deref())?;
            let tool_names = mu_coding::serve::parse_tools_csv(&tools);
            let tool_vec = mu_coding::serve::build_tools(&tool_names)?;
            mu_coding::serve::run(provider_arc, tool_vec).await
        }
        Command::Ask {
            prompt,
            provider,
            model,
            tools,
        } => mu_coding::ask::run(prompt, provider, model, tools).await,
        Command::Tui | Command::Orchestrate { .. } => {
            anyhow::bail!(
                "this subcommand is not yet implemented; mu is pre-MVP. \
                 Try `mu serve` or `mu ask <prompt>` for what's working."
            )
        }
    }
}
