//! DeepSeek's published prices as of a fixed date, and its time-of-use rule:
//! peak hours on Beijing weekdays cost twice the off-peak price.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Timelike, Utc, Weekday};
use orca_core::model::{FLASH_MODEL, PRO_MODEL};
use orca_core::provider_types::Usage;
use serde::{Deserialize, Serialize};

/// Prices in US dollars per million tokens.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Rates {
    pub input_cache_miss: f64,
    pub input_cache_hit: f64,
    pub output: f64,
}

/// Off-peak prices from https://api-docs.deepseek.com/quick_start/pricing as of
/// [`builtin_as_of`]. Peak hours cost [`PEAK_MULTIPLIER`] times as much.
pub const FLASH_RATES: Rates = Rates {
    input_cache_miss: 0.15,
    input_cache_hit: 0.003,
    output: 0.6,
};
pub const PRO_RATES: Rates = Rates {
    input_cache_miss: 0.66,
    input_cache_hit: 0.022,
    output: 1.98,
};

pub const PEAK_MULTIPLIER: f64 = 2.0;

/// DeepSeek states its peak hours and holidays in Beijing time (UTC+8, no DST).
const BEIJING_UTC_OFFSET_SECS: i32 = 8 * 60 * 60;

/// Peak hours as `[start, end)` Beijing hours, Monday to Friday.
const PEAK_HOURS: [(u32, u32); 2] = [(9, 12), (14, 18)];

/// Weekdays that are days off in mainland China, so off-peak all day. From the
/// State Council's yearly notice; 2026:
/// https://www.gov.cn/zhengce/zhengceku/202511/content_7047091.htm
const WEEKDAY_HOLIDAYS: &[(i32, u32, u32)] = &[
    (2026, 1, 1),
    (2026, 1, 2),
    (2026, 2, 16),
    (2026, 2, 17),
    (2026, 2, 18),
    (2026, 2, 19),
    (2026, 2, 20),
    (2026, 2, 23),
    (2026, 4, 6),
    (2026, 5, 1),
    (2026, 5, 4),
    (2026, 5, 5),
    (2026, 6, 19),
    (2026, 9, 25),
    (2026, 10, 1),
    (2026, 10, 2),
    (2026, 10, 5),
    (2026, 10, 6),
    (2026, 10, 7),
];

/// The day the built-in prices were checked against DeepSeek's pricing page.
pub fn builtin_as_of() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 20).expect("valid built-in date")
}

/// The built-in prices, by canonical model name.
pub fn builtin_rates() -> [(&'static str, Rates); 2] {
    [(FLASH_MODEL, FLASH_RATES), (PRO_MODEL, PRO_RATES)]
}

/// Whether DeepSeek bills a request made at `at` at its peak price.
pub fn is_peak(at: DateTime<Utc>) -> bool {
    let offset = FixedOffset::east_opt(BEIJING_UTC_OFFSET_SECS).expect("valid UTC offset");
    let beijing = at.with_timezone(&offset);
    if matches!(beijing.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    let date = beijing.date_naive();
    if WEEKDAY_HOLIDAYS
        .iter()
        .any(|&(year, month, day)| NaiveDate::from_ymd_opt(year, month, day) == Some(date))
    {
        return false;
    }
    let hour = beijing.hour();
    PEAK_HOURS
        .iter()
        .any(|&(start, end)| (start..end).contains(&hour))
}

pub fn time_of_use_multiplier(at: DateTime<Utc>) -> f64 {
    if is_peak(at) { PEAK_MULTIPLIER } else { 1.0 }
}

/// The cost of `usage` at `rates` times `multiplier`. `cache_tokens` are the
/// part of `input_tokens` that hit the cache.
pub fn cost_usd(rates: Rates, multiplier: f64, usage: Usage) -> f64 {
    let cache_miss = usage.input_tokens.saturating_sub(usage.cache_tokens);
    (cache_miss as f64 * rates.input_cache_miss
        + usage.cache_tokens as f64 * rates.input_cache_hit
        + usage.output_tokens as f64 * rates.output)
        * multiplier
        / 1_000_000.0
}

/// Saturday 2026-10-17, 10:00 in Beijing: off-peak. A clock for tests.
#[doc(hidden)]
pub fn off_peak_instant() -> DateTime<Utc> {
    "2026-10-17T02:00:00Z".parse().expect("valid instant")
}

/// Monday 2026-10-19, 10:00 in Beijing, a working day: peak. A clock for tests.
#[doc(hidden)]
pub fn peak_instant() -> DateTime<Utc> {
    "2026-10-19T02:00:00Z".parse().expect("valid instant")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().expect("valid UTC instant")
    }

    /// `text` is a Beijing wall-clock time such as `2026-10-19T09:00:00`.
    fn beijing(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("{text}+08:00"))
            .expect("valid Beijing time")
            .with_timezone(&Utc)
    }

    #[test]
    fn peak_windows_follow_beijing_hours_on_weekdays() {
        // Monday 2026-10-19 is an ordinary working day.
        for (time, peak) in [
            ("08:59:59", false),
            ("09:00:00", true),
            ("11:59:59", true),
            ("12:00:00", false),
            ("13:59:59", false),
            ("14:00:00", true),
            ("17:59:59", true),
            ("18:00:00", false),
        ] {
            assert_eq!(
                is_peak(beijing(&format!("2026-10-19T{time}"))),
                peak,
                "{time}"
            );
        }
    }

    #[test]
    fn weekends_are_off_peak_all_day_even_as_make_up_workdays() {
        assert!(!is_peak(beijing("2026-10-17T10:00:00"))); // Saturday
        assert!(!is_peak(beijing("2026-10-18T15:00:00"))); // Sunday
        assert!(!is_peak(beijing("2026-10-10T10:00:00"))); // Saturday make-up workday
    }

    #[test]
    fn weekday_holidays_are_off_peak_all_day() {
        assert!(!is_peak(beijing("2026-10-05T10:00:00"))); // Monday, National Day holiday
        assert!(!is_peak(beijing("2026-02-16T15:00:00"))); // Monday, Spring Festival
        assert!(is_peak(beijing("2026-10-09T10:00:00"))); // Friday after the holiday
    }

    #[test]
    fn holidays_and_weekdays_use_the_beijing_calendar() {
        // 01:30 UTC on 1 October is 09:30 on the National Day holiday in Beijing.
        assert!(!is_peak(utc("2026-10-01T01:30:00Z")));
        // 01:30 UTC on 8 October is 09:30 on an ordinary Thursday in Beijing.
        assert!(is_peak(utc("2026-10-08T01:30:00Z")));
        // 23:00 UTC on Sunday is 07:00 on Monday in Beijing: before the window.
        assert!(!is_peak(utc("2026-10-18T23:00:00Z")));
        // 06:30 UTC on Monday is 14:30 in Beijing: the afternoon window.
        assert!(is_peak(utc("2026-10-19T06:30:00Z")));
    }

    #[test]
    fn the_peak_multiplier_doubles_the_cost() {
        assert_eq!(time_of_use_multiplier(peak_instant()), PEAK_MULTIPLIER);
        assert_eq!(time_of_use_multiplier(off_peak_instant()), 1.0);
        assert_eq!(PEAK_MULTIPLIER, 2.0);
    }

    #[test]
    fn cost_matches_the_published_prices_at_both_tiers() {
        // Issue #137's bug-fix run: 12,698 cache-miss and 60,672 cache-hit
        // input tokens, 2,052 output tokens.
        let usage = Usage {
            input_tokens: 12_698 + 60_672,
            output_tokens: 2_052,
            cache_tokens: 60_672,
        };
        let expected = (12_698.0 * 0.15 + 60_672.0 * 0.003 + 2_052.0 * 0.6) / 1_000_000.0;
        assert!((cost_usd(FLASH_RATES, 1.0, usage) - expected).abs() < 1e-12);
        assert!((cost_usd(FLASH_RATES, PEAK_MULTIPLIER, usage) - 2.0 * expected).abs() < 1e-12);
        let pro = (12_698.0 * 0.66 + 60_672.0 * 0.022 + 2_052.0 * 1.98) / 1_000_000.0;
        assert!((cost_usd(PRO_RATES, 1.0, usage) - pro).abs() < 1e-12);
    }

    #[test]
    fn builtin_table_matches_the_published_off_peak_prices() {
        assert_eq!(
            builtin_as_of(),
            NaiveDate::from_ymd_opt(2026, 10, 20).unwrap()
        );
        let table = builtin_rates();
        assert_eq!(
            table[0],
            (
                FLASH_MODEL,
                Rates {
                    input_cache_miss: 0.15,
                    input_cache_hit: 0.003,
                    output: 0.6
                }
            )
        );
        assert_eq!(
            table[1],
            (
                PRO_MODEL,
                Rates {
                    input_cache_miss: 0.66,
                    input_cache_hit: 0.022,
                    output: 1.98
                }
            )
        );
    }

    #[test]
    fn the_holiday_table_only_lists_valid_weekdays() {
        for &(year, month, day) in WEEKDAY_HOLIDAYS {
            let date = NaiveDate::from_ymd_opt(year, month, day).expect("valid holiday date");
            assert!(
                !matches!(date.weekday(), Weekday::Sat | Weekday::Sun),
                "{date}"
            );
        }
        assert_eq!(WEEKDAY_HOLIDAYS.len(), 19);
    }

    #[test]
    fn test_clocks_land_in_their_tiers() {
        assert!(!is_peak(off_peak_instant()));
        assert!(is_peak(peak_instant()));
    }
}
