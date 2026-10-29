//! Renders a one-line report.

use crate::math;

pub fn render(values: &[i32]) -> String {
    format!("total={}", math::total(values))
}
