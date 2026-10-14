//! Regression coverage for mu-hint-baseline-duplication-7noj5.
//! Hint-bearing compaction baselines must be safe inputs to the injectors.

use super::capability_hints::{self, InjectedHint};
use super::compaction::heuristic::SpanFamilyDropPolicy;
use super::memory_hints::{self, InjectedMemoryHint};
use super::{kx_hints, CompactionPolicy, RetainedRope, RetentionClass, Span, SpanKind};

fn base() -> RetainedRope {
    RetainedRope::from_spans(vec![
        Span::new(
            "msg-0-user",
            SpanKind::User,
            "same text",
            RetentionClass::Hot,
        ),
        Span::new(
            "msg-1-user",
            SpanKind::User,
            "same text",
            RetentionClass::Hot,
        ),
    ])
}

fn memory(rope: &RetainedRope) -> RetainedRope {
    memory_hints::with_memory_hints(
        rope,
        &[InjectedMemoryHint {
            anchor_msg_idx: 0,
            text: Some("same text".into()),
            keys: vec!["memory-key".into()],
        }],
    )
}

fn capability(rope: &RetainedRope) -> RetainedRope {
    capability_hints::with_hints(
        rope,
        &[InjectedHint {
            anchor_msg_idx: 0,
            text: Some("same text".into()),
            paths: vec!["tool.read".into()],
        }],
    )
}

fn kx(rope: &RetainedRope) -> RetainedRope {
    kx_hints::with_kx_hint_after_last_user(rope, "same text")
}

fn all(rope: &RetainedRope) -> RetainedRope {
    // Same order as the agent loop.
    memory(&kx(&capability(rope)))
}

#[test]
fn hint_injectors_are_idempotent_and_repair_known_duplicate_spans() {
    for inject in [memory, capability, kx, all] {
        let once = inject(&base());
        assert_eq!(inject(&once).spans(), once.spans());

        // Simulate an already-bloated baseline. Real user messages have the
        // SAME text as hints; only synthetic IDs may be collapsed.
        let mut damaged = Vec::new();
        for span in once.spans() {
            damaged.push(span.clone());
            if !span.id().starts_with("msg-") {
                damaged.extend(std::iter::repeat_n(span.clone(), 12));
            }
        }
        let repaired = inject(&RetainedRope::from_spans(damaged));
        assert_eq!(repaired.spans(), once.spans());
        assert_eq!(
            repaired
                .spans()
                .iter()
                .filter(|s| s.id().starts_with("msg-"))
                .count(),
            2
        );
    }
}

#[test]
fn hints_do_not_accumulate_across_compaction_baseline_cycles() {
    for inject in [memory, capability, kx, all] {
        let expected = inject(&base());
        let mut baseline = expected.clone();
        for cycle in 0..20 {
            // Old assistants provide real eviction candidates; a zero target
            // forces the policy through every tier, retaining User hints.
            let mut spans = baseline.spans().to_vec();
            for index in 0..3 {
                spans.push(Span::new(
                    format!("assistant-{cycle}-{index}"),
                    SpanKind::Assistant,
                    "disposable work",
                    RetentionClass::Hot,
                ));
            }
            let compacted =
                SpanFamilyDropPolicy::new().compact(&RetainedRope::from_spans(spans), 0);
            baseline = inject(&compacted.rope);
            let users: Vec<_> = baseline
                .spans()
                .iter()
                .filter(|s| s.kind == SpanKind::User)
                .cloned()
                .collect();
            assert_eq!(users, expected.spans(), "cycle {cycle}");
        }
    }
}

#[test]
fn existing_anchored_hints_keep_their_position_and_content() {
    for inject in [memory, capability] {
        let once = inject(&base());
        let mut spans = once.spans().to_vec();
        spans.insert(
            1,
            Span::new("other", SpanKind::User, "unrelated", RetentionClass::Hot),
        );
        let reordered = RetainedRope::from_spans(spans);
        assert_eq!(inject(&reordered).spans(), reordered.spans());
    }
}

#[test]
fn kx_replaces_stale_hint_after_the_new_user() {
    let mut spans = kx(&base()).spans().to_vec();
    spans.push(Span::new(
        "msg-2-user",
        SpanKind::User,
        "new intent",
        RetentionClass::Hot,
    ));
    let updated =
        kx_hints::with_kx_hint_after_last_user(&RetainedRope::from_spans(spans), "new hint");
    assert_eq!(
        updated
            .spans()
            .iter()
            .filter(|s| s.id() == kx_hints::KX_HINT_SPAN_ID)
            .count(),
        1
    );
    let last = updated.spans().last().unwrap();
    assert_eq!(last.id(), kx_hints::KX_HINT_SPAN_ID);
    assert_eq!(last.content(), "new hint");
}
