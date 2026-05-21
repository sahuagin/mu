//! Hash-and-summary compaction policy — single judge-model call,
//! output is `{ keep: [hash], summary: string }`.
//!
//! Per mu-kgu.3 (the operator-designed differentiator vs Anthropic's
//! full-model summarize-and-replace):
//!
//! 1. Build a span index: each span gets a stable short hex hash
//!    (default 8 chars, expanded to 12 on collision) + its
//!    `SpanKind` + a short content preview + the full content.
//! 2. Prompt the judge model with the full span sequence and ask
//!    for `{ keep: [hash], summary: "..." }`.
//! 3. Surgery: kept hashes preserved verbatim; everything else is
//!    absorbed into a single new [`SpanKind::CompactionSummary`]
//!    span placed at the position of the earliest absorbed span.
//!
//! ## Fail-closed contract
//!
//! If any of: hash collisions that survive the 8→12 expansion, judge
//! call errors, malformed judge output, or judge keep-list entries
//! that do not match any span hash — the policy MUST return the
//! **original rope unchanged**, with one
//! [`CompactionDecision::Failed`] entry recording the reason.
//! Compaction NEVER silently drops spans.
//!
//! ## Testability
//!
//! The policy holds an `Arc<dyn Judge>` rather than a `Provider`
//! directly: keeps the trait surface narrow (one prompt → one string)
//! and lets unit tests substitute deterministic mocks without
//! reaching for an async runtime. Wiring a real provider into a
//! `Judge` is a thin bridge owned by the agent-loop integration bead
//! (mu-kgu.4).
//!
//! The hash function is also pluggable (`Arc<dyn SpanHasher>`); the
//! default uses `blake3` truncated to 8 hex chars, automatically
//! expanding to 12 on detected collisions. Tests can inject a
//! deterministic mock hasher to exercise the collision-detection
//! path without needing to engineer real cryptographic collisions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{CompactionDecision, CompactionPolicy, CompactionResult};
use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanId, SpanKind};

/// Default short hex length (mu-kgu.3 design: 8 hex = 32 bits ≈
/// 10⁻⁴ collision probability at 1000 spans).
pub const DEFAULT_HASH_SHORT_CHARS: usize = 8;
/// Fallback hex length used after a collision is detected at
/// [`DEFAULT_HASH_SHORT_CHARS`]. 12 hex = 48 bits ≈ 10⁻⁹ at the same
/// scale.
pub const DEFAULT_HASH_LONG_CHARS: usize = 12;
/// Default characters of `span.content` to include in the per-span
/// preview that the judge sees. Spans whose content is shorter than
/// this just get full content (no truncation marker).
pub const DEFAULT_PREVIEW_CHARS: usize = 120;
/// Stable identifier embedded in every [`SpanKind::CompactionSummary`]
/// span this policy produces.
pub const DEFAULT_POLICY_ID: &str = "hash-and-summary-v1";

/// Result of a judge call.
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    /// The underlying judge call (HTTP, model, transport) failed.
    #[error("judge call failed: {0}")]
    Call(String),
}

/// One judge: takes a prompt, returns the model's raw text response.
///
/// Synchronous on purpose — the [`CompactionPolicy`] trait is
/// synchronous (see `compaction.rs` module docs) and async work is
/// the implementor's problem to block on. mu-kgu.4 will provide a
/// `ProviderJudge` adapter that bridges from `Provider::stream()` to
/// this synchronous shape via a tokio runtime block-on.
pub trait Judge: Send + Sync {
    /// Submit `prompt` to the judge model. Returns raw response text
    /// (typically JSON; the policy parses it).
    fn judge(&self, prompt: &str) -> Result<String, JudgeError>;
}

/// Hash a [`Span`] to a `n_chars`-long lowercase hex string.
///
/// Pluggable so tests can force-collide without engineering real
/// blake3 collisions.
pub trait SpanHasher: Send + Sync {
    fn hash(&self, span: &Span, n_chars: usize) -> String;
}

/// Production hasher: `blake3(span.id ‖ "\0" ‖ span.content)`
/// truncated to `n_chars` hex characters. Including `span.id` in the
/// hash input means duplicate-content spans get distinct hashes (ids
/// are unique within a rope) — so only the random-collision birthday
/// case ever fires the 8→12 expansion path.
#[derive(Debug, Default, Clone, Copy)]
pub struct Blake3Hasher;

impl SpanHasher for Blake3Hasher {
    fn hash(&self, span: &Span, n_chars: usize) -> String {
        let mut input = Vec::with_capacity(span.id.len() + 1 + span.content.len());
        input.extend_from_slice(span.id.as_bytes());
        input.push(0);
        input.extend_from_slice(span.content.as_bytes());
        let h = blake3::hash(&input);
        let hex = h.to_hex();
        hex.as_str()[..n_chars.min(hex.len())].to_string()
    }
}

/// Output mode for the judge's keep-list (mu-kgu.7 rung-B optimization).
///
/// - [`HashKeep`] — rung-A (default): judge emits an array of 8-char
///   hex hashes. Stable across compaction passes; auditable.
/// - [`IndexKeep`] — rung-B: judge emits an array of 1-based integer
///   indices into the input span sequence. ~6x smaller on the keep-list
///   output token count vs hashes; pass-local (no cross-pass identity).
///   mu maps indices → spans server-side and retains hashes for
///   provenance in CompactionDecision::Kept.
///
/// Both modes preserve verbatim content and the same surgery outcome;
/// only the JSON shape the model emits differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepListMode {
    /// Judge emits hashes (e.g. `["abc12345", "def67890"]`). Default.
    #[default]
    HashKeep,
    /// Judge emits 1-based indices (e.g. `[1, 4, 7]`).
    IndexKeep,
}

/// Hash-and-summary compaction policy.
///
/// Holds a judge (the model that will pick kept hashes + produce the
/// summary), a hasher (defaults to [`Blake3Hasher`]), and tuning
/// constants. See module docs for the full algorithm.
#[derive(Clone)]
pub struct HashAndSummaryPolicy {
    judge: Arc<dyn Judge>,
    hasher: Arc<dyn SpanHasher>,
    preview_chars: usize,
    hash_short_chars: usize,
    hash_long_chars: usize,
    policy_id: String,
    output_mode: KeepListMode,
}

impl HashAndSummaryPolicy {
    /// Build a policy with default hasher, preview length, hash
    /// lengths, and policy id. The judge is supplied by the caller —
    /// production wires this from `Provider::compaction_policy()`
    /// (mu-kgu.4); tests inject mocks.
    pub fn new(judge: Arc<dyn Judge>) -> Self {
        Self {
            judge,
            hasher: Arc::new(Blake3Hasher),
            preview_chars: DEFAULT_PREVIEW_CHARS,
            hash_short_chars: DEFAULT_HASH_SHORT_CHARS,
            hash_long_chars: DEFAULT_HASH_LONG_CHARS,
            policy_id: DEFAULT_POLICY_ID.to_string(),
            output_mode: KeepListMode::default(),
        }
    }

    /// mu-kgu.7: opt into IndexKeep output mode — judge emits integer
    /// indices instead of hashes. ~6x smaller keep-list output.
    pub fn with_output_mode(mut self, mode: KeepListMode) -> Self {
        self.output_mode = mode;
        self
    }

    /// Override the hasher (primarily for tests).
    pub fn with_hasher(mut self, hasher: Arc<dyn SpanHasher>) -> Self {
        self.hasher = hasher;
        self
    }

    /// Override the preview length included in the judge prompt.
    pub fn with_preview_chars(mut self, n: usize) -> Self {
        self.preview_chars = n;
        self
    }

    /// Override the short/long hash truncation lengths.
    pub fn with_hash_chars(mut self, short: usize, long: usize) -> Self {
        self.hash_short_chars = short;
        self.hash_long_chars = long;
        self
    }

    /// Override the policy id embedded in CompactionSummary spans.
    pub fn with_policy_id(mut self, id: impl Into<String>) -> Self {
        self.policy_id = id.into();
        self
    }
}

impl std::fmt::Debug for HashAndSummaryPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashAndSummaryPolicy")
            .field("preview_chars", &self.preview_chars)
            .field("hash_short_chars", &self.hash_short_chars)
            .field("hash_long_chars", &self.hash_long_chars)
            .field("policy_id", &self.policy_id)
            .finish_non_exhaustive()
    }
}

/// What the judge is asked to emit. Hash-mode shape. Deserialized
/// via serde; extra fields are ignored.
#[derive(Debug, Clone, Deserialize)]
struct JudgeOutputHash {
    keep: Vec<String>,
    summary: String,
}

/// Index-mode shape (mu-kgu.7 rung-B). Same as `JudgeOutputHash` but
/// the `keep` array contains 1-based integer indices.
#[derive(Debug, Clone, Deserialize)]
struct JudgeOutputIndex {
    keep: Vec<usize>,
    summary: String,
}

impl CompactionPolicy for HashAndSummaryPolicy {
    fn compact(&self, rope: &RetainedRope, _target_tokens: usize) -> CompactionResult {
        let start = Instant::now();
        let spans = rope.spans();
        if spans.is_empty() {
            return CompactionResult::identity(rope.clone());
        }

        // Step 1: hash every span; expand to long-chars on collision.
        let hashes = match compute_hashes(
            self.hasher.as_ref(),
            spans,
            self.hash_short_chars,
            self.hash_long_chars,
        ) {
            Ok(h) => h,
            Err(reason) => return failed(rope, reason, start),
        };

        // Step 2: build the judge prompt (shape depends on output mode).
        let prompt = build_prompt(spans, &hashes, self.preview_chars, self.output_mode);

        // Step 3: judge call.
        let raw = match self.judge.judge(&prompt) {
            Ok(s) => s,
            Err(e) => return failed(rope, format!("judge call: {e}"), start),
        };

        // Step 4: parse JSON (shape depends on output mode) and resolve
        // the keep list into a HashSet<usize> of span indices.
        let (keep_indices, summary) = match self.output_mode {
            KeepListMode::HashKeep => {
                let parsed = match parse_judge_output_hash(&raw) {
                    Ok(p) => p,
                    Err(reason) => return failed(rope, reason, start),
                };
                // Resolve hashes → indices. Duplicates and unknowns
                // are both fail-closed.
                let mut hash_to_index: HashMap<&str, usize> = HashMap::with_capacity(hashes.len());
                for (i, h) in hashes.iter().enumerate() {
                    hash_to_index.insert(h.as_str(), i);
                }
                let mut keep: HashSet<usize> = HashSet::new();
                for entry in &parsed.keep {
                    match hash_to_index.get(entry.as_str()) {
                        Some(&i) => {
                            if !keep.insert(i) {
                                return failed(
                                    rope,
                                    format!("duplicate keep hash {entry:?}"),
                                    start,
                                );
                            }
                        }
                        None => {
                            return failed(rope, format!("unknown keep hash {entry:?}"), start);
                        }
                    }
                }
                (keep, parsed.summary)
            }
            KeepListMode::IndexKeep => {
                let parsed = match parse_judge_output_index(&raw) {
                    Ok(p) => p,
                    Err(reason) => return failed(rope, reason, start),
                };
                // Validate: indices must be 1-based, in-range, no dups.
                let mut keep: HashSet<usize> = HashSet::new();
                for &one_based in &parsed.keep {
                    if one_based == 0 || one_based > spans.len() {
                        return failed(
                            rope,
                            format!("keep index {one_based} out of range (1..={})", spans.len()),
                            start,
                        );
                    }
                    let zero_based = one_based - 1;
                    if !keep.insert(zero_based) {
                        return failed(rope, format!("duplicate keep index {one_based}"), start);
                    }
                }
                (keep, parsed.summary)
            }
        };

        // Step 5: surgery. Build the new rope, recording decisions.
        let (new_rope, decisions) = surgery(
            spans,
            &keep_indices,
            &summary,
            &self.policy_id,
            generated_at_unix_ms(),
        );

        let wall_clock_us = elapsed_ms(start);
        CompactionResult {
            rope: new_rope,
            decisions,
            tokens_before: estimate_tokens(spans),
            tokens_after: 0, // recomputed below
            wall_clock_us,
        }
        .with_tokens_after_recomputed()
    }

    /// mu-kgu.8: this policy is a candidate for background compaction.
    /// The judge call may take seconds (live Anthropic Haiku
    /// ~500-1500ms; Opus several seconds), and that latency should
    /// not block the foreground turn. The agent loop respects this
    /// flag by spawning `compact()` on a tokio task and continuing
    /// with the un-compacted rope until the result lands on a later
    /// turn.
    ///
    /// `is_async = true` regardless of whether the underlying judge
    /// is a `ProviderJudge` (live) or a `MockJudge` (test). The cost
    /// of taking the async path for a sync mock is one `tokio::spawn`
    /// — negligible. The benefit of NOT branching on judge type is a
    /// simpler trait surface.
    fn is_async(&self) -> bool {
        true
    }
}

/// Hash every span at `short`. If a collision is detected, retry at
/// `long`. Returns the chosen hash list. Fail-closed if collisions
/// persist at `long` (impossibly rare for blake3 at 12 hex with a
/// non-pathological hasher, but a real correctness boundary for
/// tests that inject a mock hasher).
fn compute_hashes(
    hasher: &dyn SpanHasher,
    spans: &[Span],
    short: usize,
    long: usize,
) -> Result<Vec<String>, String> {
    let short_hashes: Vec<String> = spans.iter().map(|s| hasher.hash(s, short)).collect();
    if !has_duplicate(&short_hashes) {
        return Ok(short_hashes);
    }
    let long_hashes: Vec<String> = spans.iter().map(|s| hasher.hash(s, long)).collect();
    if !has_duplicate(&long_hashes) {
        return Ok(long_hashes);
    }
    Err(format!(
        "hash collision persists at {long}-char width across {} spans",
        spans.len()
    ))
}

fn has_duplicate(hs: &[String]) -> bool {
    let mut seen = HashSet::with_capacity(hs.len());
    for h in hs {
        if !seen.insert(h.as_str()) {
            return true;
        }
    }
    false
}

fn build_prompt(
    spans: &[Span],
    hashes: &[String],
    preview_chars: usize,
    mode: KeepListMode,
) -> String {
    let mut out =
        String::with_capacity(spans.iter().map(|s| s.content.len()).sum::<usize>() + 1024);
    let (keep_shape, keep_example) = match mode {
        KeepListMode::HashKeep => ("hash strings (as shown after `hash=`)", "[\"<hash>\", ...]"),
        KeepListMode::IndexKeep => (
            "1-based integer indices (as shown in `#N` at the start of each block)",
            "[1, 4, 7]",
        ),
    };
    out.push_str(&format!(
        "You are a compaction judge. Given the following retained-rope spans, \
         decide which to preserve VERBATIM (`keep`) and write a SHORT \
         natural-language summary covering everything you did NOT keep.\n\n\
         Output ONLY a JSON object with this shape:\n\
         {{\"keep\": {keep_example}, \"summary\": \"<paragraph>\"}}\n\n\
         The `keep` array MUST contain {keep_shape}.\n\n\
         Spans (one per block):\n"
    ));
    for (i, (span, hash)) in spans.iter().zip(hashes.iter()).enumerate() {
        let preview = if span.content.chars().count() <= preview_chars {
            span.content.to_string()
        } else {
            let truncated: String = span.content.chars().take(preview_chars).collect();
            format!("{truncated}…")
        };
        let one_based = i + 1;
        out.push_str("---\n");
        out.push_str(&format!(
            "#{one_based} hash={hash} kind={kind} id={id}\n{preview}\n",
            kind = span_kind_label(&span.kind),
            id = span.id,
        ));
    }
    out
}

fn span_kind_label(kind: &SpanKind) -> &'static str {
    match kind {
        SpanKind::System => "system",
        SpanKind::User => "user",
        SpanKind::Assistant => "assistant",
        SpanKind::ToolResult => "tool_result",
        SpanKind::ToolCall => "tool_call",
        SpanKind::ToolSchema => "tool_schema",
        SpanKind::SkillActivation => "skill_activation",
        SpanKind::MemoryInjection => "memory_injection",
        SpanKind::Compaction => "compaction",
        SpanKind::CompactionSummary { .. } => "compaction_summary",
        SpanKind::FileLoad => "file_load",
    }
}

fn parse_judge_output_hash(s: &str) -> Result<JudgeOutputHash, String> {
    if let Ok(out) = serde_json::from_str::<JudgeOutputHash>(s.trim()) {
        return Ok(out);
    }
    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        if end > start {
            if let Ok(out) = serde_json::from_str::<JudgeOutputHash>(&s[start..=end]) {
                return Ok(out);
            }
        }
    }
    Err(format!(
        "judge output not parseable as {{keep:[hash], summary:string}}: {}",
        truncate_for_error(s)
    ))
}

fn parse_judge_output_index(s: &str) -> Result<JudgeOutputIndex, String> {
    if let Ok(out) = serde_json::from_str::<JudgeOutputIndex>(s.trim()) {
        return Ok(out);
    }
    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        if end > start {
            if let Ok(out) = serde_json::from_str::<JudgeOutputIndex>(&s[start..=end]) {
                return Ok(out);
            }
        }
    }
    Err(format!(
        "judge output not parseable as {{keep:[int], summary:string}}: {}",
        truncate_for_error(s)
    ))
}

fn truncate_for_error(s: &str) -> String {
    let max = 160;
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Build the post-compaction rope plus the decision audit log.
///
/// Spans whose index is in `keep_indices` are preserved verbatim and
/// emit `CompactionDecision::Kept`. The remaining spans are
/// "absorbed": removed from the new rope, replaced by ONE
/// [`SpanKind::CompactionSummary`] span placed at the earliest
/// absorbed-position, and a single `CompactionDecision::Summarized`
/// records the (absorbed_ids → summary_id) mapping. If no spans
/// were absorbed (keep == all), the rope shape is unchanged and no
/// summary span is inserted.
fn surgery(
    spans: &[Span],
    keep_indices: &HashSet<usize>,
    summary_text: &str,
    policy_id: &str,
    generated_at_unix_ms: u64,
) -> (RetainedRope, Vec<CompactionDecision>) {
    let mut decisions: Vec<CompactionDecision> = Vec::with_capacity(spans.len());
    let mut absorbed_ids: Vec<String> = Vec::new();
    let mut earliest_absorbed_idx: Option<usize> = None;
    for (i, span) in spans.iter().enumerate() {
        if keep_indices.contains(&i) {
            decisions.push(CompactionDecision::Kept {
                span_id: span.id.to_string(),
            });
        } else {
            if earliest_absorbed_idx.is_none() {
                earliest_absorbed_idx = Some(i);
            }
            absorbed_ids.push(span.id.to_string());
        }
    }

    if absorbed_ids.is_empty() {
        // Judge kept everything — return verbatim.
        let new_spans: Vec<Span> = spans.to_vec();
        return (RetainedRope::from_spans(new_spans), decisions);
    }

    let summary_span_id = format!("compaction-summary:{generated_at_unix_ms}");
    let absorbed_span_ids_arc: Vec<SpanId> =
        absorbed_ids.iter().map(|s| Arc::from(s.as_str())).collect();
    let summary_span = Span::new(
        summary_span_id.clone(),
        SpanKind::CompactionSummary {
            absorbed_span_ids: absorbed_span_ids_arc,
            generated_at_unix_ms,
            policy_id: policy_id.to_string(),
        },
        summary_text.to_string(),
        // CompactionSummary spans are pinned: they replace
        // previously-pinned context that the judge chose to compact
        // away. Operators can still demote them via other policies.
        RetentionClass::Pinned,
    );
    decisions.push(CompactionDecision::Summarized {
        absorbed_span_ids: absorbed_ids,
        summary_span_id: summary_span_id.clone(),
    });

    // Build the new rope: kept spans in original order, with the
    // single summary span inserted at the earliest-absorbed position.
    let insertion_idx = earliest_absorbed_idx.expect("absorbed_ids non-empty implies index set");
    let mut new_spans: Vec<Span> = Vec::with_capacity(keep_indices.len() + 1);
    let mut summary_inserted = false;
    for (i, span) in spans.iter().enumerate() {
        if i == insertion_idx {
            new_spans.push(summary_span.clone());
            summary_inserted = true;
        }
        if keep_indices.contains(&i) {
            new_spans.push(span.clone());
        }
    }
    if !summary_inserted {
        // Safety net: if insertion_idx is past the last kept span we
        // still want the summary in the output (e.g. all-tail
        // absorbed). Append.
        new_spans.push(summary_span);
    }
    (RetainedRope::from_spans(new_spans), decisions)
}

fn failed(rope: &RetainedRope, reason: String, start: Instant) -> CompactionResult {
    CompactionResult {
        rope: rope.clone(),
        decisions: vec![CompactionDecision::Failed { reason }],
        tokens_before: estimate_tokens(rope.spans()),
        tokens_after: estimate_tokens(rope.spans()),
        wall_clock_us: elapsed_ms(start),
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    let d = start.elapsed();
    d.as_micros().min(u64::MAX as u128) as u64
}

fn generated_at_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// Cross-policy shared estimator (delegates to
/// [`super::estimate_tokens`]). Local re-export so call sites in this
/// module stay terse; semantics are owned by the parent module so
/// heuristic + hash-summary report comparable numbers in benchmarks
/// (mu-kgu.5 metrics-cleanup).
use super::estimate_tokens;

impl CompactionResult {
    /// Internal helper: recompute `tokens_after` from the actual
    /// post-rope. The `compact()` body builds the result with
    /// `tokens_after == 0` then calls this so it doesn't have to
    /// thread the new-rope-byte-count separately.
    fn with_tokens_after_recomputed(mut self) -> Self {
        self.tokens_after = estimate_tokens(self.rope.spans());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mock judge that returns a canned response (Ok or Err).
    struct MockJudge {
        response: Mutex<Result<String, String>>,
    }

    impl MockJudge {
        fn ok(s: impl Into<String>) -> Arc<dyn Judge> {
            Arc::new(Self {
                response: Mutex::new(Ok(s.into())),
            })
        }
        fn err(s: impl Into<String>) -> Arc<dyn Judge> {
            Arc::new(Self {
                response: Mutex::new(Err(s.into())),
            })
        }
    }

    impl Judge for MockJudge {
        fn judge(&self, _prompt: &str) -> Result<String, JudgeError> {
            match self.response.lock().expect("mock lock").clone() {
                Ok(s) => Ok(s),
                Err(e) => Err(JudgeError::Call(e)),
            }
        }
    }

    /// Forcing-hasher: returns the first `n_chars` of an injected
    /// per-span hash. Lets tests exercise collision detection
    /// deterministically.
    struct ForceHasher {
        per_id: HashMap<String, String>,
    }

    impl SpanHasher for ForceHasher {
        fn hash(&self, span: &Span, n_chars: usize) -> String {
            let full = self
                .per_id
                .get(span.id())
                .cloned()
                .unwrap_or_else(|| format!("{:0>16}", span.id()));
            full[..n_chars.min(full.len())].to_string()
        }
    }

    fn sample_rope() -> RetainedRope {
        RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new("u1", SpanKind::User, "ping?", RetentionClass::Hot),
            Span::new("a1", SpanKind::Assistant, "pong", RetentionClass::Hot),
            Span::new(
                "t1",
                SpanKind::ToolResult,
                "{\"ok\":true,\"value\":42}",
                RetentionClass::Warm,
            ),
        ])
    }

    fn hashes_for(rope: &RetainedRope) -> Vec<String> {
        let h = Blake3Hasher;
        rope.spans()
            .iter()
            .map(|s| h.hash(s, DEFAULT_HASH_SHORT_CHARS))
            .collect()
    }

    #[test]
    fn blake3_hasher_is_deterministic_for_same_content() {
        let h = Blake3Hasher;
        let s1 = Span::new("x", SpanKind::User, "hello", RetentionClass::Hot);
        let s2 = Span::new("x", SpanKind::User, "hello", RetentionClass::Hot);
        assert_eq!(h.hash(&s1, 8), h.hash(&s2, 8));
        // Different id → different hash (id is included in input).
        let s3 = Span::new("y", SpanKind::User, "hello", RetentionClass::Hot);
        assert_ne!(h.hash(&s1, 8), h.hash(&s3, 8));
    }

    #[test]
    fn blake3_hasher_truncates_to_requested_width() {
        let h = Blake3Hasher;
        let s = Span::new("x", SpanKind::User, "hello", RetentionClass::Hot);
        assert_eq!(h.hash(&s, 8).len(), 8);
        assert_eq!(h.hash(&s, 12).len(), 12);
    }

    #[test]
    fn empty_rope_round_trips_to_identity_result() {
        let policy = HashAndSummaryPolicy::new(MockJudge::ok("{\"keep\":[],\"summary\":\"\"}"));
        let rope = RetainedRope::new();
        let result = policy.compact(&rope, 1_000);
        assert!(result.rope.is_empty());
        assert!(result.decisions.is_empty());
        assert_eq!(result.tokens_before, 0);
        assert_eq!(result.tokens_after, 0);
    }

    #[test]
    fn valid_judge_keeps_named_hashes_and_absorbs_the_rest() {
        let rope = sample_rope();
        let hs = hashes_for(&rope);
        // Keep the system + assistant spans; absorb user + tool_result.
        let keep = [&hs[0], &hs[2]];
        let response = format!(
            "{{\"keep\":[\"{}\",\"{}\"],\"summary\":\"the user pinged and a tool returned ok=true with value=42\"}}",
            keep[0], keep[1],
        );
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(response));

        let result = policy.compact(&rope, 1_000);

        // Rope: sys, [summary], a1
        let new_spans = result.rope.spans();
        assert_eq!(new_spans.len(), 3, "kept(2) + 1 summary span");
        assert_eq!(new_spans[0].id(), "sys");
        // Summary inserted at the position of the earliest absorbed
        // span (u1 was at index 1).
        match &new_spans[1].kind {
            SpanKind::CompactionSummary {
                absorbed_span_ids,
                policy_id,
                ..
            } => {
                let actual: Vec<&str> = absorbed_span_ids.iter().map(AsRef::as_ref).collect();
                assert_eq!(actual, vec!["u1", "t1"]);
                assert_eq!(policy_id, DEFAULT_POLICY_ID);
            }
            other => panic!("expected CompactionSummary, got {other:?}"),
        }
        assert_eq!(
            new_spans[1].content(),
            "the user pinged and a tool returned ok=true with value=42"
        );
        assert_eq!(new_spans[2].id(), "a1");

        // Decisions: Kept(sys), Kept(a1), Summarized([u1, t1] → summary).
        let kept_ids: Vec<&str> = result
            .decisions
            .iter()
            .filter_map(|d| match d {
                CompactionDecision::Kept { span_id } => Some(span_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(kept_ids, vec!["sys", "a1"]);
        let summarized: Vec<_> = result
            .decisions
            .iter()
            .filter_map(|d| match d {
                CompactionDecision::Summarized {
                    absorbed_span_ids,
                    summary_span_id,
                } => Some((absorbed_span_ids.clone(), summary_span_id.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(summarized.len(), 1);
        assert_eq!(summarized[0].0, vec!["u1".to_string(), "t1".to_string()]);

        // No Failed decisions on the happy path.
        assert!(!result
            .decisions
            .iter()
            .any(|d| matches!(d, CompactionDecision::Failed { .. })));
    }

    #[test]
    fn malformed_judge_output_returns_original_rope_and_failed_decision() {
        let rope = sample_rope();
        let policy = HashAndSummaryPolicy::new(MockJudge::ok("not-json at all"));

        let result = policy.compact(&rope, 1_000);

        // Rope unchanged.
        assert_eq!(result.rope.spans(), rope.spans());
        // Exactly one Failed decision.
        let failed: Vec<_> = result
            .decisions
            .iter()
            .filter_map(|d| match d {
                CompactionDecision::Failed { reason } => Some(reason.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(failed.len(), 1, "decisions = {:?}", result.decisions);
        assert!(failed[0].contains("not parseable"));
    }

    #[test]
    fn judge_call_error_fails_closed() {
        let rope = sample_rope();
        let policy = HashAndSummaryPolicy::new(MockJudge::err("HTTP 503 from judge"));

        let result = policy.compact(&rope, 1_000);

        assert_eq!(result.rope.spans(), rope.spans());
        let reason = result
            .decisions
            .iter()
            .find_map(|d| match d {
                CompactionDecision::Failed { reason } => Some(reason.as_str()),
                _ => None,
            })
            .expect("Failed decision");
        assert!(reason.contains("judge call"));
        assert!(reason.contains("HTTP 503"));
    }

    #[test]
    fn empty_keep_list_absorbs_every_span_into_one_summary() {
        let rope = sample_rope();
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(
            "{\"keep\":[],\"summary\":\"everything happened\"}",
        ));

        let result = policy.compact(&rope, 1_000);

        // Rope: just the summary span at the position of the earliest
        // absorbed span (index 0). No kept spans precede it.
        assert_eq!(result.rope.spans().len(), 1);
        match &result.rope.spans()[0].kind {
            SpanKind::CompactionSummary {
                absorbed_span_ids, ..
            } => {
                let actual: Vec<&str> = absorbed_span_ids.iter().map(AsRef::as_ref).collect();
                assert_eq!(actual, vec!["sys", "u1", "a1", "t1"]);
            }
            other => panic!("expected CompactionSummary, got {other:?}"),
        }
        assert_eq!(result.rope.spans()[0].content(), "everything happened");

        // No Kept decisions; one Summarized covering all four span ids.
        let kept_count = result
            .decisions
            .iter()
            .filter(|d| matches!(d, CompactionDecision::Kept { .. }))
            .count();
        assert_eq!(kept_count, 0);
        let summarized = result
            .decisions
            .iter()
            .find_map(|d| match d {
                CompactionDecision::Summarized {
                    absorbed_span_ids, ..
                } => Some(absorbed_span_ids.clone()),
                _ => None,
            })
            .expect("Summarized");
        assert_eq!(summarized.len(), 4);
    }

    #[test]
    fn keep_unknown_hash_fails_closed() {
        let rope = sample_rope();
        let policy =
            HashAndSummaryPolicy::new(MockJudge::ok("{\"keep\":[\"deadbeef\"],\"summary\":\"x\"}"));

        let result = policy.compact(&rope, 1_000);

        assert_eq!(result.rope.spans(), rope.spans());
        let reason = result
            .decisions
            .iter()
            .find_map(|d| match d {
                CompactionDecision::Failed { reason } => Some(reason.as_str()),
                _ => None,
            })
            .expect("Failed decision");
        assert!(reason.contains("unknown keep hash"));
    }

    #[test]
    fn hash_collision_at_short_expands_to_long_then_succeeds() {
        // Two spans hash to the same prefix at 4 chars (`abcd`) but
        // diverge at 8 (`abcd1111` vs `abcd2222`). The policy should
        // detect the collision at `short=4`, retry at `long=8`, and
        // proceed to a successful compaction.
        let rope = RetainedRope::from_spans(vec![
            Span::new("a", SpanKind::User, "alpha", RetentionClass::Hot),
            Span::new("b", SpanKind::Assistant, "beta", RetentionClass::Hot),
        ]);
        let forcing = ForceHasher {
            per_id: HashMap::from([
                ("a".to_string(), "abcd1111deadbeef".to_string()),
                ("b".to_string(), "abcd2222deadbeef".to_string()),
            ]),
        };
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(
            "{\"keep\":[\"abcd1111\"],\"summary\":\"absorbed beta\"}",
        ))
        .with_hasher(Arc::new(forcing))
        .with_hash_chars(4, 8);

        let result = policy.compact(&rope, 1_000);

        // No Failed: collision was resolved by the 4→8 expansion.
        assert!(
            !result
                .decisions
                .iter()
                .any(|d| matches!(d, CompactionDecision::Failed { .. })),
            "decisions = {:?}",
            result.decisions,
        );
        // Outcome: span "a" kept; span "b" absorbed; one summary span.
        let ids: Vec<&str> = result.rope.spans().iter().map(|s| s.id()).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "a");
        assert!(matches!(
            result.rope.spans()[1].kind,
            SpanKind::CompactionSummary { .. },
        ));
    }

    #[test]
    fn hash_collision_persisting_at_long_fails_closed() {
        // ForceHasher returns the SAME prefix at every width for both
        // spans — the 4→8 expansion can't resolve.
        let rope = RetainedRope::from_spans(vec![
            Span::new("a", SpanKind::User, "alpha", RetentionClass::Hot),
            Span::new("b", SpanKind::Assistant, "beta", RetentionClass::Hot),
        ]);
        let forcing = ForceHasher {
            per_id: HashMap::from([
                ("a".to_string(), "ffffffffffffffff".to_string()),
                ("b".to_string(), "ffffffffffffffff".to_string()),
            ]),
        };
        let policy =
            HashAndSummaryPolicy::new(MockJudge::ok("{\"keep\":[],\"summary\":\"ignored\"}"))
                .with_hasher(Arc::new(forcing))
                .with_hash_chars(4, 8);

        let result = policy.compact(&rope, 1_000);

        // Rope unchanged.
        assert_eq!(result.rope.spans(), rope.spans());
        let reason = result
            .decisions
            .iter()
            .find_map(|d| match d {
                CompactionDecision::Failed { reason } => Some(reason.as_str()),
                _ => None,
            })
            .expect("Failed decision");
        assert!(reason.contains("hash collision persists"));
    }

    #[test]
    fn duplicate_keep_hash_fails_closed() {
        let rope = sample_rope();
        let hs = hashes_for(&rope);
        let response = format!(
            "{{\"keep\":[\"{}\",\"{}\"],\"summary\":\"x\"}}",
            hs[0], hs[0],
        );
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(response));

        let result = policy.compact(&rope, 1_000);
        assert_eq!(result.rope.spans(), rope.spans());
        let reason = result
            .decisions
            .iter()
            .find_map(|d| match d {
                CompactionDecision::Failed { reason } => Some(reason.as_str()),
                _ => None,
            })
            .expect("Failed decision");
        assert!(reason.contains("duplicate keep hash"));
    }

    #[test]
    fn keep_all_returns_kept_decisions_and_no_summary_span() {
        let rope = sample_rope();
        let hs = hashes_for(&rope);
        let response = format!(
            "{{\"keep\":[\"{}\",\"{}\",\"{}\",\"{}\"],\"summary\":\"unused\"}}",
            hs[0], hs[1], hs[2], hs[3],
        );
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(response));

        let result = policy.compact(&rope, 1_000);

        // Rope shape unchanged.
        assert_eq!(result.rope.spans().len(), 4);
        // No summary span anywhere.
        for span in result.rope.spans() {
            assert!(
                !matches!(span.kind, SpanKind::CompactionSummary { .. }),
                "unexpected summary span for keep-all path",
            );
        }
        // Decisions: 4 Kept, no Summarized, no Failed.
        assert_eq!(
            result
                .decisions
                .iter()
                .filter(|d| matches!(d, CompactionDecision::Kept { .. }))
                .count(),
            4,
        );
        assert!(result
            .decisions
            .iter()
            .all(|d| matches!(d, CompactionDecision::Kept { .. })));
    }

    #[test]
    fn judge_output_inside_markdown_fences_still_parses() {
        let rope = sample_rope();
        let hs = hashes_for(&rope);
        let response = format!(
            "Sure, here's the JSON:\n```json\n{{\"keep\":[\"{}\"],\"summary\":\"rest\"}}\n```\n",
            hs[0],
        );
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(response));
        let result = policy.compact(&rope, 1_000);
        // Should NOT be a Failed result — fallback brace-extraction
        // picks up the JSON.
        assert!(
            !result
                .decisions
                .iter()
                .any(|d| matches!(d, CompactionDecision::Failed { .. })),
            "decisions = {:?}",
            result.decisions,
        );
        // Kept "sys", absorbed the other three.
        assert_eq!(result.rope.spans()[0].id(), "sys");
    }

    #[test]
    fn policy_is_send_sync_as_arc_trait_object() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<HashAndSummaryPolicy>();
        let _arc: Arc<dyn CompactionPolicy> = Arc::new(HashAndSummaryPolicy::new(MockJudge::ok(
            "{\"keep\":[],\"summary\":\"\"}",
        )));
    }

    // ========================================================================
    // mu-kgu.7: rung-B IndexKeep mode tests
    // ========================================================================

    #[test]
    fn rung_b_valid_index_keep_resolves_to_correct_spans() {
        // Keep #1 (sys) and #3 (a1) via integer indices — absorbs #2,#4.
        let rope = sample_rope();
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(
            "{\"keep\":[1,3],\"summary\":\"user pinged and tool returned ok\"}",
        ))
        .with_output_mode(KeepListMode::IndexKeep);

        let result = policy.compact(&rope, 1_000);

        let new_spans = result.rope.spans();
        assert_eq!(new_spans.len(), 3, "kept(2) + 1 summary span");
        assert_eq!(new_spans[0].id(), "sys");
        assert!(matches!(
            new_spans[1].kind,
            SpanKind::CompactionSummary { .. }
        ));
        assert_eq!(new_spans[2].id(), "a1");
        // No Failed decision.
        assert!(!result
            .decisions
            .iter()
            .any(|d| matches!(d, CompactionDecision::Failed { .. })));
    }

    #[test]
    fn rung_b_empty_keep_absorbs_everything() {
        let rope = sample_rope();
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(
            "{\"keep\":[],\"summary\":\"all rolled into one\"}",
        ))
        .with_output_mode(KeepListMode::IndexKeep);

        let result = policy.compact(&rope, 1_000);

        // One CompactionSummary span replacing the entire rope.
        let new_spans = result.rope.spans();
        assert_eq!(new_spans.len(), 1);
        assert!(matches!(
            new_spans[0].kind,
            SpanKind::CompactionSummary { .. }
        ));
    }

    #[test]
    fn rung_b_out_of_range_index_fails_closed() {
        let rope = sample_rope(); // 4 spans → valid indices are 1..=4
        let policy = HashAndSummaryPolicy::new(MockJudge::ok("{\"keep\":[1,5],\"summary\":\"x\"}"))
            .with_output_mode(KeepListMode::IndexKeep);

        let result = policy.compact(&rope, 1_000);

        // Fail-closed: original rope preserved + Failed decision.
        assert_eq!(result.rope.spans(), rope.spans());
        let failed_reason = result
            .decisions
            .iter()
            .find_map(|d| match d {
                CompactionDecision::Failed { reason } => Some(reason.clone()),
                _ => None,
            })
            .expect("Failed decision expected");
        assert!(
            failed_reason.contains("out of range"),
            "got: {failed_reason}"
        );
    }

    #[test]
    fn rung_b_zero_index_fails_closed() {
        // 0 is not a valid 1-based index → fail-closed.
        let rope = sample_rope();
        let policy = HashAndSummaryPolicy::new(MockJudge::ok("{\"keep\":[0],\"summary\":\"x\"}"))
            .with_output_mode(KeepListMode::IndexKeep);

        let result = policy.compact(&rope, 1_000);
        assert_eq!(result.rope.spans(), rope.spans());
        assert!(result
            .decisions
            .iter()
            .any(|d| matches!(d, CompactionDecision::Failed { .. })));
    }

    #[test]
    fn rung_b_duplicate_index_fails_closed() {
        let rope = sample_rope();
        let policy = HashAndSummaryPolicy::new(MockJudge::ok("{\"keep\":[1,1],\"summary\":\"x\"}"))
            .with_output_mode(KeepListMode::IndexKeep);

        let result = policy.compact(&rope, 1_000);
        assert_eq!(result.rope.spans(), rope.spans());
        let failed_reason = result
            .decisions
            .iter()
            .find_map(|d| match d {
                CompactionDecision::Failed { reason } => Some(reason.clone()),
                _ => None,
            })
            .expect("Failed decision expected");
        assert!(failed_reason.contains("duplicate"), "got: {failed_reason}");
    }

    #[test]
    fn rung_b_malformed_keep_array_fails_closed() {
        // Strings where indices were expected.
        let rope = sample_rope();
        let policy =
            HashAndSummaryPolicy::new(MockJudge::ok("{\"keep\":[\"abc\"],\"summary\":\"x\"}"))
                .with_output_mode(KeepListMode::IndexKeep);

        let result = policy.compact(&rope, 1_000);
        assert_eq!(result.rope.spans(), rope.spans());
        assert!(result
            .decisions
            .iter()
            .any(|d| matches!(d, CompactionDecision::Failed { .. })));
    }

    #[test]
    fn rung_b_prompt_includes_indexed_markers() {
        // Verify the prompt is shaped to elicit integer indices.
        let rope = sample_rope();
        let hashes = hashes_for(&rope);
        let prompt = build_prompt(rope.spans(), &hashes, 80, KeepListMode::IndexKeep);
        assert!(prompt.contains("#1 hash="), "should include #1 prefix");
        assert!(prompt.contains("#2 hash="), "should include #2 prefix");
        assert!(
            prompt.contains("1-based integer indices"),
            "instruction should name the shape",
        );
        assert!(prompt.contains("[1, 4, 7]"), "shape example present");
    }

    #[test]
    fn rung_a_prompt_includes_hash_shape_instruction() {
        // Default mode (HashKeep) instructs the judge to emit hashes.
        let rope = sample_rope();
        let hashes = hashes_for(&rope);
        let prompt = build_prompt(rope.spans(), &hashes, 80, KeepListMode::HashKeep);
        assert!(prompt.contains("hash strings"), "shape descriptor");
        assert!(prompt.contains("[\"<hash>\", ...]"), "shape example");
    }

    #[test]
    fn rung_b_output_mode_default_is_hash_keep() {
        // Constructed without override → HashKeep (backward-compatible).
        let policy = HashAndSummaryPolicy::new(MockJudge::ok("{\"keep\":[],\"summary\":\"\"}"));
        assert_eq!(policy.output_mode, KeepListMode::HashKeep);
    }

    #[test]
    fn compaction_preserves_blocks_field_on_kept_assistant_spans() {
        // mu-yqeq.3 acceptance: post-compaction non-absorbed spans
        // retain their blocks field. Phase C wire adapters depend on
        // this — if a kept assistant span loses its blocks, the
        // adapter can't reconstruct tool calls.
        use crate::agent::types::{ContentBlock, ToolCall};

        let original_blocks = vec![
            ContentBlock::Text {
                text: "calling read".into(),
            },
            ContentBlock::ToolCall(ToolCall {
                id: "toolu_kept".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "/x"}),
            }),
        ];
        // Build a rope by hand: sys + an assistant span carrying blocks
        // + a volatile user span that we expect compaction to absorb.
        let assistant_span = Span::with_cacheable(
            "msg-0-assistant",
            SpanKind::Assistant,
            "calling read [tool_call:read({\"path\":\"/x\"})]",
            RetentionClass::Hot,
            false,
        )
        .with_blocks(original_blocks.clone());
        let pre_rope = RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            assistant_span,
            Span::with_cacheable(
                "msg-1-user",
                SpanKind::User,
                "stale",
                RetentionClass::Warm,
                false,
            ),
        ]);

        // Judge keeps sys + assistant; absorbs the volatile user.
        let hasher = Blake3Hasher;
        let hashes: Vec<String> = pre_rope
            .spans()
            .iter()
            .map(|s| hasher.hash(s, DEFAULT_HASH_SHORT_CHARS))
            .collect();
        let response = format!(
            "{{\"keep\":[\"{}\",\"{}\"],\"summary\":\"absorbed stale user\"}}",
            hashes[0], hashes[1],
        );
        let policy = HashAndSummaryPolicy::new(MockJudge::ok(response));
        let result = policy.compact(&pre_rope, 1_000);

        // The kept assistant span retains its blocks verbatim.
        let kept_assistant = result
            .rope
            .spans()
            .iter()
            .find(|s| s.id() == "msg-0-assistant")
            .expect("assistant span survived compaction");
        let kept_blocks = kept_assistant
            .blocks()
            .expect("kept assistant span still carries blocks");
        assert_eq!(kept_blocks, original_blocks.as_slice());

        // The newly-inserted CompactionSummary span has no blocks
        // (none were supplied at synthesis).
        let summary = result
            .rope
            .spans()
            .iter()
            .find(|s| matches!(s.kind(), SpanKind::CompactionSummary { .. }))
            .expect("summary span inserted");
        assert!(
            summary.blocks().is_none(),
            "CompactionSummary span carries no blocks (Phase A scope; structural reconstruction \
             from kept-span blocks happens at the wire adapter layer)",
        );
    }
}
