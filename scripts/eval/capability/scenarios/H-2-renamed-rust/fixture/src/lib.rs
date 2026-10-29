//! Public surface.

mod calc;
mod line;

pub use calc::total;
pub use line::describe;

/// Aggregate assigned to callers under an explicit name.
pub fn aggregate_sum(values: &[i32]) -> i64 {
    let _ = values;
    todo!("implement aggregate_sum in calc.rs and re-export it")
}
