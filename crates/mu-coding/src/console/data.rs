use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mu_core::{
    agent::Usage,
    event_log::{SessionEvent, SessionEventLog},
};

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) events_dir: PathBuf,
    pub(crate) analytics_db: Option<PathBuf>,
    pub(crate) base_path: String,
}

impl AppState {
    pub(crate) fn href(&self, path: &str) -> String {
        if self.base_path.is_empty() {
            path.to_string()
        } else if path == "/" {
            self.base_path.clone()
        } else {
            format!("{}{}", self.base_path, path)
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ScanResult {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) malformed_files: usize,
    pub(crate) skipped_entries: usize,
}

#[derive(Debug)]
pub(crate) struct SessionSummary {
    pub(crate) daemon_id: String,
    pub(crate) session_id: String,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) last_activity_unix_ms: Option<u64>,
    pub(crate) ask_count: u32,
    pub(crate) context_assembly_count: u32,
    pub(crate) tool_call_count: u32,
    pub(crate) usage: Option<Usage>,
    /// mu-index-mark-column-auiv: latest operator-mark rating, so the
    /// index shows which sessions are already covered.
    pub(crate) mark: Option<u8>,
}

pub(crate) fn scan_sessions(events_dir: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    let Ok(daemons) = std::fs::read_dir(events_dir) else {
        return result;
    };
    for daemon in daemons.flatten() {
        let Ok(ft) = daemon.file_type() else {
            result.skipped_entries += 1;
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let daemon_id = daemon.file_name().to_string_lossy().to_string();
        let Ok(files) = std::fs::read_dir(daemon.path()) else {
            result.skipped_entries += 1;
            continue;
        };
        for file in files.flatten() {
            if file.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            match SessionEventLog::from_jsonl(&file.path()) {
                Ok((log, malformed)) => {
                    if malformed > 0 {
                        result.malformed_files += 1;
                    }
                    let (provider, model) = log
                        .provider_info()
                        .map(|(p, m)| (Some(p), Some(m)))
                        .unwrap_or((None, None));
                    result.sessions.push(SessionSummary {
                        daemon_id: daemon_id.clone(),
                        session_id: log.session_id().to_owned(),
                        provider,
                        model,
                        last_activity_unix_ms: log.last_activity_unix_ms(),
                        ask_count: log.ask_count(),
                        context_assembly_count: log.context_assembly_count(),
                        tool_call_count: log.tool_call_count(),
                        usage: log.live_usage().0.or_else(|| log.cumulative_usage()),
                        mark: log.latest_operator_mark().map(|(rating, _)| rating),
                    });
                }
                Err(_) => result.malformed_files += 1,
            }
        }
    }
    result
        .sessions
        .sort_by_key(|s| std::cmp::Reverse(s.last_activity_unix_ms.unwrap_or(0)));
    result
}

pub(crate) fn load_events(
    events_dir: &Path,
    daemon_id: &str,
    session_id: &str,
) -> Result<(Vec<SessionEvent>, usize)> {
    let path = events_dir
        .join(daemon_id)
        .join(format!("{session_id}.jsonl"));
    let (log, malformed) = SessionEventLog::from_jsonl(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok((log.snapshot(), malformed))
}

pub(crate) fn normalize_base_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return String::new();
    }
    let no_edges = trimmed.trim_matches('/');
    format!("/{no_edges}")
}
