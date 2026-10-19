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
pub const CACHE_VERSION: u32 = 1;
/// The highest price accepted from any source, in US dollars per million tokens.
const MAX_PRICE: f64 = 100.0;
/// How far a fetched price may move from the built-in one before it is taken
/// for a unit error (per token, per thousand tokens).
const MAX_DRIFT: f64 = 10.0;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PricingCache {
    pub version: u32,
    pub source: String,
    pub fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub models: BTreeMap<String, Rates>,
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

/// Whether `rates` can be trusted for `model`: every price is a plausible
/// amount, and none is ten times off the built-in price for the same model.
pub fn rates_are_plausible(model: &str, rates: Rates) -> bool {
    let prices = [rates.input_cache_miss, rates.input_cache_hit, rates.output];
    if prices
        .iter()
        .any(|price| !price.is_finite() || *price < 0.0 || *price > MAX_PRICE)
    {
        return false;
    }
    let Some((_, builtin)) = rates::builtin_rates()
        .into_iter()
        .find(|(name, _)| *name == model)
    else {
        return true;
    };
    within_drift(builtin.input_cache_miss, rates.input_cache_miss)
        && within_drift(builtin.input_cache_hit, rates.input_cache_hit)
        && within_drift(builtin.output, rates.output)
}

fn within_drift(reference: f64, price: f64) -> bool {
    price >= reference / MAX_DRIFT && price <= reference * MAX_DRIFT
}

/// The built-in table overlaid by `cache`: a cache fetched on or after the
/// built-in date wins for every model it lists; an older one only adds models
/// the built-in table lacks.
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
    let cache_is_current = cache.fetched_at.date_naive() >= as_of;
    for (model, rates) in &cache.models {
        if cache_is_current || !models.contains_key(model) {
            models.insert(model.clone(), *rates);
        }
    }
    let source = if cache_is_current {
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
/// missing, unreadable, written by another cache version, or left with nothing.
pub fn read_cache(path: &Path) -> Option<PricingCache> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut cache: PricingCache = serde_json::from_str(&text).ok()?;
    if cache.version != CACHE_VERSION {
        return None;
    }
    cache
        .models
        .retain(|model, rates| rates_are_plausible(model, *rates));
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

fn global() -> &'static RwLock<Arc<PriceBook>> {
    static BOOK: OnceLock<RwLock<Arc<PriceBook>>> = OnceLock::new();
    BOOK.get_or_init(|| {
        let cache = cache_path().and_then(|path| read_cache(&path));
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

    fn cache(fetched_at: &str) -> PricingCache {
        PricingCache {
            version: CACHE_VERSION,
            source: "https://models.dev/api.json".to_string(),
            fetched_at: fetched_at.parse().unwrap(),
            etag: Some("W/\"v1\"".to_string()),
            models: BTreeMap::from([
                (FLASH_MODEL.to_string(), FETCHED_FLASH),
                ("deepseek-v5-flash".to_string(), NEW_MODEL),
            ]),
        }
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
        assert!(matches!(book.source, PriceSource::Builtin { .. }));
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

    #[test]
    fn the_cache_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(CACHE_FILE);
        let written = cache("2026-10-20T08:00:00Z");
        write_cache(&path, &written).unwrap();
        assert_eq!(read_cache(&path), Some(written));
    }

    #[test]
    fn read_cache_ignores_corrupt_foreign_and_implausible_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        assert_eq!(read_cache(&path), None); // missing

        let mut text = serde_json::to_string(&cache("2026-10-20T08:00:00Z")).unwrap();
        text.truncate(text.len() / 2);
        std::fs::write(&path, text).unwrap();
        assert_eq!(read_cache(&path), None); // truncated by a crash

        let mut foreign = cache("2026-10-20T08:00:00Z");
        foreign.version = CACHE_VERSION + 1;
        std::fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();
        assert_eq!(read_cache(&path), None); // written by a newer Orca

        let mut implausible = cache("2026-10-20T08:00:00Z");
        implausible.models = BTreeMap::from([(
            FLASH_MODEL.to_string(),
            Rates {
                output: 0.6e-6,
                ..FLASH_RATES
            },
        )]);
        std::fs::write(&path, serde_json::to_vec(&implausible).unwrap()).unwrap();
        assert_eq!(read_cache(&path), None); // nothing plausible left

        let mut partly = cache("2026-10-20T08:00:00Z");
        partly.models.insert(
            PRO_MODEL.to_string(),
            Rates {
                input_cache_miss: 660.0,
                ..PRO_RATES
            },
        );
        std::fs::write(&path, serde_json::to_vec(&partly).unwrap()).unwrap();
        let read = read_cache(&path).expect("plausible entries survive");
        assert!(!read.models.contains_key(PRO_MODEL));
        assert_eq!(read.models.get(FLASH_MODEL), Some(&FETCHED_FLASH));
    }
}
