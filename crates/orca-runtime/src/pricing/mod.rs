//! Cost estimates at DeepSeek's prices: the built-in table and its
//! time-of-use rule (`rates`), the process-wide price book with its cache
//! (`book`), and the daily refresh from models.dev (`refresh`).

use std::collections::BTreeMap;

use orca_core::model::ModelPriceOverride;

use self::rates::Rates;

pub mod book;
pub mod rates;
pub mod refresh;

/// Convert validated user-config prices to the fixed rates used by trackers.
pub fn rates_from_overrides(
    overrides: &BTreeMap<String, ModelPriceOverride>,
) -> BTreeMap<String, Rates> {
    overrides
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
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    // Session construction replaces process-wide overrides; isolate these tests
    // from sessions created concurrently by the rest of the lib-test suite.
    pub(crate) fn with_isolated_user_overrides(test: impl FnOnce()) {
        let thread = std::thread::current();
        let name = thread.name().expect("test thread name");
        const CHILD_ENV: &str = "ORCA_PRICING_OVERRIDE_TEST";
        if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
            test();
            return;
        }
        let home = tempfile::tempdir().expect("isolated home");
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, name)
            .env("ORCA_HOME", home.path())
            .env_remove("ORCA_PRICING_URL")
            .output()
            .expect("isolated pricing test");
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{name}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
