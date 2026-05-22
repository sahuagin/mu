//! Sink — SQLite schema for the analytics projection.
//!
//! `open` creates or attaches to the DB and ensures the schema. `upsert_task`
//! writes one row per task_id (idempotent — re-running compact is safe).

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use mu_core::event_log::TaskExitReason;
use mu_core::forensics::{Classification, Confidence, Outcome};

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS tasks (
    task_id              TEXT PRIMARY KEY,
    session_id           TEXT NOT NULL,
    parent_task_id       TEXT,
    provider             TEXT NOT NULL,
    model                TEXT NOT NULL,
    model_version        TEXT,
    started_at_unix_ms   INTEGER,
    ended_at_unix_ms     INTEGER NOT NULL,
    wall_clock_ms        INTEGER,
    prompt_tokens        INTEGER,
    completion_tokens    INTEGER,
    cache_read_tokens    INTEGER,
    cache_write_tokens   INTEGER,
    exit_reason          TEXT NOT NULL,
    outcome_class        TEXT NOT NULL,
    outcome_confidence   TEXT NOT NULL,
    rationale            TEXT,
    tool_call_count      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_tasks_provider_model ON tasks(provider, model);
CREATE INDEX IF NOT EXISTS idx_tasks_ended_at ON tasks(ended_at_unix_ms);
CREATE INDEX IF NOT EXISTS idx_tasks_outcome ON tasks(outcome_class);
";

/// Lightweight additive migrations applied to existing DBs. Each
/// entry is checked against the current schema; if the column already
/// exists, the ALTER is skipped. Keeps existing telemetry.sqlite files
/// working without forcing a delete-and-recompact dance.
const MIGRATIONS: &[(&str, &str)] = &[
    // (column_name, alter_sql)
    (
        "tool_call_count",
        "ALTER TABLE tasks ADD COLUMN tool_call_count INTEGER NOT NULL DEFAULT 0",
    ),
];

/// Open the sink at `path`, creating the file (and parent dirs) + schema if
/// needed. Subsequent opens of an existing DB just attach + run any
/// pending additive migrations from MIGRATIONS.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating analytics dir {}", parent.display()))?;
    }
    let conn =
        Connection::open(path).with_context(|| format!("opening sink at {}", path.display()))?;
    conn.execute_batch(SCHEMA).context("ensuring schema")?;
    for (col, sql) in MIGRATIONS {
        if !column_exists(&conn, "tasks", col)? {
            conn.execute_batch(sql)
                .with_context(|| format!("migration: adding column {col}"))?;
        }
    }
    Ok(conn)
}

/// Check whether `column` exists on `table`. Uses sqlite's
/// `pragma_table_info` virtual table — present since 3.16, well below
/// any version we'd realistically encounter.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |r| r.get(0),
        )
        .context("pragma_table_info")?;
    Ok(n > 0)
}

/// One denormalized row per task. Mirrors the `tasks` table column order
/// for clarity at the UPSERT site.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub task_id: String,
    pub session_id: String,
    pub parent_task_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub model_version: Option<String>,
    pub started_at_unix_ms: Option<u64>,
    pub ended_at_unix_ms: u64,
    pub wall_clock_ms: Option<u64>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub exit_reason: TaskExitReason,
    pub classification: Classification,
    /// Number of tools the agent actually called during this task. 0
    /// for pure-chat sessions (mu-solo / mu-tui responding without
    /// invoking any tool). Lets the hallu metric exclude chat
    /// sessions from its denominator — the classifier's
    /// `narrative_no_action` outcome fires on any Done+no-commit and
    /// can't tell intent on its own.
    pub tool_call_count: u32,
}

/// UPSERT a task row by task_id. Re-running compact over the same logs
/// produces no duplicates.
pub fn upsert_task(conn: &Connection, row: &TaskRow) -> Result<()> {
    conn.execute(
        "INSERT INTO tasks (
            task_id, session_id, parent_task_id, provider, model, model_version,
            started_at_unix_ms, ended_at_unix_ms, wall_clock_ms,
            prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens,
            exit_reason, outcome_class, outcome_confidence, rationale,
            tool_call_count
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
        ON CONFLICT(task_id) DO UPDATE SET
            session_id           = excluded.session_id,
            parent_task_id       = excluded.parent_task_id,
            provider             = excluded.provider,
            model                = excluded.model,
            model_version        = excluded.model_version,
            started_at_unix_ms   = excluded.started_at_unix_ms,
            ended_at_unix_ms     = excluded.ended_at_unix_ms,
            wall_clock_ms        = excluded.wall_clock_ms,
            prompt_tokens        = excluded.prompt_tokens,
            completion_tokens    = excluded.completion_tokens,
            cache_read_tokens    = excluded.cache_read_tokens,
            cache_write_tokens   = excluded.cache_write_tokens,
            exit_reason          = excluded.exit_reason,
            outcome_class        = excluded.outcome_class,
            outcome_confidence   = excluded.outcome_confidence,
            rationale            = excluded.rationale,
            tool_call_count      = excluded.tool_call_count
        ",
        params![
            row.task_id,
            row.session_id,
            row.parent_task_id,
            row.provider,
            row.model,
            row.model_version,
            row.started_at_unix_ms,
            row.ended_at_unix_ms,
            row.wall_clock_ms,
            row.prompt_tokens,
            row.completion_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            exit_reason_str(row.exit_reason),
            outcome_str(row.classification.outcome),
            confidence_str(row.classification.confidence),
            row.classification.rationale,
            row.tool_call_count,
        ],
    )
    .context("upserting task row")?;
    Ok(())
}

/// Convert TaskExitReason to its stable wire string. Matches serde's
/// `rename_all = "snake_case"` so the DB string is also the wire name.
pub fn exit_reason_str(r: TaskExitReason) -> &'static str {
    match r {
        TaskExitReason::Done => "done",
        TaskExitReason::Error => "error",
        TaskExitReason::Cancelled => "cancelled",
        TaskExitReason::BudgetCap => "budget_cap",
        TaskExitReason::Timeout => "timeout",
        TaskExitReason::OperatorStopped => "operator_stopped",
    }
}

/// Convert Outcome to its stable string for storage / display.
pub fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::CleanSuccess => "clean_success",
        Outcome::CosmeticFailure => "cosmetic_failure",
        Outcome::BugInOutput => "bug_in_output",
        Outcome::HollowCommit => "hollow_commit",
        Outcome::LyingState => "lying_state",
        Outcome::NarrativeNoAction => "narrative_no_action",
        Outcome::BudgetHalted => "budget_halted",
        Outcome::ErrorExit => "error_exit",
        Outcome::Timeout => "timeout",
        Outcome::OperatorIntervention => "operator_intervention",
        Outcome::Unclassified => "unclassified",
    }
}

pub fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Definite => "Definite",
        Confidence::Probable => "Probable",
        Confidence::Inferred => "Inferred",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(task_id: &str) -> TaskRow {
        TaskRow {
            task_id: task_id.to_owned(),
            session_id: "session-x".to_owned(),
            parent_task_id: None,
            provider: "openrouter".to_owned(),
            model: "deepseek/deepseek-v4-flash".to_owned(),
            model_version: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: 1778963762000,
            wall_clock_ms: Some(2000),
            prompt_tokens: Some(2400),
            completion_tokens: Some(17),
            cache_read_tokens: None,
            cache_write_tokens: None,
            exit_reason: TaskExitReason::Done,
            classification: Classification {
                outcome: Outcome::NarrativeNoAction,
                confidence: Confidence::Definite,
                rationale: "exit=Done, no commit produced".to_owned(),
            },
            tool_call_count: 0,
        }
    }

    #[test]
    fn open_creates_schema() {
        let tmp = tempfile_path("mu_8ypx_sink_open.sqlite");
        let _ = std::fs::remove_file(&tmp);
        let conn = open(&tmp).expect("open");
        // Schema present?
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n, 0);
        drop(conn);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn upsert_is_idempotent() {
        let tmp = tempfile_path("mu_8ypx_sink_upsert.sqlite");
        let _ = std::fs::remove_file(&tmp);
        let conn = open(&tmp).expect("open");

        upsert_task(&conn, &sample_row("task-1")).unwrap();
        upsert_task(&conn, &sample_row("task-1")).unwrap();
        upsert_task(&conn, &sample_row("task-2")).unwrap();

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);

        drop(conn);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn upsert_updates_existing_row() {
        let tmp = tempfile_path("mu_8ypx_sink_update.sqlite");
        let _ = std::fs::remove_file(&tmp);
        let conn = open(&tmp).expect("open");

        let mut row = sample_row("task-3");
        row.prompt_tokens = Some(100);
        upsert_task(&conn, &row).unwrap();

        row.prompt_tokens = Some(999);
        row.classification.rationale = "updated rationale".to_owned();
        upsert_task(&conn, &row).unwrap();

        let (tokens, rationale): (i64, String) = conn
            .query_row(
                "SELECT prompt_tokens, rationale FROM tasks WHERE task_id = 'task-3'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(tokens, 999);
        assert_eq!(rationale, "updated rationale");

        drop(conn);
        let _ = std::fs::remove_file(&tmp);
    }

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(name)
    }
}
