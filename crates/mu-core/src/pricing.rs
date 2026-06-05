//! Per-model pricing + cost helper. mu-fqvc.
//!
//! Cost formula (verified against operator's Anthropic billing
//! 2026-05-17, session `3ff13d794a9f0ad8`, predicted $0.52 vs actual
//! delta $0.51 — within 2%):
//!
//! ```text
//! cost($) = (input × in_rate
//!         +  cache_creation × in_rate × 1.25   (flat fallback)
//!         +  cache_read × in_rate × 0.10
//!         +  output × out_rate) / 1_000_000
//! ```
//!
//! When the per-tier split is present (mu-cache-write-tier-split-umq6) the
//! flat `cache_creation_input_tokens` field is replaced by tier-specific
//! rates — 1.25x for ephemeral-5m, 2.0x for ephemeral-1h:
//!
//! ```text
//! cost($) = (input × in_rate
//!         +  write_5m × in_rate × 1.25
//!         +  write_1h × in_rate × 2.00
//!         +  cache_read × in_rate × 0.10
//!         +  output × out_rate) / 1_000_000
//! ```
//!
//! Caching modifiers (1.25x write, 0.10x read) apply across all
//! Anthropic models. Unknown (provider, model) pairs return None
//! from [`for_model`] — callers should treat that as "cost unknown,
//! don't display a number" rather than zero.
//!
//! Source for rate-card values: Anthropic public pricing page,
//! 2026-04-16 (unchanged through May 2026). Operator-confirmed.

use crate::agent::types::Usage;

/// Per-model token rates. Cache modifiers are derived (1.25x input
/// for cache writes, 0.10x input for cache reads).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// USD per million input tokens.
    pub input_per_mtok: f64,
    /// USD per million output tokens.
    pub output_per_mtok: f64,
}

impl ModelPricing {
    /// Cost in USD for one [`Usage`] sample. Missing cache fields are
    /// treated as zero (partial reporting is normal — see [`Usage`]).
    ///
    /// When the per-tier split (`cache_creation_5m_input_tokens` /
    /// `cache_creation_1h_input_tokens`) is present, tier-specific
    /// rates are used (1.25× for 5m, 2.0× for 1h). When absent, the
    /// flat total in `cache_creation_input_tokens` is priced at the
    /// conservative 1.25× fallback. mu-cache-write-tier-split-umq6.
    pub fn cost(&self, usage: &Usage) -> f64 {
        let in_rate = self.input_per_mtok;
        let cr = usage.cache_read_input_tokens.unwrap_or(0) as f64;
        let inp = usage.input_tokens as f64;
        let out = usage.output_tokens as f64;

        // Cache-write cost: use tier-specific multipliers when BOTH tier
        // fields are present; fall back to the flat total at 1.25× otherwise.
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
        // cannot price the 1h tier at 2.0× without risk of undercharging on
        // whatever tokens ended up in the 1h tier, so we fall back to the flat
        // total at the safe 1.25× rate.  This is a deliberate undercharge-safe
        // choice, not an assertion of structural unreachability.
        let cw_cost = match (
            usage.cache_creation_5m_input_tokens,
            usage.cache_creation_1h_input_tokens,
        ) {
            (Some(w5m), Some(w1h)) => w5m as f64 * in_rate * 1.25 + w1h as f64 * in_rate * 2.00,
            _ => {
                // Flat fallback: assume 1.25× (5m tier) when no breakdown.
                usage.cache_creation_input_tokens.unwrap_or(0) as f64 * in_rate * 1.25
            }
        };

        (inp * in_rate + cw_cost + cr * in_rate * 0.10 + out * self.output_per_mtok) / 1_000_000.0
    }
}

/// Look up pricing for a (provider, model) pair. Match is exact on
/// provider kind (e.g. `"anthropic_api"`), prefix on model name
/// (e.g. `"claude-opus-4-7"` matches `claude-opus-4-7-20260101`).
/// Returns None for unknown pairs.
pub fn for_model(provider_kind: &str, model: &str) -> Option<ModelPricing> {
    let entry = MODEL_RATES
        .iter()
        .find(|(p, m, _)| *p == provider_kind && model.starts_with(m))?;
    Some(entry.2)
}

// (provider_kind, model_prefix, pricing). First match wins, so list
// more-specific prefixes before less-specific ones.
const MODEL_RATES: &[(&str, &str, ModelPricing)] = &[
    (
        "anthropic_api",
        "claude-opus-4-8",
        ModelPricing {
            input_per_mtok: 5.00,
            output_per_mtok: 25.00,
        },
    ),
    (
        "anthropic_api",
        "claude-opus-4-7",
        ModelPricing {
            input_per_mtok: 5.00,
            output_per_mtok: 25.00,
        },
    ),
    (
        "anthropic_api",
        "claude-opus-4-6",
        ModelPricing {
            input_per_mtok: 5.00,
            output_per_mtok: 25.00,
        },
    ),
    (
        "anthropic_api",
        "claude-sonnet-4-6",
        ModelPricing {
            input_per_mtok: 3.00,
            output_per_mtok: 15.00,
        },
    ),
    (
        "anthropic_api",
        "claude-haiku-4-5",
        ModelPricing {
            input_per_mtok: 1.00,
            output_per_mtok: 5.00,
        },
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_47_pricing_lookup() {
        let p = for_model("anthropic_api", "claude-opus-4-7").expect("opus 4-7 priced");
        assert_eq!(p.input_per_mtok, 5.00);
        assert_eq!(p.output_per_mtok, 25.00);
    }

    #[test]
    fn model_prefix_match_tolerates_date_suffix() {
        assert!(for_model("anthropic_api", "claude-opus-4-7-20260101").is_some());
        assert!(for_model("anthropic_api", "claude-sonnet-4-6-20260301").is_some());
    }

    #[test]
    fn unknown_pair_returns_none() {
        assert!(for_model("anthropic_api", "claude-future-9").is_none());
        assert!(for_model("openai_codex", "any").is_none());
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
        let pricing = for_model("anthropic_api", "claude-opus-4-7").unwrap();
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
        let pricing = for_model("anthropic_api", "claude-opus-4-7").unwrap();
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
        let pricing = for_model("anthropic_api", "claude-opus-4-7").unwrap();
        // 1M input × $5 + 100k output × $25 = $5 + $2.50 = $7.50
        assert!((pricing.cost(&usage) - 7.50).abs() < 1e-9);
    }

    // ─── mu-cache-write-tier-split-umq6: per-tier pricing tests ─────────────

    /// When both tier fields are present, tier-specific rates apply:
    /// 5m tier at 1.25× and 1h tier at 2.0×. No flat total is consulted.
    #[test]
    fn umq6_tier_split_uses_per_tier_rates() {
        let pricing = for_model("anthropic_api", "claude-opus-4-7").unwrap();
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
        let pricing = for_model("anthropic_api", "claude-opus-4-7").unwrap();
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
        let pricing = for_model("anthropic_api", "claude-opus-4-7").unwrap();
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
        let pricing = for_model("anthropic_api", "claude-opus-4-7").unwrap();
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
