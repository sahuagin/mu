//! Cheap, offline index over the on-disk session event logs
//! (`<events_dir>/<daemon_id>/<session_id>.jsonl`).
//!
//! bead `mu-lazy-session-rehydration-bh4f`. The daemon used to bulk-load
//! and fully parse *every* session log at startup (mu-u1ld
//! `rehydrate_sessions`) just so `session.list` and id-addressed queries
//! could see past sessions. On a box with thousands of logs that meant
//! thousands of full JSONL parses before `mu serve` was usable.
//!
//! This module replaces that with two cheap, request-driven primitives:
//!
//! - [`scan_session_index`] — ENUMERATE. One read of the FIRST line per
//!   file (the `SessionCreated` record → provider/model/started) plus the
//!   file mtime (last-activity, free from dir metadata). No full parse.
//!   Backs the standalone `mu list-sessions` command and any future
//!   multi-session UI.
//! - [`find_session_path`] — FIND-BY-ID. Locate the one
//!   `<daemon>/<id>.jsonl` for a known id without enumerating or parsing
//!   the rest. Backs lazy `resume` / `recover` / `session.events` /
//!   `session.stats`, which already have the id and just need a match.
//!
//! Neither runs at startup; rehydration is now on demand.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use mu_core::event_log::{EventPayload, SessionEvent};

/// A lightweight session listing row — everything obtainable WITHOUT a
/// full log parse. The heavy aggregates (`ask_count`, token usage,
/// status, …) deliberately aren't here: computing them means reading the
/// whole file, which is exactly what this module avoids. Open the session
/// (`session.events` / `session.stats`) to get those.
#[derive(Debug, Clone)]
pub struct SessionHeader {
    pub session_id: String,
    pub daemon_id: String,
    /// From the first `SessionCreated` record, when present.
    pub provider_kind: Option<String>,
    pub model: Option<String>,
    pub parent_session_id: Option<String>,
    /// Timestamp of the first event (≈ session start). `None` if the file
    /// is empty / unreadable.
    pub started_at_unix_ms: Option<u64>,
    /// File mtime — a free, good-enough proxy for last activity (the log
    /// is append-only, so the last write is the last event).
    pub last_activity_unix_ms: u64,
    pub path: PathBuf,
}

fn mtime_unix_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read ONLY the first line of a session JSONL to build a [`SessionHeader`].
/// Returns `None` only if the path can't be opened at all; an empty or
/// malformed first line still yields a header (id from the filename stem)
/// so the session remains listable.
pub fn read_session_header(path: &Path, daemon_id: &str) -> Option<SessionHeader> {
    use std::io::BufRead;

    let stem_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    let last_activity_unix_ms = mtime_unix_ms(path);

    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first = String::new();
    // A read error or empty file: fall back to a filename-only header.
    let _ = reader.read_line(&mut first);
    let first = first.trim_end();

    let parsed: Option<SessionEvent> = if first.is_empty() {
        None
    } else {
        serde_json::from_str::<SessionEvent>(first).ok()
    };

    let (session_id, started_at_unix_ms, provider_kind, model, parent_session_id) = match parsed {
        Some(ev) => {
            let (provider_kind, model, parent_session_id) = match ev.payload {
                EventPayload::SessionCreated {
                    provider_kind,
                    model,
                    parent_session_id,
                    ..
                } => (Some(provider_kind), Some(model), parent_session_id),
                _ => (None, None, None),
            };
            (
                ev.session_id,
                Some(ev.timestamp_unix_ms),
                provider_kind,
                model,
                parent_session_id,
            )
        }
        None => (stem_id?, None, None, None, None),
    };

    Some(SessionHeader {
        session_id,
        daemon_id: daemon_id.to_string(),
        provider_kind,
        model,
        parent_session_id,
        started_at_unix_ms,
        last_activity_unix_ms,
        path: path.to_path_buf(),
    })
}

/// Total number of session logs discovered alongside the headers
/// returned. `total` is the full count under `events_dir`; `headers` is
/// the (possibly `last`-capped) newest-first slice. The split lets the
/// caller show "showing N of TOTAL" without a second walk.
#[derive(Debug, Clone, Default)]
pub struct SessionIndex {
    pub headers: Vec<SessionHeader>,
    pub total: usize,
}

/// Enumerate sessions under `events_dir`, newest-first by last-activity.
/// `last` caps the returned headers to the most-recent N (`None` = all).
///
/// Cheap by construction: the recency sort uses only dir-entry **mtime**
/// (no file opened), and a first-line `SessionCreated` read happens ONLY
/// for the rows actually returned. So `--last 10` over thousands of logs
/// opens ~10 files, not thousands. Best-effort — unreadable dirs/files
/// are skipped, never fatal.
pub fn scan_session_index(events_dir: &Path, last: Option<usize>) -> SessionIndex {
    // Phase 1: collect (path, daemon_id, mtime) from metadata only.
    let mut candidates: Vec<(PathBuf, String, u64)> = Vec::new();
    let Ok(daemons) = std::fs::read_dir(events_dir) else {
        return SessionIndex::default();
    };
    for daemon in daemons.flatten() {
        let daemon_path = daemon.path();
        if !daemon_path.is_dir() {
            continue;
        }
        let daemon_id = match daemon_path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let Ok(files) = std::fs::read_dir(&daemon_path) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let mtime = mtime_unix_ms(&path);
            candidates.push((path, daemon_id.clone(), mtime));
        }
    }
    let total = candidates.len();

    // Phase 2: recency sort (mtime only — no file opened yet) + cap.
    candidates.sort_by_key(|(_, _, mtime)| std::cmp::Reverse(*mtime));
    if let Some(n) = last {
        candidates.truncate(n);
    }

    // Phase 3: read the first-line header ONLY for the surviving rows.
    let headers = candidates
        .iter()
        .filter_map(|(path, daemon_id, _)| read_session_header(path, daemon_id))
        .collect();

    SessionIndex { headers, total }
}

/// Find the on-disk log for a known session id without enumerating or
/// parsing the rest. Returns the first `<daemon>/<id>.jsonl` that exists.
/// O(number of daemon dirs) stats, zero log parses.
///
/// `session_id` is RPC-supplied (`session.resume` / `session.events` /
/// `session.stats` / `session.close`), so it is validated to a single
/// safe filename component before the `format!` + `join` below — a
/// traversal id like `../../etc/foo` or `/etc/foo` would otherwise let
/// `Path::join` escape `events_dir` and stat/parse arbitrary `*.jsonl`
/// files. The old bulk-rehydration only walked `read_dir` output, so
/// this guard closes a surface the lazy path would otherwise open.
pub fn find_session_path(events_dir: &Path, session_id: &str) -> Option<PathBuf> {
    // Accept only a bare filename component (no separators, no `..`, no
    // root/prefix). `file_name()` normalizes those away, so equality with
    // the raw input is the validation.
    if Path::new(session_id).file_name() != Some(OsStr::new(session_id)) {
        return None;
    }
    let file_name = format!("{session_id}.jsonl");
    let daemons = std::fs::read_dir(events_dir).ok()?;
    for daemon in daemons.flatten() {
        let daemon_path = daemon.path();
        if !daemon_path.is_dir() {
            continue;
        }
        let candidate = daemon_path.join(&file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_events_dir() -> PathBuf {
        let pid = std::process::id();
        let unique = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("mu-sessidx-test-{pid}-{unique}"));
        std::fs::create_dir_all(&dir).expect("create events dir");
        dir
    }

    /// Write a minimal session log: a SessionCreated line + one more event.
    fn write_session(events_dir: &Path, daemon: &str, session: &str, provider: &str, model: &str) {
        let daemon_dir = events_dir.join(daemon);
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        let created = format!(
            r#"{{"id":1,"session_id":"{session}","timestamp_unix_ms":1700000000000,"actor":{{"kind":"system"}},"payload":{{"kind":"session_created","provider_kind":"{provider}","model":"{model}"}}}}"#
        );
        let user = format!(
            r#"{{"id":2,"session_id":"{session}","timestamp_unix_ms":1700000001000,"actor":{{"kind":"user"}},"payload":{{"kind":"user_message","content":"hi"}}}}"#
        );
        std::fs::write(
            daemon_dir.join(format!("{session}.jsonl")),
            format!("{created}\n{user}\n"),
        )
        .expect("write session");
    }

    #[test]
    fn header_reads_first_record_only() {
        let dir = fresh_events_dir();
        write_session(&dir, "daemon-a", "session-1", "ollama", "qwen3-coder:30b");
        let path = dir.join("daemon-a").join("session-1.jsonl");
        let h = read_session_header(&path, "daemon-a").expect("header");
        assert_eq!(h.session_id, "session-1");
        assert_eq!(h.daemon_id, "daemon-a");
        assert_eq!(h.provider_kind.as_deref(), Some("ollama"));
        assert_eq!(h.model.as_deref(), Some("qwen3-coder:30b"));
        assert_eq!(h.started_at_unix_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn empty_file_still_yields_filename_header() {
        let dir = fresh_events_dir();
        let daemon_dir = dir.join("daemon-a");
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        let path = daemon_dir.join("ghost.jsonl");
        std::fs::write(&path, "").expect("write empty");
        let h = read_session_header(&path, "daemon-a").expect("header");
        assert_eq!(h.session_id, "ghost");
        assert_eq!(h.provider_kind, None);
        assert_eq!(h.started_at_unix_ms, None);
    }

    #[test]
    fn scan_discovers_all_and_caps_with_last() {
        let dir = fresh_events_dir();
        write_session(&dir, "daemon-a", "session-1", "ollama", "m1");
        write_session(&dir, "daemon-a", "session-2", "ollama", "m2");
        write_session(&dir, "daemon-b", "session-3", "openrouter", "m3");
        // (Writes are near-simultaneous so mtime order isn't deterministic;
        // assert discovery + the cap, not a specific recency order.)
        let all = scan_session_index(&dir, None);
        assert_eq!(all.headers.len(), 3, "all three sessions discovered");
        assert_eq!(all.total, 3);

        let capped = scan_session_index(&dir, Some(2));
        assert_eq!(capped.headers.len(), 2, "--last 2 caps the returned rows");
        assert_eq!(capped.total, 3, "total still reflects all on disk");
    }

    #[test]
    fn find_by_id_locates_without_enumerating() {
        let dir = fresh_events_dir();
        write_session(&dir, "daemon-a", "session-1", "ollama", "m1");
        write_session(&dir, "daemon-b", "session-7", "openrouter", "m3");
        let found = find_session_path(&dir, "session-7").expect("found");
        assert!(found.ends_with("daemon-b/session-7.jsonl"));
        assert!(find_session_path(&dir, "does-not-exist").is_none());
    }

    #[test]
    fn find_by_id_rejects_path_traversal() {
        // session_id is RPC-supplied; a traversal id must never let the
        // join escape events_dir to stat/parse an arbitrary *.jsonl.
        let dir = fresh_events_dir();
        write_session(&dir, "daemon-a", "session-1", "ollama", "m1");
        // Legit id still resolves.
        assert!(find_session_path(&dir, "session-1").is_some());
        // Separators, parent refs, absolute paths, and empties are rejected.
        for bad in [
            "../../../etc/passwd",
            "a/b",
            "..",
            "/etc/hosts",
            "",
            "sub/session-1",
        ] {
            assert!(
                find_session_path(&dir, bad).is_none(),
                "traversal id must be rejected: {bad:?}"
            );
        }
    }
}
