//! Pricing a session's event log: one fold over the events that yields the
//! session's cost with its provenance and the most recent ask's exact
//! figure. The pricing model (cards, tiers, [`SessionCost`]) lives in
//! [`crate::pricing`]; the log's shape (asks, eras, per-call usage) lives in
//! [`crate::event_log`]; this module is where the two meet, and the only
//! place ask and era boundaries are interpreted for cost. The rules are
//! `specs/mu-047-session-cost.md`. mu-hx0ta.

use crate::agent::Usage;
use crate::event_log::{EventPayload, SessionEvent};
use crate::pricing::{CostBasis, CostLane, SessionCost};

/// The pricing era in force while folding a log: the card for the
/// (provider, model) registered by `SessionCreated` / `ProviderSwitched`,
/// interpreted under the usage convention registered with it, and whether
/// that lane is a flat-rate subscription. mu-hx0ta.
#[derive(Debug, Clone, Copy, Default)]
struct Era {
    card: Option<crate::pricing::ModelPricing>,
    api_equiv: bool,
}

impl Era {
    fn new(
        catalog: &crate::model_catalog::ModelCatalogConfig,
        provider_kind: &str,
        model: &str,
        semantics: Option<&crate::agent::capabilities::UsageSemantics>,
    ) -> Self {
        Self {
            card: crate::pricing::for_model_in(catalog, provider_kind, model)
                .map(|c| c.under_semantics(semantics)),
            api_equiv: crate::pricing::is_api_equivalent_lane(provider_kind),
        }
    }
}

/// One ask while folding a log for cost: the era it started on, whether
/// the era changed during it, the sum of its recorded per-call usage (so
/// its `Done` total can be reconciled against the calls) and its exact
/// per-call figure so far. mu-hx0ta.
#[derive(Debug, Clone, Copy)]
struct AskFold {
    started: bool,
    era: Era,
    switched: bool,
    calls: Option<Usage>,
    /// `Some(total)` while every call so far had a card and reported its
    /// usage; `None` once one did not (the ask's figure is then unknown,
    /// never partial).
    cost: Option<f64>,
    /// A model call arrived without usage: nothing can account for its
    /// consumption (the ask's `Done` sums only the calls that reported),
    /// so the ask has no exact figure and the session is unknown from
    /// that event on (rounds 18-20).
    unreported: bool,
    /// The ask hit an `Error`. The loop usually follows with a `Done`
    /// (synthesised for a ticketed ask), which closes the ask as usual;
    /// if instead the next `UserMessage` arrives first, that message is a
    /// NEW ask, not an interjection into this one, and this one is closed
    /// then (round-21 board).
    errored: bool,
}

impl Default for AskFold {
    fn default() -> Self {
        Self {
            started: false,
            era: Era::default(),
            switched: false,
            calls: None,
            cost: Some(0.0),
            unreported: false,
            errored: false,
        }
    }
}

impl AskFold {
    fn open(&mut self, era: &Era) {
        if !self.started {
            self.started = true;
            self.era = *era;
            self.switched = false;
        }
    }

    fn note_switch(&mut self) {
        if self.started {
            self.switched = true;
        }
    }

    /// What the ask's `Done` reports beyond the recorded calls, per
    /// component, or `None` when the calls account for all of it. A field
    /// the `Done` reported stays reported, even at zero: the cache-write
    /// tier split is only usable when BOTH tiers are present, and
    /// `5m = Some(0), 1h = Some(n)` is a real sample (round-16 board).
    fn remainder(&self, done: &Usage) -> Option<Usage> {
        let calls = self.calls.unwrap_or_default();
        let sub = |a: u64, b: u64| a.saturating_sub(b);
        let opt = |a: Option<u64>, b: Option<u64>| a.map(|x| sub(x, b.unwrap_or(0)));
        let mut rest = Usage {
            input_tokens: sub(done.input_tokens, calls.input_tokens),
            output_tokens: sub(done.output_tokens, calls.output_tokens),
            cache_read_input_tokens: opt(
                done.cache_read_input_tokens,
                calls.cache_read_input_tokens,
            ),
            cache_creation_input_tokens: opt(
                done.cache_creation_input_tokens,
                calls.cache_creation_input_tokens,
            ),
            cache_creation_5m_input_tokens: opt(
                done.cache_creation_5m_input_tokens,
                calls.cache_creation_5m_input_tokens,
            ),
            cache_creation_1h_input_tokens: opt(
                done.cache_creation_1h_input_tokens,
                calls.cache_creation_1h_input_tokens,
            ),
            reasoning_tokens: opt(done.reasoning_tokens, calls.reasoning_tokens),
        };
        // a cache-write split is only meaningful when it accounts for the
        // flat total; cross-call subtraction can leave a zero split over
        // positive flat writes (a tier-reporting call plus a flat-only
        // sample in the same ask), which the flat fallback must price
        // rather than a complete-looking zero (round-18 board)
        if let (Some(w5), Some(w1)) = (
            rest.cache_creation_5m_input_tokens,
            rest.cache_creation_1h_input_tokens,
        ) {
            if w5 + w1 < rest.cache_creation_input_tokens.unwrap_or(0) {
                rest.cache_creation_5m_input_tokens = None;
                rest.cache_creation_1h_input_tokens = None;
            }
        }
        let pos = |v: Option<u64>| v.is_some_and(|n| n > 0);
        let any = rest.input_tokens > 0
            || rest.output_tokens > 0
            || pos(rest.cache_read_input_tokens)
            || pos(rest.cache_creation_input_tokens)
            || pos(rest.cache_creation_5m_input_tokens)
            || pos(rest.cache_creation_1h_input_tokens)
            || pos(rest.reasoning_tokens);
        any.then_some(rest)
    }
}

/// The one cost projection of a log ([`project`]):
/// the session's figure with its provenance and the most recent ask's
/// exact figure, from a single pass over the events. mu-hx0ta.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostProjection {
    /// The whole session: see `SessionEventLog::session_cost`.
    pub session: crate::pricing::SessionCost,
    /// The most recent ask: see `SessionEventLog::last_ask_cost`.
    pub last_ask: Option<f64>,
}

impl CostProjection {
    /// A log that could not be read.
    pub const UNKNOWN: Self = Self {
        session: SessionCost::UNKNOWN,
        last_ask: None,
    };
}

/// The one pass that prices a log, feeding `SessionEventLog::session_cost`
/// and `SessionEventLog::last_ask_cost` so ask and era boundaries are
/// interpreted in exactly one place (round-17 board). The cards come from
/// the catalog passed in (the daemon's process-global one; a test's
/// `built_in()` or fixture), never from a global inside this module. The (provider, model) in
/// force — `SessionCreated`, then every `ProviderSwitched`, with the
/// usage convention each registered — is folded forward, so a
/// mid-session switch never reprices anything made before it (round
/// 7). Each ask is priced where its usage is: per model call
/// (`AssistantMessageEvent`, exact under the card in force AT THAT
/// CALL; a per-request tier lands on the calls that crossed it), and
/// whatever its `Done` reports beyond those calls — a legacy ask with
/// usage only there, or a reasoning-only retry the loop folded into
/// the ask total without a message (round 15) — at the base rate
/// under the card the ask STARTED on, which makes the session figure
/// a `BaseRate` floor and leaves the ask without an exact figure. A
/// buffered switch is logged before the ask's own `Done`, so the card
/// at `Done` is not the ask's identity (round 12); a remainder in an
/// ask whose era changed cannot be attributed and makes the session
/// unknown (round 13). Any priced usage under an unknown card, or a call
/// that arrived without usage (nothing can account for it), makes the
/// session `Unknown` rather than partial. The lane (billed /
/// API-equivalent / mixed) is folded per priced usage under the lane
/// in force at the time. mu-hx0ta.
pub fn project<'a>(
    catalog: &crate::model_catalog::ModelCatalogConfig,
    events: impl Iterator<Item = &'a SessionEvent>,
) -> CostProjection {
    let mut era = Era::default();
    let mut ask = AskFold::default();
    let mut total = 0.0;
    let mut basis = CostBasis::PerCall;
    let mut lane = CostLane::Billed;
    let mut any = false;
    let mut unknown = false;
    let mut last_ask: Option<f64> = Some(0.0);
    for ev in events {
        match &ev.payload {
            EventPayload::SessionCreated {
                provider_kind,
                model,
                usage_semantics,
                ..
            } => {
                era = Era::new(catalog, provider_kind, model, usage_semantics.as_ref());
                ask.note_switch();
            }
            EventPayload::ProviderSwitched {
                new_provider_kind,
                new_model,
                usage_semantics,
                ..
            } => {
                era = Era::new(
                    catalog,
                    new_provider_kind,
                    new_model,
                    usage_semantics.as_ref(),
                );
                ask.note_switch();
            }
            EventPayload::UserMessage { .. } => {
                if ask.errored {
                    // the errored ask got no Done: this message starts
                    // the next ask, so close the old one here
                    last_ask = ask.cost;
                    ask = AskFold::default();
                }
                ask.open(&era);
            }
            EventPayload::Error { .. } => {
                if ask.started {
                    ask.errored = true;
                }
            }
            EventPayload::AssistantMessageEvent { message } => {
                ask.open(&era);
                let Some(u) = message.usage else {
                    // a call the provider did not account for: nothing can
                    // (the ask's Done sums only the calls that reported),
                    // so the ask has no exact figure and the session is
                    // unknown from this event on — not from its Done,
                    // which an errored ask never gets (round-20 board)
                    ask.unreported = true;
                    ask.cost = None;
                    unknown = true;
                    continue;
                };
                ask.calls = Some(ask.calls.map_or(u, |c| c + u));
                match era.card {
                    Some(p) => {
                        let c = p.cost(&u);
                        total += c;
                        ask.cost = ask.cost.map(|t| t + c);
                        lane = lane.fold(era.api_equiv, any);
                        any = true;
                    }
                    None => {
                        unknown = true;
                        ask.cost = None;
                    }
                }
            }
            EventPayload::Done { usage, .. } => {
                if let Some(rest) = usage.and_then(|u| ask.remainder(&u)) {
                    // usage the calls did not account for, under the
                    // era the ask started on (an ask with no events of
                    // its own has only the current era to go on)
                    ask.cost = None;
                    let rest_era = if ask.started { ask.era } else { era };
                    match rest_era.card {
                        Some(p) if !(ask.started && ask.switched) => {
                            total += p.base_rate_cost(&rest);
                            basis = CostBasis::BaseRate;
                            lane = lane.fold(rest_era.api_equiv, any);
                            any = true;
                        }
                        _ => unknown = true,
                    }
                }
                last_ask = ask.cost;
                ask = AskFold::default();
            }
            _ => {}
        }
    }
    CostProjection {
        session: if unknown {
            SessionCost::UNKNOWN
        } else {
            SessionCost {
                usd: total,
                basis,
                lane,
            }
        },
        // an ask in flight (an `Error` arrives before its `Done`) is
        // the most recent one; otherwise the last completed
        last_ask: if ask.started { ask.cost } else { last_ask },
    }
}
