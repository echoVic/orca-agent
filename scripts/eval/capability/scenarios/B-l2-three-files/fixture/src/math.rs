//! Arithmetic helpers used by the report module.

/// Sum of `values`, saturating at the 64-bit bounds.
pub fn total(values: &[i32]) -> i64 {
    values.iter().map(|value| i64::from(*value)).sum()
}
