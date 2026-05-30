//! Phase 3 (mu-kex4.6): project mu's live capability set — registered tools and
//! discovered skills — into t4c's [`RegistrySource`], so `t4c find` over the
//! in-process registry answers "what is loaded right now": the Layer-1
//! capability manifest.
//!
//! Leaf invariant: the dependency arrow is `mu-core -> t4c` only; t4c never
//! depends on mu. This module is the seam where mu's runtime types
//! (`Tool`/`ToolSpec`, `LoadedSkill`) become t4c `Capability`s.
//!
//! Two sources rather than one: tools and skills carry distinct provenance and
//! live under distinct path prefixes (`tool.<name>` / `skill.<name>`), and a
//! t4c `Registry` merges multiple sources cleanly.
//!
//! Sub-bead `mu-kex4.6.2` (skills) is the one the discriminating cold-dogfood
//! proved out: t4c was blind to skills, and the authoritative answer lived in a
//! skill (memory `t4c-discriminating-dogfood-phase3-justified`).

use std::sync::Arc;

use t4c::{
    CapPath, Capability, Effects, Embedder, FsEffect, LexicalRanker, Ranked, Ranker, Registry,
    RegistrySource, SemanticRanker, SessionConstraints, Tree, VectorCache,
};

use crate::agent::tool::{SideEffects, Tool, ToolPolicy};
use crate::capability::Capability as MuCapability;
use crate::skill::loader::LoadedSkill;
use crate::tool_registry::ToolRegistry;

/// A t4c source backed by capabilities projected from mu's runtime.
#[derive(Debug)]
pub struct MuRegistrySource {
    name: String,
    caps: Vec<Capability>,
}

impl MuRegistrySource {
    /// Project registered tools into `tool.<name>` capabilities.
    ///
    /// Pass an **already-attenuated** slice (see
    /// [`crate::tool_registry::ToolRegistry::attenuate_with`]); doing the
    /// capability-filter upstream is what makes discovery track permission —
    /// the projected source contains only tools the caller may invoke, so
    /// `requires` is left empty here.
    pub fn from_tools(tools: &[Arc<dyn Tool>]) -> Self {
        let caps = tools
            .iter()
            .filter_map(|t| {
                let spec = t.spec();
                capability_for(
                    "tool",
                    &spec.name,
                    &spec.description,
                    spec.when.as_deref(),
                    // Tools carry a runtime `ToolPolicy` — project it so the
                    // model can see a tool's effects before invoking.
                    Some(effects_from_policy(&spec.policy)),
                )
            })
            .collect();
        Self {
            name: "mu-tools".into(),
            caps,
        }
    }

    /// Project discovered skills into `skill.<name>` capabilities.
    pub fn from_skills(skills: &[LoadedSkill]) -> Self {
        let caps = skills
            .iter()
            .filter_map(|s| {
                let fm = &s.frontmatter;
                capability_for(
                    "skill",
                    &fm.name,
                    &fm.description,
                    fm.when_to_use.as_deref(),
                    // Skills carry no runtime policy, so their effects are
                    // unannotated (`None`) rather than falsely-benign. A skill
                    // is a routing hint, not a tool with a known effect surface.
                    None,
                )
            })
            .collect();
        Self {
            name: "mu-skills".into(),
            caps,
        }
    }

    /// Number of capabilities projected. Some inputs may be skipped if their
    /// name can't form a valid [`CapPath`].
    pub fn len(&self) -> usize {
        self.caps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }
}

impl RegistrySource for MuRegistrySource {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> anyhow::Result<Vec<Capability>> {
        Ok(self.caps.clone())
    }
}

/// Assemble mu's live capability manifest: registered tools + discovered skills
/// projected into one t4c [`Registry`]. Pass an already-attenuated tool slice so
/// discovery tracks permission. Build the returned registry into a [`Tree`] to
/// query it — this is the in-process Layer-1 manifest, the same shape the CLI
/// builds from a curated catalog but sourced from mu's runtime (mu-kex4.6.4).
pub fn build_manifest(tools: &[Arc<dyn Tool>], skills: &[LoadedSkill]) -> Registry {
    let mut reg = Registry::new();
    reg.add_source(Box::new(MuRegistrySource::from_tools(tools)));
    reg.add_source(Box::new(MuRegistrySource::from_skills(skills)));
    reg
}

/// Build the manifest for a session, applying its capability so **discovery
/// tracks permission**: only tools `cap` permits are projected, via
/// [`ToolRegistry::attenuate_with`]. This is the *permission* axis — an agent
/// can't discover a tool it may not invoke. *Availability* is orthogonal and
/// upstream: the registry holds only loaded tools (the in-process analogue of
/// the CLI's catalog-∩-installed probe). Skills are not yet capability-gated;
/// biscuit-driven skill attenuation is future work (mu-5u5f).
pub fn build_manifest_for(
    registry: &ToolRegistry,
    cap: &MuCapability,
    skills: &[LoadedSkill],
) -> Registry {
    build_manifest(&registry.attenuate_with(cap), skills)
}

/// Rank a built manifest's capabilities against a free-text intent, best-first —
/// the in-process `find`, lexical floor. Always available (no embedder needed);
/// [`discover_semantic`] is the semantic upgrade when an embedder is present.
pub fn discover<'a>(tree: &'a Tree, intent: &str) -> Vec<Ranked<'a>> {
    let caps: Vec<&Capability> = tree.all().collect();
    LexicalRanker.rank(intent, &caps)
}

/// Semantic in-process `find` (mu-kex4.6.3): embed the manifest's capabilities
/// and the intent, rank by cosine. mu's caps are live/dynamic, so the vector
/// cache is built here rather than loaded from a pre-computed file. The caller
/// supplies the embedder (`ConfigEmbedder::from_config()` live, `FakeEmbedder`
/// in tests) and a model label for cache provenance. Lexical [`discover`]
/// remains the offline floor. Delivers `.3`'s intent via t4c's merged embedder
/// rather than a RecallProvider-specific ranker.
pub fn discover_semantic<'a, E: Embedder>(
    tree: &'a Tree,
    intent: &str,
    embedder: E,
    model: &str,
) -> anyhow::Result<Vec<Ranked<'a>>> {
    let caps: Vec<&Capability> = tree.all().collect();
    let cache = VectorCache::build(&embedder, model, &caps)?;
    Ok(SemanticRanker::new(embedder, cache.by_path).rank(intent, &caps))
}

/// A serializable view of a ranked capability — the wire/JSON shape a daemon
/// `capabilities/discover` RPC or a `--json` CLI returns (mu-kex4.6.4). Keeps
/// the borrow-free, `Serialize`-able fields the model needs to pick + adapt a
/// call: the path, what it's for, its keywords, the match score, its effects
/// (mu-kex4.6.6), and whether the session's constraints permit it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CapabilityView {
    pub path: String,
    pub summary: String,
    pub keywords: Vec<String>,
    pub score: f64,
    /// What invoking this does to the world, when known. `None` = unannotated
    /// (e.g. a skill), deliberately distinct from a known-benign effect set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effects: Option<Effects>,
    /// False when the capability is installed + permitted but inappropriate
    /// this session (e.g. a writer under a read-only session). Separates
    /// "installed" from "appropriate" — the discovery surface shows it rather
    /// than hiding it, so the model learns *why* it can't use it.
    pub allowed_by_session: bool,
    /// Why the session disallows it, when `allowed_by_session` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disallowed_reason: Option<String>,
    /// Where this capability came from — the source name that produced the
    /// winning entry (mu-kex4.6.8). Lets the model tell "live MCP says loaded"
    /// from "curated catalog says installed". `None` only if the tree lost the
    /// path's provenance (shouldn't happen for a ranked result).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Rank the manifest and project the top `limit` results into serializable
/// [`CapabilityView`]s — the JSON-first `find` the consuming surfaces emit.
/// Unconstrained: every result is `allowed_by_session: true`. Use
/// [`discover_view_constrained`] to apply a session's restrictions.
pub fn discover_view(tree: &Tree, intent: &str, limit: usize) -> Vec<CapabilityView> {
    discover_view_constrained(tree, intent, limit, &SessionConstraints::default())
}

/// Like [`discover_view`], but apply the session's [`SessionConstraints`]: a
/// ranked capability whose [`Effects`] violate a constraint is still returned
/// (it ranked — the model should see it) but marked `allowed_by_session: false`
/// with a reason. Capabilities with unannotated effects (`None`) can't be
/// checked, so they stay allowed — unknown is not the same as disallowed.
pub fn discover_view_constrained(
    tree: &Tree,
    intent: &str,
    limit: usize,
    constraints: &SessionConstraints,
) -> Vec<CapabilityView> {
    discover(tree, intent)
        .into_iter()
        .take(limit)
        .map(|r| {
            let effects = r.cap.effects.clone();
            let disallowed_reason = effects.as_ref().and_then(|e| e.disallowed_by(constraints));
            CapabilityView {
                path: r.cap.path.to_string(),
                summary: r.cap.summary.clone(),
                keywords: r.cap.keywords.clone(),
                score: r.score,
                effects,
                allowed_by_session: disallowed_reason.is_none(),
                disallowed_reason,
                source: tree.source_of(&r.cap.path).map(str::to_string),
            }
        })
        .collect()
}

/// Build one capability under `<source>.<name>`. Returns `None` when `name`
/// can't form a valid path (empty / too deep) — best-effort projection: skip a
/// pathological entry rather than fail the whole source.
fn capability_for(
    source: &str,
    name: &str,
    description: &str,
    when: Option<&str>,
    effects: Option<Effects>,
) -> Option<Capability> {
    let path = CapPath::parse(&format!("{source}.{name}")).ok()?;
    let summary = if description.is_empty() {
        name.to_string()
    } else {
        description.to_string()
    };
    // Keywords feed the ranker. Tokenize the name and the routing hint (`when`)
    // so both lexical and semantic ranking have signal — the cold dogfood
    // showed the `when` hint is where a tool's intent vocabulary lives.
    let mut keywords = tokenize(name);
    if let Some(w) = when {
        keywords.extend(tokenize(w));
    }
    Some(Capability {
        path,
        summary,
        keywords,
        // In-process: invocation is a tool-call routed by the daemon
        // (mu-kex4.6.4), not a shell argv.
        invoke: Vec::new(),
        // Schema is served in-process (no `--help` to shell out to); the daemon
        // RPC surface carries `ToolSpec.input_schema` directly.
        help: None,
        // Slice is pre-attenuated, so everything here is already permitted.
        requires: Vec::new(),
        effects,
    })
}

/// Project mu's runtime [`ToolPolicy`] into t4c [`Effects`] — the lossy but
/// honest bridge from mu's single `SideEffects` axis to t4c's multi-axis safety
/// surface. mu collapses effects into one enum, so this maps what mu actually
/// knows (filesystem reach; network for `External`) and leaves the axes mu
/// can't speak to at their conservative default. The one inference: a tool
/// gated on an AWS capability hits the network and spends, so flag both.
fn effects_from_policy(policy: &ToolPolicy) -> Effects {
    let mut e = Effects::default();
    match policy.side_effects {
        // ReadOnly = "no observable change" — the common case is a filesystem
        // read; mu has no finer signal, so claim read, nothing more.
        SideEffects::ReadOnly => e.filesystem = FsEffect::Read,
        // Mutating / Destructive both write; mu's enum doesn't distinguish
        // reversibility, and t4c's `Effects` doesn't model it, so both land as
        // a write claim.
        SideEffects::Mutating | SideEffects::Destructive => e.filesystem = FsEffect::Write,
        // External = reaches the network (mu's reserved network class).
        SideEffects::External => e.network = true,
    }
    if policy.required_aws_capability.is_some() {
        e.network = true;
        e.spend = true;
    }
    e
}

/// Lowercase alphanumeric word-split — splits on any non-alphanumeric so
/// `freebsd-jails` -> `["freebsd", "jails"]` and `code_recall` -> `["code", "recall"]`.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::PermissionLevel;
    use crate::agent::tool::RetryPolicy;

    /// Test shorthand: project a `tool.<name>` capability with no effect
    /// annotation (the effect axis is exercised separately).
    fn tool(name: &str, description: &str, when: Option<&str>) -> Capability {
        capability_for("tool", name, description, when, None).expect("valid path")
    }

    #[test]
    fn tool_maps_under_tool_prefix_with_when_keywords() {
        let c = capability_for(
            "tool",
            "code_recall",
            "semantic + lexical code search",
            Some("when you need to find where something is implemented"),
            None,
        )
        .expect("valid path");
        assert_eq!(c.path.segments().len(), 2);
        assert_eq!(c.path.segments()[0], "tool");
        assert_eq!(c.path.segments()[1], "code_recall");
        assert_eq!(c.summary, "semantic + lexical code search");
        // name tokenized
        assert!(c.keywords.contains(&"code".to_string()));
        assert!(c.keywords.contains(&"recall".to_string()));
        // when-hint tokenized into keywords
        assert!(c.keywords.contains(&"implemented".to_string()));
        // discovery-tracks-permission: pre-attenuated => no residual gate
        assert!(c.requires.is_empty());
        // in-process => no shell invoke/help
        assert!(c.invoke.is_empty());
        assert!(c.help.is_none());
    }

    #[test]
    fn skill_maps_under_skill_prefix_and_splits_hyphens() {
        let c = capability_for(
            "skill",
            "freebsd-jails",
            "jail + pot architecture",
            None,
            None,
        )
        .expect("valid path");
        assert_eq!(c.path.segments()[0], "skill");
        assert_eq!(c.path.segments()[1], "freebsd-jails");
        assert!(c.keywords.contains(&"freebsd".to_string()));
        assert!(c.keywords.contains(&"jails".to_string()));
        // skills carry no policy => unannotated effects, not falsely-benign
        assert!(c.effects.is_none());
    }

    #[test]
    fn empty_description_falls_back_to_name() {
        let c = capability_for("tool", "bash", "", None, None).expect("valid path");
        assert_eq!(c.summary, "bash");
    }

    #[test]
    fn unparseable_name_is_skipped_not_fatal() {
        // empty segment => CapPath::parse errors => None (best-effort skip)
        assert!(capability_for("tool", "", "x", None, None).is_none());
    }

    #[test]
    fn discover_ranks_by_intent_across_tools_and_skills() {
        let caps = vec![
            tool(
                "grep",
                "line search for exact strings",
                Some("exact string matches"),
            ),
            capability_for(
                "skill",
                "freebsd-jails",
                "jail and pot architecture",
                Some("spline jexec pot"),
                None,
            )
            .unwrap(),
            tool(
                "code_recall",
                "semantic code search",
                Some("find where something is implemented in the codebase"),
            ),
        ];
        let mut reg = Registry::new();
        reg.add_source(Box::new(t4c::StaticSource::new("test", caps)));
        let tree = reg.build().expect("build manifest");
        assert_eq!(tree.len(), 3);

        let ranked = discover(&tree, "where is this function implemented in the code");
        assert!(!ranked.is_empty());
        // intent overlaps code_recall's keywords (implemented/code), not grep/jails
        assert_eq!(ranked[0].cap.path.segments()[1], "code_recall");
    }

    #[test]
    fn discover_semantic_with_fake_embedder_ranks_token_overlap_first() {
        let caps = vec![
            tool(
                "grep",
                "line search for exact strings",
                Some("exact string matches"),
            ),
            capability_for(
                "skill",
                "freebsd-jails",
                "jail and pot architecture",
                Some("spline jexec pot"),
                None,
            )
            .unwrap(),
            tool(
                "code_recall",
                "semantic code search",
                Some("find where something is implemented in the codebase"),
            ),
        ];
        let mut reg = Registry::new();
        reg.add_source(Box::new(t4c::StaticSource::new("test", caps)));
        let tree = reg.build().expect("build manifest");

        // FakeEmbedder is a deterministic hashed bag-of-words (CI-safe, offline),
        // so token overlap still drives cosine: the code/implemented intent ranks
        // code_recall first. Verifies the semantic wiring end-to-end.
        let ranked = discover_semantic(
            &tree,
            "where is this implemented in the code",
            t4c::FakeEmbedder::new(),
            "fake",
        )
        .expect("semantic discover");
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].cap.path.segments()[1], "code_recall");
    }

    #[test]
    fn discover_view_projects_serializable_dtos_and_respects_limit() {
        let caps = vec![
            tool("grep", "line search", Some("exact string matches")),
            tool(
                "code_recall",
                "semantic code search",
                Some("find where something is implemented in the code"),
            ),
        ];
        let mut reg = Registry::new();
        reg.add_source(Box::new(t4c::StaticSource::new("test", caps)));
        let tree = reg.build().expect("build manifest");

        let views = discover_view(&tree, "where is this implemented in the code", 1);
        assert_eq!(views.len(), 1, "limit honored");
        assert_eq!(views[0].path, "tool.code_recall");
        assert!(!views[0].summary.is_empty());
        // unconstrained => allowed, and an unannotated cap omits effects on the wire
        assert!(views[0].allowed_by_session);
        // provenance: the source name that produced the entry (mu-kex4.6.8)
        assert_eq!(views[0].source.as_deref(), Some("test"));
        // serializes to JSON (the wire shape)
        let json = serde_json::to_string(&views[0]).expect("serialize");
        assert!(json.contains("tool.code_recall"));
        assert!(json.contains("\"source\":\"test\""));
    }

    #[test]
    fn policy_projects_side_effects_onto_the_right_axis() {
        // read-only => filesystem read, no flags
        let ro = effects_from_policy(&ToolPolicy::default());
        assert_eq!(ro.filesystem, FsEffect::Read);
        assert!(!ro.network && !ro.vcs && !ro.spend);

        // mutating => filesystem write
        let mutating = effects_from_policy(&ToolPolicy {
            side_effects: SideEffects::Mutating,
            permission: PermissionLevel::Allow,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: None,
            idempotent: false,
        });
        assert_eq!(mutating.filesystem, FsEffect::Write);

        // an AWS-gated tool reaches the network and spends
        let aws = effects_from_policy(&ToolPolicy {
            side_effects: SideEffects::ReadOnly,
            permission: PermissionLevel::Allow,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: Some("ec2:DescribeInstances".to_string()),
            idempotent: true,
        });
        assert!(aws.network && aws.spend);
    }

    #[test]
    fn read_only_session_marks_a_writer_disallowed_but_keeps_it_in_results() {
        // one reader, one writer — both rank, but a read-only session disallows
        // the writer (installed + permitted, yet inappropriate now).
        let reader = capability_for(
            "tool",
            "read",
            "read a file from disk",
            Some("inspect file contents"),
            Some(Effects {
                filesystem: FsEffect::Read,
                ..Effects::default()
            }),
        )
        .unwrap();
        let writer = capability_for(
            "tool",
            "write",
            "write a file to disk",
            Some("create or edit file contents"),
            Some(Effects {
                filesystem: FsEffect::Write,
                ..Effects::default()
            }),
        )
        .unwrap();
        let mut reg = Registry::new();
        reg.add_source(Box::new(t4c::StaticSource::new(
            "test",
            vec![reader, writer],
        )));
        let tree = reg.build().expect("build manifest");

        let ro = SessionConstraints {
            no_writes: true,
            ..SessionConstraints::default()
        };
        let views = discover_view_constrained(&tree, "file contents on disk", 10, &ro);
        let w = views
            .iter()
            .find(|v| v.path == "tool.write")
            .expect("writer present in results");
        // still surfaced (not hidden), but flagged with a reason
        assert!(!w.allowed_by_session);
        assert!(w.disallowed_reason.is_some());

        let r = views
            .iter()
            .find(|v| v.path == "tool.read")
            .expect("reader present");
        assert!(r.allowed_by_session);
        assert!(r.disallowed_reason.is_none());

        // the disallowed reason reaches the wire; an allowed cap omits it
        let wjson = serde_json::to_string(w).unwrap();
        assert!(wjson.contains("allowed_by_session"));
        assert!(wjson.contains("read-only"));
    }
}
