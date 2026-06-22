//! drift_check — a protocol-drift canary for `mu-openai`.
//!
//! Reads OpenAI Responses-API JSON files (paths as argv) — each a non-streaming
//! `Response`, a single `response.*` streaming event, or a JSON ARRAY of either
//! — parses them with mu-openai's types, re-serializes, and diffs. A field the
//! wire carries on a MODELED type that we don't model round-trips DIFFERENTLY
//! (DROPPED / CHANGED) and is flagged as drift: that is OpenAI adding/renaming a
//! field on a shape we claim to model.
//!
//! Why a dedicated canary: the library is intentionally LENIENT — unknown fields
//! are dropped and whole unknown item/event kinds land in `Unknown`, so a plain
//! parse never errors. This re-serializes and diffs to surface real drift.
//!
//! Deliberately-unmodeled shapes (web_search / code_interpreter / mcp items and
//! events — out of scope for agent/text) land in `Unknown`, round-trip exactly,
//! and are reported as INFO, NOT drift (reporting them would false-positive on
//! every tool response).
//!
//! Exit: 0 = clean, 3 = drift detected, 2 = usage error.

use mu_openai::{OutputContent, OutputItem, Response, ResponseStreamEvent};
use serde_json::Value;
use std::fs;
use std::process::ExitCode;

fn truncate(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 60 {
        format!("{}…", &s[..60])
    } else {
        s
    }
}

/// JSON paths where `orig` and the round-tripped `back` differ. A field
/// present-and-null in `orig` but absent in `back` is NOT a drop — the library
/// omits `None`/absent fields, which is wire-equivalent to null.
fn diff(orig: &Value, back: &Value, path: &str, out: &mut Vec<String>) {
    match (orig, back) {
        (Value::Object(mo), Value::Object(mb)) => {
            for (k, vo) in mo {
                let p = format!("{path}.{k}");
                match mb.get(k) {
                    Some(vb) => diff(vo, vb, &p, out),
                    None if vo.is_null() => {}
                    None => out.push(format!("DROPPED {p} = {}", truncate(vo))),
                }
            }
        }
        (Value::Array(ao), Value::Array(ab)) => {
            if ao.len() != ab.len() {
                out.push(format!("ARRAY-LEN {path}: {} -> {}", ao.len(), ab.len()));
            } else {
                for (i, (xo, xb)) in ao.iter().zip(ab).enumerate() {
                    diff(xo, xb, &format!("{path}[{i}]"), out);
                }
            }
        }
        _ if orig != back => {
            out.push(format!(
                "CHANGED {path}: {} -> {}",
                truncate(orig),
                truncate(back)
            ));
        }
        _ => {}
    }
}

/// Returns (drift findings, info notes) for one JSON value.
fn check_one(orig: &Value) -> (Vec<String>, Vec<String>) {
    let mut drift = Vec::new();
    let mut info = Vec::new();

    // Streaming event? (type is "response.*" or the bare "error" frame.)
    let is_event = orig
        .get("type")
        .and_then(Value::as_str)
        .map(|t| t.starts_with("response.") || t == "error")
        .unwrap_or(false);

    if is_event {
        match serde_json::from_value::<ResponseStreamEvent>(orig.clone()) {
            Ok(ResponseStreamEvent::Unknown(_)) => {
                let t = orig.get("type").and_then(Value::as_str).unwrap_or("?");
                info.push(format!(
                    "UNMODELED-EVENT type={t:?} (out of scope; not drift)"
                ));
            }
            Ok(ev) => {
                let back = serde_json::to_value(&ev).expect("serialize event");
                diff(orig, &back, "$", &mut drift);
            }
            Err(e) => drift.push(format!("does not parse as a stream event: {e}")),
        }
        return (drift, info);
    }

    // Otherwise: a non-streaming Response.
    match serde_json::from_value::<Response>(orig.clone()) {
        Ok(r) => {
            let back = serde_json::to_value(&r).expect("serialize response");
            diff(orig, &back, "$", &mut drift);
            for (i, item) in r.output.iter().enumerate() {
                match item {
                    OutputItem::Unknown(v) => {
                        let t = v
                            .as_value()
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        info.push(format!("UNMODELED-ITEM $.output[{i}].type={t:?}"));
                    }
                    OutputItem::Message { content, .. } => {
                        for (j, c) in content.iter().enumerate() {
                            if let OutputContent::Unknown(v) = c {
                                let t = v
                                    .as_value()
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("?");
                                info.push(format!(
                                    "UNMODELED-CONTENT $.output[{i}].content[{j}].type={t:?}"
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Err(e) => drift.push(format!("does not parse as a Response: {e}")),
    }
    (drift, info)
}

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: drift_check <response-or-event.json> [more.json ...]");
        eprintln!("  each file: a Response object, a response.* event, or an array of either");
        return ExitCode::from(2);
    }
    let mut any_drift = false;
    for p in &paths {
        let raw = match fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) => {
                println!("ERROR  {p}: {e}");
                any_drift = true;
                continue;
            }
        };
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                println!("ERROR  {p}: not JSON: {e}");
                any_drift = true;
                continue;
            }
        };
        // A file may hold one value or an array of values (e.g. a captured stream).
        let items: Vec<Value> = match parsed {
            Value::Array(a) => a,
            other => vec![other],
        };
        let mut drift = Vec::new();
        let mut info = Vec::new();
        for it in &items {
            let (d, n) = check_one(it);
            drift.extend(d);
            info.extend(n);
        }
        if drift.is_empty() {
            println!("clean  {p}  ({} value(s))", items.len());
        } else {
            any_drift = true;
            println!("DRIFT  {p}:");
            for f in &drift {
                println!("    {f}");
            }
        }
        for n in &info {
            println!("    info: {n}");
        }
    }
    if any_drift {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}
