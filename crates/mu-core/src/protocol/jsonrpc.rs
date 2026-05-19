//! JSON-RPC 2.0 envelope (`Request`, `Response`, `ErrorObject`,
//! `Notification`) plus the trivial `ping` round-trip that bottoms-out
//! the wire protocol surface.
//!
//! Everything else in `protocol::*` is parameterised by one of these
//! envelope types — `Request<CreateSessionRequest>`,
//! `Response<PingResponse>`, etc. — so this is the foundation submodule.
//!
//! Extracted from `protocol.rs` per mu-6a8 phase 6 (2026-05-18); re-exported
//! by `protocol::*` so external callers see no API change.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request<P> {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: P,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response<R> {
    Ok {
        jsonrpc: String,
        id: Value,
        result: R,
    },
    Err {
        jsonrpc: String,
        id: Value,
        error: ErrorObject,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification<P> {
    pub jsonrpc: String,
    pub method: String,
    pub params: P,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingRequest;

impl PingRequest {
    pub const METHOD: &'static str = "ping";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PingResponse {
    pub pong: bool,
    pub server_version: String,
}
