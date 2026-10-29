//! Rounding helpers.
//!
//! NOTE: the pipeline's rounding policy is documented in `docs/ROUNDING.md`. `round_half_up` below is
//! the legacy helper the policy explicitly tells new code not to use.

/// Round half away from zero.
pub fn round_half_up(value: f64) -> i64 {
    if value >= 0.0 { (value + 0.5).floor() as i64 } else { (value - 0.5).ceil() as i64 }
}
