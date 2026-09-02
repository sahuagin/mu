//! Prompt-time memory hints — prompt-relevant recall injection (mu-pcvqx).
//!
//! Session-start memory injection ([`super::recall::SubprocessRecallProvider`])
//! shells `agent memory context --tier <tier>`: a FIXED, tier-based standing
//! dump that never sees the current prompt. That is why the `full` tier
//! injected task-irrelevant material every session and why the operator
//! dialed it back to the `identity` kernel. There is no prompt at session
//! creation to rank against, so the fix is not a threshold on that path —
//! it is a second, per-turn path.
//!
//! This module is that path. Each user turn, the last user-role message
//! (operator ask or autonomous iteration motivation) is run through
//! `agent memory recall <intent> --json --full` — the SCORED semantic path
//! the `memory_recall` tool already uses (measured on that path during
//! mu-316wl battery 3: relevant ≈ 0.72+, unrelated ≤ 0.53). Hits below
//! `min_score` are dropped and the survivors are injected as one compact
//! span ANCHORED to the user message they were ranked for — the
//! [`super::capability_hints`] discipline:
//!
//! - the span sits right after its `msg-{idx}-user` anchor and never moves,
//!   so the prefix through it stays byte-stable (cacheable) on later turns;
//! - a memory already present in context is never re-injected;
//! - when compaction drops the anchoring user span the hint goes with it and
//!   its memories become eligible again ("already in context" stopped being
//!   true);
//! - a turn that ranks to nothing records `text: None` so tool rounds of the
//!   same ask don't re-run the embedder.
//!
//! Sizing: top-3 default, per-item body truncation, hard byte cap
//! ([`MEMORY_HINT_MAX_BYTES`]) — bounded by construction, unlike the 15.9K
//! token wall the `full` tier produced. Opt-in (`[recall].memory_hints`)
//! because `agent memory recall` embeds the query through the configured
//! embedder (~1-2 s measured); `--bare` and `MU_NO_RECALL` force it off.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::context::capability_hints::anchor_span_id;
use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};

/// Prefix for injected memory-hint span ids; the anchoring message index
/// is appended (see [`memory_hint_span_id`]).
pub const MEMORY_HINT_SPAN_PREFIX: &str = "memory-hint";

/// Hard byte ceiling on one rendered hint. The formatter drops entries
/// rather than exceed it — injection must never become the wall it exists
/// to replace.
pub const MEMORY_HINT_MAX_BYTES: usize = 2400;

/// Default recall timeout. `agent memory recall` embeds the query: measured
/// 1.0-2.1 s warm, 8.6 s on the first call after idle (the embedder loads
/// cold) — so the default must cover a cold load or the feature silently
/// drops out exactly on a session's first turn. Bounded so a hung CLI can
/// never stall a turn indefinitely; tunable via
/// `[recall].memory_hints_timeout_ms`.
pub const MEMORY_HINT_DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// The CLI's own cap on `--k`; also the deepest the rank goes.
const MAX_K: usize = 20;
/// Query text cap: the intent rides as one argv and through the embedder;
/// a multi-KB prompt file carries no extra retrieval signal past this.
const QUERY_MAX_CHARS: usize = 4000;

const NAME_MAX_CHARS: usize = 80;
const DESC_MAX_CHARS: usize = 200;
const BODY_MAX_CHARS: usize = 480;

#[derive(Clone)]
pub struct MemoryHints {
    binary_path: PathBuf,
    limit: usize,
    min_score: f32,
    max_bytes: usize,
    timeout: Duration,
    warned_about_missing_binary: Arc<AtomicBool>,
}

impl std::fmt::Debug for MemoryHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryHints")
            .field("binary_path", &self.binary_path)
            .field("limit", &self.limit)
            .field("min_score", &self.min_score)
            .field("max_bytes", &self.max_bytes)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl MemoryHints {
    pub fn new(binary_path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
            limit: 3,
            min_score: 0.60,
            max_bytes: MEMORY_HINT_MAX_BYTES,
            timeout: MEMORY_HINT_DEFAULT_TIMEOUT,
            warned_about_missing_binary: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `~/.local/bin/agent`, falling back to a bare `agent` (PATH lookup)
    /// if `$HOME` is unset — same resolution as the session-start provider
    /// and the `memory_recall` tool.
    pub fn default_binary() -> Self {
        let path = dirs::home_dir()
            .map(|h| h.join(".local").join("bin").join("agent"))
            .unwrap_or_else(|| PathBuf::from("agent"));
        Self::new(path)
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.clamp(1, MAX_K);
        self
    }

    /// Relevance floor on the CLI's `score` (rank v1: cosine × trust ×
    /// freshness). The CLI has no `--min-score` for memory recall, so the
    /// cut is applied here, client-side.
    pub fn with_min_score(mut self, min_score: f32) -> Self {
        self.min_score = min_score;
        self
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    /// Wall-clock cap on one `agent memory recall` call; on expiry the
    /// child is killed and the turn proceeds without a hint. A zero
    /// timeout is nonsensical and clamps to 1 s.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_secs(1));
        self
    }

    pub fn min_score(&self) -> f32 {
        self.min_score
    }

    /// Run `agent memory recall` for `intent` and render the hint, skipping
    /// memory ids in `already` (in context from an earlier anchored hint).
    /// Returns the text and the ids it names. All failures degrade to no
    /// hint: prompt-time recall must never block the turn with an error.
    ///
    /// **Blocks** on the child process; async callers wrap it in
    /// `spawn_blocking`.
    pub fn render_for_intent(
        &self,
        intent: &str,
        already: &HashSet<String>,
    ) -> Option<(String, Vec<String>)> {
        let query = truncate(intent.trim(), QUERY_MAX_CHARS);
        if query.is_empty() {
            return None;
        }
        // Rank deeper than `limit`: the score floor and dedup both remove
        // candidates, and asking for exactly `limit` would leave the hint
        // short whenever the top hits are already in context.
        let depth = self.limit.saturating_mul(4).clamp(8, MAX_K);

        let mut command = Command::new(&self.binary_path);
        command
            .arg("memory")
            .arg("recall")
            .arg(&query)
            .arg("--json")
            .arg("--k")
            .arg(depth.to_string())
            .arg("--full")
            // NOT inherited. `spawn()` inherits the parent's stdin, and under
            // `mu ask` the daemon's stdin is the JSON-RPC pipe: the CLI reads
            // stdin when it is not a tty and blocks until EOF, which never
            // comes — measured 40 s (until the pipe closed) vs 2.9 s with
            // stdin null. That hang is what the first acceptance runs of
            // mu-pcvqx saw as "recall timed out" at 6 s and again at 15 s.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Same posture as kx hints: the CLI resolves its embedder key from
        // its own config and never needs metered provider creds, so scrub
        // them — an ambient key must not be able to route paid calls
        // through the subprocess.
        for (key, _) in std::env::vars() {
            if key.starts_with("ANTHROPIC")
                || key == "OPENROUTER_API_KEY"
                || key == "CLAUDE_CODE_USE_BEDROCK"
                || key == "CLAUDE_CODE_USE_VERTEX"
            {
                command.env_remove(key);
            }
        }
        let output = match run_drained_with_timeout(&mut command, self.timeout) {
            Ok(Some(o)) => o,
            Ok(None) => {
                tracing::warn!(
                    binary = %self.binary_path.display(),
                    timeout_ms = self.timeout.as_millis(),
                    "MemoryHints: agent memory recall timed out; omitting hint",
                );
                return None;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !self
                    .warned_about_missing_binary
                    .swap(true, Ordering::Relaxed)
                {
                    tracing::warn!(
                        binary = %self.binary_path.display(),
                        "MemoryHints: agent CLI not found; prompt-time memory recall disabled",
                    );
                }
                return None;
            }
            Err(e) => {
                tracing::warn!(
                    binary = %self.binary_path.display(),
                    error = %e,
                    "MemoryHints: failed to run agent CLI",
                );
                return None;
            }
        };
        if !output.status.success() {
            let stderr_excerpt: String = String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(200)
                .collect();
            tracing::warn!(
                binary = %self.binary_path.display(),
                status = ?output.status.code(),
                stderr = %stderr_excerpt,
                "MemoryHints: agent memory recall exited non-zero",
            );
            return None;
        }

        let parsed: MemoryRecallOutput = match serde_json::from_slice(&output.stdout) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    binary = %self.binary_path.display(),
                    error = %e,
                    "MemoryHints: agent memory recall returned non-JSON",
                );
                return None;
            }
        };
        if let Some(error) = parsed.error.as_deref() {
            tracing::debug!(error, "MemoryHints: agent memory recall reported an error");
        }
        let rendered = format_hint(
            &parsed.results,
            &RenderOptions {
                limit: self.limit,
                min_score: self.min_score,
                already,
                max_bytes: self.max_bytes,
            },
        );
        tracing::debug!(
            candidates = parsed.results.len(),
            injected = rendered.as_ref().map(|(_, ids)| ids.len()).unwrap_or(0),
            min_score = self.min_score,
            "MemoryHints: ranked intent",
        );
        rendered
    }
}

/// Spawn `command`, drain its stdout/stderr CONCURRENTLY, and wait at most
/// `timeout`. `Ok(None)` ⇒ timed out: the direct child is killed and
/// reaped, the reader threads are DETACHED (not joined) and finish at
/// EOF. Only the direct child is signalled — a grandchild that inherited
/// the pipes (a `sh` wrapper's helper) can outlive the timeout and hold
/// the readers until it exits; that is a bounded thread leak, not a
/// stalled turn, and the same posture as kx_hints.
///
/// Why not poll `try_wait` and then `wait_with_output`: nothing reads the
/// pipes while polling, so a child whose output exceeds the pipe buffer
/// blocks on write and never exits — `--k 12 --full` returns whole memory
/// bodies (tens of KB), well past a 16–64 KB pipe. That deadlock is what
/// the mu-pcvqx acceptance runs saw as "timed out" after the stdin fix.
/// Reader threads keep the pipes flowing; on timeout the kill gives them
/// EOF, so the join cannot hang.
pub(crate) fn run_drained_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::io::Read;
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                // Timed out: kill and reap the child, but do NOT join the
                // readers. Killing the direct child doesn't close pipe ends
                // held by its grandchildren (a `sh` wrapper's `sleep`, a
                // CLI's helper), so a join here would wait for THEIR exit.
                // The output is discarded on timeout anyway; the detached
                // readers finish at EOF whenever that comes.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok(Some(std::process::Output {
        status,
        stdout,
        stderr,
    }))
}

#[derive(Debug, Deserialize)]
struct MemoryRecallOutput {
    #[serde(default)]
    results: Vec<MemoryHit>,
    #[serde(default)]
    error: Option<String>,
}

/// One hit from `agent memory recall --json`. Only the fields the hint
/// renders; the CLI emits more (cosine, tags, lifecycle, timestamps).
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryHit {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Body — present with `--full`.
    #[serde(default)]
    pub content: Option<String>,
    /// Rank score (v1: cosine × trust × freshness), the thresholded value.
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Testimony label ("recorded <date> · never verified" etc.).
    #[serde(default)]
    pub trust: Option<String>,
}

impl MemoryHit {
    /// Dedup key: the store id, or the name when the CLI emitted none.
    pub fn key(&self) -> &str {
        if self.id.trim().is_empty() {
            &self.name
        } else {
            &self.id
        }
    }
}

/// Knobs for one render.
#[derive(Debug)]
pub struct RenderOptions<'a> {
    pub limit: usize,
    pub min_score: f32,
    /// Memory keys already present in this session's context; never
    /// re-injected (see the module doc).
    pub already: &'a HashSet<String>,
    pub max_bytes: usize,
}

/// Render scored hits as the compact hint, returning the text and the
/// memory keys it names. `None` when nothing clears the floor or everything
/// is already in context — no match means no injection, never noise.
/// Public for tests; production goes through
/// [`MemoryHints::render_for_intent`].
pub fn format_hint(hits: &[MemoryHit], opts: &RenderOptions<'_>) -> Option<(String, Vec<String>)> {
    let floor = f64::from(opts.min_score);
    let mut out = format!(
        "[memory hints — memories auto-recalled for this turn (score ≥ {}); testimony, \
         not ground truth: verify before relying on it]",
        (floor * 100.0).round() as i64
    );
    let mut keys: Vec<String> = Vec::new();
    for hit in hits
        .iter()
        .filter(|h| h.score.is_some_and(|s| s >= floor))
        .filter(|h| !opts.already.contains(h.key()))
        .take(opts.limit.max(1))
    {
        let score = hit
            .score
            .map(|s| format!("{}", (s * 100.0).round() as i64))
            .unwrap_or_else(|| "?".to_string());
        let kind = hit
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(|k| format!("({k}) "))
            .unwrap_or_default();
        let desc = collapse(hit.description.as_deref().unwrap_or(""));
        let trust = hit.trust.as_deref().map(str::trim).unwrap_or("");
        let mut line = format!(
            "\n• [{score}] {kind}{}",
            truncate(&collapse(&hit.name), NAME_MAX_CHARS)
        );
        if !desc.is_empty() {
            line.push_str(" — ");
            line.push_str(&truncate(&desc, DESC_MAX_CHARS));
        }
        if !trust.is_empty() {
            line.push_str(&format!(" [{trust}]"));
        }
        // The body is where the fact lives; the description is a one-line
        // summary. Skip it when it adds nothing over the description.
        let body = collapse(hit.content.as_deref().unwrap_or(""));
        if !body.is_empty() && body != desc {
            line.push_str("\n  ");
            line.push_str(&truncate(&body, BODY_MAX_CHARS));
        }
        if out.len() + line.len() > opts.max_bytes {
            break;
        }
        out.push_str(&line);
        keys.push(hit.key().to_string());
    }
    (!keys.is_empty()).then_some((out, keys))
}

/// Collapse runs of whitespace (memory bodies are multi-line markdown) so
/// a hit renders as one compact block.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

/// One hint injected into this session's context, anchored to the user
/// message it was ranked for. `text: None` records a turn that ranked to
/// nothing (below the floor, or all already in context) — kept so the loop
/// doesn't re-run the embedder on every tool round of the same ask.
#[derive(Clone, Debug, PartialEq)]
pub struct InjectedMemoryHint {
    /// Index into the agent's `messages` of the anchoring user message
    /// (matches the `msg-{idx}-user` span id).
    pub anchor_msg_idx: usize,
    pub text: Option<String>,
    /// Memory keys named by this hint — the dedup key set.
    pub keys: Vec<String>,
}

/// Span id of the injected memory hint for the message at `msg_idx`.
pub fn memory_hint_span_id(msg_idx: usize) -> String {
    format!("{MEMORY_HINT_SPAN_PREFIX}-{msg_idx}")
}

/// Return a rope with each hint span inserted immediately after the user
/// span it anchors to. Hints whose anchor is absent (compacted away) are
/// skipped; the caller prunes them via
/// [`super::capability_hints::rope_has_anchor`].
pub fn with_memory_hints(rope: &RetainedRope, hints: &[InjectedMemoryHint]) -> RetainedRope {
    if hints.iter().all(|h| h.text.is_none()) {
        return rope.clone();
    }
    let spans = rope.spans();
    let mut out: Vec<Span> = Vec::with_capacity(spans.len() + hints.len());
    for span in spans {
        let anchored = if span.kind == SpanKind::User {
            hints
                .iter()
                .find(|h| h.text.is_some() && anchor_span_id(h.anchor_msg_idx) == *span.id)
        } else {
            None
        };
        out.push(span.clone());
        if let Some(h) = anchored {
            out.push(Span::new(
                memory_hint_span_id(h.anchor_msg_idx),
                SpanKind::User,
                h.text.as_deref().unwrap_or_default(),
                RetentionClass::Hot,
            ));
        }
    }
    RetainedRope::from_spans(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentMessage, ToolSpec};
    use crate::context::assemble_rope;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn hit(id: &str, score: f64) -> MemoryHit {
        MemoryHit {
            id: id.to_string(),
            name: format!("mem-{id}"),
            description: Some(format!("summary of {id}")),
            content: Some(format!("the {id} fact is 42\nsecond line")),
            score: Some(score),
            kind: Some("project".to_string()),
            trust: Some("recorded 2026-09-01 · never verified".to_string()),
        }
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    fn opts<'a>(limit: usize, min_score: f32, already: &'a HashSet<String>) -> RenderOptions<'a> {
        RenderOptions {
            limit,
            min_score,
            already,
            max_bytes: MEMORY_HINT_MAX_BYTES,
        }
    }

    #[test]
    fn format_hint_keeps_only_hits_at_or_above_the_floor() {
        // The mu-316wl measurement: relevant ~0.72, noise 0.52-0.53. A 0.60
        // floor keeps exactly the relevant one.
        let hits = vec![hit("rel", 0.72), hit("n1", 0.53), hit("n2", 0.52)];
        let seen = none();
        let (text, keys) = format_hint(&hits, &opts(5, 0.60, &seen)).expect("hint");
        assert_eq!(keys, vec!["rel".to_string()]);
        assert!(text.starts_with("[memory hints"));
        assert!(text.contains("• [72] (project) mem-rel — summary of rel"));
        assert!(text.contains("never verified"));
        // Body collapsed onto one indented line, newline folded.
        assert!(text.contains("\n  the rel fact is 42 second line"));
        assert!(!text.contains("mem-n1"));
    }

    #[test]
    fn format_hint_none_when_nothing_clears_the_floor() {
        let hits = vec![hit("n1", 0.53), hit("n2", 0.52)];
        let seen = none();
        assert_eq!(format_hint(&hits, &opts(5, 0.60, &seen)), None);
        // A hit with no score never injects — unscored is unranked.
        let mut unscored = hit("u", 0.9);
        unscored.score = None;
        assert_eq!(format_hint(&[unscored], &opts(5, 0.0, &seen)), None);
    }

    #[test]
    fn format_hint_skips_memories_already_in_context() {
        let hits = vec![hit("a", 0.80), hit("b", 0.75), hit("c", 0.70)];
        let seen: HashSet<String> = ["a".to_string()].into_iter().collect();
        let (text, keys) = format_hint(&hits, &opts(2, 0.60, &seen)).expect("hint");
        assert_eq!(keys, vec!["b".to_string(), "c".to_string()]);
        assert!(!text.contains("mem-a"));
        // Everything already aboard ⇒ nothing to say.
        let all: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(format_hint(&hits, &opts(2, 0.60, &all)), None);
    }

    #[test]
    fn format_hint_respects_limit_and_byte_cap() {
        let hits: Vec<MemoryHit> = (0..6)
            .map(|i| hit(&format!("m{i}"), 0.9 - i as f64 * 0.01))
            .collect();
        let seen = none();
        let (_, keys) = format_hint(&hits, &opts(3, 0.60, &seen)).expect("hint");
        assert_eq!(keys.len(), 3);
        // A cap too small for even one entry ⇒ None, never a partial line.
        let tight = RenderOptions {
            limit: 3,
            min_score: 0.60,
            already: &seen,
            max_bytes: 120,
        };
        assert_eq!(format_hint(&hits, &tight), None);
    }

    #[test]
    fn hit_key_falls_back_to_name_without_id() {
        let mut h = hit("", 0.9);
        h.name = "named-only".to_string();
        assert_eq!(h.key(), "named-only");
        assert_eq!(hit("abc", 0.9).key(), "abc");
    }

    #[test]
    fn inserts_after_the_anchoring_user_span_only() {
        let rope = assemble_rope(
            Some("system"),
            &[
                AgentMessage::User {
                    content: "first".into(),
                },
                AgentMessage::User {
                    content: "second".into(),
                },
            ],
            &[ToolSpec::new("read", "read", json!({}))],
        );
        let hints = vec![
            InjectedMemoryHint {
                anchor_msg_idx: 0,
                text: Some("hint-0".into()),
                keys: vec!["a".into()],
            },
            InjectedMemoryHint {
                anchor_msg_idx: 1,
                text: None,
                keys: Vec::new(),
            },
        ];
        let rope = with_memory_hints(&rope, &hints);
        let ids: Vec<&str> = rope.spans().iter().map(|s| s.id()).collect();
        let u0 = ids.iter().position(|id| *id == "msg-0-user").unwrap();
        assert_eq!(ids[u0 + 1], memory_hint_span_id(0));
        assert!(ids.contains(&"msg-1-user"));
        assert!(
            !ids.iter().any(|id| *id == memory_hint_span_id(1)),
            "a text-less record injects nothing"
        );
        assert_eq!(
            ids.iter()
                .filter(|id| id.starts_with(MEMORY_HINT_SPAN_PREFIX))
                .count(),
            1
        );
    }

    #[test]
    fn no_text_hints_leave_the_rope_untouched() {
        let rope = assemble_rope(
            Some("system"),
            &[AgentMessage::User {
                content: "hello".into(),
            }],
            &[],
        );
        let hints = vec![InjectedMemoryHint {
            anchor_msg_idx: 0,
            text: None,
            keys: Vec::new(),
        }];
        let out = with_memory_hints(&rope, &hints);
        assert_eq!(out.spans().len(), rope.spans().len());
    }

    fn stub_binary(name: &str, body: &str) -> PathBuf {
        let pid = std::process::id();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("mu-memhint-test-{pid}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join(name);
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Retry on the Unix ETXTBSY fork/exec race (same pattern as the
    /// session-start provider's tests): a sibling test's stub write-fd can
    /// be briefly open when this process forks.
    fn render_retrying(
        hints: &MemoryHints,
        intent: &str,
        already: &HashSet<String>,
    ) -> Option<(String, Vec<String>)> {
        for _ in 0..3 {
            if let Some(r) = hints.render_for_intent(intent, already) {
                return Some(r);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        hints.render_for_intent(intent, already)
    }

    #[test]
    fn render_for_intent_invokes_agent_memory_recall_json_full_and_filters() {
        // Stub echoes argv into the description of the top hit so the test
        // can assert what reached the CLI, and emits one sub-floor hit.
        let script = stub_binary(
            "agent",
            r#"#!/bin/sh
printf '%s' "{\"results\":[{\"id\":\"aaa\",\"name\":\"zephyr\",\"description\":\"argv: $*\",\"content\":\"tuning constant 7\",\"score\":0.73,\"type\":\"project\",\"trust\":\"recorded\"},{\"id\":\"bbb\",\"name\":\"noise\",\"description\":\"unrelated\",\"score\":0.52}]}"
"#,
        );
        let hints = MemoryHints::new(&script).with_limit(2).with_min_score(0.60);
        let seen = none();
        let (text, keys) = render_retrying(&hints, "Zephyr tuning constant", &seen).expect("hint");
        assert_eq!(keys, vec!["aaa".to_string()]);
        assert!(
            text.contains("argv: memory recall Zephyr tuning constant --json --k 8 --full"),
            "args must reach the CLI verbatim (depth = limit*4 min 8); got: {text}"
        );
        assert!(
            text.contains("tuning constant 7"),
            "body must inject; got: {text}"
        );
        assert!(
            !text.contains("noise"),
            "sub-floor hit must not inject; got: {text}"
        );
    }

    #[test]
    fn render_for_intent_degrades_to_none() {
        let seen = none();
        // Missing binary.
        let missing = MemoryHints::new("/this/path/does/not/exist/agent-memhint");
        assert_eq!(missing.render_for_intent("anything", &seen), None);
        // Non-zero exit.
        assert_eq!(
            MemoryHints::new("/bin/false").render_for_intent("x", &seen),
            None
        );
        // Non-JSON stdout.
        let junk = stub_binary("agent-junk", "#!/bin/sh\necho 'not json'\n");
        assert_eq!(render_retrying(&MemoryHints::new(&junk), "x", &seen), None);
        // Empty intent never spawns.
        assert_eq!(
            MemoryHints::new(&junk).render_for_intent("   ", &seen),
            None
        );
    }

    /// The child must get a NULL stdin, never the parent's. A stub that
    /// drains stdin before answering returns immediately on null stdin and
    /// hangs (until the timeout) on an inherited open pipe — which is what
    /// the daemon's JSON-RPC stdin is under `mu ask`.
    #[test]
    fn child_stdin_is_null_so_a_stdin_reading_cli_cannot_hang() {
        let script = stub_binary(
            "agent-drain",
            "#!/bin/sh\ncat > /dev/null\nprintf '%s' '{\"results\":[{\"id\":\"a\",\"name\":\"a\",\"score\":0.9}]}'\n",
        );
        let hints = MemoryHints::new(&script).with_timeout(Duration::from_secs(3));
        let start = Instant::now();
        let out = render_retrying(&hints, "x", &none());
        assert!(out.is_some(), "stub must answer once stdin hits EOF");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "an inherited stdin would have hung to the timeout"
        );
    }

    /// Output larger than any pipe buffer must not deadlock: the reader
    /// threads drain while the child runs. A 512 KB JSON payload (whole
    /// memory bodies at `--k 12 --full` are tens of KB; this is the same
    /// failure with margin) completes well inside the timeout.
    #[test]
    fn large_output_is_drained_not_deadlocked() {
        let script = stub_binary(
            "agent-big",
            "#!/bin/sh\nbig=$(head -c 524288 /dev/zero | tr '\\0' 'x')\nprintf '%s' \"{\\\"results\\\":[{\\\"id\\\":\\\"a\\\",\\\"name\\\":\\\"a\\\",\\\"description\\\":\\\"d\\\",\\\"content\\\":\\\"$big\\\",\\\"score\\\":0.9}]}\"\n",
        );
        let hints = MemoryHints::new(&script).with_timeout(Duration::from_secs(5));
        let start = Instant::now();
        let out = render_retrying(&hints, "x", &none());
        assert!(out.is_some(), "large output must be drained and parsed");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "an undrained pipe would have hung to the timeout"
        );
    }

    #[test]
    fn timeout_kills_a_hung_recall_and_yields_none() {
        let script = stub_binary("agent-hang", "#!/bin/sh\nsleep 30\n");
        let hints = MemoryHints::new(&script).with_timeout(Duration::from_secs(1));
        let start = Instant::now();
        assert_eq!(hints.render_for_intent("x", &none()), None);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "hung child must be killed at the timeout, not waited out"
        );
    }

    #[test]
    fn limit_clamps_to_cli_cap() {
        let h = MemoryHints::new("agent").with_limit(0);
        assert_eq!(h.limit, 1);
        let h = MemoryHints::new("agent").with_limit(500);
        assert_eq!(h.limit, MAX_K);
    }
}
