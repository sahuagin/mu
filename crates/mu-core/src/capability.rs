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
    /// mu-f5o: typed AWS-capability grants the session holds. Empty
    /// set = no AWS access. Multi-grant by design (a worker may hold
    /// `aws.scout.readonly` + `aws.sandbox.build` simultaneously).
    /// Narrowing-only on `intersect` and `attenuate`: child cannot
    /// gain AWS caps the parent does not hold. See `AwsCapability`
    /// and `intersect_aws_sets`.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub aws: HashSet<AwsCapability>,
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

/// mu-f5o: typed AWS capability — one named role-grant (matched to the
/// catalog at `mu-aws-sandbox-infra/capabilities/aws.json`, e.g.
/// `aws.scout.readonly`). The optional `session_policy` is the biscuit-
/// shaped per-invocation narrowing axis: an inline policy passed to
/// `sts:AssumeRole` that further restricts what the role can do on this
/// specific call. Carried for type-level prep; intersect of two `Some`
/// policies is deferred (see `AwsCapability::intersect`).
///
/// **Hash/Eq subtlety:** `serde_json::Value` does not implement `Hash`,
/// so `Hash` is implemented manually on `name` only. `PartialEq` compares
/// both fields. A `HashSet<AwsCapability>` may therefore contain two caps
/// with the same name and different policies as distinct elements; in
/// practice the invariant "one cap per name" is maintained by the
/// `intersect` operation, which collapses same-name pairs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwsCapability {
    /// The capability name from the catalog. Matches the role-bundle
    /// the runner will assume (e.g. `aws.scout.readonly`).
    pub name: String,
    /// Optional inline session policy passed to `sts:AssumeRole` to
    /// further narrow the role's effective permissions on this call.
    /// None = use the role's identity policy as-is. Intersect of two
    /// `Some` is deferred — see `intersect`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_policy: Option<serde_json::Value>,
}

impl PartialEq for AwsCapability {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.session_policy == other.session_policy
    }
}

impl Eq for AwsCapability {}

impl std::hash::Hash for AwsCapability {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Hash by name only. session_policy is intentionally excluded:
        // serde_json::Value lacks Hash, and the practical invariant is
        // one-cap-per-name (see struct doc).
        self.name.hash(state);
    }
}

impl AwsCapability {
    /// Intersect two `AwsCapability` values. Narrowing-only:
    /// * Different name → `None` (incompatible; drop on intersect).
    /// * Same name + at most one `Some` session_policy → `Some` with
    ///   the policy from whichever side has one (the narrower of
    ///   "unrestricted within role" vs "policy-narrowed").
    /// * Same name + both `Some` session_policies → `None`. Policy-
    ///   intersection logic (AWS-style policy narrowing) is deferred
    ///   to a future bead; the conservative `None` outcome preserves
    ///   the narrowing-only invariant (drops the cap rather than
    ///   producing a possibly-too-broad combined policy).
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        if self.name != other.name {
            return None;
        }
        let session_policy = match (&self.session_policy, &other.session_policy) {
            (None, None) => None,
            (Some(p), None) | (None, Some(p)) => Some(p.clone()),
            (Some(_), Some(_)) => {
                // Deferred: both-Some intersect needs a policy-narrowing
                // algorithm not in v1 scope. Return None (drop) — the
                // most-restrictive outcome.
                return None;
            }
        };
        Some(AwsCapability {
            name: self.name.clone(),
            session_policy,
        })
    }
}

/// Intersect two `HashSet<AwsCapability>` values. For each name present
/// in both sides, produce the narrower cap (via `AwsCapability::intersect`)
/// and include it. Names present in only one side are dropped. Two caps
/// with the same name and incompatible session_policies (both `Some`)
/// are also dropped.
fn intersect_aws_sets(
    a: &HashSet<AwsCapability>,
    b: &HashSet<AwsCapability>,
) -> HashSet<AwsCapability> {
    // Pre-index b by name for O(1) lookup, making the overall
    // operation O(n+m) instead of O(n*m). Practical N is small but
    // the cleaner algorithm is easy and obvious. (mu-lwt)
    let b_by_name: std::collections::HashMap<&str, &AwsCapability> =
        b.iter().map(|c| (c.name.as_str(), c)).collect();
    let mut result = HashSet::new();
    for cap in a {
        if let Some(other) = b_by_name.get(cap.name.as_str()) {
            if let Some(narrower) = cap.intersect(other) {
                result.insert(narrower);
            }
        }
    }
    result
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

        // mu-f5o: AWS axis. None on request → child inherits parent's
        // AWS set (no narrowing requested). Some(vec) → child gets
        // parent ∩ requested. Either way the result is ⊆ parent.
        let aws = match &attenuations.aws {
            None => self.aws.clone(),
            Some(requested) => {
                let requested_set: HashSet<AwsCapability> = requested.iter().cloned().collect();
                intersect_aws_sets(&self.aws, &requested_set)
            }
        };

        Capability {
            allowed_tools,
            expires_at_unix_ms,
            max_tool_calls_remaining,
            autonomy,
            aws,
        }
    }

    /// mu-f5o: symmetric intersect of two capabilities — the broker-
    /// pattern primitive. Composes two grants into their most-restrictive
    /// combination. Distinct from `attenuate`, which is asymmetric
    /// (parent + delegate's narrowing request).
    ///
    /// Narrowing-only on every axis (INV-1 generalized): the result
    /// is ⊆ self AND ⊆ other on every axis.
    ///
    /// * `allowed_tools`: `None` is identity (unrestricted); both `Some`
    ///   → set intersection.
    /// * `expires_at_unix_ms` / `max_tool_calls_remaining`: `None` is
    ///   identity; both `Some` → minimum.
    /// * `autonomy`: delegates to `AutonomyCapability::intersect`.
    /// * `aws`: per `intersect_aws_sets` — name-match required, same-
    ///   name pairs collapse via `AwsCapability::intersect`.
    pub fn intersect(&self, other: &Self) -> Capability {
        let allowed_tools = match (&self.allowed_tools, &other.allowed_tools) {
            (None, None) => None,
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (Some(a), Some(b)) => Some(a.intersection(b).cloned().collect()),
        };

        let expires_at_unix_ms = match (self.expires_at_unix_ms, other.expires_at_unix_ms) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(a),
            (Some(a), Some(b)) => Some(a.min(b)),
        };

        let max_tool_calls_remaining =
            match (self.max_tool_calls_remaining, other.max_tool_calls_remaining) {
                (None, None) => None,
                (Some(a), None) | (None, Some(a)) => Some(a),
                (Some(a), Some(b)) => Some(a.min(b)),
            };

        let autonomy = self.autonomy.intersect(&other.autonomy);
        let aws = intersect_aws_sets(&self.aws, &other.aws);

        Capability {
            allowed_tools,
            expires_at_unix_ms,
            max_tool_calls_remaining,
            autonomy,
            aws,
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
    /// mu-f5o: requested AWS-capability grants for the delegate.
    /// `None` = no narrowing requested on this axis → child inherits
    /// parent's AWS set as-is. `Some(vec)` = explicit request → child's
    /// AWS set is `parent ∩ requested` per `intersect_aws_sets`. The
    /// `Vec` shape (rather than `HashSet`) on the wire is for stable
    /// JSON ordering; converted to a set internally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws: Option<Vec<AwsCapability>>,
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
            aws: HashSet::new(),
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

    // ── mu-f5o: AwsCapability ────────────────────────────────────

    fn aws(name: &str) -> AwsCapability {
        AwsCapability {
            name: name.to_string(),
            session_policy: None,
        }
    }

    fn aws_with_policy(name: &str, policy: serde_json::Value) -> AwsCapability {
        AwsCapability {
            name: name.to_string(),
            session_policy: Some(policy),
        }
    }

    fn aws_set(caps: &[AwsCapability]) -> HashSet<AwsCapability> {
        caps.iter().cloned().collect()
    }

    #[test]
    fn aws_capability_round_trips_via_serde() -> Result<(), serde_json::Error> {
        // No policy round-trip.
        let bare = aws("aws.scout.readonly");
        let v = serde_json::to_value(&bare)?;
        assert_eq!(v["name"], "aws.scout.readonly");
        assert!(v.get("session_policy").is_none(), "None policy should be skipped");
        let decoded: AwsCapability = serde_json::from_value(v)?;
        assert_eq!(decoded, bare);

        // With policy round-trip.
        let policied = aws_with_policy(
            "aws.scout.readonly",
            serde_json::json!({"Version": "2012-10-17", "Statement": []}),
        );
        let v = serde_json::to_value(&policied)?;
        assert_eq!(v["session_policy"]["Version"], "2012-10-17");
        let decoded: AwsCapability = serde_json::from_value(v)?;
        assert_eq!(decoded, policied);
        Ok(())
    }

    #[test]
    fn aws_intersect_same_name_no_policies_is_same_cap() {
        let a = aws("aws.scout.readonly");
        let b = aws("aws.scout.readonly");
        let result = a.intersect(&b).expect("same name + no policies → Some");
        assert_eq!(result.name, "aws.scout.readonly");
        assert!(result.session_policy.is_none());
    }

    #[test]
    fn aws_intersect_different_names_yields_none() {
        let a = aws("aws.scout.readonly");
        let b = aws("aws.sandbox.build");
        assert!(a.intersect(&b).is_none(), "different names must drop");
        assert!(b.intersect(&a).is_none(), "intersect is symmetric");
    }

    #[test]
    fn aws_intersect_one_some_policy_carries_through() {
        let policy = serde_json::json!({"Statement": [{"Effect": "Deny", "Resource": "*"}]});
        let bare = aws("aws.scout.readonly");
        let with_pol = aws_with_policy("aws.scout.readonly", policy.clone());
        // None policy on one side + Some on the other → narrower (Some) wins.
        let r1 = bare.intersect(&with_pol).expect("same name → Some");
        assert_eq!(r1.session_policy, Some(policy.clone()));
        let r2 = with_pol.intersect(&bare).expect("same name → Some (symmetric)");
        assert_eq!(r2.session_policy, Some(policy));
    }

    #[test]
    fn aws_intersect_both_some_policies_is_deferred_to_none() {
        // Deferred per spec: both-Some session_policy returns None to
        // preserve narrowing-only without a policy-intersection algorithm.
        let pol_a = serde_json::json!({"Statement": [{"Resource": "arn:aws:s3:::a/*"}]});
        let pol_b = serde_json::json!({"Statement": [{"Resource": "arn:aws:s3:::b/*"}]});
        let a = aws_with_policy("aws.scout.readonly", pol_a);
        let b = aws_with_policy("aws.scout.readonly", pol_b);
        assert!(
            a.intersect(&b).is_none(),
            "both-Some session_policy must return None (deferred policy intersect)"
        );
    }

    #[test]
    fn intersect_aws_sets_drops_unmatched_names() {
        let parent = aws_set(&[aws("aws.scout.readonly"), aws("aws.sandbox.build")]);
        let child = aws_set(&[aws("aws.scout.readonly"), aws("aws.auditor.read")]);
        let result = intersect_aws_sets(&parent, &child);
        // Only the common name survives.
        assert_eq!(result.len(), 1);
        assert!(result.contains(&aws("aws.scout.readonly")));
        assert!(!result.contains(&aws("aws.sandbox.build")));
        assert!(!result.contains(&aws("aws.auditor.read")));
    }

    #[test]
    fn intersect_aws_sets_is_narrowing_only_property() {
        // INV-1 generalized: result ⊆ a AND result ⊆ b (by name).
        let a = aws_set(&[
            aws("aws.scout.readonly"),
            aws("aws.auditor.read"),
            aws("aws.sandbox.build"),
        ]);
        let b = aws_set(&[
            aws("aws.scout.readonly"),
            aws("aws.iac.plan"),
            aws("aws.sandbox.build"),
        ]);
        let result = intersect_aws_sets(&a, &b);
        // Every name in the result must appear in both a and b.
        let names_a: HashSet<String> = a.iter().map(|c| c.name.clone()).collect();
        let names_b: HashSet<String> = b.iter().map(|c| c.name.clone()).collect();
        for cap in &result {
            assert!(names_a.contains(&cap.name), "result has cap not in a: {}", cap.name);
            assert!(names_b.contains(&cap.name), "result has cap not in b: {}", cap.name);
        }
        // Result is non-trivial (the test-data has overlap).
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn intersect_aws_sets_preserves_inv1_even_when_some_pairs_drop() {
        // mu-lwt regression: when same-name caps have both-Some session
        // policies, `AwsCapability::intersect` returns None (deferred) and
        // the cap is dropped from `intersect_aws_sets`. The set-level INV-1
        // property — result names ⊆ a.names AND result names ⊆ b.names —
        // must still hold. Catches a future regression that might "fall
        // back to keeping the original cap" on the None return path.
        let pol_a = serde_json::json!({"Statement": [{"Effect": "Allow"}]});
        let pol_b = serde_json::json!({"Statement": [{"Effect": "Deny"}]});
        let a = aws_set(&[
            aws_with_policy("aws.scout.readonly", pol_a),
            aws("aws.auditor.read"),
        ]);
        let b = aws_set(&[
            aws_with_policy("aws.scout.readonly", pol_b),
            aws("aws.auditor.read"),
        ]);
        let result = intersect_aws_sets(&a, &b);
        // scout was dropped (both-Some-Some deferred); auditor survives.
        let result_names: HashSet<String> = result.iter().map(|c| c.name.clone()).collect();
        assert_eq!(
            result_names,
            ["aws.auditor.read".to_string()].into_iter().collect::<HashSet<String>>(),
            "scout must be dropped (both-Some-Some deferred); auditor must survive"
        );
        // INV-1 at set level: every name in result is in BOTH a and b.
        let a_names: HashSet<String> = a.iter().map(|c| c.name.clone()).collect();
        let b_names: HashSet<String> = b.iter().map(|c| c.name.clone()).collect();
        for cap in &result {
            assert!(a_names.contains(&cap.name), "INV-1 violated: result name not in a: {}", cap.name);
            assert!(b_names.contains(&cap.name), "INV-1 violated: result name not in b: {}", cap.name);
        }
    }

    #[test]
    fn capability_attenuate_carries_aws_through_when_request_is_none() {
        // Parent has AWS caps; child requests no AWS narrowing → child
        // inherits parent's AWS as-is.
        let parent = Capability {
            aws: aws_set(&[aws("aws.scout.readonly")]),
            ..Default::default()
        };
        let attn = CapabilityAttenuations {
            aws: None,
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert_eq!(child.aws, aws_set(&[aws("aws.scout.readonly")]));
    }

    #[test]
    fn capability_attenuate_narrows_aws_to_request_intersection() {
        // Parent has {scout, sandbox}; child requests {scout, auditor}
        // → child gets {scout} (the intersection).
        let parent = Capability {
            aws: aws_set(&[aws("aws.scout.readonly"), aws("aws.sandbox.build")]),
            ..Default::default()
        };
        let attn = CapabilityAttenuations {
            aws: Some(vec![aws("aws.scout.readonly"), aws("aws.auditor.read")]),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert_eq!(child.aws, aws_set(&[aws("aws.scout.readonly")]));
    }

    #[test]
    fn capability_attenuate_cannot_widen_aws() {
        // Parent has empty AWS; child requests AWS caps → child still
        // has empty AWS (cannot widen).
        let parent = Capability::root();
        assert!(parent.aws.is_empty());
        let attn = CapabilityAttenuations {
            aws: Some(vec![aws("aws.scout.readonly")]),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert!(child.aws.is_empty(), "empty parent → empty child regardless of request");
    }

    #[test]
    fn capability_intersect_is_narrowing_only_inv1() {
        // INV-1 (load-bearing for this experiment): for any two
        // capabilities, intersect produces a capability ⊆ both inputs
        // on every axis — including the new AWS axis.
        let a = Capability {
            allowed_tools: Some(set(&["read", "grep", "edit"])),
            expires_at_unix_ms: Some(now_unix_ms() + 10_000),
            max_tool_calls_remaining: Some(20),
            autonomy: AutonomyCapability::Disallowed,
            aws: aws_set(&[aws("aws.scout.readonly"), aws("aws.sandbox.build")]),
        };
        let b = Capability {
            allowed_tools: Some(set(&["read", "edit", "bash"])),
            expires_at_unix_ms: Some(now_unix_ms() + 5_000),
            max_tool_calls_remaining: Some(10),
            autonomy: AutonomyCapability::Disallowed,
            aws: aws_set(&[aws("aws.scout.readonly"), aws("aws.auditor.read")]),
        };
        let r = a.intersect(&b);

        // allowed_tools ⊆ a.allowed_tools AND ⊆ b.allowed_tools
        let r_tools = r.allowed_tools.as_ref().expect("Some");
        let a_tools = a.allowed_tools.as_ref().unwrap();
        let b_tools = b.allowed_tools.as_ref().unwrap();
        for t in r_tools {
            assert!(a_tools.contains(t));
            assert!(b_tools.contains(t));
        }
        assert_eq!(r_tools, &set(&["read", "edit"]));

        // expiry ≤ both
        let r_exp = r.expires_at_unix_ms.unwrap();
        assert!(r_exp <= a.expires_at_unix_ms.unwrap());
        assert!(r_exp <= b.expires_at_unix_ms.unwrap());

        // budget ≤ both
        let r_budget = r.max_tool_calls_remaining.unwrap();
        assert!(r_budget <= a.max_tool_calls_remaining.unwrap());
        assert!(r_budget <= b.max_tool_calls_remaining.unwrap());

        // aws ⊆ both (by name)
        let a_names: HashSet<String> = a.aws.iter().map(|c| c.name.clone()).collect();
        let b_names: HashSet<String> = b.aws.iter().map(|c| c.name.clone()).collect();
        for cap in &r.aws {
            assert!(a_names.contains(&cap.name));
            assert!(b_names.contains(&cap.name));
        }
        // Concretely: only "aws.scout.readonly" is in both.
        assert_eq!(r.aws, aws_set(&[aws("aws.scout.readonly")]));
    }

    #[test]
    fn capability_intersect_none_axes_pick_the_some_side() {
        // When one side has None on an axis, intersect picks the
        // constraining (Some) side — None is the unrestricted identity.
        let unconstrained = Capability::root();
        let constrained = Capability {
            allowed_tools: Some(set(&["read"])),
            expires_at_unix_ms: Some(1_000_000),
            max_tool_calls_remaining: Some(5),
            autonomy: AutonomyCapability::Disallowed,
            aws: aws_set(&[aws("aws.scout.readonly")]),
        };
        let r = unconstrained.intersect(&constrained);
        // Every axis takes constrained's value (it's the narrower side).
        assert_eq!(r.allowed_tools, Some(set(&["read"])));
        assert_eq!(r.expires_at_unix_ms, Some(1_000_000));
        assert_eq!(r.max_tool_calls_remaining, Some(5));
        // AWS axis: unconstrained's empty set ∩ constrained's {scout}
        // is empty — intersect_aws_sets requires name match on BOTH
        // sides, and the empty side has no matches. This is the
        // "deny by default" property of HashSet intersection (correct
        // for the broker pattern: both grants must agree).
        assert!(r.aws.is_empty(), "intersect with empty side is empty");
    }

    #[test]
    fn capability_intersect_preserves_other_axes_when_aws_empty() {
        // Aws axis empty on both sides should not affect other axes'
        // intersect outcomes.
        let a = Capability {
            allowed_tools: Some(set(&["read"])),
            ..Default::default()
        };
        let b = Capability {
            allowed_tools: Some(set(&["grep"])),
            ..Default::default()
        };
        let r = a.intersect(&b);
        assert_eq!(r.allowed_tools, Some(HashSet::new()));
        assert!(r.aws.is_empty());
    }

    #[test]
    fn capability_round_trips_with_aws_via_serde() -> Result<(), serde_json::Error> {
        let cap = Capability {
            allowed_tools: Some(set(&["read"])),
            expires_at_unix_ms: None,
            max_tool_calls_remaining: None,
            autonomy: AutonomyCapability::default(),
            aws: aws_set(&[
                aws("aws.scout.readonly"),
                aws_with_policy(
                    "aws.sandbox.build",
                    serde_json::json!({"Statement": [{"Effect": "Allow"}]}),
                ),
            ]),
        };
        let v = serde_json::to_value(&cap)?;
        // aws field serializes as a JSON array of {name, session_policy?}.
        assert!(v["aws"].is_array());
        let decoded: Capability = serde_json::from_value(v)?;
        assert_eq!(decoded, cap);
        Ok(())
    }
}
