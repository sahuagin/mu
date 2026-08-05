//! Mesh gateway (at-uws): mu-dialogue speaks the NATS agent mesh on behalf of
//! the MCP peers connected to it, so cc and `agent dialogue` stay unchanged.
//!
//! The gateway holds its subscriptions continuously — core NATS has no replay,
//! and a CLI that subscribes only for the length of one `poll` would drop
//! anything sent between polls. Verified inbound DMs become ordinary `dialogue`
//! rows, so the long-poll, the rowid cursor and `role:identity` peer ids are
//! untouched. The store is the buffer; there is no second one.
//!
//! Routing: one path delivers, never both. If `$SRV` says the target is on the
//! mesh, the DM goes over the mesh and the row is marked `route = 'mesh'` so
//! `dialogue_poll` will not serve it again. The check fails CLOSED and a failed
//! publish clears the mark, so the failure mode is a duplicate, not a loss.
//!
//! Opt-in via `[dialogue.mesh] enabled = true`; with it absent no NATS
//! connection is opened. Wire shape mirrors `mesh-slice/src/agent.rs` and
//! `mu-coding/src/serve/mesh_dialogue.rs`. Inbound DMs are capability-verified
//! per message; unauthorized envelopes are dropped, never delivered.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::Engine as _;
use biscuit_auth::macros::{authorizer, biscuit};
use biscuit_auth::{Biscuit, KeyPair, PrivateKey, PublicKey};
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

/// A mesh agent `x` registers the NATS Micro service `agent_x`; `$SRV`
/// discovery strips this prefix. Must match the mesh contract.
pub const PRESENCE_PREFIX: &str = "agent_";
/// The right a DM capability must grant (mesh contract).
const DM_RIGHT: &str = "agent_dm";
/// How long a `$SRV.PING` sweep collects responders.
const WHO_WINDOW: Duration = Duration::from_millis(300);
/// Bound on connect + subscribe at startup.
const NATS_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// A mesh agent's DM inbox subject.
pub fn dm_subject(agent: &str) -> String {
    format!("mu.agent.{agent}.dm")
}

// ─────────────────────────────── Config ─────────────────────────────────────

/// `[dialogue.mesh]` from the mu config, with `nats_url` / `issuer_key`
/// inherited from the fleet-wide `[mesh]` section when not set explicitly —
/// the key is already there and duplicating it invites drift.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MeshConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub nats_url: String,
    /// Hex Ed25519 private key. DMs are capability-verified per message, so
    /// without it nothing could be sent or accepted.
    #[serde(default)]
    pub issuer_key: String,
}

/// Load `[dialogue.mesh]`. Returns None (gateway disabled) when the file, the
/// section, or `enabled = true` is missing, or when no NATS url / issuer key
/// can be resolved — every "not configured" shape means "run exactly as
/// before".
pub fn load(path: &Path) -> Option<MeshConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let root: toml::Value = text.parse().ok()?;
    let mut cfg: MeshConfig = root.get("dialogue")?.get("mesh")?.clone().try_into().ok()?;
    if !cfg.enabled {
        return None;
    }
    let inherit = |field: &str| -> String {
        root.get("mesh")
            .and_then(|m| m.get(field))
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    if cfg.nats_url.is_empty() {
        cfg.nats_url = inherit("nats_url");
    }
    if cfg.issuer_key.is_empty() {
        cfg.issuer_key = inherit("issuer_key");
    }
    if cfg.nats_url.is_empty() || cfg.issuer_key.is_empty() {
        return None;
    }
    Some(cfg)
}

// ──────────────────────── Wire types (mesh contract) ────────────────────────

#[derive(Serialize, Deserialize)]
pub struct DmEnvelope {
    pub id: String,
    /// Base64 biscuit token bytes (the contract's `capability` encoding).
    pub capability: String,
    pub command: AgentCommand,
}

#[derive(Serialize, Deserialize)]
pub enum AgentCommand {
    Dm {
        from: String,
        body: String,
        /// Target session on the receiving daemon. Absent ⇒ its well-known
        /// `supervisor` session. Additive vs the slice contract.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// Sender-authored envelope line rendered in the receiver's wake.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
        /// The SENDER's session on `from`'s daemon. Without it a DM is
        /// attributed to the daemon alone and a reply lands on that daemon's
        /// supervisor session rather than the one that wrote, so the first
        /// message arrives and the conversation cannot continue.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_session: Option<String>,
    },
}

/// Verify an inbound envelope's capability against `issuer` for [`DM_RIGHT`].
/// Any failure — bad base64, bad signature, missing right — is `false`; there
/// is no fail-open path.
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

// ─────────────────────────── Peer id translation ────────────────────────────

/// Where a dialogue peer id lands on the mesh: the subject to publish to, plus
/// a session named in the envelope when the subject does not already identify
/// one. The gateway owns this translation so mesh addressing never leaks into
/// the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshTarget {
    pub subject: String,
    pub session: Option<String>,
}

/// `mu:<daemon>` → that daemon's supervisor session;
/// `mu:<daemon>:<session>` → that session.
///
/// Everything else — `cc:*` above all — is None: those peers are served by
/// this gateway's own store, and publishing them to the mesh would round-trip
/// straight back into it as a duplicate.
pub fn resolve_target(peer_id: &str) -> Option<MuPeer> {
    let rest = peer_id.strip_prefix("mu:")?;
    match rest.split_once(':') {
        Some((daemon, session)) if !daemon.is_empty() && !session.is_empty() => Some(MuPeer {
            daemon: daemon.to_string(),
            session: Some(session.to_string()),
        }),
        // `mu:` alone, `mu::x`, `mu:d:` — malformed, not addressable.
        Some(_) => None,
        None if !rest.is_empty() => Some(MuPeer {
            daemon: rest.to_string(),
            session: None,
        }),
        None => None,
    }
}

/// A `mu:` peer id parsed into the daemon that hosts it and, when present, the
/// session on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuPeer {
    pub daemon: String,
    pub session: Option<String>,
}

/// A mesh `from` (a bare daemon id) as a dialogue peer id.
///
/// The mesh contract carries no SENDER session — `AgentCommand::Dm.session`
/// names the target — so an inbound DM is attributed to the daemon. A reply
/// therefore lands on that daemon's supervisor session rather than the session
/// that wrote it. That is a gap in the contract, not in this translation.
pub fn inbound_peer_id(from: &str) -> String {
    format!("mu:{from}")
}

/// A mesh `from` plus the sender's session as a dialogue peer id.
///
/// With `from_session` the result is `mu:<daemon>:<session>`, which
/// [`resolve_target`] turns straight back into a DM addressed at that same
/// session — so a reply reaches whoever wrote. Without it the attribution
/// falls back to the daemon, which is where peers predating the field land.
pub fn inbound_peer_id_with_session(from: &str, from_session: Option<&str>) -> String {
    match from_session {
        Some(s) if !s.is_empty() => format!("mu:{from}:{s}"),
        _ => inbound_peer_id(from),
    }
}

/// A dialogue peer id as a mesh agent id.
///
/// Metadata key carrying a peer's identity, unmangled.
const META_PEER_ID: &str = "peer_id";
/// Metadata key carrying the exact subject to publish to.
const META_DM_SUBJECT: &str = "dm_subject";

/// A dialogue peer id as its DM subject: each `:` level becomes a NATS subject
/// token, so routing is the transport's own hierarchy (mu-b1lq). Must match
/// `mu-coding`'s `peer_dm_subject`.
pub fn peer_dm_subject(peer_id: &str) -> String {
    format!("mu.agent.{}.dm", peer_id.replace(':', "."))
}

/// A Micro service name for a peer. Micro allows only `[A-Za-z0-9_-]`, but this
/// is a registration KEY, not an identity — that travels in metadata — so it
/// only has to be legal and unique. Nothing parses it.
pub fn micro_name(peer_id: &str) -> String {
    peer_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A `$SRV` agent id as a dialogue peer id — the inverse of [`mesh_name`].
///
/// A `$SRV` responder's name as a dialogue peer id, for peers that advertise no
/// metadata. Those are daemons registering their bare id, so the role is `mu`.
pub fn srv_agent_to_peer_id(agent: &str) -> String {
    inbound_peer_id(agent)
}

/// One verified inbound DM, handed to the store writer.
///
/// A channel rather than a direct `Store` call: the `Store` owns the
/// `Gateway`, so the inbound task cannot also own the `Store` without a cycle.
#[derive(Debug, Clone)]
pub struct InboundDm {
    pub to_peer: String,
    pub from_peer: String,
    pub body: String,
    pub subject: Option<String>,
}

// ─────────────────────────────── Gateway ────────────────────────────────────

/// A peer this gateway fronts on the mesh: its DM subscription and its `$SRV`
/// presence registration. Dropping it releases both (aborting the task ends
/// the subscription; dropping the Micro `Service` deregisters presence).
struct Fronted {
    task: tokio::task::JoinHandle<()>,
    _presence: async_nats::service::Service,
}

impl Drop for Fronted {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Gateway {
    client: async_nats::Client,
    /// Mints capabilities for outbound DMs.
    root: KeyPair,
    /// Verifies inbound capabilities.
    issuer: PublicKey,
    inbound_tx: mpsc::UnboundedSender<InboundDm>,
    fronted: Mutex<HashMap<String, Fronted>>,
    /// Cached `$SRV` view, because routing every `say` correctly means knowing
    /// whether the target is on the mesh, and a discovery sweep costs
    /// [`WHO_WINDOW`] — far too much to pay per message. Mesh membership
    /// changes on the scale of process lifetimes, so a short TTL is plenty.
    live_cache: Mutex<Option<(tokio::time::Instant, HashMap<String, String>)>>,
}

/// How long a `$SRV` sweep's result is trusted for routing decisions. Short
/// enough that a daemon which just died stops attracting mesh-routed messages
/// almost immediately (the deferred dead-letter decision on at-uws assumes the
/// window is small), long enough that a burst of messages costs one sweep.
const LIVENESS_TTL: Duration = Duration::from_secs(5);

/// Connect to the mesh. Bounded setup — this runs on the startup path, and a
/// NATS that is down must fail fast rather than hang the server.
pub async fn connect(cfg: &MeshConfig) -> Result<(Gateway, mpsc::UnboundedReceiver<InboundDm>)> {
    let root = KeyPair::from(
        &PrivateKey::from_bytes_hex(&cfg.issuer_key, biscuit_auth::builder::Algorithm::Ed25519)
            .map_err(|e| anyhow!("[mesh].issuer_key is not a valid hex Ed25519 key: {e}"))?,
    );
    let issuer = root.public();
    let client = tokio::time::timeout(NATS_SETUP_TIMEOUT, async {
        let client = async_nats::connect(&cfg.nats_url)
            .await
            .map_err(|e| anyhow!("gateway: connect NATS at {}: {e}", cfg.nats_url))?;
        client
            .flush()
            .await
            .map_err(|e| anyhow!("gateway: flush: {e}"))?;
        Ok::<_, anyhow::Error>(client)
    })
    .await
    .map_err(|_| anyhow!("gateway: NATS setup at {} timed out", cfg.nats_url))??;

    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    info!(nats = %cfg.nats_url, "mesh gateway: connected");
    Ok((
        Gateway {
            client,
            root,
            issuer,
            inbound_tx,
            fronted: Mutex::new(HashMap::new()),
            live_cache: Mutex::new(None),
        },
        inbound_rx,
    ))
}

impl Gateway {
    /// Publish a DM to a mesh target on `from_peer`'s behalf, capability minted
    /// per send. Returns the envelope id. Fire-and-forget: core NATS does not
    /// tell us whether anyone was subscribed, which is exactly why the caller
    /// stores its durable row regardless.
    pub async fn publish_dm(
        &self,
        from_peer: &str,
        target: &MeshTarget,
        body: &str,
        subject: Option<&str>,
    ) -> Result<String> {
        let token = biscuit!(r#"right({r});"#, r = DM_RIGHT)
            .build(&self.root)
            .map_err(|e| anyhow!("mint capability: {e}"))?
            .to_vec()
            .map_err(|e| anyhow!("encode capability: {e}"))?;
        let id = ulid::Ulid::new().to_string();
        let env = DmEnvelope {
            id: id.clone(),
            capability: base64::engine::general_purpose::STANDARD.encode(token),
            command: AgentCommand::Dm {
                // The dialogue peer id, not the gateway's — the receiving
                // session must see who actually wrote, and be able to reply.
                from: from_peer.to_string(),
                body: body.to_string(),
                session: target.session.clone(),
                subject: subject.map(str::to_string),
                // A fronted peer's id is already flat (`cc:<uuid>`), so there
                // is no second level for the receiver to reassemble.
                from_session: None,
            },
        };
        let payload = serde_json::to_vec(&env)?;
        self.client
            .publish(target.subject.clone(), payload.into())
            .await
            .map_err(|e| anyhow!("publish: {e}"))?;
        self.client
            .flush()
            .await
            .map_err(|e| anyhow!("flush: {e}"))?;
        Ok(id)
    }

    /// Agents present on the mesh right now, via `$SRV.PING` — liveness-derived
    /// discovery, no roster. Returns bare agent ids (prefix stripped).
    /// Live peers, as peer id -> the subject to publish to. The subject is the
    /// one the peer ADVERTISES, so this never re-derives a naming rule; a peer
    /// publishing no metadata is a daemon on the legacy flat subject.
    pub async fn srv_agents(&self) -> Result<HashMap<String, String>> {
        let inbox = self.client.new_inbox();
        let mut sub = self
            .client
            .subscribe(inbox.clone())
            .await
            .map_err(|e| anyhow!("srv inbox: {e}"))?;
        self.client
            .publish_with_reply("$SRV.PING".to_string(), inbox, Bytes::new())
            .await
            .map_err(|e| anyhow!("srv ping: {e}"))?;
        self.client
            .flush()
            .await
            .map_err(|e| anyhow!("srv flush: {e}"))?;

        let mut agents = HashMap::new();
        let deadline = tokio::time::Instant::now() + WHO_WINDOW;
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, sub.next()).await {
            let Ok(v) = serde_json::from_slice::<Value>(&msg.payload) else {
                continue;
            };
            let meta = |k: &str| {
                v.get("metadata")
                    .and_then(|m| m.get(k))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            match meta(META_PEER_ID) {
                Some(peer_id) => {
                    let subject =
                        meta(META_DM_SUBJECT).unwrap_or_else(|| peer_dm_subject(&peer_id));
                    agents.insert(peer_id, subject);
                }
                // No metadata: a daemon predating mu-b1lq, reachable only on
                // its flat subject. Deriving a hierarchical one would publish
                // where it is not listening.
                None => {
                    if let Some(agent) = v
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(|n| n.strip_prefix(PRESENCE_PREFIX))
                    {
                        agents.insert(srv_agent_to_peer_id(agent), dm_subject(agent));
                    }
                }
            }
        }
        Ok(agents)
    }

    /// Is `agent` on the mesh right now? The routing question: when the answer
    /// is yes the mesh carries the message and the store must not ALSO serve it
    /// to a poller, because a daemon reading both would see everything twice.
    ///
    /// Backed by a [`LIVENESS_TTL`] cache. The sweep runs without the lock
    /// held, so a burst of concurrent sends may briefly duplicate the sweep
    /// rather than queue behind it — the wasted work is one PING, and the
    /// alternative serialises every `say` behind a 300ms window.
    pub async fn live_subject(&self, peer_id: &str) -> Option<String> {
        if let Some((at, agents)) = self.live_cache.lock().await.as_ref() {
            if at.elapsed() < LIVENESS_TTL {
                return agents.get(peer_id).cloned();
            }
        }
        let agents: HashMap<String, String> = match self.srv_agents().await {
            Ok(a) => a,
            Err(e) => {
                // Fail CLOSED for routing: if we cannot confirm the peer is on
                // the mesh, keep the message in the store where its MCP poll
                // will find it. Never strand it on a path we cannot verify.
                warn!("gateway: $SRV liveness check failed, routing to the store: {e:#}");
                return None;
            }
        };
        let found = agents.get(peer_id).cloned();
        *self.live_cache.lock().await = Some((tokio::time::Instant::now(), agents));
        found
    }

    /// Where to send a DM for `peer_id`, or None if it is not on the mesh.
    /// Prefers the session's own inbox (mu-6s7s), where the subject is the
    /// address and no `session` field is needed; falls back to the daemon
    /// subject plus that field for daemons without per-session inboxes.
    pub async fn address(&self, peer_id: &str) -> Option<MeshTarget> {
        let target = resolve_target(peer_id)?;
        // The session itself on the mesh → its own subject identifies it, so
        // the envelope needs no `session` field.
        if target.session.is_some() {
            if let Some(subject) = self.live_subject(peer_id).await {
                return Some(MeshTarget {
                    subject,
                    session: None,
                });
            }
        }
        // Otherwise its daemon, which still needs the session named. Use the
        // subject the daemon advertises, so one predating mu-b1lq is reached
        // on the flat subject it actually listens to.
        self.live_subject(&inbound_peer_id(&target.daemon))
            .await
            .map(|subject| MeshTarget {
                subject,
                session: target.session.clone(),
            })
    }

    /// Start fronting `peer_id` on the mesh: register its presence so mu's
    /// `who` lists it, and hold a DM subscription so messages arriving between
    /// its polls are buffered into the store rather than dropped.
    ///
    /// Idempotent — re-fronting an already-fronted peer is a no-op, since a
    /// second subscription would double every delivery. Returns whether this
    /// call created the registration.
    pub async fn front_peer(&self, peer_id: &str) -> Result<bool> {
        use async_nats::service::ServiceExt as _;

        // Claim the slot before any await so a concurrent call cannot
        // double-subscribe; released on failure so a transient error does not
        // poison future attempts.
        {
            let fronted = self.fronted.lock().await;
            if fronted.contains_key(peer_id) {
                return Ok(false);
            }
        }
        // One encoded name for both the presence registration and the inbox,
        // so the subject a daemon derives from `who` is the one we listen on.
        let subject = peer_dm_subject(peer_id);
        let presence = self
            .client
            .service_builder()
            .description("mu-dialogue gateway (fronting an MCP peer)")
            .metadata(std::collections::HashMap::from([
                (META_PEER_ID.to_string(), peer_id.to_string()),
                (META_DM_SUBJECT.to_string(), subject.clone()),
            ]))
            .start(format!("{PRESENCE_PREFIX}{}", micro_name(peer_id)), "0.1.0")
            .await
            .map_err(|e| anyhow!("gateway: presence register {peer_id}: {e}"))?;
        let mut sub = self
            .client
            .subscribe(subject.clone())
            .await
            .map_err(|e| anyhow!("gateway: dm subscribe {peer_id}: {e}"))?;
        self.client
            .flush()
            .await
            .map_err(|e| anyhow!("gateway: flush: {e}"))?;

        let issuer = self.issuer;
        let tx = self.inbound_tx.clone();
        let to_peer = peer_id.to_string();
        let task = tokio::spawn(async move {
            while let Some(msg) = sub.next().await {
                let Ok(env) = serde_json::from_slice::<DmEnvelope>(&msg.payload) else {
                    debug!(peer = %to_peer, "gateway: dropping malformed dm envelope");
                    continue;
                };
                if !dm_authorized(&env.capability, issuer) {
                    warn!(peer = %to_peer, "gateway: dropping unauthorized dm");
                    continue;
                }
                let AgentCommand::Dm {
                    from,
                    body,
                    subject,
                    from_session,
                    ..
                } = env.command;
                if tx
                    .send(InboundDm {
                        to_peer: to_peer.clone(),
                        from_peer: inbound_peer_id_with_session(&from, from_session.as_deref()),
                        body,
                        subject,
                    })
                    .is_err()
                {
                    break; // store writer gone; the server is shutting down
                }
            }
        });

        let mut fronted = self.fronted.lock().await;
        if fronted.contains_key(peer_id) {
            // Lost a race: another call registered while we were awaiting.
            // Dropping ours releases both the subscription and the presence.
            task.abort();
            return Ok(false);
        }
        fronted.insert(
            peer_id.to_string(),
            Fronted {
                task,
                _presence: presence,
            },
        );
        info!(peer = %peer_id, "mesh gateway: fronting peer (presence + dm inbox)");
        Ok(true)
    }

    /// Stop fronting a peer — releases its subscription and deregisters its
    /// presence. Used by the stale-peer sweep.
    pub async fn release_peer(&self, peer_id: &str) -> bool {
        let removed = self.fronted.lock().await.remove(peer_id).is_some();
        if removed {
            info!(peer = %peer_id, "mesh gateway: released peer");
        }
        removed
    }

    /// The peers this gateway currently fronts.
    pub async fn fronted_peers(&self) -> Vec<String> {
        let mut v: Vec<String> = self.fronted.lock().await.keys().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GOLDEN: the DM envelope must match the mesh contract
    /// (`mesh-slice/src/agent.rs`) — externally-tagged command, base64
    /// capability, ulid string id — and mu's mirror in
    /// `crates/mu-coding/src/serve/mesh_dialogue.rs`. `session`/`subject` are
    /// additive: absent when None, so slice-shaped peers still parse it.
    #[test]
    fn dm_envelope_wire_shape_matches_the_mesh_contract() {
        let env = DmEnvelope {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            capability: "AAEC".to_string(),
            command: AgentCommand::Dm {
                from: "cc:abc".to_string(),
                body: "review PR 42?".to_string(),
                session: None,
                subject: None,
                from_session: None,
            },
        };
        assert_eq!(
            serde_json::to_value(&env).expect("serialize"),
            serde_json::json!({
                "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                "capability": "AAEC",
                "command": {"Dm": {"from": "cc:abc", "body": "review PR 42?"}}
            })
        );
        // Slice-shaped inbound (no session/subject) decodes.
        let inbound: DmEnvelope = serde_json::from_value(serde_json::json!({
            "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "capability": "AAEC",
            "command": {"Dm": {"from": "bb073ae9893a123a", "body": "hi"}}
        }))
        .expect("decode slice-shaped dm");
        let AgentCommand::Dm {
            from,
            session,
            subject,
            ..
        } = inbound.command;
        assert_eq!(from, "bb073ae9893a123a");
        assert!(session.is_none());
        assert!(subject.is_none());
    }

    /// The richer fields serialize when present — otherwise the subject slice
    /// (mu#511/agent_tools#59) is wasted on every gateway-sent DM.
    #[test]
    fn session_and_subject_ride_the_envelope_when_set() {
        let env = DmEnvelope {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            capability: "AAEC".to_string(),
            command: AgentCommand::Dm {
                from: "cc:abc".to_string(),
                body: "body".to_string(),
                session: Some("session-2".to_string()),
                subject: Some("PR 42 is green".to_string()),
                from_session: None,
            },
        };
        assert_eq!(
            serde_json::to_value(&env).expect("serialize")["command"]["Dm"],
            serde_json::json!({
                "from": "cc:abc",
                "body": "body",
                "session": "session-2",
                "subject": "PR 42 is green"
            })
        );
    }

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

        // A token minted by anyone else is refused.
        assert!(!dm_authorized(&good, KeyPair::new().public()));

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

    /// The translation the bead names: dialogue's `role:identity` ids onto the
    /// mesh's subject + optional session field.
    #[test]
    fn peer_ids_translate_to_mesh_targets() {
        // Daemon-level id → supervisor session (no `session` field).
        assert_eq!(
            resolve_target("mu:100c058851c356a5"),
            Some(MuPeer {
                daemon: "100c058851c356a5".to_string(),
                session: None
            })
        );
        // Session-level id → subject is the DAEMON, session rides the envelope.
        assert_eq!(
            resolve_target("mu:100c058851c356a5:session-2"),
            Some(MuPeer {
                daemon: "100c058851c356a5".to_string(),
                session: Some("session-2".to_string())
            })
        );
        // cc peers are served by this gateway's own store. Publishing one to
        // the mesh would round-trip back into that store as a duplicate.
        assert_eq!(
            resolve_target("cc:17302f24-836a-4f82-a988-cb711338e6e7"),
            None
        );
        // Malformed / non-mesh ids address nothing.
        assert_eq!(resolve_target("mu:"), None);
        assert_eq!(resolve_target("mu::session-1"), None);
        assert_eq!(resolve_target("mu:daemon:"), None);
        assert_eq!(resolve_target("warden:x"), None);
        assert_eq!(resolve_target(""), None);
    }

    #[test]
    fn inbound_from_is_attributed_to_the_sending_daemon() {
        assert_eq!(inbound_peer_id("bb073ae9893a123a"), "mu:bb073ae9893a123a");
        // Round-trips: the id an inbound DM is attributed to is one a reply
        // can be addressed to.
        assert!(resolve_target(&inbound_peer_id("bb073ae9893a123a")).is_some());
    }

    /// The reply path. A DM carrying the sender's session must be attributed
    /// to THAT session, and addressing the attribution back must resolve to
    /// the same daemon and session — otherwise a reply reaches the daemon's
    /// supervisor and the session that wrote never hears back.
    #[test]
    fn a_reply_addresses_the_session_that_wrote_not_the_supervisor() {
        let peer = inbound_peer_id_with_session("bb073ae9893a123a", Some("session-3"));
        assert_eq!(peer, "mu:bb073ae9893a123a:session-3");
        assert_eq!(
            resolve_target(&peer),
            Some(MuPeer {
                daemon: "bb073ae9893a123a".to_string(),
                session: Some("session-3".to_string()),
            }),
            "a reply must come back to the sending session"
        );
        // A peer predating the field still resolves — to the daemon, which is
        // the old behaviour rather than a broken id.
        for absent in [None, Some("")] {
            let peer = inbound_peer_id_with_session("bb073ae9893a123a", absent);
            assert_eq!(peer, "mu:bb073ae9893a123a");
            assert_eq!(
                resolve_target(&peer).unwrap().session,
                None,
                "without from_session a reply falls back to the supervisor"
            );
        }
    }

    #[test]
    fn dm_subject_matches_the_contract() {
        assert_eq!(dm_subject("abc"), "mu.agent.abc.dm");
        assert_eq!(peer_dm_subject("cc:uuid"), "mu.agent.cc.uuid.dm");
    }

    /// A peer id's levels become subject tokens, so NATS itself routes:
    /// dispatch on the daemon token, then the session token (mu-b1lq).
    #[test]
    fn peer_ids_become_hierarchical_subjects() {
        assert_eq!(
            peer_dm_subject("mu:100c058851c356a5:session-2"),
            "mu.agent.mu.100c058851c356a5.session-2.dm"
        );
        assert_eq!(
            peer_dm_subject("mu:100c058851c356a5"),
            "mu.agent.mu.100c058851c356a5.dm"
        );
        assert_eq!(
            peer_dm_subject("cc:deploy-test"),
            "mu.agent.cc.deploy-test.dm"
        );
        // A daemon's wildcard for all of its sessions is a prefix of these.
        assert!(peer_dm_subject("mu:d:s1").starts_with("mu.agent.mu.d."));
    }

    /// The Micro service name is a registration KEY, not an identity — Micro
    /// allows only [A-Za-z0-9_-], and identity travels in metadata instead.
    #[test]
    fn micro_names_are_legal_for_every_peer_id() {
        for peer in [
            "cc:17302f24-836a-4f82-a988-cb711338e6e7",
            "warden:sub_agent_3",
            "mu:100c058851c356a5:session-2",
        ] {
            let n = micro_name(peer);
            assert!(
                n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{n} is not a legal Micro service name"
            );
        }
    }

    fn write_cfg(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mu-dlg-mesh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn config_absent_or_disabled_means_none() {
        let dir = std::env::temp_dir().join(format!("mu-dlg-mesh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir.join("missing.toml")).is_none());
        assert!(load(&write_cfg("nosection.toml", "[other]\nx = 1\n")).is_none());
        // A [mesh] section alone must NOT switch the gateway on — the daemon
        // has had one for a while and enabling it here is a separate decision.
        assert!(load(&write_cfg(
            "meshonly.toml",
            "[mesh]\nenabled = true\nnats_url = \"127.0.0.1:4222\"\nissuer_key = \"ab\"\n"
        ))
        .is_none());
        assert!(load(&write_cfg(
            "off.toml",
            "[dialogue.mesh]\nenabled = false\n[mesh]\nnats_url = \"x\"\nissuer_key = \"ab\"\n"
        ))
        .is_none());
        // Enabled but nothing to connect to / sign with → still disabled.
        assert!(load(&write_cfg(
            "nourl.toml",
            "[dialogue.mesh]\nenabled = true\n"
        ))
        .is_none());
        assert!(load(&write_cfg(
            "nokey.toml",
            "[dialogue.mesh]\nenabled = true\n[mesh]\nnats_url = \"127.0.0.1:4222\"\n"
        ))
        .is_none());
    }

    #[test]
    fn config_inherits_from_the_fleet_mesh_section_and_can_override() {
        let cfg = load(&write_cfg(
            "inherit.toml",
            "[dialogue.mesh]\nenabled = true\n\
             [mesh]\nnats_url = \"127.0.0.1:4222\"\nissuer_key = \"beef\"\n",
        ))
        .expect("enabled");
        assert_eq!(cfg.nats_url, "127.0.0.1:4222");
        assert_eq!(cfg.issuer_key, "beef");

        let cfg = load(&write_cfg(
            "override.toml",
            "[dialogue.mesh]\nenabled = true\nnats_url = \"10.0.0.9:4222\"\n\
             [mesh]\nnats_url = \"127.0.0.1:4222\"\nissuer_key = \"beef\"\n",
        ))
        .expect("enabled");
        assert_eq!(cfg.nats_url, "10.0.0.9:4222");
        assert_eq!(cfg.issuer_key, "beef");
    }
}

/// End-to-end tests against a REAL NATS server, `#[ignore]`d so `cargo test`
/// stays hermetic:
/// `MU_DIALOGUE_TEST_NATS=127.0.0.1:4222 cargo test -p mu-dialogue -- --ignored`
///
/// Each mints its own issuer keypair and uses process-unique subjects, so none
/// can reach a live daemon's mailbox or depend on the fleet issuer key.
#[cfg(test)]
mod live_tests {
    use super::*;

    fn nats_url() -> String {
        std::env::var("MU_DIALOGUE_TEST_NATS").unwrap_or_else(|_| "127.0.0.1:4222".to_string())
    }

    /// A gateway on a throwaway issuer key, plus a raw client to play the peer
    /// on the other side of the wire.
    async fn live_gateway() -> (Gateway, mpsc::UnboundedReceiver<InboundDm>, KeyPair) {
        let root = KeyPair::new();
        let cfg = MeshConfig {
            enabled: true,
            nats_url: nats_url(),
            issuer_key: root.private().to_bytes_hex(),
        };
        let (gw, rx) = connect(&cfg).await.expect("connect to NATS");
        (gw, rx, root)
    }

    fn mint(root: &KeyPair, right: &str) -> String {
        let token = biscuit!(r#"right({r});"#, r = right)
            .build(root)
            .unwrap()
            .to_vec()
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(token)
    }

    /// Micro accepts only `[A-Za-z0-9_-]` in a service NAME, so identity is
    /// advertised in metadata instead. Pins that against a real server: the
    /// peer registers, and `$SRV` hands back its id verbatim.
    #[tokio::test]
    #[ignore = "requires a live NATS server"]
    async fn a_fronted_peer_registers_and_is_discoverable_by_its_real_id() {
        let (gw, _rx, _root) = live_gateway().await;
        let peer = format!("cc:gw-selftest-{}", std::process::id());
        assert!(gw.front_peer(&peer).await.expect("front a cc: peer"));
        // Re-fronting must not add a second subscription.
        assert!(!gw.front_peer(&peer).await.expect("idempotent"));
        assert_eq!(gw.fronted_peers().await, vec![peer.clone()]);

        // It answers $SRV under the encoded name, so mu's `who` lists it...
        let agents = gw.srv_agents().await.expect("srv sweep");
        assert!(
            agents.contains_key(&peer),
            "fronted peer missing from $SRV: {agents:?}"
        );

        // Micro still refuses a colon in a NAME — which is why identity moved
        // to metadata rather than being encoded into the name.
        use async_nats::service::ServiceExt as _;
        let err = gw
            .client
            .service_builder()
            .start(format!("{PRESENCE_PREFIX}{peer}"), "0.1.0")
            .await
            .expect_err("NATS Micro must reject a colon in a service name");
        assert!(
            err.to_string().contains("not a valid string"),
            "unexpected rejection reason: {err}"
        );

        assert!(gw.release_peer(&peer).await);
    }

    /// Inbound: a capability-signed DM on a fronted peer's subject arrives on
    /// the channel, attributed to the sending daemon, with its subject intact.
    #[tokio::test]
    #[ignore = "requires a live NATS server"]
    async fn a_verified_inbound_dm_reaches_the_store_writer() {
        let (gw, mut rx, root) = live_gateway().await;
        let peer = format!("cc:gw-inbound-{}", std::process::id());
        gw.front_peer(&peer).await.expect("front");

        let client = async_nats::connect(&nats_url()).await.expect("raw client");
        let env = DmEnvelope {
            id: ulid::Ulid::new().to_string(),
            capability: mint(&root, DM_RIGHT),
            command: AgentCommand::Dm {
                from: "bb073ae9893a123a".to_string(),
                body: "the gateway works".to_string(),
                session: None,
                subject: Some("gateway selftest".to_string()),
                // A daemon on the current build names its sending session.
                from_session: Some("session-7".to_string()),
            },
        };
        client
            .publish(
                peer_dm_subject(&peer),
                serde_json::to_vec(&env).unwrap().into(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();

        let dm = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("inbound dm within 5s")
            .expect("channel open");
        assert_eq!(dm.to_peer, peer);
        // Attributed to the sending SESSION, so replying to `from_peer`
        // reaches the session that wrote rather than the daemon's supervisor.
        assert_eq!(dm.from_peer, "mu:bb073ae9893a123a:session-7");
        assert_eq!(
            resolve_target(&dm.from_peer)
                .and_then(|t| t.session)
                .as_deref(),
            Some("session-7"),
        );
        assert_eq!(dm.body, "the gateway works");
        assert_eq!(dm.subject.as_deref(), Some("gateway selftest"));
    }

    /// No fail-open: a DM signed by the wrong key, or carrying the wrong right,
    /// is dropped rather than delivered — verified on the real wire, not just
    /// against `dm_authorized` in isolation.
    #[tokio::test]
    #[ignore = "requires a live NATS server"]
    async fn an_unauthorized_inbound_dm_is_dropped_on_the_wire() {
        let (gw, mut rx, root) = live_gateway().await;
        let peer = format!("cc:gw-authz-{}", std::process::id());
        gw.front_peer(&peer).await.expect("front");
        let client = async_nats::connect(&nats_url()).await.expect("raw client");

        let send = |capability: String, body: &str| {
            let env = DmEnvelope {
                id: ulid::Ulid::new().to_string(),
                capability,
                command: AgentCommand::Dm {
                    from: "rogue".to_string(),
                    body: body.to_string(),
                    session: None,
                    subject: None,
                    from_session: None,
                },
            };
            serde_json::to_vec(&env).unwrap()
        };
        // Signed by a key this gateway does not trust.
        let rogue = KeyPair::new();
        client
            .publish(
                peer_dm_subject(&peer),
                send(mint(&rogue, DM_RIGHT), "rogue").into(),
            )
            .await
            .unwrap();
        // Correct issuer, wrong right.
        client
            .publish(
                peer_dm_subject(&peer),
                send(mint(&root, "code_recall"), "wrong right").into(),
            )
            .await
            .unwrap();
        // Not even an envelope.
        client
            .publish(peer_dm_subject(&peer), Bytes::from_static(b"{garbage"))
            .await
            .unwrap();
        client.flush().await.unwrap();

        // Then a good one: if it arrives first, the three above were dropped.
        client
            .publish(
                peer_dm_subject(&peer),
                send(mint(&root, DM_RIGHT), "legitimate").into(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();

        let dm = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the legitimate dm within 5s")
            .expect("channel open");
        assert_eq!(
            dm.body, "legitimate",
            "an unauthorized dm was delivered ahead of the legitimate one"
        );
    }

    /// A session on the mesh is addressed DIRECTLY, on the subject it
    /// advertises, so the envelope needs no `session` field. A daemon that
    /// predates mu-b1lq advertises no metadata and is reached on its flat
    /// subject with the session named — deriving a hierarchical subject for it
    /// would publish where it is not listening.
    ///
    /// All cases come off ONE `$SRV` sweep: the liveness cache is keyed by
    /// time, not by agent, so registering everything up front avoids a wait.
    #[tokio::test]
    #[ignore = "requires a live NATS server"]
    async fn addressing_prefers_a_live_session_and_respects_a_legacy_daemon() {
        use async_nats::service::ServiceExt as _;
        let (gw, _rx, _root) = live_gateway().await;
        let pid = std::process::id();
        let modern = format!("gwmodern{pid}");
        let legacy = format!("gwlegacy{pid}");
        let sess_peer = format!("mu:{modern}:session-4");

        let client = async_nats::connect(&nats_url()).await.expect("raw client");
        let meta = |peer: &str| {
            std::collections::HashMap::from([
                (META_PEER_ID.to_string(), peer.to_string()),
                (META_DM_SUBJECT.to_string(), peer_dm_subject(peer)),
            ])
        };
        // A daemon that advertises itself, plus one of its sessions.
        let _d1 = client
            .service_builder()
            .metadata(meta(&format!("mu:{modern}")))
            .start(format!("{PRESENCE_PREFIX}{modern}"), "0.1.0")
            .await
            .expect("modern daemon presence");
        let _s1 = client
            .service_builder()
            .metadata(meta(&sess_peer))
            .start(
                format!("{PRESENCE_PREFIX}{}", micro_name(&sess_peer)),
                "0.1.0",
            )
            .await
            .expect("session presence");
        // A daemon predating this: no metadata at all.
        let _d2 = client
            .service_builder()
            .start(format!("{PRESENCE_PREFIX}{legacy}"), "0.1.0")
            .await
            .expect("legacy daemon presence");
        client.flush().await.unwrap();

        // Session present → its own hierarchical subject, no envelope field.
        let t = gw.address(&sess_peer).await.expect("session reachable");
        assert_eq!(t.subject, format!("mu.agent.mu.{modern}.session-4.dm"));
        assert_eq!(
            t.session, None,
            "the subject already identifies the session"
        );

        // Same daemon, a session that never joined → the daemon, session named.
        let t = gw
            .address(&format!("mu:{modern}:session-99"))
            .await
            .expect("falls back to the daemon");
        assert_eq!(t.subject, format!("mu.agent.mu.{modern}.dm"));
        assert_eq!(t.session.as_deref(), Some("session-99"));

        // Legacy daemon → the FLAT subject it actually listens on.
        let t = gw
            .address(&format!("mu:{legacy}:session-1"))
            .await
            .expect("legacy daemon reachable");
        assert_eq!(t.subject, format!("mu.agent.{legacy}.dm"));
        assert_eq!(t.session.as_deref(), Some("session-1"));

        // Absent entirely → not reachable, so the message stays in the store.
        assert!(gw
            .address(&format!("mu:gwabsent{pid}:session-1"))
            .await
            .is_none());
    }

    /// Outbound: what `dialogue_say` puts on the wire is an envelope a mu
    /// daemon accepts — right subject, right shape, session and subject
    /// carried. Addressed to a daemon id unique to this test, so no live
    /// mailbox is touched.
    #[tokio::test]
    #[ignore = "requires a live NATS server"]
    async fn an_outbound_dm_matches_what_a_daemon_expects() {
        let (gw, _rx, root) = live_gateway().await;
        let daemon = format!("gwtest{}", std::process::id());
        let client = async_nats::connect(&nats_url()).await.expect("raw client");
        let mut sub = client.subscribe(dm_subject(&daemon)).await.expect("sub");
        client.flush().await.unwrap();

        let parsed = resolve_target(&format!("mu:{daemon}:session-2")).expect("resolves");
        assert_eq!(parsed.daemon, daemon);
        let target = MeshTarget {
            subject: dm_subject(&daemon),
            session: parsed.session.clone(),
        };
        let sent_id = gw
            .publish_dm("cc:sender", &target, "body text", Some("a subject"))
            .await
            .expect("publish");

        let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("envelope within 5s")
            .expect("subscription open");
        let env: DmEnvelope = serde_json::from_slice(&msg.payload).expect("decode envelope");
        assert_eq!(env.id, sent_id);
        // A daemon holding the same issuer key accepts it.
        assert!(dm_authorized(&env.capability, root.public()));
        let AgentCommand::Dm {
            from,
            body,
            session,
            subject,
            ..
        } = env.command;
        // The DIALOGUE peer id rides through, so the receiver can reply.
        assert_eq!(from, "cc:sender");
        assert_eq!(body, "body text");
        assert_eq!(session.as_deref(), Some("session-2"));
        assert_eq!(subject.as_deref(), Some("a subject"));
    }
}
