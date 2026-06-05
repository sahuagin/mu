//! Rescue parser for text-dialect tool calls from locally-served
//! models (bead mu-ollama-qwen-tool-dialect-yfl0).
//!
//! qwen3-coder:30b (and likely other coder models behind ollama)
//! nondeterministically emits tool calls in its training-native XML
//! dialect as plain assistant text:
//!
//! ```text
//! <function=grep>
//! <parameter=pattern>
//! resolver
//! </parameter>
//! </function>
//! </tool_call>
//! ```
//!
//! instead of the structured `tool_calls` the chat template promises.
//! Measured 2026-06-04 at ~50-75% of turns with a large system prompt,
//! on BOTH ollama endpoints (`/v1/chat/completions` and `/api/chat`) —
//! ollama's own template parser does not recover it either, so by the
//! time the SSE stream ends mu holds a "final text answer" that is
//! actually an unexecuted tool call, and the agent loop terminates.
//!
//! This module rewrites the final [`AssistantMessage`]: when a turn
//! ends with no structured tool calls but the text parses as one or
//! more dialect calls against KNOWN tool names, the markup is replaced
//! by real [`ToolCall`] blocks and the stop reason becomes `ToolUse`.
//! Two dialects are recognized: the qwen-coder XML form above, and the
//! qwen3-standard JSON form `<tool_call>{"name":…,"arguments":…}</tool_call>`.
//!
//! Conservative by construction: only `EndTurn` messages are touched,
//! a call naming an unknown tool aborts the whole rescue (prose that
//! merely *discusses* the syntax stays prose), and any parse ambiguity
//! returns the message unchanged.

use serde_json::{Map, Value};

use mu_core::agent::{AssistantMessage, ContentBlock, StopReason, ToolArgs, ToolCall, ToolSpec};

/// Rewrite `msg` if its text is actually a text-dialect tool call.
/// Returns the message unchanged unless every gate passes.
pub(crate) fn rescue_assistant_message(
    msg: AssistantMessage,
    tools: &[ToolSpec],
) -> AssistantMessage {
    if msg.stop_reason != StopReason::EndTurn || tools.is_empty() {
        return msg;
    }
    if msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall(_)))
    {
        return msg;
    }
    let text: String = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(&**text),
            _ => None,
        })
        .collect();
    let Some((remainder, calls)) = parse_dialect_calls(&text, tools) else {
        return msg;
    };
    let mut content: Vec<ContentBlock> = Vec::with_capacity(calls.len() + 1);
    if !remainder.is_empty() {
        content.push(ContentBlock::Text {
            text: remainder.into(),
        });
    }
    content.extend(calls.into_iter().map(ContentBlock::ToolCall));
    AssistantMessage {
        content,
        stop_reason: StopReason::ToolUse,
        usage: msg.usage,
    }
}

/// Scan `text` for dialect tool calls. Returns the text with the
/// markup spans removed plus the parsed calls, or None when nothing
/// parses cleanly (caller leaves the message untouched).
fn parse_dialect_calls(text: &str, tools: &[ToolSpec]) -> Option<(String, Vec<ToolCall>)> {
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut remainder = String::new();
    let mut rest = text;

    loop {
        // Next markup opener of either dialect, whichever comes first.
        let xml_at = rest.find("<function=");
        let wrap_at = rest.find("<tool_call>");
        let start = match (xml_at, wrap_at) {
            (Some(x), Some(w)) => x.min(w),
            (Some(x), None) => x,
            (None, Some(w)) => w,
            (None, None) => break,
        };
        remainder.push_str(&rest[..start]);
        let mut span = &rest[start..];

        // Optional `<tool_call>` wrapper. JSON dialect if `{` follows;
        // XML dialect if `<function=` follows (qwen sometimes opens the
        // wrapper, sometimes only closes it — both are tolerated).
        let mut wrapped = false;
        if let Some(s) = span.strip_prefix("<tool_call>") {
            span = s.trim_start();
            wrapped = true;
        }
        let (after, call) = if span.starts_with('{') {
            if !wrapped {
                return None; // bare '{' without wrapper can't happen via the finds above
            }
            parse_json_call(span, tools)?
        } else {
            parse_xml_call(span, tools)?
        };
        let mut after = after.trim_start_matches(['\n', ' ']);
        // Closing wrapper: consume when present, opener or not — the
        // observed leak closes a wrapper it never opened.
        if let Some(s) = after.strip_prefix("</tool_call>") {
            after = s;
        }
        calls.push(call);
        rest = after;
    }
    if calls.is_empty() {
        return None;
    }
    remainder.push_str(rest);
    let remainder = remainder.trim().to_string();
    Some((remainder, calls))
}

/// Parse `<function=NAME> (<parameter=KEY>VALUE</parameter>)* </function>`.
/// `span` starts at `<function=`. Returns (text after the call, call).
fn parse_xml_call<'a>(span: &'a str, tools: &[ToolSpec]) -> Option<(&'a str, ToolCall)> {
    let body = span.strip_prefix("<function=")?;
    let name_end = body.find('>')?;
    let name = body[..name_end].trim();
    let spec = tools.iter().find(|t| t.name == name)?;
    let mut rest = &body[name_end + 1..];
    let mut args = Map::new();
    loop {
        rest = rest.trim_start();
        // Proper close — or the lenient variants qwen actually emits:
        // closing the wrapper without closing the function, or simply
        // stopping after the last parameter. Both are safe to accept
        // here: every parameter above parsed completely and the tool
        // name is known.
        if let Some(after) = rest.strip_prefix("</function>") {
            let call = build_call(spec, Value::Object(args), tools_call_index(span))?;
            return Some((after, call));
        }
        if rest.is_empty() || rest.starts_with("</tool_call>") {
            let call = build_call(spec, Value::Object(args), tools_call_index(span))?;
            return Some((rest, call));
        }
        let param = rest.strip_prefix("<parameter=")?;
        let key_end = param.find('>')?;
        let key = param[..key_end].trim().to_string();
        let value_body = &param[key_end + 1..];
        let value_end = value_body.find("</parameter>")?;
        let raw = value_body[..value_end].trim();
        args.insert(key.clone(), coerce_value(raw, spec, &key));
        rest = &value_body[value_end + "</parameter>".len()..];
    }
}

/// Parse the JSON dialect body: `{"name": …, "arguments": …}` followed
/// by `</tool_call>`. `span` starts at `{`.
fn parse_json_call<'a>(span: &'a str, tools: &[ToolSpec]) -> Option<(&'a str, ToolCall)> {
    let end = span.find("</tool_call>")?;
    let obj: Value = serde_json::from_str(span[..end].trim()).ok()?;
    let name = obj.get("name")?.as_str()?;
    let spec = tools.iter().find(|t| t.name == name)?;
    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let call = build_call(spec, arguments, tools_call_index(span))?;
    // Leave the closing tag for the caller's wrapper-consumption step.
    Some((&span[end..], call))
}

fn build_call(spec: &ToolSpec, arguments: Value, index: usize) -> Option<ToolCall> {
    Some(ToolCall {
        id: format!("dialect_rescue_{index}"),
        name: spec.name.clone(),
        arguments: ToolArgs::new(arguments).ok()?,
    })
}

/// Cheap unique-enough id discriminator: byte length of the unparsed
/// span. Two calls in one message necessarily have different spans.
fn tools_call_index(span: &str) -> usize {
    span.len()
}

/// XML parameter values arrive as raw text; coerce by the declared
/// JSON-Schema type. Unknown/missing types stay strings — a wrongly
/// stringified number is recoverable by the tool's own validation,
/// whereas a wrongly numified string (grep pattern "2") is not.
fn coerce_value(raw: &str, spec: &ToolSpec, key: &str) -> Value {
    let ty = spec
        .input_schema
        .get("properties")
        .and_then(|p| p.get(key))
        .and_then(|s| s.get("type"))
        .and_then(|t| t.as_str());
    match ty {
        Some("integer") => raw
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.into())),
        Some("number") => raw
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(raw.into())),
        Some("boolean") => raw
            .parse::<bool>()
            .map(Value::Bool)
            .unwrap_or_else(|_| Value::String(raw.into())),
        Some("array") | Some("object") => {
            serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()))
        }
        _ => Value::String(raw.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, schema: Value) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: String::new(),
            input_schema: schema,
            display: None,
            when: None,
            policy: Default::default(),
        }
    }

    fn grep_read_tools() -> Vec<ToolSpec> {
        vec![
            spec(
                "grep",
                json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]}),
            ),
            spec(
                "read",
                json!({"type":"object","properties":{"path":{"type":"string"},"limit":{"type":"integer"}},"required":["path"]}),
            ),
        ]
    }

    fn end_turn(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: None,
        }
    }

    fn rescued_calls(msg: &AssistantMessage) -> Vec<&ToolCall> {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect()
    }

    /// The exact leak observed 2026-06-04: XML dialect, newline-wrapped
    /// values, stray closing wrapper with no opener.
    #[test]
    fn rescues_observed_qwen_leak() {
        let text = "<function=grep>\n<parameter=pattern>\nresolver\n</parameter>\n<parameter=path>\nroot\n</parameter>\n</function>\n</tool_call>";
        let out = rescue_assistant_message(end_turn(text), &grep_read_tools());
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        let calls = rescued_calls(&out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "grep");
        assert_eq!(
            calls[0].arguments.as_value(),
            &json!({"pattern": "resolver", "path": "root"})
        );
        // Pure markup → no leftover text block.
        assert_eq!(out.content.len(), 1);
    }

    #[test]
    fn preserves_prose_preamble() {
        let text = "I'll inspect the repository.\n<function=read>\n<parameter=path>Cargo.toml</parameter>\n</function>";
        let out = rescue_assistant_message(end_turn(text), &grep_read_tools());
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        match &out.content[0] {
            ContentBlock::Text { text } => assert_eq!(&**text, "I'll inspect the repository."),
            other => panic!("expected leading text block, got {other:?}"),
        }
        assert_eq!(rescued_calls(&out).len(), 1);
    }

    #[test]
    fn rescues_json_dialect() {
        let text = "<tool_call>\n{\"name\": \"read\", \"arguments\": {\"path\": \"Cargo.toml\"}}\n</tool_call>";
        let out = rescue_assistant_message(end_turn(text), &grep_read_tools());
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        let calls = rescued_calls(&out);
        assert_eq!(calls[0].name, "read");
        assert_eq!(
            calls[0].arguments.as_value(),
            &json!({"path": "Cargo.toml"})
        );
    }

    #[test]
    fn rescues_multiple_calls() {
        let text = "<function=read>\n<parameter=path>a.rs</parameter>\n</function>\n<function=read>\n<parameter=path>b.rs</parameter>\n</function>";
        let out = rescue_assistant_message(end_turn(text), &grep_read_tools());
        let calls = rescued_calls(&out);
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].id, calls[1].id);
    }

    #[test]
    fn coerces_integer_params_by_schema() {
        let text = "<function=read>\n<parameter=path>x.rs</parameter>\n<parameter=limit>50</parameter>\n</function>";
        let out = rescue_assistant_message(end_turn(text), &grep_read_tools());
        let calls = rescued_calls(&out);
        assert_eq!(
            calls[0].arguments.as_value(),
            &json!({"path": "x.rs", "limit": 50})
        );
    }

    /// A numeric-looking STRING param must stay a string (grep for "2").
    #[test]
    fn string_typed_params_never_numify() {
        let text = "<function=grep>\n<parameter=pattern>2</parameter>\n</function>";
        let out = rescue_assistant_message(end_turn(text), &grep_read_tools());
        let calls = rescued_calls(&out);
        assert_eq!(calls[0].arguments.as_value(), &json!({"pattern": "2"}));
    }

    #[test]
    fn unknown_tool_name_aborts_rescue() {
        let text = "<function=launch_missiles>\n<parameter=target>moon</parameter>\n</function>";
        let msg = end_turn(text);
        let out = rescue_assistant_message(msg.clone(), &grep_read_tools());
        assert_eq!(out, msg);
    }

    #[test]
    fn prose_without_markup_untouched() {
        let msg = end_turn("The resolver value is 2.");
        let out = rescue_assistant_message(msg.clone(), &grep_read_tools());
        assert_eq!(out, msg);
    }

    #[test]
    fn prose_discussing_syntax_mid_sentence_untouched() {
        // "<function=" appears but never parses to a known call.
        let msg = end_turn("qwen emits markup like <function=NAME> when it leaks.");
        let out = rescue_assistant_message(msg.clone(), &grep_read_tools());
        assert_eq!(out, msg);
    }

    #[test]
    fn non_end_turn_untouched() {
        let mut msg = end_turn("<function=read>\n<parameter=path>x</parameter>\n</function>");
        msg.stop_reason = StopReason::MaxTokens; // possibly truncated markup
        let out = rescue_assistant_message(msg.clone(), &grep_read_tools());
        assert_eq!(out, msg);
    }

    #[test]
    fn structured_calls_present_untouched() {
        let msg = AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: ToolArgs::new(json!({"path": "x"})).unwrap(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: None,
        };
        let out = rescue_assistant_message(msg.clone(), &grep_read_tools());
        assert_eq!(out, msg);
    }

    #[test]
    fn malformed_markup_aborts_rescue() {
        // Unterminated PARAMETER — must not fabricate a call from a
        // value of unknown extent.
        let msg = end_turn("<function=read>\n<parameter=path>x.rs");
        let out = rescue_assistant_message(msg.clone(), &grep_read_tools());
        assert_eq!(out, msg);
    }

    /// Lenient closes: wrapper-only close and end-of-text close are
    /// accepted once every parameter has parsed completely.
    #[test]
    fn rescues_wrapper_closed_function() {
        let text = "<tool_call>\n<function=read>\n<parameter=path>x.rs</parameter>\n</tool_call>";
        let out = rescue_assistant_message(end_turn(text), &grep_read_tools());
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(rescued_calls(&out)[0].name, "read");
    }

    #[test]
    fn rescues_unclosed_function_at_end_of_text() {
        let text = "I'll read it.\n<function=read>\n<parameter=path>x.rs</parameter>\n";
        let out = rescue_assistant_message(end_turn(text), &grep_read_tools());
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        let calls = rescued_calls(&out);
        assert_eq!(calls[0].arguments.as_value(), &json!({"path": "x.rs"}));
    }
}
