//! mu-dialogue — a networked multi-peer inter-agent dialogue channel over MCP.
//!
//! Revived from `c137-dialogue-mcp` (bead at-revive-dialogue-mcp-8rk): the
//! "email / inbox over MCP" model — the only inter-agent *messaging* the stack
//! has (c137-blink is telemetry). Three tools over a `dialogue` table in
//! agent.sqlite:
//!
//!   - dialogue_say(from, to, content, session_thread?)  → {id, ts}
//!   - dialogue_poll(to, since?, after_seq?, timeout_ms?, limit?) → {messages: [...]}  (notify long-poll; rowid `seq` keyset cursor)
//!   - dialogue_history(session_thread, limit?)           → {messages: [...]}
//!
//! Transport: **pure rmcp** — `StreamableHttpService` over HTTP at `/mcp`
//! (matching agent-mcp / beadsd), with a stdio fallback for local spawn. The
//! original hand-rolled JSON-RPC framing and the pi-facing `/api/dialogue/*`
//! HTTP surface are gone (pi is retired; all peers speak MCP).
//!
//! Peers: cc, mu, warden subagents, orchestrators. Prime use case is cc↔mu.
//!
//! Config (env / CLI, mirroring the agent-mcp service tier — no hardcoded
//! endpoints):
//!   --listen <host:port> | LISTEN | MU_DIALOGUE_ADDR   → HTTP bind (else stdio)
//!   --allow-host <h> (repeatable) | MU_DIALOGUE_ALLOWED_HOSTS (comma-sep)
//!   DATABASE_PATH                                       → sqlite path

// rmcp's ServerHandler trait returns `impl Future + Send + '_` in several
// methods, so these can't become plain `async fn` without fighting the SDK
// shape (same suppression as mu-coding's serve/mcp.rs).
#![allow(clippy::manual_async_fn)]

use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rmcp::model::*;
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map as JsonMap, Value};
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;
use tracing::{info, warn};
use ulid::Ulid;

mod presence;

const SERVER_NAME: &str = "mu-dialogue";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_POLL_TIMEOUT_MS: u64 = 30_000;
/// Peer registrations whose `last_seen` is older than this are stale and get
/// pruned (startup + a periodic sweep). Presence here is activity-derived
/// compatibility behavior — the lease-backed etcd registry from
/// `specs/plans/mu-dialogue-push-mailbox-v1.md` §1 is the real fix; until it
/// lands, a TTL keeps dead session ids from accumulating forever. Override
/// with `--peer-ttl-ms` / `MU_DIALOGUE_PEER_TTL_MS`.
const DEFAULT_PEER_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// How often the background sweep prunes stale peers.
const PRUNE_INTERVAL_SECS: u64 = 3600;
/// Default recency window for `dialogue_broadcast` recipients: the PA system
/// addresses peers present "in the building", not every id ever seen.
const DEFAULT_BROADCAST_WINDOW_MS: i64 = DEFAULT_PEER_TTL_MS;
/// Cap on how long a single `notified()` wait blocks before re-checking the
/// store. The wake is notify-driven (not busy-wait); this only bounds the
/// worst-case latency to observe a message inserted by a *different* process
/// (cross-process writers don't fire this process's in-memory `Notify`).
const POLL_RECHECK_INTERVAL_MS: u64 = 1_000;
/// mu-rkhj: server-initiated MCP notification method carrying one inbound
/// dialogue message to a subscribed daemon. Event-driven receive — the
/// wire counterpart of the daemon's `AgentInput::DialogueMessage` seam.
const DIALOGUE_PUSH_METHOD: &str = "notifications/dialogue.message";

// ───────────────────────────── Storage ──────────────────────────────────────

#[derive(Clone)]
struct Store {
    db: Arc<Mutex<Connection>>,
    notify: Arc<Notify>,
    /// Stale-peer cutoff: registrations idle longer than this are pruned
    /// (startup, the periodic sweep, and dialogue_prune's default).
    peer_ttl_ms: i64,
    /// Optional etcd-lease presence (config-gated; see src/presence.rs).
    /// None = the section is absent/disabled and no network is ever touched.
    presence: Option<Presence>,
    /// mu-rkhj: live push subscriptions — daemon_id → that daemon's MCP
    /// connection peer (registered via `dialogue_subscribe`). A message to
    /// `mu:<daemon>:<session>` is PUSHED over the subscription and marked
    /// delivered; without one it stays undelivered in the store and
    /// replays when the daemon subscribes. Send failure drops the entry
    /// (dead connection) — the daemon re-subscribes on reconnect.
    subs: Arc<Mutex<HashMap<String, Peer<RoleServer>>>>,
}

#[derive(Clone)]
struct Presence {
    cfg: presence::PresenceConfig,
    http: reqwest::Client,
}

impl Store {
    /// Lease-live peers from etcd, or None when presence is disabled OR etcd
    /// is unreachable (fail-open: a presence outage degrades to
    /// activity-derived behavior, it never blocks messaging).
    async fn lease_live(&self) -> Option<Vec<presence::LeasePeer>> {
        let p = self.presence.as_ref()?;
        match presence::lease_peers(&p.http, &p.cfg).await {
            Ok(peers) => Some(peers),
            Err(e) => {
                warn!("etcd presence unavailable, falling back to activity-derived: {e:#}");
                None
            }
        }
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS dialogue (
            id              TEXT PRIMARY KEY,
            from_peer       TEXT NOT NULL,
            to_peer         TEXT NOT NULL,
            session_thread  TEXT,
            content         TEXT NOT NULL,
            ts              INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_dialogue_to_ts
            ON dialogue(to_peer, ts);
        CREATE INDEX IF NOT EXISTS idx_dialogue_thread_ts
            ON dialogue(session_thread, ts);

        -- Presence registry: one row per distinct peer id ever seen on the
        -- channel. There is no explicit register step — a peer is recorded the
        -- first time it sends (dialogue_say.from) or polls (dialogue_poll.to),
        -- and last_seen advances on every subsequent say/poll. `role` is the
        -- prefix before the first ':' (e.g. "mu" from "mu:<daemon>:<session>",
        -- "cc" from "cc:<uuid>") so dialogue_peers can filter by kind. This is
        -- activity-derived presence: a peer that has never spoken or polled is
        -- not listed (see dialogue_peers).
        CREATE TABLE IF NOT EXISTS peers (
            peer_id     TEXT PRIMARY KEY,
            role        TEXT NOT NULL,
            first_seen  INTEGER NOT NULL,
            last_seen   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_peers_role_seen
            ON peers(role, last_seen);

        -- Team membership: a peer registers interest in a named group mailbox
        -- (the multicast model from the push-mailbox spec's roadmap). A
        -- multicast to <team> fans out one durable mailbox row per current
        -- member, so delivery rides the existing per-peer poll path unchanged.
        CREATE TABLE IF NOT EXISTS team_members (
            team       TEXT NOT NULL,
            peer_id    TEXT NOT NULL,
            joined_at  INTEGER NOT NULL,
            PRIMARY KEY (team, peer_id)
        );
        CREATE INDEX IF NOT EXISTS idx_team_members_peer
            ON team_members(peer_id);
        "#,
    )?;
    // mu-rkhj: delivered_at marks a row as PUSHED to a live daemon
    // subscription (NULL = not yet pushed; poll-consumed rows are never
    // marked — delivery there is the poller's cursor). Additive column on
    // an existing table, so it can't ride CREATE TABLE IF NOT EXISTS.
    let has_delivered = conn
        .prepare("SELECT 1 FROM pragma_table_info('dialogue') WHERE name = 'delivered_at'")?
        .exists([])?;
    if !has_delivered {
        conn.execute("ALTER TABLE dialogue ADD COLUMN delivered_at INTEGER", [])?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Serialize, Clone)]
struct DialogueRow {
    /// Opaque monotonic cursor token = the row's insertion order (`rowid`).
    /// Clients pass the last delivered `seq` back as `after_seq` to page
    /// forward exactly. Unlike `ts` (millisecond-coarse) or `id` (a ULID whose
    /// within-millisecond order is RANDOM), `seq` strictly increases with
    /// insertion, so a keyset on it never skips or repeats a concurrently
    /// inserted same-millisecond message.
    seq: i64,
    id: String,
    from: String,
    to: String,
    session_thread: Option<String>,
    content: String,
    ts: i64,
}

impl Store {
    async fn say(
        &self,
        from: &str,
        to: &str,
        content: &str,
        session_thread: Option<&str>,
    ) -> Result<(String, i64)> {
        let id = Ulid::new().to_string();
        let ts = now_ms();
        // First message in a thread mints the thread id = its own message id.
        let thread = session_thread
            .map(String::from)
            .unwrap_or_else(|| id.clone());
        {
            let conn = self.db.lock().await;
            conn.execute(
                "INSERT INTO dialogue (id, from_peer, to_peer, session_thread, content, ts)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![id, from, to, thread, content, ts],
            )?;
        }
        // Wake any in-process long-pollers; each re-checks its own filter.
        self.notify.notify_waiters();
        // mu-rkhj: event-driven delivery — push to the target's daemon if
        // it holds a live subscription; otherwise the row waits in the
        // store and replays on subscribe.
        self.push_message(&id, from, to, &thread, content, ts).await;
        Ok((id, ts))
    }

    /// Fetch messages for `to` after the cursor, oldest-first by insertion.
    ///
    /// Two cursor modes:
    /// - `after_seq = None` (a poll's first fetch): forward-only by timestamp,
    ///   `ts > since_ms` — establishes the starting point without replaying
    ///   anything older than the poller.
    /// - `after_seq = Some(seq)` (every fetch after the first message): a keyset
    ///   on `rowid`, `rowid > seq`. `rowid` is the only strictly
    ///   insertion-monotonic key the table has — `ts` is millisecond-coarse and
    ///   a ULID `id`'s within-millisecond order is RANDOM, so neither can page a
    ///   same-millisecond burst without either starving the tail (timestamp) or
    ///   skipping a concurrent insert (random id). `rowid` does both correctly,
    ///   and once anchored makes `ts` irrelevant: a later insert always has a
    ///   higher rowid, so it is always delivered, exactly once.
    ///
    /// Correctness relies on `dialogue` being append-only (no row is ever
    /// deleted, so a rowid is never reused) — which it is.
    async fn fetch_for(
        &self,
        to: &str,
        since_ms: i64,
        after_seq: Option<i64>,
        limit: i64,
    ) -> Result<Vec<DialogueRow>> {
        let conn = self.db.lock().await;
        match after_seq {
            Some(after_seq) => {
                let mut stmt = conn.prepare(
                    "SELECT rowid AS seq, id, from_peer, to_peer, session_thread, content, ts
                       FROM dialogue
                      WHERE to_peer = ?1 AND rowid > ?2
                      ORDER BY rowid ASC
                      LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(params![to, after_seq, limit], dialogue_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT rowid AS seq, id, from_peer, to_peer, session_thread, content, ts
                       FROM dialogue
                      WHERE to_peer = ?1 AND ts > ?2
                      ORDER BY rowid ASC
                      LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(params![to, since_ms, limit], dialogue_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            }
        }
    }

    async fn history(&self, session_thread: &str, limit: i64) -> Result<Vec<DialogueRow>> {
        let conn = self.db.lock().await;
        let mut stmt = conn.prepare(
            "SELECT rowid AS seq, id, from_peer, to_peer, session_thread, content, ts
               FROM dialogue
              WHERE session_thread = ?
              ORDER BY rowid ASC
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![session_thread, limit], dialogue_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record (or refresh) a peer's presence. Called with the `from` of a say
    /// and the `to` of a poll — the two acts that prove a peer is live. Upsert:
    /// first_seen is set once, last_seen advances every time. `role` is the
    /// prefix before the first ':' (the whole id if there is none). A blank id
    /// is ignored rather than recorded as a ghost peer.
    async fn touch_peer(&self, peer_id: &str, ts: i64) -> Result<()> {
        if peer_id.is_empty() {
            return Ok(());
        }
        let role = peer_id.split(':').next().unwrap_or(peer_id);
        let conn = self.db.lock().await;
        conn.execute(
            "INSERT INTO peers (peer_id, role, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(peer_id) DO UPDATE SET
                 last_seen = excluded.last_seen,
                 role      = excluded.role",
            params![peer_id, role, ts],
        )?;
        Ok(())
    }

    /// Deliver one message to many mailboxes: one durable `dialogue` row per
    /// recipient (a group mailbox is an expansion list), all sharing one
    /// thread, inserted in a single transaction, with a single wake. Delivery
    /// then rides the existing per-peer poll path unchanged — every current
    /// client (cc Stop-hook listener, mu daemon) receives group messages with
    /// zero client changes. Returns the thread id and timestamp.
    async fn fan_out(
        &self,
        from: &str,
        recipients: &[String],
        content: &str,
        session_thread: Option<&str>,
    ) -> Result<(String, i64)> {
        let ts = now_ms();
        // The fan-out mints ONE id up front: it is the thread every recipient's
        // copy carries, so dialogue_history replays the announcement and every
        // reply to it as one conversation.
        let thread = session_thread
            .map(String::from)
            .unwrap_or_else(|| Ulid::new().to_string());
        if !recipients.is_empty() {
            let mut inserted: Vec<(String, String)> = Vec::with_capacity(recipients.len());
            {
                let mut conn = self.db.lock().await;
                let tx = conn.transaction()?;
                for to in recipients {
                    let id = Ulid::new().to_string();
                    tx.execute(
                        "INSERT INTO dialogue (id, from_peer, to_peer, session_thread, content, ts)
                         VALUES (?, ?, ?, ?, ?, ?)",
                        params![id, from, to, thread, content, ts],
                    )?;
                    inserted.push((id, to.clone()));
                }
                tx.commit()?;
            }
            self.notify.notify_waiters();
            // mu-rkhj: the broadcast/multicast itself is fire-and-return —
            // the caller gets the thread id as its correlation handle and
            // never waits. Each recipient copy is pushed to its daemon's
            // subscription when one is live.
            for (id, to) in &inserted {
                self.push_message(id, from, to, &thread, content, ts).await;
            }
        }
        Ok((thread, ts))
    }

    /// Peer ids active since the cutoff, optionally filtered by role,
    /// most-recently-active first. The broadcast recipient set.
    async fn active_peer_ids(
        &self,
        role: Option<&str>,
        active_since_ms: i64,
    ) -> Result<Vec<String>> {
        Ok(self
            .list_peers(role, active_since_ms)
            .await?
            .into_iter()
            .map(|p| p.peer_id)
            .collect())
    }

    /// Register a peer's interest in a team (group mailbox). Idempotent.
    /// Returns the team's member count after the join.
    async fn team_join(&self, team: &str, peer_id: &str, ts: i64) -> Result<i64> {
        let conn = self.db.lock().await;
        conn.execute(
            "INSERT INTO team_members (team, peer_id, joined_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(team, peer_id) DO NOTHING",
            params![team, peer_id, ts],
        )?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM team_members WHERE team = ?1",
            params![team],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    /// Withdraw a peer's interest in a team. Returns whether a row was removed.
    async fn team_leave(&self, team: &str, peer_id: &str) -> Result<bool> {
        let conn = self.db.lock().await;
        let n = conn.execute(
            "DELETE FROM team_members WHERE team = ?1 AND peer_id = ?2",
            params![team, peer_id],
        )?;
        Ok(n > 0)
    }

    /// Members of one team, oldest joiner first.
    async fn team_members_of(&self, team: &str) -> Result<Vec<(String, i64)>> {
        let conn = self.db.lock().await;
        let mut stmt = conn.prepare(
            "SELECT peer_id, joined_at FROM team_members WHERE team = ?1 ORDER BY joined_at ASC",
        )?;
        let rows = stmt
            .query_map(params![team], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every team and its member count; `peer_id` narrows to the teams that
    /// peer belongs to.
    async fn teams_overview(&self, peer_id: Option<&str>) -> Result<Vec<(String, i64)>> {
        let conn = self.db.lock().await;
        let rows = match peer_id {
            Some(p) => {
                let mut stmt = conn.prepare(
                    "SELECT t.team, COUNT(*) FROM team_members t
                      WHERE t.team IN (SELECT team FROM team_members WHERE peer_id = ?1)
                      GROUP BY t.team ORDER BY t.team",
                )?;
                let rows = stmt
                    .query_map(params![p], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT team, COUNT(*) FROM team_members GROUP BY team ORDER BY team",
                )?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            }
        };
        Ok(rows)
    }

    /// Remove peer registrations not seen since `cutoff_ms`, and those peers'
    /// team memberships with them. Returns (peers_removed, memberships_removed).
    /// Pruning is cosmetic, not authoritative: a pruned-but-alive peer
    /// re-registers on its next say/poll (touch_peer).
    async fn prune_peers(&self, cutoff_ms: i64) -> Result<(usize, usize)> {
        let conn = self.db.lock().await;
        let memberships = conn.execute(
            "DELETE FROM team_members WHERE peer_id IN
                 (SELECT peer_id FROM peers WHERE last_seen < ?1)",
            params![cutoff_ms],
        )?;
        let peers = conn.execute("DELETE FROM peers WHERE last_seen < ?1", params![cutoff_ms])?;
        Ok((peers, memberships))
    }

    /// List known peers, most-recently-active first. `role` filters by kind
    /// (e.g. "cc", "mu"); `active_since_ms` drops anyone whose last_seen is
    /// older than the cutoff (0 = no recency filter).
    async fn list_peers(&self, role: Option<&str>, active_since_ms: i64) -> Result<Vec<PeerRow>> {
        let conn = self.db.lock().await;
        let mut sql = String::from(
            "SELECT peer_id, role, first_seen, last_seen FROM peers WHERE last_seen >= ?1",
        );
        if role.is_some() {
            sql.push_str(" AND role = ?2");
        }
        sql.push_str(" ORDER BY last_seen DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = match role {
            Some(r) => stmt
                .query_map(params![active_since_ms, r], peer_row)?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt
                .query_map(params![active_since_ms], peer_row)?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }
    /// mu-rkhj: parse the daemon segment of a mu peer id
    /// (`mu:<daemon>:<session>`). None for non-mu peers (cc etc. receive by
    /// poll, never by push).
    fn daemon_of(peer_id: &str) -> Option<&str> {
        let rest = peer_id.strip_prefix("mu:")?;
        let daemon = rest.split(':').next()?;
        if daemon.is_empty() {
            None
        } else {
            Some(daemon)
        }
    }

    /// mu-rkhj: push one message to its target daemon's live subscription,
    /// if any. Fire-and-forget from the sender's perspective: success marks
    /// the row delivered; a send failure drops the dead subscription and
    /// leaves the row undelivered for replay on the daemon's re-subscribe.
    async fn push_message(
        &self,
        id: &str,
        from: &str,
        to: &str,
        thread: &str,
        content: &str,
        ts: i64,
    ) {
        let Some(daemon) = Self::daemon_of(to) else {
            return;
        };
        let peer = { self.subs.lock().await.get(daemon).cloned() };
        let Some(peer) = peer else { return };
        let note = CustomNotification::new(
            DIALOGUE_PUSH_METHOD,
            Some(json!({
                "id": id,
                "from": from,
                "to": to,
                "session_thread": thread,
                "content": content,
                "ts": ts,
            })),
        );
        match peer
            .send_notification(ServerNotification::CustomNotification(note))
            .await
        {
            Ok(()) => {
                // Deliberately NOT marked delivered here (panel finding): a
                // notification send is unacknowledged fire-and-forget — Ok
                // proves the write left, not that the daemon routed it into
                // the session. delivered_at is set only by the daemon's
                // dialogue_ack, so an unrouted message replays on the next
                // subscribe. The daemon dedups the at-least-once window by
                // message id.
            }
            Err(e) => {
                warn!("push to daemon {daemon} failed ({e:#}); dropping its subscription");
                self.subs.lock().await.remove(daemon);
            }
        }
    }

    /// mu-rkhj: acknowledged delivery — the daemon confirms a pushed
    /// message reached its session's input channel; only then is the row
    /// non-replayable. Returns whether a row was newly marked.
    async fn ack_message(&self, id: &str) -> Result<bool> {
        let conn = self.db.lock().await;
        let n = conn.execute(
            "UPDATE dialogue SET delivered_at = ? WHERE id = ? AND delivered_at IS NULL",
            params![now_ms(), id],
        )?;
        Ok(n > 0)
    }

    /// mu-rkhj: register a daemon's connection for push delivery, then
    /// replay every message addressed to its sessions that was never
    /// pushed. Returns the number of replayed messages. Store order =
    /// arrival order (append-only rowid), no cursor.
    async fn subscribe_daemon(&self, daemon_id: &str, peer: Peer<RoleServer>) -> Result<usize> {
        // Open-trust posture, stated loudly (panel finding): the channel
        // has no caller authentication anywhere (dialogue_say takes any
        // `from`; dialogue_poll reads any inbox), and subscribe is the
        // same trust level. A replacement is legitimate (daemon restart /
        // reconnect) but is ALWAYS logged so a diverted subscription is
        // visible in the server log, never silent. Real authn (peer
        // handles / capability tokens, mu-037 lineage) is follow-up work
        // recorded on the bead.
        if self
            .subs
            .lock()
            .await
            .insert(daemon_id.to_string(), peer)
            .is_some()
        {
            warn!(
                "dialogue_subscribe: REPLACED existing subscription for daemon {daemon_id} \
                 (legitimate on daemon restart; investigate if unexpected)"
            );
        }
        let rows: Vec<DialogueRow> = {
            let conn = self.db.lock().await;
            let mut stmt = conn.prepare(
                "SELECT rowid AS seq, id, from_peer, to_peer, session_thread, content, ts
                 FROM dialogue
                 WHERE to_peer LIKE ?1 AND delivered_at IS NULL
                 ORDER BY rowid ASC",
            )?;
            let it = stmt.query_map(params![format!("mu:{daemon_id}:%")], dialogue_row)?;
            it.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let n = rows.len();
        for r in rows {
            self.push_message(
                &r.id,
                &r.from,
                &r.to,
                r.session_thread.as_deref().unwrap_or(&r.id),
                &r.content,
                r.ts,
            )
            .await;
        }
        Ok(n)
    }

    /// mu-rkhj: does this (mu) peer's daemon hold a live subscription?
    /// Subscribed daemons receive by PUSH, so their polls drain instead of
    /// waiting (operator standard: no model-visible blocking receive).
    async fn daemon_subscribed(&self, peer_id: &str) -> bool {
        match Self::daemon_of(peer_id) {
            Some(d) => self.subs.lock().await.contains_key(d),
            None => false,
        }
    }
}

fn dialogue_row(row: &rusqlite::Row) -> rusqlite::Result<DialogueRow> {
    Ok(DialogueRow {
        seq: row.get(0)?,
        id: row.get(1)?,
        from: row.get(2)?,
        to: row.get(3)?,
        session_thread: row.get(4)?,
        content: row.get(5)?,
        ts: row.get(6)?,
    })
}

#[derive(Debug, Serialize, Clone)]
struct PeerRow {
    peer_id: String,
    role: String,
    first_seen: i64,
    last_seen: i64,
}

fn peer_row(row: &rusqlite::Row) -> rusqlite::Result<PeerRow> {
    Ok(PeerRow {
        peer_id: row.get(0)?,
        role: row.get(1)?,
        first_seen: row.get(2)?,
        last_seen: row.get(3)?,
    })
}

// ─────────────────────────── Tool arguments ─────────────────────────────────

#[derive(Deserialize)]
struct SayArgs {
    from: String,
    to: String,
    content: String,
    session_thread: Option<String>,
}

#[derive(Deserialize)]
struct PollArgs {
    to: String,
    #[serde(default)]
    since: i64,
    /// Keyset cursor: with this set, only messages whose `seq` (insertion
    /// order) is strictly greater are returned, in `seq` order — paging through
    /// a same-millisecond burst of any size with no skips or repeats. Omit (or
    /// null) on the first poll, then pass the last delivered message's `seq`.
    #[serde(default)]
    after_seq: Option<i64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct HistoryArgs {
    session_thread: String,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct PeersArgs {
    /// Filter to one kind of peer ("cc", "mu", …). None = all kinds.
    #[serde(default)]
    role: Option<String>,
    /// Only return peers whose last_seen is within this many ms of now.
    /// None or 0 = no recency filter (every peer ever seen).
    #[serde(default)]
    active_within_ms: Option<i64>,
}

#[derive(Deserialize)]
struct BroadcastArgs {
    from: String,
    content: String,
    /// Narrow the announcement to one kind of peer ("cc", "mu", …).
    #[serde(default)]
    role: Option<String>,
    /// Recency window for recipients (ms). Peers whose last_seen is older are
    /// not addressed. Default DEFAULT_BROADCAST_WINDOW_MS.
    #[serde(default)]
    active_within_ms: Option<i64>,
    #[serde(default)]
    session_thread: Option<String>,
}

#[derive(Deserialize)]
struct MulticastArgs {
    from: String,
    team: String,
    content: String,
    #[serde(default)]
    session_thread: Option<String>,
}

#[derive(Deserialize)]
struct TeamJoinArgs {
    team: String,
    peer_id: String,
}

#[derive(Deserialize)]
struct TeamLeaveArgs {
    team: String,
    peer_id: String,
}

#[derive(Deserialize)]
struct TeamsArgs {
    /// List the members of this team. Omit for the all-teams overview.
    #[serde(default)]
    team: Option<String>,
    /// Overview mode: narrow to the teams this peer belongs to.
    #[serde(default)]
    peer_id: Option<String>,
}

#[derive(Deserialize)]
struct SubscribeArgs {
    /// The subscribing daemon's id — the `<daemon>` segment of its
    /// sessions' peer ids (`mu:<daemon>:<session>`).
    daemon_id: String,
}

#[derive(Deserialize)]
struct PruneArgs {
    /// Remove peers whose last_seen is older than this many ms. Omit for the
    /// server's configured peer TTL.
    #[serde(default)]
    max_age_ms: Option<i64>,
}

async fn handle_say(store: &Store, args: SayArgs) -> Result<Value> {
    let (id, ts) = store
        .say(
            &args.from,
            &args.to,
            &args.content,
            args.session_thread.as_deref(),
        )
        .await?;
    // Sending proves the sender is live — register/refresh its presence.
    store.touch_peer(&args.from, ts).await?;
    Ok(json!({ "id": id, "ts": ts }))
}

async fn handle_poll(store: &Store, args: PollArgs) -> Result<Value> {
    let limit = args.limit.unwrap_or(25).clamp(1, 200);
    let mut timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_POLL_TIMEOUT_MS);
    // mu-rkhj: a daemon with a live push subscription receives by PUSH —
    // waiting here is meaningless (anything new arrives as a notification),
    // so its poll degenerates to an immediate drain. Unsubscribed peers
    // (cc's out-of-band watcher) keep the long-poll unchanged.
    if store.daemon_subscribed(&args.to).await {
        timeout_ms = 0;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    // Polling its own inbox proves the poller (`to`) is live.
    store.touch_peer(&args.to, now_ms()).await?;

    loop {
        let rows = store
            .fetch_for(&args.to, args.since, args.after_seq, limit)
            .await?;
        if !rows.is_empty() {
            return Ok(json!({ "messages": rows }));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(json!({ "messages": [] }));
        }
        // Wake on a notify or the re-check cap, whichever comes first.
        let _ = timeout(
            remaining.min(Duration::from_millis(POLL_RECHECK_INTERVAL_MS)),
            store.notify.notified(),
        )
        .await;
    }
}

async fn handle_history(store: &Store, args: HistoryArgs) -> Result<Value> {
    let limit = args.limit.unwrap_or(50).clamp(1, 1000);
    let rows = store.history(&args.session_thread, limit).await?;
    Ok(json!({ "messages": rows }))
}

async fn handle_peers(store: &Store, args: PeersArgs) -> Result<Value> {
    let now = now_ms();
    // active_within_ms → an absolute last_seen cutoff; 0/None means no filter.
    let active_since = args
        .active_within_ms
        .filter(|w| *w > 0)
        .map(|w| now - w)
        .unwrap_or(0);
    let peers = store.list_peers(args.role.as_deref(), active_since).await?;
    let lease = store.lease_live().await;
    let Some(lease) = lease else {
        // Presence disabled (or etcd down, fail-open): the original shape.
        return Ok(json!({ "peers": peers, "now": now }));
    };

    // Merge: lease-live peers are authoritative (live RIGHT NOW by lease
    // expiry, so they bypass the recency filter); activity-derived rows are
    // compatibility presence, each marked so callers can tell them apart.
    let lease_ids: std::collections::HashSet<&str> =
        lease.iter().map(|p| p.peer_id.as_str()).collect();
    let mut out: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in &peers {
        let src = if lease_ids.contains(p.peer_id.as_str()) {
            "lease"
        } else {
            "activity"
        };
        seen.insert(p.peer_id.clone());
        out.push(json!({
            "peer_id": p.peer_id, "role": p.role,
            "first_seen": p.first_seen, "last_seen": p.last_seen,
            "presence": src,
        }));
    }
    for lp in &lease {
        if seen.contains(&lp.peer_id) {
            continue;
        }
        if let Some(role) = &args.role {
            if role != &lp.role {
                continue;
            }
        }
        // Never said/polled (pure lease registration): live now, no activity
        // timestamps to report beyond its registration time.
        out.push(json!({
            "peer_id": lp.peer_id, "role": lp.role,
            "first_seen": lp.registered_at, "last_seen": Value::Null,
            "presence": "lease",
        }));
    }
    Ok(json!({ "peers": out, "now": now, "presence_backend": "etcd" }))
}

/// PA system: deliver one announcement to every peer active within the window
/// (optionally one role), excluding the sender. The recipient set is fixed at
/// send time — a peer that appears later does not receive it, exactly like a
/// PA address reaches whoever is in the building.
async fn handle_broadcast(store: &Store, args: BroadcastArgs) -> Result<Value> {
    if args.from.is_empty() {
        anyhow::bail!("broadcast requires a non-empty 'from'");
    }
    let now = now_ms();
    let window = args
        .active_within_ms
        .filter(|w| *w > 0)
        .unwrap_or(DEFAULT_BROADCAST_WINDOW_MS);
    let mut recipients: Vec<String> = store
        .active_peer_ids(args.role.as_deref(), now - window)
        .await?
        .into_iter()
        .filter(|p| p != &args.from)
        .collect();
    // Lease-live peers are in the building by definition — address them even
    // if they've never said/polled (pure lease registration).
    if let Some(lease) = store.lease_live().await {
        let have: std::collections::HashSet<String> = recipients.iter().cloned().collect();
        for lp in lease {
            let role_ok = args.role.as_deref().is_none_or(|r| r == lp.role);
            if role_ok && lp.peer_id != args.from && !have.contains(&lp.peer_id) {
                recipients.push(lp.peer_id);
            }
        }
    }
    let (thread, ts) = store
        .fan_out(
            &args.from,
            &recipients,
            &args.content,
            args.session_thread.as_deref(),
        )
        .await?;
    // Announcing proves the announcer is live.
    store.touch_peer(&args.from, ts).await?;
    Ok(json!({
        "id": thread, "ts": ts,
        "count": recipients.len(), "recipients": recipients,
    }))
}

/// Team multicast: deliver to the current members of a group mailbox
/// (registered interest via dialogue_team_join), excluding the sender.
/// Members are addressed regardless of activity — the mailbox is durable, an
/// idle member reads it on its next poll.
async fn handle_multicast(store: &Store, args: MulticastArgs) -> Result<Value> {
    if args.from.is_empty() {
        anyhow::bail!("multicast requires a non-empty 'from'");
    }
    if args.team.is_empty() {
        anyhow::bail!("multicast requires a non-empty 'team'");
    }
    let recipients: Vec<String> = store
        .team_members_of(&args.team)
        .await?
        .into_iter()
        .map(|(peer, _joined)| peer)
        .filter(|p| p != &args.from)
        .collect();
    let (thread, ts) = store
        .fan_out(
            &args.from,
            &recipients,
            &args.content,
            args.session_thread.as_deref(),
        )
        .await?;
    store.touch_peer(&args.from, ts).await?;
    Ok(json!({
        "id": thread, "ts": ts, "team": args.team,
        "count": recipients.len(), "recipients": recipients,
    }))
}

async fn handle_team_join(store: &Store, args: TeamJoinArgs) -> Result<Value> {
    if args.team.is_empty() || args.peer_id.is_empty() {
        anyhow::bail!("team_join requires non-empty 'team' and 'peer_id'");
    }
    let ts = now_ms();
    let members = store.team_join(&args.team, &args.peer_id, ts).await?;
    // Joining proves the joiner is live.
    store.touch_peer(&args.peer_id, ts).await?;
    Ok(json!({ "team": args.team, "peer_id": args.peer_id, "members": members }))
}

async fn handle_team_leave(store: &Store, args: TeamLeaveArgs) -> Result<Value> {
    let removed = store.team_leave(&args.team, &args.peer_id).await?;
    Ok(json!({ "team": args.team, "peer_id": args.peer_id, "removed": removed }))
}

async fn handle_teams(store: &Store, args: TeamsArgs) -> Result<Value> {
    match args.team {
        Some(team) => {
            let members: Vec<Value> = store
                .team_members_of(&team)
                .await?
                .into_iter()
                .map(|(peer_id, joined_at)| json!({ "peer_id": peer_id, "joined_at": joined_at }))
                .collect();
            Ok(json!({ "team": team, "members": members }))
        }
        None => {
            let teams: Vec<Value> = store
                .teams_overview(args.peer_id.as_deref())
                .await?
                .into_iter()
                .map(|(team, members)| json!({ "team": team, "members": members }))
                .collect();
            Ok(json!({ "teams": teams }))
        }
    }
}

async fn handle_prune(store: &Store, args: PruneArgs) -> Result<Value> {
    let max_age = args
        .max_age_ms
        .filter(|a| *a > 0)
        .unwrap_or(store.peer_ttl_ms);
    let cutoff = now_ms() - max_age;
    let (peers, memberships) = store.prune_peers(cutoff).await?;
    Ok(json!({
        "removed_peers": peers,
        "removed_memberships": memberships,
        "cutoff": cutoff,
    }))
}

// ─────────────────────────── rmcp ServerHandler ─────────────────────────────

#[derive(Clone)]
struct DialogueHandler {
    store: Store,
}

fn schema(v: Value) -> Arc<JsonMap<String, Value>> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => Arc::new(JsonMap::new()),
    }
}

fn tools_list() -> Vec<Tool> {
    vec![
        // mu-rkhj: dialogue_subscribe and dialogue_ack are deliberately NOT
        // listed — they are daemon plumbing dispatched by name (the
        // handshake's experimental mu.dialoguePush capability is the
        // discovery signal). Keeping them out of tools/list keeps them off
        // every model/tool surface that builds from the listing.
        Tool::new(
            "dialogue_say",
            "Send a message to another peer through the dialogue channel. \
             session_thread groups a multi-turn conversation; omit it for a fresh thread \
             (the returned id becomes the thread id).",
            schema(json!({
                "type": "object",
                "properties": {
                    "from":           {"type": "string", "description": "Sender peer id (e.g. 'cc', 'mu')"},
                    "to":             {"type": "string", "description": "Recipient peer id"},
                    "content":        {"type": "string", "description": "Message body"},
                    "session_thread": {"type": "string", "description": "Optional thread id; minted from the message id if omitted"}
                },
                "required": ["from", "to", "content"]
            })),
        ),
        Tool::new(
            "dialogue_poll",
            "Long-poll for messages addressed to a peer. Returns immediately if any \
             postdate `since`; otherwise blocks up to timeout_ms or until a new message \
             arrives (notify-driven).",
            schema(json!({
                "type": "object",
                "properties": {
                    "to":         {"type": "string", "description": "Peer id to poll for"},
                    "since":      {"type": "number", "description": "epoch_ms cutoff; only messages with ts > since are returned (default 0). Used only on the first poll (when after_seq is absent)."},
                    "after_seq":  {"type": "number", "description": "keyset cursor: returns only messages whose seq (insertion order) is > after_seq, in seq order. Pages a same-millisecond burst of any size with no skips or repeats. Omit on the first poll; then pass the last delivered message's seq."},
                    "timeout_ms": {"type": "number", "description": "Max wait in ms (default 30000)"},
                    "limit":      {"type": "number", "description": "Max messages per response (default 25, max 200)"}
                },
                "required": ["to"]
            })),
        ),
        Tool::new(
            "dialogue_history",
            "Retrieve a thread, oldest-first. Useful for replay or reconstructing context \
             after a restart.",
            schema(json!({
                "type": "object",
                "properties": {
                    "session_thread": {"type": "string", "description": "Thread id (returned by dialogue_say)"},
                    "limit":          {"type": "number", "description": "Max messages (default 50, max 1000)"}
                },
                "required": ["session_thread"]
            })),
        ),
        Tool::new(
            "dialogue_peers",
            "Discover peers on the channel. Presence is activity-derived: a peer \
             is listed once it has sent (dialogue_say) or polled (dialogue_poll), \
             with last_seen advancing on each. Returns {peers:[{peer_id, role, \
             first_seen, last_seen}], now}; compare last_seen to now for staleness.",
            schema(json!({
                "type": "object",
                "properties": {
                    "role":             {"type": "string", "description": "Filter to one kind of peer ('cc', 'mu', …). Omit for all."},
                    "active_within_ms": {"type": "number", "description": "Only peers whose last_seen is within this many ms of now. Omit/0 = no recency filter."}
                }
            })),
        ),
        Tool::new(
            "dialogue_broadcast",
            "PA system: send one announcement to every peer active within the \
             recency window (optionally one role), excluding yourself. Each \
             recipient gets a durable mailbox copy on one shared thread; the \
             recipient set is fixed at send time. Returns {id, ts, count, \
             recipients}.",
            schema(json!({
                "type": "object",
                "properties": {
                    "from":             {"type": "string", "description": "Sender peer id"},
                    "content":          {"type": "string", "description": "Announcement body"},
                    "role":             {"type": "string", "description": "Only address one kind of peer ('cc', 'mu', …). Omit for all."},
                    "active_within_ms": {"type": "number", "description": "Recency window for recipients (ms). Default 24h."},
                    "session_thread":   {"type": "string", "description": "Optional thread id; minted if omitted"}
                },
                "required": ["from", "content"]
            })),
        ),
        Tool::new(
            "dialogue_multicast",
            "Team multicast: send to the current members of a team (group \
             mailbox), excluding yourself. Members register interest with \
             dialogue_team_join; delivery is a durable mailbox copy per member \
             on one shared thread, regardless of member activity. Returns \
             {id, ts, team, count, recipients}.",
            schema(json!({
                "type": "object",
                "properties": {
                    "from":           {"type": "string", "description": "Sender peer id"},
                    "team":           {"type": "string", "description": "Team (group mailbox) name"},
                    "content":        {"type": "string", "description": "Message body"},
                    "session_thread": {"type": "string", "description": "Optional thread id; minted if omitted"}
                },
                "required": ["from", "team", "content"]
            })),
        ),
        Tool::new(
            "dialogue_team_join",
            "Register a peer's interest in a team (group mailbox) so it \
             receives dialogue_multicast messages sent to that team. \
             Idempotent. Returns {team, peer_id, members}.",
            schema(json!({
                "type": "object",
                "properties": {
                    "team":    {"type": "string", "description": "Team name"},
                    "peer_id": {"type": "string", "description": "Peer joining the team"}
                },
                "required": ["team", "peer_id"]
            })),
        ),
        Tool::new(
            "dialogue_team_leave",
            "Withdraw a peer's interest in a team. Returns {team, peer_id, removed}.",
            schema(json!({
                "type": "object",
                "properties": {
                    "team":    {"type": "string", "description": "Team name"},
                    "peer_id": {"type": "string", "description": "Peer leaving the team"}
                },
                "required": ["team", "peer_id"]
            })),
        ),
        Tool::new(
            "dialogue_teams",
            "List teams. With 'team': that team's members. Without: every team \
             and its member count ('peer_id' narrows to teams that peer belongs to).",
            schema(json!({
                "type": "object",
                "properties": {
                    "team":    {"type": "string", "description": "List this team's members"},
                    "peer_id": {"type": "string", "description": "Overview mode: only teams this peer belongs to"}
                }
            })),
        ),
        Tool::new(
            "dialogue_prune",
            "Remove stale peer registrations (and their team memberships): \
             peers whose last_seen is older than max_age_ms (default: the \
             server's peer TTL). Pruning is cosmetic — a live peer re-registers \
             on its next say/poll. Returns {removed_peers, removed_memberships, cutoff}.",
            schema(json!({
                "type": "object",
                "properties": {
                    "max_age_ms": {"type": "number", "description": "Staleness cutoff in ms (default: server peer TTL, 24h unless overridden)"}
                }
            })),
        ),
    ]
}

impl DialogueHandler {
    /// Dispatch one tool call to its handler, returning the JSON payload or a
    /// human-readable error string (surfaced as an MCP tool error).
    /// mu-rkhj: register a daemon's push subscription. Lives OUTSIDE
    /// [`dispatch`] because it needs the calling connection's peer handle
    /// (`RequestContext::peer`), which only `call_tool` has. Daemon
    /// plumbing, not a model surface: a mu daemon calls it once after the
    /// MCP handshake; the server then PUSHES every message addressed to
    /// `mu:<daemon_id>:*` and replays what accumulated while it was away.
    async fn subscribe(
        &self,
        arguments: Value,
        peer: &Peer<RoleServer>,
    ) -> std::result::Result<Value, String> {
        let args: SubscribeArgs = serde_json::from_value(arguments)
            .map_err(|e| format!("dialogue_subscribe bad args: {e}"))?;
        let replayed = self
            .store
            .subscribe_daemon(&args.daemon_id, peer.clone())
            .await
            .map_err(|e| format!("dialogue_subscribe failed: {e:#}"))?;
        info!(
            daemon_id = %args.daemon_id,
            replayed,
            "dialogue push subscription registered"
        );
        Ok(json!({ "subscribed": args.daemon_id, "replayed": replayed }))
    }

    async fn dispatch(&self, name: &str, arguments: Value) -> std::result::Result<Value, String> {
        match name {
            "dialogue_ack" => {
                // mu-rkhj: daemon plumbing — confirms a PUSHED message
                // reached its session's input channel. Only this marks a
                // row delivered/non-replayable (a notification send is
                // unacknowledged, so the push itself proves nothing).
                let id = arguments
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "dialogue_ack bad args: missing id".to_string())?
                    .to_string();
                let acked = self
                    .store
                    .ack_message(&id)
                    .await
                    .map_err(|e| format!("dialogue_ack failed: {e:#}"))?;
                Ok(json!({ "acked": acked, "id": id }))
            }
            "dialogue_say" => {
                let args: SayArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_say bad args: {e}"))?;
                handle_say(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_say failed: {e:#}"))
            }
            "dialogue_poll" => {
                let args: PollArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_poll bad args: {e}"))?;
                handle_poll(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_poll failed: {e:#}"))
            }
            "dialogue_history" => {
                let args: HistoryArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_history bad args: {e}"))?;
                handle_history(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_history failed: {e:#}"))
            }
            "dialogue_peers" => {
                let args: PeersArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_peers bad args: {e}"))?;
                handle_peers(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_peers failed: {e:#}"))
            }
            "dialogue_broadcast" => {
                let args: BroadcastArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_broadcast bad args: {e}"))?;
                handle_broadcast(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_broadcast failed: {e:#}"))
            }
            "dialogue_multicast" => {
                let args: MulticastArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_multicast bad args: {e}"))?;
                handle_multicast(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_multicast failed: {e:#}"))
            }
            "dialogue_team_join" => {
                let args: TeamJoinArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_team_join bad args: {e}"))?;
                handle_team_join(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_team_join failed: {e:#}"))
            }
            "dialogue_team_leave" => {
                let args: TeamLeaveArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_team_leave bad args: {e}"))?;
                handle_team_leave(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_team_leave failed: {e:#}"))
            }
            "dialogue_teams" => {
                let args: TeamsArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_teams bad args: {e}"))?;
                handle_teams(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_teams failed: {e:#}"))
            }
            "dialogue_prune" => {
                let args: PruneArgs = serde_json::from_value(arguments)
                    .map_err(|e| format!("dialogue_prune bad args: {e}"))?;
                handle_prune(&self.store, args)
                    .await
                    .map_err(|e| format!("dialogue_prune failed: {e:#}"))
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

impl ServerHandler for DialogueHandler {
    fn get_info(&self) -> InitializeResult {
        // mu-rkhj: the push capability is announced in the HANDSHAKE, not
        // in tools/list — dialogue_subscribe/dialogue_ack are daemon
        // plumbing reachable by name only (panel finding: advertising
        // them as ordinary tools invited model/tool-surface callers).
        let mut mu = JsonMap::new();
        mu.insert("dialoguePush".to_string(), Value::Bool(true));
        let mut experimental = ExperimentalCapabilities::new();
        experimental.insert("mu".to_string(), mu);
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_experimental_with(experimental)
                .build(),
        )
        .with_server_info(Implementation::new(SERVER_NAME, VERSION))
        .with_instructions(
            "Multi-peer inter-agent dialogue channel (the email/inbox-over-MCP model). \
                 dialogue_say to send, dialogue_poll to long-poll an inbox, dialogue_history \
                 to replay a thread, dialogue_peers to discover who is on the channel. \
                 dialogue_broadcast is the PA system (announce to every active peer); \
                 dialogue_multicast sends to a team's members (register interest with \
                 dialogue_team_join / dialogue_team_leave, inspect with dialogue_teams). \
                 Peers: cc, mu, warden subagents, orchestrators.\n\
                 \n\
                 Peer ids are 'role:identity'. Identify yourself consistently in the \
                 'from'/'to' fields: a Claude Code peer uses 'cc:' + its \
                 CLAUDE_CODE_SESSION_ID (e.g. 'cc:2257560e-...'); an mu session uses \
                 'mu:<daemon_id>:<session_id>'. The 'role:' prefix is what dialogue_peers \
                 groups on. Presence is activity-derived — you appear to others the first \
                 time you say or poll, not at connect. Stale registrations expire: peers \
                 idle past the server TTL are pruned (dialogue_prune forces a sweep).",
        )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        async move {
            Ok(ListToolsResult {
                tools: tools_list(),
                ..Default::default()
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        async move {
            let arguments = Value::Object(request.arguments.unwrap_or_default());
            let outcome = if request.name.as_ref() == "dialogue_subscribe" {
                self.subscribe(arguments, &context.peer).await
            } else {
                self.dispatch(&request.name, arguments).await
            };
            match outcome {
                Ok(v) => Ok(CallToolResult::success(vec![Content::new(
                    RawContent::text(v.to_string()),
                    None,
                )])),
                Err(msg) => Ok(CallToolResult::error(vec![Content::new(
                    RawContent::text(msg),
                    None,
                )])),
            }
        }
    }
}

// ─────────────────────────────── Config ─────────────────────────────────────

fn default_db_path() -> PathBuf {
    if let Ok(p) = env::var("DATABASE_PATH") {
        return PathBuf::from(p);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/share/agent.sqlite")
}

/// `--listen <addr>` / `--listen=<addr>`, else None (caller falls back to env).
fn parse_listen(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--listen=") {
            return Some(v.to_string());
        }
        if a == "--listen" {
            return it.next().cloned();
        }
    }
    None
}

/// `--allow-host <h>` (repeatable) / `--allow-host=<h>`, falling back to
/// `MU_DIALOGUE_ALLOWED_HOSTS` (comma-separated). Empty = allow any Host (the
/// trusted-network default; rmcp's own default is localhost-only, which 403s
/// remote clients even on a 0.0.0.0 bind). Mirrors agent-mcp.
fn parse_allowed_hosts(args: &[String]) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(h) = a.strip_prefix("--allow-host=") {
            hosts.push(h.to_string());
        } else if a == "--allow-host" {
            if let Some(h) = it.next() {
                hosts.push(h.clone());
            }
        }
    }
    if hosts.is_empty() {
        if let Ok(env) = env::var("MU_DIALOGUE_ALLOWED_HOSTS") {
            hosts.extend(
                env.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
    }
    hosts
}

/// `--peer-ttl-ms <ms>` / `--peer-ttl-ms=<ms>`, falling back to
/// `MU_DIALOGUE_PEER_TTL_MS`, else DEFAULT_PEER_TTL_MS.
fn parse_peer_ttl_ms(args: &[String]) -> i64 {
    let mut it = args.iter();
    let mut from_args = None;
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--peer-ttl-ms=") {
            from_args = Some(v.to_string());
        } else if a == "--peer-ttl-ms" {
            from_args = it.next().cloned();
        }
    }
    from_args
        .or_else(|| env::var("MU_DIALOGUE_PEER_TTL_MS").ok())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|ttl| *ttl > 0)
        .unwrap_or(DEFAULT_PEER_TTL_MS)
}

fn open_store(peer_ttl_ms: i64) -> Result<Store> {
    let db_path = default_db_path();
    info!(version = VERSION, db = %db_path.display(), peer_ttl_ms, "mu-dialogue starting");
    let conn =
        Connection::open(&db_path).with_context(|| format!("open db {}", db_path.display()))?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    migrate(&conn).context("schema migration")?;
    // Optional etcd-lease presence: enabled only by [dialogue.presence]
    // enabled=true in the mu config — a bare install runs without etcd.
    let cfg_path = presence::default_config_path();
    let presence = match presence::load(&cfg_path) {
        Some(cfg) => {
            info!(etcd = ?cfg.etcd, prefix = %cfg.prefix,
                  "etcd-lease presence ENABLED ({})", cfg_path.display());
            Some(Presence {
                cfg,
                http: reqwest::Client::new(),
            })
        }
        None => {
            info!(
                "etcd-lease presence disabled (no [dialogue.presence] enabled=true in {}); \
                 using activity-derived presence + TTL sweep",
                cfg_path.display()
            );
            None
        }
    };
    Ok(Store {
        db: Arc::new(Mutex::new(conn)),
        notify: Arc::new(Notify::new()),
        peer_ttl_ms,
        presence,
        subs: Arc::new(Mutex::new(HashMap::new())),
    })
}

// ─────────────────────────────── Entry ──────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Logs to stderr only — stdout is the JSON-RPC channel in stdio mode.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = env::args().skip(1).collect();
    let listen = parse_listen(&args)
        .or_else(|| env::var("LISTEN").ok())
        .or_else(|| env::var("MU_DIALOGUE_ADDR").ok())
        .filter(|s| !s.is_empty());
    let allowed_hosts = parse_allowed_hosts(&args);
    let store = open_store(parse_peer_ttl_ms(&args))?;

    // Stale-registration hygiene: prune once at startup, then sweep hourly.
    // TTL-based expiry is compatibility behavior until the lease-backed etcd
    // presence registry (push-mailbox spec §1) replaces touch_peer presence.
    match store.prune_peers(now_ms() - store.peer_ttl_ms).await {
        Ok((peers, memberships)) if peers > 0 || memberships > 0 => {
            info!(
                peers,
                memberships, "pruned stale peer registrations at startup"
            );
        }
        Ok(_) => {}
        Err(e) => warn!("startup peer prune failed: {e:#}"),
    }
    {
        let store = store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(PRUNE_INTERVAL_SECS));
            tick.tick().await; // the immediate first tick — startup already pruned
            loop {
                tick.tick().await;
                match store.prune_peers(now_ms() - store.peer_ttl_ms).await {
                    Ok((peers, memberships)) if peers > 0 || memberships > 0 => {
                        info!(peers, memberships, "pruned stale peer registrations");
                    }
                    Ok(_) => {}
                    Err(e) => warn!("periodic peer prune failed: {e:#}"),
                }
            }
        });
    }

    match listen {
        Some(addr) => serve_http(&addr, store, allowed_hosts).await,
        None => {
            info!("mu-dialogue: stdio transport");
            let running = DialogueHandler { store }
                .serve(rmcp::transport::stdio())
                .await?;
            running.waiting().await?;
            Ok(())
        }
    }
}

async fn serve_http(addr: &str, store: Store, allowed_hosts: Vec<String>) -> Result<()> {
    use axum::Router;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, tower::StreamableHttpService,
        StreamableHttpServerConfig,
    };

    // EMPTY allowed_hosts = allow any Host (trusted-network bind, where clients
    // connect by LAN IP/hostname). Lock a public bind down with --allow-host /
    // MU_DIALOGUE_ALLOWED_HOSTS. Mirrors agent-mcp's serve_http.
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts.clone());

    let service: StreamableHttpService<DialogueHandler, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(DialogueHandler {
                    store: store.clone(),
                })
            },
            LocalSessionManager::default().into(),
            config,
        );

    let app = Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    if allowed_hosts.is_empty() {
        warn!("mu-dialogue: allowed-hosts = any (trusted network)");
    }
    info!(addr = %addr, "mu-dialogue: listening on http://{addr}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> Store {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        Store {
            db: Arc::new(Mutex::new(conn)),
            notify: Arc::new(Notify::new()),
            peer_ttl_ms: DEFAULT_PEER_TTL_MS,
            presence: None,
            subs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[tokio::test]
    async fn say_poll_history_roundtrip() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        // say mints a thread = the message id
        let said = h
            .dispatch(
                "dialogue_say",
                json!({"from": "cc", "to": "mu", "content": "ping"}),
            )
            .await
            .unwrap();
        let thread = said["id"].as_str().unwrap().to_string();
        assert!(said["ts"].as_i64().unwrap() > 0);

        // poll mu's inbox (single-shot) returns the message
        let polled = h
            .dispatch(
                "dialogue_poll",
                json!({"to": "mu", "since": 0, "timeout_ms": 0}),
            )
            .await
            .unwrap();
        let msgs = polled["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "ping");
        assert_eq!(msgs[0]["from"], "cc");
        assert_eq!(msgs[0]["session_thread"].as_str().unwrap(), thread);

        // reply on the same thread, then history reconstructs both oldest-first
        h.dispatch(
            "dialogue_say",
            json!({"from": "mu", "to": "cc", "content": "pong", "session_thread": thread}),
        )
        .await
        .unwrap();
        let hist = h
            .dispatch("dialogue_history", json!({"session_thread": thread}))
            .await
            .unwrap();
        let hm = hist["messages"].as_array().unwrap();
        assert_eq!(hm.len(), 2);
        assert_eq!(hm[0]["content"], "ping");
        assert_eq!(hm[1]["content"], "pong");
    }

    #[tokio::test]
    async fn poll_empty_returns_immediately_with_zero_timeout() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        let polled = h
            .dispatch(
                "dialogue_poll",
                json!({"to": "nobody", "since": 0, "timeout_ms": 0}),
            )
            .await
            .unwrap();
        assert_eq!(polled["messages"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn poll_filters_by_recipient_and_since() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        let s1 = h
            .dispatch(
                "dialogue_say",
                json!({"from": "a", "to": "b", "content": "first"}),
            )
            .await
            .unwrap();
        let ts1 = s1["ts"].as_i64().unwrap();
        // a message to a different recipient is not returned
        h.dispatch(
            "dialogue_say",
            json!({"from": "a", "to": "c", "content": "other"}),
        )
        .await
        .unwrap();
        let p = h
            .dispatch(
                "dialogue_poll",
                json!({"to": "b", "since": 0, "timeout_ms": 0}),
            )
            .await
            .unwrap();
        assert_eq!(p["messages"].as_array().unwrap().len(), 1);
        // since = ts1 excludes it (strictly-greater filter)
        let p2 = h
            .dispatch(
                "dialogue_poll",
                json!({"to": "b", "since": ts1, "timeout_ms": 0}),
            )
            .await
            .unwrap();
        assert_eq!(p2["messages"].as_array().unwrap().len(), 0);
    }

    /// A burst larger than the page limit that lands in a single millisecond
    /// must page through completely: the `seq` (rowid) keyset walks every row
    /// exactly once, in insertion order, with no repeats and no starved tail.
    /// The ids are inserted in DESCENDING lexical order, so a cursor that keyed
    /// on the ULID `id` (or any non-insertion order) would mis-page — proving
    /// the cursor follows `seq`, not `id`.
    #[tokio::test]
    async fn poll_keyset_pages_through_same_ms_burst() {
        let store = test_store().await;
        // Five messages to "b", all at the same ts. Insertion order 0..5 but
        // ids descend (id-4, id-3, …), so id order is the REVERSE of insertion.
        {
            let conn = store.db.lock().await;
            for i in 0..5 {
                conn.execute(
                    "INSERT INTO dialogue (id, from_peer, to_peer, session_thread, content, ts)
                     VALUES (?1, 'a', 'b', ?1, ?2, 1000)",
                    rusqlite::params![format!("id-{}", 4 - i), format!("msg-{i}")],
                )
                .unwrap();
            }
        }
        // Page size 2 (< 5): forces the cursor to advance within one ts.
        let since = 0i64;
        let mut after: Option<i64> = None;
        let mut got: Vec<String> = Vec::new();
        loop {
            let rows = store.fetch_for("b", since, after, 2).await.unwrap();
            if rows.is_empty() {
                break;
            }
            for r in &rows {
                got.push(r.content.clone());
            }
            after = Some(rows.last().unwrap().seq);
        }
        assert_eq!(
            got,
            vec!["msg-0", "msg-1", "msg-2", "msg-3", "msg-4"],
            "keyset must page a same-ms burst exactly once, in INSERTION order (by seq, not id)"
        );
    }

    #[tokio::test]
    async fn bad_args_and_unknown_tool_error() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        // missing required `content`
        assert!(h
            .dispatch("dialogue_say", json!({"from": "a", "to": "b"}))
            .await
            .is_err());
        assert!(h.dispatch("nope", json!({})).await.is_err());
    }

    #[test]
    fn advertises_all_tools() {
        let names: Vec<_> = tools_list().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(
            names,
            [
                "dialogue_say",
                "dialogue_poll",
                "dialogue_history",
                "dialogue_peers",
                "dialogue_broadcast",
                "dialogue_multicast",
                "dialogue_team_join",
                "dialogue_team_leave",
                "dialogue_teams",
                "dialogue_prune",
            ]
        );
    }

    /// mu-rkhj: only mu peers have a daemon segment — cc and friends
    /// receive by poll, never push.
    #[test]
    fn daemon_of_parses_mu_peer_ids_only() {
        assert_eq!(Store::daemon_of("mu:abc123:session-1"), Some("abc123"));
        assert_eq!(Store::daemon_of("cc:some-uuid"), None);
        assert_eq!(Store::daemon_of("mu:"), None);
        assert_eq!(Store::daemon_of("warden:x:y"), None);
    }

    /// mu-rkhj: rows start undelivered; the migration adds the column to
    /// pre-existing databases (idempotent).
    #[tokio::test]
    async fn undelivered_rows_await_replay_and_migration_is_idempotent() {
        let store = test_store().await;
        // migrate twice — the guarded ALTER must not error
        {
            let conn = store.db.lock().await;
            migrate(&conn).unwrap();
        }
        store
            .say("cc:peer", "mu:d1:session-1", "hello", None)
            .await
            .unwrap();
        let undelivered: i64 = {
            let conn = store.db.lock().await;
            conn.query_row(
                "SELECT COUNT(*) FROM dialogue WHERE to_peer LIKE 'mu:d1:%' AND delivered_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(undelivered, 1, "no subscription -> stays undelivered");
        // and an unsubscribed daemon's poll still long-polls (flag false)
        assert!(!store.daemon_subscribed("mu:d1:session-1").await);
        assert!(!store.daemon_subscribed("cc:peer").await);
    }

    /// mu-rkhj acked delivery: only dialogue_ack marks a row delivered;
    /// a second ack for the same id is a no-op (dedup on the daemon side).
    #[tokio::test]
    async fn ack_marks_delivered_exactly_once() {
        let store = test_store().await;
        let (id, _ts) = store
            .say("cc:peer", "mu:d1:session-1", "hello", None)
            .await
            .unwrap();
        assert!(store.ack_message(&id).await.unwrap(), "first ack marks");
        assert!(
            !store.ack_message(&id).await.unwrap(),
            "second ack is a no-op"
        );
        let undelivered: i64 = {
            let conn = store.db.lock().await;
            conn.query_row(
                "SELECT COUNT(*) FROM dialogue WHERE delivered_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(undelivered, 0, "acked row is non-replayable");
    }

    /// Broadcast reaches every active peer except the sender; every copy
    /// shares one thread; each recipient's poll delivers exactly its own copy.
    #[tokio::test]
    async fn broadcast_fans_out_to_active_peers_except_sender() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        // Three peers become known through activity.
        for p in ["cc:a", "cc:b", "mu:d1:s1"] {
            h.dispatch(
                "dialogue_poll",
                json!({"to": p, "since": 0, "timeout_ms": 0}),
            )
            .await
            .unwrap();
        }
        let out = h
            .dispatch(
                "dialogue_broadcast",
                json!({"from": "cc:a", "content": "all hands"}),
            )
            .await
            .unwrap();
        assert_eq!(out["count"], 2);
        let recipients: Vec<_> = out["recipients"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(recipients.contains(&"cc:b".to_string()));
        assert!(recipients.contains(&"mu:d1:s1".to_string()));
        assert!(!recipients.contains(&"cc:a".to_string()), "sender excluded");

        // Each recipient's inbox has exactly one copy, on the shared thread.
        let thread = out["id"].as_str().unwrap();
        for p in ["cc:b", "mu:d1:s1"] {
            let polled = h
                .dispatch(
                    "dialogue_poll",
                    json!({"to": p, "since": 0, "timeout_ms": 0}),
                )
                .await
                .unwrap();
            let msgs = polled["messages"].as_array().unwrap();
            assert_eq!(msgs.len(), 1, "{p} gets exactly one copy");
            assert_eq!(msgs[0]["content"], "all hands");
            assert_eq!(msgs[0]["session_thread"].as_str().unwrap(), thread);
        }
        // The sender's inbox got nothing.
        let own = h
            .dispatch(
                "dialogue_poll",
                json!({"to": "cc:a", "since": 0, "timeout_ms": 0}),
            )
            .await
            .unwrap();
        assert_eq!(own["messages"].as_array().unwrap().len(), 0);
        // history replays the announcement (one row per recipient).
        let hist = h
            .dispatch("dialogue_history", json!({"session_thread": thread}))
            .await
            .unwrap();
        assert_eq!(hist["messages"].as_array().unwrap().len(), 2);
    }

    /// The role filter and the recency window narrow the broadcast set.
    #[tokio::test]
    async fn broadcast_respects_role_and_recency() {
        let store = test_store().await;
        // A stale cc peer (last_seen far in the past) and a fresh mu peer.
        store.touch_peer("cc:old", 1000).await.unwrap();
        store.touch_peer("mu:d1:s1", now_ms()).await.unwrap();
        store.touch_peer("cc:fresh", now_ms()).await.unwrap();
        let h = DialogueHandler { store };

        // Default window: the stale peer is not addressed.
        let out = h
            .dispatch(
                "dialogue_broadcast",
                json!({"from": "mu:d1:s1", "content": "x"}),
            )
            .await
            .unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["recipients"][0], "cc:fresh");

        // Role filter: address only mu peers (sender excluded → zero).
        let out = h
            .dispatch(
                "dialogue_broadcast",
                json!({"from": "mu:d1:s1", "content": "x", "role": "mu"}),
            )
            .await
            .unwrap();
        assert_eq!(out["count"], 0);
    }

    /// Multicast reaches current team members only, excluding the sender;
    /// join/leave change the set.
    #[tokio::test]
    async fn multicast_delivers_to_team_members() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        for p in ["cc:a", "cc:b", "cc:c"] {
            h.dispatch(
                "dialogue_team_join",
                json!({"team": "search", "peer_id": p}),
            )
            .await
            .unwrap();
        }
        // A non-member never sees team traffic.
        h.dispatch(
            "dialogue_poll",
            json!({"to": "cc:outsider", "since": 0, "timeout_ms": 0}),
        )
        .await
        .unwrap();

        let out = h
            .dispatch(
                "dialogue_multicast",
                json!({"from": "cc:a", "team": "search", "content": "regroup"}),
            )
            .await
            .unwrap();
        assert_eq!(out["count"], 2);
        assert_eq!(out["team"], "search");

        for p in ["cc:b", "cc:c"] {
            let polled = h
                .dispatch(
                    "dialogue_poll",
                    json!({"to": p, "since": 0, "timeout_ms": 0}),
                )
                .await
                .unwrap();
            assert_eq!(polled["messages"].as_array().unwrap().len(), 1);
        }
        let outsider = h
            .dispatch(
                "dialogue_poll",
                json!({"to": "cc:outsider", "since": 0, "timeout_ms": 0}),
            )
            .await
            .unwrap();
        assert_eq!(outsider["messages"].as_array().unwrap().len(), 0);

        // cc:b leaves; the next multicast reaches only cc:c.
        let left = h
            .dispatch(
                "dialogue_team_leave",
                json!({"team": "search", "peer_id": "cc:b"}),
            )
            .await
            .unwrap();
        assert_eq!(left["removed"], true);
        let out = h
            .dispatch(
                "dialogue_multicast",
                json!({"from": "cc:a", "team": "search", "content": "again"}),
            )
            .await
            .unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["recipients"][0], "cc:c");

        // Unknown team → zero recipients, not an error.
        let out = h
            .dispatch(
                "dialogue_multicast",
                json!({"from": "cc:a", "team": "ghosts", "content": "hello?"}),
            )
            .await
            .unwrap();
        assert_eq!(out["count"], 0);
    }

    #[tokio::test]
    async fn teams_listing_and_membership_views() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        h.dispatch(
            "dialogue_team_join",
            json!({"team": "alpha", "peer_id": "cc:a"}),
        )
        .await
        .unwrap();
        h.dispatch(
            "dialogue_team_join",
            json!({"team": "alpha", "peer_id": "cc:b"}),
        )
        .await
        .unwrap();
        h.dispatch(
            "dialogue_team_join",
            json!({"team": "beta", "peer_id": "cc:b"}),
        )
        .await
        .unwrap();
        // join is idempotent
        let again = h
            .dispatch(
                "dialogue_team_join",
                json!({"team": "alpha", "peer_id": "cc:a"}),
            )
            .await
            .unwrap();
        assert_eq!(again["members"], 2);

        let all = h.dispatch("dialogue_teams", json!({})).await.unwrap();
        let teams = all["teams"].as_array().unwrap();
        assert_eq!(teams.len(), 2);

        let alpha = h
            .dispatch("dialogue_teams", json!({"team": "alpha"}))
            .await
            .unwrap();
        assert_eq!(alpha["members"].as_array().unwrap().len(), 2);

        let of_a = h
            .dispatch("dialogue_teams", json!({"peer_id": "cc:a"}))
            .await
            .unwrap();
        let of_a_teams = of_a["teams"].as_array().unwrap();
        assert_eq!(of_a_teams.len(), 1);
        assert_eq!(of_a_teams[0]["team"], "alpha");
    }

    /// Prune removes idle registrations and their team memberships; fresh
    /// peers and their memberships survive.
    #[tokio::test]
    async fn prune_removes_stale_peers_and_memberships() {
        let store = test_store().await;
        store.touch_peer("cc:stale", 1000).await.unwrap();
        store.touch_peer("cc:fresh", now_ms()).await.unwrap();
        store.team_join("t", "cc:stale", 1000).await.unwrap();
        store.team_join("t", "cc:fresh", now_ms()).await.unwrap();
        let h = DialogueHandler { store };

        let out = h.dispatch("dialogue_prune", json!({})).await.unwrap();
        assert_eq!(out["removed_peers"], 1);
        assert_eq!(out["removed_memberships"], 1);

        let peers = h.dispatch("dialogue_peers", json!({})).await.unwrap();
        let ids: Vec<_> = peers["peers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["peer_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["cc:fresh"]);
        let team = h
            .dispatch("dialogue_teams", json!({"team": "t"}))
            .await
            .unwrap();
        let members = team["members"].as_array().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0]["peer_id"], "cc:fresh");
    }

    #[tokio::test]
    async fn say_and_poll_register_peers_for_discovery() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        // A sender is registered from `from`; a poller from `to`.
        h.dispatch(
            "dialogue_say",
            json!({"from": "cc:abc123", "to": "mu:d1:s1", "content": "hi"}),
        )
        .await
        .unwrap();
        h.dispatch(
            "dialogue_poll",
            json!({"to": "mu:d1:s1", "since": 0, "timeout_ms": 0}),
        )
        .await
        .unwrap();

        // Both peers are now discoverable; role is the prefix before ':'.
        let all = h.dispatch("dialogue_peers", json!({})).await.unwrap();
        let peers = all["peers"].as_array().unwrap();
        assert_eq!(peers.len(), 2);
        let by_id = |id: &str| peers.iter().find(|p| p["peer_id"] == id).unwrap().clone();
        assert_eq!(by_id("cc:abc123")["role"], "cc");
        assert_eq!(by_id("mu:d1:s1")["role"], "mu");

        // role filter narrows to one kind.
        let just_mu = h
            .dispatch("dialogue_peers", json!({"role": "mu"}))
            .await
            .unwrap();
        let mu_peers = just_mu["peers"].as_array().unwrap();
        assert_eq!(mu_peers.len(), 1);
        assert_eq!(mu_peers[0]["peer_id"], "mu:d1:s1");
    }

    #[tokio::test]
    async fn peers_recency_filter_and_unprefixed_role() {
        let h = DialogueHandler {
            store: test_store().await,
        };
        // An id with no ':' takes the whole string as its role.
        h.dispatch(
            "dialogue_say",
            json!({"from": "warden", "to": "mu:d1:s1", "content": "x"}),
        )
        .await
        .unwrap();
        let all = h.dispatch("dialogue_peers", json!({})).await.unwrap();
        let warden = all["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["peer_id"] == "warden")
            .unwrap();
        assert_eq!(warden["role"], "warden");

        // A tiny recency window excludes everyone (last_seen is in the past
        // relative to a fresh `now`, and the window is sub-millisecond-ish).
        let recent = h
            .dispatch("dialogue_peers", json!({"active_within_ms": 0_i64}))
            .await
            .unwrap();
        // active_within_ms = 0 means "no filter", so this still returns peers.
        assert!(!recent["peers"].as_array().unwrap().is_empty());
    }
}
