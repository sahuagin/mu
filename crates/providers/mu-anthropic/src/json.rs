//! [`JsonValue`] — a quarantined `serde_json::Value`.
//!
//! `Value` cannot derive `Eq` (it may hold an `f64`, and `f64: !Eq`), and a
//! non-finite float (NaN / ±Inf) has no `Eq`/total-order meaning. Left bare,
//! that limitation PROPAGATES: every type containing a `Value` is forced to
//! `PartialEq`-only, rippling `!Eq` through the whole crate.
//!
//! `JsonValue` stops that ripple AT THE FIELD BOUNDARY. It validates on
//! construction (and on deserialize, via `try_from`) that no number anywhere in
//! the tree is non-finite, then ASSERTS `Eq` — earned by the invariant, not
//! argued. Types that hold a `JsonValue` are free to derive `Eq`/`Hash`; the
//! `Value`'s trait limits no longer dictate our trait surface.
//!
//! Pattern mirrored from mu-core's `ToolArgs` (bead mu-gdwd) — reimplemented
//! here, NOT imported, to keep this crate free of any mu dependency.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Arbitrary schemaless JSON (tool input, JSON Schema, opaque passthrough) with
/// a finite-numbers invariant, so containing types keep a full trait surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Value", into = "Value")]
pub struct JsonValue(Value);

// SAFETY OF Eq: `new`/`try_from` reject any NaN/±Inf at every depth, and JSON
// itself has no NaN/Inf literal (serde_json::Number::from_f64 returns None for
// them), so within this type `PartialEq` is total. Asserting `Eq` is honest.
impl Eq for JsonValue {}

/// Construction error: a number in the tree is not finite.
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum JsonValueError {
    #[error("non-finite number at path {path}: {value}")]
    NonFinite { path: String, value: f64 },
}

impl JsonValue {
    /// Wrap a `Value`, rejecting NaN/±Inf at any nesting depth.
    pub fn new(value: Value) -> Result<Self, JsonValueError> {
        validate(&value, "$")?;
        Ok(Self(value))
    }

    /// The wrapped value.
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Consume into the inner `Value`.
    pub fn into_value(self) -> Value {
        self.0
    }

    /// An empty JSON object (`{}`). The common "no input" tool-call default.
    pub fn empty_object() -> Self {
        Self(Value::Object(serde_json::Map::new()))
    }
}

impl TryFrom<Value> for JsonValue {
    type Error = JsonValueError;
    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<JsonValue> for Value {
    fn from(j: JsonValue) -> Value {
        j.0
    }
}

fn validate(v: &Value, path: &str) -> Result<(), JsonValueError> {
    match v {
        Value::Number(n) => {
            // serde_json only yields a finite f64 here (it rejects NaN/Inf at
            // parse), but a hand-built Number could differ; check defensively.
            if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return Err(JsonValueError::NonFinite {
                        path: path.to_string(),
                        value: f,
                    });
                }
            }
            Ok(())
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                validate(item, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for (k, val) in map {
                validate(val, &format!("{path}.{k}"))?;
            }
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_finite_numbers_at_depth() {
        let v = json!({"a": {"b": [1, 2.5, -0.0, {"c": 1e308}]}});
        assert!(JsonValue::new(v).is_ok());
    }

    #[test]
    fn accepts_strings_bools_nulls() {
        let v = json!({"s": "hi", "b": true, "n": null, "arr": [1, "a", null]});
        assert!(JsonValue::new(v).is_ok());
    }

    #[test]
    fn serde_json_rejects_nan_at_parse_time() {
        // The first line of defense: NaN/Inf can't even become a Number.
        assert!(serde_json::Number::from_f64(f64::NAN).is_none());
        assert!(serde_json::Number::from_f64(f64::INFINITY).is_none());
        assert!(serde_json::Number::from_f64(f64::NEG_INFINITY).is_none());
    }

    #[test]
    fn eq_is_usable() {
        let a = JsonValue::new(json!({"x": 1})).unwrap();
        let b = JsonValue::new(json!({"x": 1})).unwrap();
        assert_eq!(a, b); // exercises the asserted Eq
    }

    #[test]
    fn round_trips_through_serde() {
        let v = json!({"nested": {"arr": [1, 2, 3], "s": "hi"}});
        let j = JsonValue::new(v.clone()).unwrap();
        let s = serde_json::to_string(&j).unwrap();
        let back: JsonValue = serde_json::from_str(&s).unwrap();
        assert_eq!(back.as_value(), &v);
    }

    #[test]
    fn empty_object_helper() {
        assert_eq!(JsonValue::empty_object().as_value(), &json!({}));
    }
}
