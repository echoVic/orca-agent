//! Slice statistics used by the report pipeline.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub sum: i64,
    pub min: i32,
    pub max: i32,
    pub count: usize,
}

/// Compute the summary of `values`.
///
/// - `sum` is the 64-bit total (values can be large).
/// - `min` / `max` are the smallest / largest value.
/// - `count` is the number of values.
///
/// An empty slice yields `Summary { sum: 0, min: 0, max: 0, count: 0 }`.
pub fn summarize(values: &[i32]) -> Summary {
    let _ = values;
    todo!("implement summarize")
}
