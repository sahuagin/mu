//! mu-051 P1: the daemon's entry in the fleet registry
//! (`specs/mu-051-client-api-over-nats.md` §6/§7).
//!
//! One JetStream KV bucket, `mu_daemons`, holds one key per live daemon,
//! keyed by its daemon id. A client lists the bucket to find every daemon
//! without knowing a name in advance (ruling 2), and watches it to see them
//! come and go.
//!
//! Liveness is the bucket's `max_age` = `[mesh].registry_ttl_secs`: the task
//! rewrites the key every third of it, so a daemon that dies without
//! deregistering ages out. The bucket is created with `limit_markers`, so the
//! server writes a `MaxAge` marker when a key ages out and a watcher sees it
//! go (as a purge), instead of the key silently vanishing. A clean shutdown
//! deletes the key at once ([`RegistryHandle::shutdown`]).
//!
//! A restart registers under the new process's daemon id, so nothing a
//! previous run left behind can block it; the old key ages out.
//!
//! Gated by `[mesh] enabled = true` + `registry = true`. A TTL below the
//! minimum refuses to start ([`from_mesh`]); NATS or JetStream trouble at
//! runtime is logged with its cause and remedy, retried every heartbeat, and
//! never touches a session.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use async_nats::jetstream::{self, kv};
use mu_core::config::MeshConfig;
use serde::Serialize;
use tokio::sync::oneshot;

pub(crate) const BUCKET: &str = "mu_daemons";
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
#[derive(Debug, Clone, Serialize)]
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
    ttl: Duration,
    limit: Duration,
) -> Result<kv::Store, String> {
    let js = jetstream::new(client.clone());
    let lookup = match timed(limit, "open bucket", js.get_key_value(BUCKET)).await {
        Ok(store) => return Ok(store),
        Err(e) => e,
    };
    timed(
        limit,
        "create bucket",
        js.create_key_value(kv::Config {
            bucket: BUCKET.to_string(),
            description: "mu fleet registry: one key per live daemon (mu-051)".to_string(),
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
    if max_age < Duration::from_secs(MIN_TTL_SECS) || markers.is_none() {
        return Err(format!(
            "bucket {BUCKET} cannot expire dead daemons (max_age {max_age:?}, limit markers \
             {markers:?}; it needs max_age >= {MIN_TTL_SECS}s and limit markers on). Something \
             other than a mu daemon made it; delete it (`nats kv del {BUCKET}`) and the next \
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

/// Spawn the registry task. Never fails and never blocks the caller.
pub(crate) fn spawn(
    cfg: RegistryConfig,
    daemon_id: String,
    version: String,
    started_ms: u64,
) -> RegistryHandle {
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let limit = cfg.call_timeout;
    let task = tokio::spawn(async move {
        let host = gethostname::gethostname().to_string_lossy().into_owned();
        let pid = std::process::id();
        let mut client: Option<async_nats::Client> = None;
        let mut store: Option<kv::Store> = None;
        let mut tick = cfg.ttl / 3;
        // The last fault reported, so a persisting fault is logged once and
        // its clearing is logged too, instead of a line every heartbeat.
        let mut fault: Option<String> = None;
        let mut registered = false;
        loop {
            let result: Result<(), String> = async {
                if client.is_none() {
                    let c = timed(limit, "connect", async_nats::connect(cfg.nats_url.as_str()));
                    client = Some(c.await?);
                }
                if store.is_none() {
                    let c = client.as_ref().expect("set above");
                    let s = open_bucket(c, cfg.ttl, limit).await?;
                    let bucket = bucket_ttl(&s, limit).await?;
                    if bucket != cfg.ttl {
                        tracing::warn!(
                            bucket_max_age = ?bucket, registry_ttl = ?cfg.ttl,
                            "registry: bucket {BUCKET} already exists with a different max_age; \
                             it governs expiry, so the heartbeat follows it. To change it, align \
                             [mesh].registry_ttl_secs across daemons and delete the bucket"
                        );
                    }
                    tick = bucket / 3;
                    store = Some(s);
                }
                let entry = DaemonEntry {
                    daemon: daemon_id.clone(),
                    host: host.clone(),
                    pid,
                    version: version.clone(),
                    started_ms,
                    heartbeat_ms: now_ms(),
                };
                let value = serde_json::to_vec(&entry).expect("entry serializes");
                let s = store.as_ref().expect("set above");
                if let Err(e) = timed(limit, "put", s.put(&daemon_id, value.into())).await {
                    // Re-open the bucket next time: it may have been deleted.
                    store = None;
                    return Err(e);
                }
                Ok(())
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
                _ = tokio::time::sleep(tick) => {}
                _ = &mut stop_rx => break,
            }
        }
        if let Some(s) = store {
            match timed(limit, "delete", s.delete(&daemon_id)).await {
                Ok(()) => tracing::info!(key = %daemon_id, "registry: deregistered"),
                Err(e) => tracing::warn!(
                    "registry: {}; the key ages out at the bucket TTL instead",
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

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
        let cfg = RegistryConfig {
            nats_url: url.clone(),
            ttl: Duration::from_secs(6),
            call_timeout: Duration::from_secs(5),
        };

        // Clean lifecycle.
        let h = spawn(cfg.clone(), "d-clean".into(), "v1".into(), 42);
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
        let h = spawn(cfg, "d-crash".into(), "v1".into(), 0);
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
        let store = open_bucket(&client, Duration::from_secs(6), Duration::from_secs(5))
            .await
            .unwrap();
        let err = bucket_ttl(&store, Duration::from_secs(5))
            .await
            .expect_err("no max_age, no markers");
        assert!(err.contains("nats kv del"), "remedy named: {err}");

        let h = spawn(
            RegistryConfig {
                nats_url: url.clone(),
                ttl: Duration::from_secs(6),
                call_timeout: Duration::from_secs(5),
            },
            "d-foreign".into(),
            "v1".into(),
            0,
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            store.get("d-foreign").await.unwrap().is_none(),
            "must not register into a bucket that never expires keys"
        );
        h.shutdown().await;
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
        let err = open_bucket(&client, Duration::from_secs(6), Duration::from_secs(5))
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
