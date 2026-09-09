//! mu-c9b2l: the marker a streaming provider puts on a tool call whose
//! argument JSON never finished arriving, plus the text the model reads in
//! place of a result.
//!
//! An openai-chat stream that hits the output ceiling mid-`arguments` leaves
//! a truncated JSON string. Parsing it fails, and the fallback was to
//! dispatch `{}` — which reaches the tool as "missing required argument".
//! That error is true and useless: nothing in it says the call was cut, so
//! the model cannot tell a typo from running out of room, and it reissues
//! the same oversized call. The marker carries the facts it needs (cut, why,
//! after N bytes, under which ceilings) from the wire, through the assistant
//! message and the event log, to the dispatcher, which answers with them and
//! executes nothing.
//!
//! There are two ceilings, and only the wire layer sees both: mu's own
//! `[session].max_tool_call_bytes`, and what the request's `max_tokens`
//! buys. Advice drawn from the larger one asks for parts the model cannot
//! emit, so the marker carries both and the refusal quotes the smaller.
//!
//! The marker rides in the call's `arguments` because that is the only field
//! every layer between the accumulator and the dispatcher already carries
//! verbatim.

use serde_json::{json, Value};

/// Default for `[session].max_tool_call_bytes`: the accumulated argument
/// budget for a single streamed tool call. Sized so a cut is legible rather
/// than fatal — a call this large is already past the point where the model
/// should have chunked, and reading further only spends tokens on output
/// that will be discarded.
pub const DEFAULT_MAX_TOOL_CALL_BYTES: usize = 32 * 1024;

/// The per-part size to recommend when no cap is configured. Half the
/// default cap, so a model that follows the advice has headroom for the rest
/// of the call's JSON.
pub const SUGGESTED_PART_KIB: usize = DEFAULT_MAX_TOOL_CALL_BYTES / 1024 / 2;

/// mu-c9b2l: bytes per output token, for turning a request's `max_tokens`
/// into an argument-byte budget. Deliberately low — code and JSON run nearer
/// 3 bytes/token than English prose does, and under-estimating keeps the
/// advice inside the real ceiling instead of over it.
pub const BYTES_PER_OUTPUT_TOKEN: usize = 3;

/// The argument bytes a `max_tokens` budget can carry, at
/// [`BYTES_PER_OUTPUT_TOKEN`].
pub fn output_budget_bytes(max_tokens: u32) -> usize {
    max_tokens as usize * BYTES_PER_OUTPUT_TOKEN
}

/// mu-c9b2l: the per-part size to advise, from the smaller of the two
/// ceilings a call actually runs under.
///
/// `cap` is the EFFECTIVE `[session].max_tool_call_bytes` (not the
/// compile-time default); `budget_bytes` is what the request's `max_tokens`
/// buys, via [`output_budget_bytes`]. Either can bite first, and quoting
/// only the byte cap advises parts the model has no room to emit: a model
/// absent from the catalog is sent the 4096-token floor, so ~12 KB of output
/// total, while the default cap would advise 16 KB parts — the model chunks
/// as told and is cut again. Both `None` falls back to the default cap's
/// suggestion. Floored at 1 KB so a tiny ceiling still names a usable number.
pub fn suggested_part_kib(cap: Option<usize>, budget_bytes: Option<usize>) -> usize {
    let effective = match (cap, budget_bytes) {
        (Some(cap), Some(budget)) => cap.min(budget),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => DEFAULT_MAX_TOOL_CALL_BYTES,
    };
    (effective / 1024 / 2).max(1)
}

/// Reserved argument key. Underscore-prefixed and mu-namespaced so it cannot
/// collide with a real tool schema; a model that somehow sent it would get
/// the cut refusal, which is harmless.
pub const CUT_ARG_KEY: &str = "__mu_tool_call_cut";

/// Why the argument stream ended early. Operator-facing detail — the text
/// the model reads is the same either way, because the remedy is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutCause {
    /// Accumulated argument bytes crossed `[session].max_tool_call_bytes`
    /// and mu stopped reading the response.
    ByteCap,
    /// The provider reported `finish_reason: "length"` — the model hit its
    /// own output ceiling mid-argument.
    OutputLimit,
    /// The argument JSON ended mid-value at end of stream, with no
    /// finish_reason saying so.
    TruncatedJson,
}

impl CutCause {
    pub fn as_str(self) -> &'static str {
        match self {
            CutCause::ByteCap => "byte_cap",
            CutCause::OutputLimit => "output_limit",
            CutCause::TruncatedJson => "truncated_json",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "byte_cap" => Some(CutCause::ByteCap),
            "output_limit" => Some(CutCause::OutputLimit),
            "truncated_json" => Some(CutCause::TruncatedJson),
            _ => None,
        }
    }
}

/// A tool call whose arguments were cut off, and how far it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCallCut {
    /// Argument bytes accumulated before the stream ended.
    pub bytes: usize,
    pub cause: CutCause,
    /// mu-c9b2l: the effective `[session].max_tool_call_bytes` the stream ran
    /// under, carried so the model-facing advice quotes the cap actually in
    /// force. `None` when the cap is disabled. Rides with the marker because
    /// the dispatcher that writes the refusal has no view of session config.
    pub cap: Option<usize>,
    /// mu-c9b2l: the argument bytes the request's `max_tokens` buys (see
    /// [`output_budget_bytes`]). The OTHER ceiling — often the lower one, and
    /// the one nothing downstream can see, since only the accumulator knows
    /// what `max_tokens` went out. `None` when the wire sent no ceiling.
    pub budget_bytes: Option<usize>,
}

impl ToolCallCut {
    pub fn new(
        bytes: usize,
        cause: CutCause,
        cap: Option<usize>,
        budget_bytes: Option<usize>,
    ) -> Self {
        Self {
            bytes,
            cause,
            cap,
            budget_bytes,
        }
    }

    /// The complete `arguments` object for a cut call. It replaces the
    /// truncated arguments entirely: nothing in a half-arrived JSON string
    /// is safe to hand a tool.
    pub fn marker_arguments(&self) -> Value {
        json!({
            CUT_ARG_KEY: {
                "bytes": self.bytes,
                "cause": self.cause.as_str(),
                "cap": self.cap,
                "budget_bytes": self.budget_bytes,
            }
        })
    }

    /// Size rounded up to whole KB, for the model-facing text. Binary KB
    /// (1024), matching how the cap is configured.
    pub fn kb(&self) -> usize {
        self.bytes.div_ceil(1024)
    }

    /// The effective cap in whole KB. Falls back to the size reached, which
    /// is the only figure there is when no cap was configured.
    fn cap_kb(&self) -> usize {
        self.cap.map_or_else(|| self.kb(), |cap| cap.div_ceil(1024))
    }

    /// The per-part size to advise for this cut — see [`suggested_part_kib`].
    pub fn suggested_part_kib(&self) -> usize {
        suggested_part_kib(self.cap, self.budget_bytes)
    }
}

/// Read the marker back off a call's arguments. `None` for every ordinary
/// call.
pub fn detect(arguments: &Value) -> Option<ToolCallCut> {
    let marker = arguments.get(CUT_ARG_KEY)?;
    let bytes = marker.get("bytes").and_then(Value::as_u64).unwrap_or(0) as usize;
    let cause = marker
        .get("cause")
        .and_then(Value::as_str)
        .and_then(CutCause::parse)
        .unwrap_or(CutCause::TruncatedJson);
    let cap = marker
        .get("cap")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let budget_bytes = marker
        .get("budget_bytes")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    Some(ToolCallCut::new(bytes, cause, cap, budget_bytes))
}

/// The tool result a cut call gets instead of execution. Names the fact the
/// model cannot otherwise observe (the call was cut, and how big it got) and
/// the concrete next move, because these models act on tool-result text.
///
/// mu-c9b2l: the clause naming the CAUSE differs per cause — mu's own
/// argument limit, the model's output ceiling and an incomplete stream are
/// three different things to fix, and the old text asserted the middle one
/// for all three. The REMEDY sentence is identical across causes because the
/// remedy is, and its part size comes from whichever of the two live
/// ceilings — the effective cap, the request's output budget — is smaller,
/// rather than from the compile-time default.
pub fn refusal_text(tool_name: &str, cut: &ToolCallCut) -> String {
    let cause = match cut.cause {
        CutCause::ByteCap => format!(
            "exceeded mu's per-call argument limit of {} KB (`[session].max_tool_call_bytes`)",
            cut.cap_kb()
        ),
        CutCause::OutputLimit => format!("hit the model's output limit after {} KB", cut.kb()),
        CutCause::TruncatedJson => format!("arrived incomplete after {} KB", cut.kb()),
    };
    // The append route exists only on `write`; a cut `edit`, `bash` or
    // imported tool gets the size advice without a remedy it cannot use.
    let remedy = if tool_name == "write" {
        format!(
            "Split the work into smaller calls — write the first part, then `write` the rest \
             with `append: true`, keeping each part under ~{} KB.",
            cut.suggested_part_kib()
        )
    } else {
        format!(
            "Split the work into smaller calls, each under ~{} KB of arguments.",
            cut.suggested_part_kib()
        )
    };
    format!("runtime: your `{tool_name}` call {cause}; nothing was executed. {remedy}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_remedy_is_offered_only_to_write() {
        let cut = ToolCallCut::new(40 * 1024, CutCause::ByteCap, Some(32 * 1024), None);
        let w = refusal_text("write", &cut);
        assert!(w.contains("`append: true`"), "{w}");
        for other in ["bash", "edit", "some_mcp_tool"] {
            let t = refusal_text(other, &cut);
            assert!(!t.contains("append"), "{other}: {t}");
            assert!(t.contains("smaller calls"), "{other}: {t}");
        }
    }

    #[test]
    fn marker_round_trips_through_arguments() {
        for cause in [
            CutCause::ByteCap,
            CutCause::OutputLimit,
            CutCause::TruncatedJson,
        ] {
            for cap in [None, Some(8 * 1024)] {
                for budget_bytes in [None, Some(12 * 1024)] {
                    let cut = ToolCallCut::new(85_622, cause, cap, budget_bytes);
                    let detected = detect(&cut.marker_arguments()).expect("marker detected");
                    assert_eq!(detected, cut);
                }
            }
        }
    }

    #[test]
    fn detect_is_none_for_ordinary_arguments() {
        assert!(detect(&json!({})).is_none());
        assert!(detect(&json!({"path": "/tmp/x", "content": "hi"})).is_none());
        assert!(detect(&json!("not an object")).is_none());
    }

    #[test]
    fn kb_rounds_up_and_default_cap_is_exact() {
        assert_eq!(
            ToolCallCut::new(85_622, CutCause::OutputLimit, None, None).kb(),
            84
        );
        assert_eq!(
            ToolCallCut::new(DEFAULT_MAX_TOOL_CALL_BYTES, CutCause::ByteCap, None, None).kb(),
            32
        );
        assert_eq!(ToolCallCut::new(1, CutCause::ByteCap, None, None).kb(), 1);
        assert_eq!(SUGGESTED_PART_KIB, 16);
    }

    /// mu-c9b2l: half the EFFECTIVE cap, with the default standing in when
    /// the cap is off, and never zero.
    #[test]
    fn suggested_part_size_tracks_the_effective_cap() {
        assert_eq!(suggested_part_kib(None, None), SUGGESTED_PART_KIB);
        assert_eq!(
            suggested_part_kib(Some(DEFAULT_MAX_TOOL_CALL_BYTES), None),
            16
        );
        assert_eq!(suggested_part_kib(Some(8 * 1024), None), 4);
        assert_eq!(suggested_part_kib(Some(512), None), 1);
    }

    /// mu-c9b2l: the request's `max_tokens` is the other ceiling, and the
    /// advice quotes whichever bites first. A model absent from the catalog
    /// is sent the 4096-token floor — ~12 KB of output — so advising 16 KB
    /// parts under the 32 KB cap asks for chunks it cannot emit.
    #[test]
    fn suggested_part_size_takes_the_smaller_of_cap_and_output_budget() {
        let floor_budget = output_budget_bytes(4096);
        assert_eq!(floor_budget, 12 * 1024);

        // Cap 32 KB, floor max_tokens: the budget bites first.
        assert_eq!(
            suggested_part_kib(Some(DEFAULT_MAX_TOOL_CALL_BYTES), Some(floor_budget)),
            6
        );
        // Cap 32 KB, a roomy 32k-token budget: the cap bites first.
        assert_eq!(
            suggested_part_kib(
                Some(DEFAULT_MAX_TOOL_CALL_BYTES),
                Some(output_budget_bytes(32_768))
            ),
            16
        );
        // Cap disabled: the output budget is the only ceiling left.
        assert_eq!(suggested_part_kib(None, Some(floor_budget)), 6);
    }

    /// mu-c9b2l: each cause names what actually happened; the remedy is one
    /// shared sentence, so a model reads the same next move either way.
    #[test]
    fn refusal_text_names_the_cause_and_keeps_one_remedy() {
        let cap = Some(DEFAULT_MAX_TOOL_CALL_BYTES);
        let remedy = "Split the work into smaller calls — write the first part, then `write` \
                      the rest with `append: true`, keeping each part under ~16 KB.";

        let byte_cap = refusal_text(
            "write",
            &ToolCallCut::new(85_622, CutCause::ByteCap, cap, None),
        );
        assert_eq!(
            byte_cap,
            format!(
                "runtime: your `write` call exceeded mu's per-call argument limit of 32 KB \
                 (`[session].max_tool_call_bytes`); nothing was executed. {remedy}"
            )
        );

        let output_limit = refusal_text(
            "write",
            &ToolCallCut::new(85_622, CutCause::OutputLimit, cap, None),
        );
        assert_eq!(
            output_limit,
            format!(
                "runtime: your `write` call hit the model's output limit after 84 KB; \
                 nothing was executed. {remedy}"
            )
        );

        let truncated = refusal_text(
            "write",
            &ToolCallCut::new(85_622, CutCause::TruncatedJson, cap, None),
        );
        assert_eq!(
            truncated,
            format!(
                "runtime: your `write` call arrived incomplete after 84 KB; \
                 nothing was executed. {remedy}"
            )
        );
    }

    /// mu-c9b2l: a lowered `[session].max_tool_call_bytes` changes both the
    /// limit the refusal quotes and the part size it advises — the advice
    /// used to be pinned to the compile-time default whatever the config
    /// said.
    #[test]
    fn refusal_text_quotes_the_configured_cap_not_the_default() {
        let text = refusal_text(
            "write",
            &ToolCallCut::new(9_000, CutCause::ByteCap, Some(8 * 1024), None),
        );
        assert!(text.contains("argument limit of 8 KB"), "{text}");
        assert!(text.contains("under ~4 KB"), "{text}");

        // Cap off: the model's own ceiling still cuts calls, and the
        // default's suggestion stands in.
        let uncapped = refusal_text(
            "write",
            &ToolCallCut::new(9_000, CutCause::OutputLimit, None, None),
        );
        assert!(uncapped.contains("under ~16 KB"), "{uncapped}");
    }

    /// mu-c9b2l: the refusal a 4096-token model actually reads — the cap it
    /// names is still the byte cap, but the part size it advises is the one
    /// the output budget leaves room for.
    #[test]
    fn refusal_text_advises_within_the_output_budget() {
        let text = refusal_text(
            "write",
            &ToolCallCut::new(
                13_000,
                CutCause::OutputLimit,
                Some(DEFAULT_MAX_TOOL_CALL_BYTES),
                Some(output_budget_bytes(4096)),
            ),
        );
        assert!(text.contains("under ~6 KB"), "{text}");
        assert!(!text.contains("~16 KB"), "{text}");
    }
}
