//! Order validation.

use crate::items::Item;

/// Reject orders that cannot be priced.
///
/// - an empty order is invalid,
/// - a line with `qty == 0` is invalid,
/// - a line with a negative price is invalid.
pub fn validate(items: &[Item]) -> Result<(), String> {
    let _ = items;
    todo!("implement validate")
}
