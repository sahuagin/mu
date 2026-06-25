//! Per-session dialogue poller (mu-dialogue-inbound-wakeup).
//!
//! Inbound dialogue is otherwise pull-only: a live session never learns a
//! peer wrote to it unless the model itself decides to call `dialogue_poll`.
//! This module makes delivery event-driven. For each live session that has a
//! session-bound `dialogue_poll` tool, the daemon spawns one background task
//! that long-polls the tool and injects every fresh message into the agent
//! loop as [`AgentInput::DialogueMessage`] over the session's own input
//! channel — the same "wakeup channel" the watch tool and mailbox use. The
//! loop synthesizes an inline user message and runs the model, so a peer's
//! message wakes an idle session the moment it arrives.
//!
//! Cursor discipline. The first poll is forward-only by timestamp: `since` is
//! wall-clock now minus 1 ms (so the startup millisecond is inclusive rather
//! than racing the first poll), and nothing older is ever replayed. Every poll
//! after the first message uses `after_seq` — the server's `seq`, a strictly
//! insertion-monotonic token (SQLite `rowid`). Keying on `seq` rather than `ts`
//! (millisecond-coarse) or the message `id` (a ULID whose within-millisecond
//! order is RANDOM) is what makes delivery exactly-once: a later insert always
//! gets a higher `seq`, so a same-millisecond burst of any size pages through
//! with no starved tail and no skipped concurrent insert, and no dedup
//! bookkeeping is needed.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mu_core::agent::{AgentInput, Tool};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Long-poll window handed to the `dialogue_poll` tool. The tool blocks up
/// to this long (or until a message arrives) before returning empty, so the
/// poller spends almost all its time parked rather than spinning.
const POLL_TIMEOUT_MS: u64 = 30_000;
/// Upper bound on messages returned per poll (the server clamps to its own
/// max, currently 200).
const POLL_LIMIT: i64 = 200;
/// Backoff after a poll error or an unparseable result, so a persistently
/// failing server can't turn the poller into a hot loop. Cancellation still
/// interrupts the wait promptly.
const ERROR_BACKOFF_MS: u64 = 1_000;

/// Owned handle to a running dialogue poller, stored in the session's
/// registry state. Dropping it does NOT stop the task on its own — the task
/// also self-terminates when the agent loop's input receiver is gone — but
/// [`shutdown_and_join`](Self::shutdown_and_join) gives a deterministic
/// teardown: signal, then await the task.
pub(crate) struct DialoguePollerHandle {
    cancel_tx: oneshot::Sender<()>,
    join: JoinHandle<()>,
}

impl DialoguePollerHandle {
    /// Signal the poller to stop and await its task. The in-flight poll (if
    /// any) is dropped at the next `select!` boundary, so this returns
    /// promptly. Must NOT be called while holding the sessions mutex — it
    /// awaits.
    pub(crate) async fn shutdown_and_join(self) {
        let _ = self.cancel_tx.send(());
        // A clean cooperative stop joins as `Ok(())`; only a panic surfaces as
        // an error. Log that rather than swallow it, so a broken poller is
        // diagnosable instead of silently vanishing at session close.
        if let Err(e) = self.join.await {
            tracing::warn!(error = %e, "dialogue poller task did not exit cleanly");
        }
    }
}

/// One message as returned in a `dialogue_poll` result's `messages` array.
/// Only the fields the poller needs are deserialized; the rest (`id`, `to`,
/// `ts`, `session_thread`) are ignored — the cursor is driven entirely by
/// `seq` (the server's insertion-order token).
///
/// `seq` is REQUIRED, deliberately not `#[serde(default)]`: a response missing
/// it (e.g. an older dialogue server deployed before this cursor existed) must
/// fail to parse — surfacing as a logged poll error and a safe backoff — rather
/// than silently defaulting to `0` and cursoring on `rowid > 0`, which would
/// replay or skip messages. Fail closed and visible on version skew, never
/// silently wrong. (See the deploy-ordering note: server before client.)
#[derive(Deserialize)]
struct PollMsg {
    seq: i64,
    #[serde(default)]
    from: String,
    #[serde(default)]
    content: String,
}

/// Shape of a `dialogue_poll` result body: `{ "messages": [...] }`.
#[derive(Deserialize)]
struct PollBatch {
    #[serde(default)]
    messages: Vec<PollMsg>,
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Spawn the per-session poller. `poll_tool` is the session-bound
/// `dialogue_poll` tool (its `to` argument already defaults to this
/// session's peer id, so the poller need not supply it); `input_tx` is a
/// clone of the agent loop's input sender; `peer_id` is used only for
/// diagnostic logging.
pub(crate) fn spawn_dialogue_poller(
    poll_tool: Arc<dyn Tool>,
    input_tx: mpsc::Sender<AgentInput>,
    peer_id: String,
) -> DialoguePollerHandle {
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(poll_loop(poll_tool, input_tx, peer_id, cancel_rx));
    DialoguePollerHandle { cancel_tx, join }
}

async fn poll_loop(
    poll_tool: Arc<dyn Tool>,
    input_tx: mpsc::Sender<AgentInput>,
    peer_id: String,
    mut cancel_rx: oneshot::Receiver<()>,
) {
    // Forward-only until the first message arrives: `since` bounds the first
    // poll by timestamp, inclusive of the startup millisecond (subtract 1 ms so
    // the server's exclusive `ts > since` matches `ts >= now` — a message that
    // races poller startup within the same coarse millisecond is delivered, not
    // dropped; nothing with an earlier `ts` is replayed). Once any message has
    // been delivered, `after_seq` (the server's strictly-insertion-monotonic
    // cursor) takes over and `since` is moot: every later message has a higher
    // `seq` and is delivered exactly once, immune to millisecond coarseness and
    // to ULID id randomness within a millisecond.
    let since: i64 = now_unix_ms() - 1;
    let mut after_seq: Option<i64> = None;

    loop {
        let mut args = serde_json::json!({
            "since": since,
            "timeout_ms": POLL_TIMEOUT_MS,
            "limit": POLL_LIMIT,
        });
        if let Some(seq) = after_seq {
            args["after_seq"] = serde_json::Value::from(seq);
        }
        // The tool requires a cancel receiver; hold its sender across the
        // select so the receiver doesn't fire spuriously. When our own
        // cancel fires we `break`, dropping the execute future (and this
        // sender) — which cancels the in-flight call promptly.
        let (_tool_cancel_tx, tool_cancel_rx) = oneshot::channel::<()>();
        let result = tokio::select! {
            _ = &mut cancel_rx => break,
            r = poll_tool.execute(args, tool_cancel_rx) => r,
        };

        if result.is_error {
            tracing::debug!(
                peer = %peer_id,
                error = %result.content,
                "dialogue poll errored; backing off and retrying",
            );
            if backoff_or_cancel(&mut cancel_rx).await {
                break;
            }
            continue;
        }

        let batch: PollBatch = match serde_json::from_str(&result.content) {
            Ok(b) => b,
            Err(e) => {
                // Warn, not debug: an unparseable poll result is a contract
                // violation (most likely version skew — a dialogue server
                // predating the `seq` cursor), and this is an always-on
                // delivery path, so a silent stall would be hard to diagnose.
                tracing::warn!(
                    peer = %peer_id,
                    error = %e,
                    "dialogue poll: unparseable result (server too old / protocol mismatch?); backing off and retrying",
                );
                if backoff_or_cancel(&mut cancel_rx).await {
                    break;
                }
                continue;
            }
        };

        if batch.messages.is_empty() {
            // Long-poll timed out with nothing new — poll again immediately.
            continue;
        }

        // The server returns rows in `seq` order, strictly after the cursor, so
        // each is new — no dedup needed. Advance the keyset cursor as we go; a
        // full page just means the next poll fetches the next one.
        for m in batch.messages {
            after_seq = Some(m.seq);
            // Race the delivery against cancellation: a full agent-input
            // channel must never wedge teardown (the send would otherwise
            // block until the loop drains).
            let send = input_tx.send(AgentInput::DialogueMessage {
                from: m.from,
                content: m.content,
            });
            tokio::select! {
                _ = &mut cancel_rx => return,
                res = send => {
                    if res.is_err() {
                        // The agent loop is gone — the session ended.
                        return;
                    }
                }
            }
        }
    }
}

/// Wait out the error backoff, returning `true` if cancellation fired first
/// (caller should stop), `false` if the backoff elapsed (caller continues).
async fn backoff_or_cancel(cancel_rx: &mut oneshot::Receiver<()>) -> bool {
    tokio::select! {
        _ = cancel_rx => true,
        _ = tokio::time::sleep(Duration::from_millis(ERROR_BACKOFF_MS)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mu_core::agent::{ToolResult, ToolSpec};
    use serde_json::{json, Value};

    #[derive(Clone)]
    struct Row {
        /// Insertion-order token (the server's `seq`/rowid). Set independently
        /// of `id` so a test can prove the cursor follows `seq`, not `id`.
        seq: i64,
        id: &'static str,
        ts: i64,
        from: &'static str,
        content: &'static str,
    }

    fn row_json(r: &Row) -> Value {
        json!({
            "seq": r.seq,
            "id": r.id,
            "from": r.from,
            "to": "mu:d1:s1",
            "session_thread": Value::Null,
            "content": r.content,
            "ts": r.ts,
        })
    }

    /// A fake `dialogue_poll` tool backed by a fixed table of rows, mirroring
    /// the real server's two cursor modes: `after_seq = Some(s)` filters
    /// `seq > s`; otherwise it filters `ts > since` (the forward-only first
    /// poll). Rows are returned in `seq` (insertion) order, truncated to `page`
    /// (the server's per-response limit). When nothing is past the cursor it
    /// long-polls — blocks until cancelled, then returns empty — like a real
    /// idle inbox, so the poller parks instead of spinning.
    struct FakePollTool {
        rows: Vec<Row>,
        page: usize,
    }

    impl FakePollTool {
        fn arc(rows: Vec<Row>, page: usize) -> Arc<dyn Tool> {
            Arc::new(Self { rows, page })
        }
    }

    #[async_trait]
    impl Tool for FakePollTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "dialogue_poll".into(),
                ..Default::default()
            }
        }

        async fn execute(&self, arguments: Value, cancel_rx: oneshot::Receiver<()>) -> ToolResult {
            let since = arguments.get("since").and_then(Value::as_i64).unwrap_or(0);
            let after_seq = arguments.get("after_seq").and_then(Value::as_i64);
            let mut matching: Vec<Row> = self
                .rows
                .iter()
                .filter(|r| match after_seq {
                    Some(s) => r.seq > s,
                    None => r.ts > since,
                })
                .cloned()
                .collect();
            matching.sort_by_key(|r| r.seq);
            matching.truncate(self.page);
            if matching.is_empty() {
                // Nothing past the cursor — park like a real idle long-poll.
                let _ = cancel_rx.await;
                return ToolResult {
                    content: json!({ "messages": [] }).to_string(),
                    is_error: false,
                };
            }
            let msgs: Vec<Value> = matching.iter().map(row_json).collect();
            ToolResult {
                content: json!({ "messages": msgs }).to_string(),
                is_error: false,
            }
        }
    }

    /// Drain up to `max` injected dialogue messages within `budget`,
    /// returning the (from, content) pairs seen.
    async fn drain(
        rx: &mut mpsc::Receiver<AgentInput>,
        max: usize,
        budget: Duration,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + budget;
        while out.len() < max {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(AgentInput::DialogueMessage { from, content })) => {
                    out.push((from, content));
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => break,
            }
        }
        out
    }

    /// Forward-only init: a message that predates the poller (ts below the
    /// start cursor) is never delivered.
    #[tokio::test]
    async fn forward_only_init_drops_pre_start_message() {
        // ts = 0 is guaranteed below now_unix_ms(), so the first-poll
        // forward-only filter (ts > since) drops it.
        let tool = FakePollTool::arc(
            vec![Row {
                seq: 1,
                id: "old-1",
                ts: 0,
                from: "cc:peer",
                content: "stale",
            }],
            10,
        );
        let (tx, mut rx) = mpsc::channel::<AgentInput>(8);
        let poller = spawn_dialogue_poller(tool, tx, "mu:d1:s1".into());

        let got = drain(&mut rx, 1, Duration::from_millis(300)).await;
        assert!(
            got.is_empty(),
            "pre-start message must not be delivered, got {got:?}"
        );
        poller.shutdown_and_join().await;
    }

    /// Two messages sharing the same millisecond are both delivered.
    #[tokio::test]
    async fn same_ts_messages_both_delivered() {
        let ts = now_unix_ms() + 1_000_000; // safely above the start cursor
        let tool = FakePollTool::arc(
            vec![
                Row {
                    seq: 1,
                    id: "m-a",
                    ts,
                    from: "cc:peer",
                    content: "first",
                },
                Row {
                    seq: 2,
                    id: "m-b",
                    ts,
                    from: "cc:peer",
                    content: "second",
                },
            ],
            10,
        );
        let (tx, mut rx) = mpsc::channel::<AgentInput>(8);
        let poller = spawn_dialogue_poller(tool, tx, "mu:d1:s1".into());

        let got = drain(&mut rx, 2, Duration::from_millis(500)).await;
        let mut contents: Vec<&str> = got.iter().map(|(_, c)| c.as_str()).collect();
        contents.sort_unstable();
        assert_eq!(got.len(), 2, "both same-ts messages delivered, got {got:?}");
        assert_eq!(contents, vec!["first", "second"]);
        poller.shutdown_and_join().await;
    }

    /// A same-millisecond burst larger than one page is delivered completely
    /// and in INSERTION order — the `seq` keyset pages through it with no
    /// starved tail and no skipped concurrent insert. The `id`s descend while
    /// `seq` ascends, so a cursor that keyed on the ULID `id` would mis-order or
    /// skip; correctness here proves the cursor follows `seq`.
    #[tokio::test]
    async fn same_ts_burst_pages_through_beyond_limit() {
        let ts = now_unix_ms() + 1_000_000;
        let rows = vec![
            Row {
                seq: 1,
                id: "m-05",
                ts,
                from: "cc:peer",
                content: "c1",
            },
            Row {
                seq: 2,
                id: "m-04",
                ts,
                from: "cc:peer",
                content: "c2",
            },
            Row {
                seq: 3,
                id: "m-03",
                ts,
                from: "cc:peer",
                content: "c3",
            },
            Row {
                seq: 4,
                id: "m-02",
                ts,
                from: "cc:peer",
                content: "c4",
            },
            Row {
                seq: 5,
                id: "m-01",
                ts,
                from: "cc:peer",
                content: "c5",
            },
        ];
        // Page size 2 (< 5) forces the cursor to advance within one ms.
        let tool = FakePollTool::arc(rows, 2);
        let (tx, mut rx) = mpsc::channel::<AgentInput>(8);
        let poller = spawn_dialogue_poller(tool, tx, "mu:d1:s1".into());

        let got = drain(&mut rx, 5, Duration::from_millis(800)).await;
        let contents: Vec<&str> = got.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(
            contents,
            vec!["c1", "c2", "c3", "c4", "c5"],
            "all same-ms messages delivered exactly once, in order, got {got:?}"
        );
        poller.shutdown_and_join().await;
    }

    /// Version-skew safety: a poll result whose messages omit the required
    /// `seq` (an older server that predates the cursor) must NOT be delivered.
    /// It fails to parse, so the poller backs off rather than cursoring on a
    /// silently-defaulted `seq = 0`.
    #[tokio::test]
    async fn missing_seq_is_not_delivered() {
        struct NoSeqTool;
        #[async_trait]
        impl Tool for NoSeqTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "dialogue_poll".into(),
                    ..Default::default()
                }
            }
            async fn execute(&self, _args: Value, _cancel: oneshot::Receiver<()>) -> ToolResult {
                // A well-formed body, but the message has no `seq` field —
                // exactly what a pre-cursor server returns.
                ToolResult {
                    content: json!({
                        "messages": [{ "from": "cc:peer", "content": "x", "ts": 123 }]
                    })
                    .to_string(),
                    is_error: false,
                }
            }
        }
        let (tx, mut rx) = mpsc::channel::<AgentInput>(8);
        let poller = spawn_dialogue_poller(Arc::new(NoSeqTool), tx, "mu:d1:s1".into());

        let got = drain(&mut rx, 1, Duration::from_millis(300)).await;
        assert!(
            got.is_empty(),
            "a message missing `seq` must not be delivered, got {got:?}"
        );
        poller.shutdown_and_join().await;
    }

    /// Lifecycle: a poller blocked on a long-poll tears down promptly when
    /// signalled.
    #[tokio::test]
    async fn shutdown_joins_promptly_while_long_polling() {
        // Empty table → the fake immediately long-polls (parks until cancel).
        let tool = FakePollTool::arc(vec![], 10);
        let (tx, _rx) = mpsc::channel::<AgentInput>(8);
        let poller = spawn_dialogue_poller(tool, tx, "mu:d1:s1".into());

        // Give the task a moment to enter the long-poll.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(2), poller.shutdown_and_join())
            .await
            .expect("poller must tear down promptly when signalled");
    }
}
