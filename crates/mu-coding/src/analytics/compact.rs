//! Compact — read event-log JSONLs and project `TaskTelemetry` events
//! into sink rows.
//!
//! v1 calls `forensics::classify_task` with `commit=None, pr=None` (the
//! enricher that would shell out to git/gh and populate those is a
//! follow-up bead — see spec mu-042 §"Out of scope"). Terminal-exit
//! classes still come through accurately; Done-with-no-commit-info
//! lands as `NarrativeNoAction`.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;

use mu_core::event_log::{EventPayload, SessionEvent, TaskExitReason};
use mu_core::forensics::{classify_task, ClassificationInputs, TaskTelemetrySnapshot};

use super::sink::{upsert_task, TaskRow};

/// Summary of a single compact run — useful for the CLI to surface to
/// the user (e.g. "compacted 47 tasks across 12 sessions, skipped 3
/// malformed lines").
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompactSummary {
    pub files_scanned: usize,
    pub lines_read: usize,
    pub tasks_upserted: usize,
    pub malformed_lines_skipped: usize,
    pub tasks_filtered_out: usize,
}

/// Scan every `*.jsonl` file under `events_dir/<daemon_id>/` (one level
/// deep — that's the canonical layout from mu-upb). Project every
/// `TaskTelemetry` event into the sink. Optionally filter by
/// `min_ended_at_unix_ms`.
pub fn compact_dir(
    conn: &Connection,
    events_dir: &Path,
    min_ended_at_unix_ms: Option<u64>,
) -> Result<CompactSummary> {
    let mut summary = CompactSummary::default();

    if !events_dir.exists() {
        // Empty dir is not an error — the user may have run `mu serve`
        // with disk persistence off, or just never spawned a session.
        return Ok(summary);
    }

    for daemon_entry in std::fs::read_dir(events_dir)
        .with_context(|| format!("reading events dir {}", events_dir.display()))?
    {
        let daemon_entry = daemon_entry?;
        if !daemon_entry.file_type()?.is_dir() {
            continue;
        }
        for session_entry in std::fs::read_dir(daemon_entry.path())? {
            let session_entry = session_entry?;
            let path = session_entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            summary.files_scanned += 1;
            compact_file(conn, &path, min_ended_at_unix_ms, &mut summary)?;
        }
    }

    Ok(summary)
}

/// Compact a single JSONL file. Pure helper for testability.
pub fn compact_file(
    conn: &Connection,
    path: &Path,
    min_ended_at_unix_ms: Option<u64>,
    summary: &mut CompactSummary,
) -> Result<()> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading event log {}", path.display()))?;
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        summary.lines_read += 1;
        let event: SessionEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => {
                summary.malformed_lines_skipped += 1;
                continue;
            }
        };
        if let Some(row) = project_event(&event, min_ended_at_unix_ms) {
            upsert_task(conn, &row)?;
            summary.tasks_upserted += 1;
        } else if matches!(event.payload, EventPayload::TaskTelemetry { .. }) {
            // Filtered by --since
            summary.tasks_filtered_out += 1;
        }
    }
    Ok(())
}

/// Project a single SessionEvent into a TaskRow, or None if this event
/// isn't a TaskTelemetry (or is filtered out by `--since`). Pure
/// function for unit testing.
pub fn project_event(event: &SessionEvent, min_ended_at_unix_ms: Option<u64>) -> Option<TaskRow> {
    // Use a manually-deserialized shape so we don't depend on the
    // exact variant shape staying stable — we read by serde from the
    // payload tag. mu-040's `TaskTelemetry` variant is what we want.
    let payload_json = serde_json::to_value(&event.payload).ok()?;
    if payload_json.get("kind")?.as_str()? != "task_telemetry" {
        return None;
    }
    let parsed: ParsedTelemetry = serde_json::from_value(payload_json).ok()?;

    if let Some(min) = min_ended_at_unix_ms {
        if parsed.ended_at_unix_ms < min {
            return None;
        }
    }

    let exit_reason = parsed.exit_reason;
    let snapshot = TaskTelemetrySnapshot {
        task_id: parsed.task_id.clone(),
        session_id: parsed.session_id.clone(),
        exit_reason,
    };
    let classification = classify_task(&ClassificationInputs {
        telemetry: &snapshot,
        commit: None,
        pr: None,
    });

    Some(TaskRow {
        task_id: parsed.task_id,
        session_id: parsed.session_id,
        parent_task_id: parsed.parent_task_id,
        provider: parsed.provider_kind,
        model: parsed.model,
        model_version: parsed.model_version,
        started_at_unix_ms: parsed.started_at_unix_ms,
        ended_at_unix_ms: parsed.ended_at_unix_ms,
        wall_clock_ms: parsed.wall_clock_ms,
        prompt_tokens: parsed.prompt_tokens,
        completion_tokens: parsed.completion_tokens,
        cache_read_tokens: parsed.cache_read_tokens,
        cache_write_tokens: parsed.cache_write_tokens,
        exit_reason,
        classification,
    })
}

/// Subset of `EventPayload::TaskTelemetry` fields we project from. Using
/// a local struct (rather than reaching into the enum variant) means
/// the compactor doesn't need to match-update if the variant gains
/// fields in follow-up beads — new fields are simply ignored until we
/// extend this struct.
#[derive(Debug, Deserialize)]
struct ParsedTelemetry {
    task_id: String,
    session_id: String,
    #[serde(default)]
    parent_task_id: Option<String>,
    provider_kind: String,
    model: String,
    #[serde(default)]
    model_version: Option<String>,
    #[serde(default)]
    started_at_unix_ms: Option<u64>,
    ended_at_unix_ms: u64,
    #[serde(default)]
    wall_clock_ms: Option<u64>,
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    cache_read_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
    exit_reason: TaskExitReason,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::event_log::EventActor;

    fn telemetry_event() -> SessionEvent {
        SessionEvent {
            id: 99,
            session_id: "session-abc".to_owned(),
            parent_event_ids: vec![],
            timestamp_unix_ms: 1778963762000,
            actor: EventActor::System,
            payload: EventPayload::TaskTelemetry {
                task_id: "task-00000000000000000001".to_owned(),
                session_id: "session-abc".to_owned(),
                parent_task_id: None,
                provider_kind: "openrouter".to_owned(),
                model: "deepseek/deepseek-v4-flash".to_owned(),
                model_version: None,
                started_at_unix_ms: None,
                ended_at_unix_ms: 1778963762000,
                wall_clock_ms: Some(2000),
                prompt_tokens: Some(2400),
                completion_tokens: Some(17),
                cache_read_tokens: None,
                cache_write_tokens: None,
                tools_granted: vec![],
                tools_actually_called: vec![],
                exit_reason: TaskExitReason::Done,
                max_budget_usd: None,
                actual_spend_usd: None,
                local_hour: None,
                day_of_week: None,
                tz: None,
            },
        }
    }

    #[test]
    fn project_telemetry_event_produces_row() {
        let event = telemetry_event();
        let row = project_event(&event, None).expect("telemetry should project");
        assert_eq!(row.task_id, "task-00000000000000000001");
        assert_eq!(row.session_id, "session-abc");
        assert_eq!(row.provider, "openrouter");
        assert_eq!(row.model, "deepseek/deepseek-v4-flash");
        assert_eq!(row.exit_reason, TaskExitReason::Done);
        assert_eq!(row.prompt_tokens, Some(2400));
        // No commit info → classifier returns NarrativeNoAction for Done.
        use mu_core::forensics::Outcome;
        assert_eq!(row.classification.outcome, Outcome::NarrativeNoAction);
    }

    #[test]
    fn project_non_telemetry_event_returns_none() {
        let event = SessionEvent {
            id: 1,
            session_id: "session-x".to_owned(),
            parent_event_ids: vec![],
            timestamp_unix_ms: 0,
            actor: EventActor::User,
            payload: EventPayload::UserMessage {
                content: "hi".to_owned(),
            },
        };
        assert!(project_event(&event, None).is_none());
    }

    #[test]
    fn project_respects_since_filter() {
        let event = telemetry_event();
        // ended_at = 1778963762000
        assert!(project_event(&event, Some(1778963762001)).is_none());
        assert!(project_event(&event, Some(1778963761999)).is_some());
    }

    #[test]
    fn compact_file_skips_malformed_lines() {
        let tmp = std::env::temp_dir().join("mu_8ypx_compact_malformed.jsonl");
        let _ = std::fs::remove_file(&tmp);
        // Write one valid telemetry + one garbage line + one blank.
        let event = telemetry_event();
        let valid = serde_json::to_string(&event).unwrap();
        std::fs::write(&tmp, format!("{}\nnot json at all\n\n{}\n", valid, valid)).unwrap();

        let dbpath = std::env::temp_dir().join("mu_8ypx_compact_malformed.sqlite");
        let _ = std::fs::remove_file(&dbpath);
        let conn = super::super::sink::open(&dbpath).unwrap();

        let mut summary = CompactSummary::default();
        compact_file(&conn, &tmp, None, &mut summary).unwrap();

        assert_eq!(summary.lines_read, 3); // blank skipped before counting
        assert_eq!(summary.tasks_upserted, 2);
        assert_eq!(summary.malformed_lines_skipped, 1);

        drop(conn);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&dbpath);
    }

    #[test]
    fn compact_dir_missing_returns_empty_summary() {
        let dbpath = std::env::temp_dir().join("mu_8ypx_compact_missing.sqlite");
        let _ = std::fs::remove_file(&dbpath);
        let conn = super::super::sink::open(&dbpath).unwrap();

        let summary =
            compact_dir(&conn, std::path::Path::new("/nonexistent/path/here"), None).unwrap();
        assert_eq!(summary.files_scanned, 0);
        assert_eq!(summary.tasks_upserted, 0);

        drop(conn);
        let _ = std::fs::remove_file(&dbpath);
    }
}
