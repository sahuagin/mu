//! mu-console: web operator console over mu event logs.
//!
//! V1 is deliberately boring: Axum + server-rendered HTML, reading
//! `~/.local/share/mu/events/<daemon_id>/<session_id>.jsonl` directly.
//! It is an inspection projection, not a control surface — with one
//! deliberate exception: POST `/sessions/{d}/{s}/mark` appends an
//! `OperatorMark` quality event (mu-operator-mark-5mwr). That write
//! goes through the same event-log append path as everything else;
//! the console itself still renders only what the log says.

mod cc_data;
mod data;
mod html;
pub mod mark;
mod views;

pub use cc_data::default_cc_projects_dir;
pub use mark::default_cc_marks_db;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{Form, Path as AxumPath, Query, State},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Router,
};
use serde::Deserialize;

use self::{
    data::{normalize_base_path, AppState},
    views::{
        compare_placeholder, render_cc_session_page, render_compaction_one, render_context_one,
        render_session_page, render_sessions_index, CcDetailTab, DetailTab,
    },
};

#[derive(Debug, Clone)]
pub struct ConsoleOptions {
    pub bind: SocketAddr,
    pub base_path: String,
    pub events_dir: PathBuf,
    pub analytics_db: Option<PathBuf>,
    /// mu-cc-sessions-console-lqqt.1: when set, also scan this
    /// claude-code projects dir and merge cc sessions into the index.
    /// `None` keeps cc scanning off (explicit opt-in).
    pub cc_projects_dir: Option<PathBuf>,
    /// mu-cc-sessions-console-lqqt.3: task_log sidecar DB for cc session
    /// marks (index column + mark POST). `None` keeps cc marks off.
    pub cc_marks_db: Option<PathBuf>,
    /// mu-console-hosts-dashboard-zy26: path to the cron-generated stats
    /// HTML served at GET /dashboard. The file is read fresh per request
    /// (it regenerates hourly out-of-band); a missing file renders a
    /// friendly note instead of an error. Default: ~/mu-stats/dashboard.html.
    pub dashboard_path: PathBuf,
}

/// mu-console-hosts-dashboard-zy26: default location of the cron-generated
/// dashboard artifact. `None` only when the home dir can't be resolved.
pub fn default_dashboard_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("mu-stats/dashboard.html"))
}

pub async fn run(opts: ConsoleOptions) -> Result<()> {
    let base_path = normalize_base_path(&opts.base_path);
    let state = Arc::new(AppState {
        events_dir: opts.events_dir,
        analytics_db: opts.analytics_db,
        cc_projects_dir: opts.cc_projects_dir,
        cc_marks_db: opts.cc_marks_db,
        dashboard_path: opts.dashboard_path,
        base_path: base_path.clone(),
    });

    let inner = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/dashboard", get(dashboard))
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
        .route(
            "/sessions/{daemon_id}/{session_id}/mark",
            post(session_mark),
        )
        // mu-cc-sessions-console-lqqt.2: claude-code detail view. A
        // distinct route prefix keeps cc sessions (read from the cc
        // projects dir) off the event-log detail path; the index links
        // cc rows here.
        .route("/cc/{project_dir}/{session_id}", get(cc_session_detail))
        .route(
            "/cc/{project_dir}/{session_id}/events",
            get(cc_session_events),
        )
        .route("/cc/{project_dir}/{session_id}/cost", get(cc_session_cost))
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

/// mu-console-hosts-dashboard-zy26: serve the latest cron-generated stats
/// HTML. The artifact is a complete standalone document, so on success it
/// is returned verbatim (wrapping it in the console chrome would nest two
/// documents). It is read fresh every request — the cron pipeline rewrites
/// it hourly and the console deliberately does no caching. A missing or
/// unreadable file renders a friendly note inside the console chrome
/// instead of a bare error, so a not-yet-generated dashboard is explained
/// rather than surfaced as a 500.
async fn dashboard(State(state): State<Arc<AppState>>) -> Html<String> {
    match std::fs::read_to_string(&state.dashboard_path) {
        Ok(contents) => Html(contents),
        Err(e) => {
            let body = format!(
                "<h1>dashboard</h1><p class=warn>No dashboard artifact yet.</p>\
                 <p class=muted>Expected at <code>{}</code> ({}). It is generated \
                 hourly by the stats cron pipeline; check back once that has run.</p>",
                html::esc(&state.dashboard_path.display().to_string()),
                html::esc(&e.to_string())
            );
            html::page(&state, "dashboard", &body)
        }
    }
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

async fn cc_session_detail(
    State(state): State<Arc<AppState>>,
    AxumPath((project_dir, session_id)): AxumPath<(String, String)>,
) -> Html<String> {
    render_cc_session_page(state, project_dir, session_id, CcDetailTab::Transcript)
}

async fn cc_session_events(
    State(state): State<Arc<AppState>>,
    AxumPath((project_dir, session_id)): AxumPath<(String, String)>,
) -> Html<String> {
    render_cc_session_page(state, project_dir, session_id, CcDetailTab::Events)
}

async fn cc_session_cost(
    State(state): State<Arc<AppState>>,
    AxumPath((project_dir, session_id)): AxumPath<(String, String)>,
) -> Html<String> {
    render_cc_session_page(state, project_dir, session_id, CcDetailTab::Cost)
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

#[derive(Debug, Deserialize)]
struct MarkForm {
    rating: u8,
    note: Option<String>,
    /// mu-cc-sessions-console-lqqt.3: the cc detail form sends
    /// `provider=claude-code` so the POST routes to the task_log sidecar
    /// instead of an OperatorMark event. Absent → mu session (default).
    provider: Option<String>,
}

/// mu-operator-mark-5mwr / mu-cc-sessions-console-lqqt.3: append an
/// operator quality mark, then bounce back to the session page (which
/// re-reads the mark and shows it). mu sessions append an `OperatorMark`
/// event; claude-code sessions write a task_log sidecar row instead —
/// their transcripts are claude-code's files and must never be appended
/// to. The GET routes pass ids straight into a path join; this route
/// writes, so it additionally refuses path-ish id segments outright.
async fn session_mark(
    State(state): State<Arc<AppState>>,
    AxumPath((daemon_id, session_id)): AxumPath<(String, String)>,
    Form(form): Form<MarkForm>,
) -> axum::response::Response {
    let id_ok = |s: &str| !s.is_empty() && !s.contains(['/', '\\']) && !s.contains("..");
    if !id_ok(&daemon_id) || !id_ok(&session_id) {
        return Html("<h1>bad request</h1><p class=err>invalid session path</p>".to_string())
            .into_response();
    }

    // Provider-keyed dispatch. A session is cc when the form says so, or
    // (defensively, if the detail form omits the hint) when no mu event
    // log exists for it but a cc transcript does. Filesystem detection
    // keeps the POST self-contained rather than trusting the form alone.
    let mu_path = state
        .events_dir
        .join(&daemon_id)
        .join(format!("{session_id}.jsonl"));
    let is_cc = form.provider.as_deref() == Some("claude-code")
        || (!mu_path.exists()
            && state
                .cc_projects_dir
                .as_ref()
                .map(|d| {
                    d.join(&daemon_id)
                        .join(format!("{session_id}.jsonl"))
                        .exists()
                })
                .unwrap_or(false));

    let result = if is_cc {
        match state.cc_marks_db.as_deref() {
            Some(db) => mark::mark_cc_session(db, &session_id, form.rating, form.note).map(|_| ()),
            None => Err(anyhow::anyhow!(
                "cc marks unavailable: no task_log sidecar configured"
            )),
        }
    } else {
        mark::mark_session_file(&mu_path, form.rating, form.note).map(|_| ())
    };

    match result {
        Ok(()) => Redirect::to(&state.href(&format!("/sessions/{daemon_id}/{session_id}")))
            .into_response(),
        Err(e) => Html(format!(
            "<h1>mark failed</h1><p class=err>{}</p>",
            html::esc(&e.to_string())
        ))
        .into_response(),
    }
}
