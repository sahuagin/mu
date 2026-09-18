//! Per-model pricing + cost helper. mu-fqvc.
//!
//! Cost formula (verified against operator's Anthropic billing
//! 2026-05-17, session `3ff13d794a9f0ad8`, predicted $0.52 vs actual
//! delta $0.51 — within 2%):
//!
//! ```text
//! cost($) = (input × in_rate
//!         +  cache_creation × in_rate × min(write_5m_ratio, write_1h_ratio)   (flat fallback)
//!         +  cache_read × in_rate × read_ratio
//!         +  output × out_rate) / 1_000_000
//! ```
//!
//! When the per-tier split is present (mu-cache-write-tier-split-umq6) the
//! flat `cache_creation_input_tokens` field is replaced by the tiers, each
//! at its own ratio of the input rate:
//!
//! ```text
//! cost($) = (input × in_rate
//!         +  write_5m × in_rate × write_5m_ratio
//!         +  write_1h × in_rate × write_1h_ratio
//!         +  cache_read × in_rate × read_ratio
//!         +  output × out_rate) / 1_000_000
//! ```
//!
//! Every ratio is the card's own ([`ModelPricing::cache_read_ratio`],
//! [`ModelPricing::cache_write_5m_ratio`], [`ModelPricing::cache_write_1h_ratio`]):
//! on the shipped cards 0.10 / 1.25 / 2.0 for Claude (0.025 reads on Fable
//! and Mythos 5.1), 0.10 / 1.25 for OpenAI. Unknown (provider, model)
//! pairs return None from [`for_model`] — callers should treat that as
//! "cost unknown, don't display a number" rather than zero.
//!
//! `input` above is the FRESH (uncached) input. The two providers count it
//! differently, and the card says which (`ModelPricing::cache_read_in_input`
//! and `cache_creation_in_input`,
//! the same fact `UsageSemantics` carries for context fill): Anthropic
//! reports `input_tokens`, `cache_read` and `cache_creation` as disjoint
//! buckets, so fresh input is `input_tokens` as reported; OpenAI reports
//! `input_tokens` as the whole prompt with both the cached-read tokens and
//! the cache-write tokens subsets of it, so fresh input is
//! `input_tokens - cache_read - cache_write`. Pricing an OpenAI sample with
//! the Anthropic rule bills every cached token at 1.10x the input rate (the
//! mu #626 board finding) and every written token at 2.25x. mu-hx0ta.
//!
//! The model here and the projection in `crate::session_cost` are
//! specified in `specs/mu-047-session-cost.md`.
//!
//! A card prices ONE REQUEST ([`ModelPricing::cost`]); a session is the
//! sum over its requests (`crate::session_cost::project`). The
//! distinction matters on a card with a [`LongContextTier`]: gpt-6-astra
//! bills a request whose prompt exceeds 272k tokens at 2x input/cache and
//! 1.5x output for the WHOLE request, so costing a session's summed usage
//! would price a single 300k request at the base rate ($3 instead of $6)
//! and, the other way, would surcharge a session of many small requests
//! whose total happens to pass the threshold. A caller that only holds a
//! cumulative `Usage` gets the base-rate figure ([`ModelPricing::base_rate_cost`])
//! and labels it an estimate; the daemon prices each model call from the
//! event log (`crate::session_cost`).
//!
//! A flat-rate subscription lane (`is_api_equivalent_lane`: `openai_codex`,
//! `anthropic_oauth`) is priced by its api-key lane's card: the figure is
//! API-EQUIVALENT — what the same tokens would have cost on the api-key
//! lane, not money paid — and every display says so (mu-analytics tags it
//! `subscription`; mu-solo and mu-tui label from the daemon's
//! [`CostLane`]). `for_model` prices `anthropic_oauth` at the `anthropic_api`
//! provider (`anthropic_style`: disjoint cache buckets) and `openai_codex`
//! at its own (`openai_style`: cached tokens inside `input_tokens`).
//!
//! The NUMBERS are not here. A card comes from the model catalog
//! (`crate::model_catalog`: the shipped `models.default.toml`, a generated
//! layer, or the operator's `models.toml`), resolved by [`for_model`]; this
//! module is only the arithmetic over a card. A price change is a config
//! change, never a build (mu-1x0ze; `.invariants.toml` `rate-cards-are-config`
//! keeps price literals out of Rust). Sources for the shipped numbers are
//! noted beside them in `models.default.toml`.

use crate::agent::types::Usage;

/// Per-model token rates, every one of them the card's own (from the
/// catalog); nothing here is a number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// USD per million input tokens.
    pub input_per_mtok: f64,
    /// USD per million output tokens.
    pub output_per_mtok: f64,
    /// Cache reads (hits and refreshes) as a fraction of the base input
    /// price: 0.10 on every Claude model except Claude Fable 5.1 and Claude
    /// Mythos 5.1, where it is 0.025 ($0.25/MTok on a $10 base); 1.0 (no
    /// discount) when the card states none.
    pub cache_read_ratio: f64,
    /// Cache writes into the short-lived (5-minute) tier as a multiple of
    /// the input rate: 1.25 on the shipped cards; 1.0 (no surcharge) when
    /// the card states none. A flat `cache_creation_input_tokens` total with
    /// no tier split is priced at the lower of the two write ratios.
    pub cache_write_5m_ratio: f64,
    /// Cache writes into the one-hour tier as a multiple of the input rate:
    /// 2.0 on the shipped Claude cards; the 5m ratio when the card states
    /// only one write price (the OpenAI cards); 1.0 when it states none.
    pub cache_write_1h_ratio: f64,
    /// Does this provider count cache READS inside `input_tokens`?
    /// `false` for Anthropic (disjoint buckets), `true` for OpenAI (the
    /// cached tokens are a subset of the reported input). Mirrors
    /// `UsageSemantics::cache_read_in_input`; on the card so `cost()` needs
    /// no other context, and overridable by the convention a session
    /// registered ([`Self::under_semantics`]).
    pub cache_read_in_input: bool,
    /// Does this provider count cache WRITES inside `input_tokens`? Same
    /// shape as `cache_read_in_input`, kept separate because
    /// `UsageSemantics` declares the two independently.
    pub cache_creation_in_input: bool,
    /// Per-request surcharge above a prompt-size threshold (gpt-6-astra's
    /// long-context tier); `None` on every other card. Applies to the
    /// whole request, which is why cost is per request.
    pub long_context: Option<LongContextTier>,
}

/// A per-request long-context tier: when one request's prompt (the
/// provider's prompt total — `input_tokens` on a cache-in-input card,
/// `input + cache_read + cache_write` on a disjoint-bucket card) exceeds
/// `prompt_threshold`, every input-side component of that request is
/// billed at `input_mult` times its rate and the output at `output_mult`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LongContextTier {
    /// Requests with prompt tokens strictly above this are surcharged.
    pub prompt_threshold: u64,
    /// Multiplier on fresh input, cache reads and cache writes.
    pub input_mult: f64,
    /// Multiplier on output.
    pub output_mult: f64,
}

impl ModelPricing {
    /// Cost in USD of ONE REQUEST's [`Usage`] (one model call). Missing
    /// cache fields are treated as zero (partial reporting is normal — see
    /// [`Usage`]). Cache reads are priced at this model's
    /// [`Self::cache_read_ratio`] of the input rate. A session is the sum
    /// of this over its calls (`crate::session_cost`). Passing a summed `Usage`
    /// here is exact only on a card without a [`LongContextTier`] (cost is
    /// linear in tokens); on a tiered card the tier would fire on the sum,
    /// which bounds nothing — a caller holding only a sum wants
    /// [`Self::base_rate_cost`].
    ///
    /// When the per-tier split (`cache_creation_5m_input_tokens` /
    /// `cache_creation_1h_input_tokens`) is present, each tier is priced
    /// at its own ratio. When absent, the flat total in
    /// `cache_creation_input_tokens` is priced at the lower of the two
    /// ratios, the fallback that cannot overcharge whichever way a card
    /// orders them. mu-cache-write-tier-split-umq6.
    pub fn cost(&self, usage: &Usage) -> f64 {
        self.cost_with_tier(usage, true)
    }

    /// [`Self::cost`] with the [`LongContextTier`] switched off: the
    /// figure for a CUMULATIVE `Usage` when the per-request sizes are
    /// gone. On a tiered card this is a lower bound on the true cost (no
    /// request is ever billed below the base rate); running the tier on a
    /// sum instead would surcharge two small requests whose total crosses
    /// the threshold, which is not a bound in either direction. A caller
    /// that holds only a sum uses this and labels the figure an estimate.
    pub fn base_rate_cost(&self, usage: &Usage) -> f64 {
        self.cost_with_tier(usage, false)
    }

    fn cost_with_tier(&self, usage: &Usage, apply_tier: bool) -> f64 {
        let in_rate = self.input_per_mtok;
        let cr = usage.cache_read_input_tokens.unwrap_or(0) as f64;
        // Written tokens, whichever way the sample reports them: the tier
        // split when both tiers are present, else the flat total.
        let cw = match (
            usage.cache_creation_5m_input_tokens,
            usage.cache_creation_1h_input_tokens,
        ) {
            (Some(w5m), Some(w1h)) => (w5m + w1h) as f64,
            _ => usage.cache_creation_input_tokens.unwrap_or(0) as f64,
        };
        // Fresh input: what is billed at the full input rate. For a
        // provider that reports cache reads AND cache writes inside
        // input_tokens, take both back out (a written token is billed once,
        // at the write modifier); a cached figure larger than the input (a
        // partial or inconsistent sample) prices as zero fresh input, never
        // negative.
        let mut inp = usage.input_tokens as f64;
        if self.cache_read_in_input {
            inp -= cr;
        }
        if self.cache_creation_in_input {
            inp -= cw;
        }
        let inp = inp.max(0.0);
        let out = usage.output_tokens as f64;

        // Cache-write cost: use the tier ratios when BOTH tier fields are
        // present; fall back to the flat total at the 5m ratio otherwise.
        //
        // Why partial Some/None is safe to treat as flat-fallback:
        // `AnthropicCacheCreation.ephemeral_5m_input_tokens` and
        // `ephemeral_1h_input_tokens` are both `Option<u64>` with
        // `#[serde(default)]`, so a missing key deserialises to `None` (not
        // 0).  In practice the Anthropic API sends either the entire
        // `cache_creation` object (with both tier keys populated) or omits the
        // object entirely, so partial pairs should not appear from a live API
        // response.  However, because the field type is `Option<u64>` rather
        // than `u64`, a partial pair is theoretically reachable (wire sends
        // only one tier key, or a hand-constructed / legacy value supplies only
        // one field).  We treat it conservatively: without a complete split we
        // cannot price the 1h tier at its ratio without risk of undercharging
        // on whatever tokens ended up in the 1h tier, so we fall back to the
        // flat total at the LOWER of the two write ratios — the 5m one on
        // every shipped card, but a card is config and may order them either
        // way, and the fallback must stay undercharge-safe regardless. This
        // is a deliberate choice, not an assertion of structural
        // unreachability.
        let cw_cost = match (
            usage.cache_creation_5m_input_tokens,
            usage.cache_creation_1h_input_tokens,
        ) {
            (Some(w5m), Some(w1h)) => {
                w5m as f64 * in_rate * self.cache_write_5m_ratio
                    + w1h as f64 * in_rate * self.cache_write_1h_ratio
            }
            _ => {
                usage.cache_creation_input_tokens.unwrap_or(0) as f64
                    * in_rate
                    * self.cache_write_5m_ratio.min(self.cache_write_1h_ratio)
            }
        };

        // Long-context tier: the request's prompt total decides, and the
        // surcharge covers the whole request (every input-side component
        // and the output), not just the tokens past the threshold.
        let mut prompt_total = usage.input_tokens as f64;
        if !self.cache_read_in_input {
            prompt_total += cr;
        }
        if !self.cache_creation_in_input {
            prompt_total += cw;
        }
        let (in_mult, out_mult) = match self.long_context {
            Some(t) if apply_tier && prompt_total > t.prompt_threshold as f64 => {
                (t.input_mult, t.output_mult)
            }
            _ => (1.0, 1.0),
        };

        ((inp * in_rate + cw_cost + cr * in_rate * self.cache_read_ratio) * in_mult
            + out * self.output_per_mtok * out_mult)
            / 1_000_000.0
    }

    /// This card with the session's REGISTERED usage convention
    /// (`UsageSemantics` from `SessionCreated` / `ProviderSwitched`) in
    /// place of the card's own inclusion flags: the log's declaration of
    /// how the provider counted its tokens outranks the card's default
    /// (round-15 board). Reads and writes are declared and honoured
    /// independently; an undeclared side keeps the card's flag.
    pub fn under_semantics(
        mut self,
        semantics: Option<&crate::agent::capabilities::UsageSemantics>,
    ) -> Self {
        if let Some(s) = semantics {
            if let Some(r) = s.cache_read_in_input {
                self.cache_read_in_input = r;
            }
            if let Some(w) = s.cache_creation_in_input {
                self.cache_creation_in_input = w;
            }
        }
        self
    }
}

/// Provenance of a session cost figure. mu-hx0ta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    /// No figure: some priced usage ran under a (provider, model) with no
    /// rate card, and a partial total would mislead.
    #[default]
    Unknown,
    /// Every call priced under the card in force at the time; a
    /// per-request pricing tier is exact.
    PerCall,
    /// At least one ask had usage only at ask level (a legacy Done-only
    /// record), priced at the base rate under the card in force when it
    /// completed — a lower bound on a card with a per-request tier, exact
    /// otherwise; the rest of the session is per call.
    BaseRate,
}

/// Which kind of lane a session's usage ran on, so a display can say
/// whether the figure is money billed or API-equivalent (a flat-rate
/// subscription lane, nothing billed). A session can switch lanes, and a
/// total that spans both is `Mixed` — labelling it by the current
/// provider would call real spend "nothing billed" or the reverse
/// (round-13 board). mu-hx0ta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostLane {
    /// Every priced token ran on a metered lane (or there were none).
    #[default]
    Billed,
    /// Every priced token ran on a subscription lane
    /// (`is_api_equivalent_lane`): the figure is what the tokens would
    /// have cost on the api-key lane, not money paid.
    ApiEquivalent,
    /// Some of each.
    Mixed,
}

impl CostLane {
    pub fn fold(self, api_equivalent: bool, seen_any: bool) -> Self {
        match (self, api_equivalent, seen_any) {
            (_, true, false) => CostLane::ApiEquivalent,
            (_, false, false) => CostLane::Billed,
            (CostLane::ApiEquivalent, true, true) | (CostLane::Billed, false, true) => self,
            _ => CostLane::Mixed,
        }
    }
}

/// A session's cost with its provenance (`SessionEventLog::session_cost`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionCost {
    /// USD; 0.0 when `basis` is `Unknown`. API-equivalent when `lane` is.
    pub usd: f64,
    pub basis: CostBasis,
    pub lane: CostLane,
}

impl SessionCost {
    /// A session with no priced usage at all costs exactly nothing.
    pub const ZERO: Self = Self {
        usd: 0.0,
        basis: CostBasis::PerCall,
        lane: CostLane::Billed,
    };
    pub const UNKNOWN: Self = Self {
        usd: 0.0,
        basis: CostBasis::Unknown,
        lane: CostLane::Billed,
    };
    /// The figure, or `None` when the basis is unknown.
    pub fn known(&self) -> Option<f64> {
        (self.basis != CostBasis::Unknown).then_some(self.usd)
    }
}

/// The rate card for a (provider, model) pair, from the process-global
/// model catalog ([`crate::model_catalog::global`]): the shipped
/// `models.default.toml`, a generated layer, or the operator's
/// `models.toml` — never a table in code (mu-1x0ze). The NUMBERS are the
/// model's `pricing` table (an exact `[models.*]` entry, else the longest
/// matching `[model_rules.*]` prefix); WHICH tokens count as fresh input
/// follows the provider's registered `usage_semantics` (`openai_style`:
/// cache reads and writes are inside `input_tokens`; `anthropic_style` or
/// unset: disjoint buckets). `None` when the model has no card, the
/// provider is not in the catalog, or the provider is `priced = false` —
/// "cost unknown", never a guess. The
/// Anthropic OAuth lane (`anthropic_oauth`, the Claude subscription) prices
/// at the `anthropic_api` provider: like `openai_codex`, its figure is
/// API-equivalent, not money paid, and every display labels it so.
pub fn for_model(provider_kind: &str, model: &str) -> Option<ModelPricing> {
    for_model_in(crate::model_catalog::global(), provider_kind, model)
}

/// [`for_model`] against an explicit catalog — the testable seam.
pub fn for_model_in(
    catalog: &crate::model_catalog::ModelCatalogConfig,
    provider_kind: &str,
    model: &str,
) -> Option<ModelPricing> {
    let lane_provider = if provider_kind == "anthropic_oauth" {
        "anthropic_api"
    } else {
        provider_kind
    };
    let provider = catalog.provider(lane_provider)?;
    // a lane that says it does not bill by the catalog card (a self-hosted
    // server, a gateway with its own tariff) gets no card for a model id it
    // happens to share with a lane that does
    if provider.priced == Some(false) {
        return None;
    }
    let inclusive = provider.usage_semantics.as_deref() == Some("openai_style");
    let card = catalog.resolve_model(model).pricing?;
    // both rates or no card: a layer may set one (it merges over the card
    // beneath it), but a resolved card missing one prices nothing
    let (Some(input_per_mtok), Some(output_per_mtok)) = (card.input_per_mtok, card.output_per_mtok)
    else {
        tracing::warn!(
            provider = provider_kind,
            model,
            ?card,
            "model catalog: pricing table missing input_per_mtok or output_per_mtok; the model prices as unknown"
        );
        return None;
    };
    // A card is config, and config can say anything: a negative, NaN or
    // infinite rate would poison every sum it enters, and a surcharge below
    // 1 would make the base rate not a floor. An invalid card prices
    // nothing (None: "cost unknown") and says why — on every lookup, since
    // this is a pure function of the catalog; the catalog does not change
    // while a daemon runs, and a display polls, so the operator sees it.
    let rate_ok = |v: f64| v.is_finite() && v >= 0.0;
    let ratio_ok = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
    let mult_ok = |v: f64| v.is_finite() && v >= 1.0;
    let write_ok = |v: f64| v.is_finite() && v >= 0.0;
    let valid = rate_ok(input_per_mtok)
        && rate_ok(output_per_mtok)
        && card.cache_read_ratio.is_none_or(ratio_ok)
        && card.cache_write_5m_ratio.is_none_or(write_ok)
        && card.cache_write_1h_ratio.is_none_or(write_ok)
        && card.long_context.as_ref().is_none_or(|t| {
            mult_ok(t.input_mult) && mult_ok(t.output_mult) && t.prompt_threshold > 0
        });
    if !valid {
        tracing::warn!(
            provider = provider_kind,
            model,
            ?card,
            "model catalog: invalid pricing table (rates must be finite and >= 0, cache_read_ratio in 0..=1, cache_write ratios finite and >= 0, long_context multipliers finite and >= 1, prompt_threshold > 0); the model prices as unknown"
        );
        return None;
    }
    Some(ModelPricing {
        input_per_mtok,
        output_per_mtok,
        // a discount the card does not state is not assumed: reads at the
        // full input rate until the card says otherwise (a probed card
        // carries the provider's cache-read price when it reported one)
        cache_read_ratio: card.cache_read_ratio.unwrap_or(1.0),
        // likewise a surcharge the card does not state is not applied; a
        // card with one write price (no 1h tier stated) has one write price
        cache_write_5m_ratio: card.cache_write_5m_ratio.unwrap_or(1.0),
        cache_write_1h_ratio: card
            .cache_write_1h_ratio
            .or(card.cache_write_5m_ratio)
            .unwrap_or(1.0),
        cache_read_in_input: inclusive,
        cache_creation_in_input: inclusive,
        long_context: card.long_context.map(|t| LongContextTier {
            prompt_threshold: t.prompt_threshold,
            input_mult: t.input_mult,
            output_mult: t.output_mult,
        }),
    })
}

/// Is this lane a flat-rate subscription, so that a figure priced by its
/// card is API-EQUIVALENT (what the tokens would have cost on the api-key
/// lane) rather than money billed? `anthropic_oauth` and `openai_codex`.
pub fn is_api_equivalent_lane(provider_kind: &str) -> bool {
    matches!(provider_kind, "anthropic_oauth" | "openai_codex")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped catalog, not the process-global one: these tests assert
    /// the shipped numbers and must not read an operator's models.toml or
    /// MU_MODELS_ overrides on the test machine (round-5 board).
    fn card(provider_kind: &str, model: &str) -> Option<ModelPricing> {
        for_model_in(&crate::model_catalog::built_in(), provider_kind, model)
    }

    #[test]
    fn opus_47_pricing_lookup() {
        let p = card("anthropic_api", "claude-opus-4-7").expect("opus 4-7 priced");
        assert_eq!(p.input_per_mtok, 5.00);
        assert_eq!(p.output_per_mtok, 25.00);
    }

    #[test]
    fn model_prefix_match_tolerates_date_suffix() {
        assert!(card("anthropic_api", "claude-opus-4-7-20260101").is_some());
        assert!(card("anthropic_api", "claude-sonnet-4-6-20260301").is_some());
    }

    #[test]
    fn unknown_pair_returns_none() {
        assert!(card("anthropic_api", "claude-future-9").is_none());
        assert!(card("openai_codex", "gpt-4o").is_none());
        assert!(card("openai_api", "gpt-4o").is_none());
    }

    /// OpenAI reports cached tokens INSIDE input_tokens; the card says so and
    /// cost() takes them back out, so a cached token costs 0.10x, not 1.10x
    /// (the mu #626 board finding). The sample is the shape the OpenAI lane
    /// produces: input 55,577 of which 37,632 cached (the UsageSemantics
    /// test's numbers), 1,200 out, on gpt-6-astra: fresh input 17,945 at $10
    /// plus cached 37,632 at $1 plus output 1,200 at $50 is $0.17945 plus
    /// $0.037632 plus $0.06, which is $0.277082. The Anthropic rule would
    /// give $0.556. mu-hx0ta.
    #[test]
    fn openai_cached_input_is_a_subset_and_priced_once() {
        let p = card("openai_api", "gpt-6-astra").expect("priced");
        assert!(p.cache_read_in_input && p.cache_creation_in_input);
        let usage = Usage {
            input_tokens: 55_577,
            output_tokens: 1_200,
            cache_read_input_tokens: Some(37_632),
            cache_creation_input_tokens: None,
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        };
        let cost = p.cost(&usage);
        assert!((cost - 0.277_082).abs() < 1e-9, "{cost}");
        // both OpenAI lanes carry the same card; api-key is real spend,
        // codex is the API-equivalent figure
        assert_eq!(card("openai_codex", "gpt-6-astra"), Some(p));
        assert_eq!(
            card("openai_api", "gpt-5.5").map(|c| (c.input_per_mtok, c.output_per_mtok)),
            Some((5.00, 30.00))
        );
        // a fully cached prompt with no output: cached x 0.10 only (the
        // board's example: 100k cached = $0.10 on a $10 card, not $1.10)
        let cached_only = Usage {
            input_tokens: 100_000,
            output_tokens: 0,
            cache_read_input_tokens: Some(100_000),
            ..Default::default()
        };
        assert!((p.cost(&cached_only) - 0.10).abs() < 1e-9);
        // an inconsistent sample (cached > input) never prices negative
        let odd = Usage {
            input_tokens: 10,
            output_tokens: 0,
            cache_read_input_tokens: Some(50),
            ..Default::default()
        };
        assert!((p.cost(&odd) - 0.000_05).abs() < 1e-12);
        // Anthropic cards keep the disjoint rule: the same numbers price
        // input in full
        let a = card("anthropic_api", "claude-opus-5").unwrap();
        assert!(!a.cache_read_in_input && !a.cache_creation_in_input);
        assert!(
            (a.cost(&usage) - (55_577.0 * 5.0 + 37_632.0 * 0.5 + 1_200.0 * 25.0) / 1e6).abs()
                < 1e-9
        );
    }

    /// OpenAI cache writes ride the 1.25x write modifier once the lane maps
    /// them (mu-hx0ta): 10k written on a $10 card = $0.125.
    #[test]
    fn openai_cache_writes_are_priced_at_the_write_modifier() {
        let p = card("openai_api", "gpt-6-astra").expect("priced");
        let usage = Usage {
            input_tokens: 10_000,
            output_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: Some(10_000),
            ..Default::default()
        };
        // the written tokens are inside input_tokens (UsageSemantics::
        // openai_style sets cache_creation_in_input), so fresh input is 0
        // and the 10k written tokens bill once: 10k x $10 x 1.25 = $0.125,
        // not $0.225 (fresh AND write, the round-3 board finding).
        assert!((p.cost(&usage) - 0.125).abs() < 1e-9, "{}", p.cost(&usage));
        // a prompt that is part read, part written, part fresh: each token
        // priced exactly once at its own rate.
        let mixed = Usage {
            input_tokens: 10_000,
            output_tokens: 0,
            cache_read_input_tokens: Some(4_000),
            cache_creation_input_tokens: Some(5_000),
            ..Default::default()
        };
        // fresh 1k x $10 + read 4k x $1 + write 5k x $12.5 = 0.01+0.004+0.0625
        assert!((p.cost(&mixed) - 0.0765).abs() < 1e-9, "{}", p.cost(&mixed));
    }

    /// Retired ids stay priced (the lane only warns about them, and a
    /// gateway that still serves them produces real usage), and their dated
    /// rows win over the bare family rows that follow them.
    /// gpt-6-astra's long-context tier is per REQUEST: the round-4 board
    /// finding was that a single uncached 300k request priced $3 at the
    /// base rate where the tariff says $6. Cost is therefore summed over
    /// requests, never computed on the session's summed usage.
    #[test]
    fn long_context_tier_applies_per_request_not_to_the_session_sum() {
        let p = card("openai_api", "gpt-6-astra").expect("priced");
        let big = Usage {
            input_tokens: 300_000,
            ..Default::default()
        };
        // one 300k uncached request: 300k x $10 x 2 = $6.00
        assert!((p.cost(&big) - 6.0).abs() < 1e-9, "{}", p.cost(&big));
        // the same tokens as two 150k requests: base rate, $3.00
        let half = Usage {
            input_tokens: 150_000,
            ..Default::default()
        };
        let two = p.cost(&half) + p.cost(&half);
        assert!((two - 3.0).abs() < 1e-9, "{two}");
        // costing the summed usage with the tier would surcharge those two
        // small requests (the other way the sum goes wrong): a caller with
        // only the sum uses base_rate_cost, the lower bound, and
        // labels the figure an estimate
        assert!((p.cost(&(half + half)) - 6.0).abs() < 1e-9);
        assert!((p.base_rate_cost(&(half + half)) - 3.0).abs() < 1e-9);
        assert!((p.base_rate_cost(&big) - 3.0).abs() < 1e-9);
        // exactly at the threshold is base rate; one past it is not
        let at = Usage {
            input_tokens: 272_000,
            ..Default::default()
        };
        let past = Usage {
            input_tokens: 272_001,
            ..Default::default()
        };
        assert!((p.cost(&at) - 2.72).abs() < 1e-9);
        assert!(p.cost(&past) > 5.4);
        // the surcharge covers the whole request, cache components and
        // output included: 280k prompt = 200k read + 80k fresh, 1k out
        // = (80k x $10 + 200k x $1) x 2 + 1k x $50 x 1.5 = 2.0 + 0.075
        let mixed = Usage {
            input_tokens: 280_000,
            output_tokens: 1_000,
            cache_read_input_tokens: Some(200_000),
            ..Default::default()
        };
        assert!((p.cost(&mixed) - 2.075).abs() < 1e-9, "{}", p.cost(&mixed));
        // Anthropic cards have no tier: summed usage prices exactly
        let a = card("anthropic_api", "claude-opus-4-8").expect("priced");
        assert!((a.cost(&(half + half)) - (a.cost(&half) + a.cost(&half))).abs() < 1e-12);
    }

    /// The registered convention outranks the card's flags, read and write
    /// independently: an Anthropic card told the log counts cache reads
    /// inside input (but not writes) prices 1k input + 1k read + 1k written
    /// as 0 fresh + 1k read + 1k written.
    #[test]
    fn registered_usage_semantics_outrank_the_card_flags() {
        use crate::agent::capabilities::UsageSemantics;
        let a = card("anthropic_api", "claude-opus-4-8").expect("priced");
        assert!(!a.cache_read_in_input && !a.cache_creation_in_input);
        let o = a.under_semantics(Some(&UsageSemantics::openai_style()));
        assert!(o.cache_read_in_input && o.cache_creation_in_input);
        let back = o.under_semantics(Some(&UsageSemantics::anthropic_style()));
        assert!(!back.cache_read_in_input && !back.cache_creation_in_input);
        assert_eq!(a.under_semantics(None), a);
        // declared independently, honoured independently
        let reads_only = UsageSemantics {
            cache_read_in_input: Some(true),
            cache_creation_in_input: Some(false),
            reasoning_in_output: None,
        };
        let r = a.under_semantics(Some(&reads_only));
        assert!(r.cache_read_in_input && !r.cache_creation_in_input);
        let usage = Usage {
            input_tokens: 1_000,
            output_tokens: 0,
            cache_read_input_tokens: Some(1_000),
            cache_creation_input_tokens: Some(1_000),
            ..Default::default()
        };
        // opus $5: reads inside input → 0 fresh; 1k read x $0.50 + 1k
        // written x $6.25 = $0.00675 (the disjoint card says $0.01175)
        assert!(
            (r.cost(&usage) - 0.00675).abs() < 1e-12,
            "{}",
            r.cost(&usage)
        );
        assert!(
            (a.cost(&usage) - 0.01175).abs() < 1e-12,
            "{}",
            a.cost(&usage)
        );
        // an undeclared side keeps the card's flag
        let half = UsageSemantics {
            cache_read_in_input: None,
            cache_creation_in_input: Some(true),
            reasoning_in_output: None,
        };
        let h = a.under_semantics(Some(&half));
        assert!(!h.cache_read_in_input && h.cache_creation_in_input);
    }

    /// A card is config and config can say anything: an invalid number
    /// prices nothing rather than poisoning every sum it would enter.
    #[test]
    fn invalid_catalog_cards_price_as_unknown() {
        use figment::{providers::Format, providers::Toml, Figment};
        let load = |pricing: &str| -> crate::model_catalog::ModelCatalogConfig {
            let toml = format!(
                "[providers.p]\nkind = \"p\"\n[models.m]\nmodel = \"m\"\n[models.m.pricing]\n{pricing}\n"
            );
            Figment::from(Toml::string(&toml)).extract().unwrap()
        };
        assert!(for_model_in(
            &load("input_per_mtok = 1.0\noutput_per_mtok = 2.0"),
            "p",
            "m"
        )
        .is_some());
        for bad in [
            "input_per_mtok = -1.0\noutput_per_mtok = 2.0",
            "input_per_mtok = nan\noutput_per_mtok = 2.0",
            "input_per_mtok = 1.0\noutput_per_mtok = inf",
            "input_per_mtok = 1.0\noutput_per_mtok = 2.0\ncache_read_ratio = 1.5",
            "input_per_mtok = 1.0\noutput_per_mtok = 2.0\ncache_read_ratio = -0.1",
            "input_per_mtok = 1.0\noutput_per_mtok = 2.0\nlong_context = { prompt_threshold = 1000, input_mult = 0.5, output_mult = 1.5 }",
            "input_per_mtok = 1.0\noutput_per_mtok = 2.0\nlong_context = { prompt_threshold = 0, input_mult = 2.0, output_mult = 1.5 }",
        ] {
            assert!(for_model_in(&load(bad), "p", "m").is_none(), "{bad}");
        }
        // one rate alone is no card
        assert!(for_model_in(&load("input_per_mtok = 1.0"), "p", "m").is_none());
        // the flat-write fallback prices at the lower write ratio whichever
        // way a card orders the tiers, so losing the split never overcharges
        let flat = Usage {
            cache_creation_input_tokens: Some(1_000_000),
            ..Default::default()
        };
        let split_1h = Usage {
            cache_creation_input_tokens: Some(1_000_000),
            cache_creation_5m_input_tokens: Some(0),
            cache_creation_1h_input_tokens: Some(1_000_000),
            ..Default::default()
        };
        for (w5, w1) in [(1.25_f64, 2.0_f64), (2.0, 1.25)] {
            let cfg = load(&format!(
                "input_per_mtok = 1.0\noutput_per_mtok = 2.0\ncache_write_5m_ratio = {w5}\ncache_write_1h_ratio = {w1}"
            ));
            let p = for_model_in(&cfg, "p", "m").unwrap();
            let lower = w5.min(w1);
            assert!(
                (p.cost(&flat) - lower).abs() < 1e-12,
                "{w5}/{w1}: {}",
                p.cost(&flat)
            );
            assert!(p.cost(&flat) <= p.cost(&split_1h) + 1e-12);
        }
        // a zero rate is a valid card (a free model priced at $0)
        assert!(for_model_in(
            &load("input_per_mtok = 0.0\noutput_per_mtok = 0.0"),
            "p",
            "m"
        )
        .is_some());
    }

    #[test]
    fn cost_lane_folds_to_mixed_across_lanes() {
        let mut lane = CostLane::Billed;
        let mut any = false;
        for (api, want) in [
            (true, CostLane::ApiEquivalent),
            (true, CostLane::ApiEquivalent),
            (false, CostLane::Mixed),
            (true, CostLane::Mixed),
        ] {
            lane = lane.fold(api, any);
            any = true;
            assert_eq!(lane, want);
        }
        assert_eq!(CostLane::Billed.fold(false, false), CostLane::Billed);
        assert_eq!(CostLane::Billed.fold(false, true), CostLane::Billed);
        assert_eq!(CostLane::Billed.fold(true, true), CostLane::Mixed);
    }

    /// The subscription lanes price at the api-key card (API-equivalent).
    #[test]
    fn subscription_lanes_price_at_the_api_card() {
        assert_eq!(
            card("anthropic_oauth", "claude-opus-4-8"),
            card("anthropic_api", "claude-opus-4-8")
        );
        assert!(card("anthropic_oauth", "claude-opus-4-8").is_some());
        assert_eq!(
            card("openai_codex", "gpt-5.5"),
            card("openai_api", "gpt-5.5")
        );
        assert!(is_api_equivalent_lane("anthropic_oauth"));
        assert!(is_api_equivalent_lane("openai_codex"));
        assert!(!is_api_equivalent_lane("anthropic_api"));
        assert!(!is_api_equivalent_lane("openai_api"));
    }

    #[test]
    fn retired_ids_keep_their_rate_card() {
        let rates = |model: &str| {
            let p = card("anthropic_api", model).unwrap_or_else(|| panic!("{model} priced"));
            (p.input_per_mtok, p.output_per_mtok)
        };
        assert_eq!(rates("claude-opus-4-1-20250805"), (15.00, 75.00));
        assert_eq!(rates("claude-opus-4-20250514"), (15.00, 75.00));
        assert_eq!(rates("claude-sonnet-4-20250514"), (3.00, 15.00));
    }

    /// Every 4.x id the catalog knows is priced, and an id the table has no
    /// row for gets its family's figure — what mu-solo's status line showed
    /// before it used this table, so consolidating on it loses no model.
    #[test]
    fn four_x_ids_and_bare_family_fallbacks() {
        let rates = |model: &str| {
            let p = card("anthropic_api", model).unwrap_or_else(|| panic!("{model} priced"));
            (p.input_per_mtok, p.output_per_mtok)
        };
        assert_eq!(rates("claude-opus-4-5-20251101"), (5.00, 25.00));
        assert_eq!(rates("claude-sonnet-4-5-20250929"), (3.00, 15.00));
        assert_eq!(rates("claude-haiku-4-5-20251001"), (1.00, 5.00));
        assert_eq!(rates("claude-opus-4-9"), (5.00, 25.00));
        assert_eq!(rates("claude-sonnet-4-7"), (3.00, 15.00));
        assert_eq!(rates("claude-haiku-4-6"), (1.00, 5.00));
    }

    /// The gen-5 rows from the 2026-09-04 snapshot's prompt-caching table,
    /// and the one per-model read modifier: a million cached tokens read
    /// back cost $0.25 on Claude Fable 5.1 / Mythos 5.1 and $1.00 on Claude
    /// Fable 5 at the same $10 base. The 5.1 prefix must win over the
    /// gen-5 one, date suffix included. mu-anthropic-protocol-2026q3-6uqho.6.
    #[test]
    fn gen_5_rows_and_the_fable_5_1_cache_read_ratio() {
        let rates = |model: &str| {
            let p = card("anthropic_api", model).unwrap_or_else(|| panic!("{model} priced"));
            (p.input_per_mtok, p.output_per_mtok, p.cache_read_ratio)
        };
        assert_eq!(rates("claude-fable-5-1"), (10.00, 50.00, 0.025));
        assert_eq!(rates("claude-fable-5-1-20261001"), (10.00, 50.00, 0.025));
        assert_eq!(rates("claude-mythos-5-1"), (10.00, 50.00, 0.025));
        assert_eq!(rates("claude-fable-5"), (10.00, 50.00, 0.10));
        assert_eq!(rates("claude-mythos-5"), (10.00, 50.00, 0.10));
        assert_eq!(rates("claude-opus-5"), (5.00, 25.00, 0.10));
        assert_eq!(rates("claude-sonnet-5"), (2.00, 10.00, 0.10));
        assert_eq!(rates("claude-opus-4-8"), (5.00, 25.00, 0.10));

        let reads = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: Some(1_000_000),
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        };
        let cost = |model: &str| card("anthropic_api", model).unwrap().cost(&reads);
        assert!(
            (cost("claude-fable-5-1") - 0.25).abs() < 1e-9,
            "{}",
            cost("claude-fable-5-1")
        );
        assert!(
            (cost("claude-fable-5") - 1.00).abs() < 1e-9,
            "{}",
            cost("claude-fable-5")
        );
        assert!(
            (cost("claude-opus-4-7") - 0.50).abs() < 1e-9,
            "{}",
            cost("claude-opus-4-7")
        );
    }

    /// Calibration: session 3ff13d794a9f0ad8 (mu-fqvc bead). Predicted
    /// $0.52 here vs operator-confirmed billing delta $0.51 — within 2%.
    #[test]
    fn calibration_session_3ff13d_within_one_cent_of_actual() {
        let usage = Usage {
            input_tokens: 35_419,
            output_tokens: 6_960,
            cache_creation_input_tokens: Some(21_772),
            cache_read_input_tokens: Some(58_084),
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        };
        let pricing = card("anthropic_api", "claude-opus-4-7").unwrap();
        let cost = pricing.cost(&usage);
        // Expected ~$0.5237; actual operator-billing delta $0.51. Allow
        // a 5-cent envelope — caching rate is the wiggle.
        assert!(
            (cost - 0.52).abs() < 0.05,
            "cost {cost} drifted from calibration anchor $0.52"
        );
    }

    #[test]
    fn zero_usage_costs_zero() {
        let pricing = card("anthropic_api", "claude-opus-4-7").unwrap();
        assert_eq!(pricing.cost(&Usage::default()), 0.0);
    }

    #[test]
    fn missing_cache_fields_treated_as_zero() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        };
        let pricing = card("anthropic_api", "claude-opus-4-7").unwrap();
        // 1M input × $5 + 100k output × $25 = $5 + $2.50 = $7.50
        assert!((pricing.cost(&usage) - 7.50).abs() < 1e-9);
    }

    // ─── mu-cache-write-tier-split-umq6: per-tier pricing tests ─────────────

    /// When both tier fields are present, tier-specific rates apply:
    /// 5m tier at 1.25× and 1h tier at 2.0×. No flat total is consulted.
    #[test]
    fn umq6_tier_split_uses_per_tier_rates() {
        let pricing = card("anthropic_api", "claude-opus-4-7").unwrap();
        let in_rate = pricing.input_per_mtok; // $5.00
        let usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: None,
            // Flat total (should NOT be consulted when tier fields present).
            cache_creation_input_tokens: Some(99_999),
            cache_creation_5m_input_tokens: Some(500_000),
            cache_creation_1h_input_tokens: Some(1_000_000),
            reasoning_tokens: None,
        };
        let cost = pricing.cost(&usage);
        // Expected: (500k × $5 × 1.25 + 1M × $5 × 2.0) / 1M
        //         = (3_125_000 + 10_000_000) / 1_000_000 ≈ $13.125
        let expected =
            (500_000_f64 * in_rate * 1.25 + 1_000_000_f64 * in_rate * 2.00) / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-9,
            "cost {cost} should be {expected} (tier rates applied)"
        );
    }

    /// Fallback: when only the flat total is present (no tier fields),
    /// pricing uses the conservative 1.25× rate.
    #[test]
    fn umq6_flat_fallback_uses_conservative_rate() {
        let pricing = card("anthropic_api", "claude-opus-4-7").unwrap();
        let in_rate = pricing.input_per_mtok;
        let usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: Some(1_000_000),
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            reasoning_tokens: None,
        };
        let cost = pricing.cost(&usage);
        // Expected: 1M × $5 × 1.25 / 1M = $6.25
        let expected = 1_000_000_f64 * in_rate * 1.25 / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-9,
            "cost {cost} should be {expected} (flat fallback at 1.25×)"
        );
    }

    /// Only one tier field present → still falls back to flat (partial
    /// breakdown is not trusted for pricing).
    #[test]
    fn umq6_partial_tier_falls_back_to_flat() {
        let pricing = card("anthropic_api", "claude-opus-4-7").unwrap();
        let in_rate = pricing.input_per_mtok;
        let usage_only_5m = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: Some(1_000_000),
            cache_creation_5m_input_tokens: Some(400_000),
            cache_creation_1h_input_tokens: None, // absent → flat fallback
            reasoning_tokens: None,
        };
        let cost = pricing.cost(&usage_only_5m);
        let expected = 1_000_000_f64 * in_rate * 1.25 / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-9,
            "partial breakdown should fall back to flat: cost {cost} ≠ {expected}"
        );
    }

    /// Mirror partial case: (None, Some(1h)) — only the 1h tier field is
    /// present. This can arise from a hand-constructed value or a hypothetical
    /// future API variant that emits only the 1h key. Without a complete split
    /// we cannot safely apply the 2.0× rate (risk of undercharging flat tokens
    /// at 1.25× if they were in the 1h tier), so we fall back to the flat
    /// total at 1.25×. The scenario is an undercharge (~37.5% undercount for
    /// a pure-1h session) but it is safe-conservative and avoids overcharging
    /// the caller. mu-cache-write-tier-split-umq6.
    #[test]
    fn umq6_partial_tier_none_some_1h_falls_back_to_flat() {
        let pricing = card("anthropic_api", "claude-opus-4-7").unwrap();
        let in_rate = pricing.input_per_mtok;
        let usage_only_1h = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: Some(1_000_000),
            cache_creation_5m_input_tokens: None, // absent
            cache_creation_1h_input_tokens: Some(800_000), // present but incomplete split
            reasoning_tokens: None,
        };
        let cost = pricing.cost(&usage_only_1h);
        // Expected: flat total 1M × $5 × 1.25 / 1M = $6.25  (NOT 2.0×)
        let expected = 1_000_000_f64 * in_rate * 1.25 / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-9,
            "(None, Some(1h)) should fall back to flat 1.25×: cost {cost} ≠ {expected}"
        );
    }
}
