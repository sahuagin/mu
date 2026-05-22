//! Query — preset queries over the sink, with text-tabular formatting.
//!
//! v1 surface: `summary` (totals by exit_reason/provider+model/outcome)
//! and `rate --metric hallucination` (hallu rate by provider+model).
//! Future metrics and `--by` fields land here as new functions.

use std::fmt::Write;

use anyhow::Result;
use rusqlite::Connection;

/// Counted breakdown — generic shape used by summary's three sub-tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountRow {
    pub label: String,
    pub count: i64,
}

/// `summary` aggregates: total + the three breakdowns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    pub total: i64,
    pub by_exit_reason: Vec<CountRow>,
    pub by_provider_model: Vec<CountRow>,
    pub by_outcome: Vec<CountRow>,
}

/// Pull the summary; `since_unix_ms` filters `ended_at_unix_ms >= since`.
pub fn summary(conn: &Connection, since_unix_ms: Option<u64>) -> Result<Summary> {
    let since = since_unix_ms.unwrap_or(0) as i64;

    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE ended_at_unix_ms >= ?1",
        [since],
        |r| r.get(0),
    )?;

    let by_exit_reason = group_count(
        conn,
        "exit_reason",
        "exit_reason",
        Some("ended_at_unix_ms >= ?1"),
        since,
    )?;
    let by_provider_model = group_count_two(
        conn,
        "provider, model",
        Some("ended_at_unix_ms >= ?1"),
        since,
    )?;
    let by_outcome = group_count(
        conn,
        "outcome_class || ' (' || outcome_confidence || ')'",
        "outcome_class, outcome_confidence",
        Some("ended_at_unix_ms >= ?1"),
        since,
    )?;

    Ok(Summary {
        total,
        by_exit_reason,
        by_provider_model,
        by_outcome,
    })
}

/// Group by a single column expression (`select_expr`) using
/// `group_by_expr` for the GROUP BY clause. Optional WHERE clause.
fn group_count(
    conn: &Connection,
    select_expr: &str,
    group_by_expr: &str,
    where_clause: Option<&str>,
    since: i64,
) -> Result<Vec<CountRow>> {
    let where_part = where_clause
        .map(|w| format!(" WHERE {w}"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT {select_expr} AS label, COUNT(*) AS n FROM tasks{where_part} \
         GROUP BY {group_by_expr} ORDER BY n DESC, label ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([since], |r| {
            Ok(CountRow {
                label: r.get(0)?,
                count: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Specialized for provider+model — formats as "provider/model".
fn group_count_two(
    conn: &Connection,
    cols: &str,
    where_clause: Option<&str>,
    since: i64,
) -> Result<Vec<CountRow>> {
    let where_part = where_clause
        .map(|w| format!(" WHERE {w}"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT provider, model, COUNT(*) AS n FROM tasks{where_part} \
         GROUP BY {cols} ORDER BY n DESC, provider ASC, model ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([since], |r| {
            let provider: String = r.get(0)?;
            let model: String = r.get(1)?;
            let count: i64 = r.get(2)?;
            Ok(CountRow {
                label: format!("{provider}/{model}"),
                count,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn format_summary(s: &Summary) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "total tasks: {}\n", s.total);

    let _ = writeln!(out, "by exit_reason:");
    let label_width = s
        .by_exit_reason
        .iter()
        .map(|r| r.label.len())
        .max()
        .unwrap_or(0);
    for r in &s.by_exit_reason {
        let _ = writeln!(
            out,
            "  {:width$} {:>5}",
            r.label,
            r.count,
            width = label_width
        );
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "by provider/model:");
    let label_width = s
        .by_provider_model
        .iter()
        .map(|r| r.label.len())
        .max()
        .unwrap_or(0);
    for r in &s.by_provider_model {
        let _ = writeln!(
            out,
            "  {:width$} {:>5}",
            r.label,
            r.count,
            width = label_width
        );
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "by outcome:");
    let label_width = s
        .by_outcome
        .iter()
        .map(|r| r.label.len())
        .max()
        .unwrap_or(0);
    for r in &s.by_outcome {
        let _ = writeln!(
            out,
            "  {:width$} {:>5}",
            r.label,
            r.count,
            width = label_width
        );
    }

    out
}

/// `rate --metric hallucination` row: hallu count + done count, grouped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateRow {
    pub group_label: String,
    pub numerator: i64,
    pub denominator: i64,
}

impl RateRow {
    pub fn percent(&self) -> f64 {
        if self.denominator == 0 {
            0.0
        } else {
            (self.numerator as f64 / self.denominator as f64) * 100.0
        }
    }
}

/// `rate --metric hallucination --by provider,model`. The hallucination
/// definition (per spec mu-042): `hollow_commit + lying_state +
/// narrative_no_action` divided by total `Done` tasks.
///
/// Filters to `tool_call_count > 0` to exclude pure-chat sessions
/// from the denominator. The classifier's `narrative_no_action`
/// outcome fires on every `Done + no-commit` task, which is the
/// right call for autonomous coding agents but classifies all chat
/// sessions (mu-solo, mu-tui) as hallucinations by construction.
/// Filtering to tasks that called at least one tool is a structural
/// proxy for "intended an action," and gets the rate honest without
/// requiring an upstream `session_kind` field on every task. If a
/// chat session happens to call `bash ls` it'll be counted — false
/// positive, but small in the data.
pub fn rate_hallucination(conn: &Connection, since_unix_ms: Option<u64>) -> Result<Vec<RateRow>> {
    let since = since_unix_ms.unwrap_or(0) as i64;
    let sql = "\
        SELECT provider, model,\n        \
               SUM(CASE WHEN outcome_class IN ('hollow_commit','lying_state','narrative_no_action') THEN 1 ELSE 0 END) AS hallu,\n        \
               SUM(CASE WHEN exit_reason = 'done' THEN 1 ELSE 0 END) AS done_count\n        \
          FROM tasks\n         WHERE ended_at_unix_ms >= ?1\n           AND tool_call_count > 0\n      GROUP BY provider, model\n      ORDER BY hallu DESC, provider ASC, model ASC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([since], |r| {
            let provider: String = r.get(0)?;
            let model: String = r.get(1)?;
            let hallu: i64 = r.get(2)?;
            let done: i64 = r.get(3)?;
            Ok(RateRow {
                group_label: format!("{provider}/{model}"),
                numerator: hallu,
                denominator: done,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn format_rate(rows: &[RateRow], metric_label: &str) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        let _ = writeln!(out, "no tasks in window — {metric_label} rate unavailable");
        return out;
    }

    let label_width = rows
        .iter()
        .map(|r| r.group_label.len())
        .max()
        .unwrap_or(0)
        .max("group".len());

    let _ = writeln!(
        out,
        "{:width$}  rate     ({metric_label} / done)",
        "group",
        width = label_width
    );
    for r in rows {
        let _ = writeln!(
            out,
            "{:width$}  {:>5.1}%   {:>4} / {:<4}",
            r.group_label,
            r.percent(),
            r.numerator,
            r.denominator,
            width = label_width
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::sink::{open, upsert_task, TaskRow};
    use mu_core::event_log::TaskExitReason;
    use mu_core::forensics::{Classification, Confidence, Outcome};

    fn fixture(
        task_id: &str,
        provider: &str,
        model: &str,
        exit: TaskExitReason,
        outcome: Outcome,
        ended_at: u64,
    ) -> TaskRow {
        // Default fixture is a tool-using task (tool_call_count = 1)
        // so it's included in the hallu rate denominator. Tests that
        // want to exercise the "pure chat" filter mutate the row
        // after construction to set tool_call_count = 0.
        TaskRow {
            task_id: task_id.to_owned(),
            session_id: "s".to_owned(),
            parent_task_id: None,
            provider: provider.to_owned(),
            model: model.to_owned(),
            model_version: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: ended_at,
            wall_clock_ms: Some(1000),
            prompt_tokens: Some(100),
            completion_tokens: Some(10),
            cache_read_tokens: None,
            cache_write_tokens: None,
            exit_reason: exit,
            classification: Classification {
                outcome,
                confidence: Confidence::Definite,
                rationale: "fixture".to_owned(),
            },
            tool_call_count: 1,
        }
    }

    fn seeded_conn(name: &str) -> (Connection, std::path::PathBuf) {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        let conn = open(&path).unwrap();

        // 5 openrouter+deepseek tasks: 3 narrative_no_action (Done),
        // 1 error_exit, 1 operator_intervention.
        upsert_task(
            &conn,
            &fixture(
                "t1",
                "openrouter",
                "ds",
                TaskExitReason::Done,
                Outcome::NarrativeNoAction,
                1000,
            ),
        )
        .unwrap();
        upsert_task(
            &conn,
            &fixture(
                "t2",
                "openrouter",
                "ds",
                TaskExitReason::Done,
                Outcome::NarrativeNoAction,
                1100,
            ),
        )
        .unwrap();
        upsert_task(
            &conn,
            &fixture(
                "t3",
                "openrouter",
                "ds",
                TaskExitReason::Done,
                Outcome::NarrativeNoAction,
                1200,
            ),
        )
        .unwrap();
        upsert_task(
            &conn,
            &fixture(
                "t4",
                "openrouter",
                "ds",
                TaskExitReason::Error,
                Outcome::ErrorExit,
                1300,
            ),
        )
        .unwrap();
        upsert_task(
            &conn,
            &fixture(
                "t5",
                "openrouter",
                "ds",
                TaskExitReason::Cancelled,
                Outcome::OperatorIntervention,
                1400,
            ),
        )
        .unwrap();
        // 2 anthropic+haiku tasks: 1 clean_success (Done), 1 bug_in_output (Done).
        upsert_task(
            &conn,
            &fixture(
                "t6",
                "anthropic_api",
                "claude-haiku-4-5",
                TaskExitReason::Done,
                Outcome::CleanSuccess,
                1500,
            ),
        )
        .unwrap();
        upsert_task(
            &conn,
            &fixture(
                "t7",
                "anthropic_api",
                "claude-haiku-4-5",
                TaskExitReason::Done,
                Outcome::BugInOutput,
                1600,
            ),
        )
        .unwrap();

        (conn, path)
    }

    #[test]
    fn summary_counts_breakdowns() {
        let (conn, path) = seeded_conn("mu_8ypx_q_summary.sqlite");
        let s = summary(&conn, None).unwrap();
        assert_eq!(s.total, 7);

        let exit_counts: std::collections::HashMap<_, _> = s
            .by_exit_reason
            .iter()
            .map(|r| (r.label.as_str(), r.count))
            .collect();
        assert_eq!(exit_counts["done"], 5);
        assert_eq!(exit_counts["error"], 1);
        assert_eq!(exit_counts["cancelled"], 1);

        let pm_counts: std::collections::HashMap<_, _> = s
            .by_provider_model
            .iter()
            .map(|r| (r.label.as_str(), r.count))
            .collect();
        assert_eq!(pm_counts["openrouter/ds"], 5);
        assert_eq!(pm_counts["anthropic_api/claude-haiku-4-5"], 2);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rate_hallucination_groups_by_provider_model() {
        let (conn, path) = seeded_conn("mu_8ypx_q_rate.sqlite");
        let rows = rate_hallucination(&conn, None).unwrap();
        let by_label: std::collections::HashMap<_, _> = rows
            .iter()
            .map(|r| (r.group_label.as_str(), (r.numerator, r.denominator)))
            .collect();
        // openrouter/ds: 3 hallu (narrative_no_action) out of 3 Done.
        assert_eq!(by_label["openrouter/ds"], (3, 3));
        // anthropic_api: 0 hallu out of 2 Done.
        assert_eq!(by_label["anthropic_api/claude-haiku-4-5"], (0, 2));

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rate_hallucination_excludes_zero_tool_call_chat_sessions() {
        // Mirror the real-world blind spot: a chat session with no
        // tool calls hits narrative_no_action by definition (it
        // produced no commit). Before the filter it was 100% hallu;
        // now it's excluded from the rate denominator entirely.
        let path = std::env::temp_dir().join("mu_8ypx_q_chat_filter.sqlite");
        let _ = std::fs::remove_file(&path);
        let conn = open(&path).unwrap();

        // One tool-calling task that legitimately failed to commit.
        upsert_task(
            &conn,
            &fixture(
                "tool-1",
                "openrouter",
                "ds",
                TaskExitReason::Done,
                Outcome::NarrativeNoAction,
                2000,
            ),
        )
        .unwrap();
        // Three pure-chat tasks (same provider+model, no tool calls).
        for (i, id) in ["chat-1", "chat-2", "chat-3"].iter().enumerate() {
            let mut row = fixture(
                id,
                "openrouter",
                "ds",
                TaskExitReason::Done,
                Outcome::NarrativeNoAction,
                2100 + i as u64,
            );
            row.tool_call_count = 0;
            upsert_task(&conn, &row).unwrap();
        }

        let rows = rate_hallucination(&conn, None).unwrap();
        let by_label: std::collections::HashMap<_, _> = rows
            .iter()
            .map(|r| (r.group_label.as_str(), (r.numerator, r.denominator)))
            .collect();
        // Only the one tool-calling task is counted. The chat rows
        // are excluded — not (1, 4).
        assert_eq!(by_label["openrouter/ds"], (1, 1));

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rate_percent_handles_zero_denominator() {
        let r = RateRow {
            group_label: "x".to_owned(),
            numerator: 0,
            denominator: 0,
        };
        assert_eq!(r.percent(), 0.0);
    }

    #[test]
    fn since_filter_drops_old_rows() {
        let (conn, path) = seeded_conn("mu_8ypx_q_since.sqlite");
        // Cut at ended_at >= 1400 → only t5 (cancelled), t6 (Done/clean), t7 (Done/bug).
        let s = summary(&conn, Some(1400)).unwrap();
        assert_eq!(s.total, 3);

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
