//! A session-level spend ceiling: a dollar limit the daemon enforces at the
//! model-call boundary, OFF unless configured or asked for on the command
//! line. Spec `specs/mu-048-spend-ceiling.md`; bead mu-z94um.
//!
//! Two rules, load-bearing (operator, 2026-09-18): a limit you can hit and
//! cannot turn off is a bug, so nothing here is on by default and no number
//! is compiled in; and the caller that spends is the one that arms it —
//! `[spend]` in config is the standing setting, `mu ask --max-usd` the
//! runtime override.
//!
//! The pieces: [`SpendCeiling`] (what was asked for, validated),
//! [`SpendMeter`] (what a session has spent so far, priced per request under
//! the card in force at that request, the way mu-047's log projection
//! prices a call), and [`SpendConfig`] (the `[spend]` section). The agent loop
//! records each assistant message's usage on the meter and, before queueing
//! the next model call, ends the ask with `StopReason::BudgetCap` when the
//! meter says the ceiling is reached. The call that crosses the line is
//! allowed to finish — a request cannot be priced before it runs — so the
//! overshoot is bounded by one request.

use serde::{Deserialize, Serialize};

use crate::agent::types::Usage;
use crate::pricing::{self, ModelPricing};

/// Which lanes a ceiling counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendLanes {
    /// Metered lanes only (openrouter, anthropic_api, openai_api): money.
    #[default]
    Billed,
    /// Metered lanes AND the subscription lanes' API-equivalent figure — the
    /// bound that protects the rest of a subscription month.
    All,
}

/// A validated ceiling: `max_usd` is finite and > 0. `0` is refused, not
/// "unlimited" — to run unlimited, arm nothing. The fields are private and
/// deserialization goes through [`SpendCeiling::new`], so a ceiling that
/// exists is one that can be reached: `nan` or `inf` in a request would
/// otherwise arm a ceiling `reached()` never reports.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SpendCeilingWire")]
pub struct SpendCeiling {
    max_usd: f64,
    lanes: SpendLanes,
}

/// The wire shape of a ceiling, validated into [`SpendCeiling`] on the way in.
#[derive(Deserialize)]
struct SpendCeilingWire {
    max_usd: f64,
    #[serde(default)]
    lanes: SpendLanes,
}

impl TryFrom<SpendCeilingWire> for SpendCeiling {
    type Error = SpendCeilingError;
    fn try_from(w: SpendCeilingWire) -> Result<Self, Self::Error> {
        SpendCeiling::new(w.max_usd, w.lanes)
    }
}

/// Why a ceiling could not be built or armed. Each is the caller's to
/// surface verbatim: an invalid ceiling is refused, never silently
/// widened or dropped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpendCeilingError {
    #[error("spend ceiling must be a finite amount greater than zero (got {0}); to run unlimited, set no ceiling")]
    InvalidMax(String),
    #[error("spend ceiling cannot be metered on {provider_kind}/{model}: no rate card in the model catalog (models.toml) — add one, or run without a ceiling")]
    Unmeterable {
        provider_kind: String,
        model: String,
    },
    #[error("spend ceiling is armed but {calls} call(s) on this session were not accounted for by the provider (no usage reported); the session's spend is unknown and the ceiling cannot bound it — start a new session, or run without a ceiling")]
    Unaccounted { calls: u32 },
    #[error("spend ceiling is armed on a continued session whose log does not establish its spend (unknown, or a base-rate floor); the ceiling cannot bound it — start a new session, or run without a ceiling")]
    UnknownHistory,
    #[error("spend ceiling is armed on a continued session without its spend history (the meter was not restored from the session's log); refusing to grant a fresh allowance — restore the meter from the log projection, or run without a ceiling")]
    Unrestored,
}

impl SpendCeiling {
    pub fn new(max_usd: f64, lanes: SpendLanes) -> Result<Self, SpendCeilingError> {
        if !(max_usd.is_finite() && max_usd > 0.0) {
            return Err(SpendCeilingError::InvalidMax(format!("{max_usd}")));
        }
        Ok(Self { max_usd, lanes })
    }

    pub fn max_usd(&self) -> f64 {
        self.max_usd
    }

    pub fn lanes(&self) -> SpendLanes {
        self.lanes
    }

    /// The card a ceiling would meter (provider, model) with, or the reason
    /// it cannot: a ceiling on an unpriceable lane refuses to arm rather
    /// than pretending to bound anything. `priced = false` lanes (a
    /// self-hosted server) are unmeterable by declaration.
    pub fn card_for(
        catalog: &crate::model_catalog::ModelCatalogConfig,
        provider_kind: &str,
        model: &str,
    ) -> Result<ModelPricing, SpendCeilingError> {
        pricing::for_model_in(catalog, provider_kind, model).ok_or_else(|| {
            SpendCeilingError::Unmeterable {
                provider_kind: provider_kind.to_string(),
                model: model.to_string(),
            }
        })
    }
}

/// What a session has spent against its ceiling: priced per request under
/// the card in force at that request. `record` is called once per
/// assistant message with usage; `reached` is asked before each model
/// call. The meter lives as long as the session: a `/clear` empties the
/// context, it refunds nothing, so a session that reached its ceiling
/// stays stopped until a new session arms a new ceiling.
///
/// The meter fails closed. A call the provider did not account for
/// (`usage: None`) is money the meter cannot see; once one has happened the
/// session's spend is unknown and no later call is allowed under the
/// ceiling — [`SpendMeter::preflight`] refuses every subsequent call, not
/// only the ask the unaccounted call ended. Without that, each new ask
/// would run one unmetered call and the ceiling would bound nothing.
///
/// The meter is a projection of the session's log, never a second source
/// of truth (invariant 1): a fresh session starts it at zero
/// ([`SpendMeter::new`]) and every metered call is also an assistant event
/// in the log; a CONTINUED session (a resume: the loop starts with history)
/// restores it from the log's cost projection
/// ([`SpendMeter::from_projection`]) — an unknown projection locks it. A
/// continuation that arms a fresh meter instead is refused by the loop
/// ([`SpendMeter::lock_unrestored`]): a resumed session never gets a fresh
/// allowance by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendMeter {
    ceiling: SpendCeiling,
    spent_usd: f64,
    /// Requests priced into `spent_usd` by this meter (a restored figure
    /// carries no call count).
    counted: u32,
    /// Requests on a lane the ceiling does not count (subscription usage
    /// under `lanes = "billed"`), with their API-equivalent figure, so a
    /// display can still say what the session would have cost.
    uncounted_usd: f64,
    /// Requests that ran but came back without usage: unknown money.
    unaccounted: u32,
    /// Built from a log projection (a continuation), not from zero.
    restored: bool,
    /// A lock no later call clears: the spend cannot be established.
    lock: Option<SpendLock>,
}

/// Why a meter refuses every call for the rest of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpendLock {
    /// A continuation's log prices as unknown.
    UnknownHistory,
    /// A continuation armed a fresh meter instead of restoring one.
    Unrestored,
}

impl SpendMeter {
    /// A meter for a FRESH session: nothing spent yet.
    pub fn new(ceiling: SpendCeiling) -> Self {
        Self {
            ceiling,
            spent_usd: 0.0,
            counted: 0,
            uncounted_usd: 0.0,
            unaccounted: 0,
            restored: false,
            lock: None,
        }
    }

    /// A meter for a CONTINUED session, restored from the session log's
    /// cost projection (`SessionEventLog::cost_projection`): what the log
    /// says was spent is what the ceiling has left to give. Only an exact
    /// figure (`PerCall`) restores; `Unknown` locks the meter, and so does
    /// `BaseRate` — a floor on a tiered card, and a ceiling restored from
    /// a floor could overshoot in the unsafe direction. Under
    /// `lanes = "billed"` an all-subscription history counts as nothing
    /// spent (its figure is API-equivalent, shown but not bounded) and a
    /// mixed one counts in full — the projection cannot split it, and
    /// over-counting stops early, never late.
    pub fn from_projection(
        ceiling: SpendCeiling,
        projection: &crate::session_cost::CostProjection,
    ) -> Self {
        use crate::pricing::{CostBasis, CostLane};
        let mut meter = Self::new(ceiling);
        meter.restored = true;
        let session = &projection.session;
        match session.basis {
            CostBasis::Unknown | CostBasis::BaseRate => {
                meter.lock = Some(SpendLock::UnknownHistory)
            }
            CostBasis::PerCall => match (ceiling.lanes, session.lane) {
                (SpendLanes::Billed, CostLane::ApiEquivalent) => {
                    meter.uncounted_usd = session.usd;
                }
                _ => meter.spent_usd = session.usd,
            },
        }
        meter
    }

    /// Whether this meter was restored from a log projection (a
    /// continuation) rather than started from zero.
    pub fn restored(&self) -> bool {
        self.restored
    }

    /// The loop's guard for a continuation that armed a fresh meter: from
    /// here on `preflight` refuses, so a resumed session cannot be granted
    /// a fresh allowance by a caller that forgot to restore.
    pub fn lock_unrestored(&mut self) {
        self.lock = Some(SpendLock::Unrestored);
    }

    /// Whether the next call to (provider, model) may run under this
    /// ceiling, and the card it will be metered with if so. Refuses when
    /// an earlier call went unaccounted (the spend is unknown) or when the
    /// lane has no card. `reached()` is the caller's separate question:
    /// that exit is a cap, these are errors.
    pub fn preflight(
        &self,
        catalog: &crate::model_catalog::ModelCatalogConfig,
        provider_kind: &str,
        model: &str,
    ) -> Result<ModelPricing, SpendCeilingError> {
        match self.lock {
            Some(SpendLock::UnknownHistory) => return Err(SpendCeilingError::UnknownHistory),
            Some(SpendLock::Unrestored) => return Err(SpendCeilingError::Unrestored),
            None => {}
        }
        if self.unaccounted > 0 {
            return Err(SpendCeilingError::Unaccounted {
                calls: self.unaccounted,
            });
        }
        SpendCeiling::card_for(catalog, provider_kind, model)
    }

    /// `calls` requests the provider accepted ran without reporting usage
    /// (a completed response with none, or an accepted stream that broke
    /// before its usage frame): from here on the session's spend is
    /// unknown and `preflight` refuses. Zero is a no-op.
    pub fn mark_unaccounted(&mut self, calls: u32) {
        self.unaccounted += calls;
    }

    pub fn unaccounted_calls(&self) -> u32 {
        self.unaccounted
    }

    pub fn ceiling(&self) -> SpendCeiling {
        self.ceiling
    }

    /// Money (or API-equivalent, under `lanes = "all"`) counted so far.
    pub fn spent_usd(&self) -> f64 {
        self.spent_usd
    }

    /// API-equivalent figure of the usage the ceiling did not count.
    pub fn uncounted_usd(&self) -> f64 {
        self.uncounted_usd
    }

    pub fn counted_requests(&self) -> u32 {
        self.counted
    }

    /// Price one request under `card` and add it to the meter. Whether it
    /// counts depends on the lane: a subscription lane's figure is
    /// API-equivalent and counts only under `SpendLanes::All`.
    pub fn record(&mut self, provider_kind: &str, card: &ModelPricing, usage: &Usage) {
        let cost = card.cost(usage);
        let api_equivalent = pricing::is_api_equivalent_lane(provider_kind);
        if api_equivalent && self.ceiling.lanes == SpendLanes::Billed {
            self.uncounted_usd += cost;
        } else {
            self.spent_usd += cost;
            self.counted += 1;
        }
    }

    /// Has the ceiling been reached? Asked before each model call; the
    /// call that crossed it has already been recorded.
    pub fn reached(&self) -> bool {
        self.spent_usd >= self.ceiling.max_usd
    }

    /// One line for a stop or a status: `$0.4213 of $2.00 (lanes: billed)`.
    pub fn describe(&self) -> String {
        let lanes = match self.ceiling.lanes {
            SpendLanes::Billed => "billed",
            SpendLanes::All => "all",
        };
        let unaccounted = if self.unaccounted > 0 {
            format!(", {} call(s) unaccounted", self.unaccounted)
        } else {
            String::new()
        };
        format!(
            "${:.4} of ${:.2} (lanes: {lanes}{unaccounted})",
            self.spent_usd, self.ceiling.max_usd
        )
    }
}

/// `[spend]` — the standing ceiling for every session the daemon creates
/// unless the session request carries its own. OFF by default: `enabled`
/// must be set, and then `max_usd` must be present and valid, or the
/// section is refused loudly and no ceiling is armed.
///
/// ```toml
/// [spend]
/// enabled = false      # default: no ceiling anywhere
/// max_usd = 2.00       # the standing ceiling when enabled
/// lanes   = "billed"   # or "all"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpendConfig {
    pub enabled: bool,
    pub max_usd: Option<f64>,
    pub lanes: SpendLanes,
}

impl SpendConfig {
    /// The ceiling this section arms: `Ok(None)` when disabled, `Ok(Some)`
    /// when enabled with a valid `max_usd`, `Err` when enabled without one
    /// (or with an invalid one) — a misconfigured ceiling is an error the
    /// operator sees, never a silently unlimited run.
    pub fn ceiling(&self) -> Result<Option<SpendCeiling>, SpendCeilingError> {
        if !self.enabled {
            return Ok(None);
        }
        let max = self
            .max_usd
            .ok_or_else(|| SpendCeilingError::InvalidMax("unset".into()))?;
        SpendCeiling::new(max, self.lanes).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_catalog::built_in;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[test]
    fn ceiling_must_be_finite_and_positive() {
        assert!(SpendCeiling::new(0.5, SpendLanes::Billed).is_ok());
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    SpendCeiling::new(bad, SpendLanes::Billed),
                    Err(SpendCeilingError::InvalidMax(_))
                ),
                "{bad}"
            );
        }
    }

    /// One unaccounted call poisons the session: `preflight` refuses from
    /// then on, whatever the lane, and the figure says so.
    #[test]
    fn an_unaccounted_call_fails_the_meter_closed() {
        let catalog = built_in();
        let mut meter = SpendMeter::new(SpendCeiling::new(5.0, SpendLanes::Billed).unwrap());
        assert!(meter
            .preflight(&catalog, "anthropic_api", "claude-haiku-4-5")
            .is_ok());
        meter.mark_unaccounted(1);
        assert!(matches!(
            meter.preflight(&catalog, "anthropic_api", "claude-haiku-4-5"),
            Err(SpendCeilingError::Unaccounted { calls: 1 })
        ));
        assert!(!meter.reached(), "unknown spend is not a reached cap");
        assert!(
            meter.describe().ends_with("1 call(s) unaccounted)"),
            "{}",
            meter.describe()
        );
    }

    /// A continuation restores the meter from the log's projection: the
    /// figure is what the ceiling has left to give; an unknown projection
    /// locks it; a fresh meter on a continuation is locked by the loop.
    #[test]
    fn a_restored_meter_carries_the_logged_spend_and_an_unknown_log_locks_it() {
        use crate::pricing::{CostBasis, CostLane, SessionCost};
        use crate::session_cost::CostProjection;
        let catalog = built_in();
        let ceiling = SpendCeiling::new(1.0, SpendLanes::Billed).unwrap();
        let logged = |usd, basis, lane| CostProjection {
            session: SessionCost { usd, basis, lane },
            last_ask: None,
        };

        let m = SpendMeter::from_projection(
            ceiling,
            &logged(0.75, CostBasis::PerCall, CostLane::Billed),
        );
        assert!(m.restored());
        assert_eq!(m.spent_usd(), 0.75);
        assert!(!m.reached());
        assert!(m
            .preflight(&catalog, "anthropic_api", "claude-haiku-4-5")
            .is_ok());

        // a base-rate figure is a floor on a tiered card: locked, not restored
        let m = SpendMeter::from_projection(
            ceiling,
            &logged(0.5, CostBasis::BaseRate, CostLane::Billed),
        );
        assert!(matches!(
            m.preflight(&catalog, "anthropic_api", "claude-haiku-4-5"),
            Err(SpendCeilingError::UnknownHistory)
        ));

        // all-subscription history under `billed`: shown, not bounded
        let m = SpendMeter::from_projection(
            ceiling,
            &logged(3.0, CostBasis::PerCall, CostLane::ApiEquivalent),
        );
        assert_eq!((m.spent_usd(), m.uncounted_usd()), (0.0, 3.0));
        // mixed history under `billed`: counted in full (stops early, never late)
        let m =
            SpendMeter::from_projection(ceiling, &logged(0.9, CostBasis::PerCall, CostLane::Mixed));
        assert_eq!(m.spent_usd(), 0.9);

        let m = SpendMeter::from_projection(ceiling, &CostProjection::UNKNOWN);
        assert!(matches!(
            m.preflight(&catalog, "anthropic_api", "claude-haiku-4-5"),
            Err(SpendCeilingError::UnknownHistory)
        ));

        let mut m = SpendMeter::new(ceiling);
        assert!(!m.restored());
        m.lock_unrestored();
        assert!(matches!(
            m.preflight(&catalog, "anthropic_api", "claude-haiku-4-5"),
            Err(SpendCeilingError::Unrestored)
        ));
    }

    /// A ceiling arriving on the wire (a session request) is validated the
    /// same way: `nan`/`inf`/`0` cannot arm an unreachable ceiling.
    #[test]
    fn a_deserialized_ceiling_is_validated_too() {
        let ok: SpendCeiling = serde_json::from_str(r#"{"max_usd": 2.5}"#).unwrap();
        assert_eq!(ok, SpendCeiling::new(2.5, SpendLanes::Billed).unwrap());
        for bad in [
            "max_usd = nan",
            "max_usd = inf",
            "max_usd = 0.0",
            "max_usd = -1",
        ] {
            let err = toml::from_str::<SpendCeiling>(bad).unwrap_err().to_string();
            assert!(
                err.contains("finite amount greater than zero"),
                "{bad}: {err}"
            );
        }
        let round_trip: SpendCeiling =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(round_trip, ok);
    }

    #[test]
    fn config_is_off_by_default_and_loud_when_half_set() {
        assert_eq!(SpendConfig::default().ceiling(), Ok(None));
        let half = SpendConfig {
            enabled: true,
            max_usd: None,
            lanes: SpendLanes::Billed,
        };
        assert!(matches!(
            half.ceiling(),
            Err(SpendCeilingError::InvalidMax(_))
        ));
        let on = SpendConfig {
            enabled: true,
            max_usd: Some(2.0),
            lanes: SpendLanes::All,
        };
        assert_eq!(
            on.ceiling(),
            Ok(Some(SpendCeiling {
                max_usd: 2.0,
                lanes: SpendLanes::All
            }))
        );
        // disabled with a max set is still off
        let off = SpendConfig {
            enabled: false,
            max_usd: Some(2.0),
            lanes: SpendLanes::Billed,
        };
        assert_eq!(off.ceiling(), Ok(None));
        // the wire form
        let parsed: SpendConfig =
            toml::from_str("enabled = true\nmax_usd = 1.5\nlanes = \"all\"\n").unwrap();
        assert_eq!(parsed.ceiling().unwrap().unwrap().lanes, SpendLanes::All);
        assert!(
            toml::from_str::<SpendConfig>("enabled = true\nmax_usd = 1.5\nlane = \"all\"\n")
                .is_err()
        );
    }

    #[test]
    fn meter_prices_per_request_and_reaches_after_the_crossing_call() {
        let catalog = built_in();
        let card = SpendCeiling::card_for(&catalog, "anthropic_api", "claude-haiku-4-5").unwrap();
        let ceiling = SpendCeiling::new(0.25, SpendLanes::Billed).unwrap();
        let mut m = SpendMeter::new(ceiling);
        assert!(!m.reached());
        // haiku $1/MTok in, $5/MTok out: 100k in = $0.10 per call
        m.record("anthropic_api", &card, &usage(100_000, 0));
        assert!((m.spent_usd() - 0.10).abs() < 1e-12);
        assert!(!m.reached());
        m.record("anthropic_api", &card, &usage(100_000, 0));
        assert!(!m.reached());
        // the third call crosses; it has already run, the NEXT one is refused
        m.record("anthropic_api", &card, &usage(100_000, 0));
        assert!(m.reached());
        assert_eq!(m.counted_requests(), 3);
        assert_eq!(m.describe(), "$0.3000 of $0.25 (lanes: billed)");
    }

    #[test]
    fn subscription_usage_counts_only_under_all_lanes() {
        let catalog = built_in();
        let card = SpendCeiling::card_for(&catalog, "openai_codex", "gpt-5.5").unwrap();
        let billed = SpendCeiling::new(0.10, SpendLanes::Billed).unwrap();
        let mut m = SpendMeter::new(billed);
        // gpt-5.5 $5/MTok in: 100k = $0.50, API-equivalent on codex
        m.record("openai_codex", &card, &usage(100_000, 0));
        assert_eq!(m.spent_usd(), 0.0);
        assert!((m.uncounted_usd() - 0.5).abs() < 1e-12);
        assert!(!m.reached());
        let all = SpendCeiling::new(0.10, SpendLanes::All).unwrap();
        let mut m = SpendMeter::new(all);
        m.record("openai_codex", &card, &usage(100_000, 0));
        assert!((m.spent_usd() - 0.5).abs() < 1e-12);
        assert!(m.reached());
    }

    #[test]
    fn an_unpriceable_lane_refuses_to_arm() {
        let catalog = built_in();
        assert!(matches!(
            SpendCeiling::card_for(&catalog, "ollama", "qwen3.8:27b-q8_0"),
            Err(SpendCeilingError::Unmeterable { .. })
        ));
        assert!(matches!(
            SpendCeiling::card_for(&catalog, "anthropic_api", "claude-future-9"),
            Err(SpendCeilingError::Unmeterable { .. })
        ));
        let err = SpendCeiling::card_for(&catalog, "faux", "m").unwrap_err();
        assert!(err.to_string().contains("no rate card"), "{err}");
    }
}
