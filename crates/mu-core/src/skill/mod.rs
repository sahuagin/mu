//! Skill catalog + activation manager (mu-nat).
//!
//! A [`Skill`] is metadata + reference files addressed as
//! [`crate::context::Span`]s with per-file granularity (v1, per the
//! experiment spec). A [`SkillManager`] holds the catalog of
//! registered skills and tracks which are currently active. The
//! manager itself is small — most of the load-bearing behavior lives
//! on the [`crate::context::RetainedRope`] it cooperates with:
//! [`SkillManager::activate`] just delegates to
//! [`crate::context::RetainedRope::activate_skill`], which pushes
//! the spans into the retained set and emits a
//! [`crate::context::RopeEvent::SkillActivated`] event.
//!
//! Per `specs/architecture/event-sourced-context.md` lines 542-562:
//! "Activating a skill = adding pointers to the retained set;
//! deactivating = dropping them. There is no separate 'skill loader'
//! mechanism — skill activation IS pointer-set membership." This
//! file is the realization of that principle.

pub mod loader;

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::context::{RetainedRope, Span};

/// One registered skill: a stable id plus the per-file spans that
/// will enter the rope on activation.
///
/// Span ids should be stable across activations of the same skill
/// (e.g., `skill:<id>:<file>`) so provenance lookups are
/// deterministic. The manager does not enforce a particular id
/// shape — callers pick the convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub spans: Vec<Span>,
}

impl Skill {
    /// Construct a skill from id + spans. The id must match the id
    /// later passed to [`SkillManager::activate`] /
    /// [`SkillManager::deactivate`].
    pub fn new(id: impl Into<String>, spans: Vec<Span>) -> Self {
        Self {
            id: id.into(),
            spans,
        }
    }
}

/// Catalog of registered skills + the active-id set.
///
/// The manager does NOT own the rope — `activate` / `deactivate`
/// receive `&mut RetainedRope` so the rope's lifetime/ownership is
/// the caller's concern. This matches the spec's "rope is the
/// substrate, manager is a higher-level helper" framing.
#[derive(Debug, Default)]
pub struct SkillManager {
    /// `skill_id -> Skill`.
    catalog: HashMap<String, Skill>,
    /// Currently-active skill ids. Updated by activate / deactivate.
    active: HashSet<String>,
}

/// Errors from [`SkillManager`] operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    #[error("skill not registered: {0}")]
    NotRegistered(String),
    #[error("skill already active: {0}")]
    AlreadyActive(String),
    #[error("skill not active: {0}")]
    NotActive(String),
}

impl SkillManager {
    /// Empty manager — no skills registered, none active.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a skill in the catalog. Replaces any prior
    /// registration with the same id (the spans of the old version
    /// are NOT removed from the rope; deactivate first if that's
    /// the intent).
    pub fn register(&mut self, skill: Skill) {
        self.catalog.insert(skill.id.clone(), skill);
    }

    /// Activate a registered skill: push its spans into the rope's
    /// retained set and emit a [`crate::context::RopeEvent::SkillActivated`]
    /// event. Returns [`SkillError::NotRegistered`] if the skill is
    /// not in the catalog, [`SkillError::AlreadyActive`] if it is
    /// already active.
    ///
    /// The spans are cloned into the rope so the catalog retains
    /// its copy (re-activation after deactivation is supported).
    pub fn activate(&mut self, skill_id: &str, rope: &mut RetainedRope) -> Result<(), SkillError> {
        let skill = self
            .catalog
            .get(skill_id)
            .ok_or_else(|| SkillError::NotRegistered(skill_id.to_string()))?;
        if self.active.contains(skill_id) {
            return Err(SkillError::AlreadyActive(skill_id.to_string()));
        }
        rope.activate_skill(skill.id.clone(), skill.spans.clone());
        self.active.insert(skill.id.clone());
        Ok(())
    }

    /// Deactivate a skill: remove its spans from the rope and emit
    /// a [`crate::context::RopeEvent::SkillDeactivated`] event.
    /// Returns [`SkillError::NotActive`] if the skill is not
    /// currently active.
    ///
    /// The skill stays in the catalog after deactivation so it can
    /// be re-activated later.
    pub fn deactivate(
        &mut self,
        skill_id: &str,
        rope: &mut RetainedRope,
    ) -> Result<(), SkillError> {
        if !self.active.contains(skill_id) {
            return Err(SkillError::NotActive(skill_id.to_string()));
        }
        rope.deactivate_skill(skill_id);
        self.active.remove(skill_id);
        Ok(())
    }

    /// True iff `skill_id` is currently active.
    pub fn is_active(&self, skill_id: &str) -> bool {
        self.active.contains(skill_id)
    }

    /// True iff `skill_id` is registered (active or not).
    pub fn is_registered(&self, skill_id: &str) -> bool {
        self.catalog.contains_key(skill_id)
    }

    /// Iterate registered skill ids.
    pub fn registered(&self) -> impl Iterator<Item = &str> {
        self.catalog.keys().map(String::as_str)
    }

    /// Iterate currently-active skill ids.
    pub fn active_ids(&self) -> impl Iterator<Item = &str> {
        self.active.iter().map(String::as_str)
    }

    /// Number of currently-active skills.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{RetentionClass, RopeEvent, SpanKind};

    fn skill_with_two_files(id: &str) -> Skill {
        Skill::new(
            id,
            vec![
                Span::new(
                    format!("skill:{id}:SKILL.md"),
                    SpanKind::SkillActivation,
                    "skill body",
                    RetentionClass::Pinned,
                ),
                Span::new(
                    format!("skill:{id}:references/stop-criteria.md"),
                    SpanKind::SkillActivation,
                    "stop criteria body",
                    RetentionClass::Pinned,
                ),
            ],
        )
    }

    #[test]
    fn activate_pushes_spans_into_rope() {
        let mut mgr = SkillManager::new();
        let mut rope = RetainedRope::new();
        mgr.register(skill_with_two_files("goal-protocol"));
        mgr.activate("goal-protocol", &mut rope)
            .expect("activate ok");
        assert_eq!(rope.len(), 2);
        assert!(mgr.is_active("goal-protocol"));
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn activate_unknown_skill_errors() {
        let mut mgr = SkillManager::new();
        let mut rope = RetainedRope::new();
        let err = mgr.activate("nope", &mut rope).expect_err("should error");
        assert_eq!(err, SkillError::NotRegistered("nope".into()));
        assert!(rope.is_empty());
    }

    #[test]
    fn double_activate_errors_without_double_pushing() {
        let mut mgr = SkillManager::new();
        let mut rope = RetainedRope::new();
        mgr.register(skill_with_two_files("review"));
        mgr.activate("review", &mut rope).expect("first activate");
        let err = mgr
            .activate("review", &mut rope)
            .expect_err("second errors");
        assert_eq!(err, SkillError::AlreadyActive("review".into()));
        // Span count unchanged after the rejected second activate.
        assert_eq!(rope.len(), 2);
    }

    #[test]
    fn deactivate_drops_spans_and_clears_active() {
        let mut mgr = SkillManager::new();
        let mut rope = RetainedRope::new();
        mgr.register(skill_with_two_files("goal-protocol"));
        mgr.activate("goal-protocol", &mut rope).unwrap();
        mgr.deactivate("goal-protocol", &mut rope)
            .expect("deactivate ok");
        assert!(rope.is_empty());
        assert!(!mgr.is_active("goal-protocol"));
        // Still registered after deactivation.
        assert!(mgr.is_registered("goal-protocol"));
    }

    #[test]
    fn deactivate_not_active_errors() {
        let mut mgr = SkillManager::new();
        let mut rope = RetainedRope::new();
        mgr.register(skill_with_two_files("review"));
        let err = mgr.deactivate("review", &mut rope).expect_err("not active");
        assert_eq!(err, SkillError::NotActive("review".into()));
    }

    #[test]
    fn reactivation_after_deactivation_works() {
        let mut mgr = SkillManager::new();
        let mut rope = RetainedRope::new();
        mgr.register(skill_with_two_files("review"));

        mgr.activate("review", &mut rope).unwrap();
        mgr.deactivate("review", &mut rope).unwrap();
        // Skill is still registered; re-activate succeeds.
        mgr.activate("review", &mut rope).expect("re-activate");
        assert_eq!(rope.len(), 2);
        assert!(mgr.is_active("review"));
    }

    #[test]
    fn manager_drives_rope_event_log() {
        let mut mgr = SkillManager::new();
        let mut rope = RetainedRope::new();
        mgr.register(skill_with_two_files("review"));
        mgr.activate("review", &mut rope).unwrap();
        mgr.deactivate("review", &mut rope).unwrap();
        // The rope log records both transitions.
        assert_eq!(rope.events().len(), 2);
        matches!(rope.events()[0], RopeEvent::SkillActivated { .. });
        matches!(rope.events()[1], RopeEvent::SkillDeactivated { .. });
    }

    #[test]
    fn activate_concurrent_skills_independent_in_rope() {
        let mut mgr = SkillManager::new();
        let mut rope = RetainedRope::new();
        mgr.register(skill_with_two_files("goal-protocol"));
        mgr.register(skill_with_two_files("review"));

        mgr.activate("goal-protocol", &mut rope).unwrap();
        mgr.activate("review", &mut rope).unwrap();
        assert_eq!(rope.len(), 4);

        // Deactivating one leaves the other intact.
        mgr.deactivate("goal-protocol", &mut rope).unwrap();
        assert_eq!(rope.len(), 2);
        assert!(rope.spans().iter().all(|s| s.id.contains("review")));
    }
}
