//! An `orca exec` run prices usage from the cache in its ORCA_HOME, falling
//! back to the built-in table. The clock is real, so each expectation allows
//! DeepSeek's off-peak price or its doubled peak price.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

/// `mock_usage` reports 120 input tokens (10 cached) and 30 output tokens on
/// the default model, `deepseek-flash`.
fn mock_usage_cost(home: &Path) -> f64 {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home)
        .env_remove("ORCA_PRICING_URL")
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
fn exec_without_a_cache_uses_the_builtin_prices() {
    let home = tempfile::tempdir().unwrap();
    let off_peak = (110.0 * 0.15 + 10.0 * 0.003 + 30.0 * 0.6) / 1_000_000.0;
    at_either_tier(mock_usage_cost(home.path()), off_peak);
    assert!(
        !home.path().join("pricing-cache.json").exists(),
        "a development build must not refresh"
    );
}

#[test]
fn exec_prices_usage_from_a_current_cache() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("pricing-cache.json"),
        serde_json::json!({
            "version": 2,
            "source": "https://models.dev/api.json",
            "fetched_at": "2026-10-19T00:00:00Z",
            "models": {"deepseek-flash": {
                "input_cache_miss": 0.2, "input_cache_hit": 0.004, "output": 0.8,
                "updated": "2026-10-19"
            }}
        })
        .to_string(),
    )
    .unwrap();
    let off_peak = (110.0 * 0.2 + 10.0 * 0.004 + 30.0 * 0.8) / 1_000_000.0;
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
