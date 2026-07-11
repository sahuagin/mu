//! Optional etcd-lease presence — the registration model from
//! `specs/plans/mu-dialogue-push-mailbox-v1.md` §1 (bead mu-dialogue-presence-etcd).
//!
//! A consumer registers ITS OWN mailbox: a key under the presence prefix, held
//! by an etcd lease. The lease IS the liveness proof — it auto-expires on
//! death, so a key's existence means the peer is live *now* (no timestamps, no
//! TTL heuristics). This module is the server's READ side: it lists lease-live
//! peers so `dialogue_peers` can report them as authoritative and
//! `dialogue_broadcast` can address them. Clients write their own keys (mu
//! daemon per session; the cc Stop-hook watch process for Claude Code peers).
//!
//! **Strictly opt-in.** Presence is enabled only by
//!
//! ```toml
//! # ~/.config/mu/config.toml
//! [dialogue.presence]
//! enabled = true
//! etcd    = ["http://10.1.1.172:2379"]        # endpoints, tried in order
//! # prefix = "/mu/dialogue/v1/peers/"         # default
//! ```
//!
//! With the section absent or `enabled = false`, mu-dialogue behaves exactly
//! as before (activity-derived presence + the TTL sweep) and never touches the
//! network — someone trying out mu does not need etcd installed.
//!
//! **Fail-open.** If etcd is unreachable at call time the server logs and
//! falls back to activity-derived presence for that call (same convention as
//! with-ollama-lease): a monitoring outage must not take down messaging.
//!
//! Transport is etcd's v3 JSON gateway (`POST /v3/kv/range`, base64 keys) over
//! the workspace's existing reqwest — no gRPC/tonic dependency.

use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

pub const DEFAULT_PREFIX: &str = "/mu/dialogue/v1/peers/";
const ETCD_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// `[dialogue.presence]` from the mu config. Deserialized leniently: unknown
/// fields are ignored so the section can grow (lease_ttl_seconds is a client
/// concern the server doesn't read).
#[derive(Debug, Clone, Deserialize)]
pub struct PresenceConfig {
    #[serde(default)]
    pub enabled: bool,
    /// etcd endpoints, tried in order until one answers.
    #[serde(default)]
    pub etcd: Vec<String>,
    #[serde(default = "default_prefix")]
    pub prefix: String,
}

fn default_prefix() -> String {
    DEFAULT_PREFIX.to_string()
}

/// Load `[dialogue.presence]` from a mu config.toml. Returns None (presence
/// disabled) when the file, the section, or `enabled = true` is missing, or
/// when enabled without endpoints — every "not configured" shape means "run
/// exactly as before".
pub fn load(path: &std::path::Path) -> Option<PresenceConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let root: toml::Value = text.parse().ok()?;
    let section = root.get("dialogue")?.get("presence")?.clone();
    let cfg: PresenceConfig = section.try_into().ok()?;
    if !cfg.enabled || cfg.etcd.is_empty() {
        return None;
    }
    Some(cfg)
}

/// Default config path: `$MU_CONFIG` or `~/.config/mu/config.toml`.
pub fn default_config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("MU_CONFIG") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".config/mu/config.toml")
}

/// One lease-live peer, parsed from its etcd key/value. The key suffix (after
/// the prefix) is the peer id; the value is the registration JSON from the
/// spec (`{"peer_id","role",...}`) but only advisory — a malformed value still
/// counts as a live peer (the LEASE is the truth, not the payload).
#[derive(Debug, Clone, PartialEq)]
pub struct LeasePeer {
    pub peer_id: String,
    pub role: String,
    /// registered_at_unix_ms from the value, when present.
    pub registered_at: Option<i64>,
}

/// The exclusive upper bound for a prefix range query: prefix with its last
/// byte incremented (etcd's standard prefix-scan idiom).
fn prefix_range_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last < 0xff {
            *last += 1;
            return end;
        }
        end.pop();
    }
    // All 0xff (or empty): scan to the end of the keyspace.
    vec![0]
}

fn parse_kv(prefix: &str, kv: &Value) -> Option<LeasePeer> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let key_raw = b64.decode(kv.get("key")?.as_str()?).ok()?;
    let key = String::from_utf8(key_raw).ok()?;
    let peer_id = key.strip_prefix(prefix)?.to_string();
    if peer_id.is_empty() {
        return None;
    }
    // The value payload is advisory; the lease-held key alone proves liveness.
    let payload: Option<Value> = kv
        .get("value")
        .and_then(Value::as_str)
        .and_then(|v| b64.decode(v).ok())
        .and_then(|raw| serde_json::from_slice(&raw).ok());
    let role = payload
        .as_ref()
        .and_then(|p| p.get("role"))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| {
            peer_id
                .split(':')
                .next()
                .unwrap_or(peer_id.as_str())
                .to_string()
        });
    let registered_at = payload
        .as_ref()
        .and_then(|p| p.get("registered_at_unix_ms"))
        .and_then(Value::as_i64);
    Some(LeasePeer {
        peer_id,
        role,
        registered_at,
    })
}

/// List the lease-live peers: a prefix range over the presence keyspace.
/// Every key returned is held by an unexpired lease, so every entry is live
/// right now. Tries each endpoint in order; errors only if all fail (callers
/// fail open).
pub async fn lease_peers(client: &reqwest::Client, cfg: &PresenceConfig) -> Result<Vec<LeasePeer>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let body = serde_json::json!({
        "key": b64.encode(cfg.prefix.as_bytes()),
        "range_end": b64.encode(prefix_range_end(cfg.prefix.as_bytes())),
    });
    let mut last_err = None;
    for ep in &cfg.etcd {
        let url = format!("{}/v3/kv/range", ep.trim_end_matches('/'));
        let resp = client
            .post(&url)
            .timeout(ETCD_CALL_TIMEOUT)
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let v: Value = r.json().await.context("etcd range: decode response")?;
                let peers = v
                    .get("kvs")
                    .and_then(Value::as_array)
                    .map(|kvs| {
                        kvs.iter()
                            .filter_map(|kv| parse_kv(&cfg.prefix, kv))
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(peers);
            }
            Ok(r) => last_err = Some(anyhow::anyhow!("etcd {url}: HTTP {}", r.status())),
            Err(e) => last_err = Some(anyhow::Error::new(e).context(format!("etcd {url}"))),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no etcd endpoints configured")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_end_increments_last_byte() {
        assert_eq!(prefix_range_end(b"/mu/"), b"/mu0".to_vec());
        assert_eq!(prefix_range_end(b"a\xff"), b"b".to_vec());
        assert_eq!(prefix_range_end(b"\xff"), vec![0]);
    }

    #[test]
    fn config_absent_or_disabled_means_none() {
        let dir = std::env::temp_dir().join(format!("mu-dlg-presence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No file.
        assert!(load(&dir.join("missing.toml")).is_none());
        // File without the section.
        let p = dir.join("nosection.toml");
        std::fs::write(&p, "[other]\nx = 1\n").unwrap();
        assert!(load(&p).is_none());
        // Section disabled.
        let p = dir.join("disabled.toml");
        std::fs::write(
            &p,
            "[dialogue.presence]\nenabled = false\netcd = [\"http://x:2379\"]\n",
        )
        .unwrap();
        assert!(load(&p).is_none());
        // Enabled but no endpoints → still disabled.
        let p = dir.join("noeps.toml");
        std::fs::write(&p, "[dialogue.presence]\nenabled = true\n").unwrap();
        assert!(load(&p).is_none());
    }

    #[test]
    fn config_enabled_parses_with_defaults() {
        let dir = std::env::temp_dir().join(format!("mu-dlg-presence2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("on.toml");
        std::fs::write(
            &p,
            "[dialogue.presence]\nenabled = true\netcd = [\"http://10.0.0.1:2379\"]\n",
        )
        .unwrap();
        let cfg = load(&p).unwrap();
        assert_eq!(cfg.etcd, vec!["http://10.0.0.1:2379"]);
        assert_eq!(cfg.prefix, DEFAULT_PREFIX);
    }

    #[test]
    fn parse_kv_derives_peer_and_role() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let key = b64.encode(format!("{DEFAULT_PREFIX}cc:abc"));
        // Value present with role.
        let kv = serde_json::json!({
            "key": key,
            "value": b64.encode(r#"{"peer_id":"cc:abc","role":"cc","registered_at_unix_ms":123}"#),
        });
        let p = parse_kv(DEFAULT_PREFIX, &kv).unwrap();
        assert_eq!(p.peer_id, "cc:abc");
        assert_eq!(p.role, "cc");
        assert_eq!(p.registered_at, Some(123));
        // Malformed value: the lease-held key still counts; role from the id.
        let kv = serde_json::json!({
            "key": b64.encode(format!("{DEFAULT_PREFIX}mu:d:s")),
            "value": b64.encode("not-json"),
        });
        let p = parse_kv(DEFAULT_PREFIX, &kv).unwrap();
        assert_eq!(p.peer_id, "mu:d:s");
        assert_eq!(p.role, "mu");
        // Key outside the prefix is ignored.
        let kv = serde_json::json!({ "key": b64.encode("/elsewhere/x"), "value": "" });
        assert!(parse_kv(DEFAULT_PREFIX, &kv).is_none());
    }
}
