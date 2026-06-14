//! mu-anthropic-py — thin pyo3 binding over `mu-anthropic`.
//!
//! RULE (mu-anthropic AGENTS.md): this crate contains ONLY `#[pyfunction]` /
//! `#[pymethods]` that delegate one-to-one to `mu-anthropic`. No logic, no
//! parsing, no branching beyond error conversion. That keeps "we don't test the
//! binding seam" true by construction — the Rust lib is tested in Rust, the
//! Python wheel is tested by calling these exports from Python.
//!
//! Each parse fn takes wire JSON (a str) and returns the round-tripped JSON
//! (Rust deserialized it into a typed value, then re-serialized) — so a caller
//! gets proof the typed model accepted the input, and a normalized form back.
//! Python never drives serde; it hands a string and gets a string.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

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

#[pymodule]
fn mu_anthropic_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parse_response_message, m)?)?;
    m.add_function(wrap_pyfunction!(parse_stream_event, m)?)?;
    m.add_function(wrap_pyfunction!(parse_request, m)?)?;
    m.add_function(wrap_pyfunction!(is_valid_response_message, m)?)?;
    Ok(())
}
