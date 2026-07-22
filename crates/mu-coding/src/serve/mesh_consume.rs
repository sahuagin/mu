//! mu-a0l6: mu CONSUMES its first mesh service — `code_index` as session
//! tools over NATS request/reply, subject-addressed (`mu.svc.code_index.*`),
//! replacing the `[[mcp.servers]]` HTTP import when enabled.
//!
//! The daemon-side counterpart of the mesh design's client rule (spec
//! `specs/architecture/mesh-nats-adoption.md`): mu makes the SAME
//! `code_recall`/`code_status` calls it makes today; the proxy here
//! interprets them into typed envelopes + per-request biscuit capabilities.
//! The caller never sees the bus.
//!
//! Wire types mirror the mesh contract (`mesh-slice/src/contract.rs`)
//! byte-for-byte on the wire — the golden test below pins the JSON shape.
//! Promotion to a shared contract crate is mu-kc9v; until then this is the
//! consumer-side copy, kept deliberately minimal.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::Engine as _;
use biscuit_auth::macros::biscuit;
use biscuit_auth::{KeyPair, PrivateKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{Tool, ToolPolicy, ToolResult, ToolSpec};
use mu_core::config::MeshConfig;

/// Subject root the mesh `code_index` service listens on (endpoints:
/// `.recall`, `.status`) — must match the service side.
const SERVICE_SUBJECT: &str = "mu.svc.code_index";
/// One request/reply round-trip bound. Generous: covers the backend's
/// cold-embedder reload behind the service.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// ── wire types (consumer-side mirror of the mesh contract) ────────────────

#[derive(Serialize)]
struct Envelope<'a> {
    id: String,
    /// Base64 of the biscuit token bytes (the contract's `capability`
    /// field serializes `Vec<u8>` as base64).
    capability: String,
    command: Command<'a>,
}

#[derive(Serialize)]
enum Command<'a> {
    CodeRecall {
        query: &'a str,
        limit: Option<u32>,
        /// Target index: serving-side name or absolute path; None = the
        /// service's default. Restores the repo-targeting the direct MCP
        /// tool always had (operator regression report, 2026-07-21).
        #[serde(skip_serializing_if = "Option::is_none")]
        db: Option<&'a str>,
    },
    CodeStatus,
}

impl Command<'_> {
    fn required_right(&self) -> &'static str {
        match self {
            Command::CodeRecall { .. } => "code_recall",
            Command::CodeStatus => "code_status",
        }
    }
    fn endpoint(&self) -> &'static str {
        match self {
            Command::CodeRecall { .. } => "recall",
            Command::CodeStatus => "status",
        }
    }
}

#[derive(Deserialize)]
struct Reply {
    id: String,
    result: CommandResult,
}

#[derive(Debug, Deserialize)]
enum CommandResult {
    CodeRecall(Vec<Hit>),
    CodeStatus(StatusInfo),
    Error(String),
}

#[derive(Debug, Deserialize)]
struct Hit {
    symbol: String,
    path: String,
    score: f32,
}

#[derive(Debug, Deserialize)]
struct StatusInfo {
    indexed_repos: u32,
    healthy: bool,
}

// ── proxy ─────────────────────────────────────────────────────────────────

/// mu's handle to the mesh `code_index` service: a NATS client + the key it
/// mints request capabilities with. No host:port — the service is reached by
/// subject.
struct CodeIndexProxy {
    client: async_nats::Client,
    root: KeyPair,
}

impl CodeIndexProxy {
    /// Mint the command's capability, envelope it, request/reply on the
    /// command's endpoint subject, decode + correlate the typed reply.
    async fn call(&self, command: Command<'_>) -> Result<CommandResult> {
        let token = biscuit!(r#"right({r});"#, r = command.required_right())
            .build(&self.root)
            .map_err(|e| anyhow!("mint capability: {e}"))?
            .to_vec()
            .map_err(|e| anyhow!("encode capability: {e}"))?;
        let subject = format!("{SERVICE_SUBJECT}.{}", command.endpoint());
        let env = Envelope {
            id: ulid::Ulid::generate().to_string(),
            capability: base64::engine::general_purpose::STANDARD.encode(token),
            command,
        };
        let payload = serde_json::to_vec(&env)?;
        let reply = self
            .client
            .request(subject, payload.into())
            .await
            .map_err(|e| anyhow!("mesh request failed: {e}"))?;
        let reply: Reply = serde_json::from_slice(&reply.payload)
            .map_err(|e| anyhow!("malformed mesh reply: {e}"))?;
        if reply.id != env.id {
            return Err(anyhow!(
                "correlation mismatch: sent {}, got {}",
                env.id,
                reply.id
            ));
        }
        Ok(reply.result)
    }
}

// ── session tools ─────────────────────────────────────────────────────────

/// One mesh-backed session tool (`code_recall` or `code_status`).
struct MeshCodeIndexTool {
    proxy: Arc<CodeIndexProxy>,
    spec: ToolSpec,
    recall: bool,
}

#[async_trait]
impl Tool for MeshCodeIndexTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, arguments: Value, mut cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let call = async {
            let result = if self.recall {
                let query = arguments.get("query").and_then(Value::as_str).unwrap_or("");
                if query.is_empty() {
                    return ToolResult {
                        content: "code_recall requires a non-empty `query`".to_string(),
                        is_error: true,
                    };
                }
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    // Saturate rather than wrap: a limit beyond u32::MAX is
                    // absurd input, but wrapping would silently shrink it.
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
                let db = arguments.get("db").and_then(Value::as_str);
                self.proxy
                    .call(Command::CodeRecall { query, limit, db })
                    .await
            } else {
                self.proxy.call(Command::CodeStatus).await
            };
            match result {
                Ok(CommandResult::CodeRecall(hits)) => ToolResult {
                    content: render_hits(&hits),
                    is_error: false,
                },
                Ok(CommandResult::CodeStatus(s)) => ToolResult {
                    content: format!("healthy={} indexed_repos={}", s.healthy, s.indexed_repos),
                    is_error: false,
                },
                Ok(CommandResult::Error(e)) => ToolResult {
                    content: format!("mesh code_index error: {e}"),
                    is_error: true,
                },
                Err(e) => ToolResult {
                    content: format!("mesh code_index unavailable: {e}"),
                    is_error: true,
                },
            }
        };
        tokio::select! {
            biased;
            _ = &mut cancel_rx => ToolResult {
                content: format!("{} cancelled", self.spec.name),
                is_error: true,
            },
            r = tokio::time::timeout(REQUEST_TIMEOUT, call) => match r {
                Ok(result) => result,
                Err(_) => ToolResult {
                    content: format!(
                        "{}: mesh request timed out after {}s",
                        self.spec.name,
                        REQUEST_TIMEOUT.as_secs()
                    ),
                    is_error: true,
                },
            },
        }
    }
}

/// Model-facing rendering: one hit per line, score/symbol/path — compact and
/// grep-able, mirroring what the agent needs from recall.
fn render_hits(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no hits".to_string();
    }
    hits.iter()
        .map(|h| format!("{:.3}  {}  ({})", h.score, h.symbol, h.path))
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_only(spec: ToolSpec) -> ToolSpec {
    // The CANONICAL read-only policy (ReadOnly + Allow). Spreading
    // `..ToolPolicy::default()` here inherited the fail-closed default's
    // `Ask` and prompted the operator on EVERY recall (caught live,
    // 2026-07-21) — exactly the omission the default is designed to punish.
    spec.with_policy(ToolPolicy::read_only())
}

/// Build the mesh-backed `code_recall` + `code_status` session tools:
/// bounded NATS connect (this runs on daemon startup — same fail-fast rule
/// as the mesh adapter), one shared proxy, two tools. Best-effort caller:
/// an error here degrades to "no mesh tools", never a startup failure.
pub(crate) async fn mesh_code_index_tools(mesh: &MeshConfig) -> Result<Vec<Arc<dyn Tool>>> {
    if mesh.issuer_key.is_empty() {
        return Err(anyhow!(
            "[mesh].consume_code_index requires [mesh].issuer_key (hex Ed25519); \
             mesh services accept no anonymous requests"
        ));
    }
    let root = KeyPair::from(
        &PrivateKey::from_bytes_hex(&mesh.issuer_key, biscuit_auth::builder::Algorithm::Ed25519)
            .map_err(|e| anyhow!("[mesh].issuer_key is not a valid hex Ed25519 key: {e}"))?,
    );
    let client = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async_nats::ConnectOptions::new()
            .request_timeout(Some(REQUEST_TIMEOUT))
            .connect(&mesh.nats_url),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "mesh consume: connect NATS at {}: timed out after 5s",
            mesh.nats_url
        )
    })?
    .map_err(|e| anyhow!("mesh consume: connect NATS at {}: {e}", mesh.nats_url))?;

    let proxy = Arc::new(CodeIndexProxy { client, root });
    let recall_spec = read_only(ToolSpec {
        name: "code_recall".to_string(),
        description: "Hybrid symbol/concept code recall over the indexed codebase \
                      (served by the mesh code_index service). Preferred over grep \
                      for finding symbols, types, functions, or patterns."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Natural language query or symbol name"},
                "limit": {"type": "number", "description": "Max results (default service-side)"},
                "db": {"type": "string", "description": "Target index: a repo name (e.g. \"mu\") or absolute path. Omit for the default index."}
            },
            "required": ["query"]
        }),
        ..ToolSpec::default()
    });
    let status_spec = read_only(ToolSpec {
        name: "code_status".to_string(),
        description: "Health of the mesh code_index service (availability of the \
                      code index behind code_recall)."
            .to_string(),
        input_schema: json!({"type": "object", "properties": {}}),
        ..ToolSpec::default()
    });
    Ok(vec![
        Arc::new(MeshCodeIndexTool {
            proxy: proxy.clone(),
            spec: recall_spec,
            recall: true,
        }),
        Arc::new(MeshCodeIndexTool {
            proxy,
            spec: status_spec,
            recall: false,
        }),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GOLDEN: the envelope this consumer emits must match the mesh
    /// contract's serde shape exactly (externally-tagged command enum,
    /// base64 capability string, ulid string id). If this test moves, the
    /// deployed services stop understanding mu — change both sides or
    /// neither (promotion to a shared crate: mu-kc9v).
    #[test]
    fn envelope_wire_shape_matches_the_mesh_contract() {
        let env = Envelope {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            capability: "AAEC".to_string(), // base64 of [0,1,2]
            command: Command::CodeRecall {
                query: "where are sessions built",
                limit: Some(2),
                db: None,
            },
        };
        let v = serde_json::to_value(&env).expect("serialize");
        assert_eq!(
            v,
            json!({
                "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "capability": "AAEC",
                // `db` absent when None — old services never see the field.
                "command": {"CodeRecall": {"query": "where are sessions built", "limit": 2}}
            })
        );
        let with_db = serde_json::to_value(Command::CodeRecall {
            query: "q",
            limit: None,
            db: Some("mu"),
        })
        .expect("serialize");
        assert_eq!(
            with_db,
            json!({"CodeRecall": {"query": "q", "limit": null, "db": "mu"}})
        );
        let status = serde_json::to_value(Command::CodeStatus).expect("serialize");
        assert_eq!(status, json!("CodeStatus"));
    }

    /// GOLDEN, reply direction: the service's typed results decode.
    #[test]
    fn reply_wire_shape_matches_the_mesh_contract() {
        let reply: Reply = serde_json::from_value(json!({
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "result": {"CodeRecall": [
                {"symbol": "TranscriptBlock", "path": "./crates/mu-solo/src/transcript.rs", "score": 0.031}
            ]}
        }))
        .expect("decode recall reply");
        match reply.result {
            CommandResult::CodeRecall(hits) => {
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].symbol, "TranscriptBlock");
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let err: Reply = serde_json::from_value(json!({
            "id": "x", "result": {"Error": "unauthorized"}
        }))
        .expect("decode error reply");
        assert!(matches!(err.result, CommandResult::Error(_)));
    }

    /// LIVE (skips without nats-server): the full consume path — tool
    /// execute → minted biscuit capability VERIFIED by the service side →
    /// typed reply → rendered hits. Proves mu's mint interoperates with the
    /// mesh's verify, not just the JSON shapes.
    #[tokio::test]
    async fn consumed_tool_round_trips_with_capability_verification() {
        use futures::StreamExt;

        let bin = std::env::var("NATS_BIN").unwrap_or_else(|_| "nats-server".to_string());
        let store = "target/nats-js-14530";
        let _ = std::fs::remove_dir_all(store);
        let Ok(mut nats) = std::process::Command::new(&bin)
            .args(["-p", "14530", "-js", "-sd", store, "-a", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            eprintln!("skipping: cannot spawn nats-server ({bin})");
            return;
        };
        for _ in 0..100 {
            if tokio::net::TcpStream::connect("127.0.0.1:14530")
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let root = KeyPair::new();
        let issuer = root.public();
        let issuer_hex = root.private().to_bytes_hex();

        // Minimal service side: decode, VERIFY the capability against the
        // issuer (signature + right, no fail-open), reply typed hits.
        let svc = async_nats::connect("127.0.0.1:14530")
            .await
            .expect("svc connect");
        let mut sub = svc
            .subscribe(format!("{SERVICE_SUBJECT}.recall"))
            .await
            .expect("svc subscribe");
        svc.flush().await.expect("svc flush");
        let svc_task = tokio::spawn(async move {
            while let Some(msg) = sub.next().await {
                #[derive(Deserialize)]
                struct InEnv {
                    id: String,
                    capability: String,
                }
                let env: InEnv = serde_json::from_slice(&msg.payload).expect("decode env");
                let token = base64::engine::general_purpose::STANDARD
                    .decode(&env.capability)
                    .expect("b64");
                let authorized = biscuit_auth::Biscuit::from(&token, issuer)
                    .ok()
                    .and_then(|t| {
                        biscuit_auth::macros::authorizer!(r#"allow if right("code_recall");"#)
                            .build(&t)
                            .ok()
                    })
                    .map(|mut a| a.authorize().is_ok())
                    .unwrap_or(false);
                let result = if authorized {
                    json!({"CodeRecall": [
                        {"symbol": "real::hit", "path": "src/real.rs", "score": 0.9}
                    ]})
                } else {
                    json!({"Error": "unauthorized"})
                };
                let reply = serde_json::to_vec(&json!({"id": env.id, "result": result})).unwrap();
                if let Some(rt) = msg.reply {
                    svc.publish(rt, reply.into()).await.ok();
                }
            }
        });

        let mesh = MeshConfig {
            nats_url: "127.0.0.1:14530".to_string(),
            consume_code_index: true,
            issuer_key: issuer_hex,
            ..MeshConfig::default()
        };
        let tools = mesh_code_index_tools(&mesh).await.expect("build tools");
        let recall = tools
            .iter()
            .find(|t| t.spec().name == "code_recall")
            .expect("code_recall tool");

        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let result = recall
            .execute(json!({"query": "anything", "limit": 1}), cancel_rx)
            .await;
        assert!(
            !result.is_error,
            "live round-trip failed: {}",
            result.content
        );
        assert!(
            result.content.contains("real::hit") && result.content.contains("src/real.rs"),
            "rendered hits expected, got: {}",
            result.content
        );

        svc_task.abort();
        nats.kill().ok();
        nats.wait().ok();
    }

    #[test]
    fn hits_render_compact_lines() {
        let out = render_hits(&[Hit {
            symbol: "a::b".into(),
            path: "src/x.rs".into(),
            score: 0.5,
        }]);
        assert_eq!(out, "0.500  a::b  (src/x.rs)");
        assert_eq!(render_hits(&[]), "no hits");
    }
}
