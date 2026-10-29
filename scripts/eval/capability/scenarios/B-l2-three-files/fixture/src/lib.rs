//! Public surface of the report crate.

mod math;
mod report;

pub use math::total;
pub use report::render;

/// Total of `values`, with an explicit name the callers depend on.
pub fn compute_total(values: &[i32]) -> i64 {
    let _ = values;
    todo!("implement compute_total in math.rs and re-export it")
}
