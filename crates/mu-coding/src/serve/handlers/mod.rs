//! Per-domain request handlers for JSON-RPC methods.
//!
//! Dispatched by the router in [`super::dispatch`]. Methods are organized
//! by domain (session, daemon, mailbox, peer).

pub mod auth;
pub mod capabilities;
pub mod daemon;
pub mod mailbox;
pub mod session;

use serde_json::Value;

/// Serialize a value to JSON, falling back to `Value::Null` on error.
pub fn to_value_or_null<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Unwrap `Ok` or early-return a JSON-RPC error response carrying the
/// error's `Display` — compresses the repeated match-or-`err_response`
/// blocks (PR #275 review). The wire message is `"<ctx>: <error>"`,
/// byte-identical to the inline `format!("…: {e}")` matches it replaces.
macro_rules! ok_or_respond {
    ($expr:expr, $id:expr, $code:expr, $ctx:literal) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                return mu_core::transport::err_response(
                    $id,
                    $code,
                    format!(concat!($ctx, ": {e}"), e = e),
                )
            }
        }
    };
}
pub(crate) use ok_or_respond;

/// `Option` sibling of [`ok_or_respond!`]: unwrap `Some` or early-return
/// a JSON-RPC error response with the given message (evaluated only on
/// the `None` path).
macro_rules! some_or_respond {
    ($expr:expr, $id:expr, $code:expr, $msg:expr) => {
        match $expr {
            Some(v) => v,
            None => return mu_core::transport::err_response($id, $code, $msg),
        }
    };
}
pub(crate) use some_or_respond;
