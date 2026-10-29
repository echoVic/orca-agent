//! Receipt rendering.

use crate::cart::Cart;
use crate::currency::format_cents;
use crate::pricing::line_total;

/// Multi-line receipt: one `sku  $total` line per item.
pub fn render(cart: &Cart) -> String {
    cart.items
        .iter()
        .map(|item| format!("{}  {}", item.sku, format_cents(line_total(item))))
        .collect::<Vec<_>>()
        .join("\n")
}
