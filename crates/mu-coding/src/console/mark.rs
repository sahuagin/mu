//! mu-operator-mark-5mwr: append an operator quality mark to a
//! session's durable event log.
//!
//! Shared by the `mu mark` CLI (quit-time capture without a browser)
//! and the console's POST mark handler. The event log stays the single
//! source of truth — the console header and the mu-mucm `session_marks`
//! view are projections; re-marking appends a newer event and readers
//! take the latest by event id.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};

/// What a successful mark did, for caller-side reporting.
#[derive(Debug)]
pub struct MarkOutcome {
    pub daemon_id: String,
    pub session_id: String,
    pub path: PathBuf,
    pub event_id: u64,
    pub rating: u8,
}

/// Sessions under `events_dir` whose session id starts with `prefix`.
/// Returns `(daemon_id, session_id, jsonl path)` tuples; an exact id is
/// just a prefix that matches once.
pub fn resolve_sessions(events_dir: &Path, prefix: &str) -> Vec<(String, String, PathBuf)> {
    let mut matches = Vec::new();
    let Ok(daemons) = std::fs::read_dir(events_dir) else {
        return matches;
    };
    for daemon in daemons.flatten() {
        if !daemon.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let daemon_id = daemon.file_name().to_string_lossy().to_string();
        let Ok(files) = std::fs::read_dir(daemon.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with(prefix) {
                matches.push((daemon_id.clone(), stem.to_string(), path));
            }
        }
    }
    matches
}

/// Append an `OperatorMark` to the session uniquely identified by
/// `session_prefix`. Refuses ambiguity rather than guessing, and
/// verifies the line actually landed on disk before reporting success
/// (the daemon-side append is deliberately best-effort; a marking tool
/// that silently drops the mark would defeat its purpose).
pub fn mark_session(
    events_dir: &Path,
    session_prefix: &str,
    rating: u8,
    note: Option<String>,
) -> Result<MarkOutcome> {
    if !(1..=5).contains(&rating) {
        bail!("rating must be 1-5, got {rating}");
    }
    let mut matches = resolve_sessions(events_dir, session_prefix);
    let (daemon_id, session_id, path) = match matches.len() {
        0 => bail!(
            "no session under {} matches '{}'",
            events_dir.display(),
            session_prefix
        ),
        1 => matches.remove(0),
        n => {
            let listing: Vec<String> = matches
                .iter()
                .take(8)
                .map(|(d, s, _)| format!("  {d}/{s}"))
                .collect();
            bail!(
                "'{}' is ambiguous ({} matches):\n{}{}",
                session_prefix,
                n,
                listing.join("\n"),
                if n > 8 { "\n  …" } else { "" }
            );
        }
    };

    let event_id = mark_session_file(&path, rating, note)?;
    Ok(MarkOutcome {
        daemon_id,
        session_id,
        path,
        event_id,
        rating,
    })
}

/// Append a mark to one specific session log file (the console's POST
/// path, where daemon and session ids are exact). Verifies the line
/// actually landed on disk before reporting success.
pub fn mark_session_file(path: &Path, rating: u8, note: Option<String>) -> Result<u64> {
    if !(1..=5).contains(&rating) {
        bail!("rating must be 1-5, got {rating}");
    }
    let (log, _malformed) = SessionEventLog::from_jsonl(path)
        .with_context(|| format!("reading event log {}", path.display()))?;
    log.attach_disk_writer(path)
        .with_context(|| format!("opening {} for append", path.display()))?;
    let note = note.filter(|n| !n.trim().is_empty());
    let event_id = log.append(
        EventActor::User,
        EventPayload::OperatorMark { rating, note },
    );

    // append()'s disk write is best-effort by design; re-read to
    // confirm the mark is durable before claiming success.
    let (reread, _) = SessionEventLog::from_jsonl(path)
        .with_context(|| format!("re-reading {} to verify mark", path.display()))?;
    let landed = reread
        .snapshot()
        .iter()
        .any(|ev| ev.id == event_id && matches!(ev.payload, EventPayload::OperatorMark { .. }));
    if !landed {
        bail!(
            "mark did not land in {} (event id {event_id} missing on re-read)",
            path.display()
        );
    }
    Ok(event_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_session(dir: &Path, daemon: &str, session: &str) -> PathBuf {
        let path = dir.join(daemon).join(format!("{session}.jsonl"));
        let log = SessionEventLog::new(session);
        log.attach_disk_writer(&path).expect("attach");
        log.append(
            EventActor::User,
            EventPayload::UserMessage {
                content: "hi".into(),
            },
        );
        path
    }

    #[test]
    fn mark_appends_durable_event_and_resolves_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        seed_session(tmp.path(), "d1", "abc123");

        let outcome = mark_session(tmp.path(), "abc", 2, Some("sluggish".into())).expect("marks");
        assert_eq!(outcome.daemon_id, "d1");
        assert_eq!(outcome.session_id, "abc123");
        assert_eq!(outcome.event_id, 2);

        // Re-mark supersedes by id; both events persist.
        let again = mark_session(tmp.path(), "abc123", 4, None).expect("re-marks");
        assert_eq!(again.event_id, 3);

        let (log, malformed) = SessionEventLog::from_jsonl(&outcome.path).expect("reread");
        assert_eq!(malformed, 0);
        let marks: Vec<(u64, u8)> = log
            .snapshot()
            .iter()
            .filter_map(|ev| match &ev.payload {
                EventPayload::OperatorMark { rating, .. } => Some((ev.id, *rating)),
                _ => None,
            })
            .collect();
        assert_eq!(marks, vec![(2, 2), (3, 4)]);
    }

    #[test]
    fn mark_refuses_ambiguity_bad_rating_and_misses() {
        let tmp = tempfile::tempdir().expect("tempdir");
        seed_session(tmp.path(), "d1", "abc123");
        seed_session(tmp.path(), "d2", "abc999");

        let err = mark_session(tmp.path(), "abc", 3, None).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");

        let err = mark_session(tmp.path(), "abc123", 0, None).unwrap_err();
        assert!(err.to_string().contains("rating must be 1-5"), "{err}");
        let err = mark_session(tmp.path(), "abc123", 6, None).unwrap_err();
        assert!(err.to_string().contains("rating must be 1-5"), "{err}");

        let err = mark_session(tmp.path(), "zzz", 3, None).unwrap_err();
        assert!(err.to_string().contains("no session"), "{err}");
    }
}
