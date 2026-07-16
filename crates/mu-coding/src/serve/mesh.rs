//! mu-wxc4: the mesh transport adapter — mu as a first-class adapter on the
//! NATS service mesh, integrated through the SAME mu-046 seams as the stdio
//! (#1) and MCP (#2) adapters. This is the deliberate answer to the mu-side
//! integration failures the operator called out:
//!
//!   - **Inbound** crosses [`pipeline::ingest`] — journaled and sequenced at
//!     the one border — becoming an ordinary command. NOT `input_rx`
//!     side-injection, NOT poll-and-inject.
//!   - **Outbound** rides an outbound [`Router`] lane this adapter registers
//!     and is the SOLE consumer of — mirroring the stdio `write_loop`. NOT a
//!     second consumer filtering the shared stream.
//!
//! The transport (NATS) is behind a seam: inbound messages arrive on a
//! channel, outbound goes through [`MeshEgress`]. In production a NATS
//! subscription feeds the channel and a NATS publisher implements the trait;
//! in tests both are in-memory, so the integration — that traffic actually
//! traverses `ingest` and the `Router` — is proven without a live broker.
//!
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;

use mu_core::command_journal::Origin;
use mu_core::protocol::Request;
use mu_core::transport::{LaneTerminated, Outbound, OutboundEnvelope, Router};

use super::auth::AuthStateHandle;
use super::pipeline::{self, ControlPlane};

/// One inbound mesh message: a JSON-RPC request plus the subject its reply
/// should be published to. Transport-agnostic — a NATS subscription builds
/// these in production; a test channel does in unit tests.
pub(crate) struct MeshInbound {
    pub request: Request<Value>,
    pub reply_to: String,
}

/// Where the adapter publishes outbound bytes on the mesh. A NATS client
/// implements this in production; a capturing sink in tests. `publish` is
/// awaited by the drain in lane order (not spawned), so replies leave the mesh
/// in the order the Router delivered them and no publish task can outlive the
/// adapter — the `Send` future keeps `serve_mesh`'s task spawnable.
pub(crate) trait MeshEgress: Send + Sync + 'static {
    fn publish(
        &self,
        subject: String,
        payload: Value,
    ) -> impl std::future::Future<Output = ()> + Send;
}

static MESH_CONN_SEQ: AtomicU64 = AtomicU64::new(0);
/// Monotonic, process-global correlation-id source. JSON-RPC request ids are
/// client-local, but the mesh subject multiplexes many peers onto ONE Router
/// lane whose replies are correlated by request id — so two peers both using
/// `id: 1` would collide and misroute. Each inbound mesh request is rewritten
/// to a unique correlation id from this counter for its trip through the
/// pipeline; the client's original id is restored on the reply.
static MESH_CORR_SEQ: AtomicU64 = AtomicU64::new(0);

/// Bounded `correlation-id → (original JSON-RPC id, reply subject)` map. The
/// mesh — unlike the stdio socket, where one connection carries every reply —
/// must correlate each async outbound back to the NATS reply subject its
/// request arrived on, AND restore the client's own id. An entry is recorded
/// on inbound (keyed by the unique correlation id, NOT the client id, so peers
/// cannot collide) and taken on the matching outbound. A request that `ingest`
/// accepts but that never produces a matching outbound (e.g. a session torn
/// down before it responds) would otherwise leak its entry forever; `CAP`
/// bounds that by evicting the oldest still-pending entry (with a warning) once
/// full. `map` and `order` stay in lockstep, so eviction is O(1) and `take` is
/// O(pending).
struct PendingReplies {
    map: HashMap<String, (Value, String)>,
    order: VecDeque<String>,
}

impl PendingReplies {
    const CAP: usize = 4096;

    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Record the client's `original_id` + reply `subject` under correlation id
    /// `corr`, evicting the oldest pending entry first if at capacity.
    fn record(&mut self, corr: String, original_id: Value, subject: String) {
        if self.map.len() >= Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
                tracing::warn!(
                    evicted = %old,
                    cap = Self::CAP,
                    "mesh: pending-reply map full — evicting oldest unanswered request; \
                     a peer was accepted but never had a reply routed back to it"
                );
            }
        }
        if self
            .map
            .insert(corr.clone(), (original_id, subject))
            .is_none()
        {
            self.order.push_back(corr);
        }
    }

    /// Take (and remove) the `(original_id, subject)` recorded for `corr`.
    fn take(&mut self, corr: &str) -> Option<(Value, String)> {
        let entry = self.map.remove(corr)?;
        if let Some(pos) = self.order.iter().position(|x| x == corr) {
            self.order.remove(pos);
        }
        Some(entry)
    }
}

/// Run the mesh adapter: the mirror of `serve_with_ingest` over a message
/// transport. Registers one egress lane (this adapter is a single logical
/// connection, like a stdio client), drains it to the mesh routed by reply
/// subject, and feeds every inbound message through `ingest`. Returns when
/// the inbound channel closes (transport gone), which drops the lane and
/// continues the shutdown cascade.
pub(crate) async fn serve_mesh<E: MeshEgress>(
    control: Arc<ControlPlane>,
    auth_state: AuthStateHandle,
    router: Router,
    mut inbound: mpsc::UnboundedReceiver<MeshInbound>,
    egress: Arc<E>,
) {
    let origin = Origin {
        transport: "mesh".into(),
        connection_id: Some(format!(
            "mesh-{}",
            MESH_CONN_SEQ.fetch_add(1, Ordering::Relaxed)
        )),
    };
    let lane = router.register(origin.clone());

    // request_id → reply subject, recorded on inbound, consumed on outbound.
    // The mesh needs per-request reply routing the stdio socket does not (one
    // socket carries every reply; the mesh addresses each by subject). Bounded
    // (see `PendingReplies`) so unanswered requests cannot leak unbounded.
    let replies: Arc<Mutex<PendingReplies>> = Arc::new(Mutex::new(PendingReplies::new()));

    // Egress drain — the ONE consumer of this lane (INV-8/INV-11), mirroring
    // stdio's `write_loop`. Pull each outbound envelope, route it to its
    // request's reply subject, publish. Runs INSIDE this task (via the
    // `select!` below), NOT a detached spawn: so when the adapter is dropped
    // and serve_mesh's task is aborted, the lane `lane` is dropped with it —
    // the lifetime contract MeshAdapterHandle promises. A dead lane ends the
    // drain (and thus serve_mesh).
    let drain = {
        let replies = replies.clone();
        let egress = egress.clone();
        let lane_origin = origin.clone();
        async move {
            loop {
                match lane.recv().await {
                    Ok(envelope) => {
                        // Correlate by the unique correlation id (recover from a
                        // poisoned lock rather than wedge the outbound — matches
                        // lock_recovering and the ingest side below).
                        let corr = envelope.request_id.as_ref().and_then(|id| id.as_str());
                        let taken = corr.and_then(|corr| {
                            replies.lock().unwrap_or_else(|e| e.into_inner()).take(corr)
                        });
                        if let Some((original_id, subject)) = taken {
                            // Restore the client's own JSON-RPC id before the
                            // reply leaves the mesh — the correlation id is
                            // internal to the pipeline hop.
                            let mut value = envelope.item.0;
                            if let Some(obj) = value.as_object_mut() {
                                obj.insert("id".to_string(), original_id);
                            }
                            // Awaited in lane order — the ONE publisher for this
                            // lane, so replies reach NATS in delivery order.
                            egress.publish(subject, value).await;
                        }
                        // Notifications (no request_id) target a session's mesh
                        // inbox rather than a reply subject — wired when sessions
                        // publish events to the mesh; request/reply is this slice.
                    }
                    Err(LaneTerminated::Closed) => break,
                    Err(LaneTerminated::SlowConsumer { dropped_ephemeral }) => {
                        // Mirror stdio's write_loop: surface the backpressure
                        // disconnect rather than exiting silently (INV-11).
                        tracing::error!(
                            origin = ?lane_origin,
                            dropped_ephemeral,
                            "mesh outbound lane overflowed: disconnecting slow consumer \
                             (spec mu-046 INV-11); the journals hold what did not reach the bus"
                        );
                        break;
                    }
                }
            }
        }
    };

    // Inbound loop: record the reply route, then cross the border via
    // `ingest`. An immediate reject (`Some`) is sent back through the SAME
    // Router lane, so even rejections take the one egress path.
    let ingest_loop = async move {
        while let Some(msg) = inbound.recv().await {
            // Rewrite the client's id to a unique correlation id for the trip
            // through the pipeline (peers multiplexed on this subject may reuse
            // ids); the drain restores the original id on the reply.
            let mut request = msg.request;
            let original_id = request.id.clone();
            let corr = format!("mesh-{}", MESH_CORR_SEQ.fetch_add(1, Ordering::Relaxed));
            request.id = Value::String(corr.clone());

            // Recover from poisoning rather than panic the task (a panic here
            // would drop the lane — the wrong failure mode; see the drain).
            replies.lock().unwrap_or_else(|e| e.into_inner()).record(
                corr.clone(),
                original_id,
                msg.reply_to,
            );

            // Both the immediate reject (Some) and the async response take the
            // SAME egress path: sent via the Router lane keyed by `corr`,
            // restored + published by the drain.
            if let Some(response) = pipeline::ingest(&control, request, origin.clone(), &auth_state)
            {
                match serde_json::to_value(response) {
                    Ok(value) => router.send(OutboundEnvelope {
                        origin: Some(origin.clone()),
                        request_id: Some(Value::String(corr)),
                        command_seq: None,
                        item: Outbound(value),
                    }),
                    // Unlike stdio (which can only drop it), name the loss.
                    Err(e) => tracing::warn!(
                        error = %e,
                        "mesh: immediate reject was unserializable — no reply sent"
                    ),
                }
            }
        }
    };

    // Drive both in this one task. Whichever ends first — transport gone
    // (`ingest_loop`) or lane closed (`drain`) — ends serve_mesh; the other
    // future is dropped with it, releasing the lane. Aborting serve_mesh's
    // task (MeshAdapterHandle::drop) therefore releases the lane too.
    tokio::select! {
        _ = drain => {}
        _ = ingest_loop => {}
    }
}

/// mu-wxc4: the NATS transport for the mesh adapter. `publish` is awaited by
/// the drain (not spawned): async-nats `publish` only buffers into the client's
/// send queue and returns — it does not round-trip to the server — so awaiting
/// it in the drain preserves lane order on the wire without meaningfully
/// blocking, and leaves no detached task to outlive the adapter.
struct NatsMeshEgress {
    client: async_nats::Client,
}

impl MeshEgress for NatsMeshEgress {
    async fn publish(&self, subject: String, payload: Value) {
        match serde_json::to_vec(&payload) {
            // A lost publish means the peer waits out its own request timeout;
            // make that observable rather than silent (the stdio write_loop
            // surfaces its IO failures too).
            Ok(bytes) => {
                if let Err(e) = self.client.publish(subject.clone(), bytes.into()).await {
                    tracing::warn!(error = %e, %subject, "mesh: reply publish failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, %subject, "mesh: reply was unserializable"),
        }
    }
}

/// Holds the adapter's tasks; aborting on drop releases the Router lane and
/// the ControlPlane/Router clones, continuing the shutdown cascade — the same
/// lifetime contract as the MCP/presence guards (mu-ad5x lesson).
pub(crate) struct MeshAdapterHandle {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for MeshAdapterHandle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Connect to NATS, subscribe the daemon's inbound request subject, and run
/// the mesh adapter: inbound NATS requests become [`MeshInbound`] fed through
/// `ingest`; responses publish back to each request's NATS reply subject.
/// This is the production wiring of [`serve_mesh`]; the ingest/Router seams it
/// rides are unit-proven by the test below.
pub(crate) async fn spawn_mesh_adapter(
    nats_url: &str,
    subject: &str,
    control: Arc<ControlPlane>,
    auth_state: AuthStateHandle,
    router: Router,
) -> anyhow::Result<MeshAdapterHandle> {
    use futures::StreamExt;

    let client = async_nats::connect(nats_url)
        .await
        .map_err(|e| anyhow::anyhow!("mesh: connect NATS at {nats_url}: {e}"))?;
    let mut sub = client
        .subscribe(subject.to_string())
        .await
        .map_err(|e| anyhow::anyhow!("mesh: subscribe {subject}: {e}"))?;
    client
        .flush()
        .await
        .map_err(|e| anyhow::anyhow!("mesh: flush: {e}"))?;

    let (tx, rx) = mpsc::unbounded_channel::<MeshInbound>();

    // Inbound: each NATS request (which carries a reply subject) becomes a
    // MeshInbound. A message with no reply subject is a publish (not a
    // request) with nowhere to answer, so it is dropped; an unparseable
    // payload that DOES carry a reply subject gets a JSON-RPC error reply
    // (like stdio's parse_request_line), not silence.
    let err_client = client.clone();
    let inbound_task = tokio::spawn(async move {
        while let Some(msg) = sub.next().await {
            let Some(reply) = msg.reply.clone() else {
                // No reply subject: the peer used publish, not request. The
                // mesh request/reply contract has nowhere to send a response.
                tracing::debug!(subject = %msg.subject, "mesh: dropping request with no reply subject");
                continue;
            };
            let request = match serde_json::from_slice::<Request<Value>>(&msg.payload) {
                Ok(request) => request,
                Err(e) => {
                    // Match stdio: reply with a JSON-RPC error. Per JSON-RPC
                    // 2.0 the id is null for an unidentifiable request — parse
                    // error (-32700) for non-JSON, invalid request (-32600) for
                    // JSON that is not a well-formed request.
                    let (code, message) = if serde_json::from_slice::<Value>(&msg.payload).is_ok() {
                        (-32600, "Invalid Request")
                    } else {
                        (-32700, "Parse error")
                    };
                    tracing::warn!(%reply, error = %e, code, "mesh: unparseable request — replying JSON-RPC error");
                    let err = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": code, "message": message },
                    });
                    if let Ok(bytes) = serde_json::to_vec(&err) {
                        let _ = err_client.publish(reply, bytes.into()).await;
                    }
                    continue;
                }
            };
            if tx
                .send(MeshInbound {
                    request,
                    reply_to: reply.to_string(),
                })
                .is_err()
            {
                break; // adapter gone
            }
        }
    });

    let egress = Arc::new(NatsMeshEgress { client });
    let serve_task = tokio::spawn(serve_mesh(control, auth_state, router, rx, egress));

    tracing::info!(nats = %nats_url, %subject, "mesh adapter connected");
    Ok(MeshAdapterHandle {
        tasks: vec![inbound_task, serve_task],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use mu_core::command_journal::{CommandJournal, FsyncPolicy, JournalPayload};
    use mu_core::config::Config;
    use mu_core::protocol::{Request, JSONRPC_VERSION};
    use serde_json::json;

    use crate::serve::auth::{AuthState, AuthStateHandle};
    use crate::serve::pipeline::{spawn_control_plane, PipelineCtx};
    use crate::serve::sessions::Sessions;
    use crate::serve::LocalRegistryBackend;

    fn test_ctx() -> PipelineCtx {
        let sessions = Sessions::new();
        let factory: crate::serve::factory::ProviderFactory = Arc::new(|_selector, _cache_ttl| {
            Err(anyhow::anyhow!("no provider in mesh unit tests"))
        });
        let daemon_info = crate::serve::DaemonInfo::new("test");
        let discovery = Arc::new(LocalRegistryBackend::new(
            sessions.clone(),
            daemon_info.daemon_id().to_string(),
        ));
        PipelineCtx {
            sessions,
            factory,
            tools: Arc::new(Vec::new()),
            skills: Arc::new(Vec::new()),
            daemon_info,
            discovery,
            auth_registry: Arc::new(crate::serve::auth::registry_from_config(
                &Config::default().auth,
            )),
        }
    }

    fn authed_state() -> AuthStateHandle {
        Arc::new(std::sync::Mutex::new(AuthState::Authenticated {
            capability: mu_core::capability::Capability::root(),
        }))
    }

    /// A capturing egress sink.
    #[derive(Default)]
    struct SinkEgress(Mutex<Vec<(String, Value)>>);
    impl MeshEgress for SinkEgress {
        async fn publish(&self, subject: String, payload: Value) {
            self.0.lock().unwrap().push((subject, payload));
        }
    }

    /// THE integration proof: an inbound mesh message actually traverses
    /// `ingest` (it is journaled at the border) and its response actually
    /// traverses an outbound `Router` lane (the adapter publishes it) — no
    /// side-injection, no polling, no second consumer. Uses the REAL control
    /// plane, journal, and Router; only the transport is in-memory.
    #[tokio::test]
    async fn mesh_inbound_traverses_ingest_and_outbound_traverses_router_lane() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_path = dir.path().join("d.jsonl");
        let journal = Arc::new(
            CommandJournal::open(&journal_path, "d", FsyncPolicy::Never).expect("open journal"),
        );
        let router = Router::new();
        // Same Router the adapter registers its lane on: pipeline responses
        // route back to that lane.
        let control = Arc::new(spawn_control_plane(journal, test_ctx(), router.clone()));

        let (tx, rx) = mpsc::unbounded_channel::<MeshInbound>();
        let egress = Arc::new(SinkEgress::default());
        {
            let control = control;
            let router = router.clone();
            let egress = egress.clone();
            tokio::spawn(async move {
                serve_mesh(control, authed_state(), router, rx, egress).await;
            });
        }

        // Push an inbound mesh request — `ping`, a real journaled method that
        // the pipeline answers — with a mesh reply subject.
        let reply_subject = "mesh.reply.abc123";
        tx.send(MeshInbound {
            request: Request {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: json!(7),
                method: "ping".to_string(),
                params: json!(null),
            },
            reply_to: reply_subject.to_string(),
        })
        .expect("send inbound");

        // Outbound must arrive on the egress sink, routed to the reply
        // subject — proving it traversed the Router lane.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let published = loop {
            if let Some(hit) = egress
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|(s, _)| s == reply_subject)
                .cloned()
            {
                break hit;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "response never published to the mesh reply subject"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(published.0, reply_subject);
        assert_eq!(published.1["id"], 7, "reply correlates to the request id");

        // Inbound must have been journaled at the border — proving it crossed
        // `ingest`, not a side channel.
        let (records, _) = CommandJournal::replay(&journal_path).expect("replay");
        let journaled_ping = records.iter().any(|r| {
            matches!(&r.payload, JournalPayload::CommandReceived { method, .. } if method == "ping")
        });
        assert!(
            journaled_ping,
            "inbound mesh command must be journaled by ingest: {records:?}"
        );
    }

    /// Regression (review round 3): the subject multiplexes peers, whose
    /// JSON-RPC ids are independent. Two peers both using `id: 1` must NOT
    /// collide — each reply must reach its OWN reply subject, with its own id
    /// restored. Proves the correlation keys on a unique per-request id, not
    /// the client-chosen one.
    #[tokio::test]
    async fn concurrent_requests_reusing_the_same_id_do_not_misroute() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(
            CommandJournal::open(&dir.path().join("d.jsonl"), "d", FsyncPolicy::Never)
                .expect("open journal"),
        );
        let router = Router::new();
        let control = Arc::new(spawn_control_plane(journal, test_ctx(), router.clone()));

        let (tx, rx) = mpsc::unbounded_channel::<MeshInbound>();
        let egress = Arc::new(SinkEgress::default());
        {
            let router = router.clone();
            let egress = egress.clone();
            tokio::spawn(async move {
                serve_mesh(control, authed_state(), router, rx, egress).await;
            });
        }

        // Two peers, SAME JSON-RPC id, DIFFERENT reply subjects.
        for subject in ["mesh.reply.peerA", "mesh.reply.peerB"] {
            tx.send(MeshInbound {
                request: Request {
                    jsonrpc: JSONRPC_VERSION.to_string(),
                    id: json!(1),
                    method: "ping".to_string(),
                    params: json!(null),
                },
                reply_to: subject.to_string(),
            })
            .expect("send inbound");
        }

        // Both subjects must receive a reply — proving no collision dropped one.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let (a, b) = {
                let hits = egress.0.lock().unwrap();
                (
                    hits.iter().find(|(s, _)| s == "mesh.reply.peerA").cloned(),
                    hits.iter().find(|(s, _)| s == "mesh.reply.peerB").cloned(),
                )
            };
            if let (Some(a), Some(b)) = (a, b) {
                // Each carries the client's own (restored) id, not the internal
                // correlation id, and each landed on its own subject.
                assert_eq!(a.1["id"], 1, "peerA reply id restored");
                assert_eq!(b.1["id"], 1, "peerB reply id restored");
                assert_eq!(a.1["result"]["pong"], true);
                assert_eq!(b.1["result"]["pong"], true);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "both reply subjects must receive a reply; one was misrouted or dropped"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
