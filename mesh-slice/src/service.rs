//! The `code_index` service handler — a NATS Micro service (mu-wxc4 N5/N7).
//!
//! `start` registers the service on NATS, which makes it **discoverable by
//! name** ($SRV.PING/INFO) and **addressable by subject** — no `ip:port`
//! anywhere. Each endpoint decodes the typed [`Request`], verifies the
//! in-band capability (N12) before doing any work, and responds with the
//! typed [`Response`].
//!
//! The backend is chosen by [`Backend`]: `Stub` for deterministic tests, or
//! `CodeIndexMcp` — a thin **protocol adapter** that forwards each command to
//! the real, already-running `code_index` MCP server and relays its answer.
//! The mesh is the adapter; the index itself is unchanged. Swapping backends
//! touches only this file, never the contract or the wire.

use std::sync::Arc;

use anyhow::Result;
use async_nats::service::ServiceExt as NatsServiceExt;
use biscuit_auth::PublicKey;
use bytes::Bytes;
use futures::StreamExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt as McpServiceExt;
use serde_json::json;
use ulid::Ulid;

use crate::capability;
use crate::contract::MeshCommand;
use crate::contract::{Command, CommandResult, Hit, Request, Response, StatusInfo};

/// The service's addressable name/subject root. Consumers reach it by THIS,
/// never by host:port. Endpoints hang off it: `<SUBJECT>.recall`, `.status`.
pub const SERVICE_SUBJECT: &str = "mu.svc.code_index";
pub const SERVICE_NAME: &str = "code_index";

/// Which backend the service serves from.
pub enum Backend {
    /// Deterministic fixture data — for tests that don't need a live index.
    Stub,
    /// Forward to the real `code_index` MCP server at `url` (e.g.
    /// `http://host:7622/mcp`). The service becomes a protocol adapter: NATS
    /// in, code_index MCP out, typed reply back.
    CodeIndexMcp { url: String },
}

/// The live backend the handlers hold. `CodeIndex` keeps one MCP client to the
/// code_index server open for the service's lifetime (connected once at start).
#[derive(Clone)]
enum BackendState {
    Stub,
    CodeIndex(Arc<RunningService<RoleClient, ()>>),
}

/// Start the code_index service on `client`, backed by `backend`. Capabilities
/// are verified against `issuer`. Returns once endpoints are subscribed;
/// handlers run on spawned tasks for the connection's lifetime.
pub async fn start(
    client: async_nats::Client,
    issuer: PublicKey,
    backend: Backend,
) -> Result<async_nats::service::Service> {
    // Connect the backend ONCE (not per request). For CodeIndexMcp this opens
    // an MCP client to the running code_index server.
    let backend_state = match backend {
        Backend::Stub => BackendState::Stub,
        Backend::CodeIndexMcp { url } => {
            let cx = ()
                .serve(StreamableHttpClientTransport::from_uri(url.clone()))
                .await
                .map_err(|e| anyhow::anyhow!("connect code_index MCP at {url}: {e}"))?;
            BackendState::CodeIndex(Arc::new(cx))
        }
    };

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
        let backend = backend_state.clone();
        tokio::spawn(async move {
            while let Some(req) = endpoint.next().await {
                let reply = serve(&req.message.payload, &issuer, &backend).await;
                // A reply that cannot be sent leaves the caller waiting out
                // its timeout — make that observable, never silent.
                if let Err(e) = req.respond(Ok(reply)).await {
                    eprintln!("code_index service: failed to send reply: {e}");
                }
            }
        });
    }
    // `subscribe()` returns before the SUB reaches the server; without this
    // flush a caller that requests immediately after `start()` returns can
    // race the registration and get a spurious no-responders.
    client
        .flush()
        .await
        .map_err(|e| anyhow::anyhow!("flush after start: {e}"))?;
    // The caller MUST keep this Service handle alive: dropping it tears down
    // the endpoints. Returned so the owner outlives the calls.
    Ok(service)
}

/// Decode → authorize → run → encode. Every failure path produces a typed
/// `Error` response (never a panic, never a silent drop).
async fn serve(payload: &[u8], issuer: &PublicKey, backend: &BackendState) -> Bytes {
    let request: Request = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => return encode_err(Ulid::nil(), format!("malformed request: {e}")),
    };

    // Capability gate (N12): the request must carry a grant, signed by the
    // issuer, that authorizes exactly this command. No grant → no work. This
    // runs BEFORE the backend is touched.
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

    let result = run_command(&request.command, backend).await;
    encode(&Response {
        id: request.id,
        result,
    })
}

/// Dispatch the authorized command to the configured backend.
async fn run_command(command: &Command, backend: &BackendState) -> CommandResult {
    match backend {
        BackendState::Stub => stub_command(command),
        BackendState::CodeIndex(cx) => code_index_command(command, cx)
            .await
            .unwrap_or_else(|e| CommandResult::Error(format!("code_index backend: {e}"))),
    }
}

/// Deterministic fixture backend (tests).
fn stub_command(command: &Command) -> CommandResult {
    match command {
        Command::CodeRecall { query, limit, .. } => {
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

/// Bound on one backend (code_index MCP) round-trip. Without it a stalled
/// backend hangs the handler task; NATS peers time out but server tasks
/// accumulate (review 2026-07-21). Generous: covers cold-embedder reloads.
const BACKEND_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Await `fut` under [`BACKEND_CALL_TIMEOUT`], turning a timeout into a typed
/// error instead of an indefinite hang.
async fn bounded<T, E: std::fmt::Display>(
    what: &str,
    fut: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    match tokio::time::timeout(BACKEND_CALL_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(anyhow::anyhow!("{what}: {e}")),
        Err(_) => Err(anyhow::anyhow!(
            "{what}: backend timed out after {}s",
            BACKEND_CALL_TIMEOUT.as_secs()
        )),
    }
}

/// Protocol adapter: forward the command to the real code_index MCP server and
/// map its answer into the typed contract.
async fn code_index_command(
    command: &Command,
    cx: &RunningService<RoleClient, ()>,
) -> Result<CommandResult> {
    match command {
        Command::CodeRecall { query, limit, db } => {
            let mut args = serde_json::Map::new();
            args.insert("query".into(), json!(query));
            if let Some(l) = limit {
                args.insert("limit".into(), json!(l));
            }
            if let Some(db) = db {
                args.insert("db".into(), json!(db));
            }
            let res = bounded(
                "code_recall call",
                cx.call_tool(CallToolRequestParams::new("code_recall").with_arguments(args)),
            )
            .await?;
            let text = res
                .content
                .iter()
                .find_map(|c| c.as_text().map(|t| t.text.clone()));
            // A tool-level error must surface as a typed error, never as an
            // empty successful result (review 2026-07-20: all four seats).
            if res.is_error.unwrap_or(false) {
                anyhow::bail!(
                    "code_recall backend error: {}",
                    text.as_deref().unwrap_or("(no error text)")
                );
            }
            let Some(text) = text else {
                anyhow::bail!("code_recall returned no text content");
            };
            Ok(CommandResult::CodeRecall(hits_from_markdown(&text)?))
        }
        Command::CodeStatus => {
            let res = bounded(
                "code_status call",
                cx.call_tool(CallToolRequestParams::new("code_status")),
            )
            .await?;
            let healthy = !res.is_error.unwrap_or(false);
            // `indexed_repos` is 1/0 for the ONE index this service fronts —
            // the contract has no richer field and stays unchanged for this
            // slice (operator constraint); it is availability, not a count.
            Ok(CommandResult::CodeStatus(StatusInfo {
                indexed_repos: u32::from(healthy),
                healthy,
            }))
        }
    }
}

/// Parse code_index's markdown recall output into typed [`Hit`]s. Each result
/// is a header line: `## <symbol> (<score>) <kind> — <path>:<lines>`.
///
/// Drift guard: semantic recall over an indexed corpus always returns nearest
/// neighbors, so a NON-empty body that parses to zero hits is surfaced as a
/// typed error, never a silent empty result. The backend's own prose (e.g.
/// "No results found. Has the repository been indexed?" for an empty/missing
/// `db` target) passes through VERBATIM — it is the useful message; calling it
/// "format drift" was misdiagnosis (review warned this was brittle; terrain
/// confirmed it on a `db` targeting an unindexed name, 2026-07-21).
fn hits_from_markdown(markdown: &str) -> anyhow::Result<Vec<Hit>> {
    let hits: Vec<Hit> = markdown.lines().filter_map(parse_hit_line).collect();
    if hits.is_empty() && !markdown.trim().is_empty() {
        let preview: String = markdown.chars().take(200).collect();
        // No prefix: the enclosing layers already attribute (run_command adds
        // "code_index backend:"), and the backend prose stands on its own.
        anyhow::bail!("{preview}");
    }
    Ok(hits)
}

fn parse_hit_line(line: &str) -> Option<Hit> {
    let rest = line.strip_prefix("## ")?;
    let (symbol, after) = rest.split_once(" (")?; // "sym" , "0.016) Kind — path:lines"
    let (score_str, after) = after.split_once(')')?; // "0.016" , " Kind — path:lines"
    let score = score_str.trim().parse::<f32>().ok()?;
    // The path follows the em-dash; take up to the ":line-span".
    let tail = after.split_once("— ").map(|(_, p)| p).unwrap_or(after);
    let path = tail.split(':').next().unwrap_or(tail).trim().to_string();
    if path.is_empty() {
        return None;
    }
    Some(Hit {
        symbol: symbol.trim().to_string(),
        path,
        score,
    })
}

fn encode(response: &Response) -> Bytes {
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

#[cfg(test)]
mod tests {
    use super::hits_from_markdown;

    #[test]
    fn parses_real_recall_header_lines() {
        let md = "## TranscriptBlock (0.031) Class — ./crates/mu-solo/src/transcript.rs:19-35\n\
                  \n```\nstruct body elided\n```\n\n---\n\n\
                  ## render_cc_transcript (0.016) Method — ./crates/mu-coding/src/console/views.rs:1-4\n";
        let hits = hits_from_markdown(md).expect("parse");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].symbol, "TranscriptBlock");
        assert_eq!(hits[0].path, "./crates/mu-solo/src/transcript.rs");
        assert!((hits[0].score - 0.031).abs() < 1e-6);
    }

    #[test]
    fn empty_body_is_legitimately_zero_hits() {
        assert!(hits_from_markdown("").expect("empty ok").is_empty());
        assert!(hits_from_markdown("  \n ").expect("ws ok").is_empty());
    }

    #[test]
    fn format_drift_is_an_error_not_silent_empty() {
        let err = hits_from_markdown("Results:\n* TranscriptBlock at transcript.rs (0.03)\n")
            .expect_err("drifted format must error");
        assert!(err.to_string().contains("TranscriptBlock at"), "{err}");
    }
}
