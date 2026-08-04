//! Live-turn write-ahead log (mu-d04a slice 2).
//!
//! Streaming deltas and tool events for the in-flight turn append here as
//! they arrive; the file truncates when the turn commits to the event log.
//! Renderer-journal rigor tier, deliberately (operator, 2026-08-03): cheap
//! JSONL appends, errors swallowed, no replay guarantees, no schema
//! ceremony — the pedantry stays in the model-side event log. Value: crash
//! readback (a mu-solo death mid-turn leaves the streamed content on disk)
//! plus the uniform two-append-only-logs read model of the mu-d04a design.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Per-process live-turn WAL. One in-flight turn exists today (the
/// per-session generalization rides mu-d04a Phase 2); the file is keyed by
/// session id so concurrent daemons never collide.
pub struct TurnWal {
    file: Option<File>,
    /// Warn once, then stay silent — a broken WAL must never break the TUI.
    warned: bool,
}

/// WAL directory: alongside the renderer journal, never near the semantic
/// event store.
pub fn wal_path(session_id: &str) -> Option<PathBuf> {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dirs::data_dir().map(|d| {
        d.join("mu")
            .join("solo")
            .join("wal")
            .join(format!("{safe}.jsonl"))
    })
}

impl TurnWal {
    /// Open (create) the WAL for `session_id`. Non-fatal: on any failure the
    /// WAL is inert and the TUI proceeds without it.
    pub fn open(session_id: &str) -> Self {
        let file = wal_path(session_id).and_then(|p| {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            OpenOptions::new().create(true).append(true).open(&p).ok()
        });
        Self {
            file,
            warned: false,
        }
    }

    /// Append one event. `kind` is a short tag ("text" | "thinking" |
    /// "tool" | "tool_result"); `text` is the delta/content.
    pub fn append(&mut self, kind: &str, text: &str) {
        let Some(ref mut f) = self.file else { return };
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = serde_json::json!({ "ts_ms": ts_ms, "kind": kind, "text": text });
        if writeln!(f, "{line}").is_err() && !self.warned {
            tracing::warn!("live-turn WAL append failed; continuing without it");
            self.warned = true;
        }
    }

    /// Truncate on turn commit — the content now lives in the event log.
    pub fn truncate(&mut self) {
        if let Some(ref mut f) = self.file {
            let _ = f.set_len(0);
            let _ = f.seek(SeekFrom::Start(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_wal() -> (TurnWal, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "mu-solo-wal-test-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        (
            TurnWal {
                file: Some(file),
                warned: false,
            },
            path,
        )
    }

    #[test]
    fn appends_valid_jsonl_and_truncates_on_commit() {
        let (mut wal, path) = temp_wal();
        wal.append("text", "hello ");
        wal.append("tool", "Bash(ls)");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(v["ts_ms"].is_number());
            assert!(v["kind"].is_string());
        }
        wal.truncate();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn inert_wal_is_silent() {
        let mut wal = TurnWal {
            file: None,
            warned: false,
        };
        // Must not panic or error.
        wal.append("text", "x");
        wal.truncate();
    }

    #[test]
    fn wal_path_sanitizes_session_ids() {
        let p = wal_path("session/1:weird id").unwrap();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "session_1_weird_id.jsonl");
    }
}
