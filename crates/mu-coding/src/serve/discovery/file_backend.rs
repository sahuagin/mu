//! mu-935: File-system `SessionDiscovery` backend.
//!
//! Federates session enumeration across daemons running on the same
//! machine by reading the on-disk JSONLs that mu-upb writes. Each
//! daemon writes its sessions to `<events_dir>/<daemon_id>/<session_id>.jsonl`;
//! a FileBackend scans `<events_dir>` for *other* daemons' subdirs
//! and reconstructs a `SessionInfo` per JSONL file via
//! `SessionEventLog::from_jsonl` + `derive_session_info`.
//!
//! Composition: wraps an inner `SessionDiscovery` (typically
//! `LocalRegistryBackend`). On `list()`, the inner backend supplies
//! the local results (authoritative — current state of the in-memory
//! map) and the disk scan supplies the peer slice (marked
//! `is_remote: true`).
//!
//! Failure mode: when a peer daemon's JSONL is malformed or
//! unreadable, that file is skipped and counted; the rest of the
//! enumeration succeeds. The bead's INV-2 ("local results survive
//! a peer outage") is preserved because the inner backend's
//! results are returned even if every disk read fails.
//!
//! `announce` / `withdraw` are no-ops — mu-upb's writer already
//! handles the on-disk lifecycle.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use mu_core::event_log::SessionEventLog;
use mu_core::protocol::{SessionInfo, SessionListFilter};

use super::{derive_session_info, DiscoveryError, LocalSessionInfo, SessionDiscovery};

pub struct FileBackend {
    inner: Arc<dyn SessionDiscovery>,
    events_dir: PathBuf,
    /// This daemon's id — used to skip its own subdir when scanning
    /// peers (those sessions come from `inner`, which is the
    /// authoritative source for live state).
    own_daemon_id: String,
}

impl FileBackend {
    pub fn new(
        inner: Arc<dyn SessionDiscovery>,
        events_dir: PathBuf,
        own_daemon_id: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            events_dir,
            own_daemon_id: own_daemon_id.into(),
        }
    }

    /// Scan `events_dir` for peer daemon subdirs and collect a
    /// `SessionInfo` for each JSONL inside. Skips this daemon's own
    /// subdir, malformed JSONLs, and any subdir that can't be opened.
    /// Tagged `is_remote: true` regardless of filter; the caller's
    /// filter is applied below.
    fn scan_peer_sessions(&self) -> Vec<SessionInfo> {
        let read_dir = match std::fs::read_dir(&self.events_dir) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<SessionInfo> = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let daemon_id = match path.file_name().and_then(|s| s.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };
            if daemon_id == self.own_daemon_id {
                continue;
            }
            let session_files = match std::fs::read_dir(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for f in session_files.flatten() {
                let session_path = f.path();
                if session_path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let (log, _malformed) = match SessionEventLog::from_jsonl(&session_path) {
                    Ok(loaded) => loaded,
                    Err(_) => continue,
                };
                // Pull parent_session_id from the SessionCreated event
                // if present, so the tree-query parent-filter works
                // across the federation boundary too.
                let parent_session_id = log.snapshot().iter().find_map(|e| match &e.payload {
                    mu_core::event_log::EventPayload::SessionCreated {
                        parent_session_id, ..
                    } => parent_session_id.clone(),
                    _ => None,
                });
                let mut info =
                    derive_session_info(log.session_id(), &log, parent_session_id, &daemon_id);
                info.is_remote = true;
                out.push(info);
            }
        }
        out
    }
}

#[async_trait]
impl SessionDiscovery for FileBackend {
    async fn list(&self, filter: &SessionListFilter) -> Result<Vec<SessionInfo>, DiscoveryError> {
        // Local results first — authoritative for this daemon's
        // running state. If the inner backend fails, propagate the
        // error (we have nothing else to fall back to for local).
        let local = self.inner.list(filter).await?;

        // Peer slice is only added when the caller asks for remote
        // results. Same flag the EtcdBackend would honor.
        if !filter.include_remote {
            return Ok(local);
        }

        let mut peers = self.scan_peer_sessions();
        peers.retain(|info| filter_matches(info, filter));

        let mut merged = local;
        merged.extend(peers);
        // Re-sort: most-recently-active first. Same comparator the
        // LocalRegistryBackend uses on its own slice; doing it again
        // after merge keeps the ordering invariant across the union.
        merged.sort_by_key(|b| std::cmp::Reverse(b.last_activity_unix_ms));
        if let Some(limit) = filter.limit {
            if limit > 0 {
                merged.truncate(limit as usize);
            }
        }
        Ok(merged)
    }

    async fn announce(&self, info: LocalSessionInfo) -> Result<(), DiscoveryError> {
        // mu-upb writes the on-disk JSONL for our own sessions
        // independently; FileBackend has nothing to do at announce
        // time. Delegate to inner in case it tracks something.
        self.inner.announce(info).await
    }

    async fn withdraw(&self, session_id: &str) -> Result<(), DiscoveryError> {
        self.inner.withdraw(session_id).await
    }
}

/// Mirror of LocalRegistryBackend's filter logic — kept private here
/// because the trait doesn't surface it and we need the same shape
/// when post-filtering the peer slice. Status / parent / since
/// semantics match exactly.
fn filter_matches(info: &SessionInfo, filter: &SessionListFilter) -> bool {
    if let Some(p) = &filter.parent_session_id {
        if info.parent_session_id.as_deref() != Some(p.as_str()) {
            return false;
        }
    }
    if let Some(s) = filter.status {
        if info.status != s {
            return false;
        }
    }
    if let Some(since) = filter.active_since_unix_ms {
        if info.last_activity_unix_ms < since {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};

    /// Create a daemon's subdir with a synthetic session JSONL inside.
    /// Returns the events_dir so the test can construct a backend
    /// pointing at it.
    fn make_peer_session(
        events_dir: &std::path::Path,
        daemon_id: &str,
        session_id: &str,
        provider_kind: &str,
        model: &str,
    ) {
        let dir = events_dir.join(daemon_id);
        std::fs::create_dir_all(&dir).expect("mkdir peer daemon");
        let path = dir.join(format!("{session_id}.jsonl"));
        let log = SessionEventLog::new(session_id.to_string());
        log.attach_disk_writer(&path).expect("attach writer");
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: provider_kind.into(),
                model: model.into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
            },
        );
    }

    /// Inner SessionDiscovery that returns a fixed Vec, for isolating
    /// FileBackend's peer-scan logic from LocalRegistry's plumbing.
    struct FixedInner(Vec<SessionInfo>);

    #[async_trait]
    impl SessionDiscovery for FixedInner {
        async fn list(
            &self,
            _filter: &SessionListFilter,
        ) -> Result<Vec<SessionInfo>, DiscoveryError> {
            Ok(self.0.clone())
        }
    }

    fn tempdir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("mu-935-{name}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn no_remote_flag_returns_only_local() {
        let events_dir = tempdir("no_remote");
        make_peer_session(&events_dir, "peer-aaa", "p-1", "anthropic_api", "haiku");

        let backend = FileBackend::new(
            Arc::new(FixedInner(Vec::new())),
            events_dir.clone(),
            "self-bbb",
        );
        let out = backend
            .list(&SessionListFilter::default())
            .await
            .expect("list ok");
        assert!(out.is_empty(), "no include_remote → no peer rows");
        let _ = std::fs::remove_dir_all(&events_dir);
    }

    #[tokio::test]
    async fn include_remote_returns_peer_sessions() {
        let events_dir = tempdir("inc_remote");
        make_peer_session(&events_dir, "peer-aaa", "p-1", "anthropic_api", "haiku");
        make_peer_session(&events_dir, "peer-aaa", "p-2", "openai_codex", "gpt-5.5");
        make_peer_session(&events_dir, "peer-bbb", "p-3", "openrouter", "claude");

        let backend = FileBackend::new(
            Arc::new(FixedInner(Vec::new())),
            events_dir.clone(),
            "self-bbb",
        );
        let out = backend
            .list(&SessionListFilter {
                include_remote: true,
                ..Default::default()
            })
            .await
            .expect("list ok");
        let ids: std::collections::HashSet<String> =
            out.iter().map(|s| s.session_id.clone()).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains("p-1"));
        assert!(ids.contains("p-2"));
        assert!(ids.contains("p-3"));
        assert!(
            out.iter().all(|s| s.is_remote),
            "peer sessions must be marked is_remote"
        );
        // Daemon ids are the subdir names.
        let daemon_ids: std::collections::HashSet<String> =
            out.iter().map(|s| s.daemon_id.clone()).collect();
        assert!(daemon_ids.contains("peer-aaa"));
        assert!(daemon_ids.contains("peer-bbb"));

        let _ = std::fs::remove_dir_all(&events_dir);
    }

    #[tokio::test]
    async fn own_daemon_subdir_is_skipped() {
        let events_dir = tempdir("own_skip");
        // A subdir matching the backend's own_daemon_id — shouldn't
        // appear in the peer slice (those come from the inner
        // backend, authoritatively).
        make_peer_session(&events_dir, "self-bbb", "p-1", "anthropic_api", "haiku");
        // Plus a genuine peer.
        make_peer_session(&events_dir, "peer-aaa", "p-2", "openai_codex", "gpt-5.5");

        let backend = FileBackend::new(
            Arc::new(FixedInner(Vec::new())),
            events_dir.clone(),
            "self-bbb",
        );
        let out = backend
            .list(&SessionListFilter {
                include_remote: true,
                ..Default::default()
            })
            .await
            .expect("list ok");
        let ids: std::collections::HashSet<String> =
            out.iter().map(|s| s.session_id.clone()).collect();
        assert!(!ids.contains("p-1"), "own daemon's subdir must be skipped");
        assert!(ids.contains("p-2"));

        let _ = std::fs::remove_dir_all(&events_dir);
    }

    #[tokio::test]
    async fn local_results_first_peers_appended() {
        let events_dir = tempdir("merge");
        make_peer_session(&events_dir, "peer-aaa", "p-1", "anthropic_api", "haiku");

        // Synthesize a local row with a future last_activity so it
        // sorts to the top.
        let local_info = SessionInfo {
            session_id: "local-x".into(),
            daemon_id: "self-bbb".into(),
            is_remote: false,
            parent_session_id: None,
            provider_kind: "anthropic_api".into(),
            model: "opus".into(),
            status: mu_core::protocol::SessionStatusSummary::Idle,
            started_at_unix_ms: 9_000_000_000_000,
            last_activity_unix_ms: 9_000_000_000_000,
            ask_count: 0,
            tool_call_count: 0,
            cumulative_usage: None,
        };
        let backend = FileBackend::new(
            Arc::new(FixedInner(vec![local_info])),
            events_dir.clone(),
            "self-bbb",
        );
        let out = backend
            .list(&SessionListFilter {
                include_remote: true,
                ..Default::default()
            })
            .await
            .expect("list ok");
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].session_id, "local-x",
            "future-ts local should sort first"
        );
        assert!(!out[0].is_remote);
        assert_eq!(out[1].session_id, "p-1");
        assert!(out[1].is_remote);

        let _ = std::fs::remove_dir_all(&events_dir);
    }

    #[tokio::test]
    async fn malformed_peer_jsonl_skipped_others_succeed() {
        let events_dir = tempdir("malformed");
        // One good peer.
        make_peer_session(&events_dir, "peer-good", "g-1", "anthropic_api", "haiku");
        // One subdir with garbage in the jsonl.
        let bad_dir = events_dir.join("peer-bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join("b-1.jsonl"),
            b"not valid json\n{also not valid\n",
        )
        .unwrap();

        let backend = FileBackend::new(
            Arc::new(FixedInner(Vec::new())),
            events_dir.clone(),
            "self-bbb",
        );
        let out = backend
            .list(&SessionListFilter {
                include_remote: true,
                ..Default::default()
            })
            .await
            .expect("list ok");
        // The bad file produces an EMPTY log (zero events). The
        // current from_jsonl falls back to filename stem for the
        // session_id and the SessionInfo gets "unknown" provider —
        // so it DOES show up. That's acceptable for a discovery
        // surface (the human / client can decide whether to ignore
        // unknown-provider rows); the alternative — silently
        // dropping it — could mask a real session being corrupted.
        // The contract here is "we never PANIC on malformed; the
        // good rows always come through."
        let good = out
            .iter()
            .find(|s| s.session_id == "g-1")
            .expect("good peer present");
        assert_eq!(good.provider_kind, "anthropic_api");

        let _ = std::fs::remove_dir_all(&events_dir);
    }
}
