//! mu-mesh-svc (mu-wxc4): run the mesh `code_index` service as a process.
//!
//! Tests start the service in-process; this is the deployable equivalent —
//! connect to NATS, register the `code_index` NATS Micro service backed by the
//! real code_index MCP server, and serve until killed.
//!
//! Config resolution (fleet convention): **env (MU_MESH_*) > `[mesh]` section
//! of `~/.config/agent/config.toml` > default**.
//!   nats_url        — env `MU_MESH_NATS_URL`, key `nats_url`
//!                     (default 127.0.0.1:4222)
//!   code_index_url  — env `MU_MESH_CODE_INDEX_URL`, key `code_index_url`
//!                     (REQUIRED: the real code_index MCP endpoint)
//!   issuer_key      — env `MU_MESH_ISSUER_KEY`, key `issuer_key`
//!                     (REQUIRED: hex Ed25519 private key; the service trusts
//!                     its PUBLIC half — the same key the bridge mints with.
//!                     Single-operator model; per-request grants are mu-iqo8.)

use anyhow::{anyhow, Context, Result};
use biscuit_auth::builder::Algorithm;
use biscuit_auth::{KeyPair, PrivateKey};

use mesh_slice::config::setting;
use mesh_slice::service::{self, Backend};

#[tokio::main]
async fn main() -> Result<()> {
    let nats_url =
        setting("MU_MESH_NATS_URL", "nats_url").unwrap_or_else(|| "127.0.0.1:4222".to_string());
    let code_index_url = setting("MU_MESH_CODE_INDEX_URL", "code_index_url").ok_or_else(|| {
        anyhow!("mu-mesh-svc: no code_index endpoint (set MU_MESH_CODE_INDEX_URL or [mesh] code_index_url)")
    })?;
    let issuer_hex = setting("MU_MESH_ISSUER_KEY", "issuer_key").ok_or_else(|| {
        anyhow!(
            "mu-mesh-svc: no issuer key (set MU_MESH_ISSUER_KEY or [mesh] issuer_key) — \
                 the service must trust a stable key; an ephemeral one would reject every peer"
        )
    })?;
    let issuer = KeyPair::from(
        &PrivateKey::from_bytes_hex(&issuer_hex, Algorithm::Ed25519)
            .map_err(|e| anyhow!("mesh issuer_key is not a valid hex Ed25519 key: {e}"))?,
    )
    .public();

    let client = async_nats::connect(&nats_url)
        .await
        .with_context(|| format!("mu-mesh-svc: connect NATS at {nats_url}"))?;
    let _service = service::start(
        client,
        issuer,
        Backend::CodeIndexMcp {
            url: code_index_url.clone(),
        },
    )
    .await?;

    eprintln!(
        "mu-mesh-svc: code_index service live on the mesh (nats {nats_url}, backend {code_index_url})"
    );
    // Serve until killed; the Service handle must stay alive (dropping it
    // tears down the endpoints).
    tokio::signal::ctrl_c().await.ok();
    eprintln!("mu-mesh-svc: shutting down");
    Ok(())
}
