//! L1 — the typed command/response interface ("MCP 2.0"), transport-agnostic.
//!
//! This is OURS and stable. It is what mu code (behind a per-service proxy)
//! and the service handler both speak; the bus (NATS) only ever carries the
//! serialized [`Envelope`] as an opaque payload, so the bus is swappable
//! without touching this contract. The first slice models one service —
//! `code_index` — with its two current operations; adding a service = adding
//! `Command`/`CommandResult` variants (or a sibling enum), never a new wire.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Any typed command that crosses the mesh names the capability right it
/// requires. Every service's command enum implements this; the shared
/// authorization gate ([`crate::capability`]) checks the request's biscuit
/// for exactly this right before the service runs. New service = new command
/// enum implementing this — never a new envelope, never a new wire.
pub trait MeshCommand {
    fn required_right(&self) -> &'static str;
}

/// A typed request as it crosses the mesh, generic over any [`MeshCommand`]:
/// a correlation id, an in-band capability that authorizes the work (a
/// serialized biscuit token — a request is accompanied by the grant that
/// permits it), and the typed command. Serialized as the opaque bus payload,
/// so the bus is swappable without touching this contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<C> {
    /// Correlation id — echoed on the [`Reply`]. Carried in-envelope so
    /// correlation is independent of any one transport's mechanism.
    pub id: Ulid,
    /// Object-capability grant for THIS request (biscuit bytes). The service
    /// verifies it authorizes `command` before any work. Empty = deliberately
    /// unauthorized (used to prove the service refuses).
    #[serde(with = "serde_bytes_b64")]
    pub capability: Vec<u8>,
    /// The typed operation.
    pub command: C,
}

/// A typed response: echoes the request id, carries the typed result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply<R> {
    pub id: Ulid,
    pub result: R,
}

/// The typed operations of the `code_index` service (its current surface:
/// `code_recall`, `code_status`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Command {
    /// Hybrid symbol/concept recall. `db` targets a specific index (a name
    /// resolving to `~/.cache/code_index/<name>.db` on the serving side, or
    /// an absolute path); `None` = the service's default index. Restores the
    /// repo-targeting the direct MCP tool always had — dropping it made the
    /// mesh hardcode one repo (operator regression report, 2026-07-21).
    CodeRecall {
        query: String,
        limit: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        db: Option<String>,
    },
    /// Index health.
    CodeStatus,
}

impl MeshCommand for Command {
    fn required_right(&self) -> &'static str {
        match self {
            Command::CodeRecall { .. } => "code_recall",
            Command::CodeStatus => "code_status",
        }
    }
}

/// code_index's request/response, as instances of the generic envelope.
pub type Request = Envelope<Command>;
pub type Response = Reply<CommandResult>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CommandResult {
    CodeRecall(Vec<Hit>),
    CodeStatus(StatusInfo),
    /// The service refused or failed — carries a human-readable reason. An
    /// unauthorized request (bad/missing capability) surfaces here.
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub symbol: String,
    pub path: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusInfo {
    pub indexed_repos: u32,
    pub healthy: bool,
}

/// Base64 for the capability bytes so the envelope is clean JSON on the wire
/// (the slice serializes envelopes as JSON; a binary codec is a later L4
/// upgrade and doesn't touch this contract).
mod serde_bytes_b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    // Minimal base64 (no external dep) — the slice's payloads are small.
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
            out.push(B64[(n >> 18 & 63) as usize] as char);
            out.push(B64[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                B64[(n >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                B64[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        let val = |c: u8| B64.iter().position(|&x| x == c).map(|p| p as u32);
        let mut out = Vec::new();
        let cs: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
        for chunk in cs.chunks(4) {
            let mut n = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                n |= val(c).ok_or_else(|| serde::de::Error::custom("bad base64"))? << (18 - 6 * i);
            }
            out.push((n >> 16 & 0xff) as u8);
            if chunk.len() > 2 {
                out.push((n >> 8 & 0xff) as u8);
            }
            if chunk.len() > 3 {
                out.push((n & 0xff) as u8);
            }
        }
        Ok(out)
    }
}
