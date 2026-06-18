//! mu-dialogue — a networked multi-peer inter-agent dialogue channel over MCP.
//!
//! Revived from `c137-dialogue-mcp` (bead at-revive-dialogue-mcp-8rk): the
//! "email / inbox over MCP" model — the only inter-agent *messaging* the stack
//! has (c137-blink is telemetry). Three tools over a `dialogue` table in
//! agent.sqlite:
//!
//!   - dialogue_say(from, to, content, session_thread?)  → {id, ts}
//!   - dialogue_poll(to, since?, timeout_ms?, limit?)     → {messages: [...]}   (notify long-poll)
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
    env,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rmcp::model::*;
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map as JsonMap, Value};
use tokio::sync::{Mutex, Notify};
use tokio::time::timeout;
use tracing::{info, warn};
use ulid::Ulid;

const SERVER_NAME: &str = "mu-dialogue";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_POLL_TIMEOUT_MS: u64 = 30_000;
/// Cap on how long a single `notified()` wait blocks before re-checking the
/// store. The wake is notify-driven (not busy-wait); this only bounds the
/// worst-case latency to observe a message inserted by a *different* process
/// (cross-process writers don't fire this process's in-memory `Notify`).
const POLL_RECHECK_INTERVAL_MS: u64 = 1_000;

// ───────────────────────────── Storage ──────────────────────────────────────

#[derive(Clone)]
struct Store {
    db: Arc<Mutex<Connection>>,
    notify: Arc<Notify>,
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
        "#,
    )?;
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
        Ok((id, ts))
    }

    async fn fetch_for(&self, to: &str, since_ms: i64, limit: i64) -> Result<Vec<DialogueRow>> {
        let conn = self.db.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, from_peer, to_peer, session_thread, content, ts
               FROM dialogue
              WHERE to_peer = ? AND ts > ?
              ORDER BY ts ASC
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![to, since_ms, limit], dialogue_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    async fn history(&self, session_thread: &str, limit: i64) -> Result<Vec<DialogueRow>> {
        let conn = self.db.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, from_peer, to_peer, session_thread, content, ts
               FROM dialogue
              WHERE session_thread = ?
              ORDER BY ts ASC
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
}

fn dialogue_row(row: &rusqlite::Row) -> rusqlite::Result<DialogueRow> {
    Ok(DialogueRow {
        id: row.get(0)?,
        from: row.get(1)?,
        to: row.get(2)?,
        session_thread: row.get(3)?,
        content: row.get(4)?,
        ts: row.get(5)?,
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
    let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_POLL_TIMEOUT_MS);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    // Polling its own inbox proves the poller (`to`) is live.
    store.touch_peer(&args.to, now_ms()).await?;

    loop {
        let rows = store.fetch_for(&args.to, args.since, limit).await?;
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
    Ok(json!({ "peers": peers, "now": now }))
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
                    "since":      {"type": "number", "description": "epoch_ms cutoff; only messages with ts > since are returned (default 0)"},
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
    ]
}

impl DialogueHandler {
    /// Dispatch one tool call to its handler, returning the JSON payload or a
    /// human-readable error string (surfaced as an MCP tool error).
    async fn dispatch(&self, name: &str, arguments: Value) -> std::result::Result<Value, String> {
        match name {
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
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

impl ServerHandler for DialogueHandler {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, VERSION))
            .with_instructions(
                "Multi-peer inter-agent dialogue channel (the email/inbox-over-MCP model). \
                 dialogue_say to send, dialogue_poll to long-poll an inbox, dialogue_history \
                 to replay a thread, dialogue_peers to discover who is on the channel. \
                 Peers: cc, mu, warden subagents, orchestrators.\n\
                 \n\
                 Peer ids are 'role:identity'. Identify yourself consistently in the \
                 'from'/'to' fields: a Claude Code peer uses 'cc:' + its \
                 CLAUDE_CODE_SESSION_ID (e.g. 'cc:2257560e-...'); an mu session uses \
                 'mu:<daemon_id>:<session_id>'. The 'role:' prefix is what dialogue_peers \
                 groups on. Presence is activity-derived — you appear to others the first \
                 time you say or poll, not at connect.",
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
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        async move {
            let arguments = Value::Object(request.arguments.unwrap_or_default());
            match self.dispatch(&request.name, arguments).await {
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

fn open_store() -> Result<Store> {
    let db_path = default_db_path();
    info!(version = VERSION, db = %db_path.display(), "mu-dialogue starting");
    let conn =
        Connection::open(&db_path).with_context(|| format!("open db {}", db_path.display()))?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    migrate(&conn).context("schema migration")?;
    Ok(Store {
        db: Arc::new(Mutex::new(conn)),
        notify: Arc::new(Notify::new()),
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
    let store = open_store()?;

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
    fn advertises_four_tools() {
        let names: Vec<_> = tools_list().iter().map(|t| t.name.to_string()).collect();
        assert_eq!(
            names,
            [
                "dialogue_say",
                "dialogue_poll",
                "dialogue_history",
                "dialogue_peers"
            ]
        );
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
