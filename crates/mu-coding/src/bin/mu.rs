//! `mu` — the coding agent binary.
//!
//! One binary, multiple modes. `mu serve` is the JSON-RPC core daemon;
//! every other subcommand is a frontend that owns one or more daemons.

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use mu_ai::FauxProvider;
use mu_core::agent::Provider;

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
    Serve,
    /// One-shot ask — spawn the daemon, single roundtrip, exit.
    Ask {
        /// The prompt to send.
        prompt: String,
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
        Command::Serve => {
            // v1: hardcoded FauxProvider::echo. Real provider selection
            // is a future spec.
            let provider: Arc<dyn Provider> = Arc::new(FauxProvider::echo());
            mu_coding::serve::run(provider).await
        }
        Command::Ask { .. } | Command::Tui | Command::Orchestrate { .. } => {
            anyhow::bail!(
                "this subcommand is not yet implemented; mu is pre-MVP. \
                 Try `mu serve` for the JSON-RPC daemon, or `mu versions` to \
                 confirm the workspace builds."
            )
        }
    }
}
