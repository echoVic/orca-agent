//! Coupons.

/// Value of a `percent`-off coupon on `amount_cents`, rounded with the documented policy.
pub fn coupon_cents(amount_cents: i64, percent: u32) -> i64 {
    let _ = (amount_cents, percent);
    todo!("implement coupon_cents")
}
