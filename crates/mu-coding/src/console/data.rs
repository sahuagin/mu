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
    /// mu-cc-sessions-console-lqqt.1: claude-code projects dir to merge
    /// into the index, or `None` to scan only mu's native event logs.
    pub(crate) cc_projects_dir: Option<PathBuf>,
    /// mu-cc-sessions-console-lqqt.3: task_log sidecar DB holding cc
    /// session marks. Populates the index mark column for cc rows and
    /// receives the console mark POST's cc storage path. `None` keeps cc
    /// marks unread/unwritten.
    pub(crate) cc_marks_db: Option<PathBuf>,
    /// mu-console-hosts-dashboard-zy26: path to the cron-generated stats
    /// HTML served at GET /dashboard, read fresh per request.
    pub(crate) dashboard_path: PathBuf,
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
    /// The session's model. For sessions that switch models mid-run this
    /// is last-model-wins (the most recent assistant model); `models_seen`
    /// records how many distinct models appeared so the switch is visible
    /// (mu-y5hz).
    pub(crate) model: Option<String>,
    /// mu-y5hz: count of distinct models seen across the session. 1 for a
    /// normal single-model session, ≥2 flags a mid-run model switch, 0
    /// when no model could be resolved (then `model` is `None` too).
    pub(crate) models_seen: u8,
    pub(crate) last_activity_unix_ms: Option<u64>,
    pub(crate) ask_count: u32,
    pub(crate) context_assembly_count: u32,
    pub(crate) tool_call_count: u32,
    pub(crate) usage: Option<Usage>,
    /// mu-index-mark-column-auiv: latest operator-mark rating, so the
    /// index shows which sessions are already covered.
    pub(crate) mark: Option<u8>,
    /// mu-y5hz: subagent (isSidechain:true) message turns excluded from
    /// this session's ask/assistant/tool/usage rollups and surfaced here
    /// rather than silently dropped. Always 0 for native mu sessions,
    /// which have no sidechain concept.
    pub(crate) sidechain_entries: u32,
}

/// mu-cc-sessions-console-lqqt.1: the index's scan entry point. Always
/// scans mu's native event logs; when `cc_projects_dir` is `Some`, also
/// scans claude-code transcripts and merges both corpora into one
/// last-activity-sorted list. The two scanners are independent and
/// best-effort, so their malformed/skipped counts simply add.
pub(crate) fn scan_all(
    events_dir: &Path,
    cc_projects_dir: Option<&Path>,
    cc_marks_db: Option<&Path>,
) -> ScanResult {
    let mut result = scan_sessions(events_dir);
    if let Some(dir) = cc_projects_dir {
        let mut cc = crate::console::cc_data::scan_cc_sessions(dir);
        // mu-cc-sessions-console-lqqt.3: cc marks live in the task_log
        // sidecar, not the transcript — populate the index mark column
        // from there (latest row wins). mu sessions already carry their
        // mark from the OperatorMark event in scan_sessions above.
        if let Some(db) = cc_marks_db {
            let marks = crate::console::mark::cc_marks_by_session(db);
            for s in &mut cc.sessions {
                s.mark = marks.get(&s.session_id).copied();
            }
        }
        result.sessions.extend(cc.sessions);
        result.malformed_files += cc.malformed_files;
        result.skipped_entries += cc.skipped_entries;
        result
            .sessions
            .sort_by_key(|s| std::cmp::Reverse(s.last_activity_unix_ms.unwrap_or(0)));
    }
    result
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
            // mu-console-skip-supervisor-ankc: each daemon dir holds the
            // agent session log(s) alongside a `supervisor.jsonl` of
            // daemon-lifecycle records. That file carries no SessionEvent
            // ask/usage content, so ingesting it produced a metricless
            // "supervisor" ghost row in the sessions index. Skip it by
            // name — daemon-lifecycle logs may earn their own surface
            // later, but never as a fake session row.
            if file.file_name().to_str() == Some("supervisor.jsonl") {
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
                        // Native mu sessions report a single provider/model
                        // pair; 1 when resolved, 0 otherwise. The mid-run
                        // model-switch case (models_seen ≥ 2) is a cc-only
                        // concern today (mu-y5hz).
                        models_seen: u8::from(model.is_some()),
                        model,
                        last_activity_unix_ms: log.last_activity_unix_ms(),
                        ask_count: log.ask_count(),
                        context_assembly_count: log.context_assembly_count(),
                        tool_call_count: log.tool_call_count(),
                        usage: log.live_usage().0.or_else(|| log.cumulative_usage()),
                        mark: log.latest_operator_mark().map(|(rating, _)| rating),
                        // Native mu sessions have no sidechain concept.
                        sidechain_entries: 0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mu-scan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Write a one-event session log at `<dir>/<file>` via the real
    /// serializer, so the fixture matches on-disk reality rather than a
    /// hand-rolled JSON literal that could drift from the event format.
    fn write_session_log(dir: &Path, file: &str, session_id: &str) {
        let log = SessionEventLog::new(session_id);
        log.attach_disk_writer(&dir.join(file)).unwrap();
        log.append(
            EventActor::System,
            EventPayload::SessionCreated {
                provider_kind: "anthropic".into(),
                model: "claude-opus-4-8".into(),
                parent_session_id: None,
                branched_at_parent_event_id: None,
                usage_semantics: None,
            },
        );
    }

    /// mu-console-skip-supervisor-ankc: a daemon dir holds the agent
    /// session log alongside a `supervisor.jsonl` of daemon-lifecycle
    /// records. Both are valid JSONL, so this guards the *filename* skip:
    /// only the real session may surface as an index row.
    #[test]
    fn scan_skips_supervisor_jsonl() {
        let root = tmp("supervisor-skip");
        let daemon = root.join("daemon-1");
        std::fs::create_dir_all(&daemon).unwrap();
        write_session_log(&daemon, "session-1.jsonl", "session-1");
        write_session_log(&daemon, "supervisor.jsonl", "supervisor");

        let scan = scan_sessions(&root);
        assert_eq!(
            scan.sessions.len(),
            1,
            "supervisor.jsonl must not appear as a session row"
        );
        assert_eq!(scan.sessions[0].session_id, "session-1");
        assert_eq!(scan.malformed_files, 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
