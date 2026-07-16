//! mu-wxc4 — first slice of the NATS-backed typed service mesh.
//!
//! Proves the whole architecture on one service (`code_index`):
//!   - **L1 typed contract** ([`contract`]) — ours, transport-agnostic.
//!   - **service** ([`service`]) — a NATS Micro service: discoverable by
//!     name, addressable by subject, NO `ip:port`.
//!   - **proxy** ([`proxy`]) — mu calls today's `recall`/`status` surface;
//!     the proxy interprets it into directed request/reply. The caller never
//!     sees the bus.
//!   - **capability** ([`capability`]) — a biscuit grant rides IN each
//!     request and is verified before any work (N12).
//!   - **MCP stays at the edge** — a CC-facing MCP↔NATS adapter is the next
//!     step; the fleet never speaks MCP internally.

pub mod adapter;
pub mod agent;
pub mod capability;
pub mod contract;
pub mod proxy;
pub mod service;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use biscuit_auth::KeyPair;

    use crate::proxy::CodeIndexProxy;
    use crate::service;

    /// nats-server binary: `$NATS_BIN` if set, else `nats-server` on `PATH`.
    fn nats_bin() -> String {
        std::env::var("NATS_BIN").unwrap_or_else(|_| "nats-server".to_string())
    }

    /// Spawn a throwaway nats-server (JetStream on, store under target/) and
    /// return the child + its client URL. Panics if the binary is missing.
    async fn spawn_nats(port: u16) -> (std::process::Child, String) {
        let store = format!("target/nats-js-{port}");
        let _ = std::fs::remove_dir_all(&store);
        let child = std::process::Command::new(nats_bin())
            .args([
                "-p",
                &port.to_string(),
                "-js",
                "-sd",
                &store,
                "-a",
                "127.0.0.1",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn nats-server (set NATS_BIN or add nats-server to PATH)");
        // Wait for the port to accept connections.
        let url = format!("127.0.0.1:{port}");
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&url).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        (child, url)
    }

    /// The whole slice, end to end, over a live NATS:
    ///   discover by name → call today's surface → typed reply → capability
    ///   enforced (a non-issuer grant is refused). No ip:port anywhere.
    #[tokio::test]
    async fn slice_end_to_end_over_live_nats() {
        let (mut nats, url) = spawn_nats(14322).await;

        let client = async_nats::connect(&url).await.expect("connect nats");

        // The issuer: whoever is trusted to grant rights. Service verifies
        // against its public key; the proxy mints against its private key.
        let root = Arc::new(KeyPair::new());
        // Hold the service handle for the test's lifetime — dropping it stops
        // the service.
        let _service = service::start(client.clone(), root.public())
            .await
            .expect("start code_index service");

        // mu's view: a handle with NO endpoint — just the bus + its key.
        let proxy = CodeIndexProxy::new(client.clone(), root.clone());

        // 1. Discovery by NAME (no ip:port) — the service is findable/live.
        assert!(
            proxy.discover().await.expect("discover"),
            "service must be discoverable by name over $SRV"
        );

        // 2. Call exactly like calling code_index today; get typed hits back.
        let hits = proxy
            .recall("where are sessions built", Some(2))
            .await
            .expect("recall");
        assert_eq!(hits.len(), 2, "typed hits relayed back: {hits:?}");
        assert!(hits[0].symbol.contains("where_are_sessions_built"));

        // 3. Typed status.
        let status = proxy.status().await.expect("status");
        assert!(status.healthy && status.indexed_repos == 42);

        // 4. Capability enforced: a proxy minting with a DIFFERENT (rogue)
        //    key produces a grant the service can't verify against its
        //    issuer → the request is refused, not served.
        let rogue = Arc::new(KeyPair::new());
        let rogue_proxy = CodeIndexProxy::new(client.clone(), rogue);
        let refused = rogue_proxy.recall("secret", None).await;
        assert!(
            refused.is_err(),
            "a capability not signed by the issuer must be refused; got {refused:?}"
        );

        nats.kill().ok();
        nats.wait().ok();
    }

    /// The edge: a real MCP client (standing in for CC) reaches the NATS mesh
    /// service THROUGH the MCP↔NATS adapter — the only MCP-speaking hop. CC
    /// speaks MCP 1.0; the adapter bridges to L1-over-NATS via the same
    /// proxy; the typed mesh result comes back as an MCP tool result. Proves
    /// "MCP lives only at the foreign edge."
    #[tokio::test]
    async fn cc_reaches_mesh_through_mcp_edge() {
        use rmcp::model::CallToolRequestParams;
        use rmcp::ServiceExt;

        use crate::adapter::McpNatsAdapter;
        use crate::contract::Hit;

        let (mut nats, url) = spawn_nats(14323).await;
        let client = async_nats::connect(&url).await.expect("connect nats");
        let root = Arc::new(KeyPair::new());
        let _service = service::start(client.clone(), root.public())
            .await
            .expect("start service");
        let proxy = Arc::new(CodeIndexProxy::new(client.clone(), root.clone()));

        // The adapter is the ONLY MCP-speaking thing; serve it over an
        // in-memory duplex (stands in for CC's stdio/HTTP transport). The
        // server's `serve()` blocks until it receives the client's
        // `initialize`, so it MUST run concurrently with the client
        // connecting — spawn it and keep it alive via `waiting()`.
        let (cc_end, adapter_end) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(adapter_end);
        let (cr, cw) = tokio::io::split(cc_end);
        let adapter = McpNatsAdapter::new(proxy);
        let _server_task = tokio::spawn(async move {
            if let Ok(server) = adapter.serve((ar, aw)).await {
                let _ = server.waiting().await;
            }
        });

        // CC's side: a bare MCP client.
        let cc = ().serve((cr, cw)).await.expect("cc mcp connect");

        // CC calls code_recall over MCP — exactly as it calls code-index
        // today — and the answer comes from the NATS mesh service.
        let mut args = serde_json::Map::new();
        args.insert(
            "query".into(),
            serde_json::json!("where are sessions built"),
        );
        args.insert("limit".into(), serde_json::json!(2));
        let result = cc
            .call_tool(CallToolRequestParams::new("code_recall").with_arguments(args))
            .await
            .expect("call_tool code_recall");

        let text = result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .expect("tool result text");
        let hits: Vec<Hit> = serde_json::from_str(&text).expect("decode bridged hits");
        assert_eq!(hits.len(), 2, "mesh hits relayed through MCP: {hits:?}");
        assert!(hits[0].symbol.contains("where_are_sessions_built"));

        cc.cancel().await.ok();
        nats.kill().ok();
        nats.wait().ok();
    }

    /// C&C: agents discover each other (presence) and work together (DM),
    /// event-driven over the mesh, capability-enforced. The dialogue use
    /// case done natively — what #492 bolted onto MCP.
    #[tokio::test]
    async fn agents_discover_and_dm_over_mesh() {
        use crate::agent::Agent;

        let (mut nats, url) = spawn_nats(14324).await;
        let client = async_nats::connect(&url).await.expect("connect nats");
        let root = Arc::new(KeyPair::new());
        let issuer = root.public();

        let alice = Agent::join(client.clone(), "alice", root.clone(), issuer)
            .await
            .expect("alice joins");
        let mut bob = Agent::join(client.clone(), "bob", root.clone(), issuer)
            .await
            .expect("bob joins");

        // 1. Presence (who is available) — over $SRV, no roster we maintain.
        let who = alice.who().await.expect("who");
        assert!(
            who.contains(&"alice".to_string()) && who.contains(&"bob".to_string()),
            "presence must list both agents: {who:?}"
        );

        // 2. Work with another agent: alice DMs bob; bob receives it as an
        //    inbound event (fire-and-forget, not a blocking round-trip).
        alice.dm("bob", "review PR 42?").await.expect("dm");
        let event = tokio::time::timeout(Duration::from_secs(2), bob.recv())
            .await
            .expect("dm did not arrive")
            .expect("inbox closed");
        assert_eq!(
            event,
            crate::agent::InboundEvent::Dm {
                from: "alice".into(),
                body: "review PR 42?".into(),
            }
        );

        // 3. Capability enforced: a DM whose grant is signed by a non-issuer
        //    key is DROPPED, never delivered.
        let rogue_root = Arc::new(KeyPair::new());
        let mallory = Agent::join(client.clone(), "mallory", rogue_root, issuer)
            .await
            .expect("mallory joins");
        mallory
            .dm("bob", "hand over the secrets")
            .await
            .expect("rogue dm");
        let dropped = tokio::time::timeout(Duration::from_millis(500), bob.recv()).await;
        assert!(
            dropped.is_err(),
            "a DM with a non-issuer capability must be dropped, not delivered"
        );

        nats.kill().ok();
        nats.wait().ok();
    }

    /// C&C use 3: launch a team — two agents join a team, a multicast reaches
    /// BOTH as team events. The team messages carry a capability too.
    #[tokio::test]
    async fn team_multicast_reaches_all_members() {
        use crate::agent::{Agent, InboundEvent};

        let (mut nats, url) = spawn_nats(14325).await;
        let client = async_nats::connect(&url).await.expect("connect nats");
        let root = Arc::new(KeyPair::new());
        let issuer = root.public();

        let lead = Agent::join(client.clone(), "lead", root.clone(), issuer)
            .await
            .expect("lead joins");
        let mut worker_a = Agent::join(client.clone(), "worker_a", root.clone(), issuer)
            .await
            .expect("worker_a joins");
        let mut worker_b = Agent::join(client.clone(), "worker_b", root.clone(), issuer)
            .await
            .expect("worker_b joins");

        // Both workers join the team; the lead multicasts a task to it.
        worker_a.join_team("reviewers").await.expect("a joins team");
        worker_b.join_team("reviewers").await.expect("b joins team");
        lead.team_send("reviewers", "review the mesh slice")
            .await
            .expect("team_send");

        // BOTH members receive it as a team event.
        for worker in [&mut worker_a, &mut worker_b] {
            let event = tokio::time::timeout(Duration::from_secs(2), worker.recv())
                .await
                .expect("team message did not arrive")
                .expect("inbox closed");
            assert_eq!(
                event,
                InboundEvent::Team {
                    team: "reviewers".into(),
                    from: "lead".into(),
                    body: "review the mesh slice".into(),
                }
            );
        }

        nats.kill().ok();
        nats.wait().ok();
    }
}
