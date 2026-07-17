//! mu-mesh-bridge, live: launch the ACTUAL bridge binary the way CC does — as
//! an MCP subprocess over stdio — and call a mesh service THROUGH it. Proves
//! the deployable artifact, not just the in-crate adapter: CC → (MCP/stdio) →
//! mu-mesh-bridge → (NATS request/reply) → code_index service → typed hits back.

use std::time::Duration;

use biscuit_auth::KeyPair;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;

use mesh_slice::contract::Hit;
use mesh_slice::service;

/// Spawn a throwaway nats-server: `$NATS_BIN` if set, else `nats-server` on
/// PATH. Returns None (skip) if it cannot be spawned.
#[allow(clippy::zombie_processes)]
async fn spawn_nats(port: u16) -> Option<(std::process::Child, String)> {
    let bin = std::env::var("NATS_BIN").unwrap_or_else(|_| "nats-server".to_string());
    let store = format!("target/nats-js-{port}");
    let _ = std::fs::remove_dir_all(&store);
    let child = match std::process::Command::new(&bin)
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
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping bridge test: cannot spawn nats-server ({bin}): {e}");
            return None;
        }
    };
    let url = format!("127.0.0.1:{port}");
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(&url).await.is_ok() {
            return Some((child, url));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("nats-server on {url} never accepted connections");
}

#[tokio::test]
async fn bridge_binary_serves_code_recall_over_stdio() {
    let Some((mut nats, url)) = spawn_nats(14326).await else {
        return; // no nats-server on this host
    };

    // The issuer both sides share: the service verifies against its public key;
    // the bridge (via MU_MESH_ISSUER_KEY) mints against its private key.
    let root = KeyPair::new();
    let issuer_hex = root.private().to_bytes_hex();

    let svc_client = async_nats::connect(&url).await.expect("connect nats");
    let _service = service::start(svc_client, root.public())
        .await
        .expect("start code_index service");

    // Launch the bridge EXACTLY as CC would: the built binary, over stdio.
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_mu-mesh-bridge"));
    cmd.arg("--nats-url")
        .arg(&url)
        .env("MU_MESH_ISSUER_KEY", &issuer_hex);
    let cc =
        ().serve(TokioChildProcess::new(cmd).expect("spawn bridge subprocess"))
            .await
            .expect("MCP connect to bridge over stdio");

    // CC calls code_recall over MCP; the answer comes from the mesh service.
    let mut args = serde_json::Map::new();
    args.insert(
        "query".into(),
        serde_json::json!("where are sessions built"),
    );
    args.insert("limit".into(), serde_json::json!(2));
    let result = cc
        .call_tool(CallToolRequestParams::new("code_recall").with_arguments(args))
        .await
        .expect("call_tool code_recall through the bridge");

    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .expect("tool result text");
    let hits: Vec<Hit> = serde_json::from_str(&text).expect("decode bridged hits");
    assert_eq!(
        hits.len(),
        2,
        "mesh hits relayed through the bridge: {hits:?}"
    );
    assert!(hits[0].symbol.contains("where_are_sessions_built"));

    cc.cancel().await.ok();
    nats.kill().ok();
    nats.wait().ok();
}
