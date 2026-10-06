//! A JSON value that keeps object keys in WIRE (arrival) order — not sorted.
//!
//! The workspace's `serde_json` is built without `preserve_order`, so a
//! `serde_json::Value` object is a `BTreeMap` and sorts its keys. That is
//! wrong for provider blocks whose key order carries meaning (the OpenAI
//! codex `usage.attribution.items` object lists input items in request
//! order). [`WireOrderJson`] holds the same data with objects as ordered
//! `(key, value)` lists, and (de)serializes them in that order — including
//! when read back through serde's buffered content (internally tagged
//! enums), which also keeps map entries in document order.

use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::{Number, Value};

/// JSON with object keys kept in document order. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireOrderJson {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<WireOrderJson>),
    Object(Vec<(String, WireOrderJson)>),
}

impl WireOrderJson {
    /// Parse JSON text, keeping object key order.
    pub fn from_json_str(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }

    /// Serialize any value into an [`WireOrderJson`], keeping the key order its
    /// `Serialize` impl emits.
    pub fn from_serialize<T: Serialize + ?Sized>(v: &T) -> serde_json::Result<Self> {
        Self::from_json_str(&serde_json::to_string(v)?)
    }

    /// The member `key` of an object (first match), else None.
    pub fn get(&self, key: &str) -> Option<&WireOrderJson> {
        match self {
            WireOrderJson::Object(entries) => {
                entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    /// An object's keys in document order (empty for a non-object).
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        let entries: &[(String, WireOrderJson)] = match self {
            WireOrderJson::Object(entries) => entries,
            _ => &[],
        };
        entries.iter().map(|(k, _)| k.as_str())
    }

    /// Convert to a plain `serde_json::Value`. Object keys come out sorted
    /// (that is what `Value` does in this workspace).
    pub fn to_value(&self) -> Value {
        match self {
            WireOrderJson::Null => Value::Null,
            WireOrderJson::Bool(b) => Value::Bool(*b),
            WireOrderJson::Number(n) => Value::Number(n.clone()),
            WireOrderJson::String(s) => Value::String(s.clone()),
            WireOrderJson::Array(a) => Value::Array(a.iter().map(Self::to_value).collect()),
            WireOrderJson::Object(o) => {
                Value::Object(o.iter().map(|(k, v)| (k.clone(), v.to_value())).collect())
            }
        }
    }
}

impl Serialize for WireOrderJson {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            WireOrderJson::Null => ser.serialize_unit(),
            WireOrderJson::Bool(b) => ser.serialize_bool(*b),
            WireOrderJson::Number(n) => n.serialize(ser),
            WireOrderJson::String(s) => ser.serialize_str(s),
            WireOrderJson::Array(a) => {
                let mut seq = ser.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            WireOrderJson::Object(o) => {
                let mut map = ser.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for WireOrderJson {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = WireOrderJson;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E>(self) -> Result<WireOrderJson, E> {
                Ok(WireOrderJson::Null)
            }
            fn visit_none<E>(self) -> Result<WireOrderJson, E> {
                Ok(WireOrderJson::Null)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<WireOrderJson, D::Error> {
                WireOrderJson::deserialize(d)
            }
            fn visit_bool<E>(self, b: bool) -> Result<WireOrderJson, E> {
                Ok(WireOrderJson::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<WireOrderJson, E> {
                Ok(WireOrderJson::Number(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<WireOrderJson, E> {
                Ok(WireOrderJson::Number(n.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<WireOrderJson, E> {
                // Refuse, don't coerce: a non-finite float is not JSON, and
                // silently storing `null` would lose data (and is what makes
                // `Eq` total here — `Number` cannot hold NaN/inf).
                Number::from_f64(n)
                    .map(WireOrderJson::Number)
                    .ok_or_else(|| {
                        E::custom(format_args!("non-finite number {n} is not valid JSON"))
                    })
            }
            fn visit_str<E>(self, s: &str) -> Result<WireOrderJson, E> {
                Ok(WireOrderJson::String(s.to_owned()))
            }
            fn visit_string<E>(self, s: String) -> Result<WireOrderJson, E> {
                Ok(WireOrderJson::String(s))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<WireOrderJson, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(v) = seq.next_element()? {
                    out.push(v);
                }
                Ok(WireOrderJson::Array(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WireOrderJson, A::Error> {
                let mut out = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((k, v)) = map.next_entry::<String, WireOrderJson>()? {
                    out.push((k, v));
                }
                Ok(WireOrderJson::Object(out))
            }
        }
        de.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_round_trip_in_document_order() -> serde_json::Result<()> {
        let text = r#"{"z":1,"a":[true,null,-2,1.5,"s"],"m":{"y":{},"b":[]}}"#;
        let j = WireOrderJson::from_json_str(text)?;
        assert_eq!(j.keys().collect::<Vec<_>>(), ["z", "a", "m"]);
        assert_eq!(serde_json::to_string(&j)?, text);
        assert_eq!(j.to_value(), serde_json::from_str::<Value>(text)?);
        Ok(())
    }

    /// Order also survives serde's buffered content (an internally tagged
    /// enum), the path an event-log payload takes on read-back.
    #[test]
    fn order_survives_internally_tagged_enum() -> serde_json::Result<()> {
        #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(tag = "type")]
        enum E {
            Raw { raw: WireOrderJson },
        }
        let text = r#"{"type":"Raw","raw":{"b":1,"a":2}}"#;
        let e: E = serde_json::from_str(text)?;
        let E::Raw { raw } = &e;
        assert_eq!(raw.keys().collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(serde_json::to_string(&e)?, text);
        Ok(())
    }
}

#[cfg(test)]
mod non_finite_tests {
    use super::*;
    use serde::de::IntoDeserializer;

    #[test]
    fn non_finite_floats_are_refused_not_coerced_to_null() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let de: serde::de::value::F64Deserializer<serde::de::value::Error> =
                bad.into_deserializer();
            let err = WireOrderJson::deserialize(de).unwrap_err().to_string();
            assert!(err.contains("non-finite"), "{bad}: {err}");
        }
        let de: serde::de::value::F64Deserializer<serde::de::value::Error> =
            1.5.into_deserializer();
        assert!(matches!(
            WireOrderJson::deserialize(de),
            Ok(WireOrderJson::Number(_))
        ));
    }
}
