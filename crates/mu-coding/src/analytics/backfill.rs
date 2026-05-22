//! Backfill — documentary historical task entries (mu-mk9l / spec mu-043).
//!
//! Unlike [`super::compact`], this path does NOT call `classify_task` —
//! callers specify `outcome_class` and `outcome_confidence` directly.
//! Use case: historical datasets (overnights, incidents) where the
//! operator knows ground truth from PR review and recall, not from
//! observable evidence the classifier could re-derive.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde::Deserialize;

use mu_core::event_log::TaskExitReason;
use mu_core::forensics::{Classification, Confidence, Outcome};

use super::sink::{upsert_task, TaskRow};

/// Embedded preset: 2026-05-16 overnight worker run. Five PRs landed
/// (#49–#53; #52 closed unmerged). See bead `mu-mk9l` for the
/// per-attempt outcome rationale.
pub const PRESET_OVERNIGHT_2026_05_16: &str = include_str!("fixtures/overnight-2026-05-16.toml");

/// Outer TOML shape — collection of entries with an optional description.
#[derive(Debug, Clone, Deserialize)]
pub struct BackfillFile {
    #[serde(default)]
    pub description: Option<String>,
    pub tasks: Vec<BackfillTask>,
}

/// One historical task entry.
#[derive(Debug, Clone, Deserialize)]
pub struct BackfillTask {
    pub task_id: String,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub exit_reason: String,
    pub outcome_class: String,
    #[serde(default = "default_confidence")]
    pub outcome_confidence: String,
    #[serde(default)]
    pub rationale: Option<String>,

    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub model_version: Option<String>,
    #[serde(default)]
    pub started_at_unix_ms: Option<u64>,
    pub ended_at_unix_ms: u64,
    #[serde(default)]
    pub wall_clock_ms: Option<u64>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
}

fn default_confidence() -> String {
    "definite".to_owned()
}

/// Summary of a backfill apply — useful for the CLI to surface.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackfillSummary {
    pub tasks_upserted: usize,
}

/// Load a backfill TOML from disk.
pub fn load_file(path: &Path) -> Result<BackfillFile> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading backfill file {}", path.display()))?;
    parse_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Parse a TOML string into a BackfillFile. Pure for testability.
pub fn parse_str(s: &str) -> Result<BackfillFile> {
    toml::from_str::<BackfillFile>(s).map_err(|e| anyhow!("TOML parse error: {e}"))
}

/// Apply every task in `file` to the sink via upsert. Returns count.
pub fn apply(conn: &Connection, file: &BackfillFile) -> Result<BackfillSummary> {
    let mut summary = BackfillSummary::default();
    for (i, t) in file.tasks.iter().enumerate() {
        let row = task_to_row(t).with_context(|| format!("task #{} (task_id={})", i, t.task_id))?;
        upsert_task(conn, &row)?;
        summary.tasks_upserted += 1;
    }
    Ok(summary)
}

/// Convert a parsed BackfillTask into a TaskRow. Fails on unknown
/// `exit_reason`, `outcome_class`, or `outcome_confidence` — never
/// silently coerce, because that would create misclassified
/// documentary entries.
pub fn task_to_row(t: &BackfillTask) -> Result<TaskRow> {
    Ok(TaskRow {
        task_id: t.task_id.clone(),
        session_id: t.session_id.clone(),
        parent_task_id: t.parent_task_id.clone(),
        provider: t.provider.clone(),
        model: t.model.clone(),
        model_version: t.model_version.clone(),
        started_at_unix_ms: t.started_at_unix_ms,
        ended_at_unix_ms: t.ended_at_unix_ms,
        wall_clock_ms: t.wall_clock_ms,
        prompt_tokens: t.prompt_tokens,
        completion_tokens: t.completion_tokens,
        cache_read_tokens: t.cache_read_tokens,
        cache_write_tokens: t.cache_write_tokens,
        exit_reason: parse_exit_reason(&t.exit_reason)?,
        classification: Classification {
            outcome: parse_outcome(&t.outcome_class)?,
            confidence: parse_confidence(&t.outcome_confidence)?,
            rationale: t.rationale.clone().unwrap_or_default(),
        },
        // Backfill is for documentary inserts (pre-classified historic
        // tasks), not live tool-call telemetry. The TOML schema has no
        // tool_call_count field; default to 0. If a future backfill
        // wants to record this, add an Option field on BackfillTask
        // and surface it here.
        tool_call_count: 0,
    })
}

fn parse_exit_reason(s: &str) -> Result<TaskExitReason> {
    Ok(match s {
        "done" => TaskExitReason::Done,
        "error" => TaskExitReason::Error,
        "cancelled" => TaskExitReason::Cancelled,
        "budget_cap" => TaskExitReason::BudgetCap,
        "timeout" => TaskExitReason::Timeout,
        "operator_stopped" => TaskExitReason::OperatorStopped,
        other => {
            return Err(anyhow!(
                "unknown exit_reason '{other}' — expected one of: \
                 done, error, cancelled, budget_cap, timeout, operator_stopped"
            ))
        }
    })
}

fn parse_outcome(s: &str) -> Result<Outcome> {
    Ok(match s {
        "clean_success" => Outcome::CleanSuccess,
        "cosmetic_failure" => Outcome::CosmeticFailure,
        "bug_in_output" => Outcome::BugInOutput,
        "hollow_commit" => Outcome::HollowCommit,
        "lying_state" => Outcome::LyingState,
        "narrative_no_action" => Outcome::NarrativeNoAction,
        "budget_halted" => Outcome::BudgetHalted,
        "error_exit" => Outcome::ErrorExit,
        "timeout" => Outcome::Timeout,
        "operator_intervention" => Outcome::OperatorIntervention,
        "unclassified" => Outcome::Unclassified,
        other => {
            return Err(anyhow!(
                "unknown outcome_class '{other}' — expected one of: \
                 clean_success, cosmetic_failure, bug_in_output, \
                 hollow_commit, lying_state, narrative_no_action, \
                 budget_halted, error_exit, timeout, \
                 operator_intervention, unclassified"
            ))
        }
    })
}

fn parse_confidence(s: &str) -> Result<Confidence> {
    Ok(match s.to_lowercase().as_str() {
        "definite" => Confidence::Definite,
        "probable" => Confidence::Probable,
        "inferred" => Confidence::Inferred,
        other => {
            return Err(anyhow!(
                "unknown outcome_confidence '{other}' — expected: definite | probable | inferred"
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::sink::open;

    fn fresh_db(name: &str) -> (Connection, std::path::PathBuf) {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        let conn = open(&path).unwrap();
        (conn, path)
    }

    #[test]
    fn preset_overnight_parses_and_has_seven_tasks() {
        let file = parse_str(PRESET_OVERNIGHT_2026_05_16).expect("preset parses");
        assert_eq!(
            file.tasks.len(),
            7,
            "expected exactly 7 entries per bead mu-mk9l"
        );
        assert!(file.description.as_ref().is_some_and(|d| !d.is_empty()));
    }

    #[test]
    fn preset_outcome_counts_match_bead() {
        let file = parse_str(PRESET_OVERNIGHT_2026_05_16).unwrap();
        let mut clean = 0;
        let mut bug = 0;
        let mut hollow = 0;
        let mut lying = 0;
        let mut narrative = 0;
        for t in &file.tasks {
            match t.outcome_class.as_str() {
                "clean_success" => clean += 1,
                "bug_in_output" => bug += 1,
                "hollow_commit" => hollow += 1,
                "lying_state" => lying += 1,
                "narrative_no_action" => narrative += 1,
                other => panic!("unexpected outcome_class {other:?} in preset"),
            }
        }
        assert_eq!(clean, 2, "clean_success count");
        assert_eq!(bug, 2, "bug_in_output count");
        assert_eq!(hollow, 1, "hollow_commit count");
        assert_eq!(lying, 1, "lying_state count");
        assert_eq!(narrative, 1, "narrative_no_action count");
    }

    #[test]
    fn apply_preset_inserts_seven_rows() {
        let (conn, path) = fresh_db("mu_mk9l_apply_preset.sqlite");
        let file = parse_str(PRESET_OVERNIGHT_2026_05_16).unwrap();
        let s = apply(&conn, &file).unwrap();
        assert_eq!(s.tasks_upserted, 7);

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 7);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_is_idempotent() {
        let (conn, path) = fresh_db("mu_mk9l_idempotent.sqlite");
        let file = parse_str(PRESET_OVERNIGHT_2026_05_16).unwrap();
        apply(&conn, &file).unwrap();
        apply(&conn, &file).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 7, "re-applying preset should not duplicate");
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_outcome_class_errors() {
        let bad = r#"
[[tasks]]
task_id = "x"
session_id = "y"
provider = "p"
model = "m"
exit_reason = "done"
outcome_class = "totally_made_up"
ended_at_unix_ms = 1
"#;
        let file = parse_str(bad).unwrap();
        let (conn, path) = fresh_db("mu_mk9l_bad_outcome.sqlite");
        let err = apply(&conn, &file).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("totally_made_up"),
            "error should name the bad value: {msg}"
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_exit_reason_errors() {
        let bad = r#"
[[tasks]]
task_id = "x"
session_id = "y"
provider = "p"
model = "m"
exit_reason = "fancy_new_reason"
outcome_class = "clean_success"
ended_at_unix_ms = 1
"#;
        let file = parse_str(bad).unwrap();
        let (conn, path) = fresh_db("mu_mk9l_bad_exit.sqlite");
        let err = apply(&conn, &file).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fancy_new_reason"),
            "error should name the bad value: {msg}"
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn confidence_defaults_to_definite_when_omitted() {
        let toml_str = r#"
[[tasks]]
task_id = "x"
session_id = "y"
provider = "p"
model = "m"
exit_reason = "done"
outcome_class = "clean_success"
ended_at_unix_ms = 1
"#;
        let file = parse_str(toml_str).unwrap();
        assert_eq!(file.tasks[0].outcome_confidence, "definite");
    }
}
