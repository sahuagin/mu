//! Forensics — post-completion outcome classification (mu-8alb / spec mu-041).
//!
//! Pure classifier. No I/O, no emission. Caller assembles
//! `ClassificationInputs` (telemetry + commit + PR/CI) and calls
//! [`classify_task`] for an `(Outcome, Confidence, rationale)` triple.
//!
//! The classifier is best-effort. Confidence levels (`Definite` /
//! `Probable` / `Inferred`) let downstream analytics weight outcomes
//! appropriately and reserve manual review for low-confidence cases.

use serde::{Deserialize, Serialize};

use crate::event_log::TaskExitReason;

/// Outcome class for a single task. Discriminator definitions live in
/// spec mu-041 §"Discriminator order".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// exit Done + claimed files ⊆ diff + CI green.
    CleanSuccess,
    /// exit Done + CI fmt-only-fail (cheap rework, no semantic bug).
    CosmeticFailure,
    /// exit Done + CI test/clippy fail (real bug, but commit was honest).
    BugInOutput,
    /// exit Done + commit message claims files NOT in diff (mu-nk3 mode).
    HollowCommit,
    /// exit Done + claimed PR # doesn't exist on origin (mu-2jg mode).
    LyingState,
    /// exit Done + zero file modifications + narrative completion
    /// (codex mu-el3c mode).
    NarrativeNoAction,
    /// `TaskExitReason::BudgetCap` — caller decided this task wasn't worth more.
    BudgetHalted,
    /// `TaskExitReason::Error` — terminal error from provider / loop.
    ErrorExit,
    /// `TaskExitReason::Timeout` — watchdog tripped.
    Timeout,
    /// `TaskExitReason::Cancelled` / `OperatorStopped` — human stop.
    OperatorIntervention,
    /// None of the above discriminators matched. Escape hatch.
    Unclassified,
}

/// Strength of a classification. Lets downstream analytics treat
/// `Inferred` outcomes differently (e.g. surface for manual review)
/// without dropping them from aggregates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Discriminator is unambiguous (e.g. exit_reason == BudgetCap).
    Definite,
    /// Discriminator inferred from heuristics that have known false-
    /// positive shapes (claim-parser regex, etc.).
    Probable,
    /// Fallback class; no discriminator strongly matched.
    Inferred,
}

/// Result of classifying a single task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub outcome: Outcome,
    pub confidence: Confidence,
    /// One-line human-readable rationale (debug-friendly; not a
    /// stable wire field, may be reworded freely).
    pub rationale: String,
}

/// Inputs to [`classify_task`]. Borrows everything — caller owns the
/// data. Lifetime allows the classifier to be called repeatedly over a
/// rolling set of records without ownership shuffling.
#[derive(Debug, Clone, Copy)]
pub struct ClassificationInputs<'a> {
    pub telemetry: &'a TaskTelemetrySnapshot,
    /// Commit info — None if no commit was produced (narrative_no_action case).
    pub commit: Option<&'a CommitInfo>,
    /// PR + CI state — None if no PR was claimed or PR lookup hasn't run.
    pub pr: Option<&'a PrInfo>,
}

/// Slim view of `EventPayload::TaskTelemetry` fields the classifier
/// actually reads. Caller projects from the full payload (which has
/// many fields the classifier doesn't need) to keep this module
/// independent of the event-log enum surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTelemetrySnapshot {
    pub task_id: String,
    pub session_id: String,
    pub exit_reason: TaskExitReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
    /// Files that actually changed in the commit. Set on disk.
    pub diff_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInfo {
    /// PR number claimed in state.json or commit message.
    pub claimed_number: Option<u32>,
    /// Whether the claimed PR exists on origin. None when not yet checked.
    pub exists_on_origin: Option<bool>,
    /// CI status when present.
    pub ci: Option<CiStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CiStatus {
    pub fmt_pass: bool,
    pub clippy_pass: bool,
    pub test_pass: bool,
}

impl CiStatus {
    pub fn all_pass(&self) -> bool {
        self.fmt_pass && self.clippy_pass && self.test_pass
    }
    pub fn fmt_only_fail(&self) -> bool {
        !self.fmt_pass && self.clippy_pass && self.test_pass
    }
}

/// Extract file-path claims from a commit message body. Returns the
/// distinct file paths matched by the patterns documented in
/// [`matches_any_claim_pattern`] (insertion order).
///
/// Hand-rolled scan — no `regex` crate dep yet in mu-core. If the
/// pattern set grows beyond the maintainable hand-coded cases,
/// switching to `regex` (with cached compilation) is the obvious
/// upgrade.
pub fn extract_claimed_files(message: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for token in message.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
            )
    }) {
        let trimmed = token.trim_end_matches(|c: char| {
            matches!(c, '.' | ',' | ';' | ':' | ')' | ']' | '}' | '!' | '?')
        });
        if trimmed.is_empty() {
            continue;
        }
        if !matches_any_claim_pattern(trimmed) {
            continue;
        }
        if seen.insert(trimmed.to_owned()) {
            found.push(trimmed.to_owned());
        }
    }
    found
}

/// True iff `s` looks like a file path the commit message is claiming
/// to have touched. Patterns recognized (narrow on purpose — spec
/// mu-041 prefers expansion based on real false-positive evidence
/// over speculative broadening):
///
/// - Top-level fixed names: `justfile`, `README.md`, `Cargo.toml`
/// - `crates/<crate>/(src|tests)/<file>.rs`
/// - `specs/<name>.md`
/// - `scripts/<name>.sh`
fn matches_any_claim_pattern(s: &str) -> bool {
    // Top-level files (exact match).
    if matches!(s, "justfile" | "README.md" | "Cargo.toml") {
        return true;
    }
    // crates/<crate>/(src|tests)/<file>.rs
    if s.starts_with("crates/") && s.ends_with(".rs") {
        let rest = &s["crates/".len()..];
        if let Some(slash) = rest.find('/') {
            let after_crate = &rest[slash + 1..];
            if after_crate.starts_with("src/") || after_crate.starts_with("tests/") {
                return true;
            }
        }
    }
    // specs/<name>.md
    if s.starts_with("specs/") && s.ends_with(".md") {
        return true;
    }
    // scripts/<name>.sh
    if s.starts_with("scripts/") && s.ends_with(".sh") {
        return true;
    }
    false
}

/// Classify a single task. See spec mu-041 §"Discriminator order" for
/// the rules this implements. Discriminators tried in order; first
/// match wins. Always returns a Classification — `Outcome::Unclassified`
/// is the escape hatch.
pub fn classify_task(inputs: &ClassificationInputs<'_>) -> Classification {
    // 1. Terminal-state exits — Definite, directly from exit_reason.
    match inputs.telemetry.exit_reason {
        TaskExitReason::BudgetCap => {
            return Classification {
                outcome: Outcome::BudgetHalted,
                confidence: Confidence::Definite,
                rationale: "exit_reason=BudgetCap".to_owned(),
            };
        }
        TaskExitReason::Error => {
            return Classification {
                outcome: Outcome::ErrorExit,
                confidence: Confidence::Definite,
                rationale: "exit_reason=Error".to_owned(),
            };
        }
        TaskExitReason::Timeout => {
            return Classification {
                outcome: Outcome::Timeout,
                confidence: Confidence::Definite,
                rationale: "exit_reason=Timeout".to_owned(),
            };
        }
        TaskExitReason::Cancelled | TaskExitReason::OperatorStopped => {
            return Classification {
                outcome: Outcome::OperatorIntervention,
                confidence: Confidence::Definite,
                rationale: format!("exit_reason={:?}", inputs.telemetry.exit_reason),
            };
        }
        TaskExitReason::Done => {
            // Fall through to commit/PR-aware discriminators.
        }
    }

    // 2. narrative_no_action — Done + no commit, or commit with empty diff.
    match inputs.commit {
        None => {
            return Classification {
                outcome: Outcome::NarrativeNoAction,
                confidence: Confidence::Definite,
                rationale: "exit=Done, no commit produced".to_owned(),
            };
        }
        Some(commit) if commit.diff_files.is_empty() => {
            return Classification {
                outcome: Outcome::NarrativeNoAction,
                confidence: Confidence::Probable,
                rationale: format!("exit=Done, commit {} has empty diff", commit.sha),
            };
        }
        _ => {}
    }

    // 3. lying_state — Done + commit + claimed PR doesn't exist on origin.
    if let Some(pr) = inputs.pr {
        if let (Some(claimed), Some(false)) = (pr.claimed_number, pr.exists_on_origin) {
            return Classification {
                outcome: Outcome::LyingState,
                confidence: Confidence::Definite,
                rationale: format!("claimed PR #{claimed} does not exist on origin"),
            };
        }
    }

    // 4. hollow_commit — Done + commit message claims files not in diff.
    let commit = inputs.commit.expect("checked None branch above");
    let claimed_files = extract_claimed_files(&commit.message);
    if !claimed_files.is_empty() {
        let diff_set: std::collections::HashSet<&str> =
            commit.diff_files.iter().map(String::as_str).collect();
        let missing: Vec<&String> = claimed_files
            .iter()
            .filter(|f| !diff_set.contains(f.as_str()))
            .collect();
        if !missing.is_empty() {
            let preview: Vec<&str> = missing.iter().take(3).map(|s| s.as_str()).collect();
            return Classification {
                outcome: Outcome::HollowCommit,
                confidence: Confidence::Probable,
                rationale: format!(
                    "commit {} claims {} file(s) not in diff: {}{}",
                    commit.sha,
                    missing.len(),
                    preview.join(", "),
                    if missing.len() > 3 { ", ..." } else { "" },
                ),
            };
        }
    }

    // 5. CI-discriminated classes — require pr.ci to be set.
    if let Some(pr) = inputs.pr {
        if let Some(ci) = pr.ci {
            if ci.all_pass() {
                return Classification {
                    outcome: Outcome::CleanSuccess,
                    confidence: Confidence::Definite,
                    rationale: format!("commit {} + CI all-pass", commit.sha),
                };
            }
            if ci.fmt_only_fail() {
                return Classification {
                    outcome: Outcome::CosmeticFailure,
                    confidence: Confidence::Definite,
                    rationale: format!("commit {} + CI fmt-only-fail", commit.sha),
                };
            }
            if !ci.test_pass || !ci.clippy_pass {
                return Classification {
                    outcome: Outcome::BugInOutput,
                    confidence: Confidence::Definite,
                    rationale: format!(
                        "commit {} + CI fail (test_pass={}, clippy_pass={})",
                        commit.sha, ci.test_pass, ci.clippy_pass,
                    ),
                };
            }
        }
    }

    // 6. unclassified — escape hatch.
    Classification {
        outcome: Outcome::Unclassified,
        confidence: Confidence::Inferred,
        rationale: format!(
            "Done exit with commit {} but no other discriminator matched (no PR/CI info)",
            commit.sha,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry(exit: TaskExitReason) -> TaskTelemetrySnapshot {
        TaskTelemetrySnapshot {
            task_id: "task-00000000000000000001".to_owned(),
            session_id: "session-test".to_owned(),
            exit_reason: exit,
        }
    }

    // ─── Terminal-state exits (Definite) ─────────────────────────────────

    #[test]
    fn budget_halted_definite() {
        let t = telemetry(TaskExitReason::BudgetCap);
        let inputs = ClassificationInputs {
            telemetry: &t,
            commit: None,
            pr: None,
        };
        let c = classify_task(&inputs);
        assert_eq!(c.outcome, Outcome::BudgetHalted);
        assert_eq!(c.confidence, Confidence::Definite);
    }

    #[test]
    fn error_exit_definite() {
        let t = telemetry(TaskExitReason::Error);
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: None,
            pr: None,
        });
        assert_eq!(c.outcome, Outcome::ErrorExit);
        assert_eq!(c.confidence, Confidence::Definite);
    }

    #[test]
    fn timeout_definite() {
        let t = telemetry(TaskExitReason::Timeout);
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: None,
            pr: None,
        });
        assert_eq!(c.outcome, Outcome::Timeout);
        assert_eq!(c.confidence, Confidence::Definite);
    }

    #[test]
    fn cancelled_maps_to_operator_intervention() {
        let t = telemetry(TaskExitReason::Cancelled);
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: None,
            pr: None,
        });
        assert_eq!(c.outcome, Outcome::OperatorIntervention);
        assert_eq!(c.confidence, Confidence::Definite);
    }

    #[test]
    fn operator_stopped_maps_to_operator_intervention() {
        let t = telemetry(TaskExitReason::OperatorStopped);
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: None,
            pr: None,
        });
        assert_eq!(c.outcome, Outcome::OperatorIntervention);
    }

    // ─── narrative_no_action (codex mu-el3c mode) ───────────────────────

    #[test]
    fn done_with_no_commit_is_narrative_no_action_definite() {
        let t = telemetry(TaskExitReason::Done);
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: None,
            pr: None,
        });
        assert_eq!(c.outcome, Outcome::NarrativeNoAction);
        assert_eq!(c.confidence, Confidence::Definite);
    }

    #[test]
    fn done_with_empty_diff_is_narrative_no_action_probable() {
        let t = telemetry(TaskExitReason::Done);
        let commit = CommitInfo {
            sha: "abc123".to_owned(),
            message: "did the work".to_owned(),
            diff_files: vec![],
        };
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: Some(&commit),
            pr: None,
        });
        assert_eq!(c.outcome, Outcome::NarrativeNoAction);
        assert_eq!(c.confidence, Confidence::Probable);
    }

    // ─── lying_state (mu-2jg mode) ──────────────────────────────────────

    #[test]
    fn done_with_pr_claim_that_does_not_exist_is_lying_state() {
        let t = telemetry(TaskExitReason::Done);
        let commit = CommitInfo {
            sha: "def456".to_owned(),
            message: "feat(x): mu-foo".to_owned(),
            diff_files: vec!["crates/mu-core/src/x.rs".to_owned()],
        };
        let pr = PrInfo {
            claimed_number: Some(42),
            exists_on_origin: Some(false),
            ci: None,
        };
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: Some(&commit),
            pr: Some(&pr),
        });
        assert_eq!(c.outcome, Outcome::LyingState);
        assert_eq!(c.confidence, Confidence::Definite);
        assert!(c.rationale.contains("42"));
    }

    // ─── hollow_commit (mu-nk3 mode) ────────────────────────────────────

    #[test]
    fn done_with_commit_claiming_unchanged_files_is_hollow() {
        let t = telemetry(TaskExitReason::Done);
        let commit = CommitInfo {
            sha: "ghi789".to_owned(),
            message: "feat: added crates/mu-core/src/new_thing.rs and specs/mu-999-fake.md"
                .to_owned(),
            diff_files: vec!["README.md".to_owned()],
        };
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: Some(&commit),
            pr: None,
        });
        assert_eq!(c.outcome, Outcome::HollowCommit);
        assert_eq!(c.confidence, Confidence::Probable);
        assert!(
            c.rationale.contains("new_thing.rs") || c.rationale.contains("mu-999"),
            "rationale should name the missing files: {}",
            c.rationale
        );
    }

    #[test]
    fn commit_with_claims_all_present_is_not_hollow() {
        let t = telemetry(TaskExitReason::Done);
        let commit = CommitInfo {
            sha: "jkl012".to_owned(),
            message: "feat: edits crates/mu-core/src/x.rs".to_owned(),
            diff_files: vec!["crates/mu-core/src/x.rs".to_owned()],
        };
        // Without PR/CI info, no clean_success determination; should land
        // as Unclassified (not HollowCommit).
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: Some(&commit),
            pr: None,
        });
        assert_eq!(c.outcome, Outcome::Unclassified);
    }

    // ─── CI-discriminated classes ───────────────────────────────────────

    #[test]
    fn done_with_ci_green_is_clean_success() {
        let t = telemetry(TaskExitReason::Done);
        let commit = CommitInfo {
            sha: "mno345".to_owned(),
            message: "feat: crates/mu-core/src/x.rs".to_owned(),
            diff_files: vec!["crates/mu-core/src/x.rs".to_owned()],
        };
        let pr = PrInfo {
            claimed_number: Some(99),
            exists_on_origin: Some(true),
            ci: Some(CiStatus {
                fmt_pass: true,
                clippy_pass: true,
                test_pass: true,
            }),
        };
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: Some(&commit),
            pr: Some(&pr),
        });
        assert_eq!(c.outcome, Outcome::CleanSuccess);
        assert_eq!(c.confidence, Confidence::Definite);
    }

    #[test]
    fn done_with_fmt_only_fail_is_cosmetic_failure() {
        let t = telemetry(TaskExitReason::Done);
        let commit = CommitInfo {
            sha: "pqr678".to_owned(),
            message: "feat: crates/mu-core/src/x.rs".to_owned(),
            diff_files: vec!["crates/mu-core/src/x.rs".to_owned()],
        };
        let pr = PrInfo {
            claimed_number: None,
            exists_on_origin: None,
            ci: Some(CiStatus {
                fmt_pass: false,
                clippy_pass: true,
                test_pass: true,
            }),
        };
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: Some(&commit),
            pr: Some(&pr),
        });
        assert_eq!(c.outcome, Outcome::CosmeticFailure);
    }

    #[test]
    fn done_with_test_fail_is_bug_in_output() {
        let t = telemetry(TaskExitReason::Done);
        let commit = CommitInfo {
            sha: "stu901".to_owned(),
            message: "feat: crates/mu-core/src/x.rs".to_owned(),
            diff_files: vec!["crates/mu-core/src/x.rs".to_owned()],
        };
        let pr = PrInfo {
            claimed_number: None,
            exists_on_origin: None,
            ci: Some(CiStatus {
                fmt_pass: true,
                clippy_pass: true,
                test_pass: false,
            }),
        };
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: Some(&commit),
            pr: Some(&pr),
        });
        assert_eq!(c.outcome, Outcome::BugInOutput);
        assert!(c.rationale.contains("test_pass=false"));
    }

    #[test]
    fn done_with_clippy_fail_is_bug_in_output() {
        let t = telemetry(TaskExitReason::Done);
        let commit = CommitInfo {
            sha: "vwx234".to_owned(),
            message: "refactor: crates/mu-core/src/x.rs".to_owned(),
            diff_files: vec!["crates/mu-core/src/x.rs".to_owned()],
        };
        let pr = PrInfo {
            claimed_number: None,
            exists_on_origin: None,
            ci: Some(CiStatus {
                fmt_pass: true,
                clippy_pass: false,
                test_pass: true,
            }),
        };
        let c = classify_task(&ClassificationInputs {
            telemetry: &t,
            commit: Some(&commit),
            pr: Some(&pr),
        });
        assert_eq!(c.outcome, Outcome::BugInOutput);
    }

    // ─── claim parser unit tests ────────────────────────────────────────

    #[test]
    fn claim_parser_extracts_crate_rs_files() {
        let msg = "Added crates/mu-core/src/foo.rs and crates/mu-ai/tests/bar.rs";
        let found = extract_claimed_files(msg);
        assert!(found.contains(&"crates/mu-core/src/foo.rs".to_owned()));
        assert!(found.contains(&"crates/mu-ai/tests/bar.rs".to_owned()));
    }

    #[test]
    fn claim_parser_extracts_spec_md() {
        let msg = "See specs/mu-040-telemetry-substrate.md for context.";
        let found = extract_claimed_files(msg);
        assert_eq!(found, vec!["specs/mu-040-telemetry-substrate.md"]);
    }

    #[test]
    fn claim_parser_handles_backticks_and_quotes() {
        let msg = "Touched `crates/mu-core/src/x.rs` and 'specs/mu-041-forensics-classifier.md'";
        let found = extract_claimed_files(msg);
        assert!(found.contains(&"crates/mu-core/src/x.rs".to_owned()));
        assert!(found.contains(&"specs/mu-041-forensics-classifier.md".to_owned()));
    }

    #[test]
    fn claim_parser_dedups() {
        let msg = "crates/x/src/a.rs and crates/x/src/a.rs again";
        let found = extract_claimed_files(msg);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn claim_parser_skips_prose() {
        let msg = "Refactored the parser to be cleaner; no new files.";
        let found = extract_claimed_files(msg);
        assert!(found.is_empty());
    }

    #[test]
    fn claim_parser_recognizes_toplevel_files() {
        let msg = "Updated justfile and README.md plus Cargo.toml";
        let found = extract_claimed_files(msg);
        assert!(found.contains(&"justfile".to_owned()));
        assert!(found.contains(&"README.md".to_owned()));
        assert!(found.contains(&"Cargo.toml".to_owned()));
    }
}
