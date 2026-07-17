//! mu-mesh-bridge (mu-wxc4): a stdio MCP server that bridges CC's tool calls
//! onto the NATS service mesh.
//!
//! CC launches this as a subprocess (`.mcp.json` `command=`), speaks MCP 1.0
//! to its stdio, and this bridge relays each tool call onto the mesh via the
//! SAME `CodeIndexProxy` the fleet uses internally — MCP civilized at the
//! edge, NATS native inside. First slice: the `code_index` service
//! (`code_recall` / `code_status`), retiring the hardcoded `ip:port` endpoint.
//!
//! Usage:
//!   mu-mesh-bridge [--nats-url <url>]
//! Env:
//!   MU_MESH_NATS_URL      NATS url (default 127.0.0.1:4222; --nats-url wins)
//!   MU_MESH_ISSUER_KEY    hex Ed25519 private key the bridge mints request
//!                         capabilities with. The mesh services must trust its
//!                         public key. If unset, an EPHEMERAL key is generated
//!                         and its public key logged (dev / single-tenant).

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use biscuit_auth::builder::Algorithm;
use biscuit_auth::{KeyPair, PrivateKey};
use rmcp::ServiceExt;

use mesh_slice::adapter::McpNatsAdapter;
use mesh_slice::proxy::CodeIndexProxy;

#[tokio::main]
async fn main() -> Result<()> {
    let nats_url = arg_or_env("--nats-url", "MU_MESH_NATS_URL")
        .unwrap_or_else(|| "127.0.0.1:4222".to_string());
    let root = issuer_keypair()?;

    let client = async_nats::connect(&nats_url)
        .await
        .with_context(|| format!("mu-mesh-bridge: connect NATS at {nats_url}"))?;
    let proxy = Arc::new(CodeIndexProxy::new(client, Arc::new(root)));
    let adapter = McpNatsAdapter::new(proxy);

    // Serve MCP over this process's stdio — the transport a launched MCP
    // server speaks to its parent (CC). `serve` completes the initialize
    // handshake; `waiting` runs until the client disconnects (stdin EOF).
    eprintln!("mu-mesh-bridge: serving MCP over stdio (mesh at {nats_url})");
    let server = adapter
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
        .map_err(|e| anyhow!("mu-mesh-bridge: serve over stdio: {e}"))?;
    server
        .waiting()
        .await
        .map_err(|e| anyhow!("mu-mesh-bridge: serving ended in error: {e}"))?;
    Ok(())
}

/// The issuer keypair the proxy mints request capabilities with: loaded from
/// `MU_MESH_ISSUER_KEY` (hex Ed25519 private key) if set, else ephemeral.
fn issuer_keypair() -> Result<KeyPair> {
    match std::env::var("MU_MESH_ISSUER_KEY") {
        Ok(hex) if !hex.is_empty() => {
            let pk = PrivateKey::from_bytes_hex(&hex, Algorithm::Ed25519)
                .map_err(|e| anyhow!("MU_MESH_ISSUER_KEY is not a valid hex Ed25519 key: {e}"))?;
            Ok(KeyPair::from(&pk))
        }
        _ => {
            let kp = KeyPair::new();
            eprintln!(
                "mu-mesh-bridge: no MU_MESH_ISSUER_KEY — generated an EPHEMERAL issuer key; \
                 public = {} (mesh services must trust this key to accept requests)",
                kp.public().to_bytes_hex()
            );
            Ok(kp)
        }
    }
}

/// First `<flag> <value>` pair on the command line, else the env var, else None.
fn arg_or_env(flag: &str, env: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
    }
    std::env::var(env).ok().filter(|v| !v.is_empty())
}
