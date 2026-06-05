//! mu-console: read-only web operator console over mu event logs.
//!
//! V1 is deliberately boring: Axum + server-rendered HTML, reading
//! `~/.local/share/mu/events/<daemon_id>/<session_id>.jsonl` directly.
//! It is an inspection projection, not a control surface.

mod data;
mod html;
mod views;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    response::{Html, IntoResponse, Redirect},
    routing::get,
    Router,
};
use serde::Deserialize;

use self::{
    data::{normalize_base_path, AppState},
    views::{
        compare_placeholder, render_compaction_one, render_context_one, render_session_page,
        render_sessions_index, DetailTab,
    },
};

#[derive(Debug, Clone)]
pub struct ConsoleOptions {
    pub bind: SocketAddr,
    pub base_path: String,
    pub events_dir: PathBuf,
    pub analytics_db: Option<PathBuf>,
}

pub async fn run(opts: ConsoleOptions) -> Result<()> {
    let base_path = normalize_base_path(&opts.base_path);
    let state = Arc::new(AppState {
        events_dir: opts.events_dir,
        analytics_db: opts.analytics_db,
        base_path: base_path.clone(),
    });

    let inner = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/sessions", get(sessions_index))
        .route("/sessions/{daemon_id}/{session_id}", get(session_detail))
        .route(
            "/sessions/{daemon_id}/{session_id}/events",
            get(session_events),
        )
        .route("/sessions/{daemon_id}/{session_id}/cost", get(session_cost))
        .route(
            "/sessions/{daemon_id}/{session_id}/context",
            get(session_context),
        )
        .route(
            "/sessions/{daemon_id}/{session_id}/context/{model_call_id}",
            get(session_context_one),
        )
        .route(
            "/sessions/{daemon_id}/{session_id}/compactions",
            get(session_compactions),
        )
        .route(
            "/sessions/{daemon_id}/{session_id}/compactions/{model_call_id}",
            get(session_compaction_one),
        )
        .route("/compare", get(compare))
        .with_state(state);

    let app = if base_path.is_empty() {
        inner
    } else {
        Router::new().nest(&base_path, inner)
    };

    let listener = tokio::net::TcpListener::bind(opts.bind)
        .await
        .with_context(|| format!("binding mu console to {}", opts.bind))?;
    eprintln!(
        "mu console listening on http://{}{}",
        opts.bind,
        if base_path.is_empty() {
            "/"
        } else {
            &base_path
        }
    );
    axum::serve(listener, app)
        .await
        .context("serving mu console")
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn index(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Redirect::temporary(&state.href("/sessions"))
}

async fn sessions_index(State(state): State<Arc<AppState>>) -> Html<String> {
    render_sessions_index(state)
}

async fn session_detail(
    State(state): State<Arc<AppState>>,
    AxumPath((daemon_id, session_id)): AxumPath<(String, String)>,
) -> Html<String> {
    render_session_page(state, daemon_id, session_id, DetailTab::Overview)
}

async fn session_events(
    State(state): State<Arc<AppState>>,
    AxumPath((daemon_id, session_id)): AxumPath<(String, String)>,
) -> Html<String> {
    render_session_page(state, daemon_id, session_id, DetailTab::Events)
}

async fn session_cost(
    State(state): State<Arc<AppState>>,
    AxumPath((daemon_id, session_id)): AxumPath<(String, String)>,
) -> Html<String> {
    render_session_page(state, daemon_id, session_id, DetailTab::Cost)
}

async fn session_context(
    State(state): State<Arc<AppState>>,
    AxumPath((daemon_id, session_id)): AxumPath<(String, String)>,
) -> Html<String> {
    render_session_page(state, daemon_id, session_id, DetailTab::Context)
}

async fn session_context_one(
    State(state): State<Arc<AppState>>,
    AxumPath((daemon_id, session_id, model_call_id)): AxumPath<(String, String, u32)>,
) -> Html<String> {
    render_context_one(state, daemon_id, session_id, model_call_id)
}

async fn session_compactions(
    State(state): State<Arc<AppState>>,
    AxumPath((daemon_id, session_id)): AxumPath<(String, String)>,
) -> Html<String> {
    render_session_page(state, daemon_id, session_id, DetailTab::Compactions)
}

async fn session_compaction_one(
    State(state): State<Arc<AppState>>,
    AxumPath((daemon_id, session_id, model_call_id)): AxumPath<(String, String, u32)>,
) -> Html<String> {
    render_compaction_one(state, daemon_id, session_id, model_call_id)
}

#[derive(Debug, Deserialize)]
struct CompareQuery {
    left: Option<String>,
    right: Option<String>,
}

async fn compare(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CompareQuery>,
) -> Html<String> {
    compare_placeholder(state, q.left, q.right)
}
