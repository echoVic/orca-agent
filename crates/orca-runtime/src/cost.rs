use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use orca_core::cost_types::UsageTotals;
use orca_core::model::{ModelPriceOverride, canonical_model_name};
use orca_core::provider_types::Usage;

use crate::pricing::book;
use crate::pricing::rates::{self, Rates};

pub(crate) fn usd_to_micros(usd: f64) -> u64 {
    if usd.is_finite() && usd > 0.0 {
        (usd * 1_000_000.0).round().min(u64::MAX as f64) as u64
    } else {
        0
    }
}

/// When usage is priced: DeepSeek's peak hours cost twice as much. Tests pin it.
pub type PricingClock = fn() -> DateTime<Utc>;

#[derive(Clone, Debug)]
pub struct CostTracker {
    totals: UsageTotals,
    /// Canonical name of the model the next usage is billed to.
    model: Option<String>,
    /// Fixed prices from `[models."<name>".pricing]`, by canonical name.
    overrides: Arc<BTreeMap<String, Rates>>,
    clock: PricingClock,
}

impl CostTracker {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            totals: UsageTotals::default(),
            model: canonical(model),
            overrides: Arc::default(),
            clock: Utc::now,
        }
    }

    /// A tracker that bills the models in `overrides` at their fixed prices.
    pub fn with_overrides(
        model: Option<&str>,
        overrides: &BTreeMap<String, ModelPriceOverride>,
    ) -> Self {
        let overrides = overrides
            .iter()
            .map(|(model, price)| {
                (
                    model.clone(),
                    Rates {
                        input_cache_miss: price.input_cache_miss.get(),
                        input_cache_hit: price.input_cache_hit.get(),
                        output: price.output.get(),
                    },
                )
            })
            .collect();
        Self {
            overrides: Arc::new(overrides),
            ..Self::new(model)
        }
    }

    pub fn with_clock(mut self, clock: PricingClock) -> Self {
        self.clock = clock;
        self
    }

    pub fn add_usage(&mut self, usage: Usage) -> UsageTotals {
        self.totals.input_tokens += usage.input_tokens;
        self.totals.output_tokens += usage.output_tokens;
        self.totals.cache_tokens += usage.cache_tokens;
        self.totals.estimated_cost_usd += self.estimate(usage);
        self.totals
    }

    /// DeepSeek bills cache-miss input, cache-hit input, and output tokens
    /// separately; peak hours double its prices, never an override's.
    fn estimate(&self, usage: Usage) -> f64 {
        let model = self.model.as_deref().unwrap_or("");
        if let Some(rates) = self.overrides.get(model) {
            return rates::cost_usd(*rates, 1.0, usage);
        }
        let rates = book::current().rates_for(model);
        rates::cost_usd(rates, rates::time_of_use_multiplier((self.clock)()), usage)
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.model = canonical(model);
    }

    pub fn merge(&mut self, other: &CostTracker) {
        self.totals.input_tokens = self
            .totals
            .input_tokens
            .saturating_add(other.totals.input_tokens);
        self.totals.output_tokens = self
            .totals
            .output_tokens
            .saturating_add(other.totals.output_tokens);
        self.totals.cache_tokens = self
            .totals
            .cache_tokens
            .saturating_add(other.totals.cache_tokens);
        self.totals.estimated_cost_usd += other.totals.estimated_cost_usd;
    }

    pub fn merge_totals(&mut self, usage: UsageTotals) {
        self.totals.input_tokens = self.totals.input_tokens.saturating_add(usage.input_tokens);
        self.totals.output_tokens = self
            .totals
            .output_tokens
            .saturating_add(usage.output_tokens);
        self.totals.cache_tokens = self.totals.cache_tokens.saturating_add(usage.cache_tokens);
        self.totals.estimated_cost_usd += usage.estimated_cost_usd;
    }

    pub fn totals(&self) -> UsageTotals {
        self.totals
    }
}

fn canonical(model: Option<&str>) -> Option<String> {
    model.map(|model| canonical_model_name(model).to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use orca_core::model::{
        FLASH_MODEL, LEGACY_FLASH_MODEL, LEGACY_VISION_MODEL, ModelPriceOverride, PRO_MODEL,
        UsdPerMillion,
    };

    use super::*;
    use crate::pricing::rates::{off_peak_instant, peak_instant};

    fn usage(input_tokens: u64, output_tokens: u64, cache_tokens: u64) -> Usage {
        Usage {
            input_tokens,
            output_tokens,
            cache_tokens,
        }
    }

    fn cost(tracker: CostTracker, usage: Usage) -> f64 {
        let mut tracker = tracker;
        tracker.add_usage(usage).estimated_cost_usd
    }

    #[test]
    fn accumulates_tokens_and_off_peak_flash_cost() {
        let mut tracker = CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant);
        let totals = tracker.add_usage(usage(120, 30, 10));
        assert_eq!(totals.input_tokens, 120);
        assert_eq!(totals.output_tokens, 30);
        assert_eq!(totals.cache_tokens, 10);
        assert_eq!(totals.total_tokens(), 150);
        assert!(totals.estimated_cost_usd > 0.0);
        let expected = (110.0 * 0.15 + 10.0 * 0.003 + 30.0 * 0.6) / 1_000_000.0;
        assert!((totals.estimated_cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn peak_hours_double_the_estimate() {
        let used = usage(1_000_000, 1_000_000, 100_000);
        let off_peak = cost(
            CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant),
            used,
        );
        let peak = cost(
            CostTracker::new(Some(FLASH_MODEL)).with_clock(peak_instant),
            used,
        );
        assert!((peak - 2.0 * off_peak).abs() < 1e-12);
    }

    #[test]
    fn merge_accumulates_from_child_tracker() {
        let mut parent = CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant);
        parent.add_usage(usage(100, 50, 20));
        let mut child = CostTracker::new(Some(LEGACY_VISION_MODEL)).with_clock(off_peak_instant);
        child.add_usage(usage(200, 80, 30));
        let parent_cost_before = parent.totals().estimated_cost_usd;
        let child_cost = child.totals().estimated_cost_usd;

        parent.merge(&child);

        assert_eq!(parent.totals().input_tokens, 300);
        assert_eq!(parent.totals().output_tokens, 130);
        assert_eq!(parent.totals().cache_tokens, 50);
        assert!(
            (parent.totals().estimated_cost_usd - (parent_cost_before + child_cost)).abs() < 1e-12
        );
    }

    #[test]
    fn retired_flash_aliases_use_canonical_flash_pricing() {
        let used = usage(1_000_000, 1_000_000, 100_000);
        let canonical = cost(
            CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant),
            used,
        );
        for alias in [LEGACY_FLASH_MODEL, LEGACY_VISION_MODEL] {
            assert_eq!(
                cost(
                    CostTracker::new(Some(alias)).with_clock(off_peak_instant),
                    used
                ),
                canonical
            );
        }
    }

    #[test]
    fn experimental_pro_names_keep_pro_pricing_and_unknown_models_use_flash() {
        let used = usage(900_000, 100_000, 200_000);
        let at = |model| {
            cost(
                CostTracker::new(Some(model)).with_clock(off_peak_instant),
                used,
            )
        };
        assert_eq!(at("deepseek-v4-pro-exp"), at(PRO_MODEL));
        assert_eq!(at("qwen-max"), at(FLASH_MODEL));
        assert_eq!(
            cost(CostTracker::new(None).with_clock(off_peak_instant), used),
            at(FLASH_MODEL)
        );
        assert!(at(PRO_MODEL) > at(FLASH_MODEL));
    }

    #[test]
    fn set_model_switches_the_prices_for_later_usage() {
        let used = usage(1_000, 1_000, 0);
        let mut tracker = CostTracker::new(Some(PRO_MODEL)).with_clock(off_peak_instant);
        tracker.set_model(Some(FLASH_MODEL));
        let expected = (1_000.0 * 0.15 + 1_000.0 * 0.6) / 1_000_000.0;
        assert!((tracker.add_usage(used).estimated_cost_usd - expected).abs() < 1e-12);
    }

    fn override_prices(miss: f64, hit: f64, output: f64) -> BTreeMap<String, ModelPriceOverride> {
        let price = |value| UsdPerMillion::new(value).unwrap();
        BTreeMap::from([(
            "my-model".to_string(),
            ModelPriceOverride {
                input_cache_miss: price(miss),
                input_cache_hit: price(hit),
                output: price(output),
            },
        )])
    }

    #[test]
    fn overrides_use_fixed_prices_at_every_hour() {
        let overrides = override_prices(1.0, 0.5, 2.0);
        let used = usage(1_000_000, 1_000_000, 500_000);
        for clock in [off_peak_instant as PricingClock, peak_instant] {
            let tracker =
                CostTracker::with_overrides(Some("my-model"), &overrides).with_clock(clock);
            assert!((cost(tracker, used) - (0.5 + 0.25 + 2.0)).abs() < 1e-12);
        }
        // Models without an override keep DeepSeek's prices.
        let flash =
            CostTracker::with_overrides(Some(FLASH_MODEL), &overrides).with_clock(off_peak_instant);
        let plain = CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant);
        assert_eq!(cost(flash, used), cost(plain, used));
    }

    #[test]
    fn a_zero_price_override_costs_nothing_at_peak() {
        let overrides = override_prices(0.0, 0.0, 0.0);
        let tracker =
            CostTracker::with_overrides(Some("my-model"), &overrides).with_clock(peak_instant);
        assert_eq!(cost(tracker, usage(5_000_000, 2_000_000, 1_000_000)), 0.0);
    }

    #[test]
    fn usd_to_micros_rounds_and_rejects_invalid_values() {
        assert_eq!(usd_to_micros(0.000_001_5), 2);
        assert_eq!(usd_to_micros(0.0), 0);
        assert_eq!(usd_to_micros(-0.1), 0);
        assert_eq!(usd_to_micros(f64::NAN), 0);
    }
}
