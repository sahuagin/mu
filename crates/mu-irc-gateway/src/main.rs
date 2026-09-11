//! `mu-irc-gateway` — the daemon.
//!
//! It does four things: read the configuration, start the bridge, translate
//! SIGINT/SIGTERM into a clean shutdown, and report failures in a way an
//! operator can act on. Everything else lives in the library.
//!
//! A clean shutdown is not just "exit": the gateway sends an IRC `QUIT` so the
//! channels it was in see it leave, and releases every `human:<nick>` endpoint
//! it fronted so those peers stop answering `$SRV` discovery. Skipping the
//! second is what leaves phantom peers on the mesh.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use mu_irc_gateway::bridge;
use mu_irc_gateway::config::{self, GatewayConfig};

/// A standalone single-nick IRC frontend to the mu agent mesh.
#[derive(Debug, Parser)]
#[command(
    name = "mu-irc-gateway",
    version,
    about = "Mirror the mu agent mesh into one IRC nick, and back",
    long_about = "Joins one IRC server as one nick and mirrors between IRC and the NATS agent \
mesh: mesh agents appear as channels, DMs to the humans it fronts appear as messages, and what \
those humans type is published to the mesh.\n\nDelivery is live and best-effort — the gateway \
holds subscriptions while it runs and mirrors between them. It is not a store, not a replay \
buffer, and no part of any durable-wake path; the mu daemon is unaware it exists."
)]
struct Cli {
    /// Config file holding `[irc]` and `[dialogue.mesh]`.
    ///
    /// Defaults to `$MU_CONFIG`, else `~/.config/mu/config.toml`.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// File holding the fleet-wide `[mesh]` section that unset mesh fields
    /// (`nats_url`, `issuer_key`) inherit from.
    ///
    /// Defaults to `$MU_CONFIG` when set — which collapses everything into one
    /// file — else `~/.config/agent/config.toml`, where the shared agent config
    /// keeps `[mesh]`. A file that does not exist simply contributes nothing.
    #[arg(long, value_name = "PATH")]
    fleet_config: Option<PathBuf>,

    /// Load and validate the configuration, print what it resolved to (secrets
    /// redacted), and exit without connecting to anything.
    #[arg(long)]
    check_config: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);
    let fleet_path = cli.fleet_config.clone().unwrap_or_else(default_fleet_path);

    let config = match config::load(&config_path, &fleet_path) {
        Ok(config) => config,
        Err(e) => {
            // The config errors are built to be safe to print: they name a
            // field or a path, never a credential value.
            error!("configuration ({}): {e}", config_path.display());
            return ExitCode::from(2);
        }
    };

    if cli.check_config {
        // `GatewayConfig`'s Debug redacts the SASL password, the issuer key and
        // any userinfo in the NATS URL.
        println!("config: {}", config_path.display());
        println!("fleet:  {}", fleet_path.display());
        println!("{config:#?}");
        return ExitCode::SUCCESS;
    }

    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Build the runtime here rather than with `#[tokio::main]`, so `--help`,
/// `--version` and a configuration failure never start one.
fn run(config: GatewayConfig) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let (tx, rx) = watch::channel(false);
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::spawn(async move {
            let signal = tokio::select! {
                _ = sigint.recv() => "SIGINT",
                _ = sigterm.recv() => "SIGTERM",
            };
            info!("{signal}: quitting IRC and releasing mesh presence");
            // A dropped receiver means the bridge already stopped.
            let _ = tx.send(true);
        });
        bridge::run(config, rx).await
    })
}

/// Where the fleet-wide `[mesh]` section is read from when no `--fleet-config`
/// is given.
///
/// `$MU_CONFIG` overrides every section for every mu tool, so when it is set it
/// is also the fleet file. Otherwise `~/.config/agent/config.toml`: `[mesh]` is
/// shared by every agent on the box, not mu-specific, which is why the mesh
/// loader takes two paths in the first place.
fn default_fleet_path() -> PathBuf {
    if let Ok(path) = std::env::var("MU_CONFIG") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".config/agent/config.toml")
}
