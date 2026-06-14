//! PROBE: do mu-anthropic's deserializers parse REAL Claude Code session logs?
//! Hypothesis: cc logs store the wire message verbatim under `.message`, so
//! our ResponseMessage should parse them with zero adaptation.
use mu_anthropic::ResponseMessage;
use serde_json::Value;

#[test]
fn parses_real_cc_log_messages() {
    let raw = include_str!("fixtures/cc_log_messages.json");
    let msgs: Vec<Value> = serde_json::from_str(raw).unwrap();
    let total = msgs.len();
    let mut ok = 0usize;
    let mut fails: Vec<(usize, String)> = vec![];
    for (i, m) in msgs.iter().enumerate() {
        match serde_json::from_value::<ResponseMessage>(m.clone()) {
            Ok(_) => ok += 1,
            Err(e) => fails.push((i, e.to_string())),
        }
    }
    eprintln!("parsed {ok}/{total} real cc-log messages");
    for (i, e) in fails.iter().take(8) {
        eprintln!("  FAIL[{i}]: {e}");
    }
    assert_eq!(
        ok,
        total,
        "{} of {} real messages failed to parse",
        total - ok,
        total
    );
}
