//! One-line report.

use crate::cart::Cart;
use crate::currency::format_cents;
use crate::tax::tax_cents;

/// `subtotal=... tax=...` for the cart.
pub fn summary(cart: &Cart) -> String {
    let subtotal = cart.subtotal();
    format!("subtotal={} tax={}", format_cents(subtotal), format_cents(tax_cents(subtotal, cart.tax_bps)))
}
