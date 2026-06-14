//! drift_check — a protocol-drift canary for `mu-anthropic`.
//!
//! Reads Anthropic `POST /v1/messages` RESPONSE JSON files (paths as argv) and,
//! for each, parses with mu-anthropic's types and reports DRIFT when the wire
//! carries something the library does not model:
//!
//!   * a field that round-trips DIFFERENTLY — dropped or changed — i.e. a new
//!     field on a type WITHOUT a catch-all tail (`Message`, `Usage`, the
//!     fixed-field block variants); or
//!   * a content block of an UNMODELED `type` (`ContentBlock::Unknown`).
//!
//! Why a dedicated canary: the library is intentionally LENIENT — it never errors
//! on an unknown field (it lands in `Unknown`/`extra`), so a plain parse won't
//! surface drift. This re-serializes and diffs to surface it.
//!
//! Exit: 0 = clean, 3 = drift detected, 2 = usage error.
//!
//! KNOWN LIMITATION: additions that land inside an `extra`/`config` tail
//! (`ServerToolResult`, `Container`, `StopDetails`, `Metadata`) round-trip
//! exactly and are NOT flagged here — only dropped fields and unmodeled block
//! TYPES are. (Tighten later with an extra-key allowlist.)

use mu_anthropic::{ContentBlock, ResponseMessage};
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

/// Collect the JSON paths where `orig` and the round-tripped `back` differ.
/// A field present-and-null in `orig` but absent in `back` is NOT a drop —
/// the library omits `None`/absent fields, which is wire-equivalent to null.
fn diff(orig: &Value, back: &Value, path: &str, out: &mut Vec<String>) {
    match (orig, back) {
        (Value::Object(mo), Value::Object(mb)) => {
            for (k, vo) in mo {
                let p = format!("{path}.{k}");
                match mb.get(k) {
                    Some(vb) => diff(vo, vb, &p, out),
                    None if vo.is_null() => {} // null ≈ omitted; not drift
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

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: drift_check <response.json> [more.json ...]");
        return ExitCode::from(2);
    }
    let mut drift = false;
    for p in &paths {
        let raw = match fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) => {
                println!("ERROR  {p}: {e}");
                drift = true;
                continue;
            }
        };
        let orig: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                println!("ERROR  {p}: not JSON: {e}");
                drift = true;
                continue;
            }
        };
        if orig.get("type").and_then(Value::as_str) == Some("error") {
            println!("skip   {p}: API error response");
            continue;
        }
        let msg: ResponseMessage = match serde_json::from_value(orig.clone()) {
            Ok(m) => m,
            Err(e) => {
                println!("DRIFT  {p}: does not parse as a Message: {e}");
                drift = true;
                continue;
            }
        };
        let mut findings = Vec::new();
        // (1) round-trip: a dropped/changed field means an unmodeled addition on
        //     a tail-less type.
        let back = serde_json::to_value(&msg).expect("serialize");
        diff(&orig, &back, "$", &mut findings);
        // (2) an unmodeled block type lands in Unknown.
        for (i, b) in msg.content.iter().enumerate() {
            if let ContentBlock::Unknown(v) = b {
                let t = v
                    .as_value()
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                findings.push(format!("UNMODELED-BLOCK $.content[{i}].type = {t:?}"));
            }
        }
        if findings.is_empty() {
            println!("clean  {p}");
        } else {
            drift = true;
            println!("DRIFT  {p}:");
            for f in &findings {
                println!("    {f}");
            }
        }
    }
    if drift {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}
