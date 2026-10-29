//! Arithmetic helpers.

/// Sum of `values`.
pub fn total(values: &[i32]) -> i64 {
    values.iter().map(|value| i64::from(*value)).sum()
}
