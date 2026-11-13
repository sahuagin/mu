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

/// Diagnose one session log the way `mu recover` would, without writing
/// anything: returns mu-core's `SessionHealth` as JSON (status, gaps, torn
/// lines, fork point, open tool calls). `predecessor` is the log a legacy
/// head forked from. Raises ValueError only if a file can't be read.
#[pyfunction]
#[pyo3(signature = (path, predecessor=None))]
fn diagnose_session(path: &str, predecessor: Option<&str>) -> PyResult<String> {
    let health = mu_core::agent::diagnose_session_log(
        std::path::Path::new(path),
        predecessor.map(std::path::Path::new),
    )
    .map_err(|e| PyValueError::new_err(e.to_string()))?;
    serde_json::to_string(&health).map_err(|e| PyValueError::new_err(e.to_string()))
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
    m.add_function(wrap_pyfunction!(parse_event, m)?)?;
    m.add_function(wrap_pyfunction!(diagnose_session, m)?)?;
    m.add_function(wrap_pyfunction!(is_valid_event, m)?)?;
    Ok(())
}
