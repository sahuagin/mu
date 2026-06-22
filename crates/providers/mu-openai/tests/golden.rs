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
