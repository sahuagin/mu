//! mu-k011 — the INSTRUCTION leg of discover-on-demand.
//!
//! Companion to the recall dial (`MU_NO_RECALL` / `[recall].enabled`,
//! mu-54xj) and to the native `discover` tool (mu-onq8, the MEANS leg).
//!
//! When session-start recall is ON, the daemon injects memory + project
//! context (`agent memory context`, `./CLAUDE.md`) into the rope, so the
//! model already knows who it's working with. When recall is OFF, an
//! *uninstructed* model just declines — it says "I don't have that, tell me
//! / point me to a file" and runs zero searches (opus discovery experiment,
//! 2026-05-30, memory e575d64e). The same model given a ~10-line bootstrap
//! instead searched memory and called the discover tool on demand, and
//! produced the leanest, best answer of the experiment. So discover-on-demand
//! is *teachable, not automatic*: recall-off mode needs a default bootstrap.

/// The default discovery-bootstrap system prompt. Injected when recall is
/// disabled and the operator did not supply their own system prompt
/// (see [`compose_system_prompt`]). Kept terse on purpose — the experiment's
/// best run started from a ~10-line fragment (~116-token startup).
pub const DISCOVERY_BOOTSTRAP: &str = "\
You discover capabilities on demand rather than relying on front-loaded context. \
Almost nothing is preloaded here — find what you need when you need it:

- WHO the operator is, their preferences, and past decisions/feedback: search the \
memory store, e.g. `agent memory search \"<topic>\"` or `agent memory recent --type user` \
(also `--type feedback`). Look before you assume or ask.
- WHICH tool or skill does something: call the `discover` tool with a plain-language \
intent (e.g. \"search code by symbol\", \"read a file\") — it ranks the capabilities \
available to THIS session. `t4c find \"<intent>\"` ranks the host's CLI tools.

Discipline: if you're about to say \"I don't have that information\", or to ask the \
operator for something a memory search or a `discover` call would answer — discover \
first. Don't guess, and don't ask prematurely.";

/// Compose the effective session system prompt from the operator-supplied
/// prompt and the daemon's recall state.
///
/// Design decision (recorded on bead mu-k011): the bootstrap is the *daemon
/// default for recall-off mode*. It applies only when recall is disabled AND
/// the operator did not supply their own `CreateSessionRequest.system_prompt`.
///
/// - Recall ON → the session already gets recall context injected, so no
///   bootstrap (would be redundant / contradictory). Operator prompt, if any,
///   passes through unchanged.
/// - Recall OFF, no operator prompt → inject [`DISCOVERY_BOOTSTRAP`].
/// - Recall OFF, operator prompt set → respect it unchanged. An explicit
///   prompt (e.g. via `--append-system-prompt`) means the operator has taken
///   control, matching mu's existing "override replaces the daemon default"
///   semantics. This is the conservative choice the bead's design Q called
///   for: it never surprises an operator who set their own prompt, and keeps
///   the injection opt-in-by-omission rather than always-on.
///
/// A future append-compose mode (operator prompt **+** bootstrap, both) is a
/// possible follow-up if recall-off sessions with custom prompts also turn out
/// to want the discovery instruction; deferred until there's a need.
///
/// Pure (takes `recall_enabled` as an argument rather than reading the
/// `MU_NO_RECALL` env itself) so it is unit-testable without touching process
/// env — the caller passes `daemon_info.config().recall_enabled()`.
pub fn compose_system_prompt(operator: Option<String>, recall_enabled: bool) -> Option<String> {
    match (operator, recall_enabled) {
        // Operator set an explicit prompt (either recall state) → respect it.
        (Some(p), _) => Some(p),
        // Recall on, no operator prompt → no system prompt (pre-mu-k011).
        (None, true) => None,
        // Recall off, no operator prompt → the discovery bootstrap default.
        (None, false) => Some(DISCOVERY_BOOTSTRAP.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_on_no_operator_prompt_injects_nothing() {
        assert_eq!(compose_system_prompt(None, true), None);
    }

    #[test]
    fn recall_off_no_operator_prompt_injects_bootstrap() {
        assert_eq!(
            compose_system_prompt(None, false),
            Some(DISCOVERY_BOOTSTRAP.to_owned())
        );
    }

    #[test]
    fn explicit_operator_prompt_is_respected_regardless_of_recall() {
        // Conservative composition: an operator prompt is never clobbered or
        // appended to, in either recall state.
        let custom = "you are a focused code reviewer".to_owned();
        assert_eq!(
            compose_system_prompt(Some(custom.clone()), true),
            Some(custom.clone())
        );
        assert_eq!(
            compose_system_prompt(Some(custom.clone()), false),
            Some(custom)
        );
    }

    #[test]
    fn bootstrap_points_at_the_two_discovery_paths() {
        // Sanity: the fragment names both the memory path (identity/decisions)
        // and the native discover tool (tools/skills) — the two legs the bead
        // and ~/tmp/mu-discovery-bootstrap.md call for.
        assert!(DISCOVERY_BOOTSTRAP.contains("agent memory"));
        assert!(DISCOVERY_BOOTSTRAP.contains("discover"));
        // Terse by design — the experiment's best run was ~10 lines.
        assert!(
            DISCOVERY_BOOTSTRAP.lines().count() <= 14,
            "bootstrap should stay short ({} lines)",
            DISCOVERY_BOOTSTRAP.lines().count()
        );
    }
}
