//! [`FiniteF64`] — a float that cannot be NaN/±Inf.
//!
//! Non-finite floats have no `Eq`/total-order meaning and would poison any
//! containing type's trait surface (and fail JSON serialization). `FiniteF64`
//! makes the bad state unrepresentable: `new` returns `None` for non-finite, so
//! a value of this type is always finite — and we assert `Eq` on that basis.
//!
//! Used for typed scalar knobs (temperature, top_p). For schemaless JSON trees
//! use [`JsonValue`](crate::JsonValue) instead.

use serde::{Deserialize, Deserializer};

/// A guaranteed-finite f64.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct FiniteF64(f64);

// Sound: construction rejects non-finite, so no NaN ever lives here.
impl Eq for FiniteF64 {}

impl FiniteF64 {
    /// Wrap a float, returning `None` if it is NaN or ±Inf.
    pub fn new(v: f64) -> Option<Self> {
        v.is_finite().then_some(Self(v))
    }
    /// The inner finite value.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl From<FiniteF64> for f64 {
    fn from(f: FiniteF64) -> f64 {
        f.0
    }
}

/// The error from [`FiniteF64::try_from`]: the source `f64` was NaN or ±Inf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("f64 is not finite (NaN or ±Inf)")]
pub struct NonFinite;

impl TryFrom<f64> for FiniteF64 {
    type Error = NonFinite;
    /// The fallible inbound conversion. There is deliberately NO `From<f64>`:
    /// a non-finite float has no `FiniteF64` image, so a total `From` could only
    /// panic or silently coerce — either would break the type's whole invariant.
    /// `TryFrom` is the conversion seam; [`FiniteF64::new`] remains the `Option`
    /// form for callers that prefer it. (Reverse: `From<FiniteF64> for f64`.)
    fn try_from(v: f64) -> Result<Self, Self::Error> {
        Self::new(v).ok_or(NonFinite)
    }
}

/// Reads whatever is present and yields `Some(finite)` only for a finite
/// number; `null`, a non-number, or a non-finite number all coerce to `None`.
/// Field *absence* is handled by `#[serde(default)]` (yields `None` without
/// calling this).
///
/// This only matters on the INBOUND path, and `MessagesRequest` is an OUTBOUND
/// type — we construct requests, we don't receive them — so the live wire never
/// exercises this. It earns its place on two narrow paths: the round-trip /
/// replay test tier (deserializing a logged request back), and the "bug or
/// corruption" backstop. The point is cascade containment: a single non-finite
/// that slips in must die HERE as one coerced-to-absent field, not propagate
/// (a NaN silently breaks every comparison it touches; an unhandled deserialize
/// error fails the whole message → turn → session). Coerce silently and keep
/// going — no logging (library-internal logging is noise, and on a degraded
/// substrate it would drown the signal it pretends to raise).
pub fn deserialize_option_finite<'de, D>(d: D) -> Result<Option<FiniteF64>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(v.as_f64().and_then(FiniteF64::new))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct Holder {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_option_finite"
        )]
        x: Option<FiniteF64>,
    }

    #[test]
    fn new_rejects_non_finite() {
        assert!(FiniteF64::new(f64::NAN).is_none());
        assert!(FiniteF64::new(f64::INFINITY).is_none());
        assert!(FiniteF64::new(f64::NEG_INFINITY).is_none());
        assert_eq!(FiniteF64::new(0.7).map(|f| f.get()), Some(0.7));
    }

    #[test]
    fn try_from_f64_is_fallible_and_round_trips_via_from() {
        // Finite: TryFrom succeeds and the reverse From recovers the value.
        let f = FiniteF64::try_from(0.7).expect("finite must convert");
        assert_eq!(f64::from(f), 0.7);
        // Non-finite: TryFrom is the seam that rejects, mirroring `new`.
        assert_eq!(FiniteF64::try_from(f64::NAN), Err(NonFinite));
        assert_eq!(FiniteF64::try_from(f64::INFINITY), Err(NonFinite));
        assert_eq!(FiniteF64::try_from(f64::NEG_INFINITY), Err(NonFinite));
    }

    #[test]
    fn finite_serializes_as_plain_float() {
        let h = Holder {
            x: FiniteF64::new(0.5),
        };
        assert_eq!(
            serde_json::to_value(&h).unwrap(),
            serde_json::json!({"x": 0.5})
        );
    }

    #[test]
    fn none_skips_the_field() {
        let h = Holder { x: None };
        assert_eq!(serde_json::to_value(&h).unwrap(), serde_json::json!({}));
    }

    #[test]
    fn eq_holds() {
        assert_eq!(
            Holder {
                x: FiniteF64::new(1.0)
            },
            Holder {
                x: FiniteF64::new(1.0)
            }
        );
    }

    #[test]
    fn absent_field_is_none() {
        let h: Holder = serde_json::from_str("{}").unwrap();
        assert_eq!(h.x, None);
    }

    #[test]
    fn json_null_field_is_none() {
        // A present null must coerce to None, not error.
        let h: Holder = serde_json::from_str(r#"{"x": null}"#).unwrap();
        assert_eq!(h.x, None);
    }

    #[test]
    fn json_finite_field_parses() {
        let h: Holder = serde_json::from_str(r#"{"x": 0.9}"#).unwrap();
        assert_eq!(h.x.map(|f| f.get()), Some(0.9));
    }

    #[test]
    fn wrong_type_coerces_to_none_not_error() {
        // A non-number where the knob goes (corruption / replay) must
        // coerce to None, not fail the whole struct deserialize. (Value absorbs
        // any JSON shape; only a finite number survives.)
        let h: Holder = serde_json::from_str(r#"{"x": "not a number"}"#)
            .expect("wrong-type knob must not fail the whole struct");
        assert_eq!(h.x, None);
        let h2: Holder = serde_json::from_str(r#"{"x": [1,2,3]}"#).unwrap();
        assert_eq!(h2.x, None);
    }
}
