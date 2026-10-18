//! mu-events-py — thin pyo3 binding over mu-core's event-log reader.
//!
//! RULE (mirrors mu-anthropic-py): this crate contains ONLY `#[pyfunction]`s
//! that delegate to mu-core's typed `SessionEvent` model. No logic, no parsing
//! beyond error conversion — the Rust lib is tested in Rust, the wheel is tested
//! from Python.
//!
//! Python hands a path (or a wire line) and gets typed-validated JSON back; it
//! never drives serde itself. The typed model is the source of truth, so
//! analytics stop re-parsing raw JSONL and guessing the schema: a schema drift
//! surfaces as a non-zero malformed count or a `False` from `is_valid_event`,
//! not a silent mis-parse.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use mu_core::event_log::{SessionEvent, SessionEventLog};

/// Read a mu-core event-log JSONL file, typed-deserialize each line into
/// `SessionEvent` (validating; malformed lines skipped and counted), and return
/// `(json_array, malformed_count)`. The returned JSON is the typed round-trip
/// (normalized) — proof the schema accepted every event. Raises ValueError only
/// if the file can't be opened.
#[pyfunction]
fn read_events(path: &str) -> PyResult<(String, usize)> {
    let (log, malformed) = SessionEventLog::from_jsonl(std::path::Path::new(path))
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    let json =
        serde_json::to_string(&log.snapshot()).map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok((json, malformed))
}

/// The redacted view of a session log (mu-09z5y part 3): like
/// `read_events`, but every tool call judged to carry a credential has its
/// arguments withheld (`mu_core::redaction::redact_log`), with the rules mu
/// itself loads (built-in plus the operator's `secret_scan.toml`). `key_path`
/// is the HMAC key file; `None` means the standard
/// `~/.config/mu/redaction.key`. Returns the events as a JSON array. Raises
/// ValueError for a missing or unusable key, bad rules, an unreadable file,
/// or a log with any line that doesn't parse (refused whole: a dropped
/// line can take a tag with it).
#[pyfunction]
#[pyo3(signature = (path, key_path=None))]
fn redacted_events(path: &str, key_path: Option<&str>) -> PyResult<String> {
    use mu_core::redaction::{redact_log, RedactionKey};
    let err = |e: &dyn std::fmt::Display| PyValueError::new_err(e.to_string());
    let key_path = match key_path {
        Some(p) => std::path::PathBuf::from(p),
        None => RedactionKey::default_path()
            .ok_or_else(|| PyValueError::new_err("no config directory; pass key_path"))?,
    };
    let key = RedactionKey::load(&key_path).map_err(|e| err(&e))?;
    let scanner = mu_core::secret_scan::SecretScanner::load_default().map_err(|e| err(&e))?;
    let events = redact_log(std::path::Path::new(path), &scanner, &key).map_err(|e| err(&e))?;
    serde_json::to_string(&events).map_err(|e| err(&e))
}

/// Validate + normalize a single `SessionEvent` wire line into typed JSON.
/// Raises ValueError if it doesn't match the typed schema.
#[pyfunction]
fn parse_event(json: &str) -> PyResult<String> {
    let ev: SessionEvent =
        serde_json::from_str(json).map_err(|e| PyValueError::new_err(e.to_string()))?;
    serde_json::to_string(&ev).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// `True` if the line parses as a valid `SessionEvent` — a cheap schema-drift gate.
#[pyfunction]
fn is_valid_event(json: &str) -> bool {
    serde_json::from_str::<SessionEvent>(json).is_ok()
}

#[pymodule]
fn mu_events_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(read_events, m)?)?;
    m.add_function(wrap_pyfunction!(redacted_events, m)?)?;
    m.add_function(wrap_pyfunction!(parse_event, m)?)?;
    m.add_function(wrap_pyfunction!(is_valid_event, m)?)?;
    Ok(())
}
