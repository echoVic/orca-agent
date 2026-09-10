use orca_core::cost_types::UsageTotals;
use orca_core::model::{PRO_MODEL, canonical_model_name};
use orca_core::provider_types::Usage;

pub(crate) fn usd_to_micros(usd: f64) -> u64 {
    if usd.is_finite() && usd > 0.0 {
        (usd * 1_000_000.0).round().min(u64::MAX as f64) as u64
    } else {
        0
    }
}

#[derive(Clone, Debug)]
pub struct CostTracker {
    totals: UsageTotals,
    pricing: ModelPricing,
}

#[derive(Clone, Copy, Debug)]
struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
    cache_per_million: f64,
}

impl CostTracker {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            totals: UsageTotals::default(),
            pricing: ModelPricing::for_model(model),
        }
    }

    pub fn add_usage(&mut self, usage: Usage) -> UsageTotals {
        self.totals.input_tokens += usage.input_tokens;
        self.totals.output_tokens += usage.output_tokens;
        self.totals.cache_tokens += usage.cache_tokens;
        self.totals.estimated_cost_usd += self.pricing.estimate(usage);
        self.totals
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.pricing = ModelPricing::for_model(model);
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

impl ModelPricing {
    fn for_model(model: Option<&str>) -> Self {
        match canonical_model_name(model.unwrap_or("")) {
            model if model == PRO_MODEL || model.contains("v4-pro") => Self {
                input_per_million: 0.435,
                output_per_million: 0.87,
                cache_per_million: 0.044,
            },
            // Flash is the default and the low-cost option when the model is omitted.
            _ => Self {
                input_per_million: 0.14,
                output_per_million: 0.28,
                cache_per_million: 0.014,
            },
        }
    }

    fn estimate(self, usage: Usage) -> f64 {
        // DeepSeek pricing: cache_tokens are a subset of input_tokens that hit cache.
        // Charge: (input - cache) at input price, cache at cache price, output at output price.
        let non_cache_input = usage.input_tokens.saturating_sub(usage.cache_tokens);
        (non_cache_input as f64 * self.input_per_million
            + usage.cache_tokens as f64 * self.cache_per_million
            + usage.output_tokens as f64 * self.output_per_million)
            / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_tokens_and_cost() {
        let mut tracker = CostTracker::new(Some(orca_core::model::FLASH_MODEL));

        let totals = tracker.add_usage(Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
        });

        assert_eq!(totals.input_tokens, 120);
        assert_eq!(totals.output_tokens, 30);
        assert_eq!(totals.cache_tokens, 10);
        // total_tokens = input + output (cache is subset of input)
        assert_eq!(totals.total_tokens(), 150);
        assert!(totals.estimated_cost_usd > 0.0);
        // Flash: (120-10)*0.14 + 10*0.014 + 30*0.28 = 23.94 per million.
        let expected = (110.0 * 0.14 + 10.0 * 0.014 + 30.0 * 0.28) / 1_000_000.0;
        assert!((totals.estimated_cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn merge_accumulates_from_child_tracker() {
        let mut parent = CostTracker::new(Some(orca_core::model::FLASH_MODEL));
        parent.add_usage(Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_tokens: 20,
        });

        let mut child = CostTracker::new(Some(orca_core::model::LEGACY_VISION_MODEL));
        child.add_usage(Usage {
            input_tokens: 200,
            output_tokens: 80,
            cache_tokens: 30,
        });

        let parent_cost_before = parent.totals.estimated_cost_usd;
        let child_cost = child.totals.estimated_cost_usd;

        parent.merge(&child);

        assert_eq!(parent.totals.input_tokens, 300);
        assert_eq!(parent.totals.output_tokens, 130);
        assert_eq!(parent.totals.cache_tokens, 50);
        assert!(
            (parent.totals.estimated_cost_usd - (parent_cost_before + child_cost)).abs() < 1e-12
        );
    }

    #[test]
    fn retired_flash_aliases_use_canonical_flash_pricing() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_tokens: 100_000,
        };
        let mut canonical = CostTracker::new(Some(orca_core::model::FLASH_MODEL));
        let mut legacy_text = CostTracker::new(Some(orca_core::model::LEGACY_FLASH_MODEL));
        let mut legacy_vision = CostTracker::new(Some(orca_core::model::LEGACY_VISION_MODEL));

        assert_eq!(
            canonical.add_usage(usage).estimated_cost_usd,
            legacy_text.add_usage(usage).estimated_cost_usd
        );
        assert_eq!(
            canonical.totals().estimated_cost_usd,
            legacy_vision.add_usage(usage).estimated_cost_usd
        );
    }

    #[test]
    fn experimental_pro_names_keep_pro_pricing() {
        let usage = Usage {
            input_tokens: 900_000,
            output_tokens: 100_000,
            cache_tokens: 200_000,
        };
        assert_eq!(
            ModelPricing::for_model(Some("deepseek-v4-pro-exp")).estimate(usage),
            ModelPricing::for_model(Some(PRO_MODEL)).estimate(usage)
        );
    }

    #[test]
    fn usd_to_micros_rounds_and_rejects_invalid_values() {
        assert_eq!(usd_to_micros(0.000_001_5), 2);
        assert_eq!(usd_to_micros(0.0), 0);
        assert_eq!(usd_to_micros(-0.1), 0);
        assert_eq!(usd_to_micros(f64::NAN), 0);
    }
}
