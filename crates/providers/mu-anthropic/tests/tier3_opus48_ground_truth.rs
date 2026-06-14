//! Tier-3 ground-truth test against REAL captured Claude Code traffic.
//!
//! Source: anthropic-wiretap capture of a live `claude-opus-4-8` streaming
//! call (2026-06-13), parsed by parse-wire.py into fixtures. This is the oracle
//! the spec text can't be: it catches what Anthropic actually sends, including
//! fields newer than the pinned spec snapshot (output_config, context_management,
//! thinking:adaptive, usage.service_tier, usage.inference_geo).
//!
//! The point is NOT byte-equality (we don't model every cc-only field). It is:
//!  1. our deserializers must SURVIVE real traffic (defensive fallbacks work),
//!  2. the fields we DO model must parse to the right values.

use mu_anthropic::StreamEvent;
use mu_anthropic::Usage;
use serde_json::Value;

fn load_events() -> Vec<(String, String)> {
    let raw = include_str!("fixtures/opus48_stream_events.json");
    let arr: Vec<Value> = serde_json::from_str(raw).expect("fixture parses");
    arr.into_iter()
        .map(|e| {
            (
                e["event"].as_str().unwrap_or("").to_string(),
                e["data"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

#[test]
fn real_opus48_sse_events_all_deserialize() {
    let events = load_events();
    assert_eq!(events.len(), 7, "fixture has the captured 7-event sequence");

    for (event_name, data) in &events {
        // `ping` carries no JSON body of interest; the rest must deserialize
        // into our StreamEvent without erroring on the live wire shape.
        let parsed: Result<StreamEvent, _> = serde_json::from_str(data);
        assert!(
            parsed.is_ok(),
            "event `{event_name}` failed to deserialize from real wire data: {:?}\n  data: {data}",
            parsed.err()
        );
    }
}

#[test]
fn real_message_start_usage_parses_typed_incl_service_tier_and_inference_geo() {
    // The captured message_start.usage carries service_tier + inference_geo,
    // which the pinned spec snapshot predates. These are now MODELLED on Usage
    // (not merely tolerated): the nested usage must parse to a typed Usage and
    // expose them, alongside the cache-creation tier split (mu-yz48 +
    // cache-write-tier-split scars).
    let events = load_events();
    let (_, data) = events
        .iter()
        .find(|(n, _)| n == "message_start")
        .expect("message_start present");

    let ev: StreamEvent =
        serde_json::from_str(data).expect("message_start must deserialize from real wire");
    match ev {
        StreamEvent::MessageStart { message } => {
            let usage: Usage = serde_json::from_value(message.as_value()["usage"].clone())
                .expect("real wire usage parses into typed Usage");
            assert_eq!(usage.cache_creation_input_tokens, Some(47548));
            assert_eq!(
                usage.cache_creation.unwrap().ephemeral_1h_input_tokens,
                Some(47548)
            );
            assert_eq!(usage.service_tier.as_deref(), Some("standard"));
            assert_eq!(usage.inference_geo.as_deref(), Some("not_available"));
        }
        other => panic!("expected MessageStart, got {other:?}"),
    }
}
