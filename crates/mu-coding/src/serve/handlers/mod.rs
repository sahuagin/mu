//! Per-domain request handlers for JSON-RPC methods.
//!
//! Dispatched by the router in [`super::dispatch`]. Methods are organized
//! by domain (session, daemon, mailbox, peer).

pub mod auth;
pub mod daemon;
pub mod mailbox;
pub mod session;

use serde_json::Value;

/// Serialize a value to JSON, falling back to `Value::Null` on error.
pub fn to_value_or_null<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}
