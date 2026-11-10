//! `mu resume` (a.k.a. `mu --resume`) — STRICT fork-at-tail resume of a
//! dead session (mu-mh4).
//!
//! Spawns `mu serve`, authenticates, then calls `session.resume` with the
//! predecessor's `daemon:session` / `mu:<daemon>/<session>` ref. The
//! daemon no longer rehydrates every log at startup
//! (mu-lazy-session-rehydration-bh4f); it lazily find-by-ids and parses
//! the predecessor's one log on demand when `session.resume` resolves it. The
//! daemon projects the predecessor's log to its last clean boundary and
//! births a fresh live session seeded with that history — or REFUSES
//! with a precise diagnosis if the log is ragged, leaving the log
//! untouched. `mu recover` ([`run_recover`]) is the repair path for those.
//!
//! On success the resumed session id is printed. When a `prompt` is
//! given, the resumed session is immediately asked it (the new head
//! continues the conversation); otherwise the command just reports the
//! attach and exits, leaving the session resumable by a TUI/console.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::time::timeout;

use mu_core::protocol::{
    RecoverSessionRequest, RecoverSessionResponse, ResumeSessionRequest, ResumeSessionResponse,
};

use crate::ask::{
    ask_and_drain, authenticate, generate_bearer_token, read_line, spawn_serve, write_line,
};

/// Options for [`run`] — the CLI-flag bundle for one `mu resume`.
#[derive(Debug, Default)]
pub struct ResumeOptions {
    /// Predecessor session ref: `daemon:session` or
    /// `mu:<daemon>/<session>`.
    pub session_ref: String,
    /// Optional prompt to ask the resumed session immediately. When
    /// None, the command attaches the head and exits.
    pub prompt: Option<String>,
    /// Provider backend for the resumed (live) session.
    pub provider: String,
    /// Model id for the resumed session.
    pub model: Option<String>,
    /// Tools to enable (forwarded to the spawned `mu serve`).
    pub tools: String,
    pub ephemeral: bool,
    pub thinking: Option<String>,
    pub bash_yolo: bool,
    pub bash_allow: Vec<String>,
    pub bash_prompt: bool,
    pub bare: bool,
    /// mu-049: `--role`; `None` → the predecessor's role, if any.
    pub role: Option<String>,
}

/// A spawned, authenticated `mu serve` for one resume or recover.
struct Daemon {
    child: tokio::process::Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Daemon {
    async fn spawn(opts: &ResumeOptions) -> Result<Self> {
        let bearer_token = generate_bearer_token();
        let mut child = spawn_serve(
            &opts.tools,
            opts.ephemeral,
            opts.thinking.as_deref(),
            opts.bash_yolo,
            &opts.bash_allow,
            opts.bash_prompt,
            opts.bare,
            true,
            &bearer_token,
        )?;
        let mut stdin = child.stdin.take().context("child stdin not captured")?;
        let stdout = child.stdout.take().context("child stdout not captured")?;
        let mut stdout = BufReader::new(stdout);
        let mut next_id: u64 = 1;
        authenticate(&mut stdin, &mut stdout, &mut next_id, &bearer_token).await?;
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id,
        })
    }

    /// Ask the new head `prompt`, or print its id when there is none, then
    /// shut the daemon down.
    async fn finish(self, session_id: Option<&str>, prompt: Option<&str>) -> Result<()> {
        let Self {
            mut child,
            mut stdin,
            mut stdout,
            mut next_id,
        } = self;
        let (stop_reason, spend_summary) = match (session_id, prompt) {
            (Some(session_id), Some(prompt)) => {
                let (text, stop_reason, spend_summary) = ask_and_drain(
                    &mut stdin,
                    &mut stdout,
                    session_id,
                    prompt,
                    // `mu resume` has no per-turn `/effort` flag (yet); leave the
                    // resumed session's standing effort untouched. (mu-bez6)
                    None,
                    &mut next_id,
                )
                .await?;
                println!("{text}");
                (stop_reason, spend_summary)
            }
            (Some(session_id), None) => {
                // No prompt: just report the new session id on stdout so a
                // caller can capture it.
                println!("{session_id}");
                (None, None)
            }
            (None, _) => (None, None),
        };

        // Dropping stdin signals EOF; the daemon's serve loop sees it and
        // begins a CLEAN shutdown (flushing each session's JSONL writer).
        // mu-mh4 (panel finding 5): give that clean shutdown a GENEROUS grace
        // window before resorting to SIGKILL. SIGKILL cannot be caught, so it
        // can truncate a session log mid-write — the ragged tail this very
        // feature exists to manage. A long patience window means we
        // effectively never SIGKILL a healthy-but-slow daemon; the kill stays
        // a true last resort for a genuinely wedged process. (A SIGTERM-then-
        // grace-then-SIGKILL escalation would be strictly nicer but needs a
        // signal dep — nix/libc — which this crate does not carry; deferred to
        // mu-nqn5's cleanup pass rather than pulling in a dep for a last-
        // resort path.)
        drop(stdin);
        // mu-pz12w: the same stop-reason → exit mapping as `mu ask` (a resumed
        // head is dispatched and walked by the same callers), from the one
        // function that holds it: 3 ceiling, 4 out of tokens, 5..9 the terminal
        // stops, 1 for a reason it does not name, 0 only for an answer. Decided
        // BEFORE the child's shutdown is judged — same rule as `mu ask`: a typed
        // stop is the caller's most actionable fact (mu-cbmru: a dispatcher
        // routes around exit 4, an operator may have to add credit), and a messy
        // daemon exit must not mask it into a generic failure.
        let ended = crate::ask::end_of_ask(stop_reason.as_deref(), spend_summary);
        let typed_stop = ended.is_err();
        let reason = stop_reason.as_deref().unwrap_or("end_turn");
        match timeout(Duration::from_secs(30), child.wait()).await {
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
                    bail!(
                        "mu serve did not exit within 30 seconds; killed (SIGKILL — log may be truncated)"
                    )
                }
                eprintln!(
                    "mu serve did not exit within 30 seconds after stop_reason={reason}; killed"
                );
            }
        }

        ended
    }
}

/// Run a single `mu resume` invocation.
pub async fn run(opts: ResumeOptions) -> Result<()> {
    let body = resume_request(&opts)?;
    let mut daemon = Daemon::spawn(&opts).await?;
    let resp: ResumeSessionResponse =
        call(&mut daemon, ResumeSessionRequest::METHOD, &body).await?;

    eprintln!(
        "resumed {} as session {} (forked at predecessor event {}, seeded {} message(s))",
        opts.session_ref,
        resp.session_id,
        fork_label(resp.branched_at_event_id),
        resp.seeded_message_count,
    );
    // mu-049: as `mu ask` — a role armed short is said up front
    notice_unrunnable(&resp.fallback_unrunnable);
    daemon
        .finish(Some(&resp.session_id), opts.prompt.as_deref())
        .await
}

/// Options for [`run_recover`] — one `mu recover`.
#[derive(Debug, Default)]
pub struct RecoverOptions {
    /// The damaged session and the new head's settings.
    pub resume: ResumeOptions,
    /// Start the head; false → diagnose only.
    pub apply: bool,
    /// Close open tool calls instead of dropping the turn that made them.
    pub close_calls: bool,
    /// A legacy head's predecessor, `daemon:session`.
    pub predecessor: Option<String>,
}

/// Run a single `mu recover` (mu-resume-recover-cli-epfx): diagnose a log
/// `mu resume` refuses, or with `apply`, start a new head forked at its last
/// clean point. The damaged log is only read.
pub async fn run_recover(opts: RecoverOptions) -> Result<()> {
    if opts.resume.prompt.is_some() && !opts.apply {
        bail!("a prompt is asked of the recovered head, so it needs --apply");
    }
    let body = RecoverSessionRequest {
        resume: resume_request(&opts.resume)?,
        apply: opts.apply,
        close_calls: opts.close_calls,
        predecessor_ref: opts.predecessor.clone(),
    };
    let mut daemon = Daemon::spawn(&opts.resume).await?;
    let resp: RecoverSessionResponse =
        call(&mut daemon, RecoverSessionRequest::METHOD, &body).await?;
    eprint!("{}", recover_report(&opts.resume.session_ref, &resp));
    notice_unrunnable(&resp.fallback_unrunnable);
    daemon
        .finish(resp.session_id.as_deref(), opts.resume.prompt.as_deref())
        .await
}

/// What `mu recover` tells the operator: the plan (or what was done), in
/// the words of the flags that change it.
pub fn recover_report(session_ref: &str, resp: &RecoverSessionResponse) -> String {
    let fork = fork_label(resp.branched_at_event_id);
    let mut out = String::new();
    if resp.is_clean() {
        out.push_str(&format!(
            "{session_ref} is not damaged: it forks cleanly at event {fork} with {} \
             message(s).\n",
            resp.seeded_message_count
        ));
        if !resp.applied {
            out.push_str(&format!("`mu resume {session_ref}` resumes it as it is.\n"));
        }
    } else if let Some(damage) = &resp.damage {
        out.push_str(&format!(
            "{session_ref}: ragged past event {fork}: {damage}\n"
        ));
        out.push_str("  that part stays in the log and is not inherited\n");
    } else {
        out.push_str(&format!("{session_ref}:\n"));
    }
    if let Some(from) = &resp.rebuilt_from {
        out.push_str(&format!(
            "  inherited history rebuilt from {from} up to the fork\n"
        ));
    }
    if !resp.closed_calls.is_empty() {
        let closes = if resp.applied {
            "closed"
        } else {
            "would close"
        };
        out.push_str(&format!(
            "  {closes} {} open tool call(s) with an error result: {}\n",
            resp.closed_calls.len(),
            resp.closed_calls.join(", ")
        ));
    }
    if resp.torn_lines > 0 {
        out.push_str(&format!(
            "  {} line(s) at the end were cut off mid-write; they are unreadable and skipped\n",
            resp.torn_lines
        ));
    }
    match resp.head_ref.as_ref().or(resp.session_id.as_ref()) {
        Some(head) => out.push_str(&format!(
            "recovered {session_ref} as {head} (forked at event {fork}, seeded {} message(s))\n",
            resp.seeded_message_count
        )),
        None if resp.is_clean() => {}
        None => {
            out.push_str(&format!(
                "  the new head would fork at event {fork} with {} message(s)\n",
                resp.seeded_message_count
            ));
            if resp.closed_calls.is_empty() && resp.damage.is_some() {
                out.push_str(
                    "  a turn cut off mid-call is run again from there; --close-calls instead \
                     answers open tool calls with an error so they are not re-run\n",
                );
            }
            out.push_str("run again with --apply to start it\n");
        }
    }
    out
}

fn fork_label(event_id: Option<u64>) -> String {
    event_id
        .map(|i| i.to_string())
        .unwrap_or_else(|| "<start>".into())
}

fn notice_unrunnable(unrunnable: &[String]) {
    if !unrunnable.is_empty() {
        crate::ask::notice(&format!("fallback cannot use: {}", unrunnable.join("; ")));
    }
}

fn resume_request(opts: &ResumeOptions) -> Result<ResumeSessionRequest> {
    // Resolve a possible SELECTION alias before the wire mapping (mu-eb98
    // item 2): a favorite name/alias as --model rewrites {provider, model}.
    let (provider, model) =
        crate::serve::resolve_launch_selection(&opts.provider, opts.model.as_deref());
    let selector = crate::serve::selector_from_cli(&provider, model.as_deref())?;
    Ok(ResumeSessionRequest {
        session_ref: opts.session_ref.clone(),
        provider: selector,
        attenuations: None,
        cwd: std::env::current_dir().ok(),
        actor: None,
        // mu-io71s: the `mu resume` CLI is the operator's explicit intent to
        // resume with the launch capability — opt in to the grant. Without
        // this, a cross-daemon resume fails closed to read_only (the mu-mh4
        // invariant) and every tool call is refused.
        grant_launch_capability: true,
        // The `mu resume` CLI doesn't forward autonomy config (that's a
        // mu-solo feature) — None → Disallowed (the root default).
        autonomy: None,
        role: opts.role.clone(),
    })
}

async fn call<B: serde::Serialize, R: serde::de::DeserializeOwned>(
    daemon: &mut Daemon,
    method: &str,
    body: &B,
) -> Result<R> {
    let id = daemon.next_id;
    daemon.next_id += 1;
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": body,
    });
    write_line(&mut daemon.stdin, &req).await?;

    // mu-mh4 (panel finding 2): bound the read loop. Without this, an
    // unresponsive daemon (hung, deadlocked, etc.) hangs the CLI forever
    // — no terminal frame ever arrives, so a bare `loop` blocks
    // indefinitely. Mirror ask.rs's `timeout()` discipline; the window is
    // generous (the project prefers long timeouts over premature failure,
    // and resume waits on a fresh `mu serve` plus the lazy parse of the
    // predecessor's one log — mu-lazy-session-rehydration-bh4f).
    let stdout = &mut daemon.stdout;
    let read_loop = async {
        loop {
            let line = read_line(stdout).await?;
            if line.get("id") == Some(&Value::from(id)) {
                if let Some(error) = line.get("error") {
                    // The daemon's refusal message carries the precise
                    // diagnosis and the remedy; a typed kind leads it, for
                    // a caller that branches on it rather than on prose.
                    let msg = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("(no message)");
                    match error
                        .get("data")
                        .and_then(|d| d.get("kind"))
                        .and_then(|k| k.as_str())
                    {
                        Some(kind) => bail!("[{kind}] {msg}"),
                        None => bail!("{msg}"),
                    }
                }
                let result = line
                    .get("result")
                    .cloned()
                    .ok_or_else(|| anyhow!("{method} response missing `result`"))?;
                return serde_json::from_value(result)
                    .with_context(|| format!("parse the {method} response"));
            }
            // Skip unrelated notifications.
        }
    };
    match timeout(Duration::from_secs(120), read_loop).await {
        Ok(res) => res,
        Err(_) => bail!(
            "{method} timed out after 120s waiting for the daemon to respond \
             (the daemon may be hung, or the log is very large to parse)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ragged() -> RecoverSessionResponse {
        RecoverSessionResponse {
            applied: false,
            predecessor_session_id: "s1".into(),
            branched_at_event_id: Some(5),
            seeded_message_count: 3,
            damage: Some("tool call `c1` has no result".into()),
            closed_calls: Vec::new(),
            rebuilt_from: None,
            torn_lines: 0,
            session_id: None,
            head_ref: None,
            fallback_unrunnable: Vec::new(),
        }
    }

    #[test]
    fn a_diagnosis_says_what_apply_would_do_and_how_to_change_it() {
        let text = recover_report("d:s1", &ragged());
        assert!(text.contains("ragged past event 5"), "{text}");
        assert!(text.contains("not inherited"), "{text}");
        assert!(text.contains("--close-calls"), "{text}");
        assert!(text.contains("--apply"), "{text}");
    }

    #[test]
    fn an_applied_recovery_names_the_new_head() {
        let mut resp = ragged();
        resp.applied = true;
        resp.session_id = Some("session-9".into());
        resp.head_ref = Some("d2:session-9".into());
        let text = recover_report("d:s1", &resp);
        assert!(text.contains("recovered d:s1 as d2:session-9"), "{text}");
        assert!(!text.contains("--apply"), "{text}");
    }

    #[test]
    fn a_clean_log_points_at_resume() {
        let mut resp = ragged();
        resp.damage = None;
        let text = recover_report("d:s1", &resp);
        assert!(text.contains("is not damaged"), "{text}");
        assert!(text.contains("`mu resume d:s1`"), "{text}");
    }
}
