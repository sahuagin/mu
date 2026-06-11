//! Daemon control-plane command journal (spec mu-046).
//!
//! The durable record of every command that crosses the daemon's
//! border, written BEFORE anything processes it. The named pattern:
//! disruptor + event sourcing, with the core treated like a matching
//! engine — adapters at the edges, a sequenced durable journal in the
//! middle, receipts out.
//!
//! Lives at `~/.local/share/mu/journal/<daemon_id>.jsonl` — a sibling
//! of `events/`, so the session-log scanners (`sessions_index`,
//! `discovery/file_backend`) never see it.
//!
//! Unlike [`SessionEventLog::append`](crate::event_log::SessionEventLog::append)
//! (best-effort, swallow-and-continue), this journal is LOAD-BEARING:
//! [`CommandJournal::append`] fsyncs per policy and propagates IO
//! errors so the caller can fail closed (reject the command with
//! `JOURNAL_UNAVAILABLE`, never process it — spec mu-046 INV-2).

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single record in a daemon's control-plane journal.
///
/// Envelope is shared across all kinds; payload is a tagged enum so
/// each kind keeps its own typed shape — same scheme as
/// [`SessionEvent`](crate::event_log::SessionEvent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    /// Monotonic per-journal id, starting at 1 — THE command id.
    /// Within a pipeline, journal order == queue order == processing
    /// order (spec mu-046 INV-3). NOT globally unique.
    pub seq: u64,
    pub daemon_id: String,
    /// Unix milliseconds at append time.
    pub timestamp_unix_ms: u64,
    pub payload: JournalPayload,
}

/// Typed journal payload. Common envelope, different shapes per kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalPayload {
    /// Journal (re)opened. Appended on EVERY open, so daemon restarts
    /// are legible boundaries in the record stream.
    JournalOpened { mu_version: String, pid: u32 },
    /// The resolved startup config entered the control plane as a
    /// message (spec mu-046 INV-9). `config` is redacted — secrets
    /// never hit a journal (INV-6; see
    /// [`redact_config`](crate::config::redact_config)).
    ConfigLoaded { sources: Vec<String>, config: Value },
    /// A command crossed the border. Journaled — fsync'd per policy —
    /// before it enters any queue (INV-1). The append happens BEFORE
    /// the auth gate, so rejected commands are visible too.
    CommandReceived {
        /// JSON-RPC id (client-chosen, NOT unique).
        request_id: Value,
        method: String,
        /// Secret-redacted params (INV-6).
        params: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        auth: AuthSnapshot,
        origin: Origin,
    },
    /// Receipt: the command completed. Wraps the original command
    /// (INV-5) — the receipt is self-contained evidence of what was
    /// asked and what came of it.
    CommandSucceeded {
        /// The `seq` of the `CommandReceived` this answers.
        command_seq: u64,
        command: CommandEcho,
        result: Value,
        elapsed_ms: u64,
    },
    /// Receipt: the command failed in processing.
    CommandFailed {
        command_seq: u64,
        command: CommandEcho,
        code: i32,
        message: String,
        elapsed_ms: u64,
    },
    /// Receipt: the command was rejected before any handler ran
    /// (auth gate, validation, routing). Rejections are receipts too.
    CommandRejected {
        command_seq: u64,
        command: CommandEcho,
        code: i32,
        message: String,
        stage: RejectStage,
    },
    /// An append-only compensating record that marks an earlier record
    /// as poisoned. The journal is NEVER edited — same rule as the
    /// session log's `Tombstone` (event_log.rs). Recovery may
    /// tombstone orphaned commands; it never erases them (INV-4).
    Tombstone { target_seq: u64, reason: String },
}

/// Spec mu-046 guideline: the "success object WRAPPING the original
/// command". Embedded in every receipt so it stands alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandEcho {
    /// JSON-RPC id (client-chosen, NOT unique).
    pub request_id: Value,
    pub method: String,
    pub params: Value,
}

/// spec mu-046 WP4: explicit receipt correlation for accept-async
/// session commands (`ask_session`). Minted by the ingest pipeline
/// when the command's `CommandReceived` lands in the session's own
/// event log, then threaded through `AgentInput::UserMessage` into the
/// agent loop, and carried back out on the terminal
/// `AgentEvent::Done` — so the forwarder can write the
/// `CommandSucceeded`/`CommandFailed` receipt with the correct pairing
/// even when several asks are queued (no inference via side tables).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandTicket {
    /// Session-log event id of the `CommandReceived` this ticket
    /// answers (the session pipeline's command id).
    pub command_event_id: u64,
    /// The original command (INV-5: receipts wrap the original).
    pub echo: CommandEcho,
    /// Unix ms the command crossed the border — receipts compute
    /// their `elapsed_ms` from this.
    pub received_at_unix_ms: u64,
}

/// Which gate rejected a command before processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectStage {
    AuthGate,
    Validation,
    Routing,
}

/// Authentication state of the connection at the moment a command
/// crossed the border. Carried on the journal/log record so gating
/// can land later (capability/biscuit work) without re-architecting —
/// the snapshot reserves the spot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthSnapshot {
    Authenticated,
    Unauthenticated,
    Denied,
}

/// Transport/connection identity of the producer that brought a
/// command across the border. Kept minimal and serde-stable —
/// adapters fill in what they know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// Adapter name, e.g. `"stdio"`, `"mcp"`.
    pub transport: String,
    /// Per-connection id when the transport has one. `None` for
    /// single-connection transports (stdio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
}

/// Durability policy for [`CommandJournal::append`]. Maps from the
/// `[journal].fsync` config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// `sync_data()` after every append, before returning — the
    /// default. A command is durable before it is processed (INV-1).
    Always,
    /// No fsync (tests / ephemeral daemons). Bytes still hit the OS
    /// before append returns; they just aren't forced to the platter.
    Never,
}

/// Append-only daemon control-plane journal.
///
/// Single writer per journal: `append` serializes writers through an
/// internal lock, so seq order == file order (INV-3).
#[derive(Debug)]
pub struct CommandJournal {
    daemon_id: String,
    /// Writers hold this for the whole assign-seq → write → fsync
    /// sequence, so records land on disk in seq order.
    file: Mutex<File>,
    next_seq: AtomicU64,
    fsync: FsyncPolicy,
}

impl CommandJournal {
    /// Open (or create) the journal at `path`, recovering `next_seq`
    /// from the existing file's tail, then append a `JournalOpened`
    /// record. Errors propagate: a daemon that cannot open its journal
    /// at boot does not serve (INV-2).
    pub fn open(path: &Path, daemon_id: &str, fsync: FsyncPolicy) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Recover the sequence from any prior incarnation. Malformed
        // lines don't block recovery — max(seq) over what parses is
        // correct as long as appends were in seq order, which the
        // single-writer lock guarantees.
        let next_seq = match Self::replay(path) {
            Ok((records, _malformed)) => records
                .iter()
                .map(|r| r.seq)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 1,
            Err(e) => return Err(e),
        };
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let journal = Self {
            daemon_id: daemon_id.to_string(),
            file: Mutex::new(file),
            next_seq: AtomicU64::new(next_seq),
            fsync,
        };
        journal.append(JournalPayload::JournalOpened {
            mu_version: crate::version().to_string(),
            pid: std::process::id(),
        })?;
        Ok(journal)
    }

    pub fn daemon_id(&self) -> &str {
        &self.daemon_id
    }

    /// Append a record. Returns the assigned seq. Writes one JSONL
    /// line and — under [`FsyncPolicy::Always`] — `sync_data()`s
    /// BEFORE returning, so a returned seq means the record is
    /// durable. Errors propagate; callers fail closed (INV-2).
    pub fn append(&self, payload: JournalPayload) -> io::Result<u64> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("command journal mutex poisoned"))?;
        // Seq is assigned under the file lock so seq order == file
        // order (INV-3): no interleaving writer can claim a later seq
        // and land earlier in the file.
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let record = JournalRecord {
            seq,
            daemon_id: self.daemon_id.clone(),
            timestamp_unix_ms: now_unix_ms(),
            payload,
        };
        let line = serde_json::to_string(&record).map_err(io::Error::other)?;
        writeln!(file, "{line}")?;
        if self.fsync == FsyncPolicy::Always {
            file.sync_data()?;
        }
        Ok(seq)
    }

    /// Read every parseable record off a journal file. Malformed
    /// lines — including records whose `kind` this build doesn't know
    /// (forward compat across mixed-version fleets) — are skipped with
    /// a counter returned, same contract as
    /// [`SessionEventLog::from_jsonl`](crate::event_log::SessionEventLog::from_jsonl).
    pub fn replay(path: &Path) -> io::Result<(Vec<JournalRecord>, usize)> {
        use std::io::BufRead;

        let file = File::open(path)?;
        let reader = io::BufReader::new(file);
        let mut records: Vec<JournalRecord> = Vec::new();
        let mut malformed: usize = 0;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    malformed = malformed.saturating_add(1);
                    continue;
                }
            };
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(&line) {
                Ok(rec) => records.push(rec),
                Err(_) => malformed = malformed.saturating_add(1),
            }
        }
        Ok((records, malformed))
    }
}

/// Spec mu-046 INV-4: collect the seqs of `CommandReceived` records
/// that never gained a receipt (`CommandSucceeded`/`Failed`/
/// `Rejected`). An orphaned `CommandReceived` IS the legible crash
/// marker — recovery may tombstone these; it never erases them, and
/// a tombstone does not make a command any less orphaned. Free
/// function over a borrowed slice, mirroring
/// [`tombstoned_ids`](crate::event_log::tombstoned_ids).
pub fn orphaned_command_seqs(records: &[JournalRecord]) -> BTreeSet<u64> {
    let mut received: BTreeSet<u64> = BTreeSet::new();
    let mut receipted: BTreeSet<u64> = BTreeSet::new();
    for rec in records {
        match &rec.payload {
            JournalPayload::CommandReceived { .. } => {
                received.insert(rec.seq);
            }
            JournalPayload::CommandSucceeded { command_seq, .. }
            | JournalPayload::CommandFailed { command_seq, .. }
            | JournalPayload::CommandRejected { command_seq, .. } => {
                receipted.insert(*command_seq);
            }
            JournalPayload::JournalOpened { .. }
            | JournalPayload::ConfigLoaded { .. }
            | JournalPayload::Tombstone { .. } => {}
        }
    }
    received.difference(&receipted).copied().collect()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_received(method: &str) -> JournalPayload {
        JournalPayload::CommandReceived {
            request_id: json!(1),
            method: method.into(),
            params: json!({"session_id": "s1"}),
            session_id: Some("s1".into()),
            auth: AuthSnapshot::Authenticated,
            origin: Origin {
                transport: "stdio".into(),
                connection_id: None,
            },
        }
    }

    fn sample_echo(method: &str) -> CommandEcho {
        CommandEcho {
            request_id: json!(1),
            method: method.into(),
            params: json!({"session_id": "s1"}),
        }
    }

    #[test]
    fn append_replay_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d1.jsonl");
        let journal = CommandJournal::open(&path, "d1", FsyncPolicy::Always).expect("open");
        let s1 = journal
            .append(sample_received("session.ask"))
            .expect("append");
        let s2 = journal
            .append(JournalPayload::CommandSucceeded {
                command_seq: s1,
                command: sample_echo("session.ask"),
                result: json!({"accepted": true}),
                elapsed_ms: 12,
            })
            .expect("append receipt");
        // Seq 1 is the JournalOpened written by open().
        assert_eq!((s1, s2), (2, 3));

        let (records, malformed) = CommandJournal::replay(&path).expect("replay");
        assert_eq!(malformed, 0);
        assert_eq!(records.len(), 3);
        assert!(matches!(
            records[0].payload,
            JournalPayload::JournalOpened { .. }
        ));
        assert_eq!(records[1].seq, 2);
        assert_eq!(records[1].daemon_id, "d1");
        assert_eq!(records[1].payload, sample_received("session.ask"));
        assert!(matches!(
            records[2].payload,
            JournalPayload::CommandSucceeded { command_seq: 2, .. }
        ));
    }

    #[test]
    fn reopen_recovers_next_seq_and_marks_the_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d1.jsonl");
        {
            let journal = CommandJournal::open(&path, "d1", FsyncPolicy::Always).expect("open");
            journal
                .append(sample_received("session.ask"))
                .expect("append");
        }
        // Second incarnation: seqs continue, and the reopen itself is
        // a legible JournalOpened boundary in the stream.
        let journal = CommandJournal::open(&path, "d1", FsyncPolicy::Always).expect("reopen");
        let s = journal
            .append(sample_received("session.list"))
            .expect("append");
        assert_eq!(s, 4, "1=opened, 2=received, 3=reopened, 4=this");

        let (records, malformed) = CommandJournal::replay(&path).expect("replay");
        assert_eq!(malformed, 0);
        let seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        assert!(matches!(
            records[2].payload,
            JournalPayload::JournalOpened { .. }
        ));
    }

    #[test]
    fn replay_skips_and_counts_malformed_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d1.jsonl");
        let journal = CommandJournal::open(&path, "d1", FsyncPolicy::Never).expect("open");
        journal
            .append(sample_received("session.ask"))
            .expect("append");
        // Corrupt the tail the way a crash mid-write would.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open raw");
            writeln!(f, "{{ this is not json").expect("write garbage");
        }
        journal
            .append(sample_received("session.list"))
            .expect("append");

        let (records, malformed) = CommandJournal::replay(&path).expect("replay");
        assert_eq!(malformed, 1);
        assert_eq!(records.len(), 3);
        // Recovery still continues the sequence past the scar.
        assert_eq!(records.last().expect("tail").seq, 3);
    }

    /// Forward compat across mixed-version fleets: a record whose
    /// `kind` this build doesn't know counts as malformed; everything
    /// else parses (spec mu-046 migration note).
    #[test]
    fn replay_counts_unknown_kind_as_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d1.jsonl");
        let journal = CommandJournal::open(&path, "d1", FsyncPolicy::Never).expect("open");
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open raw");
            writeln!(
                f,
                r#"{{"seq":99,"daemon_id":"d1","timestamp_unix_ms":0,"payload":{{"kind":"from_the_future","novelty":true}}}}"#
            )
            .expect("write future record");
        }
        journal
            .append(sample_received("session.ask"))
            .expect("append");

        let (records, malformed) = CommandJournal::replay(&path).expect("replay");
        assert_eq!(malformed, 1, "unknown kind counts as malformed");
        assert_eq!(records.len(), 2, "the known-kind records still parse");
    }

    #[test]
    fn orphan_detection_received_without_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d1.jsonl");
        let journal = CommandJournal::open(&path, "d1", FsyncPolicy::Never).expect("open");
        let answered = journal
            .append(sample_received("session.ask"))
            .expect("append");
        let orphan = journal
            .append(sample_received("session.ask"))
            .expect("append");
        let rejected = journal
            .append(sample_received("daemon.stop"))
            .expect("append");
        journal
            .append(JournalPayload::CommandSucceeded {
                command_seq: answered,
                command: sample_echo("session.ask"),
                result: json!({"accepted": true}),
                elapsed_ms: 5,
            })
            .expect("append receipt");
        // A rejection is a receipt too — it clears orphan status.
        journal
            .append(JournalPayload::CommandRejected {
                command_seq: rejected,
                command: sample_echo("daemon.stop"),
                code: -32600,
                message: "nope".into(),
                stage: RejectStage::AuthGate,
            })
            .expect("append rejection");
        // A tombstone over the orphan marks it; it does NOT receipt it.
        journal
            .append(JournalPayload::Tombstone {
                target_seq: orphan,
                reason: "daemon died mid-command".into(),
            })
            .expect("append tombstone");

        let (records, _) = CommandJournal::replay(&path).expect("replay");
        let orphans = orphaned_command_seqs(&records);
        assert_eq!(orphans.into_iter().collect::<Vec<_>>(), vec![orphan]);
    }

    /// Always vs Never produce byte-identical files (the policy is
    /// about durability, not encoding), and the Always path executes
    /// `sync_data` without error.
    #[test]
    fn fsync_policies_write_identical_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let always_path = dir.path().join("always.jsonl");
        let never_path = dir.path().join("never.jsonl");
        let always = CommandJournal::open(&always_path, "d1", FsyncPolicy::Always).expect("open");
        let never = CommandJournal::open(&never_path, "d1", FsyncPolicy::Never).expect("open");
        always
            .append(sample_received("session.ask"))
            .expect("fsync append");
        never
            .append(sample_received("session.ask"))
            .expect("plain append");

        // Strip the only nondeterministic field (timestamps) and
        // compare the rest byte-for-byte, line by line.
        let normalize = |path: &Path| -> Vec<String> {
            std::fs::read_to_string(path)
                .expect("read")
                .lines()
                .map(|l| {
                    let mut v: Value = serde_json::from_str(l).expect("line parses");
                    v.as_object_mut()
                        .expect("object")
                        .insert("timestamp_unix_ms".into(), json!(0));
                    serde_json::to_string(&v).expect("re-encode")
                })
                .collect()
        };
        // pid/version match (same process); seqs match; payloads match.
        assert_eq!(normalize(&always_path), normalize(&never_path));
    }

    #[test]
    fn payload_wire_tags_are_snake_case() -> Result<(), serde_json::Error> {
        let rec = JournalRecord {
            seq: 1,
            daemon_id: "d1".into(),
            timestamp_unix_ms: 0,
            payload: JournalPayload::CommandRejected {
                command_seq: 1,
                command: CommandEcho {
                    request_id: json!("r1"),
                    method: "session.ask".into(),
                    params: json!({}),
                },
                code: -32003,
                message: "journal unavailable".into(),
                stage: RejectStage::AuthGate,
            },
        };
        let v = serde_json::to_value(&rec)?;
        assert_eq!(v["payload"]["kind"], "command_rejected");
        assert_eq!(v["payload"]["stage"], "auth_gate");
        let decoded: JournalRecord = serde_json::from_value(v)?;
        assert_eq!(decoded, rec);

        // AuthSnapshot's wire form, pinned: it is shared with the
        // session log's command variants.
        assert_eq!(
            serde_json::to_value(AuthSnapshot::Unauthenticated)?,
            json!("unauthenticated")
        );
        Ok(())
    }
}
