//! mu-051 P1/P2: the daemon and its live sessions in the fleet registry
//! (`specs/mu-051-client-api-over-nats.md` §6/§7).
//!
//! Two JetStream KV buckets. `mu_daemons` holds one key per live daemon,
//! keyed by its daemon id. `mu_sessions` holds one key per live session,
//! `<daemon>.<session>`, whose value is the same [`SessionInfo`] that
//! `session.list` returns (status, model, activity, cost). A client lists the
//! buckets to find every daemon and session without knowing a name in
//! advance (ruling 2), and watches them to see changes. Session status is as
//! fresh as the last heartbeat; transitions are not written as they happen.
//!
//! Liveness is the bucket's `max_age` = `[mesh].registry_ttl_secs`: the task
//! rewrites its keys every third of it, so a daemon that dies without
//! deregistering ages out, sessions and all. The bucket is created with `limit_markers`, so the
//! server writes a `MaxAge` marker when a key ages out and a watcher sees it
//! go (as a purge), instead of the key silently vanishing. A closed session's
//! key is deleted on the next heartbeat; a clean shutdown deletes them all at
//! once ([`RegistryHandle::shutdown`]).
//!
//! A restart registers under the new process's daemon id, so nothing a
//! previous run left behind can block it; the old key ages out.
//!
//! Gated by `[mesh] enabled = true` + `registry = true`. A TTL below the
//! minimum refuses to start ([`from_mesh`]); NATS or JetStream trouble at
//! runtime is logged with its cause and remedy, retried every heartbeat, and
//! never touches a session.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use async_nats::jetstream::{self, kv};
use futures::StreamExt as _;
use mu_core::config::MeshConfig;
use mu_core::protocol::SessionInfo;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::discovery::derive_session_info;
use super::sessions::WeakSessions;

pub(crate) const BUCKET: &str = "mu_daemons";
pub(crate) const SESSIONS_BUCKET: &str = "mu_sessions";
const MIN_TTL_SECS: u64 = 6;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RegistryConfig {
    pub nats_url: String,
    pub ttl: Duration,
    /// Deadline for each NATS call (`[mesh].registry_call_timeout_secs`).
    pub call_timeout: Duration,
}

/// `Ok(None)` when the registry is off. A TTL below [`MIN_TTL_SECS`] is a
/// misconfiguration: the daemon refuses to start rather than run with a
/// heartbeat it cannot keep.
pub(crate) fn from_mesh(mesh: &MeshConfig) -> Result<Option<RegistryConfig>> {
    if !mesh.registers() {
        return Ok(None);
    }
    if mesh.registry_ttl_secs < MIN_TTL_SECS {
        bail!(
            "[mesh].registry_ttl_secs = {} is below the minimum of {MIN_TTL_SECS}: the \
             heartbeat runs every third of it; raise it (default 60) or set registry = false",
            mesh.registry_ttl_secs
        );
    }
    if mesh.registry_call_timeout_secs == 0 {
        bail!("[mesh].registry_call_timeout_secs must be at least 1 (default 5)");
    }
    Ok(Some(RegistryConfig {
        nats_url: mesh.nats_url.clone(),
        ttl: Duration::from_secs(mesh.registry_ttl_secs),
        call_timeout: Duration::from_secs(mesh.registry_call_timeout_secs),
    }))
}

/// The value stored under the daemon's key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonEntry {
    pub daemon: String,
    pub host: String,
    pub pid: u32,
    pub version: String,
    pub started_ms: u64,
    pub heartbeat_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Remedy text for a registry fault. JetStream disabled is the expected
/// first-run fault (the operator's server shipped without it), so it gets
/// named on its own.
fn describe(url: &str, err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("jetstream unavailable")
        || lower.contains("jetstream not enabled")
        || lower.contains("no responders")
    {
        format!(
            "JetStream is not enabled on the NATS server at {url}: add `jetstream {{}}` to \
             its nats-server.conf and restart it, or set [mesh].registry = false ({err})"
        )
    } else {
        format!("NATS registry at {url} unavailable: {err}; retrying every heartbeat")
    }
}

async fn timed<T, E: std::fmt::Display>(
    limit: Duration,
    what: &str,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{what}: {e}")),
        Err(_) => Err(format!(
            "{what}: no answer within {limit:?} ([mesh].registry_call_timeout_secs)"
        )),
    }
}

/// Open the bucket, creating it on first use. If both fail, both errors are
/// reported: the lookup's may be the real cause (a permissions or transport
/// fault), and the create's alone would hide it.
async fn open_bucket(
    client: &async_nats::Client,
    bucket: &str,
    description: &str,
    ttl: Duration,
    limit: Duration,
) -> Result<kv::Store, String> {
    let js = jetstream::new(client.clone());
    let lookup = match timed(limit, "open bucket", js.get_key_value(bucket)).await {
        Ok(store) => return Ok(store),
        Err(e) => e,
    };
    timed(
        limit,
        "create bucket",
        js.create_key_value(kv::Config {
            bucket: bucket.to_string(),
            description: description.to_string(),
            history: 1,
            max_age: ttl,
            storage: jetstream::stream::StorageType::Memory,
            limit_markers: Some(ttl),
            ..Default::default()
        }),
    )
    .await
    .map_err(|create| format!("{lookup}; then {create}"))
}

/// The bucket's expiry, if the bucket can deliver liveness: keys must age
/// out (`max_age` at least the minimum TTL; 0 means never) and the server
/// must mark it when they do (`limit_markers`), or a watcher never sees a
/// dead daemon go. A bucket that fails this is a fault, not a warning: the
/// daemon does not register into it, since its key would outlive a crash.
async fn bucket_ttl(store: &kv::Store, limit: Duration) -> Result<Duration, String> {
    let status = timed(limit, "bucket status", store.status()).await?;
    let max_age = status.max_age();
    let markers = status.info.config.subject_delete_marker_ttl;
    let bucket = status.bucket();
    if max_age < Duration::from_secs(MIN_TTL_SECS) || markers.is_none() {
        return Err(format!(
            "bucket {bucket} cannot expire dead daemons (max_age {max_age:?}, limit markers \
             {markers:?}; it needs max_age >= {MIN_TTL_SECS}s and limit markers on). Something \
             other than a mu daemon made it; delete it (`nats kv del {bucket}`) and the next \
             heartbeat recreates it"
        ));
    }
    Ok(max_age)
}

/// Handle on the registry task. [`shutdown`](Self::shutdown) deregisters;
/// dropping without it aborts the task, and the key ages out.
pub(crate) struct RegistryHandle {
    task: Option<tokio::task::JoinHandle<()>>,
    stop: Option<oneshot::Sender<()>>,
    /// How long `shutdown` waits: one in-flight call plus the delete.
    wait: Duration,
}

impl RegistryHandle {
    /// Delete this daemon's key and stop. Bounded, so it cannot hold up
    /// daemon exit. The task sees the stop between heartbeats, so the wait
    /// covers one in-flight call plus the delete. On timeout the task is
    /// aborted (by `Drop`, as `self` goes) and the key ages out instead.
    pub(crate) async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let wait = self.wait;
        let Some(task) = self.task.as_mut() else {
            return;
        };
        match tokio::time::timeout(wait, task).await {
            Ok(Ok(())) => self.task = None,
            Ok(Err(e)) => {
                self.task = None;
                tracing::warn!(error = %e,
                    "registry: the task had already died; its key ages out at the bucket TTL");
            }
            Err(_) => tracing::warn!(
                "registry: deregistration did not finish within {wait:?} (twice \
                 [mesh].registry_call_timeout_secs); aborting it, the key ages out at the \
                 bucket TTL instead"
            ),
        }
    }
}

impl Drop for RegistryHandle {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// A KV key for `<daemon>.<session>`. KV keys allow `[-/_=.A-Za-z0-9]`;
/// daemon ids are hex and session ids `session-N`, but a resumed or foreign
/// id could carry anything, so other characters become `_` rather than
/// dropping the session from the list.
fn session_key(daemon_id: &str, session_id: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-/_=".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("{}.{}", clean(daemon_id), clean(session_id))
}

/// This daemon's live sessions right now, as `(key, value)`. `None` while
/// the daemon is shutting down (the registry is gone). The strong
/// `Sessions` lives only inside this call, never across an await, so the
/// task cannot hold the shutdown cascade open (mu-ad5x). Rehydrated
/// read-only sessions are not live and not listed.
fn session_entries(sessions: &WeakSessions, daemon_id: &str) -> Option<Vec<(String, Vec<u8>)>> {
    let s = sessions.upgrade()?;
    let live: HashSet<String> = s.live_session_ids().into_iter().collect();
    let entries = s
        .snapshot_for_listing()
        .into_iter()
        .filter(|(sid, _, _)| live.contains(sid))
        .map(|(sid, log, parent)| {
            let mut info = derive_session_info(&sid, &log, parent, daemon_id);
            // The log can't show an open approval prompt (it is not logged);
            // the live registry can, and "waiting for me" is the status that
            // matters most remotely.
            if s.open_approvals(&sid) > 0 {
                info.status = mu_core::protocol::SessionStatusSummary::AwaitingInputRequired;
            }
            let value = serde_json::to_vec(&info).expect("SessionInfo serializes");
            (session_key(daemon_id, &sid), value)
        })
        .collect();
    Some(entries)
}

/// Both buckets, opened together and validated together.
struct Stores {
    daemons: kv::Store,
    sessions: kv::Store,
}

async fn open_stores(
    client: &async_nats::Client,
    ttl: Duration,
    limit: Duration,
) -> Result<(Stores, Duration), String> {
    let daemons = open_bucket(
        client,
        BUCKET,
        "mu fleet registry: one key per live daemon (mu-051)",
        ttl,
        limit,
    )
    .await?;
    let sessions = open_bucket(
        client,
        SESSIONS_BUCKET,
        "mu fleet registry: one key per live session, <daemon>.<session> (mu-051)",
        ttl,
        limit,
    )
    .await?;
    let d = bucket_ttl(&daemons, limit).await?;
    let s = bucket_ttl(&sessions, limit).await?;
    // The heartbeat must outrun the shorter of the two expiries.
    Ok((Stores { daemons, sessions }, d.min(s)))
}

/// Spawn the registry task. Never fails and never blocks the caller.
pub(crate) fn spawn(
    cfg: RegistryConfig,
    daemon_id: String,
    version: String,
    started_ms: u64,
    sessions: WeakSessions,
) -> RegistryHandle {
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let limit = cfg.call_timeout;
    let task = tokio::spawn(async move {
        let host = gethostname::gethostname().to_string_lossy().into_owned();
        let pid = std::process::id();
        let mut client: Option<async_nats::Client> = None;
        let mut stores: Option<Stores> = None;
        let mut tick = cfg.ttl / 3;
        // Session keys this daemon has put, so a closed session's key can be
        // deleted and a clean shutdown can delete them all.
        let mut published: HashSet<String> = HashSet::new();
        // The last fault reported, so a persisting fault is logged once and
        // its clearing is logged too, instead of a line every heartbeat.
        let mut fault: Option<String> = None;
        let mut registered = false;
        loop {
            // Heartbeats are spaced from START to start, so the time a sync
            // takes does not stretch the gap between refreshes past the TTL.
            let beat = tokio::time::Instant::now();
            let result: Result<(), String> = async {
                if client.is_none() {
                    let c = timed(limit, "connect", async_nats::connect(cfg.nats_url.as_str()));
                    client = Some(c.await?);
                }
                if stores.is_none() {
                    let c = client.as_ref().expect("set above");
                    let (s, bucket) = open_stores(c, cfg.ttl, limit).await?;
                    if bucket != cfg.ttl {
                        tracing::warn!(
                            bucket_max_age = ?bucket, registry_ttl = ?cfg.ttl,
                            "registry: buckets {BUCKET}/{SESSIONS_BUCKET} already exist with a \
                             different max_age; it governs expiry, so the heartbeat follows it. \
                             To change it, align [mesh].registry_ttl_secs across daemons and \
                             delete the buckets"
                        );
                    }
                    tick = bucket / 3;
                    stores = Some(s);
                }
                let s = stores.as_ref().expect("set above");
                let entry = DaemonEntry {
                    daemon: daemon_id.clone(),
                    host: host.clone(),
                    pid,
                    version: version.clone(),
                    started_ms,
                    heartbeat_ms: now_ms(),
                };
                let value = serde_json::to_vec(&entry).expect("entry serializes");
                let put = timed(limit, "put", s.daemons.put(&daemon_id, value.into())).await;
                let synced = match put {
                    Err(e) => Err(e),
                    Ok(_) => {
                        sync_sessions(&s.sessions, &sessions, &daemon_id, &mut published, limit)
                            .await
                    }
                };
                if synced.is_err() {
                    // Re-open the buckets next time: they may have been deleted.
                    stores = None;
                }
                synced
            }
            .await;
            match result {
                Ok(()) => {
                    if fault.take().is_some() {
                        tracing::info!(bucket = BUCKET, "registry: recovered; daemon registered");
                    } else if !registered {
                        tracing::info!(bucket = BUCKET, key = %daemon_id, every = ?tick,
                            "registry: daemon registered");
                    }
                    registered = true;
                }
                Err(e) => {
                    let msg = describe(&cfg.nats_url, &e);
                    if fault.as_deref() != Some(msg.as_str()) {
                        tracing::error!("registry: {msg}");
                        fault = Some(msg);
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep_until(beat + tick) => {}
                _ = &mut stop_rx => break,
            }
        }
        if let Some(s) = stores {
            // All deletes at once, so exit costs one call's latency however
            // many sessions there are, and fits `RegistryHandle::wait`.
            let sessions_gone = futures::future::join_all(
                published
                    .iter()
                    .map(|key| timed(limit, "delete", s.sessions.delete(key))),
            );
            let daemon_gone = timed(limit, "delete", s.daemons.delete(&daemon_id));
            let (sessions_gone, daemon_gone) = tokio::join!(sessions_gone, daemon_gone);
            let failed = sessions_gone
                .into_iter()
                .chain(std::iter::once(daemon_gone))
                .find_map(Result::err);
            match failed {
                None => tracing::info!(key = %daemon_id, sessions = published.len(),
                    "registry: deregistered"),
                Some(e) => tracing::warn!(
                    "registry: {}; the remaining keys age out at the bucket TTL instead",
                    describe(&cfg.nats_url, &e)
                ),
            }
        }
    });
    RegistryHandle {
        task: Some(task),
        stop: Some(stop_tx),
        wait: limit * 2,
    }
}

/// Put every live session's entry (refreshing its expiry) and delete the
/// keys of sessions that have closed since the last heartbeat. The calls go
/// out together, so a sync takes about one call's latency however many
/// sessions there are; a slow server cannot add them up into a refresh gap
/// longer than the TTL. Every key that was written is recorded even when
/// another failed, so the exit path still deletes it.
async fn sync_sessions(
    store: &kv::Store,
    sessions: &WeakSessions,
    daemon_id: &str,
    published: &mut HashSet<String>,
    limit: Duration,
) -> Result<(), String> {
    let Some(entries) = session_entries(sessions, daemon_id) else {
        return Ok(()); // shutting down; the exit path deletes what we put
    };
    let live: HashSet<String> = entries.iter().map(|(k, _)| k.clone()).collect();
    let puts = futures::future::join_all(entries.into_iter().map(|(key, value)| async move {
        let r = timed(limit, "put session", store.put(&key, value.into())).await;
        (key, r)
    }))
    .await;
    let mut failed: Option<String> = None;
    for (key, r) in puts {
        match r {
            Ok(_) => {
                published.insert(key);
            }
            Err(e) => failed = Some(e),
        }
    }
    let closed: Vec<String> = published.difference(&live).cloned().collect();
    let deletes = futures::future::join_all(closed.into_iter().map(|key| async move {
        let r = timed(limit, "delete session", store.delete(&key)).await;
        (key, r)
    }))
    .await;
    for (key, r) in deletes {
        match r {
            Ok(()) => {
                published.remove(&key);
            }
            Err(e) => failed = Some(e),
        }
    }
    failed.map_or(Ok(()), Err)
}

/// `mu sessions`: every live session in the fleet registry, one line each,
/// grouped by daemon. With `watch`, then one line per change until killed.
///
/// `call_timeout` bounds every call that should answer promptly (connect,
/// bucket lookup, the whole listing, establishing the watch); the caller
/// passes `[mesh].registry_call_timeout_secs`. A watch that sits quiet is
/// not a timeout.
pub async fn print_sessions(
    nats_url: &str,
    call_timeout: Duration,
    watch: bool,
    out: &mut (dyn std::io::Write + Send),
) -> Result<()> {
    let limit = call_timeout;
    let client = timed(limit, "connect", async_nats::connect(nats_url))
        .await
        .map_err(|e| anyhow!("{}", describe(nats_url, &e)))?;
    let js = jetstream::new(client);
    let open = |name: &'static str| {
        let js = js.clone();
        async move {
            timed(limit, "open bucket", js.get_key_value(name))
                .await
                .map_err(|e| {
                    anyhow!(
                        "no {name} bucket on {nats_url}: no daemon with [mesh] registry = true \
                         has run against this server, or JetStream is off ({e})"
                    )
                })
        }
    };
    let daemons = open(BUCKET).await?;
    let sessions = open(SESSIONS_BUCKET).await?;

    let (hosts, mut rows, unreadable) = timed(limit, "list registry", async {
        let mut hosts = std::collections::HashMap::new();
        let mut rows: Vec<SessionInfo> = Vec::new();
        let mut unreadable: Vec<String> = Vec::new();
        let mut keys = daemons
            .keys()
            .await
            .map_err(|e| anyhow!("list {BUCKET}: {e}"))?;
        while let Some(k) = keys.next().await {
            let k = k.map_err(|e| anyhow!("list {BUCKET}: {e}"))?;
            if let Some(v) = daemons.get(&k).await.map_err(|e| anyhow!("get {k}: {e}"))? {
                match serde_json::from_slice::<DaemonEntry>(&v) {
                    Ok(d) => {
                        hosts.insert(d.daemon.clone(), d);
                    }
                    Err(e) => unreadable.push(format!("{BUCKET} {k}: {e}")),
                }
            }
        }
        let mut keys = sessions
            .keys()
            .await
            .map_err(|e| anyhow!("list {SESSIONS_BUCKET}: {e}"))?;
        while let Some(k) = keys.next().await {
            let k = k.map_err(|e| anyhow!("list {SESSIONS_BUCKET}: {e}"))?;
            if let Some(v) = sessions
                .get(&k)
                .await
                .map_err(|e| anyhow!("get {k}: {e}"))?
            {
                match serde_json::from_slice::<SessionInfo>(&v) {
                    Ok(info) => rows.push(info),
                    Err(e) => unreadable.push(format!("{SESSIONS_BUCKET} {k}: {e}")),
                }
            }
        }
        Ok::<_, anyhow::Error>((hosts, rows, unreadable))
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;
    for u in &unreadable {
        writeln!(out, "(unreadable entry {u})")?;
    }
    rows.sort_by(|a, b| {
        (
            a.daemon_id.as_str(),
            std::cmp::Reverse(a.last_activity_unix_ms),
        )
            .cmp(&(
                b.daemon_id.as_str(),
                std::cmp::Reverse(b.last_activity_unix_ms),
            ))
    });
    let now = now_ms();
    let mut daemon_ids: Vec<&String> = hosts.keys().collect();
    daemon_ids.sort();
    for d in daemon_ids {
        let e = &hosts[d];
        writeln!(out, "{d}  {}  pid {}  mu {}", e.host, e.pid, e.version)?;
        let mine: Vec<&SessionInfo> = rows.iter().filter(|r| &r.daemon_id == d).collect();
        if mine.is_empty() {
            writeln!(out, "  (no live sessions)")?;
        }
        for r in mine {
            writeln!(out, "  {}", session_line(r, now))?;
        }
    }
    for r in rows.iter().filter(|r| !hosts.contains_key(&r.daemon_id)) {
        writeln!(
            out,
            "{} (daemon entry missing)  {}",
            r.daemon_id,
            session_line(r, now)
        )?;
    }
    if !watch {
        return Ok(());
    }
    out.flush()?;
    let mut w = timed(limit, "watch", sessions.watch_all())
        .await
        .map_err(|e| anyhow!("watch {SESSIONS_BUCKET}: {e}"))?;
    // Every heartbeat rewrites every key, so print a key only when what a
    // reader acts on changed (status, model, asks), not on each refresh.
    let mut last: std::collections::HashMap<String, String> = rows
        .iter()
        .map(|r| (session_key(&r.daemon_id, &r.session_id), change_mark(r)))
        .collect();
    while let Some(e) = w.next().await {
        let e = e.map_err(|e| anyhow!("watch {SESSIONS_BUCKET}: {e}"))?;
        match e.operation {
            kv::Operation::Put => match serde_json::from_slice::<SessionInfo>(&e.value) {
                Ok(info) => {
                    let mark = change_mark(&info);
                    if last.get(&e.key) != Some(&mark) {
                        writeln!(out, "{}  {}", e.key, session_line(&info, now_ms()))?;
                        last.insert(e.key.clone(), mark);
                    }
                }
                Err(err) => writeln!(out, "{}  (unreadable: {err})", e.key)?,
            },
            kv::Operation::Delete | kv::Operation::Purge => {
                // A delete's marker ages out later as a second Purge; say
                // "gone" once.
                if last.remove(&e.key).is_some() {
                    writeln!(out, "{}  gone", e.key)?;
                }
            }
        }
        out.flush()?;
    }
    Ok(())
}

fn change_mark(r: &SessionInfo) -> String {
    format!(
        "{:?}|{}/{}|{}",
        r.status, r.provider_kind, r.model, r.ask_count
    )
}

fn session_line(r: &SessionInfo, now: u64) -> String {
    let status = serde_json::to_value(r.status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{:?}", r.status));
    let idle = now.saturating_sub(r.last_activity_unix_ms) / 1000;
    let cost = r
        .cost_usd
        .map(|c| format!("${c:.2}"))
        .unwrap_or_else(|| "$?".to_string());
    format!(
        "{:<12} {:<24} {:<34} active {}s ago  {} asks  {}",
        r.session_id,
        status,
        format!("{}/{}", r.provider_kind, r.model),
        idle,
        r.ask_count,
        cost
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::{MailboxState, NewSession, ProviderStatusTracker, Sessions};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A live session with no agent behind it: enough for the registry,
    /// which only reads the session list and each session's log.
    fn insert_live(sessions: &Sessions, id: &str) {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        sessions.insert(
            id.to_string(),
            NewSession {
                input_tx: tx,
                forwarder: tokio::spawn(async {}),
                agent: tokio::spawn(async {}),
                event_log: Arc::new(mu_core::event_log::SessionEventLog::new(id.to_string())),
                pending_approvals: Arc::new(Mutex::new(HashMap::new())),
                parent_session_id: None,
                capability: Arc::new(Mutex::new(mu_core::capability::Capability::root())),
                cache_ttl: Default::default(),
                provider_status: Arc::new(Mutex::new(ProviderStatusTracker::new())),
                mailbox: Arc::new(MailboxState::new()),
                status_watch: None,
                autonomy_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                live_context_soft_limit: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                live_discover_hints: None,
            },
        );
    }

    fn mesh(toml: &str) -> MeshConfig {
        toml::from_str::<mu_core::config::Config>(toml)
            .expect("parse")
            .mesh
    }

    #[test]
    fn from_mesh_gating_and_ttl_floor() {
        assert!(from_mesh(&MeshConfig::default()).unwrap().is_none());
        assert!(
            from_mesh(&mesh("[mesh]\nenabled = false\nregistry = true\n"))
                .unwrap()
                .is_none()
        );
        let cfg = from_mesh(&mesh(
            "[mesh]\nenabled = true\nregistry = true\nnats_url = \"n:1\"\n",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(cfg.ttl, Duration::from_secs(60));
        assert_eq!(cfg.nats_url, "n:1");
        let err = from_mesh(&mesh(
            "[mesh]\nenabled = true\nregistry = true\nregistry_ttl_secs = 5\n",
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("registry_ttl_secs"),
            "names the setting: {err}"
        );
    }

    #[test]
    fn describe_names_jetstream_remedy() {
        let m = describe("h:4222", "open bucket: jetstream not enabled");
        assert!(m.contains("jetstream {}"), "{m}");
        let m = describe("h:4222", "connect: refused");
        assert!(m.contains("retrying"), "{m}");
    }

    /// Spawn a private nats-server; `None` (test skips) without a binary.
    fn nats_server(jetstream: bool) -> Option<(std::process::Child, String, tempfile::TempDir)> {
        let bin = std::env::var("NATS_BIN").unwrap_or_else(|_| "nats-server".to_string());
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .ok()?
            .local_addr()
            .ok()?
            .port();
        let dir = tempfile::tempdir().ok()?;
        let mut cmd = std::process::Command::new(&bin);
        cmd.args(["-a", "127.0.0.1", "-p", &port.to_string()]);
        if jetstream {
            cmd.args(["-js", "-sd", dir.path().to_str()?]);
        }
        let child = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(child) = child else {
            eprintln!("skipping: cannot spawn nats-server ({bin})");
            return None;
        };
        Some((child, format!("127.0.0.1:{port}"), dir))
    }

    async fn connect_retry(url: &str) -> async_nats::Client {
        for _ in 0..50 {
            if let Ok(c) = async_nats::connect(url).await {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("nats-server at {url} never came up");
    }

    /// LIVE (skips without nats-server): register → visible with its fields;
    /// clean shutdown → a Delete in the watch; a killed daemon (aborted task)
    /// → a MaxAge purge once the TTL passes.
    #[tokio::test]
    async fn registers_deregisters_and_ages_out() {
        let Some((mut srv, url, _dir)) = nats_server(true) else {
            return;
        };
        let client = connect_retry(&url).await;
        let idle = Sessions::new();
        let cfg = RegistryConfig {
            nats_url: url.clone(),
            ttl: Duration::from_secs(6),
            call_timeout: Duration::from_secs(5),
        };

        // Clean lifecycle.
        let h = spawn(
            cfg.clone(),
            "d-clean".into(),
            "v1".into(),
            42,
            idle.downgrade(),
        );
        let js = jetstream::new(client.clone());
        let store = {
            let mut tries = 0;
            loop {
                if let Ok(s) = js.get_key_value(BUCKET).await {
                    if s.get("d-clean").await.ok().flatten().is_some() {
                        break s;
                    }
                }
                tries += 1;
                assert!(tries < 50, "daemon never registered");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let v: serde_json::Value =
            serde_json::from_slice(&store.get("d-clean").await.unwrap().unwrap()).unwrap();
        assert_eq!(v["daemon"], "d-clean");
        assert_eq!(v["version"], "v1");
        assert_eq!(v["started_ms"], 42);
        assert_eq!(v["pid"], std::process::id());
        let mut watch = store.watch_all().await.unwrap();
        h.shutdown().await;
        let gone = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(e)) = watch.next().await {
                if e.key == "d-clean" && e.operation == kv::Operation::Delete {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(gone, "clean shutdown must publish a Delete");

        // Crash: abort (drop) without shutdown; the key must age out with a
        // marker the watch sees.
        let h = spawn(cfg, "d-crash".into(), "v1".into(), 0, idle.downgrade());
        let mut tries = 0;
        while store.get("d-crash").await.ok().flatten().is_none() {
            tries += 1;
            assert!(tries < 50, "crash daemon never registered");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        drop(h);
        let aged = tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(Ok(e)) = watch.next().await {
                if e.key == "d-crash" && e.operation == kv::Operation::Purge {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(
            aged,
            "a dead daemon's key must age out with a visible marker"
        );
        let _ = srv.kill();
    }

    /// LIVE: a bucket someone else made without expiry is refused, not
    /// registered into (a key there would outlive a crash), and the fault
    /// names the remedy.
    #[tokio::test]
    async fn bucket_without_expiry_is_a_fault() {
        let Some((mut srv, url, _dir)) = nats_server(true) else {
            return;
        };
        let client = connect_retry(&url).await;
        let js = jetstream::new(client.clone());
        js.create_key_value(kv::Config {
            bucket: BUCKET.to_string(),
            history: 1,
            ..Default::default()
        })
        .await
        .unwrap();
        let store = open_bucket(
            &client,
            BUCKET,
            "test",
            Duration::from_secs(6),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let err = bucket_ttl(&store, Duration::from_secs(5))
            .await
            .expect_err("no max_age, no markers");
        assert!(err.contains("nats kv del"), "remedy named: {err}");
        let idle = Sessions::new();

        let h = spawn(
            RegistryConfig {
                nats_url: url.clone(),
                ttl: Duration::from_secs(6),
                call_timeout: Duration::from_secs(5),
            },
            "d-foreign".into(),
            "v1".into(),
            0,
            idle.downgrade(),
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            store.get("d-foreign").await.unwrap().is_none(),
            "must not register into a bucket that never expires keys"
        );
        h.shutdown().await;
        let _ = srv.kill();
    }

    #[test]
    fn session_key_keeps_valid_ids_and_cleans_the_rest() {
        assert_eq!(session_key("ab12", "session-3"), "ab12.session-3");
        assert_eq!(session_key("ab12", "a b.c*"), "ab12.a_b_c_");
    }

    /// LIVE: a live session appears under `<daemon>.<session>` with its
    /// SessionInfo; closing it deletes the key on the next heartbeat; a
    /// clean shutdown deletes the rest and the daemon key.
    #[tokio::test]
    async fn sessions_appear_close_and_deregister() {
        let Some((mut srv, url, _dir)) = nats_server(true) else {
            return;
        };
        let client = connect_retry(&url).await;
        let sessions = Sessions::new();
        insert_live(&sessions, "s-keep");
        insert_live(&sessions, "s-close");
        let h = spawn(
            RegistryConfig {
                nats_url: url.clone(),
                ttl: Duration::from_secs(6),
                call_timeout: Duration::from_secs(5),
            },
            "d1".into(),
            "v1".into(),
            0,
            sessions.downgrade(),
        );
        let js = jetstream::new(client.clone());
        let store = {
            let mut tries = 0;
            loop {
                if let Ok(s) = js.get_key_value(SESSIONS_BUCKET).await {
                    if s.get("d1.s-close").await.ok().flatten().is_some() {
                        break s;
                    }
                }
                tries += 1;
                assert!(tries < 50, "sessions never published");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let info: SessionInfo =
            serde_json::from_slice(&store.get("d1.s-keep").await.unwrap().unwrap()).unwrap();
        assert_eq!(info.session_id, "s-keep");
        assert_eq!(info.daemon_id, "d1");

        // An open approval prompt shows as awaiting_input_required on the
        // next heartbeat, though the log never records it.
        let (tx, _rx) = tokio::sync::oneshot::channel();
        assert!(sessions.register_pending_approval_for_test("s-keep", "req-1", tx));
        let mut tries = 0;
        loop {
            let v = store.get("d1.s-keep").await.unwrap().unwrap();
            let info: SessionInfo = serde_json::from_slice(&v).unwrap();
            if info.status == mu_core::protocol::SessionStatusSummary::AwaitingInputRequired {
                break;
            }
            tries += 1;
            assert!(tries < 60, "open approval never reached the registry");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert!(sessions.remove("s-close"));
        let mut tries = 0;
        while store.get("d1.s-close").await.unwrap().is_some() {
            tries += 1;
            assert!(tries < 60, "closed session's key never deleted");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            store.get("d1.s-keep").await.unwrap().is_some(),
            "live one stays"
        );

        h.shutdown().await;
        assert!(
            store.get("d1.s-keep").await.unwrap().is_none(),
            "clean exit deletes sessions"
        );
        let daemons = js.get_key_value(BUCKET).await.unwrap();
        assert!(
            daemons.get("d1").await.unwrap().is_none(),
            "and the daemon key"
        );
        let _ = srv.kill();
    }

    /// LIVE: many sessions are all published within one heartbeat, since
    /// the puts go out together rather than one after another.
    #[tokio::test]
    async fn many_sessions_publish_in_one_heartbeat() {
        let Some((mut srv, url, _dir)) = nats_server(true) else {
            return;
        };
        let client = connect_retry(&url).await;
        let sessions = Sessions::new();
        for i in 0..50 {
            insert_live(&sessions, &format!("s{i}"));
        }
        let h = spawn(
            RegistryConfig {
                nats_url: url.clone(),
                ttl: Duration::from_secs(6),
                call_timeout: Duration::from_secs(5),
            },
            "dm".into(),
            "v1".into(),
            0,
            sessions.downgrade(),
        );
        let js = jetstream::new(client.clone());
        // One heartbeat is 2 s at this TTL; all 50 must land inside it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let count = loop {
            let mut n = 0;
            if let Ok(store) = js.get_key_value(SESSIONS_BUCKET).await {
                if let Ok(mut keys) = store.keys().await {
                    while let Some(Ok(_)) = keys.next().await {
                        n += 1;
                    }
                }
            }
            if n == 50 || tokio::time::Instant::now() > deadline {
                break n;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(count, 50, "every live session published within a heartbeat");
        h.shutdown().await;
        let store = js.get_key_value(SESSIONS_BUCKET).await.unwrap();
        let mut keys = store.keys().await.unwrap();
        assert!(keys.next().await.is_none(), "clean exit deleted all 50");
        let _ = srv.kill();
    }

    /// LIVE: JetStream off on the server — opening the bucket fails, and the
    /// fault the task logs names the remedy.
    #[tokio::test]
    async fn jetstream_disabled_is_a_runtime_fault() {
        let Some((mut srv, url, _dir)) = nats_server(false) else {
            return;
        };
        let client = connect_retry(&url).await;
        let err = open_bucket(
            &client,
            BUCKET,
            "test",
            Duration::from_secs(6),
            Duration::from_secs(5),
        )
        .await
        .expect_err("no JetStream, no bucket");
        let msg = describe(&url, &err);
        assert!(
            msg.contains("jetstream {}"),
            "remedy named: {msg} (raw: {err})"
        );
        let _ = srv.kill();
    }
}
