//! Deferred tool schemas with hydrate-on-touch (mu-t4l5e).
//!
//! Every request used to carry every tool's full schema (~9.4 KB for the
//! default set), and a tool that is always in the list is not thereby
//! used: twenty-one runs across two model classes with the production
//! context made zero `discover` calls. The mechanisms with evidence on
//! weak models are harness-side selection (the harness ranks the user
//! message and puts the matching schemas in the list) and need-at-the-
//! point-of-action (a call to a tool whose schema was not sent).
//!
//! Three tiers, decided at session creation and mutable only by LOADING
//! during the session, never unloading:
//!
//! - **core** — always in the tool list with full schemas
//!   (`[session].core_tools`).
//! - **deferred** — granted, schema withheld. The model sees one compact
//!   System span naming them ([`manifest_text`](DeferredTools::manifest_text)),
//!   byte-stable until a load.
//! - **loaded** — deferred tools promoted for the rest of the session;
//!   their schemas join the request from the next model call.
//!
//! Two ways a tool loads: pre-selection at turn start (the `discover`
//! ranker over the last user message, `[index].preselect`) and
//! hydrate-on-touch (a call to a withheld name loads it and runs the call
//! in the same round — see `loop_/execute_tools.rs`). Both are events
//! first (`EventPayload::ToolLoaded`, invariant 1); the in-memory set here
//! is the projection. A resumed head re-seeds from the predecessor's log
//! AND re-writes what it inherited as `Inherited` rows of its own, so the
//! loads survive a resume of the resumed head.
//!
//! Authority is untouched: deferral is presentation. The capability grant
//! (`allowed_tools`) and the effects gate keep the full set, and every
//! deferred tool stays in the loop's dispatch list so a call by name
//! resolves — which is exactly what makes touch work.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::agent::loop_::session_permits;
use crate::agent::Tool;
use crate::capability::Capability as MuCapability;
use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};
use crate::t4c_source::{self, CapabilityView};

/// Span id of the deferred-names manifest. One per rope, placed right
/// after the `system-prompt` span (or first when there is none) — see
/// [`with_manifest`].
pub const MANIFEST_SPAN_ID: &str = "deferred-tools";

/// The `discover` manifest projects native tools as `tool.<name>`
/// (`t4c_source::MuRegistrySource::from_tools`); pre-selection maps a
/// ranked path back to the tool name through this prefix.
const TOOL_PATH_PREFIX: &str = "tool.";

/// Tools the loop itself depends on, kept core whatever
/// `[session].core_tools` names: `final_answer` is how a turn ends, and
/// `discover` is the affordance the manifest points the model at.
/// Withholding either breaks the deferred tier rather than shrinking it
/// (mu-t4l5e review).
pub const ALWAYS_CORE: [&str; 2] = ["final_answer", "discover"];

/// Why a deferred tool was loaded — the `reason` on
/// `EventPayload::ToolLoaded`. Serializes as `"preselect"` / `"touch"` /
/// `"inherited"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoadReason {
    /// The turn-start ranker matched the tool against the user message.
    Preselect,
    /// The model called the tool by name while its schema was withheld.
    Touch,
    /// A resume carried the load over from the predecessor's log. The row
    /// is written on the NEW head so the load is durable there too — a
    /// resume of that head projects it like any other (mu-t4l5e review:
    /// an in-process seed alone died at the second generation).
    Inherited,
}

impl ToolLoadReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Preselect => "preselect",
            Self::Touch => "touch",
            Self::Inherited => "inherited",
        }
    }
}

/// Turn-start pre-selection knobs (`[index].preselect_limit` and the
/// ranker's `min_score_ratio`). `None` on the handle ⇒ pre-selection off;
/// touch still works.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreselectConfig {
    /// Max deferred tools loaded per user message.
    pub limit: usize,
    /// Relevance floor as a fraction of the turn's best-scoring hit across
    /// the WHOLE manifest (core tools and host CLIs included), same rule as
    /// `capability_hints`. Relative to the overall top, not the best
    /// deferred hit, so a turn about `read` does not load whichever
    /// deferred tool happens to be least irrelevant.
    pub min_score_ratio: f64,
}

/// The session's deferred set and its loaded projection, shared between
/// the agent loop (spec filtering, pre-selection, touch) and the
/// `discover` tool (marks withheld entries). Loads are monotone.
#[derive(Debug)]
pub struct DeferredTools {
    /// Names withheld at session creation. Fixed for the session; sorted
    /// so the manifest renders identically call to call.
    deferred: BTreeSet<String>,
    /// Names promoted so far. Grows only.
    loaded: Mutex<BTreeSet<String>>,
    preselect: Option<PreselectConfig>,
}

impl DeferredTools {
    /// A handle deferring exactly `deferred`. Pre-selection off.
    pub fn new(deferred: impl IntoIterator<Item = String>) -> Self {
        Self {
            deferred: deferred.into_iter().collect(),
            loaded: Mutex::new(BTreeSet::new()),
            preselect: None,
        }
    }

    /// Partition `all` tool names by `core`: everything not named core is
    /// deferred. A core name absent from the session is ignored, and
    /// [`ALWAYS_CORE`] stays core whatever `core` says.
    pub fn partition(all: impl IntoIterator<Item = String>, core: &[String]) -> Self {
        Self::new(
            all.into_iter()
                .filter(|n| !ALWAYS_CORE.contains(&n.as_str()) && !core.iter().any(|c| c == n)),
        )
    }

    pub fn with_preselect(mut self, cfg: PreselectConfig) -> Self {
        self.preselect = Some(cfg);
        self
    }

    pub fn preselect(&self) -> Option<PreselectConfig> {
        self.preselect
    }

    fn loaded(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        // A poisoned set of names is still a correct set of names.
        self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// In the deferred tier at all (loaded or not).
    pub fn is_deferred(&self, name: &str) -> bool {
        self.deferred.contains(name)
    }

    /// Deferred and not yet loaded: the schema is withheld from the
    /// request and the name is in the manifest.
    pub fn is_withheld(&self, name: &str) -> bool {
        self.is_deferred(name) && !self.loaded().contains(name)
    }

    /// Whether this tool's schema goes in the request: core, or loaded.
    pub fn is_visible(&self, name: &str) -> bool {
        !self.is_withheld(name)
    }

    /// Promote a deferred tool. Returns `true` when this call changed the
    /// set (a core or already-loaded name is a no-op). The caller appends
    /// the `ToolLoaded` event BEFORE calling this — log first, then
    /// project.
    pub fn load(&self, name: &str) -> bool {
        self.is_deferred(name) && self.loaded().insert(name.to_owned())
    }

    /// Replay: seed the loaded set from a projected log. Names outside
    /// the deferred set are ignored (the tool may be core now, or gone).
    pub fn seed_loaded(&self, names: impl IntoIterator<Item = String>) {
        let mut loaded = self.loaded();
        loaded.extend(names.into_iter().filter(|n| self.deferred.contains(n)));
    }

    /// Deferred names whose schema is still withheld, sorted.
    pub fn withheld_names(&self) -> Vec<String> {
        let loaded = self.loaded();
        self.deferred
            .iter()
            .filter(|n| !loaded.contains(*n))
            .cloned()
            .collect()
    }

    /// Names loaded so far, sorted.
    pub fn loaded_names(&self) -> Vec<String> {
        self.loaded().iter().cloned().collect()
    }

    /// The manifest span text, or `None` once nothing is withheld (the
    /// span is then dropped from the rope rather than rendered empty).
    /// Names only: the point is a stable, cheap pointer; `discover` and a
    /// touch carry the detail.
    pub fn manifest_text(&self) -> Option<String> {
        let names = self.withheld_names();
        (!names.is_empty()).then(|| {
            format!(
                "Deferred tools (call one by name to load it, or ask `discover`): {}",
                names.join(", ")
            )
        })
    }

    /// Pre-selection: rank `intent` the way the `discover` tool does
    /// (lexical, host catalog included) over the manifest ATTENUATED by
    /// `cap`, and return the withheld names that clear the floor, best
    /// first, up to the limit. Pure — the caller logs and then
    /// [`load`](Self::load)s each name. Empty when pre-selection is off,
    /// the intent is blank, nothing is withheld, or nothing scores.
    ///
    /// A tool the session may not invoke neither ranks (so it cannot set
    /// the score floor for the ones that may) nor loads — the same three
    /// checks hydrate-on-touch makes, which is what keeps the two paths in
    /// step. The grant (`check_allow`) drops the tool from the manifest in
    /// `build_manifest_for_tools`; the session's posture over the tool's
    /// `derived_effects()` marks its view `allowed_by_session: false`
    /// through `discover_view_constrained`, and [`select_candidates`] drops
    /// those before it computes the floor; and
    /// [`session_permits`](crate::agent::loop_::session_permits) — the
    /// predicate lifted out of the dispatch gate itself — re-runs all three
    /// per surviving candidate, including the required-AWS grant, which no
    /// ranked view models. Loading a tool the session cannot use would only
    /// advertise it.
    pub fn preselect_candidates(
        &self,
        tools: &[Arc<dyn Tool>],
        cap: &MuCapability,
        intent: &str,
    ) -> Vec<String> {
        let Some(cfg) = self.preselect else {
            return Vec::new();
        };
        if cfg.limit == 0 || intent.trim().is_empty() || self.withheld_names().is_empty() {
            return Vec::new();
        }
        // Manifest-build failure ⇒ nothing loads; touch still works.
        let Ok(tree) = t4c_source::build_manifest_for_tools(tools, cap, &[]).build() else {
            return Vec::new();
        };
        // Rank past the limit: core tools and host CLIs share the ranking
        // and a deferred hit that clears the floor may sit below several
        // of them.
        let depth = tools.len().saturating_mul(2).max(16);
        // Rank through the session's POSTURE, not just its grant: an
        // unconstrained session's constraints are the default (nothing
        // forbidden, so this is the old behaviour), while a session with a
        // `max_side_effects` ceiling gets its forbidden capabilities marked
        // `allowed_by_session: false` for `select_candidates` to drop.
        let constraints = cap.effective_constraints().unwrap_or_default();
        let views = t4c_source::discover_view_constrained(&tree, intent, depth, &constraints);
        select_candidates(&views, cfg, |name| {
            self.is_withheld(name)
                && tools
                    .iter()
                    .find(|t| t.spec().name == name)
                    .is_some_and(|t| session_permits(cap, &**t))
        })
    }
}

/// The floor-and-filter half of pre-selection over already-ranked views.
/// Split from the ranking so the rule is unit-testable with synthetic
/// scores (the ranker is exercised through the loop tests).
pub(crate) fn select_candidates(
    views: &[CapabilityView],
    cfg: PreselectConfig,
    is_withheld: impl Fn(&str) -> bool,
) -> Vec<String> {
    // A view the session's posture disallows is out of the ranking
    // entirely: not a candidate, and not eligible to set the floor for the
    // ones that are. Same treatment the grant already gets one level up —
    // `build_manifest_for_tools` never puts a denied tool in the tree.
    let ranked = || {
        views
            .iter()
            .filter(|v| v.allowed_by_session && v.score > 0.0)
    };
    let top = ranked().map(|v| v.score).fold(0.0_f64, f64::max);
    if top <= 0.0 {
        return Vec::new();
    }
    let floor = top * cfg.min_score_ratio.clamp(0.0, 1.0);
    ranked()
        .filter(|v| v.score >= floor)
        .filter_map(|v| v.path.strip_prefix(TOOL_PATH_PREFIX))
        .filter(|name| is_withheld(name))
        .take(cfg.limit)
        .map(str::to_owned)
        .collect()
}

/// Return a rope carrying the manifest span (or none, when `manifest` is
/// `None`): any existing manifest span is dropped first, then the new one
/// goes immediately after the `system-prompt` span, or first when the
/// rope has no system prompt. Replace-not-append matters on the
/// compaction-baseline path, where the baseline rope already holds the
/// manifest as it stood when compaction ran.
pub fn with_manifest(rope: &RetainedRope, manifest: Option<&str>) -> RetainedRope {
    let has_old = rope
        .spans()
        .iter()
        .any(|s| s.id.as_ref() == MANIFEST_SPAN_ID);
    if manifest.is_none() && !has_old {
        return rope.clone();
    }
    let spans = rope.spans();
    let mut out: Vec<Span> = Vec::with_capacity(spans.len() + 1);
    let mut placed = manifest.is_none();
    for span in spans {
        if span.id.as_ref() == MANIFEST_SPAN_ID {
            continue;
        }
        if !placed && span.id.as_ref() != "system-prompt" {
            out.push(manifest_span(manifest.unwrap_or_default()));
            placed = true;
        }
        out.push(span.clone());
        if !placed && span.id.as_ref() == "system-prompt" {
            out.push(manifest_span(manifest.unwrap_or_default()));
            placed = true;
        }
    }
    if !placed {
        out.push(manifest_span(manifest.unwrap_or_default()));
    }
    RetainedRope::from_spans(out)
}

fn manifest_span(text: &str) -> Span {
    // Startup retention: stable and cacheable like the system prompt it
    // follows, and never a compaction candidate.
    Span::new(
        MANIFEST_SPAN_ID,
        SpanKind::System,
        text,
        RetentionClass::Startup,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn view(path: &str, score: f64) -> CapabilityView {
        CapabilityView {
            path: path.to_string(),
            summary: String::new(),
            keywords: Vec::new(),
            score,
            effects: None,
            allowed_by_session: true,
            disallowed_reason: None,
            source: None,
        }
    }

    #[test]
    fn partition_defers_everything_not_core() {
        let d = DeferredTools::partition(
            names(&["read", "watch", "discover", "spawn_worker"]),
            &names(&["read", "discover", "not_in_session"]),
        );
        assert_eq!(d.withheld_names(), names(&["spawn_worker", "watch"]));
        assert!(d.is_visible("read"));
        assert!(d.is_visible("discover"));
        assert!(d.is_withheld("watch"));
        assert!(!d.is_deferred("read"));
    }

    #[test]
    fn partition_keeps_loop_critical_tools_core_whatever_the_config_says() {
        // An operator config that names neither `final_answer` nor
        // `discover` (or names only one of them) must not withhold them:
        // the turn would have no way to end and the manifest would point
        // at a tool the model cannot see.
        let d = DeferredTools::partition(
            names(&["read", "final_answer", "discover", "watch"]),
            &names(&["read"]),
        );
        assert_eq!(d.withheld_names(), names(&["watch"]));
        assert!(d.is_visible("final_answer"));
        assert!(d.is_visible("discover"));
        assert!(!d.is_deferred("final_answer"));
        assert!(!d.is_deferred("discover"));
        // Empty core list: still not withheld.
        let d = DeferredTools::partition(names(&["final_answer", "discover", "watch"]), &[]);
        assert_eq!(d.withheld_names(), names(&["watch"]));
    }

    #[test]
    fn load_is_monotone_and_reports_first_load_only() {
        let d = DeferredTools::new(names(&["watch"]));
        assert!(d.is_withheld("watch"));
        assert!(d.load("watch"), "first load changes the set");
        assert!(!d.load("watch"), "second load is a no-op");
        assert!(!d.load("read"), "a non-deferred name never loads");
        assert!(d.is_visible("watch"));
        assert_eq!(d.loaded_names(), names(&["watch"]));
        assert!(d.withheld_names().is_empty());
    }

    #[test]
    fn manifest_names_withheld_only_and_disappears_when_empty() {
        let d = DeferredTools::new(names(&["watch", "aws_recon"]));
        assert_eq!(
            d.manifest_text().as_deref(),
            Some(
                "Deferred tools (call one by name to load it, or ask `discover`): aws_recon, watch"
            )
        );
        d.load("aws_recon");
        assert_eq!(
            d.manifest_text().as_deref(),
            Some("Deferred tools (call one by name to load it, or ask `discover`): watch")
        );
        d.load("watch");
        assert_eq!(d.manifest_text(), None);
    }

    #[test]
    fn seed_loaded_replays_only_deferred_names() {
        let d = DeferredTools::new(names(&["watch", "mailbox"]));
        d.seed_loaded(names(&["watch", "read", "gone"]));
        assert!(d.is_visible("watch"));
        assert!(d.is_withheld("mailbox"));
        assert_eq!(d.loaded_names(), names(&["watch"]));
    }

    #[test]
    fn select_candidates_floors_against_the_overall_top_hit() {
        // `read` (core) is the best hit at 2.0; with ratio 0.5 only
        // deferred entries scoring >= 1.0 load. `watch` at 1.2 qualifies,
        // `mailbox` at 0.3 does not, and `read` is not withheld.
        let views = vec![
            view("tool.read", 2.0),
            view("tool.watch", 1.2),
            view("bash.rg", 1.1),
            view("tool.mailbox", 0.3),
        ];
        let withheld = ["watch", "mailbox"];
        let cfg = PreselectConfig {
            limit: 5,
            min_score_ratio: 0.5,
        };
        assert_eq!(
            select_candidates(&views, cfg, |n| withheld.contains(&n)),
            names(&["watch"])
        );
        // ratio 0 admits anything above zero, limit caps it.
        let cfg = PreselectConfig {
            limit: 1,
            min_score_ratio: 0.0,
        };
        assert_eq!(
            select_candidates(&views, cfg, |n| withheld.contains(&n)),
            names(&["watch"])
        );
        let cfg = PreselectConfig {
            limit: 5,
            min_score_ratio: 0.0,
        };
        assert_eq!(
            select_candidates(&views, cfg, |n| withheld.contains(&n)),
            names(&["watch", "mailbox"])
        );
    }

    #[test]
    fn select_candidates_ignores_views_the_session_posture_disallows() {
        // `zorblax_gizmo` is the best hit, but the session's posture forbids
        // it — what `discover_view_constrained` marks on a capability whose
        // effects exceed the ceiling. It must neither load nor anchor the
        // floor: counted, top = 4.0 and `watch` at 1.2 falls under the 0.5
        // ratio; dropped, the floor comes from `read` and `watch` clears it.
        let mut denied = view("tool.zorblax_gizmo", 4.0);
        denied.allowed_by_session = false;
        denied.disallowed_reason = Some("session is read-only (no filesystem writes)".to_string());
        let views = vec![denied, view("tool.read", 2.0), view("tool.watch", 1.2)];
        let withheld = ["watch", "zorblax_gizmo"];
        let cfg = PreselectConfig {
            limit: 5,
            min_score_ratio: 0.5,
        };
        assert_eq!(
            select_candidates(&views, cfg, |n| withheld.contains(&n)),
            names(&["watch"])
        );
    }

    #[test]
    fn select_candidates_is_empty_when_nothing_scores() {
        let views = vec![view("tool.watch", 0.0)];
        let cfg = PreselectConfig {
            limit: 5,
            min_score_ratio: 0.0,
        };
        assert!(select_candidates(&views, cfg, |_| true).is_empty());
    }

    fn rope_with_system() -> RetainedRope {
        RetainedRope::from_spans(vec![
            Span::new(
                "system-prompt",
                SpanKind::System,
                "you are mu",
                RetentionClass::Startup,
            ),
            Span::new(
                "memory-recall:k1",
                SpanKind::MemoryInjection,
                "kernel",
                RetentionClass::Startup,
            ),
            Span::new(
                "tool-schema:read",
                SpanKind::ToolSchema,
                "read",
                RetentionClass::Hot,
            ),
            Span::new("msg-0-user", SpanKind::User, "hi", RetentionClass::Hot),
        ])
    }

    fn ids(rope: &RetainedRope) -> Vec<&str> {
        rope.spans().iter().map(|s| s.id.as_ref()).collect()
    }

    #[test]
    fn manifest_lands_after_system_prompt_and_before_recall() {
        let out = with_manifest(&rope_with_system(), Some("Deferred tools: watch"));
        assert_eq!(
            ids(&out),
            vec![
                "system-prompt",
                MANIFEST_SPAN_ID,
                "memory-recall:k1",
                "tool-schema:read",
                "msg-0-user",
            ]
        );
        let span = &out.spans()[1];
        assert_eq!(span.kind, SpanKind::System);
        assert_eq!(span.content.as_ref(), "Deferred tools: watch");
    }

    #[test]
    fn manifest_is_first_without_a_system_prompt() {
        let rope = RetainedRope::from_spans(vec![Span::new(
            "msg-0-user",
            SpanKind::User,
            "hi",
            RetentionClass::Hot,
        )]);
        let out = with_manifest(&rope, Some("m"));
        assert_eq!(ids(&out), vec![MANIFEST_SPAN_ID, "msg-0-user"]);
        let empty = with_manifest(&RetainedRope::from_spans(Vec::new()), Some("m"));
        assert_eq!(ids(&empty), vec![MANIFEST_SPAN_ID]);
    }

    #[test]
    fn manifest_replaces_a_stale_one_and_none_removes_it() {
        let first = with_manifest(&rope_with_system(), Some("v1"));
        let second = with_manifest(&first, Some("v2"));
        assert_eq!(ids(&second), ids(&first), "replace in place, no duplicate");
        assert_eq!(second.spans()[1].content.as_ref(), "v2");
        let cleared = with_manifest(&second, None);
        assert_eq!(ids(&cleared), ids(&rope_with_system()));
        // No manifest and none to remove: untouched.
        assert_eq!(
            with_manifest(&rope_with_system(), None).spans(),
            rope_with_system().spans()
        );
    }

    #[test]
    fn load_reason_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ToolLoadReason::Preselect).unwrap(),
            "\"preselect\""
        );
        assert_eq!(
            serde_json::to_string(&ToolLoadReason::Touch).unwrap(),
            "\"touch\""
        );
        assert_eq!(
            serde_json::to_string(&ToolLoadReason::Inherited).unwrap(),
            "\"inherited\""
        );
        assert_eq!(ToolLoadReason::Touch.as_str(), "touch");
        assert_eq!(ToolLoadReason::Inherited.as_str(), "inherited");
    }
}
