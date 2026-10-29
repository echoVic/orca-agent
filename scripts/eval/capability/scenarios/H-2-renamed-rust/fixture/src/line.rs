//! One-line output.

use crate::calc;

pub fn describe(values: &[i32]) -> String {
    format!("total={}", calc::total(values))
}
