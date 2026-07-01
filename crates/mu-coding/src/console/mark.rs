//! mu-operator-mark-5mwr: append an operator quality mark to a
//! session's durable event log.
//!
//! Shared by the `mu mark` CLI (quit-time capture without a browser)
//! and the console's POST mark handler. The event log stays the single
//! source of truth — the console header and the mu-mucm `session_marks`
//! view are projections; re-marking appends a newer event and readers
//! take the latest by event id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rand::Rng;
use rusqlite::{params, Connection};

use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};

use super::time::now_rfc3339_utc;

fn split_mark_session_ref(reference: &str) -> (Option<&str>, &str) {
    let reference = reference.strip_prefix("mu:").unwrap_or(reference);
    if let Some((daemon, session)) = reference.split_once('/') {
        return (Some(daemon), session);
    }
    if let Some((daemon, session)) = reference.split_once(':') {
        return (Some(daemon), session);
    }
    (None, reference)
}

/// What a successful mark did, for caller-side reporting.
#[derive(Debug)]
pub struct MarkOutcome {
    pub daemon_id: String,
    pub session_id: String,
    pub path: PathBuf,
    pub event_id: u64,
    pub rating: u8,
}

/// Sessions under `events_dir` whose session id starts with `reference`.
///
/// `reference` may be either the historical bare session id/prefix
/// (`session-1`, `sess`) or a daemon-qualified ref copied from
/// `mu list-sessions`: `<daemon>/<session>` (also accepts
/// `mu:<daemon>/<session>` and `<daemon>:<session>` for symmetry with
/// resume-style refs). Daemon and session components are prefix-matched;
/// ambiguity is still refused by [`mark_session`].
///
/// Returns `(daemon_id, session_id, jsonl path)` tuples.
pub fn resolve_sessions(events_dir: &Path, reference: &str) -> Vec<(String, String, PathBuf)> {
    let (daemon_prefix, session_prefix) = split_mark_session_ref(reference);
    let mut matches = Vec::new();
    let Ok(daemons) = std::fs::read_dir(events_dir) else {
        return matches;
    };
    for daemon in daemons.flatten() {
        if !daemon.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let daemon_id = daemon.file_name().to_string_lossy().to_string();
        if daemon_prefix
            .map(|prefix| !daemon_id.starts_with(prefix))
            .unwrap_or(false)
        {
            continue;
        }
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
            if stem.starts_with(session_prefix) {
                matches.push((daemon_id.clone(), stem.to_string(), path));
            }
        }
    }
    matches
}

/// Append an `OperatorMark` to the session uniquely identified by
/// `session_ref`. Refuses ambiguity rather than guessing, and
/// verifies the line actually landed on disk before reporting success
/// (the daemon-side append is deliberately best-effort; a marking tool
/// that silently drops the mark would defeat its purpose).
pub fn mark_session(
    events_dir: &Path,
    session_ref: &str,
    rating: u8,
    note: Option<String>,
) -> Result<MarkOutcome> {
    if !(1..=5).contains(&rating) {
        bail!("rating must be 1-5, got {rating}");
    }
    let mut matches = resolve_sessions(events_dir, session_ref);
    let (daemon_id, session_id, path) = match matches.len() {
        0 => bail!(
            "no session under {} matches '{}'",
            events_dir.display(),
            session_ref
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
                session_ref,
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

// ───────────────────────── cc-session marks (sidecar) ─────────────────────
//
// mu-cc-sessions-console-lqqt.3: claude-code session transcripts under
// `~/.claude-personal/projects/` are claude-code's OWN files — appending an
// OperatorMark to them is a hard invariant violation (the 2026-06-05
// ownership incident). cc marks instead land in the task_log sidecar
// (`~/.local/share/task_log.sqlite`, `tasks` table) as ordinary rows. mu
// sessions keep the OperatorMark event path above; this is the second,
// provider-keyed storage path the console mark POST routes cc sessions to.
//
// FIELD CONTRACT (locked 2026-06-06, shared with incident-ref rows already in
// the DB and consumed by mu-mucm.1's `session_marks` view — do not drift):
//   session_ref = "cc:<uuid>"               (fleet-prefixed join key)
//   tags        = ["session-mark","fleet:cc"]  (JSON array)
//   description = "session-mark rating=<N>/5"   (rating, single digit 1-5)
//   result      = <note text>                (NULL when no note)
//   status      = "completed"
//   agent       = "mu-console"
//   created_at / updated_at = RFC3339 UTC, "+00:00" offset (second resolution)
// Re-marking APPENDS a fresh row; "latest wins" on read is by insertion order
// (SQLite `rowid` DESC), since second-resolution timestamps tie on rapid
// re-marks. mu-mucm.1's view should use the same tiebreaker.

/// The default task_log sidecar DB. `None` if the home dir can't be
/// resolved. Mirrors [`super::cc_data::default_cc_projects_dir`].
pub fn default_cc_marks_db() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".local/share/task_log.sqlite"))
}

/// A cc-session mark as read back from the sidecar — the shape the detail
/// header (sibling bead .2) renders. `created_at` is the row's RFC3339
/// timestamp, so the header can show when the latest mark was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcMark {
    pub rating: u8,
    pub note: Option<String>,
    pub created_at: String,
}

/// Extract a Claude Code session id from CLI-facing refs accepted by
/// `mu mark`: `cc:<session>`, `claude-code:<session>`,
/// `cc:<project>/<session>`, or the console URL path
/// `/cc/<project>/<session>` (leading slash optional). Returns `None`
/// for native mu refs so callers can route those to [`mark_session`].
pub fn cc_session_id_from_ref(reference: &str) -> Option<&str> {
    let reference = reference.strip_prefix('/').unwrap_or(reference);
    let rest = reference
        .strip_prefix("cc:")
        .or_else(|| reference.strip_prefix("claude-code:"))
        .or_else(|| reference.strip_prefix("cc/"))
        .or_else(|| reference.strip_prefix("claude-code/"))?;
    let session = rest.rsplit('/').next().unwrap_or(rest);
    (!session.is_empty()).then_some(session)
}

fn valid_cc_session_id(session_uuid: &str) -> bool {
    !session_uuid.is_empty() && !session_uuid.contains(['/', '\\']) && !session_uuid.contains("..")
}

/// What a successful cc mark wrote, for caller-side reporting.
#[derive(Debug)]
pub struct CcMarkOutcome {
    pub session_ref: String,
    pub row_id: String,
    pub rating: u8,
}

/// The fleet-prefixed join key for a cc session uuid.
fn cc_session_ref(session_uuid: &str) -> String {
    format!("cc:{session_uuid}")
}

/// Ensure the `tasks` table exists. On the real task_log DB it already
/// does (so this is a no-op and the live FTS triggers fire on our
/// insert); on a fresh temp DB used by tests it creates the bare table
/// without the FTS5 mirror, which is all the roundtrip needs.
fn ensure_tasks_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id          TEXT PRIMARY KEY,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            cwd         TEXT NOT NULL,
            description TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'completed',
            result      TEXT,
            tags        TEXT NOT NULL DEFAULT '[]',
            agent       TEXT NOT NULL DEFAULT 'claude',
            session_ref TEXT
        );",
    )
    .context("ensuring task_log tasks table")?;
    Ok(())
}

/// Append a cc-session quality mark to the task_log sidecar at `db_path`.
/// Writes one `tasks` row per the locked field contract above; re-marking
/// the same session appends a newer row (latest wins on read). NEVER
/// touches the cc transcript file.
pub fn mark_cc_session(
    db_path: &Path,
    session_uuid: &str,
    rating: u8,
    note: Option<String>,
) -> Result<CcMarkOutcome> {
    if !(1..=5).contains(&rating) {
        bail!("rating must be 1-5, got {rating}");
    }
    if !valid_cc_session_id(session_uuid) {
        bail!("cc session id is empty or path-like");
    }
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating task_log dir {}", parent.display()))?;
        }
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening task_log sidecar {}", db_path.display()))?;
    ensure_tasks_table(&conn)?;

    let session_ref = cc_session_ref(session_uuid);
    let note = note.filter(|n| !n.trim().is_empty());
    let now = now_rfc3339_utc();
    let row_id = new_row_id();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "-".to_string());
    let description = format!("session-mark rating={rating}/5");
    let tags = r#"["session-mark","fleet:cc"]"#;

    conn.execute(
        "INSERT INTO tasks
            (id, created_at, updated_at, cwd, description, status, result, tags, agent, session_ref)
         VALUES (?1, ?2, ?2, ?3, ?4, 'completed', ?5, ?6, 'mu-console', ?7)",
        params![row_id, now, cwd, description, note, tags, session_ref],
    )
    .with_context(|| format!("inserting cc mark for {session_ref}"))?;

    // Verify the row landed before reporting success — the sidecar write
    // is the cc analog of mark_session_file's durability re-read.
    let landed = cc_session_mark(db_path, session_uuid)?
        .map(|m| m.rating == rating)
        .unwrap_or(false);
    if !landed {
        bail!(
            "cc mark did not land in {} for {session_ref}",
            db_path.display()
        );
    }
    Ok(CcMarkOutcome {
        session_ref,
        row_id,
        rating,
    })
}

/// Read the latest mark for one cc session — the clean seam the cc detail
/// header (sibling bead .2) calls. Returns `None` when the session has no
/// mark row. A missing DB file is treated as "no mark", not an error.
pub fn cc_session_mark(db_path: &Path, session_uuid: &str) -> Result<Option<CcMark>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening task_log sidecar {}", db_path.display()))?;
    let session_ref = cc_session_ref(session_uuid);
    let row = conn
        .query_row(
            "SELECT description, result, created_at
               FROM tasks
              WHERE session_ref = ?1
                AND tags LIKE '%\"session-mark\"%'
              ORDER BY rowid DESC
              LIMIT 1",
            params![session_ref],
            |r| {
                let description: String = r.get(0)?;
                let result: Option<String> = r.get(1)?;
                let created_at: String = r.get(2)?;
                Ok((description, result, created_at))
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .with_context(|| format!("reading cc mark for {session_ref}"))?;
    Ok(row.and_then(|(description, result, created_at)| {
        parse_rating(&description).map(|rating| CcMark {
            rating,
            note: result.filter(|n| !n.trim().is_empty()),
            created_at,
        })
    }))
}

/// Latest rating per cc session uuid, for populating the index mark column
/// in one query. Keyed by the bare uuid (the `cc:` prefix stripped) so it
/// matches `SessionSummary::session_id`. A missing DB yields an empty map.
pub(crate) fn cc_marks_by_session(db_path: &Path) -> HashMap<String, u8> {
    let mut map = HashMap::new();
    if !db_path.exists() {
        return map;
    }
    let Ok(conn) = Connection::open(db_path) else {
        return map;
    };
    // Rows ordered newest-first; first row seen per session wins (latest).
    let Ok(mut stmt) = conn.prepare(
        "SELECT session_ref, description
           FROM tasks
          WHERE session_ref LIKE 'cc:%'
            AND tags LIKE '%\"session-mark\"%'
          ORDER BY rowid DESC",
    ) else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        let session_ref: String = r.get(0)?;
        let description: String = r.get(1)?;
        Ok((session_ref, description))
    }) else {
        return map;
    };
    for row in rows.flatten() {
        let (session_ref, description) = row;
        let Some(uuid) = session_ref.strip_prefix("cc:") else {
            continue;
        };
        if map.contains_key(uuid) {
            continue; // already have the latest for this session
        }
        if let Some(rating) = parse_rating(&description) {
            map.insert(uuid.to_string(), rating);
        }
    }
    map
}

/// Extract the rating from a `"session-mark rating=<N>/5"` description.
/// Tolerant of surrounding text; the rating is always a single digit 1-5.
fn parse_rating(description: &str) -> Option<u8> {
    let idx = description.find("rating=")? + "rating=".len();
    let digit = description[idx..].chars().next()?;
    let n = digit.to_digit(10)? as u8;
    (1..=5).contains(&n).then_some(n)
}

/// A short hex row id matching task_log's `uuid4().hex[:8]` convention.
fn new_row_id() -> String {
    format!("{:08x}", rand::thread_rng().gen::<u32>())
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
    fn mark_accepts_daemon_qualified_refs_from_list_sessions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        seed_session(tmp.path(), "63d69fe6a408d77e", "session-1");
        seed_session(tmp.path(), "dc5b7377666c1882", "session-1");

        let outcome = mark_session(
            tmp.path(),
            "63d69fe6a408d77e/session-1",
            5,
            Some("enjoyable".into()),
        )
        .expect("daemon/session marks exact listed session");
        assert_eq!(outcome.daemon_id, "63d69fe6a408d77e");
        assert_eq!(outcome.session_id, "session-1");

        let outcome = mark_session(tmp.path(), "mu:dc5b7377666c1882/session", 4, None)
            .expect("mu:daemon/session also marks");
        assert_eq!(outcome.daemon_id, "dc5b7377666c1882");
        assert_eq!(outcome.session_id, "session-1");

        let outcome = mark_session(tmp.path(), "63d69fe6a408d77e:session-1", 3, None)
            .expect("daemon:session also marks");
        assert_eq!(outcome.daemon_id, "63d69fe6a408d77e");
        assert_eq!(outcome.session_id, "session-1");

        let err = mark_session(tmp.path(), "session-1", 2, None).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
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

    // ── cc-session sidecar marks (temp sqlite, never the real task_log) ──

    fn temp_db(tag: &str) -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        dir.join(format!("task_log-{tag}.sqlite"))
    }

    #[test]
    fn cc_mark_roundtrips_and_latest_wins() {
        let db = temp_db("roundtrip");
        let uuid = "cff69449-850a-4854-9268-b0cb113beb88";

        // No mark yet, and a missing DB is "no mark", not an error.
        assert_eq!(cc_session_mark(&db, uuid).expect("read empty"), None);

        let out = mark_cc_session(&db, uuid, 2, Some("sluggish".into())).expect("first mark");
        assert_eq!(out.session_ref, format!("cc:{uuid}"));
        assert_eq!(out.rating, 2);

        let got = cc_session_mark(&db, uuid).expect("read").expect("some");
        assert_eq!(got.rating, 2);
        assert_eq!(got.note.as_deref(), Some("sluggish"));
        assert!(got.created_at.ends_with("+00:00"), "{}", got.created_at);

        // Re-marking appends a fresh row; the latest wins on read.
        mark_cc_session(&db, uuid, 4, None).expect("re-mark");
        let got = cc_session_mark(&db, uuid).expect("read").expect("some");
        assert_eq!(got.rating, 4);
        assert_eq!(got.note, None, "blank note stored as NULL");

        // Both rows persist (append-only, like OperatorMark events).
        let conn = Connection::open(&db).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE session_ref = ?1",
                params![format!("cc:{uuid}")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);

        // Stored exactly per the locked field contract.
        let (desc, tags, agent, status): (String, String, String, String) = conn
            .query_row(
                "SELECT description, tags, agent, status FROM tasks
                  WHERE session_ref = ?1 ORDER BY rowid DESC LIMIT 1",
                params![format!("cc:{uuid}")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(desc, "session-mark rating=4/5");
        assert_eq!(tags, r#"["session-mark","fleet:cc"]"#);
        assert_eq!(agent, "mu-console");
        assert_eq!(status, "completed");
    }

    #[test]
    fn cc_session_ref_parser_accepts_cli_and_console_forms() {
        assert_eq!(cc_session_id_from_ref("cc:sess-aaa"), Some("sess-aaa"));
        assert_eq!(
            cc_session_id_from_ref("claude-code:cff69449-850a-4854-9268-b0cb113beb88"),
            Some("cff69449-850a-4854-9268-b0cb113beb88")
        );
        assert_eq!(
            cc_session_id_from_ref("cc:-home-tcovert-src-mu/sess-aaa"),
            Some("sess-aaa")
        );
        assert_eq!(
            cc_session_id_from_ref("/cc/-home-tcovert-src-mu/sess-aaa"),
            Some("sess-aaa")
        );
        assert_eq!(cc_session_id_from_ref("session-1"), None);
        assert_eq!(cc_session_id_from_ref("mu:daemon/session-1"), None);
    }

    #[test]
    fn cc_mark_rejects_bad_rating_and_empty_id() {
        let db = temp_db("reject");
        let err = mark_cc_session(&db, "abc", 0, None).unwrap_err();
        assert!(err.to_string().contains("rating must be 1-5"), "{err}");
        let err = mark_cc_session(&db, "abc", 6, None).unwrap_err();
        assert!(err.to_string().contains("rating must be 1-5"), "{err}");
        let err = mark_cc_session(&db, "", 3, None).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
        let err = mark_cc_session(&db, "../abc", 3, None).unwrap_err();
        assert!(err.to_string().contains("path-like"), "{err}");
    }

    #[test]
    fn cc_marks_by_session_indexes_latest_per_session() {
        let db = temp_db("index");
        mark_cc_session(&db, "aaa", 1, None).expect("a1");
        mark_cc_session(&db, "aaa", 5, Some("recovered".into())).expect("a2");
        mark_cc_session(&db, "bbb", 3, None).expect("b1");

        let map = cc_marks_by_session(&db);
        assert_eq!(map.get("aaa"), Some(&5), "latest rating for aaa");
        assert_eq!(map.get("bbb"), Some(&3));
        assert_eq!(map.len(), 2);

        // Marks keyed by the bare uuid — no `cc:` prefix leaks into the key.
        assert!(!map.contains_key("cc:aaa"));
    }

    #[test]
    fn cc_marks_ignore_non_mark_rows_sharing_the_join_key() {
        // An incident-ref row shares session_ref but isn't a mark; it must
        // not be read as one (the join key is shared with mu-mucm's views).
        let db = temp_db("incident");
        let conn = Connection::open(&db).unwrap();
        ensure_tasks_table(&conn).unwrap();
        conn.execute(
            "INSERT INTO tasks
                (id, created_at, updated_at, cwd, description, status, result, tags, agent, session_ref)
             VALUES ('inc00001', '2026-06-06T07:45:19+00:00', '2026-06-06T07:45:19+00:00',
                     '/home/tcovert', 'incident-ref: some postmortem', 'completed', 'report',
                     '[\"incident-ref\",\"fleet:cc\"]', 'claude', 'cc:zzz')",
            [],
        )
        .unwrap();

        assert_eq!(cc_session_mark(&db, "zzz").expect("read"), None);
        assert!(cc_marks_by_session(&db).is_empty());

        // A real mark on the same session is found alongside the incident row.
        mark_cc_session(&db, "zzz", 3, Some("degraded".into())).expect("mark");
        let got = cc_session_mark(&db, "zzz").expect("read").expect("some");
        assert_eq!(got.rating, 3);
        assert_eq!(got.note.as_deref(), Some("degraded"));
    }
}
