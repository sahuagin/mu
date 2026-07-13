//! Optional etcd-lease presence for the daemon's live sessions
//! (`specs/plans/mu-dialogue-push-mailbox-v1.md` §1, bead
//! mu-dialogue-presence-etcd — the mu-daemon slice; cc peers got theirs in
//! agent_tools#48, the mu-dialogue server read side in mu#482).
//!
//! One lease per daemon, one reconciliation task: every TTL/3 the task
//! keepalives the lease and syncs the presence keyspace against the LIVE
//! session registry — `mu:<daemon_id>:<session_id>` per live session plus a
//! daemon key. No per-lifecycle hooks: any create/remove path (spawned
//! workers included) is picked up on the next tick, and a crashed daemon's
//! keys all vanish when the single lease expires — the lease IS the liveness
//! proof. Rehydrated read-only ghosts are deliberately NOT registered.
//!
//! **Strictly opt-in** (operator requirement: adopting mu must not require
//! etcd): runs only when the shared mu config has
//!
//! ```toml
//! [dialogue.presence]
//! enabled = true
//! etcd    = ["http://<etcd-host>:2379"]
//! ```
//!
//! mu-core carries that section opaquely (`Config::dialogue`, mu#485); this
//! module is its only in-daemon interpreter. Absent/disabled → nothing is
//! spawned, no network is touched. **Fail-open**: etcd trouble degrades to
//! activity-derived presence (the dialogue server's compatibility path) and
//! retries on the keepalive cadence; it never affects sessions themselves.
//!
//! Transport is etcd's v3 JSON gateway over reqwest (already in the dep
//! tree via rmcp's reqwest transport) — no gRPC/tonic, hence no cargo
//! feature: the compile-cost delta is nil.

use std::collections::HashSet;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use super::sessions::Sessions;

const DEFAULT_PREFIX: &str = "/mu/dialogue/v1/peers/";
const DEFAULT_TTL_S: u64 = 60;
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub struct PresenceConfig {
    pub etcd: Vec<String>,
    pub prefix: String,
    pub ttl_s: u64,
}

/// Parse `[dialogue.presence]` out of the opaque `[dialogue]` section
/// mu-core carries. Every "not configured" shape (no section, enabled
/// false/missing, no endpoints) returns None = presence stays off.
pub fn from_config(dialogue: Option<&toml::Value>) -> Option<PresenceConfig> {
    let p = dialogue?.get("presence")?;
    if !p
        .get("enabled")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let etcd: Vec<String> = p
        .get("etcd")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if etcd.is_empty() {
        return None;
    }
    let prefix = p
        .get("prefix")
        .and_then(toml::Value::as_str)
        .unwrap_or(DEFAULT_PREFIX)
        .to_string();
    let ttl_s = p
        .get("lease_ttl_seconds")
        .and_then(toml::Value::as_integer)
        .filter(|t| *t >= 5)
        .map(|t| t as u64)
        .unwrap_or(DEFAULT_TTL_S);
    Some(PresenceConfig {
        etcd,
        prefix,
        ttl_s,
    })
}

/// The peer ids this daemon should have registered right now: one per live
/// session, plus the daemon itself. Pure — the reconciliation diff against
/// it is what the sync task executes.
fn desired_peer_ids(daemon_id: &str, live_session_ids: &[String]) -> HashSet<String> {
    let mut set: HashSet<String> = live_session_ids
        .iter()
        .map(|sid| format!("mu:{daemon_id}:{sid}"))
        .collect();
    set.insert(format!("mu:{daemon_id}"));
    set
}

/// POST one JSON-gateway call against the first endpoint that answers.
async fn etcd_post(
    client: &reqwest::Client,
    cfg: &PresenceConfig,
    path: &str,
    body: &Value,
) -> Option<Value> {
    for ep in &cfg.etcd {
        let url = format!("{ep}{path}");
        let resp = client
            .post(&url)
            .timeout(CALL_TIMEOUT)
            .json(body)
            .send()
            .await;
        if let Ok(r) = resp {
            if r.status().is_success() {
                if let Ok(v) = r.json::<Value>().await {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// etcd's gateway emits int64s as JSON strings; tolerate both.
fn id_of(v: &Value, field: &str) -> Option<String> {
    match v.get(field)? {
        Value::String(s) if !s.is_empty() && s != "0" => Some(s.clone()),
        Value::Number(n) if n.as_i64().unwrap_or(0) != 0 => Some(n.to_string()),
        _ => None,
    }
}

async fn lease_grant(client: &reqwest::Client, cfg: &PresenceConfig) -> Option<String> {
    id_of(
        &etcd_post(client, cfg, "/v3/lease/grant", &json!({"TTL": cfg.ttl_s})).await?,
        "ID",
    )
}

/// Refresh the lease; false means it is gone and the caller re-grants.
async fn lease_keepalive(client: &reqwest::Client, cfg: &PresenceConfig, lease: &str) -> bool {
    let Some(resp) = etcd_post(client, cfg, "/v3/lease/keepalive", &json!({"ID": lease})).await
    else {
        return false;
    };
    resp.get("result")
        .and_then(|r| r.get("TTL"))
        .map(|t| match t {
            Value::String(s) => s.parse::<i64>().unwrap_or(0) > 0,
            Value::Number(n) => n.as_i64().unwrap_or(0) > 0,
            _ => false,
        })
        .unwrap_or(false)
}

async fn put_peer(
    client: &reqwest::Client,
    cfg: &PresenceConfig,
    lease: &str,
    daemon_id: &str,
    peer_id: &str,
) -> bool {
    let b64 = base64::engine::general_purpose::STANDARD;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let value = json!({
        "peer_id": peer_id, "role": "mu", "daemon_id": daemon_id,
        "registered_at_unix_ms": now_ms,
    });
    etcd_post(
        client,
        cfg,
        "/v3/kv/put",
        &json!({
            "key": b64.encode(format!("{}{}", cfg.prefix, peer_id)),
            "value": b64.encode(value.to_string()),
            "lease": lease,
        }),
    )
    .await
    .is_some()
}

async fn delete_peer(client: &reqwest::Client, cfg: &PresenceConfig, peer_id: &str) -> bool {
    let b64 = base64::engine::general_purpose::STANDARD;
    etcd_post(
        client,
        cfg,
        "/v3/kv/deleterange",
        &json!({ "key": b64.encode(format!("{}{}", cfg.prefix, peer_id)) }),
    )
    .await
    .is_some()
}

/// Spawn the daemon's presence reconciliation task. Never fails, never
/// blocks the caller; all etcd trouble is retried on the next tick.
pub fn spawn(cfg: PresenceConfig, daemon_id: String, sessions: Sessions) {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let tick = Duration::from_secs((cfg.ttl_s / 3).max(2));
        let mut lease: Option<String> = None;
        // Peer ids currently registered under OUR lease. Cleared whenever the
        // lease is re-granted (a new lease starts with no keys attached).
        let mut registered: HashSet<String> = HashSet::new();
        let mut announced = false;
        loop {
            // 1. Ensure the lease is alive.
            let alive = match &lease {
                Some(l) => lease_keepalive(&client, &cfg, l).await,
                None => false,
            };
            if !alive {
                lease = lease_grant(&client, &cfg).await;
                registered.clear();
                match (&lease, announced) {
                    (Some(_), false) => {
                        tracing::info!(prefix = %cfg.prefix, "dialogue presence: lease-registered");
                        announced = true;
                    }
                    (None, false) => {
                        tracing::warn!("dialogue presence: etcd unavailable; retrying (fail-open)");
                        announced = true;
                    }
                    _ => {}
                }
            }
            // 2. Reconcile keys against the live registry.
            if let Some(l) = lease.clone() {
                let live = sessions.live_session_ids();
                let desired = desired_peer_ids(&daemon_id, &live);
                for peer in desired.difference(&registered.clone()) {
                    if put_peer(&client, &cfg, &l, &daemon_id, peer).await {
                        registered.insert(peer.clone());
                    }
                }
                for peer in registered.clone().difference(&desired) {
                    if delete_peer(&client, &cfg, peer).await {
                        registered.remove(peer);
                    }
                }
            }
            tokio::time::sleep(tick).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(s: &str) -> toml::Value {
        s.parse().unwrap()
    }

    #[test]
    fn from_config_gating_shapes() {
        // No section / disabled / no endpoints → None (presence stays off).
        assert!(from_config(None).is_none());
        assert!(from_config(Some(&section("x = 1"))).is_none());
        assert!(from_config(Some(&section(
            "[presence]\nenabled = false\netcd = [\"http://a:2379\"]"
        )))
        .is_none());
        assert!(from_config(Some(&section("[presence]\nenabled = true"))).is_none());

        // Enabled with endpoints → parsed, defaults applied.
        let cfg = from_config(Some(&section(
            "[presence]\nenabled = true\netcd = [\"http://a:2379/\"]",
        )))
        .unwrap();
        assert_eq!(cfg.etcd, vec!["http://a:2379"]); // trailing slash trimmed
        assert_eq!(cfg.prefix, DEFAULT_PREFIX);
        assert_eq!(cfg.ttl_s, DEFAULT_TTL_S);

        // Overrides honored; sub-minimum TTL rejected back to default.
        let cfg = from_config(Some(&section(
            "[presence]\nenabled = true\netcd = [\"http://a:2379\"]\nprefix = \"/p/\"\nlease_ttl_seconds = 30",
        )))
        .unwrap();
        assert_eq!(cfg.prefix, "/p/");
        assert_eq!(cfg.ttl_s, 30);
        let cfg = from_config(Some(&section(
            "[presence]\nenabled = true\netcd = [\"http://a:2379\"]\nlease_ttl_seconds = 1",
        )))
        .unwrap();
        assert_eq!(cfg.ttl_s, DEFAULT_TTL_S);
    }

    #[test]
    fn desired_set_is_sessions_plus_daemon() {
        let ids = vec!["s1".to_string(), "s2".to_string()];
        let want = desired_peer_ids("d1", &ids);
        assert_eq!(want.len(), 3);
        assert!(want.contains("mu:d1:s1"));
        assert!(want.contains("mu:d1:s2"));
        assert!(want.contains("mu:d1"));
        // No sessions → just the daemon key.
        assert_eq!(desired_peer_ids("d1", &[]).len(), 1);
    }

    #[test]
    fn id_of_tolerates_string_and_number() {
        assert_eq!(id_of(&json!({"ID": "7"}), "ID"), Some("7".to_string()));
        assert_eq!(id_of(&json!({"ID": 7}), "ID"), Some("7".to_string()));
        assert_eq!(id_of(&json!({"ID": "0"}), "ID"), None);
        assert_eq!(id_of(&json!({}), "ID"), None);
    }
}
