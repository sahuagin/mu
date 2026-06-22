use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JsonValue(serde_json::Value);

#[derive(Debug, thiserror::Error)]
pub enum JsonValueError {
    #[error("JSON value contains non-finite number")]
    NonFiniteNumber,
}

impl JsonValue {
    pub fn new(value: serde_json::Value) -> Result<Self, JsonValueError> {
        reject_non_finite(&value)?;
        Ok(Self(value))
    }
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }
}

impl TryFrom<serde_json::Value> for JsonValue {
    type Error = JsonValueError;
    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

fn reject_non_finite(value: &serde_json::Value) -> Result<(), JsonValueError> {
    match value {
        serde_json::Value::Number(n) if n.as_f64().is_some_and(|f| !f.is_finite()) => {
            Err(JsonValueError::NonFiniteNumber)
        }
        serde_json::Value::Array(xs) => xs.iter().try_for_each(reject_non_finite),
        serde_json::Value::Object(m) => m.values().try_for_each(reject_non_finite),
        _ => Ok(()),
    }
}
