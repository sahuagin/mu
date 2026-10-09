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
//! # label_max_chars = 64                       # default; the cc holder reads it too
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
    /// Longest label reported, in characters. The cc presence holder reads
    /// the same key, so writer and reader cut at the same place. A value that
    /// is not a non-negative integer is logged and replaced by the default:
    /// this setting only shapes a label, so it must never take presence down
    /// with it, which a failed deserialize of the whole section would.
    #[serde(
        default = "default_label_max_chars",
        deserialize_with = "label_max_chars_or_default"
    )]
    pub label_max_chars: usize,
}

fn default_prefix() -> String {
    DEFAULT_PREFIX.to_string()
}

fn default_label_max_chars() -> usize {
    DEFAULT_LABEL_MAX_CHARS
}

fn label_max_chars_or_default<'de, D>(d: D) -> std::result::Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = toml::Value::deserialize(d)?;
    match raw.as_integer().and_then(|n| usize::try_from(n).ok()) {
        Some(n) => Ok(n),
        None => {
            tracing::warn!(
                "[dialogue.presence] label_max_chars = {raw} is not a non-negative integer; \
                 using {DEFAULT_LABEL_MAX_CHARS}"
            );
            Ok(DEFAULT_LABEL_MAX_CHARS)
        }
    }
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

fn home_config(rel: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(rel)
}

/// Does this file define the nested table named by `keys`?
fn defines(path: &std::path::Path, keys: &[&str]) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(root) = text.parse::<toml::Value>() else {
        return false;
    };
    let mut node = &root;
    for k in keys {
        match node.get(k) {
            Some(next) => node = next,
            None => return false,
        }
    }
    true
}

/// Where to read a top-level section from, same rules as
/// [`config_path_for`]. `[mesh]` is genuinely mu-specific, so it normally
/// stays in the mu config even when `[dialogue.mesh]` has moved — which is why
/// the two are resolved separately rather than assumed to share a file.
pub fn config_path_for_key(keys: &[&str]) -> std::path::PathBuf {
    let cands = config_candidates();
    for c in &cands {
        if defines(c, keys) {
            return c.clone();
        }
    }
    // Nothing defines it: report the last candidate, which is where the loader
    // will look and (truthfully) find nothing.
    cands
        .last()
        .cloned()
        .unwrap_or_else(|| home_config(".config/mu/config.toml"))
}

/// Every file consulted, in precedence order. `$MU_CONFIG` collapses this to a
/// single entry — it overrides both, for every section.
pub fn config_candidates() -> Vec<std::path::PathBuf> {
    if let Ok(p) = std::env::var("MU_CONFIG") {
        return vec![std::path::PathBuf::from(p)];
    }
    vec![
        home_config(".config/agent/config.toml"),
        home_config(".config/mu/config.toml"),
    ]
}

/// Does `path` define the nested table named by `keys`? Public so
/// `--check-config` can report a section that is present but NOT consulted.
pub fn file_defines(path: &std::path::Path, keys: &[&str]) -> bool {
    defines(path, keys)
}

/// Where to read `[dialogue.<section>]` from (mu-htit).
///
/// `~/.config/agent/config.toml` first — mu-dialogue is a tool several clients
/// use, so its config belongs with the shared agent config, not under the
/// mu-specific tree. Falls back to `~/.config/mu/config.toml`, which is where
/// these sections have lived until now.
///
/// The fallback is per SECTION, not per file, and that distinction matters: a
/// whole-file preference would silently disable presence on any host whose
/// agent config exists but carries no `[dialogue.presence]`, turning a config
/// move into an outage. This way an existing deployment keeps working untouched
/// and a section takes effect wherever it is put.
///
/// `$MU_CONFIG` still wins outright, so an explicit override overrides.
pub fn config_path_for(section: &str) -> std::path::PathBuf {
    config_path_for_key(&["dialogue", section])
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
    /// What the session is working on, as its holder last wrote it. Advisory
    /// like the rest of the value: see [`clean_label`] for what is kept.
    pub label: Option<String>,
}

/// Default for `label_max_chars`. A label is a few words for a human scanning
/// a peer list, so anything past this is cut rather than carried.
pub const DEFAULT_LABEL_MAX_CHARS: usize = 64;

/// The label as it is reported: control characters (newlines included) read
/// as spaces, runs of whitespace collapsed to one, ends trimmed, at most
/// `max_chars` characters. None when nothing printable is left. Anyone can
/// write the value, so this is applied on read rather than trusted from the
/// writer.
pub fn clean_label(raw: &str, max_chars: usize) -> Option<String> {
    let spaced: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = collapsed.chars().take(max_chars).collect();
    let cut = cut.trim_end();
    (!cut.is_empty()).then(|| cut.to_string())
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

fn parse_kv(cfg: &PresenceConfig, kv: &Value) -> Option<LeasePeer> {
    let prefix = cfg.prefix.as_str();
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
        .unwrap_or_else(|| mu_peer::PeerId::parse(&peer_id).role().to_string());
    let registered_at = payload
        .as_ref()
        .and_then(|p| p.get("registered_at_unix_ms"))
        .and_then(Value::as_i64);
    let label = payload
        .as_ref()
        .and_then(|p| p.get("label"))
        .and_then(Value::as_str)
        .and_then(|l| clean_label(l, cfg.label_max_chars));
    Some(LeasePeer {
        peer_id,
        role,
        registered_at,
        label,
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
                    .map(|kvs| kvs.iter().filter_map(|kv| parse_kv(cfg, kv)).collect())
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
        assert_eq!(cfg.label_max_chars, DEFAULT_LABEL_MAX_CHARS);
    }

    /// A bad label limit costs the limit, never presence: the section still
    /// loads, with the default in its place.
    #[test]
    fn a_bad_label_limit_keeps_presence_on() {
        let dir = std::env::temp_dir().join(format!("mu-dlg-presence3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("limit.toml");
        let base = "[dialogue.presence]\nenabled = true\netcd = [\"http://10.0.0.1:2379\"]\n";
        for (line, want) in [
            ("label_max_chars = 10", 10),
            ("label_max_chars = 0", 0),
            ("label_max_chars = -1", DEFAULT_LABEL_MAX_CHARS),
            ("label_max_chars = \"64\"", DEFAULT_LABEL_MAX_CHARS),
            ("label_max_chars = 64.0", DEFAULT_LABEL_MAX_CHARS),
        ] {
            std::fs::write(&p, format!("{base}{line}\n")).unwrap();
            let cfg = load(&p).unwrap_or_else(|| panic!("{line} disabled presence"));
            assert_eq!(cfg.label_max_chars, want, "{line}");
        }
    }

    /// mu-htit: the agent config wins for a section it defines, and the mu
    /// config still serves the ones it does not — per SECTION, so moving one
    /// does not disable the other.
    #[test]
    fn sections_resolve_from_the_agent_config_first_then_fall_back() {
        let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("mu-dlg-cfgres-{}", std::process::id()));
        let agent = dir.join(".config/agent");
        let mu = dir.join(".config/mu");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&mu).unwrap();
        // The shared agent config carries only `mesh`; presence still lives
        // in the mu config, as on a host mid-migration.
        std::fs::write(
            agent.join("config.toml"),
            "[dialogue.mesh]\nenabled = true\n",
        )
        .unwrap();
        std::fs::write(
            mu.join("config.toml"),
            "[dialogue.presence]\nenabled = true\netcd = [\"http://x:2379\"]\n",
        )
        .unwrap();

        // Isolate HOME; $MU_CONFIG must not leak in from the caller's env.
        let prev_home = std::env::var("HOME").ok();
        let prev_cfg = std::env::var("MU_CONFIG").ok();
        std::env::set_var("HOME", &dir);
        std::env::remove_var("MU_CONFIG");

        assert_eq!(config_path_for("mesh"), agent.join("config.toml"));
        assert_eq!(config_path_for("presence"), mu.join("config.toml"));
        // A section defined nowhere resolves to the mu config, where `load`
        // finds nothing and the feature stays off — the pre-mu-htit behaviour.
        assert_eq!(config_path_for("nosuch"), mu.join("config.toml"));

        // An explicit override still beats both.
        std::env::set_var("MU_CONFIG", "/explicit/path.toml");
        assert_eq!(
            config_path_for("mesh"),
            std::path::PathBuf::from("/explicit/path.toml")
        );

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match prev_cfg {
            Some(c) => std::env::set_var("MU_CONFIG", c),
            None => std::env::remove_var("MU_CONFIG"),
        }
    }

    fn test_cfg() -> PresenceConfig {
        PresenceConfig {
            enabled: true,
            etcd: vec![],
            prefix: DEFAULT_PREFIX.to_string(),
            label_max_chars: DEFAULT_LABEL_MAX_CHARS,
        }
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
        let p = parse_kv(&test_cfg(), &kv).unwrap();
        assert_eq!(p.peer_id, "cc:abc");
        assert_eq!(p.role, "cc");
        assert_eq!(p.registered_at, Some(123));
        assert_eq!(p.label, None);
        // Malformed value: the lease-held key still counts; role from the id.
        let kv = serde_json::json!({
            "key": b64.encode(format!("{DEFAULT_PREFIX}mu:d:s")),
            "value": b64.encode("not-json"),
        });
        let p = parse_kv(&test_cfg(), &kv).unwrap();
        assert_eq!(p.peer_id, "mu:d:s");
        assert_eq!(p.role, "mu");
        // Key outside the prefix is ignored.
        let kv = serde_json::json!({ "key": b64.encode("/elsewhere/x"), "value": "" });
        assert!(parse_kv(&test_cfg(), &kv).is_none());
    }

    fn kv_with_value(value: &str) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD;
        serde_json::json!({
            "key": b64.encode(format!("{DEFAULT_PREFIX}cc:abc")),
            "value": b64.encode(value),
        })
    }

    #[test]
    fn parse_kv_reads_the_label() {
        let p = parse_kv(
            &test_cfg(),
            &kv_with_value(r#"{"role":"cc","label":"mu-166mh presence labels"}"#),
        )
        .unwrap();
        assert_eq!(p.label.as_deref(), Some("mu-166mh presence labels"));
    }

    /// A bad label costs the label, never the peer: the lease is what says
    /// the peer is live.
    #[test]
    fn a_bad_label_still_reports_the_peer_live() {
        for value in [
            r#"{"role":"cc","label":42}"#,
            r#"{"role":"cc","label":null}"#,
            r#"{"role":"cc","label":"   "}"#,
            r#"{"role":"cc","label":"\n\t\u0007"}"#,
        ] {
            let p = parse_kv(&test_cfg(), &kv_with_value(value)).unwrap();
            assert_eq!(p.peer_id, "cc:abc", "{value}");
            assert_eq!(p.label, None, "{value}");
        }
        let long = format!(r#"{{"role":"cc","label":"{}"}}"#, "é".repeat(500));
        let p = parse_kv(&test_cfg(), &kv_with_value(&long)).unwrap();
        assert_eq!(p.label.unwrap().chars().count(), DEFAULT_LABEL_MAX_CHARS);
        // The configured limit is the one applied.
        let cfg = PresenceConfig {
            label_max_chars: 5,
            ..test_cfg()
        };
        let p = parse_kv(&cfg, &kv_with_value(&long)).unwrap();
        assert_eq!(p.label.unwrap().chars().count(), 5);
    }

    #[test]
    fn clean_label_strips_controls_and_trims() {
        assert_eq!(
            clean_label("  irc\ngateway  ", 64).as_deref(),
            Some("irc gateway")
        );
        assert_eq!(clean_label("a \t\r\n  b", 64).as_deref(), Some("a b"));
        // An escape sequence loses its ESC, so it cannot drive a terminal.
        assert_eq!(clean_label("\x1b[31mred", 64).as_deref(), Some("[31mred"));
        assert_eq!(clean_label("", 64), None);
        // Cut at the limit, then trimmed, so no trailing space survives.
        let s = format!("{} tail", "a".repeat(9));
        assert_eq!(clean_label(&s, 10).unwrap(), "a".repeat(9));
    }
}
