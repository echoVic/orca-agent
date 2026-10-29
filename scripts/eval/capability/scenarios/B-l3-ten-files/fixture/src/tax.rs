//! Tax.

/// Tax for `amount_cents` at `rate_bps` basis points, rounded with the documented policy.
pub fn tax_cents(amount_cents: i64, rate_bps: u32) -> i64 {
    let raw = amount_cents as f64 * rate_bps as f64 / 10_000.0;
    raw.round() as i64
}
