//! Per-session task guards (bead mu-0xhja).
//!
//! A session runs two sibling tasks joined by a pipe: the agent loop
//! (source) and the event forwarder (sink). Close asks the source to stop
//! by dropping the session's input channel; the shutdown then flows down
//! the pipe — the loop exits, its dropped event sender lets the forwarder
//! drain and exit. The supervisor spawned at close joins each task in
//! that same source→sink order and records how it ended and how long it
//! took; joining the sink first would just wait on a pipe the source
//! hasn't closed yet.
//!
//! No timeout, no abort: close never waits, and a task that never exits
//! is surfaced (draining registry → `session.stats` `live_guards`, plus
//! per-join elapsed-time logs) rather than killed on a clock. Rationale
//! and history: bead mu-0xhja.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;

/// How one guard's join ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardOutcome {
    /// The task finished on its own.
    Completed,
    /// The task had panicked — surfaced so a close can't silently absorb
    /// a crash.
    Panicked,
    /// The task was cancelled from elsewhere (nothing in this module
    /// aborts; an external abort still joins here).
    Cancelled,
}

impl GuardOutcome {
    fn as_str(self) -> &'static str {
        match self {
            GuardOutcome::Completed => "completed",
            GuardOutcome::Panicked => "panicked",
            GuardOutcome::Cancelled => "cancelled",
        }
    }
}

struct GuardEntry {
    label: &'static str,
    handle: JoinHandle<()>,
}

/// Ordered, labeled collection of a session's live task guards.
#[derive(Default)]
pub(crate) struct SessionGuards {
    entries: Vec<GuardEntry>,
}

/// Shared registry of sessions whose tasks are still draining after
/// close: session id → labels not yet joined. The supervisor removes
/// labels as tasks finish and drops the entry when the drain completes,
/// so a lookup answers "is anything from that closed session still
/// running, and what?" at any moment.
pub(crate) type DrainingMap = Arc<Mutex<HashMap<String, Vec<&'static str>>>>;

impl SessionGuards {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register a task guard. Callers register in dataflow order
    /// (source before sink) — see the module docs.
    pub(crate) fn push_task(&mut self, label: &'static str, handle: JoinHandle<()>) {
        self.entries.push(GuardEntry { label, handle });
    }

    /// The live guard labels, in registration order.
    pub(crate) fn labels(&self) -> Vec<&'static str> {
        self.entries.iter().map(|e| e.label).collect()
    }

    /// Spawn the detached supervisor: join each task in source→sink
    /// order, no timeout, no abort; log each outcome with elapsed time
    /// and keep `draining` current. Returns immediately.
    ///
    /// Drop the session's input channel before calling this — that is
    /// what asks the tasks to stop.
    pub(crate) fn supervise(self, session_id: String, draining: DrainingMap) {
        if self.entries.is_empty() {
            return;
        }
        if let Ok(mut map) = draining.lock() {
            map.insert(session_id.clone(), self.labels());
        }
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            for entry in self.entries {
                let outcome = match entry.handle.await {
                    Ok(()) => GuardOutcome::Completed,
                    Err(e) if e.is_panic() => GuardOutcome::Panicked,
                    Err(_) => GuardOutcome::Cancelled,
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                match outcome {
                    GuardOutcome::Completed => tracing::info!(
                        session_id = %session_id,
                        guard = entry.label,
                        elapsed_ms,
                        outcome = outcome.as_str(),
                        "session guard drained"
                    ),
                    _ => tracing::warn!(
                        session_id = %session_id,
                        guard = entry.label,
                        elapsed_ms,
                        outcome = outcome.as_str(),
                        "session guard drained abnormally"
                    ),
                }
                if let Ok(mut map) = draining.lock() {
                    if let Some(labels) = map.get_mut(&session_id) {
                        labels.retain(|l| *l != entry.label);
                    }
                }
            }
            if let Ok(mut map) = draining.lock() {
                map.remove(&session_id);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn draining() -> DrainingMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// Poll until `cond` holds or ~2s passes (test-only bound; the
    /// production path has no timer).
    async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn finished_tasks_drain_and_clear_the_registry() {
        let mut guards = SessionGuards::new();
        guards.push_task("agent-loop", tokio::spawn(async {}));
        guards.push_task("forwarder", tokio::spawn(async {}));
        let map = draining();
        guards.supervise("s1".to_string(), Arc::clone(&map));
        assert!(
            wait_for(|| map.lock().unwrap().is_empty()).await,
            "drain completes and removes the session entry"
        );
    }

    #[tokio::test]
    async fn wedged_task_stays_visible_and_close_path_never_blocks() {
        let mut guards = SessionGuards::new();
        guards.push_task(
            "agent-loop",
            tokio::spawn(async {
                std::future::pending::<()>().await;
            }),
        );
        let map = draining();
        let start = std::time::Instant::now();
        guards.supervise("s2".to_string(), Arc::clone(&map));
        // supervise() returns immediately — no join on the caller's path.
        assert!(start.elapsed() < Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            map.lock().unwrap().get("s2"),
            Some(&vec!["agent-loop"]),
            "a task that never exits stays visible in the draining registry"
        );
    }

    #[tokio::test]
    async fn panicked_task_still_clears_and_is_not_fatal() {
        let mut guards = SessionGuards::new();
        guards.push_task(
            "agent-loop",
            tokio::spawn(async {
                panic!("boom");
            }),
        );
        let map = draining();
        guards.supervise("s3".to_string(), Arc::clone(&map));
        assert!(
            wait_for(|| map.lock().unwrap().is_empty()).await,
            "a panicked task joins as Panicked and the drain still completes"
        );
    }
}
