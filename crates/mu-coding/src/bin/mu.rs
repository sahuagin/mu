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
    /// Interactive terminal UI. Delegates to the `mu-tui` binary
    /// (resolved next to the `mu` binary, falling back to `$PATH`).
    /// Any arguments after `tui` are forwarded to `mu-tui` unchanged,
    /// including `--help` and `--version`. (mu-yvvz)
    #[command(disable_help_flag = true)]
    Tui {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
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
    /// Telemetry projection + preset analytics queries over the
    /// `TaskTelemetry` event log (spec mu-042, bead mu-8ypx).
    Analytics {
        #[command(subcommand)]
        cmd: AnalyticsCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AnalyticsCmd {
    /// Project `TaskTelemetry` events from per-session JSONL files into the
    /// sqlite sink. Idempotent — re-running re-classifies & UPSERTs.
    Compact {
        /// Path to the events directory. Default:
        /// `~/.local/share/mu/events/`.
        #[arg(long, value_name = "PATH")]
        events_dir: Option<std::path::PathBuf>,
        /// Path to the analytics DB. Default:
        /// `~/.local/share/mu/telemetry.sqlite`.
        #[arg(long, value_name = "PATH")]
        db: Option<std::path::PathBuf>,
        /// Only project tasks with `ended_at_unix_ms >= SINCE`. Default:
        /// 0 (all).
        #[arg(long, value_name = "UNIX_MS")]
        since: Option<u64>,
    },
    /// Print totals + breakdowns by exit_reason, provider+model, outcome.
    Summary {
        #[arg(long, value_name = "PATH")]
        db: Option<std::path::PathBuf>,
        #[arg(long, value_name = "UNIX_MS")]
        since: Option<u64>,
    },
    /// Print a rate (hallucination only in v1) grouped by provider+model.
    Rate {
        #[arg(long, default_value = "hallucination", value_name = "METRIC")]
        metric: String,
        #[arg(long, value_name = "PATH")]
        db: Option<std::path::PathBuf>,
        #[arg(long, value_name = "UNIX_MS")]
        since: Option<u64>,
    },
    /// Documentary historical inserts: pre-classified task entries from
    /// a TOML file (or bundled preset). Bypasses the classifier — use
    /// only for ground-truth historical data, not live tasks (spec
    /// mu-043, bead mu-mk9l).
    Backfill {
        /// Name of a bundled preset. Currently supported:
        /// `overnight-2026-05-16`. Mutually exclusive with `--input`.
        #[arg(long, value_name = "NAME")]
        preset: Option<String>,
        /// Path to an external TOML file. Mutually exclusive with
        /// `--preset`.
        #[arg(long, value_name = "PATH")]
        input: Option<std::path::PathBuf>,
        #[arg(long, value_name = "PATH")]
        db: Option<std::path::PathBuf>,
    },
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
            let mut tool_vec = mu_coding::serve::build_tools(&tool_names, &bash_settings)?;

            // Try to connect to code-index-lsp for index_recall tool.
            // Non-fatal: if no server is available, we just don't register the tool.
            let lsp_addr = std::env::var("CODE_INDEX_LSP")
                .unwrap_or_else(|_| "127.0.0.1:7621".to_string());
            match mu_core::lsp_client::LspClient::connect(&lsp_addr).await {
                Ok(client) => {
                    let client = std::sync::Arc::new(client);
                    tracing::info!(
                        server = client.server_name(),
                        addr = %lsp_addr,
                        "connected to code-index-lsp; registering index_recall tool"
                    );
                    tool_vec.push(std::sync::Arc::new(
                        mu_coding::tools::IndexRecallTool::new(client),
                    ));
                }
                Err(e) => {
                    tracing::debug!(
                        addr = %lsp_addr,
                        error = %e,
                        "code-index-lsp not available; index_recall tool disabled"
                    );
                }
            }

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
        Command::Tui { args } => exec_mu_tui(args),
        Command::Orchestrate { .. } => {
            anyhow::bail!(
                "`mu orchestrate` is not yet implemented; mu is pre-MVP. \
                 The interactive TUI is available via `mu tui` (delegates \
                 to the `mu-tui` binary)."
            )
        }
        Command::Analytics { cmd } => run_analytics(cmd),
    }
}

fn run_analytics(cmd: AnalyticsCmd) -> Result<()> {
    use mu_coding::analytics::{
        compact::compact_dir,
        default_db_path,
        query::{format_rate, format_summary, rate_hallucination, summary},
        sink::open as sink_open,
    };

    fn resolve_db(arg: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
        if let Some(p) = arg {
            return Ok(p);
        }
        default_db_path()
            .context("could not resolve default analytics DB path; pass --db PATH explicitly")
    }
    fn resolve_events_dir(arg: Option<std::path::PathBuf>) -> Result<std::path::PathBuf> {
        if let Some(p) = arg {
            return Ok(p);
        }
        mu_coding::serve::default_events_dir()
            .context("could not resolve default events dir; pass --events-dir PATH explicitly")
    }

    match cmd {
        AnalyticsCmd::Compact {
            events_dir,
            db,
            since,
        } => {
            let db_path = resolve_db(db)?;
            let ev_dir = resolve_events_dir(events_dir)?;
            let conn = sink_open(&db_path)
                .with_context(|| format!("opening sink at {}", db_path.display()))?;
            let summary = compact_dir(&conn, &ev_dir, since)?;
            println!(
                "compacted: {} file(s), {} line(s), {} task(s) upserted, \
                 {} malformed, {} filtered",
                summary.files_scanned,
                summary.lines_read,
                summary.tasks_upserted,
                summary.malformed_lines_skipped,
                summary.tasks_filtered_out
            );
            Ok(())
        }
        AnalyticsCmd::Summary { db, since } => {
            let db_path = resolve_db(db)?;
            let conn = sink_open(&db_path)?;
            let s = summary(&conn, since)?;
            print!("{}", format_summary(&s));
            Ok(())
        }
        AnalyticsCmd::Rate { metric, db, since } => {
            if metric != "hallucination" {
                anyhow::bail!("unsupported --metric '{metric}'. v1 supports: hallucination.");
            }
            let db_path = resolve_db(db)?;
            let conn = sink_open(&db_path)?;
            let rows = rate_hallucination(&conn, since)?;
            print!("{}", format_rate(&rows, &metric));
            Ok(())
        }
        AnalyticsCmd::Backfill { preset, input, db } => {
            use mu_coding::analytics::backfill::{
                apply, load_file, parse_str, PRESET_OVERNIGHT_2026_05_16,
            };
            let file = match (preset.as_deref(), input.as_deref()) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("--preset and --input are mutually exclusive")
                }
                (None, None) => {
                    anyhow::bail!("one of --preset NAME or --input PATH is required")
                }
                (Some(name), None) => match name {
                    "overnight-2026-05-16" => parse_str(PRESET_OVERNIGHT_2026_05_16)?,
                    other => {
                        anyhow::bail!("unknown preset '{other}'. v1 supports: overnight-2026-05-16")
                    }
                },
                (None, Some(path)) => load_file(path)?,
            };
            let db_path = resolve_db(db)?;
            let conn = sink_open(&db_path)?;
            let summary = apply(&conn, &file)?;
            println!("backfilled: {} task(s) upserted", summary.tasks_upserted);
            Ok(())
        }
    }
}

/// Hand off to the `mu-tui` binary. Looks first alongside the current
/// `mu` executable (so a local cargo build picks up the matching local
/// `mu-tui`), then falls back to `$PATH`. Uses `exec` so signals, stdio,
/// and exit codes flow through transparently.
///
/// Returns only on failure — on success `exec` replaces this process and
/// never returns. (mu-yvvz)
fn exec_mu_tui(args: Vec<String>) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let candidate = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mu-tui")))
        .filter(|p| p.is_file());

    let mut cmd = match candidate {
        Some(p) => std::process::Command::new(p),
        None => std::process::Command::new("mu-tui"),
    };
    cmd.args(&args);

    let err = cmd.exec();
    anyhow::bail!(
        "could not exec mu-tui: {err}. Make sure the `mu-tui` binary is installed \
         alongside `mu` (e.g. via `cargo install --path crates/mu-tui`) or available on `$PATH`."
    )
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

#[cfg(test)]
mod tests {
    //! `run_analytics` argument-validation tests.
    //!
    //! The analytics subcommand handlers in `run_analytics` delegate
    //! the actual work (file scanning, SQL aggregation, backfill)
    //! to functions in `mu_coding::analytics`, which have their own
    //! unit tests. What's NOT covered elsewhere is the CLI-level
    //! argument-validation that lives in `run_analytics` itself: the
    //! `Backfill` `(preset, input)` XOR rules and the `Rate` metric-
    //! name allowlist. These tests pin the user-facing contracts on
    //! those bail paths (which run before any DB access, so they're
    //! cheap and hermetic).
    use super::*;

    #[test]
    fn backfill_rejects_both_preset_and_input_set() {
        let err = run_analytics(AnalyticsCmd::Backfill {
            preset: Some("overnight-2026-05-16".into()),
            input: Some("/dev/null".into()),
            db: None,
        })
        .expect_err("both flags set must error");
        assert!(
            err.to_string().contains("mutually exclusive"),
            "expected mutually-exclusive message, got: {err}"
        );
    }

    #[test]
    fn backfill_rejects_neither_preset_nor_input() {
        let err = run_analytics(AnalyticsCmd::Backfill {
            preset: None,
            input: None,
            db: None,
        })
        .expect_err("neither flag set must error");
        let msg = err.to_string();
        assert!(
            msg.contains("--preset") && msg.contains("--input"),
            "expected message naming both flags, got: {msg}"
        );
    }

    #[test]
    fn backfill_rejects_unknown_preset_and_lists_supported() {
        let err = run_analytics(AnalyticsCmd::Backfill {
            preset: Some("not-a-real-preset".into()),
            input: None,
            db: None,
        })
        .expect_err("unknown preset must error");
        let msg = err.to_string();
        // The user sees both the offending name and the supported
        // list — both halves are load-bearing for UX. Pinning the
        // current preset name is intentional: when a future bead
        // adds another preset, this test forces an update so the
        // error stays accurate.
        assert!(
            msg.contains("not-a-real-preset"),
            "error must echo the bad preset name, got: {msg}"
        );
        assert!(
            msg.contains("overnight-2026-05-16"),
            "error must name the supported preset(s), got: {msg}"
        );
    }

    #[test]
    fn rate_rejects_unknown_metric_and_lists_supported() {
        let err = run_analytics(AnalyticsCmd::Rate {
            metric: "not-a-real-metric".into(),
            db: None,
            since: None,
        })
        .expect_err("unknown metric must error");
        let msg = err.to_string();
        assert!(
            msg.contains("not-a-real-metric"),
            "error must echo the bad metric name, got: {msg}"
        );
        assert!(
            msg.contains("hallucination"),
            "error must name the supported metric(s), got: {msg}"
        );
    }
}
