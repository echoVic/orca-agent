use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tool_types::ToolServiceUsage;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    /// Everything the usage cost: tokens plus the per-use service fees.
    pub estimated_cost_usd: f64,
    /// The part of the cost that is per-use service fees, and what was billed.
    /// Absent from a record with no fees, so those records are unchanged.
    #[serde(default, skip_serializing_if = "ServiceUsageTotals::is_empty")]
    pub service: ServiceUsageTotals,
}

/// What services billed per use, apart from tokens: the fees, already counted
/// in [`UsageTotals::estimated_cost_usd`], and the billed units behind them.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ServiceUsageTotals {
    pub cost_usd: f64,
    /// DeepSeek Search API requests that returned results.
    pub deepseek_searches: u64,
    /// DeepSeek Fetch API requests that returned a page.
    pub deepseek_fetches: u64,
    /// Pages DeepSeek OCR recognized.
    pub deepseek_ocr_pages: u64,
}

impl ServiceUsageTotals {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// What was billed, for a person to read: "2 searches, 1 OCR page".
    /// Empty when nothing was.
    pub fn billed_units(&self) -> String {
        [
            (self.deepseek_searches, "search", "searches"),
            (self.deepseek_fetches, "fetch", "fetches"),
            (self.deepseek_ocr_pages, "OCR page", "OCR pages"),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, one, many)| format!("{count} {}", if count == 1 { one } else { many }))
        .collect::<Vec<_>>()
        .join(", ")
    }

    /// Counts what one tool call was billed for, at the fee it cost.
    pub fn record(&mut self, usage: ToolServiceUsage, cost_usd: f64) {
        self.cost_usd += cost_usd;
        self.deepseek_searches = self
            .deepseek_searches
            .saturating_add(u64::from(usage.deepseek_searches));
        self.deepseek_fetches = self
            .deepseek_fetches
            .saturating_add(u64::from(usage.deepseek_fetches));
        self.deepseek_ocr_pages = self
            .deepseek_ocr_pages
            .saturating_add(u64::from(usage.deepseek_ocr_pages));
    }

    fn plus(self, other: Self) -> Self {
        Self {
            cost_usd: self.cost_usd + other.cost_usd,
            deepseek_searches: self
                .deepseek_searches
                .saturating_add(other.deepseek_searches),
            deepseek_fetches: self.deepseek_fetches.saturating_add(other.deepseek_fetches),
            deepseek_ocr_pages: self
                .deepseek_ocr_pages
                .saturating_add(other.deepseek_ocr_pages),
        }
    }

    fn minus(self, other: Self) -> Self {
        Self {
            cost_usd: (self.cost_usd - other.cost_usd).max(0.0),
            deepseek_searches: self
                .deepseek_searches
                .saturating_sub(other.deepseek_searches),
            deepseek_fetches: self.deepseek_fetches.saturating_sub(other.deepseek_fetches),
            deepseek_ocr_pages: self
                .deepseek_ocr_pages
                .saturating_sub(other.deepseek_ocr_pages),
        }
    }
}

impl UsageTotals {
    pub fn total_tokens(self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// The part of the cost that is tokens.
    pub fn token_cost_usd(self) -> f64 {
        (self.estimated_cost_usd - self.service.cost_usd).max(0.0)
    }

    /// What a usage event carries: the totals, with the service fees listed
    /// apart from the cost they are already part of.
    pub fn event_payload(self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_tokens": self.cache_tokens,
            "total_tokens": self.total_tokens(),
            "estimated_cost_usd": self.estimated_cost_usd,
            "service_cost_usd": self.service.cost_usd,
            "service_usage": {
                "deepseek_searches": self.service.deepseek_searches,
                "deepseek_fetches": self.service.deepseek_fetches,
                "deepseek_ocr_pages": self.service.deepseek_ocr_pages
            }
        })
    }

    /// Reads [`Self::event_payload`] back. A payload from before service fees
    /// were listed has none.
    pub fn from_event_payload(payload: &Value) -> Self {
        let count = |value: &Value| value.as_u64().unwrap_or_default();
        let service = &payload["service_usage"];
        Self {
            input_tokens: count(&payload["input_tokens"]),
            output_tokens: count(&payload["output_tokens"]),
            cache_tokens: count(&payload["cache_tokens"]),
            estimated_cost_usd: payload["estimated_cost_usd"].as_f64().unwrap_or_default(),
            service: ServiceUsageTotals {
                cost_usd: payload["service_cost_usd"].as_f64().unwrap_or_default(),
                deepseek_searches: count(&service["deepseek_searches"]),
                deepseek_fetches: count(&service["deepseek_fetches"]),
                deepseek_ocr_pages: count(&service["deepseek_ocr_pages"]),
            },
        }
    }

    /// Both usages together.
    pub fn plus(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_tokens: self.cache_tokens.saturating_add(other.cache_tokens),
            estimated_cost_usd: self.estimated_cost_usd + other.estimated_cost_usd,
            service: self.service.plus(other.service),
        }
    }

    /// What is left of this usage without `other`, never below zero.
    pub fn minus(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(other.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(other.output_tokens),
            cache_tokens: self.cache_tokens.saturating_sub(other.cache_tokens),
            estimated_cost_usd: (self.estimated_cost_usd - other.estimated_cost_usd).max(0.0),
            service: self.service.minus(other.service),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn billed() -> UsageTotals {
        UsageTotals {
            input_tokens: 10,
            output_tokens: 4,
            cache_tokens: 2,
            estimated_cost_usd: 0.5,
            service: ServiceUsageTotals {
                cost_usd: 0.25,
                deepseek_searches: 3,
                deepseek_fetches: 2,
                deepseek_ocr_pages: 7,
            },
        }
    }

    #[test]
    fn a_record_written_before_service_fees_reads_back_without_any() {
        let old =
            r#"{"input_tokens":10,"output_tokens":4,"cache_tokens":2,"estimated_cost_usd":0.5}"#;
        let totals: UsageTotals = serde_json::from_str(old).unwrap();
        assert_eq!(totals.estimated_cost_usd, 0.5);
        assert!(totals.service.is_empty());
    }

    #[test]
    fn totals_without_service_fees_serialize_as_they_always_did() {
        let totals = UsageTotals {
            service: ServiceUsageTotals::default(),
            ..billed()
        };
        assert_eq!(
            serde_json::to_string(&totals).unwrap(),
            r#"{"input_tokens":10,"output_tokens":4,"cache_tokens":2,"estimated_cost_usd":0.5}"#
        );
    }

    #[test]
    fn service_fees_survive_a_round_trip() {
        let json = serde_json::to_string(&billed()).unwrap();
        assert_eq!(
            serde_json::from_str::<UsageTotals>(&json).unwrap(),
            billed()
        );
    }

    #[test]
    fn token_cost_is_what_the_service_fees_leave() {
        assert!((billed().token_cost_usd() - 0.25).abs() < 1e-12);
        let rounding = UsageTotals {
            estimated_cost_usd: 0.1,
            service: ServiceUsageTotals {
                cost_usd: 0.1000000001,
                ..ServiceUsageTotals::default()
            },
            ..UsageTotals::default()
        };
        assert_eq!(rounding.token_cost_usd(), 0.0);
    }

    #[test]
    fn plus_adds_every_field_and_minus_takes_it_back() {
        let sum = billed().plus(billed());
        assert_eq!(sum.input_tokens, 20);
        assert_eq!(sum.cache_tokens, 4);
        assert_eq!(sum.estimated_cost_usd, 1.0);
        assert_eq!(sum.service.cost_usd, 0.5);
        assert_eq!(sum.service.deepseek_searches, 6);
        assert_eq!(sum.service.deepseek_ocr_pages, 14);
        assert_eq!(sum.minus(billed()), billed());
    }

    #[test]
    fn an_event_payload_lists_the_service_fees_and_reads_back() {
        let payload = billed().event_payload();
        assert_eq!(payload["total_tokens"], 14);
        assert_eq!(payload["estimated_cost_usd"], 0.5);
        assert_eq!(payload["service_cost_usd"], 0.25);
        assert_eq!(payload["service_usage"]["deepseek_searches"], 3);
        assert_eq!(payload["service_usage"]["deepseek_fetches"], 2);
        assert_eq!(payload["service_usage"]["deepseek_ocr_pages"], 7);
        assert_eq!(UsageTotals::from_event_payload(&payload), billed());
    }

    #[test]
    fn an_event_payload_without_fees_still_names_them() {
        let payload = UsageTotals::default().event_payload();
        assert_eq!(payload["service_cost_usd"], 0.0);
        assert_eq!(payload["service_usage"]["deepseek_searches"], 0);
    }

    #[test]
    fn an_event_payload_from_before_service_fees_reads_back_without_any() {
        let old = serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 4,
            "cache_tokens": 2,
            "total_tokens": 14,
            "estimated_cost_usd": 0.5
        });
        let totals = UsageTotals::from_event_payload(&old);
        assert_eq!(totals.input_tokens, 10);
        assert_eq!(totals.estimated_cost_usd, 0.5);
        assert!(totals.service.is_empty());
    }

    #[test]
    fn recording_a_tool_call_counts_what_it_was_billed_for() {
        let mut service = ServiceUsageTotals::default();
        let call = ToolServiceUsage {
            deepseek_searches: 1,
            deepseek_fetches: 2,
            deepseek_ocr_pages: 5,
        };
        service.record(call, 0.01);
        service.record(call, 0.01);
        assert_eq!(service.cost_usd, 0.02);
        assert_eq!(service.deepseek_searches, 2);
        assert_eq!(service.deepseek_fetches, 4);
        assert_eq!(service.deepseek_ocr_pages, 10);
    }

    #[test]
    fn billed_units_name_only_what_was_billed() {
        let all = ServiceUsageTotals {
            cost_usd: 0.0,
            deepseek_searches: 2,
            deepseek_fetches: 1,
            deepseek_ocr_pages: 1,
        };
        assert_eq!(all.billed_units(), "2 searches, 1 fetch, 1 OCR page");
        let some = ServiceUsageTotals {
            deepseek_searches: 1,
            deepseek_fetches: 0,
            deepseek_ocr_pages: 12,
            ..all
        };
        assert_eq!(some.billed_units(), "1 search, 12 OCR pages");
        let fetches = ServiceUsageTotals {
            deepseek_searches: 0,
            deepseek_fetches: 3,
            deepseek_ocr_pages: 0,
            ..all
        };
        assert_eq!(fetches.billed_units(), "3 fetches");
        assert_eq!(ServiceUsageTotals::default().billed_units(), "");
    }

    #[test]
    fn minus_never_goes_below_zero() {
        assert_eq!(
            UsageTotals::default().minus(billed()),
            UsageTotals::default()
        );
    }
}
