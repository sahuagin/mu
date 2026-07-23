//! mu-z0zc: dialogue over the mesh, increment 1 — the daemon joins the mesh
//! as an AGENT: presence (NATS Micro registration; who-is-available is `$SRV`
//! discovery, no roster), a DM inbox subject, and `dm`/`who` session tools.
//!
//! This is the use-case PR #492 attempted on the wrong substrate (MCP push),
//! done natively on the mesh seams (mu#493/#494/#495). Inbound DMs are
//! **per-message capability-verified** (an unauthorized DM is dropped, never
//! delivered) and land in the target session's DURABLE mailbox via the exact
//! delivery mechanics of `mailbox.post` (mu-slat: the post IS the wake — the
//! agent loop receives `AgentInput::MailboxMessage`, no polling anywhere).
//! Because every envelope carries its own biscuit grant, dialogue is safe on
//! daemons with enforcing auth — unlike the RPC mesh adapter, there is no
//! shared connection auth to bypass (mu-iqo8 applies there, not here).
//!
//! Wire types mirror the mesh contract (`mesh-slice/src/agent.rs`); golden
//! tests pin the JSON. Teams/multicast are increment 2.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::Engine as _;
use biscuit_auth::macros::{authorizer, biscuit};
use biscuit_auth::{Biscuit, KeyPair, PrivateKey, PublicKey};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use mu_core::agent::{AgentInput, SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec};
use mu_core::config::MeshConfig;
use mu_core::event_log::{EventActor, EventPayload};
use mu_core::transport::Router;

use super::sessions::Sessions;

/// Mesh agent presence prefix — an agent `x` registers the Micro service
/// `agent_x`, and `who` strips this prefix from `$SRV` responders. Must match
/// the mesh contract (`mesh-slice/src/agent.rs`).
const PRESENCE_PREFIX: &str = "agent_";
/// The right a DM capability must grant (mesh contract).
const DM_RIGHT: &str = "agent_dm";
/// Message kind recorded on mailbox events for mesh DMs.
const DM_KIND: &str = "mesh.dm";
/// Presence collection window for `who` (mirrors the mesh contract's agent).
const WHO_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

fn dm_subject(agent: &str) -> String {
    format!("mu.agent.{agent}.dm")
}

// ── wire types (mirror of mesh-slice/src/agent.rs AgentCommand) ───────────

#[derive(Serialize, Deserialize)]
struct DmEnvelope {
    id: String,
    /// Base64 biscuit token bytes (the contract's `capability` encoding).
    capability: String,
    command: AgentCommand,
}

#[derive(Serialize, Deserialize)]
enum AgentCommand {
    Dm {
        from: String,
        body: String,
        /// Target session on the receiving daemon. Absent ⇒ the well-known
        /// `supervisor` session. Additive vs the slice contract (older
        /// peers neither send nor need it; serde ignores unknowns).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
}

// ── delivery (the mailbox.post mechanics, mesh-side) ──────────────────────

/// Deliver a verified DM into `target`'s durable mailbox: allocate seq,
/// append `MailboxMessagePosted`, broadcast the wire notification through the
/// Router (`origin: None` ⇒ every connection), and inject
/// `AgentInput::MailboxMessage` so a live agent loop wakes (mu-slat: the post
/// IS the trigger). Mirrors `handle_mailbox_post`'s tail exactly — one
/// durable path for session-peer posts and mesh DMs alike.
async fn deliver_dm(
    sessions: &Sessions,
    router: &Router,
    target: &str,
    from_agent: &str,
    body: &str,
) -> Result<()> {
    let mailbox = sessions
        .mailbox(target)
        .ok_or_else(|| anyhow!("target session not found: {target}"))?;
    let log = sessions
        .event_log(target)
        .ok_or_else(|| anyhow!("target session has no event log: {target}"))?;

    let subject = format!("mesh dm from {from_agent}");
    let seq = mailbox.allocate_seq();
    let posted_event_id = log.append(
        EventActor::System,
        EventPayload::MailboxMessagePosted {
            seq,
            from_daemon_id: from_agent.to_string(),
            from_session_id: "mesh".to_string(),
            message_kind: DM_KIND.to_string(),
            subject: subject.clone(),
            body: Value::String(body.to_string()),
            expires_at_unix_ms: None,
        },
    );
    let posted_at_unix_ms = log
        .snapshot()
        .iter()
        .find(|e| e.id == posted_event_id)
        .map(|e| e.timestamp_unix_ms)
        .unwrap_or(0);

    // Broadcast wire notification (every connected client's lane) — the
    // same event `mailbox.post` emits, via the same writer type.
    let _ = mu_core::transport::NotificationWriter::broadcast(router.clone())
        .emit(
            mu_core::protocol::MailboxMessageEvent::METHOD,
            mu_core::protocol::MailboxMessageEvent {
                session_id: target.to_string(),
                seq,
                from_daemon_id: from_agent.to_string(),
                from_session_id: "mesh".to_string(),
                kind: DM_KIND.to_string(),
                subject: subject.clone(),
                body: Value::String(body.to_string()),
                posted_at_unix_ms,
                expires_at_unix_ms: None,
            },
        )
        .await;

    // mu-slat: the post IS the wake — a live agent loop synthesizes a
    // UserMessage from the mailbox entry (looked up by seq) and runs.
    if let Some(tx) = sessions.input_sender(target) {
        let _ = tx.try_send(AgentInput::MailboxMessage {
            from_session_id: "mesh".to_string(),
            message_kind: DM_KIND.to_string(),
            subject,
            seq,
        });
    }
    Ok(())
}

/// Verify an inbound envelope's capability against `issuer` for [`DM_RIGHT`].
/// Any failure — bad base64, bad signature, missing right — is `false`; there
/// is no fail-open path (mesh contract N12).
fn dm_authorized(capability_b64: &str, issuer: PublicKey) -> bool {
    let Ok(token) = base64::engine::general_purpose::STANDARD.decode(capability_b64) else {
        return false;
    };
    let Ok(token) = Biscuit::from(&token, issuer) else {
        return false;
    };
    let Ok(mut authz) = authorizer!(r#"allow if right({r});"#, r = DM_RIGHT).build(&token) else {
        return false;
    };
    authz.authorize().is_ok()
}

// ── session tools ─────────────────────────────────────────────────────────

struct MeshDialogueTools {
    client: async_nats::Client,
    root: KeyPair,
    agent_id: String,
}

/// `who`: live mesh agents via `$SRV.PING` — no roster, presence is the
/// registration itself.
struct WhoTool {
    shared: Arc<MeshDialogueTools>,
    spec: ToolSpec,
}

/// `dm`: fire-and-forget directed message to another mesh agent, capability
/// minted per send.
struct DmTool {
    shared: Arc<MeshDialogueTools>,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for WhoTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, _arguments: Value, mut cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let run = async {
            match who(&self.shared.client).await {
                Ok(agents) if agents.is_empty() => ToolResult {
                    content: "no agents present on the mesh".to_string(),
                    is_error: false,
                },
                Ok(agents) => ToolResult {
                    content: agents.join("\n"),
                    is_error: false,
                },
                Err(e) => ToolResult {
                    content: format!("who failed: {e}"),
                    is_error: true,
                },
            }
        };
        tokio::select! {
            biased;
            _ = &mut cancel_rx => ToolResult { content: "who cancelled".into(), is_error: true },
            r = run => r,
        }
    }
}

#[async_trait]
impl Tool for DmTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, arguments: Value, mut cancel_rx: oneshot::Receiver<()>) -> ToolResult {
        let to = arguments.get("to").and_then(Value::as_str).unwrap_or("");
        let body = arguments.get("body").and_then(Value::as_str).unwrap_or("");
        if to.is_empty() || body.is_empty() {
            return ToolResult {
                content: "dm requires non-empty `to` and `body`".to_string(),
                is_error: true,
            };
        }
        let session = arguments
            .get("session")
            .and_then(Value::as_str)
            .map(str::to_string);
        let run = async {
            match send_dm(&self.shared, to, body, session).await {
                Ok(()) => ToolResult {
                    content: format!("dm sent to {to}"),
                    is_error: false,
                },
                Err(e) => ToolResult {
                    content: format!("dm to {to} failed: {e}"),
                    is_error: true,
                },
            }
        };
        tokio::select! {
            biased;
            _ = &mut cancel_rx => ToolResult { content: "dm cancelled".into(), is_error: true },
            r = run => r,
        }
    }
}

async fn who(client: &async_nats::Client) -> Result<Vec<String>> {
    use futures::StreamExt;
    let inbox = client.new_inbox();
    let mut sub = client
        .subscribe(inbox.clone())
        .await
        .map_err(|e| anyhow!("who inbox: {e}"))?;
    client
        .publish_with_reply("$SRV.PING".to_string(), inbox, Bytes::new())
        .await
        .map_err(|e| anyhow!("who ping: {e}"))?;
    client
        .flush()
        .await
        .map_err(|e| anyhow!("who flush: {e}"))?;

    let mut agents = Vec::new();
    let deadline = tokio::time::Instant::now() + WHO_WINDOW;
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, sub.next()).await {
        if let Ok(v) = serde_json::from_slice::<Value>(&msg.payload) {
            if let Some(name) = v.get("name").and_then(Value::as_str) {
                if let Some(agent) = name.strip_prefix(PRESENCE_PREFIX) {
                    agents.push(agent.to_string());
                }
            }
        }
    }
    agents.sort();
    agents.dedup();
    Ok(agents)
}

async fn send_dm(
    shared: &MeshDialogueTools,
    to: &str,
    body: &str,
    session: Option<String>,
) -> Result<()> {
    let token = biscuit!(r#"right({r});"#, r = DM_RIGHT)
        .build(&shared.root)
        .map_err(|e| anyhow!("mint capability: {e}"))?
        .to_vec()
        .map_err(|e| anyhow!("encode capability: {e}"))?;
    let env = DmEnvelope {
        id: ulid::Ulid::generate().to_string(),
        capability: base64::engine::general_purpose::STANDARD.encode(token),
        command: AgentCommand::Dm {
            from: shared.agent_id.clone(),
            body: body.to_string(),
            session,
        },
    };
    let payload = serde_json::to_vec(&env)?;
    shared
        .client
        .publish(dm_subject(to), payload.into())
        .await
        .map_err(|e| anyhow!("publish: {e}"))?;
    shared
        .client
        .flush()
        .await
        .map_err(|e| anyhow!("flush: {e}"))?;
    Ok(())
}

// ── startup ───────────────────────────────────────────────────────────────

/// Everything mesh-dialogue: aborting the tasks on drop releases the DM
/// subscription and presence registration (dropping the Micro `Service`
/// deregisters it) — same lifetime contract as the other mesh guards.
pub(crate) struct MeshDialogueHandle {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Presence registration lives exactly as long as this handle.
    _presence: async_nats::service::Service,
}

impl Drop for MeshDialogueHandle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Join the mesh as agent `<daemon_id>`: register presence, subscribe the DM
/// subject (verified inbound → durable mailbox), and return the `dm`/`who`
/// session tools. Bounded setup — this runs on the daemon startup path.
pub(crate) async fn spawn_mesh_dialogue(
    mesh: &MeshConfig,
    daemon_id: &str,
    sessions: Sessions,
    router: Router,
) -> Result<(MeshDialogueHandle, Vec<Arc<dyn Tool>>)> {
    use async_nats::service::ServiceExt as _;
    use futures::StreamExt;

    if mesh.issuer_key.is_empty() {
        return Err(anyhow!(
            "[mesh].dialogue requires [mesh].issuer_key (hex Ed25519): DMs are \
             capability-verified per message; without a key nothing could be \
             sent or accepted"
        ));
    }
    let root = KeyPair::from(
        &PrivateKey::from_bytes_hex(&mesh.issuer_key, biscuit_auth::builder::Algorithm::Ed25519)
            .map_err(|e| anyhow!("[mesh].issuer_key is not a valid hex Ed25519 key: {e}"))?,
    );
    let issuer = root.public();

    let (client, presence, sub) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let client = async_nats::connect(&mesh.nats_url)
            .await
            .map_err(|e| anyhow!("dialogue: connect NATS at {}: {e}", mesh.nats_url))?;
        let presence = client
            .service_builder()
            .description("mu daemon (mesh dialogue presence)")
            .start(format!("{PRESENCE_PREFIX}{daemon_id}"), "0.1.0")
            .await
            .map_err(|e| anyhow!("dialogue: presence register: {e}"))?;
        let sub = client
            .subscribe(dm_subject(daemon_id))
            .await
            .map_err(|e| anyhow!("dialogue: dm subscribe: {e}"))?;
        client
            .flush()
            .await
            .map_err(|e| anyhow!("dialogue: flush: {e}"))?;
        Ok::<_, anyhow::Error>((client, presence, sub))
    })
    .await
    .map_err(|_| {
        anyhow!(
            "dialogue: NATS setup at {} timed out after 5s",
            mesh.nats_url
        )
    })??;

    // Inbound DM loop: decode → verify capability → deliver to the mailbox.
    // Unauthorized or malformed messages are dropped (logged, never
    // delivered) — the mesh contract's no-fail-open rule.
    let inbound_sessions = sessions;
    let inbound_router = router;
    let mut sub = sub;
    let inbound_task = tokio::spawn(async move {
        while let Some(msg) = sub.next().await {
            let Ok(env) = serde_json::from_slice::<DmEnvelope>(&msg.payload) else {
                tracing::debug!("mesh dialogue: dropping malformed dm envelope");
                continue;
            };
            if !dm_authorized(&env.capability, issuer) {
                tracing::warn!("mesh dialogue: dropping unauthorized dm");
                continue;
            }
            let AgentCommand::Dm {
                from,
                body,
                session,
            } = env.command;
            let target = session.as_deref().unwrap_or("supervisor");
            match deliver_dm(&inbound_sessions, &inbound_router, target, &from, &body).await {
                Ok(()) => tracing::info!(%from, %target, "mesh dialogue: dm delivered"),
                Err(e) => tracing::warn!(%from, %target, error = %e,
                    "mesh dialogue: dm delivery failed"),
            }
        }
    });

    let shared = Arc::new(MeshDialogueTools {
        client,
        root,
        agent_id: daemon_id.to_string(),
    });
    let who_spec = ToolSpec {
        name: "who".to_string(),
        description: "List agents currently present on the mesh (live NATS Micro \
                      discovery — an agent is listed iff it is up right now)."
            .to_string(),
        input_schema: json!({"type": "object", "properties": {}}),
        ..ToolSpec::default()
    }
    .with_policy(ToolPolicy::read_only());
    let dm_spec = ToolSpec {
        name: "dm".to_string(),
        description: "Send a directed message to another mesh agent. Fire-and-forget: \
                      it lands durably in the peer's mailbox and wakes their agent loop. \
                      `to` is the peer's agent id (see `who`)."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "Peer agent id (a `who` entry)"},
                "body": {"type": "string", "description": "Message body"},
                "session": {"type": "string", "description": "Target session on the peer daemon (default: its supervisor session)"}
            },
            "required": ["to", "body"]
        }),
        ..ToolSpec::default()
    }
    // Deliberate policy: a dm SENDS — an honest Mutating side-effect — but
    // prompting for approval on every chat message would make dialogue
    // unusable, so permission is Allow. Not retried (a duplicate dm is a
    // duplicate message), not idempotent.
    .with_policy(ToolPolicy {
        side_effects: SideEffects::Mutating,
        permission: mu_core::agent::PermissionLevel::Allow,
        ..ToolPolicy::default()
    });

    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(WhoTool {
            shared: shared.clone(),
            spec: who_spec,
        }),
        Arc::new(DmTool {
            shared,
            spec: dm_spec,
        }),
    ];

    tracing::info!(agent = %daemon_id, nats = %mesh.nats_url,
        "mesh dialogue: joined (presence + dm inbox)");
    Ok((
        MeshDialogueHandle {
            tasks: vec![inbound_task],
            _presence: presence,
        },
        tools,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GOLDEN: the DM envelope must match the mesh contract
    /// (`mesh-slice/src/agent.rs`) — externally-tagged command, base64
    /// capability, ulid string id. `session` is additive: absent when None.
    #[test]
    fn dm_envelope_wire_shape_matches_the_mesh_contract() {
        let env = DmEnvelope {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            capability: "AAEC".to_string(),
            command: AgentCommand::Dm {
                from: "alice".to_string(),
                body: "review PR 42?".to_string(),
                session: None,
            },
        };
        let v = serde_json::to_value(&env).expect("serialize");
        assert_eq!(
            v,
            json!({
                "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "capability": "AAEC",
                "command": {"Dm": {"from": "alice", "body": "review PR 42?"}}
            })
        );
        // Slice-shaped inbound (no `session` field) decodes.
        let inbound: DmEnvelope = serde_json::from_value(json!({
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "capability": "AAEC",
            "command": {"Dm": {"from": "bob", "body": "hi"}}
        }))
        .expect("decode slice-shaped dm");
        let AgentCommand::Dm { from, session, .. } = inbound.command;
        assert_eq!(from, "bob");
        assert!(session.is_none());
    }

    /// Capability gate: mint→verify round-trips; a rogue key or wrong right
    /// is refused; garbage is refused. No fail-open.
    #[test]
    fn dm_capability_gate_has_no_fail_open() {
        let root = KeyPair::new();
        let token = biscuit!(r#"right({r});"#, r = DM_RIGHT)
            .build(&root)
            .unwrap()
            .to_vec()
            .unwrap();
        let good = base64::engine::general_purpose::STANDARD.encode(&token);
        assert!(dm_authorized(&good, root.public()));

        let rogue = KeyPair::new();
        assert!(!dm_authorized(&good, rogue.public()));

        let wrong_right = biscuit!(r#"right("code_recall");"#)
            .build(&root)
            .unwrap()
            .to_vec()
            .unwrap();
        let wrong = base64::engine::general_purpose::STANDARD.encode(&wrong_right);
        assert!(!dm_authorized(&wrong, root.public()));

        assert!(!dm_authorized("!!!not-base64!!!", root.public()));
        assert!(!dm_authorized("", root.public()));
    }
}
