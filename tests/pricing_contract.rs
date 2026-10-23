//! An `orca exec` run prices usage from the cache in its ORCA_HOME, falling
//! back to the built-in table. The clock is real, so each expectation allows
//! DeepSeek's off-peak price or its doubled peak price.

use std::path::Path;
use std::process::{Command, Stdio};

use orca_runtime::pricing::rates::builtin_as_of;
use serde_json::Value;

/// `mock_usage` reports 120 input tokens (10 cached) and 30 output tokens on
/// the default model, `deepseek-flash`.
fn mock_usage_cost(home: &Path) -> f64 {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home)
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "mock_usage",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run orca exec");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["type"] == "usage.updated")
        .last()
        .expect("usage event")["payload"]["estimated_cost_usd"]
        .as_f64()
        .unwrap()
}

fn at_either_tier(cost: f64, off_peak: f64) {
    assert!(
        (cost - off_peak).abs() < 1e-12 || (cost - 2.0 * off_peak).abs() < 1e-12,
        "cost {cost} is neither {off_peak} nor its peak double"
    );
}

#[test]
fn cargo_spawned_exec_inherits_offline_pricing_without_writing_a_cache() {
    // Assert before spawning: a missing Cargo default must fail without ever
    // permitting a release binary to fall back to models.dev.
    assert_eq!(std::env::var("ORCA_PRICING_URL").as_deref(), Ok("off"));
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .current_dir(home.path())
        .args(["exec", "--provider", "mock", "mock_usage"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.path().join("pricing-cache.json").exists());
}

#[test]
fn exec_without_a_cache_uses_the_builtin_prices() {
    let home = tempfile::tempdir().unwrap();
    let off_peak = (110.0 * 0.15 + 10.0 * 0.003 + 30.0 * 0.6) / 1_000_000.0;
    at_either_tier(mock_usage_cost(home.path()), off_peak);
    assert!(
        !home.path().join("pricing-cache.json").exists(),
        "a development build must not refresh"
    );
}

fn write_cache(home: &Path, updated: &str) {
    std::fs::write(
        home.join("pricing-cache.json"),
        serde_json::json!({
            "version": 2,
            "source": "https://models.dev/api.json",
            "fetched_at": format!("{}T00:00:00Z", builtin_as_of()),
            "models": {"deepseek-flash": {
                "input_cache_miss": 0.2, "input_cache_hit": 0.004, "output": 0.8,
                "updated": updated
            }}
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn exec_prices_usage_from_a_current_cache() {
    let off_peak = (110.0 * 0.2 + 10.0 * 0.004 + 30.0 * 0.8) / 1_000_000.0;
    for updated in [builtin_as_of(), builtin_as_of().succ_opt().unwrap()] {
        let home = tempfile::tempdir().unwrap();
        write_cache(home.path(), &updated.to_string());
        at_either_tier(mock_usage_cost(home.path()), off_peak);
    }
}

#[test]
fn exec_ignores_stale_prices_even_when_fetched_on_the_builtin_date() {
    let home = tempfile::tempdir().unwrap();
    write_cache(
        home.path(),
        &builtin_as_of().pred_opt().unwrap().to_string(),
    );
    let off_peak = (110.0 * 0.15 + 10.0 * 0.003 + 30.0 * 0.6) / 1_000_000.0;
    at_either_tier(mock_usage_cost(home.path()), off_peak);
}

#[test]
fn exec_ignores_a_corrupt_cache() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("pricing-cache.json"),
        b"{\"version\":1,\"source\":",
    )
    .unwrap();
    let off_peak = (110.0 * 0.15 + 10.0 * 0.003 + 30.0 * 0.6) / 1_000_000.0;
    at_either_tier(mock_usage_cost(home.path()), off_peak);
}
