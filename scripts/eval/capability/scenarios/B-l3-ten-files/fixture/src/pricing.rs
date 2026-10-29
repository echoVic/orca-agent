//! Per-line pricing.

use crate::items::Item;

/// Total for one line: `price_cents * qty`.
pub fn line_total(item: &Item) -> i64 {
    let _ = item;
    todo!("implement line_total")
}
