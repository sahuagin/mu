pub mod action_recall;
pub mod autonomy;
pub mod aws_recon;
pub mod bash;
pub mod dialogue;
pub mod discover;
pub mod edit;
pub mod edit_matcher;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod memory_recall;
pub mod path;
pub mod read;
pub mod spawn_worker;
pub mod watch;
pub mod write;

pub use autonomy::{ScheduleWakeupTool, StartAutonomousTool};
pub use aws_recon::AwsReconTool;
pub use bash::{BashMode, BashTool};
pub use dialogue::{DialogueBind, SessionDialogueTool};
pub use discover::DiscoverTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use memory_recall::MemoryRecallTool;
pub use read::ReadTool;
pub use spawn_worker::SpawnWorkerTool;
pub use watch::WatchTool;
pub use write::WriteTool;

#[cfg(test)]
mod policy_invariants {
    //! mu-usfj: guard against SELF-CLASSIFIED AUTHORITY — a tool that
    //! can affect the world while declaring `ReadOnly` + `Allow`
    //! bypasses the dispatch gate that constrains `bash` (the seed bug:
    //! `watch`/`spawn_worker` ran arbitrary `sh -c` while declared
    //! benign). This test makes "is this tool benign?" a decision a
    //! human must encode HERE with a rationale — never something a tool
    //! gets to assert about itself by omission. Adding a new tool that
    //! ships the default policy forces a conscious classification.
    use std::sync::Arc;

    use mu_core::agent::{PermissionLevel, SideEffects, Tool};

    use super::*;
    use crate::serve::{DaemonInfo, Sessions};

    /// Names whose `ReadOnly` + `Allow` posture is AUDITED-safe, each
    /// with the reason it cannot be a gate bypass. A tool NOT listed
    /// here may not ship benign-by-default — it must declare honest
    /// `side_effects` (Execute/Mutating/Destructive/External) or carry
    /// an `Ask`/`Deny` permission or an `required_aws_capability` gate.
    fn audited_benign(name: &str) -> Option<&'static str> {
        match name {
            "read" | "ls" | "glob" | "grep" => {
                Some("genuinely read-only: no fs writes, no exec, no network")
            }
            "memory_recall" => Some(
                "execs a FIXED read-oriented binary (agent memory) with structured \
                 args — the model cannot make it run arbitrary work",
            ),
            "discover" => Some("ranks the session's own tools; read-only projection"),
            "start_autonomous" | "schedule_wakeup" => Some(
                "affects only this session's own control flow, not world state; \
                 gated by AutonomyCapability at tool-presence + the loop input \
                 handler, not the tool-policy gate (mu-036)",
            ),
            "aws_recon" => {
                Some("read-only recon AND carries required_aws_capability (double-gated)")
            }
            _ => None,
        }
    }

    fn assert_not_silently_dangerous(t: &dyn Tool) {
        let s = t.spec();
        let benign = s.policy.side_effects == SideEffects::ReadOnly
            && s.policy.permission == PermissionLevel::Allow
            && s.policy.required_aws_capability.is_none();
        if benign {
            assert!(
                audited_benign(&s.name).is_some(),
                "tool `{}` ships ReadOnly+Allow but is not on the audited-benign \
                 allowlist. If it genuinely cannot affect the world, add it to \
                 audited_benign() WITH a one-line rationale. If it can \
                 (exec/spawn/write/network), declare honest side_effects. \
                 Self-classified authority is the mu-usfj bug class — do not \
                 reintroduce it.",
                s.name
            );
        }
    }

    #[test]
    fn no_builtin_tool_is_silently_dangerous() {
        let sessions = Sessions::new();
        let di = DaemonInfo::new("test");
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ReadTool::new()),
            Arc::new(WriteTool::new()),
            Arc::new(LsTool::new()),
            Arc::new(EditTool::new()),
            Arc::new(GrepTool::new()),
            Arc::new(GlobTool::new()),
            Arc::new(MemoryRecallTool::new()),
            Arc::new(BashTool::new(BashMode::Yolo)),
            Arc::new(WatchTool::new(
                sessions.downgrade(),
                "s".into(),
                BashMode::Yolo,
            )),
            Arc::new(SpawnWorkerTool::new(sessions.downgrade(), di.clone(), None)),
            Arc::new(StartAutonomousTool::new(sessions.downgrade(), "s".into())),
            Arc::new(ScheduleWakeupTool::new(sessions.downgrade(), "s".into())),
        ];
        // NOTE: aws_recon (env-dependent ctor) and discover (needs a
        // sibling-tool snapshot) are not constructed here; both are on
        // the audited_benign list with their rationale. RemoteMcpTool
        // (imported MCP tools) is the largest blast radius and is
        // handled separately — it has no honest side-effects source, so
        // it must fail SAFE at import (mu-n25a Phase 4), not here.
        for t in &tools {
            assert_not_silently_dangerous(t.as_ref());
        }

        // Positive lock: the seed-bug tools now declare Execute.
        let spec_of = |n: &str| {
            tools
                .iter()
                .find(|t| t.spec().name == n)
                .unwrap_or_else(|| panic!("missing tool {n}"))
                .spec()
        };
        assert_eq!(spec_of("watch").policy.side_effects, SideEffects::Execute);
        assert_eq!(
            spec_of("spawn_worker").policy.side_effects,
            SideEffects::Execute
        );
    }
}
