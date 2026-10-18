//! mu-anthropic-py — thin pyo3 binding over `mu-anthropic`.
//!
//! RULE (mu-anthropic AGENTS.md): this crate contains ONLY `#[pyfunction]` /
//! `#[pymethods]` that delegate one-to-one to `mu-anthropic`. No logic, no
//! parsing, no branching beyond error conversion. That keeps "we don't test the
//! binding seam" true by construction — the Rust lib is tested in Rust, the
//! Python wheel is tested by calling these exports from Python.
//!
//! Two shapes of export, same thinness:
//!   - the `parse_*` fns take wire JSON (a str) and return round-tripped JSON
//!     (Rust deserialized into a typed value, then re-serialized) — proof the
//!     typed model accepted the input, plus a normalized string back.
//!   - `response_message_to_py` takes wire JSON and returns a NATIVE Python
//!     object graph (dict/list/int/str/bool/None) projected straight off the
//!     typed value via `pythonize`. One serde parse, one projection, no JSON
//!     string in between — so the consumer (analytics/dataframe) reads parsed
//!     values directly instead of `json.loads`-ing a re-serialized string.
//! Either way Python never drives serde: it hands a string, serde does the one
//! parse, and the binding is still mechanical delegation (the `pythonize` call
//! drives the SAME tested `Serialize` impls the `parse_*` fns use — no walk,
//! no branching, no logic in the seam; AGENTS.md rule 9 holds).

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pythonize::pythonize;

use mu_anthropic::{MessagesRequest, ResponseMessage, StreamEvent};

/// Parse a wire **response message** (the `.message` object cc logs store
/// verbatim) into mu-anthropic's typed `ResponseMessage`, returning normalized
/// JSON. Raises ValueError if it doesn't match the wire contract.
#[pyfunction]
fn parse_response_message(json: &str) -> PyResult<String> {
    let v: ResponseMessage =
        serde_json::from_str(json).map_err(|e| PyValueError::new_err(e.to_string()))?;
    serde_json::to_string(&v).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Parse a single SSE event's `data` payload into a typed `StreamEvent`.
#[pyfunction]
fn parse_stream_event(json: &str) -> PyResult<String> {
    let v: StreamEvent =
        serde_json::from_str(json).map_err(|e| PyValueError::new_err(e.to_string()))?;
    serde_json::to_string(&v).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Parse an outbound **request body** into a typed `MessagesRequest`.
#[pyfunction]
fn parse_request(json: &str) -> PyResult<String> {
    let v: MessagesRequest =
        serde_json::from_str(json).map_err(|e| PyValueError::new_err(e.to_string()))?;
    serde_json::to_string(&v).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// `True` if the input parses as a valid wire response message — for analytics
/// that want a cheap validity gate without materializing the value.
#[pyfunction]
fn is_valid_response_message(json: &str) -> bool {
    serde_json::from_str::<ResponseMessage>(json).is_ok()
}

/// Parse a wire **response message** and project the typed value DIRECTLY into
/// native Python objects — a `dict` with `content` as a `list` of block dicts,
/// `usage` as a nested dict of ints, tool `input` / server-tool-result `content`
/// as native dicts/lists, and so on. No JSON string is produced: the consumer
/// reads parsed values off the returned object instead of re-parsing a string.
///
/// Fidelity + forward-compat come entirely from the typed model (`mu-anthropic`),
/// not from this seam: arbitrary tool JSON rides through as native dicts, and a
/// content block whose `type` this crate does not model is preserved verbatim in
/// the projected `content` list (it round-trips as its raw object). A message
/// with a KNOWN block carrying broken/missing fields — or any other contract
/// violation — raises `ValueError` carrying serde's reason/location, so a
/// consumer can catch it per-message, keep the raw record, and surface drift.
#[pyfunction]
fn response_message_to_py<'py>(py: Python<'py>, json: &str) -> PyResult<Bound<'py, PyAny>> {
    let v: ResponseMessage =
        serde_json::from_str(json).map_err(|e| PyValueError::new_err(e.to_string()))?;
    pythonize(py, &v).map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pymodule]
fn mu_anthropic_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse_response_message, m)?)?;
    m.add_function(wrap_pyfunction!(parse_stream_event, m)?)?;
    m.add_function(wrap_pyfunction!(parse_request, m)?)?;
    m.add_function(wrap_pyfunction!(is_valid_response_message, m)?)?;
    m.add_function(wrap_pyfunction!(response_message_to_py, m)?)?;
    Ok(())
}
