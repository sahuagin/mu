//! Heuristic effect-classifier (mu-8stm.3) — the deterministic floor under the
//! Phase-2 classification grind.
//!
//! Given a tool's probe signals (name + summary + keywords + `--help` text),
//! [`classify`] PROPOSES a structured [`Effects`] reach, a bounded-vs-passthrough
//! call, and a [`Confidence`]. It is a *starting point*, not a verdict: the grind
//! agent reads the proposal + the captured help and refines with its own model
//! before anything lands in the catalog. So this stays pure, deterministic, and
//! leaf-pure (no I/O, no LLM) — t4c is the mechanism; the agent is the judgment.
//!
//! Honesty rule (mirrors `Option<Effects>` None≠benign): when signals are weak we
//! emit a best guess at LOW confidence rather than fabricating a confident benign
//! claim — low confidence is the grind's "route this to a human" marker.

use crate::capability::{Effects, FsEffect};

/// How much the heuristic trusts its own proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }
}

/// A proposed classification for one capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// True when the tool runs arbitrary user commands (shell/interpreter/eval):
    /// its effects are invocation-determined, so a per-axis label is unsound. The
    /// grind routes these to "Execute, gated at the shell boundary," not a narrow
    /// `Effects`. This flag is the classifier's highest-value output.
    pub passthrough: bool,
    /// Proposed reach. For passthrough tools this is the maximal (worst-case)
    /// reach, so treating it as a gate fails safe.
    pub effects: Effects,
    pub confidence: Confidence,
}

/// Commands that run arbitrary user code — effects are invocation-determined.
const PASSTHROUGH_NAMES: &[&str] = &[
    "bash", "sh", "zsh", "fish", "dash", "ksh", "csh", "tcsh", "python", "python3", "perl", "ruby",
    "node", "deno", "lua", "env", "xargs", "eval", "exec", "sudo", "doas", "ssh", "nohup",
    "setsid",
];

fn basename(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// Heuristically classify a capability. `name` is the command/subcommand token
/// (basename used for strong per-tool signals); `haystack` is the lowercased
/// concatenation of name + summary + keywords + subcommand names + `--help` text.
pub fn classify(name: &str, haystack: &str) -> Classification {
    let base = basename(name);
    let hay = haystack.to_lowercase();

    // Passthrough first: an unbounded tool can't be soundly per-axis labelled.
    if PASSTHROUGH_NAMES.contains(&base)
        || any(
            &hay,
            &[
                "run arbitrary",
                "execute arbitrary",
                "run a command",
                "execute commands",
                "arbitrary shell",
                "spawn a shell",
            ],
        )
    {
        return Classification {
            passthrough: true,
            effects: Effects {
                filesystem: FsEffect::Write,
                vcs: true,
                network: true,
                spend: true,
                process: true,
            },
            confidence: Confidence::High,
        };
    }

    let mut eff = Effects::default();
    let mut axes_hit = 0u32;
    let mut strong = false;

    // --- name-based strong signals (high trust) ---------------------------
    let (name_fs, name_net): (Option<FsEffect>, bool) = match base {
        "rm" | "rmdir" | "unlink" | "shred" | "dd" | "truncate" | "tee" | "mkdir" | "touch"
        | "mv" | "cp" | "install" | "ln" => (Some(FsEffect::Write), false),
        "cat" | "less" | "more" | "head" | "tail" | "bat" | "ls" | "eza" | "find" | "fd"
        | "grep" | "rg" | "stat" | "file" | "wc" => (Some(FsEffect::Read), false),
        "curl" | "wget" | "scp" | "rsync" | "ping" | "dig" | "host" => (None, true),
        _ => (None, false),
    };
    if let Some(fs) = name_fs {
        eff.filesystem = fs;
        axes_hit += 1;
        strong = true;
    }
    if name_net {
        eff.network = true;
        axes_hit += 1;
        strong = true;
    }
    if matches!(base, "git" | "jj" | "hg" | "svn") {
        eff.vcs = true;
        axes_hit += 1;
        strong = true;
    }

    // --- keyword signals over the haystack --------------------------------
    if any(
        &hay,
        &[
            "fetch",
            "download",
            "curl",
            "wget",
            "http",
            "url",
            "upload",
            "webhook",
            "api call",
            "rest api",
            "request to",
        ],
    ) {
        if !eff.network {
            axes_hit += 1;
        }
        eff.network = true;
    }
    if any(
        &hay,
        &[
            "commit",
            "rebase",
            "checkout",
            "working copy",
            "version control",
            "revision",
            " merge",
            " tag ",
            "git ",
        ],
    ) {
        if !eff.vcs {
            axes_hit += 1;
        }
        eff.vcs = true;
        // VCS mutations write the repo/working-copy.
        if any(
            &hay,
            &["commit", "rebase", "checkout", "merge", "tag", "amend"],
        ) {
            eff.filesystem = FsEffect::Write;
        }
    }
    if any(
        &hay,
        &[
            "delete",
            "remove",
            "write",
            "save",
            "edit",
            "modify",
            "overwrite",
            "truncate",
            "rename",
            "create file",
            "destroy",
        ],
    ) {
        if eff.filesystem != FsEffect::Write {
            axes_hit += 1;
        }
        eff.filesystem = FsEffect::Write;
    } else if eff.filesystem == FsEffect::None
        && any(
            &hay,
            &[
                "read", "list", "show", "print", "display", "view", "search", "inspect", "status",
                "diff",
            ],
        )
    {
        eff.filesystem = FsEffect::Read;
        axes_hit += 1;
    }
    if any(
        &hay,
        &[
            "billing",
            "charge",
            "cost",
            "metered",
            "paid",
            " spend",
            "provision",
            "aws ",
            "gcp ",
            "azure",
        ],
    ) {
        if !eff.spend {
            axes_hit += 1;
        }
        eff.spend = true;
    }
    if any(
        &hay,
        &[
            "daemon",
            "serve",
            "server",
            "spawn",
            "background",
            "long-running",
            "watch",
            "fork a process",
        ],
    ) {
        if !eff.process {
            axes_hit += 1;
        }
        eff.process = true;
    }

    let confidence = if strong || axes_hit >= 2 {
        Confidence::High
    } else if axes_hit == 1 {
        Confidence::Medium
    } else {
        Confidence::Low
    };

    Classification {
        passthrough: false,
        effects: eff,
        confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_is_filesystem_write() {
        let c = classify("rm", "rm remove files or directories");
        assert!(!c.passthrough);
        assert_eq!(c.effects.filesystem, FsEffect::Write);
        assert_eq!(c.confidence, Confidence::High);
    }

    #[test]
    fn cat_is_filesystem_read() {
        let c = classify("cat", "cat concatenate and print files");
        assert_eq!(c.effects.filesystem, FsEffect::Read);
        assert!(!c.effects.network && !c.effects.process);
        assert_eq!(c.confidence, Confidence::High);
    }

    #[test]
    fn curl_is_network() {
        let c = classify("curl", "curl transfer data from a url");
        assert!(c.effects.network);
        assert_eq!(c.confidence, Confidence::High);
    }

    #[test]
    fn jj_commit_is_vcs_and_write() {
        let c = classify(
            "commit",
            "commit the working copy changes to a new revision",
        );
        assert!(c.effects.vcs);
        assert_eq!(c.effects.filesystem, FsEffect::Write);
    }

    #[test]
    fn bash_is_passthrough_worst_case() {
        let c = classify("bash", "bash the GNU Bourne-Again SHell");
        assert!(c.passthrough);
        assert_eq!(c.effects.filesystem, FsEffect::Write);
        assert!(c.effects.network && c.effects.process);
        assert_eq!(c.confidence, Confidence::High);
    }

    #[test]
    fn passthrough_detected_from_help_text() {
        let c = classify("xyzzy", "xyzzy runs an arbitrary shell command for you");
        assert!(c.passthrough);
    }

    #[test]
    fn ambiguous_tool_is_low_confidence_and_conservative() {
        let c = classify("frobnicate", "frobnicate the doohickey");
        assert!(!c.passthrough);
        assert_eq!(c.confidence, Confidence::Low);
        // No signal => default (FsEffect::None, all flags false). The low
        // confidence — not a fake benign claim — is what routes it to a human.
        assert_eq!(c.effects, Effects::default());
    }

    #[test]
    fn basename_strips_path() {
        let c = classify("/usr/bin/rm", "remove");
        assert_eq!(c.effects.filesystem, FsEffect::Write);
    }
}
