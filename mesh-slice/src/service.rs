//! The `code_index` service handler — a NATS Micro service (mu-wxc4 N5/N7).
//!
//! `start` registers the service on NATS, which makes it **discoverable by
//! name** ($SRV.PING/INFO) and **addressable by subject** — no `ip:port`
//! anywhere. Each endpoint decodes the typed [`Request`], verifies the
//! in-band capability (N12) before doing any work, and responds with the
//! typed [`Response`]. The index itself is stubbed for the slice; swapping
//! in the real code_index backend touches only `run_command`, not the wire.

use std::sync::Arc;

use anyhow::Result;
use async_nats::service::ServiceExt;
use biscuit_auth::PublicKey;
use bytes::Bytes;
use futures::StreamExt;
use ulid::Ulid;

use crate::capability;
use crate::contract::MeshCommand;
use crate::contract::{Command, CommandResult, Hit, Request, Response, StatusInfo};

/// The service's addressable name/subject root. Consumers reach it by THIS,
/// never by host:port. Endpoints hang off it: `<SUBJECT>.recall`, `.status`.
pub const SERVICE_SUBJECT: &str = "mu.svc.code_index";
pub const SERVICE_NAME: &str = "code_index";

/// Start the code_index service on `client`. Capabilities are verified
/// against `issuer` (the public key of whoever is trusted to grant rights).
/// Returns once endpoints are subscribed; handlers run on spawned tasks for
/// the connection's lifetime.
pub async fn start(
    client: async_nats::Client,
    issuer: PublicKey,
) -> Result<async_nats::service::Service> {
    let service = client
        .service_builder()
        .description("mu code_index — hybrid symbol/concept recall")
        .start(SERVICE_NAME, "0.1.0")
        .await
        .map_err(|e| anyhow::anyhow!("start service: {e}"))?;

    let group = service.group(SERVICE_SUBJECT);
    let recall = group
        .endpoint("recall")
        .await
        .map_err(|e| anyhow::anyhow!("recall endpoint: {e}"))?;
    let status = group
        .endpoint("status")
        .await
        .map_err(|e| anyhow::anyhow!("status endpoint: {e}"))?;

    let issuer = Arc::new(issuer);
    for mut endpoint in [recall, status] {
        let issuer = issuer.clone();
        tokio::spawn(async move {
            while let Some(req) = endpoint.next().await {
                let reply = serve(&req.message.payload, &issuer);
                let _ = req.respond(Ok(reply)).await;
            }
        });
    }
    // `subscribe()` returns before the SUB reaches the server; without this
    // flush a caller that requests immediately after `start()` returns can
    // race the registration and get a spurious no-responders. Flushing means
    // "started" == subscriptions are live server-side.
    client
        .flush()
        .await
        .map_err(|e| anyhow::anyhow!("flush after start: {e}"))?;
    // The caller MUST keep this Service handle alive: dropping it tears down
    // the endpoints (their subscriptions) even though the handler tasks
    // still hold the Endpoint streams. Returned so the owner outlives the
    // calls. In mu proper this lives in the daemon's long-lived state.
    Ok(service)
}

/// Decode → authorize → run → encode. Every failure path produces a typed
/// `Error` response (never a panic, never a silent drop).
fn serve(payload: &[u8], issuer: &PublicKey) -> Bytes {
    let request: Request = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => return encode_err(Ulid::nil(), format!("malformed request: {e}")),
    };

    // Capability gate (N12): the request must carry a grant, signed by the
    // issuer, that authorizes exactly this command. No grant → no work.
    if !capability::authorizes(
        &request.capability,
        *issuer,
        request.command.required_right(),
    ) {
        return encode_err(
            request.id,
            format!(
                "unauthorized: capability does not grant `{}`",
                request.command.required_right()
            ),
        );
    }

    let result = run_command(&request.command);
    encode(&Response {
        id: request.id,
        result,
    })
}

/// The service's actual work. Stubbed for the slice; the real code_index
/// backend plugs in HERE without touching the contract or the wire.
fn run_command(command: &Command) -> CommandResult {
    match command {
        Command::CodeRecall { query, limit } => {
            let n = limit.unwrap_or(3).min(3) as usize;
            let hits = (0..n)
                .map(|i| Hit {
                    symbol: format!("hit_{i}_for::{}", query.replace(' ', "_")),
                    path: format!("src/stub_{i}.rs"),
                    score: 0.9 - (i as f32) * 0.1,
                })
                .collect();
            CommandResult::CodeRecall(hits)
        }
        Command::CodeStatus => CommandResult::CodeStatus(StatusInfo {
            indexed_repos: 42,
            healthy: true,
        }),
    }
}

fn encode(response: &Response) -> Bytes {
    // Response is serde-derived and cannot fail to serialize; if it somehow
    // did, fall back to a typed error rather than unwrap-panicking.
    match serde_json::to_vec(response) {
        Ok(v) => Bytes::from(v),
        Err(e) => encode_err(response.id, format!("encode failed: {e}")),
    }
}

fn encode_err(id: Ulid, msg: String) -> Bytes {
    let r = Response {
        id,
        result: CommandResult::Error(msg),
    };
    Bytes::from(serde_json::to_vec(&r).unwrap_or_default())
}
