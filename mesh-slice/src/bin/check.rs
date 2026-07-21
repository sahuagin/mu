//! mu-mesh-check (mu-wxc4): mesh doctor. Discover the code_index service by
//! NAME on the mesh, run a real recall + status through the proxy, print what
//! came back. Exit 0 = the mesh path works end to end.
//!
//! Same config resolution as the other mesh binaries: env (MU_MESH_*) >
//! `[mesh]` in `~/.config/agent/config.toml` > default. Needs `issuer_key`
//! (mints request capabilities the service must trust).

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use biscuit_auth::builder::Algorithm;
use biscuit_auth::{KeyPair, PrivateKey};

use mesh_slice::config::setting;
use mesh_slice::proxy::CodeIndexProxy;

#[tokio::main]
async fn main() -> Result<()> {
    let nats_url =
        setting("MU_MESH_NATS_URL", "nats_url").unwrap_or_else(|| "127.0.0.1:4222".to_string());
    let issuer_hex = setting("MU_MESH_ISSUER_KEY", "issuer_key")
        .ok_or_else(|| anyhow!("mu-mesh-check: no issuer key configured"))?;
    let root = Arc::new(KeyPair::from(
        &PrivateKey::from_bytes_hex(&issuer_hex, Algorithm::Ed25519)
            .map_err(|e| anyhow!("mesh issuer_key is not a valid hex Ed25519 key: {e}"))?,
    ));

    // 30s request timeout (default 10s): a cold code_index backend
    // (first embedding call) can legitimately take >10s.
    let client = async_nats::ConnectOptions::new()
        .request_timeout(Some(std::time::Duration::from_secs(30)))
        .connect(&nats_url)
        .await
        .with_context(|| format!("connect NATS at {nats_url}"))?;
    let proxy = CodeIndexProxy::new(client, root);

    if !proxy.discover().await? {
        return Err(anyhow!(
            "code_index service NOT discoverable on the mesh (is mu-mesh-svc running?)"
        ));
    }
    println!("discover : code_index is live on the mesh ($SRV)");

    let status = proxy.status().await?;
    println!(
        "status   : healthy={} indexed_repos={}",
        status.healthy, status.indexed_repos
    );

    let query = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "where are sessions built".into());
    let hits = proxy.recall(&query, Some(3)).await?;
    println!("recall   : {} hit(s) for {query:?}", hits.len());
    for h in &hits {
        println!("  {:>6.3}  {}  ({})", h.score, h.symbol, h.path);
    }
    Ok(())
}
