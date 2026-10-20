//! The prices estimates use: the built-in table, overlaid by the prices last
//! fetched from models.dev and cached in `$ORCA_HOME/pricing-cache.json`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{DateTime, NaiveDate, Utc};
use orca_core::model::{FLASH_MODEL, PRO_MODEL};
use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use serde::{Deserialize, Serialize};

use super::rates::{self, Rates};

pub const CACHE_FILE: &str = "pricing-cache.json";
pub const CACHE_VERSION: u32 = 2;
/// The highest fetched or cached price, in US dollars per million tokens.
const MAX_PRICE: f64 = 100.0;
/// How far a fetched price may move from the built-in one before it is taken
/// for a unit error (per token, per thousand tokens).
const MAX_DRIFT: f64 = 10.0;

/// A model's off-peak prices and the date its source content was updated.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct CachedRates {
    #[serde(flatten)]
    pub rates: Rates,
    pub updated: NaiveDate,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PricingCache {
    pub version: u32,
    pub source: String,
    pub fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub models: BTreeMap<String, CachedRates>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PriceSource {
    Builtin {
        as_of: NaiveDate,
    },
    Fetched {
        source: String,
        fetched_at: DateTime<Utc>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PriceBook {
    pub models: BTreeMap<String, Rates>,
    pub source: PriceSource,
}

impl PriceBook {
    /// Off-peak rates for a canonical model name: its own, else Pro's for a
    /// `v4-pro` variant, else Flash's (the default model).
    pub fn rates_for(&self, model: &str) -> Rates {
        if let Some(rates) = self.models.get(model) {
            return *rates;
        }
        if model.contains("v4-pro")
            && let Some(rates) = self.models.get(PRO_MODEL)
        {
            return *rates;
        }
        self.models
            .get(FLASH_MODEL)
            .copied()
            .unwrap_or(rates::FLASH_RATES)
    }
}

/// Whether fetched or cached prices are plausible: within ten times the
/// model's built-in prices, or the Flash/Pro range for a new model.
pub fn rates_are_plausible(model: &str, rates: Rates) -> bool {
    let prices = [rates.input_cache_miss, rates.input_cache_hit, rates.output];
    if prices
        .iter()
        .any(|price| !price.is_finite() || *price < 0.0 || *price > MAX_PRICE)
    {
        return false;
    }
    let (a, b) = rates::builtin_rates()
        .into_iter()
        .find(|(name, _)| *name == model)
        .map(|(_, builtin)| (builtin, builtin))
        .unwrap_or((rates::FLASH_RATES, rates::PRO_RATES));
    within_drift(
        a.input_cache_miss,
        b.input_cache_miss,
        rates.input_cache_miss,
    ) && within_drift(a.input_cache_hit, b.input_cache_hit, rates.input_cache_hit)
        && within_drift(a.output, b.output, rates.output)
}

fn within_drift(a: f64, b: f64, price: f64) -> bool {
    // Decimal endpoint prices can round just outside bounds computed in f64.
    const RELATIVE_TOLERANCE: f64 = 1e-9;
    let lower = a.min(b) / MAX_DRIFT;
    let upper = a.max(b) * MAX_DRIFT;
    price >= lower * (1.0 - RELATIVE_TOLERANCE) && price <= upper * (1.0 + RELATIVE_TOLERANCE)
}

/// The built-in table overlaid by cached content updated on or after the
/// built-in date. Older content only adds models the built-in table lacks;
/// fetching unchanged content again does not change its precedence.
pub fn merge(cache: Option<&PricingCache>) -> PriceBook {
    let mut models: BTreeMap<String, Rates> = rates::builtin_rates()
        .into_iter()
        .map(|(model, rates)| (model.to_string(), rates))
        .collect();
    let as_of = rates::builtin_as_of();
    let Some(cache) = cache else {
        return PriceBook {
            models,
            source: PriceSource::Builtin { as_of },
        };
    };
    let mut uses_cache = false;
    for (model, entry) in &cache.models {
        if entry.updated >= as_of || !models.contains_key(model) {
            models.insert(model.clone(), entry.rates);
            uses_cache = true;
        }
    }
    let source = if uses_cache {
        PriceSource::Fetched {
            source: cache.source.clone(),
            fetched_at: cache.fetched_at,
        }
    } else {
        PriceSource::Builtin { as_of }
    };
    PriceBook { models, source }
}

pub fn cache_path() -> Option<PathBuf> {
    orca_core::home::orca_home().map(|home| home.join(CACHE_FILE))
}

/// The cache at `path`, without its implausible entries; `None` when it is
/// missing, unreadable, from another version, over a day in the future, or empty.
pub fn read_cache(path: &Path, now: DateTime<Utc>) -> Option<PricingCache> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut cache: PricingCache = serde_json::from_str(&text).ok()?;
    if cache.version != CACHE_VERSION
        || cache.fetched_at.signed_duration_since(now) > chrono::TimeDelta::days(1)
    {
        return None;
    }
    cache
        .models
        .retain(|model, entry| rates_are_plausible(model, entry.rates));
    (!cache.models.is_empty()).then_some(cache)
}

pub fn write_cache(path: &Path, cache: &PricingCache) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(cache).map_err(io::Error::other)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, AtomicWritePolicy::NoFollow).map_err(io::Error::other)
}

fn global_user_overrides() -> &'static RwLock<Arc<BTreeMap<String, Rates>>> {
    static OVERRIDES: OnceLock<RwLock<Arc<BTreeMap<String, Rates>>>> = OnceLock::new();
    OVERRIDES.get_or_init(|| RwLock::new(Arc::default()))
}

/// Replace the user-config map, installed by both build_run_config (including
/// detached workers) and InteractiveSession construction (direct RunConfig callers).
pub(crate) fn set_user_overrides(overrides: BTreeMap<String, Rates>) {
    *global_user_overrides()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(overrides);
}

/// A snapshot of fixed prices from the user config for newly created trackers.
pub fn user_overrides() -> Arc<BTreeMap<String, Rates>> {
    global_user_overrides()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn global() -> &'static RwLock<Arc<PriceBook>> {
    static BOOK: OnceLock<RwLock<Arc<PriceBook>>> = OnceLock::new();
    BOOK.get_or_init(|| {
        let cache = cache_path().and_then(|path| read_cache(&path, Utc::now()));
        RwLock::new(Arc::new(merge(cache.as_ref())))
    })
}

/// The process-wide book: loaded from the cache on first use, replaced when a
/// refresh lands.
pub fn current() -> Arc<PriceBook> {
    global()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub(crate) fn install(cache: &PricingCache) {
    let book = Arc::new(merge(Some(cache)));
    *global()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = book;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::rates::{FLASH_RATES, PRO_RATES};
    use orca_core::model::{FLASH_MODEL, PRO_MODEL};

    const FETCHED_FLASH: Rates = Rates {
        input_cache_miss: 0.2,
        input_cache_hit: 0.004,
        output: 0.8,
    };
    const NEW_MODEL: Rates = Rates {
        input_cache_miss: 0.5,
        input_cache_hit: 0.01,
        output: 2.0,
    };

    fn now() -> DateTime<Utc> {
        "2026-10-20T08:00:00Z".parse().unwrap()
    }

    fn cache(fetched_at: &str) -> PricingCache {
        PricingCache {
            version: CACHE_VERSION,
            source: "https://models.dev/api.json".to_string(),
            fetched_at: fetched_at.parse().unwrap(),
            etag: Some("W/\"v1\"".to_string()),
            models: BTreeMap::from([
                (
                    FLASH_MODEL.to_string(),
                    CachedRates {
                        rates: FETCHED_FLASH,
                        updated: fetched_at.parse::<DateTime<Utc>>().unwrap().date_naive(),
                    },
                ),
                (
                    "deepseek-v5-flash".to_string(),
                    CachedRates {
                        rates: NEW_MODEL,
                        updated: fetched_at.parse::<DateTime<Utc>>().unwrap().date_naive(),
                    },
                ),
            ]),
        }
    }

    fn content_dated_cache(updated: &str, add_model: bool) -> PricingCache {
        let mut value = serde_json::json!({
            "version": CACHE_VERSION,
            "source": "https://models.dev/api.json",
            "fetched_at": "2026-10-20T08:00:00Z",
            "models": {"deepseek-flash": {
                "input_cache_miss": 0.2, "input_cache_hit": 0.004, "output": 0.8,
                "updated": updated
            }}
        });
        if add_model {
            value["models"]["deepseek-v5-flash"] = serde_json::json!({
                "input_cache_miss": 0.5, "input_cache_hit": 0.01, "output": 2.0,
                "updated": updated
            });
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn recent_fetch_of_old_content_keeps_newer_builtin_prices() {
        let book = merge(Some(&content_dated_cache("2026-10-18", false)));
        assert_eq!(book.rates_for(FLASH_MODEL), FLASH_RATES);
        assert!(matches!(book.source, PriceSource::Builtin { .. }));
    }

    #[test]
    fn content_updated_on_or_after_builtin_date_wins() {
        for updated in ["2026-10-19", "2026-10-20"] {
            let book = merge(Some(&content_dated_cache(updated, false)));
            assert_eq!(book.rates_for(FLASH_MODEL), FETCHED_FLASH);
            assert!(matches!(book.source, PriceSource::Fetched { .. }));
        }
    }

    #[test]
    fn old_content_adding_a_model_counts_as_fetched_prices() {
        let book = merge(Some(&content_dated_cache("2026-10-18", true)));
        assert_eq!(book.rates_for(FLASH_MODEL), FLASH_RATES);
        assert_eq!(book.rates_for("deepseek-v5-flash"), NEW_MODEL);
        assert!(matches!(book.source, PriceSource::Fetched { .. }));
    }

    #[test]
    fn version_one_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        std::fs::write(&path, r#"{"version":1,"source":"https://models.dev/api.json","fetched_at":"2026-10-20T08:00:00Z","models":{"deepseek-flash":{"input_cache_miss":0.2,"input_cache_hit":0.004,"output":0.8}}}"#).unwrap();
        assert!(read_cache(&path, now()).is_none());
    }

    #[test]
    fn without_a_cache_the_book_is_the_builtin_table() {
        let book = merge(None);
        assert_eq!(book.rates_for(FLASH_MODEL), FLASH_RATES);
        assert_eq!(book.rates_for(PRO_MODEL), PRO_RATES);
        assert_eq!(
            book.source,
            PriceSource::Builtin {
                as_of: crate::pricing::rates::builtin_as_of()
            }
        );
    }

    #[test]
    fn a_current_cache_overrides_builtin_prices_and_adds_models() {
        let book = merge(Some(&cache("2026-10-20T08:00:00Z")));
        assert_eq!(book.rates_for(FLASH_MODEL), FETCHED_FLASH);
        assert_eq!(book.rates_for(PRO_MODEL), PRO_RATES);
        assert_eq!(book.rates_for("deepseek-v5-flash"), NEW_MODEL);
        assert!(matches!(book.source, PriceSource::Fetched { .. }));
    }

    #[test]
    fn a_cache_older_than_the_builtin_table_only_adds_models() {
        let book = merge(Some(&cache("2026-09-01T08:00:00Z")));
        assert_eq!(book.rates_for(FLASH_MODEL), FLASH_RATES);
        assert_eq!(book.rates_for("deepseek-v5-flash"), NEW_MODEL);
        assert!(matches!(book.source, PriceSource::Fetched { .. }));
    }

    #[test]
    fn unknown_models_fall_back_to_pro_for_v4_pro_names_and_flash_otherwise() {
        let book = merge(None);
        assert_eq!(book.rates_for("deepseek-v4-pro-exp"), PRO_RATES);
        assert_eq!(book.rates_for("qwen-max"), FLASH_RATES);
        assert_eq!(book.rates_for(""), FLASH_RATES);
    }

    #[test]
    fn plausibility_rejects_unit_errors_and_impossible_numbers() {
        let flash = |input_cache_miss| Rates {
            input_cache_miss,
            ..FLASH_RATES
        };
        assert!(rates_are_plausible(FLASH_MODEL, FLASH_RATES));
        assert!(rates_are_plausible(FLASH_MODEL, flash(1.4))); // under 10x
        assert!(!rates_are_plausible(FLASH_MODEL, flash(1.6))); // over 10x
        assert!(!rates_are_plausible(FLASH_MODEL, flash(0.15e-6))); // per token
        assert!(!rates_are_plausible(FLASH_MODEL, flash(0.15e-3))); // per thousand
        assert!(!rates_are_plausible(FLASH_MODEL, flash(-0.15)));
        assert!(!rates_are_plausible(FLASH_MODEL, flash(f64::NAN)));
        assert!(!rates_are_plausible(
            "unknown",
            Rates {
                output: 101.0,
                ..FLASH_RATES
            }
        ));
        assert!(rates_are_plausible("unknown", NEW_MODEL));
    }

    fn assert_price_bounds(model: &str, baseline: Rates, bounds: [(f64, f64); 3]) {
        for (field, (lower, upper)) in bounds.into_iter().enumerate() {
            for (price, accepted) in [
                (lower, true),
                (upper, true),
                (lower * 0.99, false),
                (upper * 1.01, false),
            ] {
                let mut rates = baseline;
                match field {
                    0 => rates.input_cache_miss = price,
                    1 => rates.input_cache_hit = price,
                    2 => rates.output = price,
                    _ => unreachable!(),
                }
                assert_eq!(
                    rates_are_plausible(model, rates),
                    accepted,
                    "{model}, field {field}, price {price}"
                );
            }
        }
    }

    #[test]
    fn unknown_model_price_bounds_accept_endpoints_and_reject_one_percent_beyond() {
        // Decimal literals express the documented bounds independently of the
        // floating-point arithmetic used by within_drift.
        assert_price_bounds(
            "unknown-endpoint-price-probe",
            NEW_MODEL,
            [(0.015, 6.6), (0.0003, 0.22), (0.06, 19.8)],
        );
    }

    #[test]
    fn flash_price_bounds_accept_endpoints_and_reject_one_percent_beyond() {
        assert_price_bounds(
            FLASH_MODEL,
            FLASH_RATES,
            [(0.015, 1.5), (0.0003, 0.03), (0.06, 6.0)],
        );
    }

    #[test]
    fn pro_price_bounds_accept_endpoints_and_reject_one_percent_beyond() {
        assert_price_bounds(
            PRO_MODEL,
            PRO_RATES,
            [(0.066, 6.6), (0.0022, 0.22), (0.198, 19.8)],
        );
    }

    #[test]
    fn cached_unknown_models_require_plausible_prices_in_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let mut cached = content_dated_cache("2026-10-20", false);
        for rates in [
            Rates {
                input_cache_miss: 0.0,
                input_cache_hit: 0.0,
                output: 0.0,
            },
            Rates {
                input_cache_miss: 0.66e-6,
                input_cache_hit: 0.022e-6,
                output: 1.98e-6,
            },
            Rates {
                input_cache_miss: 0.001,
                ..NEW_MODEL
            },
            Rates {
                input_cache_hit: 0.00001,
                ..NEW_MODEL
            },
            Rates {
                output: 0.001,
                ..NEW_MODEL
            },
            Rates {
                input_cache_miss: 7.0,
                ..NEW_MODEL
            },
            Rates {
                input_cache_hit: 0.3,
                ..NEW_MODEL
            },
            Rates {
                output: 20.0,
                ..NEW_MODEL
            },
        ] {
            cached.models = BTreeMap::from([(
                "deepseek-v4-pro-exp".into(),
                CachedRates {
                    rates,
                    updated: "2026-10-20".parse().unwrap(),
                },
            )]);
            write_cache(&path, &cached).unwrap();
            assert!(read_cache(&path, now()).is_none(), "accepted {rates:?}");
        }
        cached.models.get_mut("deepseek-v4-pro-exp").unwrap().rates = NEW_MODEL;
        write_cache(&path, &cached).unwrap();
        assert_eq!(read_cache(&path, now()), Some(cached));
    }

    #[test]
    fn the_cache_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(CACHE_FILE);
        let written = cache("2026-10-20T08:00:00Z");
        write_cache(&path, &written).unwrap();
        assert_eq!(read_cache(&path, now()), Some(written));
    }

    #[test]
    fn cache_more_than_one_day_in_the_future_is_ignored() {
        let current = now();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let mut future = cache("2026-10-20T08:00:00Z");
        future.fetched_at = current + chrono::TimeDelta::days(1) + chrono::TimeDelta::seconds(1);
        write_cache(&path, &future).unwrap();
        assert!(read_cache(&path, now()).is_none());
        future.fetched_at = current + chrono::TimeDelta::days(1);
        write_cache(&path, &future).unwrap();
        assert_eq!(read_cache(&path, now()), Some(future));
    }

    #[test]
    fn read_cache_ignores_corrupt_foreign_and_implausible_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        assert_eq!(read_cache(&path, now()), None); // missing

        let mut text = serde_json::to_string(&cache("2026-10-20T08:00:00Z")).unwrap();
        text.truncate(text.len() / 2);
        std::fs::write(&path, text).unwrap();
        assert_eq!(read_cache(&path, now()), None); // truncated by a crash

        let mut foreign = cache("2026-10-20T08:00:00Z");
        foreign.version = CACHE_VERSION + 1;
        std::fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();
        assert_eq!(read_cache(&path, now()), None); // written by a newer Orca

        let mut implausible = cache("2026-10-20T08:00:00Z");
        implausible.models = BTreeMap::from([(
            FLASH_MODEL.to_string(),
            CachedRates {
                rates: Rates {
                    output: 0.6e-6,
                    ..FLASH_RATES
                },
                updated: rates::builtin_as_of(),
            },
        )]);
        std::fs::write(&path, serde_json::to_vec(&implausible).unwrap()).unwrap();
        assert_eq!(read_cache(&path, now()), None); // nothing plausible left

        let mut partly = cache("2026-10-20T08:00:00Z");
        partly.models.insert(
            PRO_MODEL.to_string(),
            CachedRates {
                rates: Rates {
                    input_cache_miss: 660.0,
                    ..PRO_RATES
                },
                updated: rates::builtin_as_of(),
            },
        );
        std::fs::write(&path, serde_json::to_vec(&partly).unwrap()).unwrap();
        let read = read_cache(&path, now()).expect("plausible entries survive");
        assert!(!read.models.contains_key(PRO_MODEL));
        assert_eq!(
            read.models.get(FLASH_MODEL).map(|entry| entry.rates),
            Some(FETCHED_FLASH)
        );
    }
}
