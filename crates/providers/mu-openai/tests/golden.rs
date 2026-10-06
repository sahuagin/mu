//! Tier-3 golden fixtures — parse representative captured Responses-API JSON and
//! assert the modeled surface, so a wire change (or a regression in our types)
//! goes red. Fixtures live in `tests/fixtures/`. The drift canary
//! (`examples/drift_check.rs`) re-serializes and diffs these same shapes.

use mu_openai::{accumulate, OutputItem, Response, ResponseStreamEvent};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn text_response_parses_and_round_trips() {
    let raw = fixture("response_text.json");
    let orig: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let r: Response = serde_json::from_value(orig.clone()).unwrap();
    assert_eq!(r.output_text(), "Hello! How can I help?");
    assert_eq!(r.usage.as_ref().unwrap().total_tokens, Some(20));
    // Round-trips with no dropped modeled fields.
    assert_eq!(serde_json::to_value(&r).unwrap(), orig);
}

#[test]
fn reasoning_and_tool_response_threading_fields_present() {
    let r: Response = serde_json::from_str(&fixture("response_reasoning_and_tool.json")).unwrap();
    match &r.output[0] {
        OutputItem::Reasoning {
            encrypted_content,
            summary,
            ..
        } => {
            assert!(
                encrypted_content.is_some(),
                "encrypted_content needed for threading"
            );
            assert_eq!(summary.len(), 1);
        }
        other => panic!("expected reasoning item, got {other:?}"),
    }
    match &r.output[1] {
        OutputItem::FunctionCall { name, call_id, .. } => {
            assert_eq!(name.as_deref(), Some("read_file"));
            assert_eq!(call_id.as_deref(), Some("call_xyz"));
        }
        other => panic!("expected function_call, got {other:?}"),
    }
    assert_eq!(
        r.usage
            .unwrap()
            .output_tokens_details
            .unwrap()
            .reasoning_tokens,
        Some(66)
    );
}

#[tokio::test]
async fn streamed_tool_call_accumulates_to_final_response() {
    let events: Vec<ResponseStreamEvent> =
        serde_json::from_str(&fixture("stream_tool_call.json")).unwrap();
    let r = accumulate(futures::stream::iter(events)).await.unwrap();
    assert_eq!(r.id, "resp_s1");
    match &r.output[0] {
        OutputItem::FunctionCall {
            name, arguments, ..
        } => {
            assert_eq!(name.as_deref(), Some("read_file"));
            assert_eq!(arguments.as_deref(), Some("{\"path\":\"a.rs\"}"));
        }
        other => panic!("expected function_call, got {other:?}"),
    }
}

/// The WebSocket steering events from the 2026-09-09 spec's own examples
/// (`ws_steer_events_20260909.json`): each parses to its typed variant and
/// re-serializes byte-for-byte, so the drift canary replays them too.
/// mu-openai-protocol-2026q3-yyg3j.4.
#[test]
fn ws_steer_events_parse_typed_and_round_trip() {
    let raw = fixture("ws_steer_events_20260909.json");
    let orig: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
    let events: Vec<ResponseStreamEvent> = orig
        .iter()
        .map(|v| serde_json::from_value(v.clone()).unwrap())
        .collect();
    assert!(matches!(
        events[0],
        ResponseStreamEvent::SteerAccepted { .. }
    ));
    assert!(matches!(
        events[1],
        ResponseStreamEvent::SteerPending { .. }
    ));
    assert!(matches!(events[2], ResponseStreamEvent::SteerFailed { .. }));
    assert!(matches!(
        &events[3],
        ResponseStreamEvent::Error { stream_id: Some(s), .. } if s == "agent_1"
    ));
    for (e, o) in events.iter().zip(&orig) {
        assert_eq!(&serde_json::to_value(e).unwrap(), o);
    }
}

/// `usage.attribution` from two captured codex-backend calls (2026-10-05;
/// ids/objects synthetic, `usage` verbatim with wire key order). Call 1's
/// request input was [user msg, developer msg]; call 2's was [user msg,
/// function_call, function_call_output, developer msg]. The span objects must
/// come out in DOCUMENT order (the order carries the request-position
/// mapping) and the whole usage must round-trip.
#[test]
fn usage_attribution_parses_in_wire_order_and_round_trips() {
    let raw = fixture("response_usage_attribution_20261005.json");
    // Straight from the text: a `serde_json::Value` detour would sort keys.
    let calls: Vec<Response> = serde_json::from_str(&raw).unwrap();
    let orig: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
    for (r, o) in calls.iter().zip(&orig) {
        assert_eq!(&serde_json::to_value(r).unwrap(), o, "{}", r.id);
    }

    let attr = |i: usize| {
        calls[i]
            .usage
            .as_ref()
            .unwrap()
            .attribution
            .clone()
            .unwrap()
    };
    let shape = |items: &[mu_openai::AttributionItem]| {
        items
            .iter()
            .map(|i| {
                (
                    i.key[..3].to_owned(),
                    i.input_tokens.unwrap(),
                    i.output_tokens.unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let s = |k: &str, i, o| (k.to_owned(), i, o);

    let a1 = attr(0);
    // Output item listed FIRST here.
    assert_eq!(
        shape(&a1.items),
        [s("fc_", 2, 41), s("msg", 33482, 0), s("msg", 22, 0)]
    );
    assert_eq!(a1.request_fields[0].key, "instructions");
    assert_eq!(a1.request_fields[1].key, "tools");
    assert!(a1.items[1].content.is_some(), "content kept for round-trip");

    let a2 = attr(1);
    // Output item listed LAST here; request_fields in wire order (tools first).
    assert_eq!(
        shape(&a2.items),
        [
            s("msg", 33482, 0),
            s("fc_", 43, 0),
            s("fc_", 39, 0),
            s("msg", 22, 0),
            s("fc_", 2, 22)
        ]
    );
    assert_eq!(a2.items[0].cached_tokens, Some(32943));
    assert_eq!(
        a2.request_fields
            .iter()
            .map(|f| (f.key.as_str(), f.cached_tokens.unwrap()))
            .collect::<Vec<_>>(),
        [("tools", 1579), ("instructions", 38)]
    );

    // Order also survives the real stream path: a tagged `response.completed`
    // event (serde buffers the body, keeping entry order).
    let event = format!(
        r#"{{"type":"response.completed","sequence_number":9,"response":{}}}"#,
        serde_json::to_string(&calls[1]).unwrap()
    );
    match serde_json::from_str::<ResponseStreamEvent>(&event).unwrap() {
        ResponseStreamEvent::Completed { response, .. } => {
            assert_eq!(response.usage.unwrap().attribution.unwrap(), a2);
        }
        other => panic!("expected completed, got {other:?}"),
    }
}
