//! Capability attenuation primitive — the in-process v1 of the
//! biscuit/macaroon model from `specs/architecture/capability-delegation.md`.
//!
//! Capabilities express "what a session is allowed to do." A
//! capability is held by a `Session` (via `SessionState`) and
//! checked at tool dispatch. The architectural property the type
//! enforces: **a child's capability is the intersection of the
//! parent's capability with whatever narrowing the delegate request
//! specified**. There is no operation that widens a capability.
//! That's the macaroon attenuation guarantee, but enforced by Rust
//! types rather than cryptography because v1 stays in-process
//! (same daemon owns all sessions; no trust boundary crossed).
//!
//! Future work: when sessions cross daemons (cross-machine
//! delegation, cooperating-sessions over a network), swap this
//! type's storage to a signed `biscuit-auth` token. The
//! attenuation algebra stays the same; the bytes-on-the-wire
//! representation changes.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// What a session is allowed to do. `None` on any field means
/// "unrestricted on this axis." All `Some` values are upper-bound
/// constraints — narrower than `None`, never broader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capability {
    /// Tool names the session may invoke. None = any tool the
    /// daemon supports. Some(empty set) = no tools at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<HashSet<String>>,
    /// Unix milliseconds beyond which this capability is invalid.
    /// None = no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
    /// Remaining number of tool calls. Decremented at each dispatch.
    /// None = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls_remaining: Option<u32>,
    /// mu-036: whether the session may enter autonomous mode and the
    /// bounds it must respect once there. Default is `Disallowed` —
    /// autonomy is opt-in (INV-1). Wire-format is flat (snake_case)
    /// for the serde tag.
    #[serde(default)]
    pub autonomy: AutonomyCapability,
}

/// mu-036: whether a session may run autonomously (without an
/// `ask_session` between turns), and if so, the bounds the daemon
/// will enforce at every iteration boundary.
///
/// INV-1: default is `Disallowed`. A session can only enter
/// autonomous mode if its capability explicitly grants it.
/// INV-2: the bounds here are enforced by the daemon, not the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutonomyCapability {
    /// Autonomy disallowed. session.start_autonomous is rejected
    /// with `CapabilityCheck::DeniedAutonomyDisallowed`.
    Disallowed,
    /// Autonomy allowed within these bounds. Whichever bound trips
    /// first terminates the loop.
    Allowed {
        /// Cap on the number of iterations the loop may execute.
        max_iterations: u32,
        /// Cap on wall-clock time inside autonomous mode, including
        /// time spent sleeping (INV-5: sleep doesn't consume model
        /// budget, but the wall-clock budget still applies).
        max_wall_clock_ms: u64,
        /// Cap on total tool invocations across the autonomous run.
        /// Distinct from `max_tool_calls_remaining` (which applies
        /// to the whole session, autonomy or not).
        max_total_tool_calls_in_autonomy: u32,
        /// Whether the session is permitted to call
        /// `session.schedule_wakeup` to park itself.
        allow_schedule_wakeup: bool,
        /// Whether the session may use the `DelegateGrader`
        /// goal-check method (which spawns / asks a sibling session
        /// to grade — non-trivial cost).
        allow_delegate_grader: bool,
    },
}

impl Default for AutonomyCapability {
    fn default() -> Self {
        Self::Disallowed
    }
}

impl AutonomyCapability {
    /// True iff this capability permits entry to autonomous mode.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    /// Intersect two AutonomyCapability values. The narrower side
    /// always wins on every axis:
    /// * Either side `Disallowed` ⇒ result `Disallowed`.
    /// * Both `Allowed` ⇒ `Allowed` with the min of each numeric
    ///   bound and the conjunction of each boolean permission.
    pub fn intersect(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Disallowed, _) | (_, Self::Disallowed) => Self::Disallowed,
            (
                Self::Allowed {
                    max_iterations: a_iter,
                    max_wall_clock_ms: a_wall,
                    max_total_tool_calls_in_autonomy: a_tools,
                    allow_schedule_wakeup: a_sched,
                    allow_delegate_grader: a_grader,
                },
                Self::Allowed {
                    max_iterations: b_iter,
                    max_wall_clock_ms: b_wall,
                    max_total_tool_calls_in_autonomy: b_tools,
                    allow_schedule_wakeup: b_sched,
                    allow_delegate_grader: b_grader,
                },
            ) => Self::Allowed {
                max_iterations: (*a_iter).min(*b_iter),
                max_wall_clock_ms: (*a_wall).min(*b_wall),
                max_total_tool_calls_in_autonomy: (*a_tools).min(*b_tools),
                allow_schedule_wakeup: *a_sched && *b_sched,
                allow_delegate_grader: *a_grader && *b_grader,
            },
        }
    }
}

impl Capability {
    /// The root capability — unrestricted on every axis. Used for
    /// sessions created via `create_session` (no delegation chain).
    pub fn root() -> Self {
        Self::default()
    }

    /// Construct a capability by intersecting `self` with
    /// `attenuations`. The result is always ⊆ self on every axis.
    ///
    /// Tool sets: `None ∩ X = X` (unrestricted parent permits any
    /// narrowing); `Some(A) ∩ None = Some(A)` (no narrowing →
    /// child inherits); `Some(A) ∩ Some(B) = Some(A ∩ B)`.
    /// Note: if the intersection is empty, the child can't call
    /// any tool — that's intentional (delegates can be "read-nothing").
    ///
    /// Expiration: min of two (the stricter one wins; None means
    /// "no constraint from this side").
    ///
    /// Tool-call budget: min of two.
    pub fn attenuate(&self, attenuations: &CapabilityAttenuations) -> Capability {
        let allowed_tools = match (&self.allowed_tools, &attenuations.allowed_tools) {
            (None, None) => None,
            (Some(parent), None) => Some(parent.clone()),
            (None, Some(child)) => Some(child.iter().cloned().collect()),
            (Some(parent), Some(child)) => Some(
                parent
                    .iter()
                    .filter(|t| child.iter().any(|c| c == *t))
                    .cloned()
                    .collect(),
            ),
        };

        let expires_at_unix_ms = match (
            self.expires_at_unix_ms,
            attenuations
                .expires_in_seconds
                .map(|s| now_unix_ms().saturating_add(s.saturating_mul(1000))),
        ) {
            (None, None) => None,
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (Some(a), Some(b)) => Some(a.min(b)),
        };

        let max_tool_calls_remaining = match (
            self.max_tool_calls_remaining,
            attenuations.max_tool_calls,
        ) {
            (None, None) => None,
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (Some(a), Some(b)) => Some(a.min(b)),
        };

        // mu-036: intersect autonomy capability the same way.
        // Delegate sessions cannot widen autonomy beyond the
        // parent's grant.
        let autonomy = self.autonomy.intersect(&attenuations.autonomy);

        Capability {
            allowed_tools,
            expires_at_unix_ms,
            max_tool_calls_remaining,
            autonomy,
        }
    }

    /// Is `tool_name` permitted by this capability *right now*?
    /// Checks tool-list membership and expiry. The tool-call
    /// budget is NOT decremented here — see `consume_tool_call`.
    pub fn check_allow(&self, tool_name: &str) -> CapabilityCheck {
        if let Some(allowed) = &self.allowed_tools {
            if !allowed.iter().any(|t| t == tool_name) {
                return CapabilityCheck::DeniedToolNotAllowed;
            }
        }
        if let Some(deadline) = self.expires_at_unix_ms {
            if now_unix_ms() > deadline {
                return CapabilityCheck::DeniedExpired;
            }
        }
        if let Some(remaining) = self.max_tool_calls_remaining {
            if remaining == 0 {
                return CapabilityCheck::DeniedBudgetExhausted;
            }
        }
        CapabilityCheck::Allowed
    }

    /// Decrement the tool-call budget (if any). Call this AFTER a
    /// successful `check_allow`, immediately before dispatching the
    /// tool. Returns the new remaining count (or None for unlimited).
    pub fn consume_tool_call(&mut self) -> Option<u32> {
        if let Some(remaining) = self.max_tool_calls_remaining.as_mut() {
            *remaining = remaining.saturating_sub(1);
            Some(*remaining)
        } else {
            None
        }
    }
}

/// The "narrowing request" form, as it appears on the wire for
/// `session.delegate`. The runtime intersects the parent's
/// `Capability` with this to produce the child's `Capability`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CapabilityAttenuations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Capability lives for at most this many seconds from now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// mu-036: requested autonomy budget for the delegate. Intersected
    /// with parent's autonomy capability — narrower side wins.
    /// Disallowed by default (parent's Disallowed dominates regardless).
    #[serde(default)]
    pub autonomy: AutonomyCapability,
}

/// Result of `Capability::check_allow`. Distinguishes the three
/// failure modes so the agent loop's refusal callout can name
/// which axis tripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityCheck {
    Allowed,
    DeniedToolNotAllowed,
    DeniedExpired,
    DeniedBudgetExhausted,
    /// mu-036: session.start_autonomous called on a session whose
    /// capability has `autonomy: Disallowed`. The default for
    /// `Capability::root()` (INV-1).
    DeniedAutonomyDisallowed,
}

impl CapabilityCheck {
    pub fn is_allowed(&self) -> bool {
        matches!(self, CapabilityCheck::Allowed)
    }
    pub fn reason(&self) -> &'static str {
        match self {
            CapabilityCheck::Allowed => "allowed",
            CapabilityCheck::DeniedToolNotAllowed => "tool not in capability's allowed_tools set",
            CapabilityCheck::DeniedExpired => "capability has expired",
            CapabilityCheck::DeniedBudgetExhausted => "tool-call budget exhausted",
            CapabilityCheck::DeniedAutonomyDisallowed => {
                "capability has autonomy: Disallowed; cannot enter autonomous mode"
            }
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn root_capability_allows_everything() {
        let cap = Capability::root();
        assert!(cap.check_allow("bash").is_allowed());
        assert!(cap.check_allow("anything").is_allowed());
        assert!(cap.allowed_tools.is_none());
    }

    #[test]
    fn attenuate_narrows_tool_set_from_root() {
        let parent = Capability::root();
        let attn = CapabilityAttenuations {
            allowed_tools: Some(vec!["read".into(), "grep".into()]),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert_eq!(child.allowed_tools, Some(set(&["read", "grep"])));
        assert!(child.check_allow("read").is_allowed());
        assert!(!child.check_allow("bash").is_allowed());
    }

    #[test]
    fn attenuate_intersection_with_existing_set() {
        let parent = Capability {
            allowed_tools: Some(set(&["read", "grep", "glob"])),
            ..Default::default()
        };
        let attn = CapabilityAttenuations {
            allowed_tools: Some(vec![
                "read".into(),
                "edit".into(), // parent doesn't have this; gets dropped
            ]),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        // Only "read" survives — it's in both sets.
        assert_eq!(child.allowed_tools, Some(set(&["read"])));
        assert!(!child.check_allow("edit").is_allowed());
        assert!(!child.check_allow("grep").is_allowed());
    }

    #[test]
    fn attenuate_cannot_widen_tool_set() {
        // Property test: regardless of what attenuations request,
        // child's allowed_tools is always ⊆ parent's.
        let parent = Capability {
            allowed_tools: Some(set(&["read"])),
            ..Default::default()
        };
        // Try to "widen" by listing tools not in parent.
        let attn = CapabilityAttenuations {
            allowed_tools: Some(vec!["bash".into(), "edit".into()]),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        // Intersection is empty — neither "bash" nor "edit" is in parent.
        assert_eq!(child.allowed_tools, Some(HashSet::new()));
        assert!(!child.check_allow("bash").is_allowed());
        assert!(!child.check_allow("read").is_allowed());
    }

    #[test]
    fn expiry_intersection_takes_min() {
        // Parent expires in 10s, child requests 100s — child gets 10s.
        let parent_deadline = now_unix_ms() + 10_000;
        let parent = Capability {
            expires_at_unix_ms: Some(parent_deadline),
            ..Default::default()
        };
        let attn = CapabilityAttenuations {
            expires_in_seconds: Some(100),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        let child_deadline = child.expires_at_unix_ms.expect("Some");
        // Should be parent's (stricter); allow a small fudge for
        // clock movement between the two now_unix_ms calls.
        assert!(child_deadline <= parent_deadline + 5);
        assert!(child_deadline >= parent_deadline - 5);
    }

    #[test]
    fn expiry_already_passed_denies() {
        let cap = Capability {
            expires_at_unix_ms: Some(now_unix_ms() - 1000),
            ..Default::default()
        };
        assert_eq!(cap.check_allow("read"), CapabilityCheck::DeniedExpired);
    }

    #[test]
    fn budget_intersection_takes_min() {
        let parent = Capability {
            max_tool_calls_remaining: Some(10),
            ..Default::default()
        };
        let attn = CapabilityAttenuations {
            max_tool_calls: Some(3),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert_eq!(child.max_tool_calls_remaining, Some(3));
    }

    #[test]
    fn budget_consume_decrements_and_eventually_denies() {
        let mut cap = Capability {
            max_tool_calls_remaining: Some(2),
            ..Default::default()
        };
        assert!(cap.check_allow("read").is_allowed());
        cap.consume_tool_call();
        assert_eq!(cap.max_tool_calls_remaining, Some(1));
        cap.consume_tool_call();
        assert_eq!(cap.max_tool_calls_remaining, Some(0));
        // Next check is denied.
        assert_eq!(
            cap.check_allow("read"),
            CapabilityCheck::DeniedBudgetExhausted
        );
    }

    #[test]
    fn capability_round_trips_via_serde() -> Result<(), serde_json::Error> {
        let cap = Capability {
            allowed_tools: Some(set(&["read", "grep"])),
            expires_at_unix_ms: Some(1_800_000_000_000),
            max_tool_calls_remaining: Some(50),
            autonomy: AutonomyCapability::default(),
        };
        let v = serde_json::to_value(&cap)?;
        let decoded: Capability = serde_json::from_value(v)?;
        assert_eq!(decoded, cap);
        Ok(())
    }

    #[test]
    fn check_allow_reasons_are_distinct() {
        assert!(!CapabilityCheck::DeniedToolNotAllowed.is_allowed());
        assert!(!CapabilityCheck::DeniedExpired.is_allowed());
        assert!(!CapabilityCheck::DeniedBudgetExhausted.is_allowed());
        assert!(!CapabilityCheck::DeniedAutonomyDisallowed.is_allowed());
        assert!(CapabilityCheck::Allowed.is_allowed());
        // Each has a distinct human-readable reason.
        let reasons: HashSet<&str> = [
            CapabilityCheck::DeniedToolNotAllowed.reason(),
            CapabilityCheck::DeniedExpired.reason(),
            CapabilityCheck::DeniedBudgetExhausted.reason(),
            CapabilityCheck::DeniedAutonomyDisallowed.reason(),
        ]
        .into_iter()
        .collect();
        assert_eq!(reasons.len(), 4);
    }

    // ── mu-036: AutonomyCapability ───────────────────────────────

    fn autonomy_allowed(
        max_iterations: u32,
        max_wall_clock_ms: u64,
        max_total_tool_calls: u32,
    ) -> AutonomyCapability {
        AutonomyCapability::Allowed {
            max_iterations,
            max_wall_clock_ms,
            max_total_tool_calls_in_autonomy: max_total_tool_calls,
            allow_schedule_wakeup: true,
            allow_delegate_grader: true,
        }
    }

    #[test]
    fn autonomy_default_is_disallowed() {
        assert_eq!(AutonomyCapability::default(), AutonomyCapability::Disallowed);
        let root = Capability::root();
        assert_eq!(root.autonomy, AutonomyCapability::Disallowed);
        assert!(!root.autonomy.is_allowed());
    }

    #[test]
    fn autonomy_intersect_with_disallowed_yields_disallowed() {
        let allowed = autonomy_allowed(10, 60_000, 50);
        assert_eq!(
            allowed.intersect(&AutonomyCapability::Disallowed),
            AutonomyCapability::Disallowed
        );
        assert_eq!(
            AutonomyCapability::Disallowed.intersect(&allowed),
            AutonomyCapability::Disallowed
        );
    }

    #[test]
    fn autonomy_intersect_takes_min_of_numeric_bounds() {
        let parent = autonomy_allowed(10, 60_000, 50);
        let child = autonomy_allowed(20, 30_000, 100);
        let result = parent.intersect(&child);
        match result {
            AutonomyCapability::Allowed {
                max_iterations,
                max_wall_clock_ms,
                max_total_tool_calls_in_autonomy,
                ..
            } => {
                assert_eq!(max_iterations, 10);
                assert_eq!(max_wall_clock_ms, 30_000);
                assert_eq!(max_total_tool_calls_in_autonomy, 50);
            }
            _ => panic!("expected Allowed"),
        }
    }

    #[test]
    fn autonomy_intersect_conjuncts_boolean_permissions() {
        let parent = AutonomyCapability::Allowed {
            max_iterations: 10,
            max_wall_clock_ms: 60_000,
            max_total_tool_calls_in_autonomy: 50,
            allow_schedule_wakeup: true,
            allow_delegate_grader: false, // ← restrictive
        };
        let child = AutonomyCapability::Allowed {
            max_iterations: 10,
            max_wall_clock_ms: 60_000,
            max_total_tool_calls_in_autonomy: 50,
            allow_schedule_wakeup: true,
            allow_delegate_grader: true, // ← request, but parent denies
        };
        let result = parent.intersect(&child);
        match result {
            AutonomyCapability::Allowed {
                allow_schedule_wakeup,
                allow_delegate_grader,
                ..
            } => {
                assert!(allow_schedule_wakeup);
                // Parent's `false` propagates — child can't widen.
                assert!(!allow_delegate_grader);
            }
            _ => panic!("expected Allowed"),
        }
    }

    #[test]
    fn capability_attenuate_carries_autonomy_through() {
        let parent = Capability {
            autonomy: autonomy_allowed(10, 60_000, 50),
            ..Default::default()
        };
        // Child requests broader autonomy → narrower side (parent) wins.
        let attn = CapabilityAttenuations {
            autonomy: autonomy_allowed(20, 120_000, 100),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        match child.autonomy {
            AutonomyCapability::Allowed {
                max_iterations,
                max_wall_clock_ms,
                max_total_tool_calls_in_autonomy,
                ..
            } => {
                assert_eq!(max_iterations, 10);
                assert_eq!(max_wall_clock_ms, 60_000);
                assert_eq!(max_total_tool_calls_in_autonomy, 50);
            }
            _ => panic!("expected Allowed"),
        }
    }

    #[test]
    fn capability_attenuate_disallowed_parent_blocks_autonomy() {
        // Parent default is Disallowed. Any child request stays Disallowed.
        let parent = Capability::root();
        let attn = CapabilityAttenuations {
            autonomy: autonomy_allowed(99, 999_999, 999),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert_eq!(child.autonomy, AutonomyCapability::Disallowed);
        assert!(!child.autonomy.is_allowed());
    }

    #[test]
    fn autonomy_round_trips_via_serde() -> Result<(), serde_json::Error> {
        // Disallowed round-trip.
        let disallowed = AutonomyCapability::Disallowed;
        let v = serde_json::to_value(&disallowed)?;
        assert_eq!(v["kind"], "disallowed");
        let decoded: AutonomyCapability = serde_json::from_value(v)?;
        assert_eq!(decoded, disallowed);

        // Allowed round-trip.
        let allowed = autonomy_allowed(10, 60_000, 50);
        let v = serde_json::to_value(&allowed)?;
        assert_eq!(v["kind"], "allowed");
        assert_eq!(v["max_iterations"], 10);
        let decoded: AutonomyCapability = serde_json::from_value(v)?;
        assert_eq!(decoded, allowed);
        Ok(())
    }
}
