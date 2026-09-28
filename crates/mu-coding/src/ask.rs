//! `mu ask` — one-shot CLI frontend over `mu serve`.
//!
//! Spawns `mu serve` as a subprocess, speaks JSON-RPC over its
//! stdio, sends `create_session` + `ask_session`, drains notifications
//! until `session.done`, prints the assistant text, exits.
//!
//! See spec mu-005.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use mu_core::protocol::{AskSessionRequest, CreateSessionRequest, CreateSessionResponse};

/// Options for [`run`] — the CLI-flag bundle for a single `mu ask`
/// invocation. Constructed by the CLI binary's argument parser.
#[derive(Debug, Default)]
pub struct AskOptions {
    pub prompt: String,
    pub provider: String,
    pub model: Option<String>,
    pub tools: String,
    pub ephemeral: bool,
    pub thinking: Option<String>,
    /// Per-turn reasoning effort, carried on `ask_session.effort` — the
    /// same sticky `/effort` selection the mu-solo dial uses (mu-vcbm).
    /// `None` ⇒ leave the session's standing effort unchanged (the daemon
    /// falls back to the provider launch default, including `--thinking`).
    /// `Some(level)` updates it for this turn and onward. (mu-bez6)
    pub effort: Option<String>,
    pub bash_yolo: bool,
    pub bash_allow: Vec<String>,
    pub bash_prompt: bool,
    /// System prompt for the session. When present, sent as
    /// `CreateSessionRequest.system_prompt` (mu-n48 plumbing); when
    /// None, the daemon default is used. Populated by the CLI from
    /// `--append-system-prompt <FILE>` (file content read by the
    /// binary, not here, so this layer stays I/O-free).
    pub system_prompt: Option<String>,
    /// Hermetic session: forwarded as `--bare` to the spawned
    /// `mu serve` — no recall injection, no discovery bootstrap.
    /// (mu-mu-bare-flag-fxc8)
    pub bare: bool,
    /// mu-779s: cap on assistant-message turns. `None` → use the
    /// provider-aware default (20 for Anthropic, 35 for OpenAI).
    /// `Some(n)` → cap at `n` turns. `Some(0)` → disable entirely.
    /// Forwarded as `CreateSessionRequest.max_turns` to the daemon.
    pub max_turns: Option<u32>,
    /// Whether to enable MCP on the spawned one-shot daemon. `mu ask`
    /// defaults this false; callers opt in with `--enable-mcp`.
    pub mcp_enabled: bool,
    /// mu-048: `--max-usd` (+ `--spend-lanes`): the ceiling for this
    /// ask's session, forwarded as `CreateSessionRequest.spend_ceiling`.
    /// `None` → the daemon's `[spend]` default (off unless enabled).
    pub spend_ceiling: Option<mu_core::spend::SpendCeiling>,
    /// mu-049: `--role`: the role this model was chosen from, forwarded as
    /// `CreateSessionRequest.role` so the session falls back through its
    /// ranks. `None` → no fallback.
    pub role: Option<String>,
}

/// mu-049: where this process also writes its notices (`--notices`), set
/// once at startup. A notice is mu's own line — a model switch, a role
/// armed short — never model output, so a dispatcher that redirects stderr
/// to a log can forward these to its caller without reading that stream.
static NOTICES: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// mu-049: `--notices <path>`: append this invocation's notices there too.
pub fn set_notices_file(path: std::path::PathBuf) {
    let _ = NOTICES.set(path);
}

/// mu-049: say `line` to the caller — on stderr (`mu: <line>`), and into
/// the `--notices` file when one was given. A failed append is itself said
/// on stderr, not swallowed.
pub(crate) fn notice(line: &str) {
    eprintln!("mu: {line}");
    if let Some(path) = NOTICES.get() {
        use std::io::Write as _;
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = written {
            eprintln!("mu: could not write the notice to {}: {e}", path.display());
        }
    }
}

/// mu-048: the ask ended because the session's spend ceiling was
/// reached. `mu ask` prints it to stderr and exits 3 — distinct from a
/// model error (1) — so a benchmark harness can tell "cut off by the
/// ceiling" from "failed". The answer so far has already been printed.
#[derive(Debug)]
pub struct SpendCeilingReached(pub String);

impl std::fmt::Display for SpendCeilingReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "spend ceiling reached: {}", self.0)
    }
}

impl std::error::Error for SpendCeilingReached {}

/// mu-cbmru: the ask ended because the lane in force is OUT OF TOKENS — a
/// subscription usage cap or a metered lane with no credit. `mu ask` prints
/// it and exits 4, distinct from a model error (1) and from the spend
/// ceiling (3), so a dispatcher can route the task to another rank on an
/// EXIT CODE. The alternative — grepping this process's stderr for a phrase
/// — cannot distinguish a provider's error from the model's own reasoning
/// about one, since the reasoning body is printed to that same stream.
#[derive(Debug)]
pub struct ProviderOutOfTokens(pub String);

impl std::fmt::Display for ProviderOutOfTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider out of tokens: {}", self.0)
    }
}

impl std::error::Error for ProviderOutOfTokens {}

/// mu-pz12w: the exit-code vocabulary of `mu ask` / `mu resume`, in ONE
/// place. A stop reason the caller has to act on differently gets its own
/// code, so a dispatcher or harness decides on the code and never on a
/// phrase in stderr (that stream carries the model's own reasoning).
///
///   0  answered: the model finished on its own (`end_turn`)
///   1  error (model, transport, daemon), and any stop reason this list does
///      not name (`aborted`, `error`, a reason a future provider adds): the
///      catch-all in [`end_of_ask`] constructs a [`TerminalStop`] for it, so
///      a novel reason exits 1 and can never read as answered (mu-1mvq)
///   3  spend ceiling reached (`budget_cap`): answer so far is on stdout
///   4  lane out of tokens (`provider_usage_limit`): walk to the next rank
///   5  truncated at a token limit (`max_tokens`): stdout may be a fragment
///   6  stream dropped before its stop event (`degraded_eof`): fragment;
///      a retry is reasonable
///   7  refused by the provider's classifier (`refusal`): no answer
///   8  paused by the server (`pause_turn`): partial; mu does not continue
///   9  stopped at the turn cap (`iteration_cap`): the model did not finish
///      on its own. The loop emits this on any capped turn, whether or not
///      the cap's last turn was the final-answer turn (mu-frvot: only with
///      `[session].final_answer_turn` and a cap above one, and only if the
///      model obeyed it), so the text on stdout may be an answer or may be
///      the last tool round's output — the caller judges it, mu does not
///      call it answered.
///
/// Codes set OUTSIDE this process and kept clear of: 75 a seat the
/// dispatcher skipped, 78 bad config (EX_CONFIG), 124 timeout, 137 SIGKILL.
pub fn exit_code_for(stop_reason: &str) -> Option<i32> {
    match stop_reason {
        "budget_cap" => Some(3),
        "provider_usage_limit" => Some(4),
        "max_tokens" => Some(5),
        "degraded_eof" => Some(6),
        "refusal" => Some(7),
        "pause_turn" => Some(8),
        "iteration_cap" => Some(9),
        _ => None,
    }
}

/// mu-pz12w: the ask ended on a terminal stop that is neither the ceiling
/// nor a capped lane, but that a caller still must tell from a plain error:
/// the text on stdout may be a fragment (`max_tokens`, `degraded_eof`), there
/// is no answer at all (`refusal`, `pause_turn`), or the turn cap ended the
/// ask and the text is unjudged (`iteration_cap`). Before this, the first
/// four were a `bail!` — exit 1, the same as a model error — and the cap was
/// a silent 0, so a dispatcher could not tell "retry" from "fragment" from
/// "try another seat" from "answered" without parsing stderr. `mu ask`
/// prints the message and exits with [`exit_code_for`] of the stop reason.
#[derive(Debug)]
pub struct TerminalStop {
    pub stop_reason: String,
    pub message: String,
}

impl TerminalStop {
    pub fn new(stop_reason: &str, message: impl Into<String>) -> Self {
        Self {
            stop_reason: stop_reason.to_owned(),
            message: message.into(),
        }
    }

    /// The exit code for this stop; 1 for a stop reason the vocabulary does
    /// not name — which is exactly what [`end_of_ask`]'s catch-all builds,
    /// so an unlisted reason exits 1, never 0.
    pub fn exit_code(&self) -> i32 {
        exit_code_for(&self.stop_reason).unwrap_or(1)
    }
}

impl std::fmt::Display for TerminalStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TerminalStop {}

/// mu-pz12w: how an ask's terminal `stop_reason` becomes its exit — ONE
/// function for `mu ask` and `mu resume`, so the two cannot drift. `Ok` is
/// exit 0 and means answered; every `Err` is one of the typed stops the
/// binary downcasts (3, 4, or [`TerminalStop`] → 5..8, and 1 for a reason
/// this function does not name). The answered set is explicit: a reason
/// nobody classified must not exit 0 (mu-1mvq: a truncated ask once exited
/// 0 and a gate escalated every PR for a night on the strength of it).
pub fn end_of_ask(stop_reason: Option<&str>, spend_summary: Option<String>) -> Result<()> {
    match stop_reason {
        // The model finished on its own: the text on stdout is the answer.
        None | Some("end_turn") => Ok(()),
        // The turn cap ended the ask. mu-core emits IterationCap on any
        // capped turn (agent/loop_/mod.rs, the top-of-loop cap check), so
        // whether the text is a final answer (mu-frvot's answer turn ran and
        // was obeyed) or a tool round's leftovers is not known here. Its own
        // code: not an answer, not an error — the caller decides.
        Some(r @ "iteration_cap") => Err(TerminalStop::new(
            r,
            "ask stopped at its turn cap (stop_reason=iteration_cap): the model did \
             not finish on its own. If [session].final_answer_turn is on and the cap \
             is above one, the last turn was asked for an answer and the text above \
             may be it; otherwise it is the last round's output. Raise --max-turns \
             or resume to continue.",
        )
        .into()),
        // mu-cbmru: the lane is out of tokens. Exit 4, so a dispatcher can
        // walk to the next rank on the code rather than on our stderr.
        Some("provider_usage_limit") => Err(ProviderOutOfTokens(
            spend_summary.unwrap_or_else(|| "(lane not reported)".to_owned()),
        )
        .into()),
        // mu-048: the ceiling ended the ask. The figure rides on the
        // `spend` callout the loop emits just before the Done.
        Some("budget_cap") => Err(SpendCeilingReached(
            spend_summary.unwrap_or_else(|| "(figure not reported)".to_owned()),
        )
        .into()),
        // A truncated response must never exit 0 silently (mu-1mvq): the
        // ai-review gate spent a night escalating every PR because ollama
        // truncated oversized prompts to its context window, leaving one
        // token of generation budget — the model emitted a single word,
        // the stream ended *cleanly* with finish_reason=length, and no
        // layer reported anything. The partial text has already been
        // printed (it is still data); the nonzero exit + stderr line make
        // the truncation legible to scripts and humans. Each of the four
        // below is a TerminalStop with its own exit code (5..8).
        Some(r @ "max_tokens") => Err(TerminalStop::new(
            r,
            "response truncated (stop_reason=max_tokens): the model hit a token \
             limit — either the output cap, or the prompt filled the model's \
             context window (ollama silently truncates oversized prompts; see \
             mu-1mvq). Output above may be a fragment.",
        )
        .into()),
        Some(r @ "degraded_eof") => Err(TerminalStop::new(
            r,
            "response degraded (stop_reason=degraded_eof): the provider stream \
             closed without a terminal stop event (connection drop or upstream \
             truncation). Output above may be a fragment.",
        )
        .into()),
        // mu-provider-drift-2026q3-y43la: the gen-5 terminal states must not
        // exit 0 — a refused ask has no answer, and a server-paused ask ended
        // early. Same rationale as max_tokens: the nonzero exit keeps
        // headless scoring pipelines and spawn callers from reading a
        // non-answer as a clean success. Edge (panel-raised): if a refusal
        // ever cut a turn that ALSO completed a final_answer call, the
        // answer still prints above but the exit is nonzero — deliberate:
        // an answer the safety classifier cut mid-delivery should force
        // caller scrutiny, not score as clean.
        Some(r @ "refusal") => Err(TerminalStop::new(
            r,
            "ask refused (stop_reason=refusal): the provider's safety \
             classifier declined the request or cut generation. There is no \
             answer; consider a different seat/model.",
        )
        .into()),
        Some(r @ "pause_turn") => Err(TerminalStop::new(
            r,
            "response paused (stop_reason=pause_turn): the server paused a \
             long-running turn and mu does not implement continuation. \
             Output above may be partial.",
        )
        .into()),
        // `aborted`, `error`, `tool_use` (never terminal), or a reason a
        // future provider adds: not an answer. Exit 1 with the reason named,
        // never a silent 0.
        Some(other) => Err(TerminalStop::new(
            other,
            format!(
                "ask ended with stop_reason={other}, which mu does not classify as an \
                 answer. Output above, if any, is not a completed reply."
            ),
        )
        .into()),
    }
}

/// Run a single `mu ask` invocation. Flags (`provider`, `model`,
/// `tools`) are forwarded to the spawned `mu serve`.
pub async fn run(opts: AskOptions) -> Result<()> {
    // Map the CLI provider flag to a wire-level selector. This is what
    // gets sent in create_session; the daemon constructs the provider
    // per session from this. First resolve a possible SELECTION alias
    // (mu-eb98 item 2): a favorite name/alias passed as --model rewrites
    // {provider, model} to the favorite's, so the long tag lives in one place.
    let (provider, model) =
        crate::serve::resolve_launch_selection(&opts.provider, opts.model.as_deref());
    let selector = crate::serve::selector_from_cli(&provider, model.as_deref())?;

    // mu-fnn: generate a per-spawn bearer token for the trust-on-spawn
    // handshake with the child `mu serve`. The child reads
    // `MU_BEARER_TOKEN` from its env (see `serve::run`) and configures
    // BEARER auth with this single token; we then present the same
    // token in `peer.auth_initiate` before any session.* RPC.
    let bearer_token = generate_bearer_token();

    let mut child = spawn_serve(
        &opts.tools,
        opts.ephemeral,
        opts.thinking.as_deref(),
        opts.bash_yolo,
        &opts.bash_allow,
        opts.bash_prompt,
        opts.bare,
        opts.mcp_enabled,
        &bearer_token,
    )?;
    let mut stdin = child.stdin.take().context("child stdin not captured")?;
    let stdout = child.stdout.take().context("child stdout not captured")?;
    let mut stdout = BufReader::new(stdout);

    let mut next_id: u64 = 1;

    // Authenticate before any protected RPC. Failure here is fatal —
    // the gate will reject every subsequent call.
    authenticate(&mut stdin, &mut stdout, &mut next_id, &bearer_token).await?;

    // mu-phl v0 / mu-lfgh: capture the operator's cwd at the entry of
    // the ask path so the daemon's session-start recall (subprocess
    // agent memory + project-file hierarchy) scopes to the operator's
    // actual project. Falls back to `None` if cwd can't be determined
    // (extremely unusual; the daemon resolves its own fallback in
    // build_project_context).
    let invocation_cwd = std::env::current_dir().ok();
    let session_id = create_session(
        &mut stdin,
        &mut stdout,
        &mut next_id,
        &selector,
        opts.system_prompt.as_deref(),
        invocation_cwd,
        SessionLimits {
            max_turns: opts.max_turns,
            spend_ceiling: opts.spend_ceiling,
            role: opts.role.clone(),
        },
    )
    .await?;
    let (text, stop_reason, spend_summary) = ask_and_drain(
        &mut stdin,
        &mut stdout,
        &session_id,
        &opts.prompt,
        opts.effort.as_deref(),
        &mut next_id,
    )
    .await?;

    println!("{}", text);

    // Closing stdin signals the daemon to exit cleanly.
    drop(stdin);
    // The stop reason decides the exit (end_of_ask, shared with `mu resume`),
    // and it is decided BEFORE the child's shutdown is judged: a typed stop —
    // the lane out of tokens (mu-cbmru: a dispatcher routes around it on
    // exit 4, an operator may have to add credit), a truncation, a refusal —
    // is the caller's most actionable fact, and a messy daemon exit must not
    // mask it into a generic failure (mu-pz12w, board-raised: it used to
    // shield only the cap, so every other typed stop collapsed to 1 whenever
    // the daemon's own exit was unclean).
    let ended = end_of_ask(stop_reason.as_deref(), spend_summary);
    let typed_stop = ended.is_err();
    let reason = stop_reason.as_deref().unwrap_or("end_turn");
    match timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) if status.success() => {}
        Ok(Ok(status)) if typed_stop => {
            eprintln!("mu serve exited with status {status} after stop_reason={reason}");
        }
        Ok(Ok(status)) => bail!("mu serve exited with status {status}"),
        Ok(Err(e)) if typed_stop => {
            eprintln!("waiting for child after stop_reason={reason}: {e}");
        }
        Ok(Err(e)) => return Err(e).context("waiting for child"),
        Err(_) => {
            let _ = child.kill().await;
            if !typed_stop {
                bail!("mu serve did not exit within 5 seconds; killed")
            }
            eprintln!("mu serve did not exit within 5 seconds after stop_reason={reason}; killed");
        }
    }

    ended
}

/// Generate a per-spawn opaque bearer token for the parent↔child
/// handshake. The strength bar is "unguessable across this process
/// lifetime"; SHA-256 + constant-time comparison on the daemon side
/// already absorb timing concerns. 32 hex chars / 128 bits is plenty.
pub(crate) fn generate_bearer_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Run the `peer.auth_initiate` BEARER handshake. Returns Ok on
/// `Accepted`; surfaces a clear error on `Denied` or any non-success.
pub(crate) async fn authenticate(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    next_id: &mut u64,
    token: &str,
) -> Result<()> {
    let id = *next_id;
    *next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "peer.auth_initiate",
        "params": {
            "mechanism": "bearer",
            "initial_response": token,
        },
    });
    write_line(stdin, &req).await?;
    loop {
        let line = read_line(stdout).await?;
        if line.get("id").and_then(|v| v.as_u64()) == Some(id) {
            let outcome = line
                .get("result")
                .and_then(|r| r.get("outcome"))
                .and_then(|v| v.as_str());
            if outcome == Some("accepted") {
                return Ok(());
            }
            bail!("peer.auth_initiate did not accept the spawn-time token: {line}");
        }
        // Skip unrelated notifications.
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the CLI flag bundle 1:1
pub(crate) fn spawn_serve(
    tools: &str,
    ephemeral: bool,
    thinking: Option<&str>,
    bash_yolo: bool,
    bash_allow: &[String],
    bash_prompt: bool,
    bare: bool,
    mcp_enabled: bool,
    bearer_token: &str,
) -> Result<tokio::process::Child> {
    // MU_BINARY env override allows integration tests to point at a
    // specific binary path (`env!("CARGO_BIN_EXE_mu")`); production
    // falls back to the current executable.
    let binary = match std::env::var("MU_BINARY") {
        Ok(v) if !v.is_empty() => v,
        _ => std::env::current_exe()
            .context("could not determine current_exe")?
            .to_string_lossy()
            .into_owned(),
    };

    let mut cmd = Command::new(&binary);
    cmd.arg("serve");
    // mu-fnn: hand the child the same BEARER token we'll present at
    // `peer.auth_initiate`. Single source of truth: this string.
    cmd.env("MU_BEARER_TOKEN", bearer_token);
    if !tools.is_empty() {
        cmd.arg("--tools").arg(tools);
    }
    if ephemeral {
        cmd.arg("--ephemeral");
    }
    if let Some(t) = thinking {
        if !t.is_empty() {
            cmd.arg("--thinking").arg(t);
        }
    }
    if bash_yolo {
        cmd.arg("--bash-yolo");
    }
    for entry in bash_allow {
        if !entry.is_empty() {
            cmd.arg("--bash-allow").arg(entry);
        }
    }
    if bash_prompt {
        cmd.arg("--bash-prompt");
    }
    if bare {
        cmd.arg("--bare");
    }
    if mcp_enabled {
        cmd.arg("--enable-mcp");
    } else {
        cmd.arg("--disable-mcp");
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // stderr inherited — daemon logs go to the user's terminal.
        .spawn()
        .with_context(|| format!("failed to spawn `{binary} serve`"))
}

/// The per-session bounds a `mu ask` can set: the turn cap (mu-779s) and
/// the spend ceiling (mu-048). `None` for either → the daemon's default.
struct SessionLimits {
    max_turns: Option<u32>,
    spend_ceiling: Option<mu_core::spend::SpendCeiling>,
    role: Option<String>,
}

async fn create_session(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    next_id: &mut u64,
    selector: &mu_core::protocol::ProviderSelector,
    system_prompt: Option<&str>,
    cwd: Option<std::path::PathBuf>,
    limits: SessionLimits,
) -> Result<String> {
    let id = *next_id;
    *next_id += 1;
    // Build params from the typed protocol struct so that
    // serde's `skip_serializing_if = "Option::is_none"` on
    // CreateSessionRequest.system_prompt is honored — no
    // explicit null field when unset (mu-x83o).
    //
    // mu-phl v0 / mu-lfgh: cwd is plumbed through from the operator's
    // invocation (set by the `ask()` entry point to
    // std::env::current_dir()) so the daemon-side recall providers
    // (subprocess agent memory + project-file hierarchy) scope to the
    // operator's actual project rather than the daemon's process cwd.
    let body = CreateSessionRequest {
        provider: selector.clone(),
        system_prompt: system_prompt.map(str::to_owned),
        cwd,
        // mu-f1a0: `mu ask` is a batch-shaped one-shot — the 5m
        // default tier is correct (no human gaps to survive).
        cache_ttl: None,
        // mu-7e21: no autonomy grant from `mu ask` yet — a future
        // `--autonomy` flag fills this (operator-deferred; solo.toml
        // is the first frontend knob).
        autonomy: None,
        // mu-n25a: `mu ask` does not restrict side-effects (root default,
        // unrestricted ceiling). solo.toml's `[session] max_side_effects`
        // is the first operator knob.
        max_side_effects: None,
        // mu-779s: per-session max_turns cap. `None` → use provider default.
        // `Some(0)` → disable cap entirely.
        max_turns: limits.max_turns,
        // mu-vcbm: `mu ask` is a batch one-shot with no interactive
        // `/effort` dial — use the provider's launch default.
        effort: None,
        // mu-048: `--max-usd`; `None` → the daemon's `[spend]` default.
        spend_ceiling: limits.spend_ceiling,
        // mu-049: `--role` — the session falls back through its ranks
        role: limits.role,
    };
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": CreateSessionRequest::METHOD,
        "params": body,
    });
    write_line(stdin, &req).await?;

    loop {
        let line = read_line(stdout).await?;
        if line.get("id") == Some(&Value::from(id)) {
            if let Some(error) = line.get("error") {
                bail!("create_session failed: {error}");
            }
            let result = line
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("create_session response missing `result`"))?;
            let resp: CreateSessionResponse =
                serde_json::from_value(result).context("parse CreateSessionResponse")?;
            // mu-049: the role could not arm every rank — said up front, so
            // a caller reading a later "no model left" knows the roster was
            // short from the start
            if !resp.fallback_unrunnable.is_empty() {
                notice(&format!(
                    "fallback cannot use: {}",
                    resp.fallback_unrunnable.join("; ")
                ));
            }
            return Ok(resp.session_id);
        }
        // Other notifications (none expected this early) — ignore.
    }
}

/// Build the `ask_session` JSON-RPC params from the typed
/// [`AskSessionRequest`]. Split out so the `skip_serializing_if =
/// "Option::is_none"` omission of an unset per-turn `/effort` override is
/// unit-testable without driving the daemon I/O: `effort = None` must
/// produce NO `effort` key (not an explicit null), mirroring how
/// `create_session` builds its body. (mu-bez6)
pub(crate) fn build_ask_params(session_id: &str, prompt: &str, effort: Option<&str>) -> Value {
    let params = AskSessionRequest {
        session_id: session_id.to_owned(),
        user_message: prompt.to_owned(),
        effort: effort.map(str::to_owned),
    };
    serde_json::to_value(params).expect("AskSessionRequest serializes")
}

/// Send the ask and drain notifications until `session.done`. Returns
/// the assistant text plus the done event's `stop_reason` (None when
/// the daemon omits it — older daemons or malformed events), so the
/// caller can distinguish a complete answer from a truncated one
/// (mu-1mvq), and the `spend` callout's summary when the session's spend
/// ceiling was reached (mu-048).
pub(crate) async fn ask_and_drain(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    session_id: &str,
    prompt: &str,
    effort: Option<&str>,
    next_id: &mut u64,
) -> Result<(String, Option<String>, Option<String>)> {
    let id = *next_id;
    *next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": AskSessionRequest::METHOD,
        "params": build_ask_params(session_id, prompt, effort),
    });
    write_line(stdin, &req).await?;

    // Per-turn assembly: deltas stream into `current`; each turn's
    // `session.assistant_text_finalized` (mu-wk2) replaces that turn's
    // accumulated deltas with the authoritative final text. The two can
    // differ — e.g. the ollama provider's text-dialect tool-call rescue
    // (mu-ollama-qwen-tool-dialect-yfl0) strips leaked markup from the
    // final message that already went out as deltas.
    let mut finalized = String::new();
    let mut current = String::new();
    // mu-upk2: reasoning is surfaced on STDERR so stdout stays exactly the
    // answer. Deltas accumulate here; thinking_finalized flushes the block.
    let mut thinking_current = String::new();
    let mut got_done = false;
    let mut got_response = false;
    let mut stop_reason: Option<String> = None;
    // mu-048: the `spend` callout's summary (`$0.42 of $2.00 (lanes:
    // billed)`), the figure a `budget_cap` stop is reported with.
    let mut spend_summary: Option<String> = None;
    // mu-cbmru: the lane reported its usage cap during this ask. Structured
    // (`session.provider_usage_limit`), never scraped: the caller exits 4 so
    // a dispatcher routes the task to another rank.
    let mut usage_limit: Option<String> = None;
    // mu-bm6za: when the session ends via the `final_answer` tool, the
    // answer travels as the tool's argument, not as assistant text — a
    // final_answer-only closing turn can leave `finalized` empty (or
    // holding only earlier narration). Capture the argument so it can
    // stand in as stdout when no finalized text arrived. The capture is
    // two-phase (pending → promoted on an ok completion) so a refused or
    // errored final_answer call can never supply stdout.
    let mut final_answer_arg: Option<String> = None;
    let mut pending_final_answer: Option<String> = None;

    loop {
        let line = read_line(stdout).await?;
        match line.get("method").and_then(Value::as_str) {
            Some("session.text_delta") => {
                if line["params"]["session_id"] == session_id {
                    if let Some(delta) = line["params"]["delta"].as_str() {
                        current.push_str(delta);
                    }
                }
            }
            Some("session.assistant_text_finalized") => {
                if line["params"]["session_id"] == session_id {
                    // mu-cbmru: output after a cap means the cap did NOT end
                    // this ask — a fallback route answered it — so the latch
                    // must not survive to retype a later, unrelated error as
                    // the cap. (Unreachable until increment 3 arms routes;
                    // wrong the moment it is.)
                    usage_limit = None;
                    if let Some(text) = line["params"]["text"].as_str() {
                        finalized.push_str(text);
                        current.clear();
                    }
                }
            }
            Some("session.thinking_delta") => {
                if line["params"]["session_id"] == session_id {
                    if let Some(delta) = line["params"]["delta"].as_str() {
                        thinking_current.push_str(delta);
                    }
                }
            }
            Some("session.thinking_finalized") => {
                if line["params"]["session_id"] == session_id {
                    // Reasoning → stderr (stdout is reserved for the answer).
                    let text = line["params"]["text"].as_str().unwrap_or("");
                    let body = if text.is_empty() {
                        thinking_current.as_str()
                    } else {
                        text
                    };
                    if !body.trim().is_empty() {
                        eprintln!("[thinking] {body}");
                    }
                    thinking_current.clear();
                }
            }
            Some("session.tool_call_started") => {
                if line["params"]["session_id"] == session_id {
                    // mu-upk2: surface tool calls on stderr (these were
                    // dropped "in v1"). The streamed session.tool_call_delta
                    // fragments are intentionally NOT echoed here — partial
                    // JSON is noise in a headless answer pipe; the finalized
                    // call below is the useful unit.
                    let name = line["params"]["tool_name"].as_str().unwrap_or("?");
                    let args = &line["params"]["arguments"];
                    if name == "final_answer" {
                        // Only PENDING here — tool_call_started fires before
                        // the runtime's refusal gates (capability / retry /
                        // validation / permission), so a refused or errored
                        // call must never become stdout. Promotion happens on
                        // the matching completed event below; tool execution
                        // is serial, so adjacency pairing is sound.
                        // (ci-aipr gpt-5.5 finding, PR #577 round 1.)
                        pending_final_answer = args
                            .get("answer")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                    }
                    eprintln!("[tool] {name} {args}");
                }
            }
            Some("session.tool_call_completed") => {
                if line["params"]["session_id"] == session_id {
                    usage_limit = None; // mu-cbmru: see assistant_text_finalized
                    let kind = line["params"]["outcome"]["kind"].as_str().unwrap_or("?");
                    if let Some(answer) = pending_final_answer.take() {
                        if kind == "ok" {
                            final_answer_arg = Some(answer);
                        }
                    }
                    eprintln!("[tool result: {kind}]");
                }
            }
            Some("session.done") => {
                if line["params"]["session_id"] == session_id {
                    got_done = true;
                    stop_reason = line["params"]["stop_reason"].as_str().map(str::to_owned);
                }
            }
            Some("session.provider_usage_limit") => {
                if line["params"]["session_id"] == session_id {
                    let lane = format!(
                        "{}/{}",
                        line["params"]["provider_kind"].as_str().unwrap_or("?"),
                        line["params"]["model"].as_str().unwrap_or("?")
                    );
                    let plan = line["params"]["plan_type"].as_str().unwrap_or("unknown");
                    let resets = match line["params"]["resets_in_seconds"].as_u64() {
                        Some(s) => format!("resets in ~{}h{:02}m", s / 3600, (s % 3600) / 60),
                        None => "reset time not reported".to_owned(),
                    };
                    usage_limit = Some(format!("{lane} (plan {plan}, {resets})"));
                }
            }
            Some("session.callout") => {
                // the loop's `category` is the wire's `kind` (forwarder)
                if line["params"]["session_id"] == session_id && line["params"]["kind"] == "spend" {
                    spend_summary = line["params"]["body"]["summary"]
                        .as_str()
                        .map(str::to_owned);
                }
                // mu-049: the session moved to the next model in its role
                // because the one in force ran out of tokens. Said on
                // stderr so the caller can report it — the answer on stdout
                // came from a different model than it asked for, and the
                // capped account may need credit.
                if line["params"]["session_id"] == session_id
                    && line["params"]["kind"] == "fallback"
                {
                    if let Some(summary) = line["params"]["body"]["summary"].as_str() {
                        notice(summary);
                    }
                    // the cap was answered: a later error on the next
                    // model is that model's own, not this cap
                    usage_limit = None;
                }
            }
            Some("session.error") => {
                if line["params"]["session_id"] == session_id {
                    let msg = line["params"]["message"].as_str().unwrap_or("(no message)");
                    // mu-cbmru: the cap arrived first, so this error IS the
                    // cap. Carried out rather than raised here, so the child
                    // still gets its clean shutdown (an early return leaves
                    // the daemon writing into a closed pipe).
                    if let Some(lane) = usage_limit.take() {
                        // the same fold the normal epilogue does: a provider
                        // that streamed deltas without a finalize (or a cap
                        // that landed mid-stream) still has its text here,
                        // and the answer so far is data even when the ask
                        // ends on the cap
                        finalized.push_str(&current);
                        return Ok((
                            finalized,
                            Some("provider_usage_limit".to_owned()),
                            Some(format!("{lane}: {msg}")),
                        ));
                    }
                    bail!("session error: {msg}");
                }
            }
            _ => {
                // Could be the ask_session response itself.
                if line.get("id") == Some(&Value::from(id)) {
                    if let Some(error) = line.get("error") {
                        bail!("ask_session failed: {error}");
                    }
                    got_response = true;
                }
                // Else: streamed tool-arg deltas (session.tool_call_delta) or
                // other notifications — not surfaced in the headless pipe.
            }
        }
        if got_done && got_response {
            // `current` is normally empty here (every turn finalizes);
            // keep it as a defensive fallback for providers/paths that
            // never emit assistant_text_finalized.
            finalized.push_str(&current);
            // mu-bm6za: a final_answer-terminated ask whose closing turn
            // carried no assistant text still has an authoritative
            // answer — the tool argument. Only substitute when NO text
            // arrived at all: any finalized narration keeps priority so
            // this cannot mask ordinary output.
            if finalized.trim().is_empty() {
                if let Some(answer) = final_answer_arg {
                    return Ok((answer, stop_reason, spend_summary));
                }
            }
            return Ok((finalized, stop_reason, spend_summary));
        }
    }
}

pub(crate) async fn write_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    stdin.write_all(s.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

pub(crate) async fn read_line(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut line = String::new();
    let n = stdout.read_line(&mut line).await?;
    if n == 0 {
        bail!("mu serve closed stdout unexpectedly");
    }
    serde_json::from_str(line.trim_end_matches('\n')).context("parse JSON line from daemon")
}

#[cfg(test)]
mod tests {
    use super::*;

    // mu-bez6: the headless `--effort` carrier must put the per-turn
    // `/effort` selection on `ask_session.effort` exactly as the mu-solo
    // dial does — present when set, ABSENT (not null) when unset, so the
    // daemon leaves the session's standing effort unchanged.
    #[test]
    fn ask_params_carry_effort_when_set() {
        let p = build_ask_params("sess-1", "hello", Some("high"));
        assert_eq!(p["session_id"], "sess-1");
        assert_eq!(p["user_message"], "hello");
        assert_eq!(p["effort"], "high");
    }

    #[test]
    fn ask_params_omit_effort_when_unset() {
        let p = build_ask_params("sess-1", "hello", None);
        assert!(
            p.get("effort").is_none(),
            "no `/effort` override ⇒ the field must be omitted, not null: {p}"
        );
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::{exit_code_for, TerminalStop};

    /// The vocabulary is total over the terminal stop reasons and distinct
    /// per reason; codes set outside the process are never reused.
    #[test]
    fn every_terminal_stop_has_its_own_code() {
        let reasons = [
            "budget_cap",
            "provider_usage_limit",
            "max_tokens",
            "degraded_eof",
            "refusal",
            "pause_turn",
            "iteration_cap",
        ];
        let codes: Vec<i32> = reasons.iter().map(|r| exit_code_for(r).unwrap()).collect();
        assert_eq!(codes, vec![3, 4, 5, 6, 7, 8, 9]);
        for c in &codes {
            assert!(
                ![0, 1, 75, 78, 124, 137].contains(c),
                "code {c} is reserved"
            );
        }
        assert_eq!(exit_code_for("end_turn"), None);
        assert_eq!(exit_code_for(""), None);
    }

    #[test]
    fn terminal_stop_carries_the_code_and_the_message() {
        let stop = TerminalStop::new("max_tokens", "response truncated");
        assert_eq!(stop.exit_code(), 5);
        assert_eq!(stop.to_string(), "response truncated");
        // an unnamed reason must not exit 0 or collide with a named one
        assert_eq!(TerminalStop::new("something_new", "x").exit_code(), 1);
        // it must survive the anyhow boundary the binary downcasts across
        let err: anyhow::Error = TerminalStop::new("refusal", "no answer").into();
        assert_eq!(err.downcast_ref::<TerminalStop>().unwrap().exit_code(), 7);
    }

    /// The exit an ask ends with, by stop reason, as the binary would see
    /// it after downcasting: only an answer is 0, and a reason the
    /// vocabulary does not name is 1, never 0 (the board's finding on the
    /// first cut of this change: the catch-all returned Ok).
    fn exit_of(stop_reason: Option<&str>) -> i32 {
        match super::end_of_ask(stop_reason, None) {
            Ok(()) => 0,
            Err(e) if e.downcast_ref::<super::SpendCeilingReached>().is_some() => 3,
            Err(e) if e.downcast_ref::<super::ProviderOutOfTokens>().is_some() => 4,
            Err(e) => e
                .downcast_ref::<TerminalStop>()
                .map(TerminalStop::exit_code)
                .unwrap_or(-1),
        }
    }

    #[test]
    fn end_of_ask_exits_0_only_for_an_answer() {
        assert_eq!(exit_of(None), 0);
        assert_eq!(exit_of(Some("end_turn")), 0);
        // the turn cap is not an answer: the loop emits it on any capped turn
        assert_eq!(exit_of(Some("iteration_cap")), 9);
        assert_eq!(exit_of(Some("budget_cap")), 3);
        assert_eq!(exit_of(Some("provider_usage_limit")), 4);
        assert_eq!(exit_of(Some("max_tokens")), 5);
        assert_eq!(exit_of(Some("degraded_eof")), 6);
        assert_eq!(exit_of(Some("refusal")), 7);
        assert_eq!(exit_of(Some("pause_turn")), 8);
        // unlisted reasons: the loop's own remaining variants and a novel one
        for other in ["aborted", "error", "tool_use", "content_filter"] {
            assert_eq!(
                exit_of(Some(other)),
                1,
                "stop_reason={other} must not read as answered"
            );
        }
        let err = super::end_of_ask(Some("aborted"), None).unwrap_err();
        assert!(err.to_string().contains("stop_reason=aborted"), "{err}");
    }
}
