//! In-process `SessionDiscovery` backend.
//!
//! Wraps the daemon's existing `Sessions` registry. `list` derives
//! `SessionInfo` from each session's event log via
//! `derive_session_info`. `announce` / `withdraw` are no-ops — the
//! in-memory map IS the source of truth, so there's nothing to
//! propagate to.
//!
//! Zero external dependencies, zero IO. Always works. Always
//! "include_remote=false" (no peers to enumerate).
//!
//! Federating backends (FileBackend, EtcdBackend) will compose around
//! this — they'll wrap a LocalRegistryBackend, return the local
//! results for the same-daemon slice, plus their own enumeration for
//! the peer slice. That way local sessions never get lost to a
//! federation outage (INV-2 in mu-038).

use async_trait::async_trait;

use mu_core::protocol::{SessionInfo, SessionListFilter, SessionStatusSummary};

use crate::serve::sessions::Sessions;

use super::{derive_session_info, DiscoveryError, SessionDiscovery};

pub struct LocalRegistryBackend {
    sessions: Sessions,
    daemon_id: String,
}

impl LocalRegistryBackend {
    pub fn new(sessions: Sessions, daemon_id: String) -> Self {
        Self {
            sessions,
            daemon_id,
        }
    }
}

#[async_trait]
impl SessionDiscovery for LocalRegistryBackend {
    async fn list(&self, filter: &SessionListFilter) -> Result<Vec<SessionInfo>, DiscoveryError> {
        // include_remote is meaningless here (no peers). Silently
        // honor by simply not adding any remote rows — same outcome
        // as a federating backend with zero peers.
        let snapshot = self.sessions.snapshot_for_listing();
        let mut out: Vec<SessionInfo> = snapshot
            .into_iter()
            .map(|(sid, log, parent)| derive_session_info(&sid, &log, parent, &self.daemon_id))
            .filter(|info| filter_matches(info, filter))
            .collect();

        // Sort most-recently-active first — matches the TUI's expected
        // ordering and gives the limit-clamp a sensible bias.
        out.sort_by_key(|b| std::cmp::Reverse(b.last_activity_unix_ms));

        if let Some(limit) = filter.limit {
            if limit > 0 {
                out.truncate(limit as usize);
            }
        }

        Ok(out)
    }
}

fn filter_matches(info: &SessionInfo, filter: &SessionListFilter) -> bool {
    if let Some(p) = &filter.parent_session_id {
        if info.parent_session_id.as_deref() != Some(p.as_str()) {
            return false;
        }
    }
    if let Some(s) = filter.status {
        if !status_matches(info.status, s) {
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

fn status_matches(actual: SessionStatusSummary, wanted: SessionStatusSummary) -> bool {
    // v1: exact match. A future "any active" pseudo-status could be
    // added if it proves useful.
    actual == wanted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::sessions::Sessions;
    use mu_core::capability::Capability;
    use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    fn make_session(
        sessions: &Sessions,
        id: &str,
        provider_kind: &str,
        model: &str,
        parent: Option<String>,
    ) -> Arc<SessionEventLog> {
        let log = Arc::new(SessionEventLog::new(id.to_string()));
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: provider_kind.into(),
                model: model.into(),
                parent_session_id: parent.clone(),
                branched_at_parent_event_id: None,
                usage_semantics: None,
            },
        );
        let (tx, _rx) = mpsc::channel(1);
        let forwarder = tokio::spawn(async {});
        let agent = tokio::spawn(async {});
        let approvals = Arc::new(Mutex::new(HashMap::new()));
        let cap = Arc::new(Mutex::new(Capability::root()));
        let provider_status = Arc::new(Mutex::new(
            crate::serve::provider_status::ProviderStatusTracker::new(),
        ));
        sessions.insert(
            id.to_string(),
            crate::serve::sessions::NewSession {
                input_tx: tx,
                forwarder,
                agent,
                event_log: log.clone(),
                pending_approvals: approvals,
                parent_session_id: parent,
                capability: cap,
                provider_status,
                mailbox: Arc::new(crate::serve::mailbox::MailboxState::new()),
                status_watch: None,
            },
        );
        log
    }

    #[tokio::test]
    async fn list_returns_local_sessions_with_metadata() {
        let sessions = Sessions::new();
        let _a = make_session(&sessions, "s1", "anthropic_api", "haiku", None);
        let _b = make_session(&sessions, "s2", "openai_codex", "gpt-5.5", None);

        let backend = LocalRegistryBackend::new(sessions, "daemon-test".into());
        let out = backend
            .list(&SessionListFilter::default())
            .await
            .expect("list ok");
        assert_eq!(out.len(), 2);

        // Both session ids appear; ordering by last_activity is
        // intentionally not asserted (sessions created within the
        // same millisecond have equal sort keys, so order is
        // unstable in a fast test).
        let ids: std::collections::HashSet<String> =
            out.iter().map(|s| s.session_id.clone()).collect();
        assert!(ids.contains("s1"));
        assert!(ids.contains("s2"));

        // daemon_id propagates.
        assert!(out.iter().all(|s| s.daemon_id == "daemon-test"));
        // is_remote is always false for local backend.
        assert!(out.iter().all(|s| !s.is_remote));
        // provider_kind / model captured from SessionCreated.
        let s1 = out.iter().find(|s| s.session_id == "s1").unwrap();
        let s2 = out.iter().find(|s| s.session_id == "s2").unwrap();
        assert_eq!(s1.provider_kind, "anthropic_api");
        assert_eq!(s1.model, "haiku");
        assert_eq!(s2.provider_kind, "openai_codex");
        assert_eq!(s2.model, "gpt-5.5");
    }

    #[tokio::test]
    async fn list_filter_by_parent() {
        let sessions = Sessions::new();
        let _root = make_session(&sessions, "root", "anthropic_api", "haiku", None);
        let _child = make_session(
            &sessions,
            "child",
            "anthropic_api",
            "haiku",
            Some("root".into()),
        );
        let backend = LocalRegistryBackend::new(sessions, "d".into());
        let filter = SessionListFilter {
            parent_session_id: Some("root".into()),
            ..Default::default()
        };
        let out = backend.list(&filter).await.expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "child");
    }

    #[tokio::test]
    async fn list_filter_by_limit() {
        let sessions = Sessions::new();
        for i in 0..5 {
            make_session(&sessions, &format!("s{i}"), "anthropic_api", "haiku", None);
        }
        let backend = LocalRegistryBackend::new(sessions, "d".into());
        let filter = SessionListFilter {
            limit: Some(2),
            ..Default::default()
        };
        let out = backend.list(&filter).await.expect("ok");
        assert_eq!(out.len(), 2);
    }
}
