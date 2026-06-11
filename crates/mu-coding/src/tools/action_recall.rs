//! mu-8puo v1 — triggered recall at the point of action.
//!
//! "Attack the weapon" applied to memory: don't load-bear on the agent
//! remembering rules at action time — make action time the recall
//! trigger. Before `bash` executes a danger verb (`rm`, `git push
//! --force`, `jj abandon`, …), this module asks the operator's memory
//! store "is there a standing rule about this?" and, on a hit,
//! surfaces it AT THE POINT OF ACTION — not at session start where it
//! drowns in the wall of context (the session-start wall is gone
//! anyway: mu-zk2i injects only the identity kernel).
//!
//! v1 scope (operator-decided on the bead, 2026-06-04):
//! - **Bash-scoped, advisory-only.** Every danger verb the bead names
//!   is bash-mediated, so the narrowest viable seam is inside
//!   `BashTool` — no agent-loop or trait changes. The generalized
//!   PreToolUse hook (all tools, blocking variant, dispatcher
//!   surface) is the cross-cutting follow-up this v1 gathers
//!   evidence for.
//! - **Refuse-once-then-allow.** The first matching command returns
//!   the recalled rule INSTEAD of executing; an identical re-issue
//!   proceeds. The advisory result carries `is_error: false` —
//!   deliberately: strict mode's `RetryPolicy::Never` refuses
//!   identical retries of *errored* calls, so an error-shaped
//!   advisory would escalate into a hard block.
//! - **Fail-open.** Memory infrastructure must never gate the action
//!   path: absent binary, non-zero exit, timeout (1s), or zero hits
//!   all mean "execute normally". The advisory is a courtesy from
//!   the past, not a capability check — capability enforcement lives
//!   in the allowlist/approval/capability layers, untouched here.
//! - **Lexical search, not semantic recall.** `agent memory search
//!   --type feedback` is local FTS5 (~ms, deterministic, offline-
//!   safe); danger-verb rules carry their verbs literally, so lexical
//!   precision is high. The semantic-recall upgrade can ride the
//!   PreToolUse follow-up.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

/// Hard cap on how much of the search output reaches the advisory —
/// the rule's header lines are the payload; a full war story would
/// recreate the wall this whole goal removed.
const ADVISORY_EXCERPT_CHARS: usize = 1_200;
/// Subprocess budget. Local FTS5 answers in single-digit ms; anything
/// slower means something is wrong and the action path must not wait.
const SEARCH_TIMEOUT: Duration = Duration::from_millis(1_000);

/// A danger-verb pattern: command tokens must start with `prefix`,
/// and when `required_flag` is Some, that token must also appear
/// anywhere after the prefix. `query` is the memory-search phrase.
struct DangerVerb {
    prefix: &'static [&'static str],
    required_flag: Option<&'static [&'static str]>,
    query: &'static str,
}

/// The curated v1 verb list — the bead's named verbs plus the
/// incident-class kin recorded in feedback memories. Curation note:
/// precision over recall; a noisy advisory teaches the model to
/// ignore advisories.
const DANGER_VERBS: &[DangerVerb] = &[
    DangerVerb {
        prefix: &["rm"],
        required_flag: None,
        query: "rm delete files",
    },
    DangerVerb {
        prefix: &["git", "push"],
        required_flag: Some(&["--force", "-f"]),
        query: "git push force",
    },
    DangerVerb {
        prefix: &["jj", "abandon"],
        required_flag: None,
        query: "jj abandon",
    },
    DangerVerb {
        prefix: &["jj", "op", "restore"],
        required_flag: None,
        query: "jj op restore",
    },
    DangerVerb {
        prefix: &["sed"],
        required_flag: Some(&["-i"]),
        query: "sed -i in-place",
    },
    DangerVerb {
        prefix: &["cargo", "publish"],
        required_flag: None,
        query: "cargo publish",
    },
    DangerVerb {
        prefix: &["git", "reset"],
        required_flag: Some(&["--hard"]),
        query: "git reset hard",
    },
    DangerVerb {
        prefix: &["git", "checkout"],
        required_flag: Some(&["--"]),
        query: "git checkout revert uncommitted",
    },
    DangerVerb {
        prefix: &["br", "close"],
        required_flag: None,
        query: "br close bead",
    },
    DangerVerb {
        prefix: &["gh", "pr", "merge"],
        required_flag: None,
        query: "gh pr merge",
    },
    DangerVerb {
        prefix: &["gh", "pr", "close"],
        required_flag: None,
        query: "gh pr close",
    },
];

/// Match a command against the danger-verb table. Returns the memory
/// query for the first matching entry.
fn match_danger_verb(command: &str) -> Option<&'static str> {
    let tokens = shlex::split(command)?;
    DANGER_VERBS.iter().find_map(|v| {
        let prefix_matches = tokens.len() >= v.prefix.len()
            && v.prefix.iter().zip(tokens.iter()).all(|(p, t)| p == t);
        if !prefix_matches {
            return None;
        }
        match v.required_flag {
            None => Some(v.query),
            Some(flags) => tokens[v.prefix.len()..]
                .iter()
                .any(|t| flags.contains(&t.as_str()))
                .then_some(v.query),
        }
    })
}

/// Point-of-action memory advisory for `BashTool`. One instance per
/// tool (per session); the advised-set is the refuse-once state.
#[derive(Debug)]
pub struct ActionRecall {
    binary_path: PathBuf,
    /// When false, `advisory_for` always passes. Set from the
    /// `MU_NO_ACTION_RECALL` env at construction (daemon start) —
    /// construction-time read keeps per-call behavior deterministic
    /// and tests free of process-env mutation.
    enabled: bool,
    /// Subprocess budget for the memory search. Production uses
    /// [`SEARCH_TIMEOUT`] (fail-open — an advisory must never stall a
    /// turn); tests inject a generous budget because a loaded CI
    /// runner can take >1s just to fork the stub shell, and the tests'
    /// subject is the advise-once logic, not the timeout.
    search_timeout: Duration,
    /// Command-hashes already advised this session. A hash hit means
    /// the model saw the rule and re-issued: execute.
    advised: Mutex<HashSet<u64>>,
}

impl ActionRecall {
    /// Standard construction: `~/.local/bin/agent`, enabled unless
    /// `MU_NO_ACTION_RECALL` is set to a truthy value.
    pub fn new() -> Self {
        let path = dirs::home_dir()
            .map(|h| h.join(".local").join("bin").join("agent"))
            .unwrap_or_else(|| PathBuf::from("agent"));
        let disabled = std::env::var("MU_NO_ACTION_RECALL")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
            .unwrap_or(false);
        Self {
            binary_path: path,
            enabled: !disabled,
            search_timeout: SEARCH_TIMEOUT,
            advised: Mutex::new(HashSet::new()),
        }
    }

    /// Test hook: stub binary path.
    pub fn with_binary(binary_path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
            enabled: true,
            search_timeout: SEARCH_TIMEOUT,
            advised: Mutex::new(HashSet::new()),
        }
    }

    /// Test hook: stub binary path + explicit subprocess budget (see
    /// `search_timeout` — loaded CI runners need more than the 1s
    /// production fail-open budget just to fork a shell).
    #[cfg(test)]
    fn with_binary_and_timeout(binary_path: impl Into<PathBuf>, search_timeout: Duration) -> Self {
        Self {
            binary_path: binary_path.into(),
            enabled: true,
            search_timeout,
            advised: Mutex::new(HashSet::new()),
        }
    }

    /// Test hook / explicit off: advisory never fires.
    pub fn disabled() -> Self {
        Self {
            binary_path: PathBuf::from("agent"),
            enabled: false,
            search_timeout: SEARCH_TIMEOUT,
            advised: Mutex::new(HashSet::new()),
        }
    }

    /// Returns `Some(advisory_text)` when `command` matches a danger
    /// verb, has not been advised before, AND the memory store holds
    /// a standing feedback rule about it. `None` means "execute
    /// normally" — including on every infrastructure failure
    /// (fail-open by design, see module docs).
    pub async fn advisory_for(&self, command: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let query = match_danger_verb(command)?;

        let cmd_hash = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            command.hash(&mut h);
            h.finish()
        };
        // Already advised → the re-issue path. (Insert happens only
        // after a hit below, so non-hit commands never accumulate.)
        if self
            .advised
            .lock()
            .ok()
            .is_some_and(|set| set.contains(&cmd_hash))
        {
            return None;
        }

        let binary = self.binary_path.clone();
        let q = query.to_owned();
        let task = tokio::task::spawn_blocking(move || {
            Command::new(&binary)
                .args(["memory", "search", &q, "--type", "feedback", "--limit", "2"])
                .output()
        });
        let output = match tokio::time::timeout(self.search_timeout, task).await {
            Ok(Ok(Ok(out))) if out.status.success() => out,
            // Absent binary / non-zero exit / join error / timeout:
            // fail open.
            _ => return None,
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stdout = stdout.trim();
        if stdout.is_empty() {
            return None;
        }

        let excerpt: String = stdout.chars().take(ADVISORY_EXCERPT_CHARS).collect();
        if let Ok(mut set) = self.advised.lock() {
            set.insert(cmd_hash);
        }
        Some(format!(
            "memory advisory — command NOT executed.\n\
             A standing rule matches `{query}`:\n\n{excerpt}\n\n\
             The labels are testimony (recorded/verified) — weigh them. \
             If the command is still appropriate, re-issue it verbatim to \
             proceed; this advisory fires once per command."
        ))
    }
}

impl Default for ActionRecall {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn danger_verb_table_matches_and_rejects() {
        // Matches
        assert!(match_danger_verb("rm -rf /tmp/x").is_some());
        assert!(match_danger_verb("git push --force origin main").is_some());
        assert!(match_danger_verb("git push -f").is_some());
        assert!(match_danger_verb("jj abandon xyz").is_some());
        assert!(match_danger_verb("sed -i s/a/b/ file").is_some());
        assert!(match_danger_verb("git checkout -- .").is_some());
        assert!(match_danger_verb("gh pr merge 42 --merge").is_some());
        // Non-matches: plain forms of guarded verbs
        assert!(match_danger_verb("git push origin main").is_none());
        assert!(match_danger_verb("sed s/a/b/ file").is_none());
        assert!(match_danger_verb("git checkout feature-branch").is_none());
        assert!(match_danger_verb("ls -la").is_none());
        // Verb must be the command, not an argument
        assert!(match_danger_verb("echo rm -rf").is_none());
    }

    fn stub(dir_tag: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mu-8puo-{}-{}", dir_tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-stub.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        let mut perms = f.metadata().unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[tokio::test]
    async fn advisory_fires_once_then_allows_reissue() {
        let script = stub(
            "once",
            r#"echo '[838c3bf4] (feedback) never-batch-destructive — rule  [2026-06-04]'
echo '  recorded 2026-06-04 · never verified'"#,
        );
        // Generous budget: a loaded CI runner can take >1s to fork the
        // stub shell, and a timeout fails open to None — which this
        // test would misread as "did not advise".
        let ar = ActionRecall::with_binary_and_timeout(&script, Duration::from_secs(30));

        let first = ar.advisory_for("jj abandon xyz").await;
        let text = first.expect("first matching call must advise");
        assert!(text.contains("NOT executed"));
        assert!(text.contains("never-batch-destructive"));
        assert!(text.contains("re-issue"), "must teach the re-issue path");

        assert!(
            ar.advisory_for("jj abandon xyz").await.is_none(),
            "identical re-issue executes"
        );
        // A DIFFERENT dangerous command advises independently.
        assert!(ar.advisory_for("jj abandon other").await.is_some());
    }

    #[tokio::test]
    async fn fail_open_on_missing_binary_empty_output_and_non_danger() {
        let ar = ActionRecall::with_binary("/nonexistent/agent-binary");
        assert!(ar.advisory_for("rm -rf /tmp/x").await.is_none());

        let silent = stub("silent", "exit 0");
        let ar = ActionRecall::with_binary(&silent);
        assert!(
            ar.advisory_for("rm -rf /tmp/x").await.is_none(),
            "no matching rule in store → execute normally"
        );
        // Non-danger commands never even shell out.
        let ar = ActionRecall::with_binary("/nonexistent/agent-binary");
        assert!(ar.advisory_for("ls -la").await.is_none());
    }

    #[tokio::test]
    async fn disabled_never_advises() {
        let ar = ActionRecall::disabled();
        assert!(ar.advisory_for("rm -rf /").await.is_none());
    }

    #[tokio::test]
    async fn advisory_excerpt_is_capped() {
        let script = stub("long", r#"yes 'rule line padding' | head -200"#);
        // Generous budget — same CI-load rationale as
        // advisory_fires_once_then_allows_reissue.
        let ar = ActionRecall::with_binary_and_timeout(&script, Duration::from_secs(30));
        let text = ar.advisory_for("cargo publish").await.expect("advises");
        assert!(
            text.len() < ADVISORY_EXCERPT_CHARS + 400,
            "advisory must not recreate the wall: {} chars",
            text.len()
        );
    }
}
