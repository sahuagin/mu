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

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};

use crate::agent::tool::SideEffects;
use t4c::{Effects, SessionConstraints};

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
    /// mu-f5o: typed AWS-capability grants the session holds. Unlike
    /// most other axes, empty set means no AWS access (not unrestricted).
    /// Multi-grant by design (a worker may hold `aws.scout.readonly` +
    /// `aws.sandbox.build` simultaneously), but at most one grant per
    /// capability name is valid; serde deserialization routes through
    /// `AwsCapability::try_from_iter` to enforce that invariant.
    /// Narrowing-only on `intersect` and `attenuate`: child cannot gain
    /// AWS caps the parent does not hold. See `AwsCapability` and
    /// `intersect_aws_sets`.
    #[serde(
        default,
        skip_serializing_if = "HashSet::is_empty",
        deserialize_with = "deserialize_aws_capability_set"
    )]
    pub aws: HashSet<AwsCapability>,
    /// mu-n25a: the session's side-effects CEILING — the most dangerous
    /// `SideEffects` class any tool this session dispatches may declare.
    /// A tool whose `policy.side_effects` ranks ABOVE this ceiling is
    /// refused at the dispatch choke point (`check_side_effects`), BEFORE
    /// the permission gate — so a `permission: Allow` tool cannot
    /// free-ride past a restrictive posture (the mu-usfj bug class).
    ///
    /// `None` = unrestricted (Execute-equivalent ceiling): every class is
    /// permitted. This is the BACK-COMPAT default — `Capability::root()`
    /// leaves it `None`, so no existing session is restricted unless it
    /// opts in. A read-only operator posture is `Some(SideEffects::ReadOnly)`.
    ///
    /// Narrowing-only on `attenuate`/`intersect`: a child can only LOWER
    /// the ceiling (toward ReadOnly), never raise it (INV: attenuation
    /// never widens). Wire-format is flat/snake_case to match the rest of
    /// the capability serde (`"max_side_effects": "read_only"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_side_effects: Option<SideEffects>,
    /// Config-plane access: whether the session may read and/or write
    /// session configuration over the `session.get_config` /
    /// `session.set_config` RPCs (the generic, key-addressed config
    /// message). `ReadWrite` is the unrestricted default (`root()`);
    /// narrowing-only on `attenuate`/`intersect` — a delegate can drop
    /// to `ReadOnly` or `None`, never gain write it wasn't granted.
    /// This is the capability axis the config message is gated on
    /// (the "eventually everything is gated by capabilities" axis,
    /// realized for the config plane).
    #[serde(default)]
    pub config: ConfigCapability,
}

/// Config-plane grant. Ordered by privilege: `None` < `ReadOnly` <
/// `ReadWrite`. Intersection takes the lower (narrower) of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigCapability {
    /// No config-plane access: both get and set are refused.
    None,
    /// May read config (`session.get_config`) but not change it.
    ReadOnly,
    /// May read and write config (`get_config` + `set_config`).
    ReadWrite,
}

impl Default for ConfigCapability {
    /// Unrestricted — `Capability::root()` (and thus an
    /// operator-created session) gets full config access.
    fn default() -> Self {
        ConfigCapability::ReadWrite
    }
}

impl ConfigCapability {
    /// Privilege rank for narrowing (`intersect` takes the min).
    fn rank(self) -> u8 {
        match self {
            ConfigCapability::None => 0,
            ConfigCapability::ReadOnly => 1,
            ConfigCapability::ReadWrite => 2,
        }
    }

    /// The narrower (lower-privilege) of two grants — narrowing-only.
    pub fn intersect(self, other: Self) -> Self {
        if self.rank() <= other.rank() {
            self
        } else {
            other
        }
    }

    /// May the holder read config?
    pub fn can_read(self) -> bool {
        self.rank() >= ConfigCapability::ReadOnly.rank()
    }

    /// May the holder write config?
    pub fn can_write(self) -> bool {
        self == ConfigCapability::ReadWrite
    }
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
#[derive(Default)]
pub enum AutonomyCapability {
    /// Autonomy disallowed. session.start_autonomous is rejected
    /// with `CapabilityCheck::DeniedAutonomyDisallowed`.
    #[default]
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
/// both fields. A raw `HashSet<AwsCapability>` can therefore contain two
/// caps with the same name and different policies as distinct elements.
/// The semantic invariant is stricter: one cap per name. Use
/// `AwsCapability::try_from_iter` (and `Capability` serde) to enforce it
/// when accepting externally supplied capability collections; `intersect`
/// and `attenuate` preserve it while narrowing.
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("duplicate AWS capability `{name}` with different session policies")]
pub struct DuplicateAwsCapabilityError {
    pub name: String,
    pub prior_policy: Option<serde_json::Value>,
    pub new_policy: Option<serde_json::Value>,
}

impl AwsCapability {
    /// Collect AWS capabilities while enforcing Mu's semantic invariant:
    /// at most one capability per name. Exact duplicates are harmless and
    /// deduplicated; same-name entries with different `session_policy`
    /// values fail closed.
    pub fn try_from_iter<I>(caps: I) -> Result<HashSet<Self>, DuplicateAwsCapabilityError>
    where
        I: IntoIterator<Item = Self>,
    {
        let mut by_name: HashMap<String, Self> = HashMap::new();
        for cap in caps {
            match by_name.get(&cap.name) {
                Some(prior) if prior != &cap => {
                    return Err(DuplicateAwsCapabilityError {
                        name: cap.name,
                        prior_policy: prior.session_policy.clone(),
                        new_policy: cap.session_policy,
                    });
                }
                Some(_) => {}
                None => {
                    by_name.insert(cap.name.clone(), cap);
                }
            }
        }
        Ok(by_name.into_values().collect())
    }

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
fn deserialize_aws_capability_set<'de, D>(
    deserializer: D,
) -> Result<HashSet<AwsCapability>, D::Error>
where
    D: Deserializer<'de>,
{
    let caps = Vec::<AwsCapability>::deserialize(deserializer)?;
    AwsCapability::try_from_iter(caps).map_err(serde::de::Error::custom)
}

/// Intersect two side-effects ceilings. `None` = unrestricted (Execute-
/// equivalent), so it is the identity element: `None ∩ X = X`. Two `Some`
/// ceilings collapse to the NARROWER (lower-rank) one — the more
/// restrictive posture always wins, which is the narrowing-only invariant
/// (a child can lower the ceiling but never raise it). (mu-n25a)
fn intersect_side_effects(a: Option<SideEffects>, b: Option<SideEffects>) -> Option<SideEffects> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(if x.rank() <= y.rank() { x } else { y }),
    }
}

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

/// Map the coarse linear `max_side_effects` ceiling onto t4c's per-axis
/// [`SessionConstraints`] (mu-8stm.2 phase 1). The coarse ceiling is a TOTAL
/// order while constraints are INDEPENDENT axes; this mapping is exact for the
/// tool classes mu actually declares (ReadOnly/Mutating/Execute) across every
/// ceiling — proven by `structured_constraints_match_linear_ceiling_*` — and is
/// the conservative reading for the unused middle classes. `ReadOnly` forbids
/// everything above a read; `Execute` forbids nothing.
pub fn constraints_from_max_side_effects(ceiling: SideEffects) -> SessionConstraints {
    match ceiling {
        SideEffects::ReadOnly => SessionConstraints {
            no_writes: true,
            no_vcs: true,
            no_network: true,
            no_spend: true,
            no_process: true,
        },
        // Local writes/vcs allowed; anything reaching out (network/spend/
        // process) is not.
        SideEffects::Mutating | SideEffects::Destructive => SessionConstraints {
            no_writes: false,
            no_vcs: false,
            no_network: true,
            no_spend: true,
            no_process: true,
        },
        // Network allowed; spawning processes / spend is not.
        SideEffects::External => SessionConstraints {
            no_writes: false,
            no_vcs: false,
            no_network: false,
            no_spend: true,
            no_process: true,
        },
        // Execute ceiling = unconstrained.
        SideEffects::Execute => SessionConstraints::default(),
    }
}

impl Capability {
    /// The root capability — unrestricted on every axis. Used for
    /// sessions created via `create_session` (no delegation chain).
    pub fn root() -> Self {
        Self::default()
    }

    /// The most-restrictive reasonable capability — the FAIL-CLOSED
    /// baseline. No tools may be invoked (`allowed_tools = Some(empty)`),
    /// the side-effects ceiling is pinned to `ReadOnly`, autonomy is
    /// `Disallowed`, and the session holds no AWS grants. Every axis is
    /// at its narrowest.
    ///
    /// mu-mh4 / mu-nqn5: `session.resume` uses this as the resumed
    /// session's baseline when the predecessor's live capability handle
    /// is gone (the NORMAL cold/rehydrated case — a dead session has no
    /// in-memory capability). Falling back to `root()` there would let a
    /// resume WIDEN privileges (attenuation-only-narrows violation, since
    /// attenuate(root, ...) ⊇ attenuate(restricted_predecessor, ...)).
    /// Failing closed preserves the invariant until capability
    /// persistence (mu-nqn5) lets us recover the predecessor's actual
    /// capability from its log. The operator can pass explicit
    /// `attenuations` to NARROW further, never to widen past this floor.
    pub fn read_only() -> Self {
        Self {
            allowed_tools: Some(HashSet::new()),
            expires_at_unix_ms: None,
            max_tool_calls_remaining: None,
            autonomy: AutonomyCapability::Disallowed,
            aws: HashSet::new(),
            max_side_effects: Some(SideEffects::ReadOnly),
            // Fail-closed baseline may observe config but not mutate it.
            config: ConfigCapability::ReadOnly,
        }
    }

    /// The session's effective per-axis [`SessionConstraints`], or `None`
    /// (unconstrained) — the single posture the dispatch gate consults in
    /// phase 1b (mu-8stm.2). Derived from the coarse `max_side_effects`
    /// ceiling; a future additive per-axis field would take precedence here.
    pub fn effective_constraints(&self) -> Option<SessionConstraints> {
        self.max_side_effects.map(constraints_from_max_side_effects)
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

        let max_tool_calls_remaining =
            match (self.max_tool_calls_remaining, attenuations.max_tool_calls) {
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

        // mu-n25a: side-effects ceiling. Narrower (lower-rank) wins; a
        // child can only LOWER the ceiling, never raise it. None on the
        // request = no narrowing requested → child inherits parent's
        // ceiling. None on the parent = unrestricted parent → child gets
        // whatever it requested. Either way result ⊆ parent (INV).
        let max_side_effects =
            intersect_side_effects(self.max_side_effects, attenuations.max_side_effects);

        Capability {
            allowed_tools,
            expires_at_unix_ms,
            max_tool_calls_remaining,
            autonomy,
            aws,
            max_side_effects,
            // No config-narrowing request shape exists yet; a delegate
            // inherits the parent's grant unchanged (⊆ parent holds).
            config: self.config,
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

        let max_tool_calls_remaining = match (
            self.max_tool_calls_remaining,
            other.max_tool_calls_remaining,
        ) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(a),
            (Some(a), Some(b)) => Some(a.min(b)),
        };

        let autonomy = self.autonomy.intersect(&other.autonomy);
        let aws = intersect_aws_sets(&self.aws, &other.aws);
        let max_side_effects =
            intersect_side_effects(self.max_side_effects, other.max_side_effects);
        let config = self.config.intersect(other.config);

        Capability {
            allowed_tools,
            expires_at_unix_ms,
            max_tool_calls_remaining,
            autonomy,
            aws,
            max_side_effects,
            config,
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

    /// mu-n25a: does a tool declaring `declared` side-effects fit under
    /// this session's `max_side_effects` ceiling? Checked at the dispatch
    /// choke point alongside `check_allow`. `None` ceiling = unrestricted
    /// → always `Allowed` (back-compat default). Otherwise the tool is
    /// refused iff its declared class ranks ABOVE the ceiling.
    ///
    /// This is the structural close of the SELF-CLASSIFIED-AUTHORITY
    /// class: a tool's `permission: Allow` no longer lets it free-ride
    /// past a restrictive session posture — its declared danger is
    /// checked against the posture FIRST.
    pub fn check_side_effects(&self, declared: SideEffects) -> CapabilityCheck {
        match self.max_side_effects {
            None => CapabilityCheck::Allowed,
            Some(ceiling) if declared.within(ceiling) => CapabilityCheck::Allowed,
            Some(ceiling) => CapabilityCheck::DeniedSideEffectsExceeded { declared, ceiling },
        }
    }

    /// mu-8stm.2 (phase 1b): the STRUCTURED appropriateness check — the
    /// canonical replacement for `check_side_effects` at the dispatch gate.
    /// Tests the tool's canonical [`Effects`] against the session's per-axis
    /// [`SessionConstraints`] via t4c's `disallowed_by` — the SAME predicate
    /// the discovery surface uses, so a tool's gate refusal and its
    /// `allowed_by_session=false` reason are identical (single source of
    /// truth). `None` effects (unannotated) FAIL CLOSED under any active
    /// posture; an unconstrained session (no ceiling) allows everything
    /// (back-compat root default).
    pub fn check_effects(&self, effects: Option<&Effects>) -> CapabilityCheck {
        match self.effective_constraints() {
            None => CapabilityCheck::Allowed,
            Some(constraints) => match effects {
                Some(e) => match e.disallowed_by(&constraints) {
                    Some(reason) => CapabilityCheck::DeniedInappropriate { reason },
                    None => CapabilityCheck::Allowed,
                },
                None => CapabilityCheck::DeniedInappropriate {
                    reason: "tool has unclassified (unannotated) effects; refused under this \
                             session's posture — classify it before use"
                        .to_string(),
                },
            },
        }
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
    /// mu-n25a: requested side-effects ceiling for the delegate. `None`
    /// = no narrowing requested → child inherits parent's ceiling.
    /// `Some(x)` = child ceiling is `min(parent, x)` by danger rank (a
    /// child can only LOWER the ceiling, never raise it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_side_effects: Option<SideEffects>,
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
    /// mu-n25a: a tool's declared `side_effects` exceed the session's
    /// `max_side_effects` ceiling. `declared` is the tool's class;
    /// `ceiling` is the posture it overran. Surfaced so the refusal is
    /// legible ("tool X is Execute; this session is capped at ReadOnly").
    DeniedSideEffectsExceeded {
        declared: SideEffects,
        ceiling: SideEffects,
    },
    /// mu-8stm.2 (phase 1b): the tool's structured [`t4c::Effects`] are
    /// inappropriate for the session's per-axis posture — or are unannotated
    /// and refused fail-closed. `reason` is the legible, axis-named
    /// explanation (from `Effects::disallowed_by`, the SAME predicate the
    /// discovery surface uses, or the unclassified-fail-closed message).
    DeniedInappropriate {
        reason: String,
    },
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
            CapabilityCheck::DeniedSideEffectsExceeded { .. } => {
                "tool's declared side-effects exceed the session's max_side_effects ceiling"
            }
            CapabilityCheck::DeniedInappropriate { .. } => {
                "tool's effects are inappropriate for the session's posture (or unannotated)"
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
    fn config_capability_default_and_baseline() {
        // root() is unrestricted: full config access.
        assert_eq!(Capability::root().config, ConfigCapability::ReadWrite);
        assert!(Capability::root().config.can_read());
        assert!(Capability::root().config.can_write());
        // read_only() baseline observes but cannot mutate.
        assert_eq!(Capability::read_only().config, ConfigCapability::ReadOnly);
        assert!(Capability::read_only().config.can_read());
        assert!(!Capability::read_only().config.can_write());
        // None denies both.
        assert!(!ConfigCapability::None.can_read());
        assert!(!ConfigCapability::None.can_write());
    }

    #[test]
    fn config_capability_intersect_narrows_only() {
        use ConfigCapability::*;
        // min by privilege rank; order-independent.
        assert_eq!(ReadWrite.intersect(ReadOnly), ReadOnly);
        assert_eq!(ReadOnly.intersect(ReadWrite), ReadOnly);
        assert_eq!(ReadOnly.intersect(None), None);
        assert_eq!(ReadWrite.intersect(ReadWrite), ReadWrite);
        // capability intersect carries the narrower config grant.
        let parent = Capability::root(); // ReadWrite
        let mut child = Capability::root();
        child.config = ConfigCapability::ReadOnly;
        assert_eq!(parent.intersect(&child).config, ConfigCapability::ReadOnly);
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
            max_side_effects: None,
            config: ConfigCapability::ReadWrite,
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
        assert_eq!(
            AutonomyCapability::default(),
            AutonomyCapability::Disallowed
        );
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
        assert!(
            v.get("session_policy").is_none(),
            "None policy should be skipped"
        );
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
    fn aws_try_from_iter_rejects_same_name_different_policy() {
        let err = AwsCapability::try_from_iter([
            aws("aws.scout.readonly"),
            aws_with_policy(
                "aws.scout.readonly",
                serde_json::json!({"Statement": [{"Effect": "Deny"}]}),
            ),
        ])
        .expect_err("same name with different policy must fail");

        assert_eq!(err.name, "aws.scout.readonly");
        assert!(err.prior_policy.is_none());
        assert!(err.new_policy.is_some());
    }

    #[test]
    fn aws_try_from_iter_deduplicates_exact_duplicates() {
        let cap = aws_with_policy(
            "aws.scout.readonly",
            serde_json::json!({"Statement": [{"Effect": "Deny"}]}),
        );
        let set = AwsCapability::try_from_iter([cap.clone(), cap.clone()])
            .expect("exact duplicates are harmless");

        assert_eq!(set.len(), 1);
        assert!(set.contains(&cap));
    }

    #[test]
    fn capability_deserialize_rejects_duplicate_aws_name_different_policy() {
        let err = serde_json::from_value::<Capability>(serde_json::json!({
            "aws": [
                {"name": "aws.scout.readonly"},
                {
                    "name": "aws.scout.readonly",
                    "session_policy": {"Statement": [{"Effect": "Deny"}]}
                }
            ]
        }))
        .expect_err("serde must enforce one-cap-per-name invariant");

        assert!(err.to_string().contains("duplicate AWS capability"));
        assert!(err.to_string().contains("aws.scout.readonly"));
    }

    #[test]
    fn capability_deserialize_deduplicates_exact_duplicate_aws_caps() {
        let decoded: Capability = serde_json::from_value(serde_json::json!({
            "aws": [
                {"name": "aws.scout.readonly"},
                {"name": "aws.scout.readonly"}
            ]
        }))
        .expect("exact duplicates are deduplicated");

        assert_eq!(decoded.aws.len(), 1);
        assert!(decoded.aws.contains(&aws("aws.scout.readonly")));
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
        let r2 = with_pol
            .intersect(&bare)
            .expect("same name → Some (symmetric)");
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
            assert!(
                names_a.contains(&cap.name),
                "result has cap not in a: {}",
                cap.name
            );
            assert!(
                names_b.contains(&cap.name),
                "result has cap not in b: {}",
                cap.name
            );
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
            ["aws.auditor.read".to_string()]
                .into_iter()
                .collect::<HashSet<String>>(),
            "scout must be dropped (both-Some-Some deferred); auditor must survive"
        );
        // INV-1 at set level: every name in result is in BOTH a and b.
        let a_names: HashSet<String> = a.iter().map(|c| c.name.clone()).collect();
        let b_names: HashSet<String> = b.iter().map(|c| c.name.clone()).collect();
        for cap in &result {
            assert!(
                a_names.contains(&cap.name),
                "INV-1 violated: result name not in a: {}",
                cap.name
            );
            assert!(
                b_names.contains(&cap.name),
                "INV-1 violated: result name not in b: {}",
                cap.name
            );
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
        assert!(
            child.aws.is_empty(),
            "empty parent → empty child regardless of request"
        );
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
            max_side_effects: None,
            config: ConfigCapability::ReadWrite,
        };
        let b = Capability {
            allowed_tools: Some(set(&["read", "edit", "bash"])),
            expires_at_unix_ms: Some(now_unix_ms() + 5_000),
            max_tool_calls_remaining: Some(10),
            autonomy: AutonomyCapability::Disallowed,
            aws: aws_set(&[aws("aws.scout.readonly"), aws("aws.auditor.read")]),
            max_side_effects: None,
            config: ConfigCapability::ReadWrite,
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
            max_side_effects: None,
            config: ConfigCapability::ReadWrite,
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

    // ── mu-n25a: max_side_effects ceiling ────────────────────────

    #[test]
    fn side_effects_rank_is_total_and_ordered() {
        // Endpoints are load-bearing: ReadOnly is the minimum, Execute
        // the maximum (it subsumes every class).
        assert!(SideEffects::ReadOnly.rank() < SideEffects::Mutating.rank());
        assert!(SideEffects::Mutating.rank() < SideEffects::External.rank());
        assert!(SideEffects::External.rank() < SideEffects::Destructive.rank());
        assert!(SideEffects::Destructive.rank() < SideEffects::Execute.rank());
        // Ord agrees with rank.
        assert!(SideEffects::ReadOnly < SideEffects::Execute);
        // `within`: a class fits under itself and under anything higher.
        assert!(SideEffects::ReadOnly.within(SideEffects::ReadOnly));
        assert!(SideEffects::Mutating.within(SideEffects::Execute));
        assert!(!SideEffects::Execute.within(SideEffects::ReadOnly));
        assert!(!SideEffects::Destructive.within(SideEffects::Mutating));
    }

    #[test]
    fn root_capability_has_no_side_effects_ceiling() {
        // Back-compat: root is unrestricted on this axis, so existing
        // sessions are never refused by the side-effects gate.
        let cap = Capability::root();
        assert_eq!(cap.max_side_effects, None);
        assert!(cap.check_side_effects(SideEffects::Execute).is_allowed());
        assert!(cap
            .check_side_effects(SideEffects::Destructive)
            .is_allowed());
    }

    #[test]
    fn check_side_effects_refuses_above_ceiling() {
        let cap = Capability {
            max_side_effects: Some(SideEffects::ReadOnly),
            ..Default::default()
        };
        // At/below ceiling allowed.
        assert!(cap.check_side_effects(SideEffects::ReadOnly).is_allowed());
        // Above ceiling refused, with both classes legible.
        match cap.check_side_effects(SideEffects::Execute) {
            CapabilityCheck::DeniedSideEffectsExceeded { declared, ceiling } => {
                assert_eq!(declared, SideEffects::Execute);
                assert_eq!(ceiling, SideEffects::ReadOnly);
            }
            other => panic!("expected DeniedSideEffectsExceeded, got {other:?}"),
        }
        assert!(!cap.check_side_effects(SideEffects::Mutating).is_allowed());
    }

    #[test]
    fn attenuate_intersects_side_effects_narrower_wins() {
        // Parent caps at Destructive; child requests ReadOnly → child
        // gets the narrower (ReadOnly).
        let parent = Capability {
            max_side_effects: Some(SideEffects::Destructive),
            ..Default::default()
        };
        let attn = CapabilityAttenuations {
            max_side_effects: Some(SideEffects::ReadOnly),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert_eq!(child.max_side_effects, Some(SideEffects::ReadOnly));
    }

    #[test]
    fn attenuate_cannot_widen_side_effects() {
        // INV: attenuation never widens. Parent caps at ReadOnly; child
        // requests Execute → child stays at ReadOnly (the narrower).
        let parent = Capability {
            max_side_effects: Some(SideEffects::ReadOnly),
            ..Default::default()
        };
        let attn = CapabilityAttenuations {
            max_side_effects: Some(SideEffects::Execute),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert_eq!(
            child.max_side_effects,
            Some(SideEffects::ReadOnly),
            "child cannot raise the ceiling above the parent's"
        );
    }

    #[test]
    fn attenuate_unrestricted_parent_takes_child_ceiling() {
        // None (unrestricted) parent + Some child request → child gets
        // its requested ceiling (None is the identity).
        let parent = Capability::root();
        let attn = CapabilityAttenuations {
            max_side_effects: Some(SideEffects::Mutating),
            ..Default::default()
        };
        let child = parent.attenuate(&attn);
        assert_eq!(child.max_side_effects, Some(SideEffects::Mutating));
    }

    #[test]
    fn attenuate_no_request_inherits_parent_ceiling() {
        let parent = Capability {
            max_side_effects: Some(SideEffects::Mutating),
            ..Default::default()
        };
        let attn = CapabilityAttenuations::default();
        let child = parent.attenuate(&attn);
        assert_eq!(child.max_side_effects, Some(SideEffects::Mutating));
    }

    #[test]
    fn intersect_side_effects_narrower_wins() {
        let a = Capability {
            max_side_effects: Some(SideEffects::External),
            ..Default::default()
        };
        let b = Capability {
            max_side_effects: Some(SideEffects::Mutating),
            ..Default::default()
        };
        assert_eq!(
            a.intersect(&b).max_side_effects,
            Some(SideEffects::Mutating)
        );
        // None side is the identity.
        let r = a.intersect(&Capability::root());
        assert_eq!(r.max_side_effects, Some(SideEffects::External));
    }

    #[test]
    fn max_side_effects_round_trips_via_serde() -> Result<(), serde_json::Error> {
        let cap = Capability {
            max_side_effects: Some(SideEffects::ReadOnly),
            ..Default::default()
        };
        let v = serde_json::to_value(&cap)?;
        // Flat, snake_case wire form.
        assert_eq!(v["max_side_effects"], "read_only");
        let decoded: Capability = serde_json::from_value(v)?;
        assert_eq!(decoded, cap);

        // None is omitted from the wire entirely (back-compat).
        let unrestricted = Capability::root();
        let v = serde_json::to_value(&unrestricted)?;
        assert!(
            v.get("max_side_effects").is_none(),
            "None ceiling must be skipped on the wire"
        );
        Ok(())
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
            max_side_effects: None,
            config: ConfigCapability::ReadWrite,
        };
        let v = serde_json::to_value(&cap)?;
        // aws field serializes as a JSON array of {name, session_policy?}.
        assert!(v["aws"].is_array());
        let decoded: Capability = serde_json::from_value(v)?;
        assert_eq!(decoded, cap);
        Ok(())
    }

    // mu-8stm.2 phase 1a: the structured per-axis check must reproduce the
    // legacy linear ceiling for the tool classes mu actually declares, across
    // every ceiling. This is the equivalence proof that licenses swapping the
    // dispatch gate (1b) from `check_side_effects` to `disallowed_by` with no
    // behavior change.
    #[test]
    fn structured_constraints_match_linear_ceiling_for_declared_tool_classes() {
        use SideEffects::*;
        // The classes tools actually declare today (read/ls/glob/grep/... are
        // ReadOnly; write/edit are Mutating; bash/watch/spawn_worker are
        // Execute). No tool declares External or Destructive.
        let tool_classes = [ReadOnly, Mutating, Execute];
        let ceilings = [ReadOnly, Mutating, External, Destructive, Execute];
        for &tool in &tool_classes {
            for &ceiling in &ceilings {
                let c = constraints_from_max_side_effects(ceiling);
                let structured_ok = tool.effects().disallowed_by(&c).is_none();
                assert_eq!(
                    structured_ok,
                    tool.within(ceiling),
                    "tool {tool:?} under ceiling {ceiling:?}: structured allowed={structured_ok}, \
                     linear within={}",
                    tool.within(ceiling)
                );
            }
        }
    }

    // mu-8stm.2: KNOWN, ACCEPTED divergence. A total order (the linear ladder)
    // cannot be reproduced by independent per-axis constraints in the middle,
    // and `Effects` has no irreversibility axis — so a `Destructive` *tool*
    // (none exist today) reads as a plain write and is allowed under a
    // `Mutating` ceiling where the linear ladder refused it. Pinned here so the
    // gap is visible, not silent. Resolution for the capability circle-back:
    // add a `destructive` axis to t4c `Effects`, or specify per-axis session
    // constraints directly, when a Destructive tool is introduced.
    #[test]
    fn structured_model_diverges_from_linear_for_unused_middle_classes() {
        use SideEffects::*;
        let c = constraints_from_max_side_effects(Mutating);
        assert!(
            Destructive.effects().disallowed_by(&c).is_none(),
            "structured: Destructive collapses to a write, allowed under Mutating"
        );
        assert!(
            !Destructive.within(Mutating),
            "linear: Destructive ranks above Mutating, refused"
        );
    }

    #[test]
    fn effective_constraints_precedence() {
        // No ceiling -> unconstrained (back-compat root default).
        assert!(Capability::root().effective_constraints().is_none());
        // ReadOnly ceiling -> all axes constrained.
        let mut ro = Capability::root();
        ro.max_side_effects = Some(SideEffects::ReadOnly);
        let c = ro.effective_constraints().expect("constrained");
        assert!(c.no_writes && c.no_vcs && c.no_network && c.no_spend && c.no_process);
    }

    // mu-8stm.2 phase 1b: the structured gate predicate. Unconstrained sessions
    // allow everything (back-compat); a constrained posture refuses tools whose
    // effects exceed it AND fails closed on unannotated (None) effects.
    #[test]
    fn check_effects_enforces_constraints_and_fails_closed_on_none() {
        use SideEffects::*;
        // Unconstrained (root) — allows everything, even unannotated effects.
        let root = Capability::root();
        assert!(root.check_effects(Some(&Execute.effects())).is_allowed());
        assert!(root.check_effects(None).is_allowed());

        // Read-only posture.
        let mut ro = Capability::root();
        ro.max_side_effects = Some(ReadOnly);
        assert!(ro.check_effects(Some(&ReadOnly.effects())).is_allowed());
        assert!(matches!(
            ro.check_effects(Some(&Execute.effects())),
            CapabilityCheck::DeniedInappropriate { .. }
        ));
        // None-closed: unannotated effects refused under an active posture.
        assert!(matches!(
            ro.check_effects(None),
            CapabilityCheck::DeniedInappropriate { .. }
        ));
    }

    // mu-8stm.2 (1b, review: gpt-5.5): the gate consults derived_effects(), so an
    // AWS-gated tool's network/spend reach is honored — a read-only posture
    // refuses it even though its declared class is ReadOnly, and even if the AWS
    // grant is held. Pins gate<->discovery agreement (both use derived_effects)
    // and closes the fail-open where a grant let a network/spend tool slip a
    // no-network/no-spend posture.
    #[test]
    fn aws_gated_tool_refused_under_no_network_posture() {
        use crate::agent::tool::{PermissionLevel, RetryPolicy, ToolPolicy};
        let policy = ToolPolicy {
            side_effects: SideEffects::ReadOnly,
            permission: PermissionLevel::Allow,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: Some("aws.scout.readonly".to_string()),
            idempotent: true,
        };
        // derived_effects() adds network+spend for the AWS grant.
        let eff = policy.derived_effects();
        assert!(eff.network && eff.spend);
        // A read-only posture (no_network/no_spend) refuses it via the gate
        // predicate — not allowed just because its declared class is ReadOnly.
        let mut ro = Capability::root();
        ro.max_side_effects = Some(SideEffects::ReadOnly);
        assert!(matches!(
            ro.check_effects(Some(&eff)),
            CapabilityCheck::DeniedInappropriate { .. }
        ));
    }
}
