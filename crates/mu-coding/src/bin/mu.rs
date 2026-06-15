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
        /// write, ls, edit, grep, glob, memory_recall, bash.
        #[arg(long, default_value = "")]
        tools: String,
        /// For OAuth providers (openai-codex): load stored token but
        /// don't persist refreshed tokens back to disk.
        #[arg(long)]
        ephemeral: bool,
        /// Reasoning / extended-thinking effort: low | medium | high |
        /// xhigh | max (alias `minimal` = low; `off`/`none`/`disabled`
        /// to turn off). Drives Anthropic extended thinking (adaptive +
        /// summarized) and openai-codex reasoning; ignored by providers
        /// without a reasoning surface.
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
        /// Hermetic daemon: disable session-start recall injection
        /// (memory + project files) AND the discovery bootstrap, so
        /// sessions get no system content mu added on its own. For
        /// gate scripts, benches, and delegated workers. Supersedes
        /// the MU_NO_RECALL env var (which disables recall only).
        /// (mu-mu-bare-flag-fxc8)
        #[arg(long)]
        bare: bool,
    },
    /// One-shot ask — spawn the daemon, single roundtrip, exit.
    Ask {
        /// The prompt to send. Omit when using --prompt-file.
        #[arg(
            required_unless_present = "prompt_file",
            conflicts_with = "prompt_file"
        )]
        prompt: Option<String>,
        /// Read the prompt from FILE instead of argv. Required for
        /// large prompts: a megabyte-scale prompt as a positional
        /// argument overflows the exec ARG_MAX limit before the
        /// process can even start (mu-b6tl — observed live when
        /// ai-review.sh handed a ~1MB review prompt to `mu ask`).
        #[arg(long = "prompt-file", value_name = "FILE")]
        prompt_file: Option<std::path::PathBuf>,
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
        /// Hermetic session: forwarded as `--bare` to the spawned
        /// `mu serve` — no recall injection, no discovery bootstrap;
        /// the session's system prompt is exactly what (if anything)
        /// --append-system-prompt supplies. (mu-mu-bare-flag-fxc8)
        #[arg(long)]
        bare: bool,
    },
    /// Resume a dead session by forking a fresh live head at its last
    /// clean boundary (mu-mh4). STRICT: refuses a ragged log (incomplete
    /// records / unanswered tool calls) with a precise diagnosis and a
    /// `mu recover` hint — it never silently truncates. Accepts the
    /// alias `--resume` too: `mu --resume daemon:session`.
    #[command(alias = "--resume")]
    Resume {
        /// Predecessor session: `daemon:session` or the canonical
        /// `mu:<daemon>/<session>`.
        session_ref: String,
        /// Optional prompt to ask the resumed session immediately.
        /// Omit to just attach the head and print the new session id.
        prompt: Option<String>,
        /// Provider backend for the resumed (live) session.
        #[arg(long, default_value = "faux")]
        provider: String,
        /// Model id for the resumed session.
        #[arg(long)]
        model: Option<String>,
        /// Comma-separated list of tools (forwarded to `mu serve`).
        #[arg(long, default_value = "")]
        tools: String,
        /// Forwarded as `--ephemeral` to `mu serve`.
        #[arg(long)]
        ephemeral: bool,
        /// Forwarded as `--thinking` to `mu serve`.
        #[arg(long)]
        thinking: Option<String>,
        /// Forwarded as `--bash-yolo` to `mu serve`.
        #[arg(long)]
        bash_yolo: bool,
        /// Forwarded as `--bash-allow` to `mu serve` (repeatable).
        #[arg(long = "bash-allow", value_name = "CMD")]
        bash_allow: Vec<String>,
        /// Forwarded as `--bash-prompt` to `mu serve`.
        #[arg(long)]
        bash_prompt: bool,
        /// Forwarded as `--bare` to `mu serve`.
        #[arg(long)]
        bare: bool,
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
    /// Read-only web operator console over mu event logs.
    Console {
        /// Address to bind. Defaults to loopback only.
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: std::net::SocketAddr,
        /// Base path when served behind a reverse proxy (e.g. /mu-console/).
        #[arg(long, default_value = "/")]
        base_path: String,
        /// Path to the events directory. Default: ~/.local/share/mu/events/.
        #[arg(long, value_name = "PATH")]
        events_dir: Option<std::path::PathBuf>,
        /// Path to analytics DB. Default: ~/.local/share/mu/telemetry.sqlite.
        #[arg(long, value_name = "PATH")]
        analytics_db: Option<std::path::PathBuf>,
        /// mu-cc-sessions-console-lqqt.1: also merge claude-code sessions
        /// from ~/.claude-personal/projects/ into the index (read-only).
        #[arg(long)]
        cc_sessions: bool,
        /// Override the claude-code projects dir to scan. Implies
        /// --cc-sessions. Default with --cc-sessions: ~/.claude-personal/projects.
        #[arg(long, value_name = "PATH")]
        cc_projects_dir: Option<std::path::PathBuf>,
        /// mu-cc-sessions-console-lqqt.3: task_log sidecar DB for cc
        /// session marks. Default with --cc-sessions:
        /// ~/.local/share/task_log.sqlite.
        #[arg(long, value_name = "PATH")]
        cc_marks_db: Option<std::path::PathBuf>,
        /// mu-console-hosts-dashboard-zy26: path to the cron-generated
        /// stats HTML served at /dashboard. Default: ~/mu-stats/dashboard.html.
        #[arg(long, value_name = "PATH")]
        dashboard_path: Option<std::path::PathBuf>,
    },
    /// Append an operator quality mark (1-5) to a session's event log.
    /// Quit-time capture for degraded (or excellent) sessions — the
    /// mark is an ordinary event; projections (console header, the
    /// mu-stats session_marks view) take the latest. mu-operator-mark-5mwr.
    Mark {
        /// Session id, or an unambiguous prefix of one.
        session: String,
        /// Quality rating, 1 (unusable) to 5 (excellent).
        rating: u8,
        /// Optional free-form note, e.g. "relitigated settled decisions".
        note: Option<String>,
        /// Path to the events directory. Default: ~/.local/share/mu/events/.
        #[arg(long, value_name = "PATH")]
        events_dir: Option<std::path::PathBuf>,
    },
    /// Print the version of each crate (smoke test for the workspace).
    Versions,
    /// Telemetry projection + preset analytics queries over the
    /// `TaskTelemetry` event log (spec mu-042, bead mu-8ypx).
    Analytics {
        #[command(subcommand)]
        cmd: AnalyticsCmd,
    },
    /// Discover capabilities by intent — the in-process Layer-1 `t4c find`
    /// over mu's manifest (registered tools + discovered skills). Standalone:
    /// builds the manifest in-process, no running daemon required (mu-kex4.6.4).
    Capabilities {
        #[command(subcommand)]
        cmd: CapabilitiesCmd,
    },
    /// Run deterministic process-layer auditors (mu-pr6r.1) over a
    /// session event-log JSONL and print findings. Offline; reads the
    /// file directly (no daemon).
    Audit {
        /// Path to a session event-log JSONL
        /// (~/.local/share/mu/events/<daemon_id>/<session_id>.jsonl).
        #[arg(value_name = "LOG")]
        log: std::path::PathBuf,
    },
    /// List past sessions, newest-first — the discovery surface for
    /// `mu resume` / `mu --recover` (which take an id). Offline; no daemon.
    ///
    /// Cheap by design (mu-lazy-session-rehydration-bh4f): the recency
    /// sort uses file mtime only, and just the most-recent `--last` logs
    /// are opened (first record only), not the whole corpus. The daemon
    /// no longer rehydrates every log at startup, so this is how you find
    /// a session id to resume.
    ListSessions {
        /// Show at most this many of the most-recent sessions.
        #[arg(long, default_value = "20")]
        last: usize,
        /// Show all sessions (overrides --last).
        #[arg(long)]
        all: bool,
        /// Events directory. Default: ~/.local/share/mu/events/.
        #[arg(long, value_name = "PATH")]
        events_dir: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum CapabilitiesCmd {
    /// Rank the manifest against a free-text intent, best-first (JSON).
    Discover {
        /// Free-text intent, e.g. "search file contents" or "track an issue".
        intent: String,
        /// Top-k results.
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Comma-separated tools to include in the manifest.
        #[arg(long, default_value = "read,write,ls,edit,grep,glob,bash")]
        tools: String,
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
        Command::Audit { log } => run_audit(&log),
        Command::ListSessions {
            last,
            all,
            events_dir,
        } => {
            // The side effect (writing to the real stdout) lives here at
            // the call site; run_list_sessions itself just writes to the
            // stream it's handed. (#249 review)
            let stdout = std::io::stdout();
            run_list_sessions(
                &mut stdout.lock(),
                if all { None } else { Some(last) },
                events_dir,
            )
        }
        Command::Serve {
            tools,
            ephemeral,
            thinking,
            bash_yolo,
            bash_allow,
            bash_prompt,
            bare,
        } => {
            let factory = mu_coding::serve::make_provider_factory(ephemeral, thinking);
            let tool_names = mu_coding::serve::parse_tools_csv(&tools);
            let bash_settings = mu_coding::serve::BashSettings {
                yolo: bash_yolo,
                extra_allow: bash_allow,
                prompt: bash_prompt,
            };
            let tool_vec = mu_coding::serve::build_tools(&tool_names, &bash_settings)?;

            mu_coding::serve::run(factory, tool_vec, bare).await
        }
        Command::Ask {
            prompt,
            prompt_file,
            provider,
            model,
            tools,
            ephemeral,
            thinking,
            bash_yolo,
            bash_allow,
            bash_prompt,
            append_system_prompt,
            bare,
        } => {
            let system_prompt = match append_system_prompt {
                Some(path) => Some(std::fs::read_to_string(&path).with_context(|| {
                    format!("--append-system-prompt: reading {}", path.display())
                })?),
                None => None,
            };
            // clap enforces exactly one of prompt / --prompt-file.
            let prompt = match prompt_file {
                Some(path) => std::fs::read_to_string(&path)
                    .with_context(|| format!("--prompt-file: reading {}", path.display()))?,
                None => prompt.expect("clap: prompt required unless --prompt-file"),
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
                bare,
            })
            .await
        }
        Command::Resume {
            session_ref,
            prompt,
            provider,
            model,
            tools,
            ephemeral,
            thinking,
            bash_yolo,
            bash_allow,
            bash_prompt,
            bare,
        } => {
            mu_coding::resume::run(mu_coding::resume::ResumeOptions {
                session_ref,
                prompt,
                provider,
                model,
                tools,
                ephemeral,
                thinking,
                bash_yolo,
                bash_allow,
                bash_prompt,
                bare,
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
        Command::Console {
            bind,
            base_path,
            events_dir,
            analytics_db,
            cc_sessions,
            cc_projects_dir,
            cc_marks_db,
            dashboard_path,
        } => {
            let events_dir = match events_dir {
                Some(p) => p,
                None => mu_coding::serve::default_events_dir().context(
                    "could not resolve default events dir; pass --events-dir PATH explicitly",
                )?,
            };
            let analytics_db = match analytics_db {
                Some(p) => Some(p),
                None => mu_coding::analytics::default_db_path(),
            };
            // mu-cc-sessions-console-lqqt.1: an explicit --cc-projects-dir
            // implies inclusion; bare --cc-sessions uses the default root.
            // Without either flag, cc scanning stays off.
            let cc_projects_dir = match cc_projects_dir {
                Some(p) => Some(p),
                None if cc_sessions => {
                    Some(mu_coding::console::default_cc_projects_dir().context(
                        "could not resolve default cc projects dir; pass --cc-projects-dir PATH",
                    )?)
                }
                None => None,
            };
            // mu-cc-sessions-console-lqqt.3: the cc marks sidecar follows
            // cc scanning — default it on whenever cc sessions are merged,
            // unless explicitly overridden.
            let cc_marks_db = match cc_marks_db {
                Some(p) => Some(p),
                None if cc_projects_dir.is_some() => mu_coding::console::default_cc_marks_db(),
                None => None,
            };
            // mu-console-hosts-dashboard-zy26: resolve the dashboard
            // artifact path; the default is ~/mu-stats/dashboard.html. A
            // missing file is handled at request time (friendly note), so
            // we only error here if the home dir itself can't be resolved.
            let dashboard_path = match dashboard_path {
                Some(p) => p,
                None => mu_coding::console::default_dashboard_path().context(
                    "could not resolve default dashboard path; pass --dashboard-path PATH",
                )?,
            };
            mu_coding::console::run(mu_coding::console::ConsoleOptions {
                bind,
                base_path,
                events_dir,
                analytics_db,
                cc_projects_dir,
                cc_marks_db,
                dashboard_path,
            })
            .await
        }
        Command::Mark {
            session,
            rating,
            note,
            events_dir,
        } => {
            let events_dir = match events_dir {
                Some(p) => p,
                None => mu_coding::serve::default_events_dir().context(
                    "could not resolve default events dir; pass --events-dir PATH explicitly",
                )?,
            };
            let outcome =
                mu_coding::console::mark::mark_session(&events_dir, &session, rating, note)?;
            println!(
                "marked {}/{} rating={} (event {})",
                outcome.daemon_id, outcome.session_id, outcome.rating, outcome.event_id
            );
            Ok(())
        }
        Command::Analytics { cmd } => run_analytics(cmd),
        Command::Capabilities { cmd } => run_capabilities(cmd),
    }
}

/// `mu capabilities discover <intent>` — build mu's capability manifest
/// in-process (registered tools + discovered skills) and rank it by intent,
/// best-first, as JSON. Standalone: no daemon, no session, so the manifest is
/// un-attenuated (everything the named tool set + discovered skills can offer).
/// The session-attenuated form lives behind the `capabilities/discover` RPC
/// (mu-kex4.6.4).
fn run_capabilities(cmd: CapabilitiesCmd) -> Result<()> {
    match cmd {
        CapabilitiesCmd::Discover {
            intent,
            limit,
            tools,
        } => {
            let tool_names = mu_coding::serve::parse_tools_csv(&tools);
            let bash_settings = mu_coding::serve::BashSettings {
                yolo: false,
                extra_allow: Vec::new(),
                prompt: false,
            };
            let tool_vec = mu_coding::serve::build_tools(&tool_names, &bash_settings)?;

            let project_root = std::env::current_dir().ok();
            let mut dirs = mu_core::skill::loader::default_search_dirs(project_root.as_deref());
            if let Ok(extra) = std::env::var("MU_SKILLS_DIR") {
                if !extra.is_empty() {
                    dirs.push(std::path::PathBuf::from(extra));
                }
            }
            let skills = mu_core::skill::loader::discover_skills(&dirs);

            let registry = mu_core::t4c_source::build_manifest(&tool_vec, &skills);
            let tree = registry.build()?;
            let results = mu_core::t4c_source::discover_view(&tree, &intent, limit);
            println!("{}", serde_json::to_string_pretty(&results)?);
            Ok(())
        }
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

/// mu-pr6r.1: run the deterministic process-layer auditors over a
/// session event-log JSONL and print findings. Offline, no daemon.
fn run_audit(log: &std::path::Path) -> Result<()> {
    let (event_log, malformed) = mu_core::event_log::SessionEventLog::from_jsonl(log)
        .map_err(|e| anyhow::anyhow!("reading event log {}: {e}", log.display()))?;
    if malformed > 0 {
        eprintln!(
            "audit: skipped {malformed} malformed line(s) in {}",
            log.display()
        );
    }
    let events = event_log.snapshot();
    let findings = mu_core::auditor::audit_session(&events);
    if findings.is_empty() {
        println!("audit: no findings ({} events)", events.len());
    } else {
        println!(
            "audit: {} finding(s) over {} events:",
            findings.len(),
            events.len()
        );
        for f in &findings {
            println!(
                "  [{:?}] {} @event {}: {}",
                f.severity, f.invariant, f.event_id, f.detail
            );
        }
    }
    Ok(())
}

/// `mu list-sessions` — print past sessions newest-first. Offline; reads
/// only each shown log's first record + dir mtimes
/// (mu-lazy-session-rehydration-bh4f). `last == None` means all.
/// Render the session listing to `out`. Display-style: no stdout side
/// effects of its own — the caller owns the sink (so `main` writes to
/// stdout, tests write to a buffer). `now_ms` is read here only to
/// compute relative ages. (#249 review)
fn run_list_sessions<W: std::io::Write>(
    out: &mut W,
    last: Option<usize>,
    events_dir: Option<std::path::PathBuf>,
) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let events_dir = match events_dir {
        Some(p) => p,
        None => mu_coding::serve::default_events_dir()
            .context("could not resolve default events dir; pass --events-dir PATH explicitly")?,
    };

    let index = mu_coding::sessions_index::scan_session_index(&events_dir, last);
    if index.total == 0 {
        writeln!(out, "no sessions found under {}", events_dir.display())?;
        return Ok(());
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    writeln!(
        out,
        "{:<24}  {:<28}  {:>5}  DAEMON",
        "SESSION", "PROVIDER/MODEL", "AGE"
    )?;
    for h in &index.headers {
        let pm = match (&h.provider_kind, &h.model) {
            (Some(p), Some(m)) => format!("{p}/{m}"),
            (Some(p), None) => p.clone(),
            _ => "-".to_string(),
        };
        // last_activity_unix_ms is 0 when the file's mtime couldn't be
        // read; show a sentinel rather than a misleading ~2857w age.
        let age = if h.last_activity_unix_ms == 0 {
            "?".to_string()
        } else {
            humanize_age_ms(now_ms.saturating_sub(h.last_activity_unix_ms))
        };
        writeln!(
            out,
            "{:<24}  {:<28}  {:>5}  {}",
            h.session_id,
            truncate_chars(&pm, 28),
            age,
            h.daemon_id
        )?;
    }

    // Only nudge about the cap when one is actually in effect. Under
    // --all (last == None), shown < total just means some files couldn't
    // be read, not that rows were withheld — don't imply otherwise.
    let shown = index.headers.len();
    if last.is_some() && shown < index.total {
        writeln!(
            out,
            "\n(showing {shown} of {} — use --last N or --all)",
            index.total
        )?;
    }
    Ok(())
}

/// Compact, dependency-free relative age: "12s", "5m", "3h", "2d", "4w".
fn humanize_age_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d");
    }
    format!("{}w", d / 7)
}

/// Char-safe truncation with an ellipsis (keeps multi-byte models intact).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
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

    fn unique_tmp(tag: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("mu-{tag}-{pid}-{nonce}"));
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    #[test]
    fn list_sessions_writes_to_provided_stream() {
        // run_list_sessions has no stdout side effects — it writes to the
        // sink it's handed. Capture into a Vec<u8> and assert. (#249 review)
        let dir = unique_tmp("listsessions");
        let daemon = dir.join("daemon-a");
        std::fs::create_dir_all(&daemon).expect("mkdir daemon");
        std::fs::write(
            daemon.join("session-1.jsonl"),
            "{\"id\":1,\"session_id\":\"session-1\",\"timestamp_unix_ms\":1700000000000,\
             \"actor\":{\"kind\":\"system\"},\"payload\":{\"kind\":\"session_created\",\
             \"provider_kind\":\"ollama\",\"model\":\"qwen3-coder:30b\"}}\n",
        )
        .expect("write session");

        let mut buf: Vec<u8> = Vec::new();
        run_list_sessions(&mut buf, Some(10), Some(dir.clone())).expect("list");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("SESSION"), "header present, got: {out}");
        assert!(out.contains("session-1"), "session id present, got: {out}");
        assert!(
            out.contains("ollama/qwen3-coder:30b"),
            "provider/model present, got: {out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_sessions_empty_dir_writes_notice_to_stream() {
        let dir = unique_tmp("listsessions-empty");
        let mut buf: Vec<u8> = Vec::new();
        run_list_sessions(&mut buf, Some(10), Some(dir.clone())).expect("list");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("no sessions found"), "got: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
