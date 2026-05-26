//! mu-bridge: Claude-code JSONL parsing → mu event format, exposed via PyO3.
//!
//! Stable parsing logic ported from scripts/import-claude-history.py.
//! Python continues to own orchestration and experimental detectors;
//! this crate owns the invariants (event schema, type correctness,
//! token accounting).

use pyo3::prelude::*;

mod parse;
mod track;
mod types;

pub use parse::convert_session;
pub use track::ContextTracker;
pub use types::*;

// ─── PyO3 module ─────────────────────────────────────────────────────

#[pymodule]
fn mu_bridge(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMuEvent>()?;
    m.add_class::<PyContextTracker>()?;
    m.add_function(wrap_pyfunction!(py_parse_cc_line, m)?)?;
    m.add_function(wrap_pyfunction!(py_convert_session, m)?)?;
    Ok(())
}

/// Parse one claude-code JSONL line into a typed event dict.
#[pyfunction]
#[pyo3(name = "parse_cc_line")]
fn py_parse_cc_line(json_line: &str) -> PyResult<Option<PyMuEvent>> {
    let cc_event: serde_json::Value = serde_json::from_str(json_line)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    let events = parse::convert_one_event(&cc_event, 1);
    Ok(events.into_iter().next().map(PyMuEvent))
}

/// Convert a full session (list of JSON strings) to mu events.
#[pyfunction]
#[pyo3(name = "convert_session")]
fn py_convert_session(lines: Vec<String>, session_id: &str) -> PyResult<Vec<PyMuEvent>> {
    let cc_events: Vec<serde_json::Value> = lines
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let mu_events = parse::convert_session(&cc_events, session_id);
    Ok(mu_events.into_iter().map(PyMuEvent).collect())
}

// ─── Python-visible event wrapper ────────────────────────────────────

#[pyclass]
#[derive(Clone)]
struct PyMuEvent(MuEvent);

#[pymethods]
impl PyMuEvent {
    #[getter]
    fn id(&self) -> u64 {
        self.0.id
    }

    #[getter]
    fn timestamp_unix_ms(&self) -> u64 {
        self.0.timestamp_unix_ms
    }

    #[getter]
    fn kind(&self) -> &str {
        self.0.payload.kind()
    }

    fn to_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.0)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }

    fn __repr__(&self) -> String {
        format!(
            "MuEvent(id={}, kind={:?}, ts={})",
            self.0.id,
            self.0.payload.kind(),
            self.0.timestamp_unix_ms
        )
    }
}

// ─── Python-visible context tracker ──────────────────────────────────

#[pyclass]
struct PyContextTracker {
    inner: ContextTracker,
}

#[pymethods]
impl PyContextTracker {
    #[new]
    #[pyo3(signature = (threshold_tokens=250_000))]
    fn new(threshold_tokens: u64) -> Self {
        Self {
            inner: ContextTracker::new(threshold_tokens),
        }
    }

    fn feed_json(&mut self, json_line: &str) -> PyResult<()> {
        let cc_event: serde_json::Value = serde_json::from_str(json_line)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        self.inner.feed(&cc_event);
        Ok(())
    }

    fn should_compact(&self) -> bool {
        self.inner.should_compact()
    }

    #[getter]
    fn current_tokens(&self) -> u64 {
        self.inner.current_tokens()
    }

    #[getter]
    fn turn_count(&self) -> usize {
        self.inner.turn_count()
    }

    #[getter]
    fn fill_ratio(&self) -> f64 {
        self.inner.fill_ratio()
    }
}
