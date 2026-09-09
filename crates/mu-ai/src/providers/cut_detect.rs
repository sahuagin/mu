//! mu-c9b2l: reading a cut off the accumulated argument JSON, for the
//! streaming wires that have no byte cap of their own to lean on.
//!
//! The openai-chat accumulator learned this first (see `openrouter.rs`, which
//! carries the original alongside the byte-cap abort). The Anthropic Messages
//! and OpenAI Responses wires need the same judgement from the same two
//! signals, so it lives here rather than a third time in each.

use serde_json::Value;

use mu_core::agent::tool_call_cut::{CutCause, ToolCallCut};

/// Was this argument string cut off mid-value, rather than simply malformed?
///
/// Two signals, and only these: serde_json classifies the failure as `Eof`
/// (the observed "EOF while parsing a string" from an 85,622-character
/// `write` that ended at the completion ceiling), or the turn already
/// reported hitting the model's output ceiling — `stop_reason: "max_tokens"`
/// on the Messages wire, `incomplete_details.reason: "max_output_tokens"` on
/// the Responses wire. A syntax or data error on a complete-looking string is
/// a broken model, not a cut, and keeps the pre-existing empty-object
/// fallback.
///
/// `cap` is the session's effective `[session].max_tool_call_bytes` and
/// `budget_bytes` what the request's output ceiling buys; both ride on the
/// marker so the refusal quotes the limits actually in force.
pub(crate) fn detect_truncated_arguments(
    args_json: &str,
    hit_output_limit: bool,
    cap: Option<usize>,
    budget_bytes: Option<usize>,
) -> Option<ToolCallCut> {
    if args_json.is_empty() {
        return None;
    }
    let err = serde_json::from_str::<Value>(args_json).err()?;
    match err.classify() {
        serde_json::error::Category::Eof => Some(ToolCallCut::new(
            args_json.len(),
            if hit_output_limit {
                CutCause::OutputLimit
            } else {
                CutCause::TruncatedJson
            },
            cap,
            budget_bytes,
        )),
        _ if hit_output_limit => Some(ToolCallCut::new(
            args_json.len(),
            CutCause::OutputLimit,
            cap,
            budget_bytes,
        )),
        _ => None,
    }
}

/// The `arguments` to emit for one accumulated call: the cut marker when the
/// call was cut, the wire's own parse otherwise.
///
/// The marker replaces the arguments wholesale — half an arrived JSON string
/// has nothing safe to hand a tool, and `{}` is what made the cut illegible
/// in the first place.
pub(crate) fn cut_marker_or(
    cut: Option<ToolCallCut>,
    parse: impl FnOnce() -> mu_core::agent::ToolArgs,
) -> mu_core::agent::ToolArgs {
    match cut {
        Some(cut) => mu_core::agent::ToolArgs::new(cut.marker_arguments())
            .expect("cut marker holds no non-finite numbers"),
        None => parse(),
    }
}
