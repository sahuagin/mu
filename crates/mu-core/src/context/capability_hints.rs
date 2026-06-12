//! Implicit capability discovery — mu-uz0n layer 1.
//!
//! Discovery loses the tool-choice auction when it's opt-in: the model
//! already believes it knows its tools, grep/bash have massive training
//! priors, `discover` has none, and the cost of ignorance is invisible
//! (observed live: sessions working ON t4c never called discover,
//! session 7517502faa5f7ed2). Always-AVAILABLE is not always-USED.
//!
//! So the daemon stops waiting to be asked: each turn, the user's
//! message (or the autonomous iteration motivation — whatever the last
//! user-role message is) is run through the same ranking the `discover`
//! tool uses, and the top-N hits are INJECTED as a compact hint span —
//! the same push-not-pull posture as session-start recall providers.
//!
//! ## Injection sizing (the 21k-wall gate)
//!
//! The hint is machine-view compact — one line per capability, path +
//! truncated summary — and hard-capped at [`HINT_MAX_BYTES`]. Top-3
//! default is ~300-400 bytes ≈ ~100 tokens. Compare the measured 15.9K
//! token full-memory wall (config.rs, session c76f6949) this repo
//! already walked back once.
//!
//! ## Cache discipline
//!
//! The hint span is inserted immediately AFTER the last `User` span
//! (not at the rope tail): within an ask, tool rounds append assistant/
//! tool-result spans after it, so its position — and everything before
//! it — is byte-stable across rounds and the cacheable prefix is
//! untouched. Across asks the previous hint vanishes (it is transient,
//! never part of [`super::super::agent::AgentMessage`] history), which
//! invalidates at most the previous turn boundary — the bounded price
//! of not accumulating stale hints forever.

use std::sync::Arc;

use crate::agent::Tool;
use crate::capability::Capability;
use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};
use crate::skill::loader::LoadedSkill;
use crate::t4c_source::{self, CapabilityView};

/// Stable rope span id for the injected hint. One id, content replaced
/// per turn — `ContextAssembly::prefix_span_hashes` then names the
/// mutation if it ever needs diagnosing.
pub const HINT_SPAN_ID: &str = "capability-hint";

/// Hard byte ceiling on the rendered hint. The formatter drops entries
/// rather than exceed it — injection must never become the wall it
/// exists to replace.
pub const HINT_MAX_BYTES: usize = 700;

/// Per-entry summary truncation (chars).
const SUMMARY_MAX_CHARS: usize = 90;

/// Per-session wiring for implicit discovery, carried on
/// `AgentConfig::discover_hints`. `None` there ⇒ feature off (the
/// default; tests and pre-mu-uz0n behavior). The daemon wires `Some`
/// from `[index].discover_injection` at session creation.
#[derive(Clone)]
pub struct DiscoverHints {
    /// Daemon-discovered skills — same set the `discover` tool ranks.
    /// The agent loop doesn't otherwise hold skills, so they ride in.
    pub skills: Arc<Vec<LoadedSkill>>,
    /// Top-N entries to inject. Keep small; see module doc.
    pub limit: usize,
}

impl std::fmt::Debug for DiscoverHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoverHints")
            .field("skills", &self.skills.len())
            .field("limit", &self.limit)
            .finish()
    }
}

/// Rank `intent` against the session's capability surface (same
/// manifest the `discover` tool builds: tools + skills + host catalog,
/// permission-attenuated) and render the compact hint. `None` when the
/// intent is empty or nothing scores above zero — no match means no
/// injection, never noise.
///
/// Lexical ranking only: per-turn cost must stay micro. Semantic
/// ranking remains the `discover` tool's opt-in path
/// (`[index].semantic_discover`).
pub fn rank_hint(
    tools: &[Arc<dyn Tool>],
    capability: &Capability,
    skills: &[LoadedSkill],
    intent: &str,
    limit: usize,
) -> Option<String> {
    if intent.trim().is_empty() {
        return None;
    }
    let registry = t4c_source::build_manifest_for_tools(tools, capability, skills);
    // Manifest-build failure ⇒ no hint, never an error: the injection
    // is best-effort sugar on the turn, not load-bearing.
    let tree = registry.build().ok()?;
    let views = t4c_source::discover_view(&tree, intent, limit.max(1));
    format_hint(&views, limit)
}

/// Render ranked views as the compact machine-view hint. Public for
/// tests; production goes through [`rank_hint`].
pub fn format_hint(views: &[CapabilityView], limit: usize) -> Option<String> {
    let mut out = String::from(
        "[capability hints — auto-ranked against this turn; \
         call `discover` with your intent for the full list]",
    );
    let mut entries = 0usize;
    for v in views
        .iter()
        .filter(|v| v.score > 0.0 && v.allowed_by_session)
        .take(limit)
    {
        let summary: String = v.summary.chars().take(SUMMARY_MAX_CHARS).collect();
        let line = format!("\n• {} — {}", v.path, summary.trim());
        if out.len() + line.len() > HINT_MAX_BYTES {
            break;
        }
        out.push_str(&line);
        entries += 1;
    }
    (entries > 0).then_some(out)
}

/// Return a rope with the hint span inserted immediately after the
/// LAST `User` span (see module doc for why that position). A rope
/// with no user span comes back unchanged — there is no turn to hint.
pub fn with_hint_after_last_user(rope: &RetainedRope, hint: &str) -> RetainedRope {
    let spans = rope.spans();
    let Some(pos) = spans.iter().rposition(|s| matches!(s.kind, SpanKind::User)) else {
        return rope.clone();
    };
    let mut out: Vec<Span> = Vec::with_capacity(spans.len() + 1);
    out.extend_from_slice(&spans[..=pos]);
    out.push(Span::new(
        HINT_SPAN_ID,
        SpanKind::User,
        hint,
        RetentionClass::Hot,
    ));
    out.extend_from_slice(&spans[pos + 1..]);
    RetainedRope::from_spans(out)
}

/// mu-uz0n layer 2 — the error-path hook. When the model invents a
/// nonexistent tool name, the moment of failure is the one moment it
/// is receptive: rank the bad name against the real surface and name
/// the near-misses. Returns `None` when nothing scores (the bare
/// "tool not found" stands alone).
pub fn suggest_for_unknown_tool(tools: &[Arc<dyn Tool>], name: &str) -> Option<String> {
    let tree = t4c_source::build_manifest(tools, &[]).build().ok()?;
    let views = t4c_source::discover_view(&tree, name, 3);
    let paths: Vec<&str> = views
        .iter()
        .filter(|v| v.score > 0.0)
        .take(3)
        .map(|v| v.path.as_str())
        .collect();
    (!paths.is_empty()).then(|| paths.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(path: &str, summary: &str, score: f64) -> CapabilityView {
        CapabilityView {
            path: path.to_string(),
            summary: summary.to_string(),
            keywords: Vec::new(),
            score,
            effects: None,
            allowed_by_session: true,
            disallowed_reason: None,
            source: None,
        }
    }

    #[test]
    fn format_hint_is_compact_and_limited() {
        let views = vec![
            view("tool.read", "Read file contents", 0.9),
            view("skill.code-index", "semantic + lexical code recall", 0.7),
            view("bash.rg", "ripgrep", 0.5),
            view("tool.write", "should be cut by limit", 0.4),
        ];
        let hint = format_hint(&views, 3).expect("hint");
        assert!(hint.starts_with("[capability hints"));
        assert!(hint.contains("• tool.read — Read file contents"));
        assert!(hint.contains("• skill.code-index"));
        assert!(hint.contains("• bash.rg"));
        assert!(!hint.contains("tool.write"), "limit must cap entries");
        assert!(hint.len() <= HINT_MAX_BYTES, "hint exceeded byte cap");
    }

    #[test]
    fn format_hint_skips_zero_scores_and_disallowed() {
        let mut blocked = view("tool.spawn", "spawn workers", 0.9);
        blocked.allowed_by_session = false;
        let views = vec![blocked, view("tool.noise", "no match", 0.0)];
        assert_eq!(
            format_hint(&views, 3),
            None,
            "no qualifying entries ⇒ no hint"
        );
    }

    #[test]
    fn format_hint_enforces_byte_cap() {
        let views: Vec<CapabilityView> = (0..50)
            .map(|i| view(&format!("tool.t{i}"), &"x".repeat(SUMMARY_MAX_CHARS), 1.0))
            .collect();
        let hint = format_hint(&views, 50).expect("hint");
        assert!(
            hint.len() <= HINT_MAX_BYTES,
            "byte cap must hold at any limit"
        );
    }

    #[test]
    fn hint_span_lands_after_last_user_span() {
        let rope = RetainedRope::from_spans(vec![
            Span::new(
                "sys",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new("u1", SpanKind::User, "first", RetentionClass::Hot),
            Span::new("a1", SpanKind::Assistant, "reply", RetentionClass::Hot),
            Span::new("u2", SpanKind::User, "second", RetentionClass::Hot),
            Span::new("a2", SpanKind::Assistant, "working", RetentionClass::Hot),
            Span::new("t1", SpanKind::ToolResult, "{}", RetentionClass::Hot),
        ]);
        let out = with_hint_after_last_user(&rope, "[capability hints] • tool.read");
        let ids: Vec<&str> = out.spans().iter().map(|s| s.id.as_ref()).collect();
        assert_eq!(ids, vec!["sys", "u1", "a1", "u2", HINT_SPAN_ID, "a2", "t1"]);
        let hint = &out.spans()[4];
        assert!(matches!(hint.kind, SpanKind::User), "must render user-role");
    }

    #[test]
    fn rope_without_user_span_is_unchanged() {
        let rope = RetainedRope::from_spans(vec![Span::new(
            "sys",
            SpanKind::System,
            "you are mu",
            RetentionClass::Startup,
        )]);
        let out = with_hint_after_last_user(&rope, "hint");
        assert_eq!(out.spans(), rope.spans());
    }
}
