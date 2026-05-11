//! mu-pex Phase 1 + 1.5 — end-to-end smoke test for the
//! `daemon.usage_history` RPC.
//!
//! Spawns a single `mu serve` daemon, runs two short asks against
//! anthropic-api+haiku, then queries `daemon.usage_history` and
//! asserts that the response shape and all distribution counts are
//! consistent with what the asks should have produced. Specifically
//! exercises the bug-paths discovered during the 2026-05-11
//! hand-test: that ttft_ms / streaming_ms are populated (not
//! mis-detected as `None`) and that wall_ms.count matches the ask
//! count (not contaminated by inter-ask gaps).
//!
//! Gated on `MU_LIVE_ANTHROPIC=1`. The daemon inherits
//! `ANTHROPIC_API_KEY` from the test process's env.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::{live_anthropic_enabled, MU_BIN};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Read-side handle: each line of stdout becomes a Value on the
/// channel. Non-JSON lines are logged to eprintln but otherwise
/// dropped, mirroring mu_client.py's behavior.
struct DaemonHandle {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::UnboundedReceiver<Value>,
    next_id: i64,
}

impl DaemonHandle {
    async fn spawn() -> Self {
        let mut child = Command::new(MU_BIN)
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mu serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                match serde_json::from_str::<Value>(&line) {
                    Ok(v) => {
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                    Err(_) => eprintln!("[mu serve non-json] {line}"),
                }
            }
        });
        // Drain stderr to a deque so it doesn't backpressure the daemon.
        let stderr = child.stderr.take().expect("stderr");
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(_)) = reader.next_line().await {}
        });
        Self {
            child,
            stdin,
            rx,
            next_id: 1,
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = format!("{}\n", serde_json::to_string(&msg).unwrap());
        self.stdin
            .write_all(line.as_bytes())
            .await
            .expect("write to daemon stdin");
        self.stdin.flush().await.expect("flush daemon stdin");
        id
    }

    /// Wait for a JSON-RPC response with the given id (60s cap).
    async fn wait_response(&mut self, id: i64) -> Value {
        timeout(Duration::from_secs(60), async {
            loop {
                let v = self.rx.recv().await.expect("daemon closed stdout");
                if v.get("id").and_then(Value::as_i64) == Some(id) {
                    return v;
                }
            }
        })
        .await
        .expect("response id={id} timed out")
    }

    /// Wait until predicate returns true on a message; return that message.
    async fn wait_until<F: FnMut(&Value) -> bool>(&mut self, mut pred: F) -> Value {
        timeout(Duration::from_secs(120), async {
            loop {
                let v = self.rx.recv().await.expect("daemon closed stdout");
                if pred(&v) {
                    return v;
                }
            }
        })
        .await
        .expect("predicate timed out")
    }

    async fn close(mut self) {
        drop(self.stdin); // EOF → cooperative shutdown
        let _ = timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

#[tokio::test]
async fn mu_pex_usage_history_populates_per_call_metrics_via_anthropic() {
    if !live_anthropic_enabled() {
        eprintln!(
            "skipping mu_pex_usage_history_populates_per_call_metrics_via_anthropic \
             (set MU_LIVE_ANTHROPIC=1 to run)"
        );
        return;
    }

    let mut d = DaemonHandle::spawn().await;

    // ── create_session ────────────────────────────────────────────
    let rid = d
        .send(
            "create_session",
            json!({
                "provider": {
                    "kind": "anthropic_api",
                    "model": "claude-haiku-4-5-20251001"
                }
            }),
        )
        .await;
    let resp = d.wait_response(rid).await;
    assert!(
        resp.get("error").is_none(),
        "create_session failed: {resp}"
    );
    let sid = resp["result"]["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();

    // ── ask_session × 2 ───────────────────────────────────────────
    for prompt in [
        "Just say the word HELLO. Nothing else.",
        "Just say the word WORLD. Nothing else.",
    ] {
        let rid = d
            .send(
                "ask_session",
                json!({ "session_id": &sid, "user_message": prompt }),
            )
            .await;
        let ack = d.wait_response(rid).await;
        assert!(ack.get("error").is_none(), "ask_session failed: {ack}");
        // Wait for session.done for THIS session.
        let target_sid = sid.clone();
        d.wait_until(|m| {
            m.get("method").and_then(Value::as_str) == Some("session.done")
                && m["params"]
                    .get("session_id")
                    .and_then(Value::as_str)
                    == Some(target_sid.as_str())
        })
        .await;
    }

    // ── daemon.usage_history ──────────────────────────────────────
    let rid = d.send("daemon.usage_history", json!({})).await;
    let usage = d.wait_response(rid).await;
    assert!(
        usage.get("error").is_none(),
        "daemon.usage_history failed: {usage}"
    );

    let result = &usage["result"];
    let rows = result["rows"].as_array().expect("rows array");
    assert_eq!(
        rows.len(),
        1,
        "expected exactly 1 (provider, model) row; got {}: {rows:?}",
        rows.len()
    );
    let row = &rows[0];

    // Identity columns.
    assert_eq!(row["provider_kind"].as_str(), Some("anthropic_api"));
    assert!(
        row["model"]
            .as_str()
            .map(|s| s.contains("haiku"))
            .unwrap_or(false),
        "model should contain 'haiku'; got {:?}",
        row["model"]
    );
    assert_eq!(row["session_count"].as_u64(), Some(1));

    // Wall-clock: 2 asks → 2 samples.
    let wall_count = row["wall_ms"]["count"].as_u64().unwrap_or(0);
    assert_eq!(wall_count, 2, "wall_ms.count: {row}");

    // mu-pex Phase 1.5 — TTFT and streaming should both be populated
    // (regression guard for the elapsed_ms == 0 / Done-close-out bugs
    // discovered during the 2026-05-11 hand-test).
    let ttft = row.get("ttft_ms").and_then(Value::as_object);
    assert!(
        ttft.is_some(),
        "ttft_ms missing (Phase 1.5 regression): {row}"
    );
    let ttft_count = row["ttft_ms"]["count"].as_u64().unwrap_or(0);
    assert_eq!(
        ttft_count, 2,
        "ttft_ms.count should match ask count (regression for elapsed_ms != 0 \
         emission pattern): {row}"
    );

    let streaming = row.get("streaming_ms").and_then(Value::as_object);
    assert!(
        streaming.is_some(),
        "streaming_ms missing (Phase 1.5 regression): {row}"
    );
    let streaming_count = row["streaming_ms"]["count"].as_u64().unwrap_or(0);
    assert_eq!(
        streaming_count, 2,
        "streaming_ms.count should match ask count (regression for \
         Done-driven close-out): {row}"
    );

    // Token sums non-zero (some bookkeeping actually happened).
    assert!(
        row["input_tokens_sum"].as_u64().unwrap_or(0) > 0,
        "input_tokens_sum should be > 0: {row}"
    );
    assert!(
        row["output_tokens_sum"].as_u64().unwrap_or(0) > 0,
        "output_tokens_sum should be > 0: {row}"
    );

    // ── cleanup ───────────────────────────────────────────────────
    let rid = d
        .send("close_session", json!({ "session_id": &sid }))
        .await;
    d.wait_response(rid).await;
    d.close().await;
}
