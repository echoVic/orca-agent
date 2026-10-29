use cap_eval_l3::{coupon_cents, format_cents, line_total, render, summary, tax_cents, validate, Cart, Item};

fn item(sku: &str, price_cents: i64, qty: u32) -> Item {
    Item::new(sku, price_cents, qty)
}

#[test]
fn formats_money() {
    assert_eq!(format_cents(1234), "$12.34");
    assert_eq!(format_cents(-5), "-$0.05");
}

#[test]
fn rejects_empty_orders() {
    assert!(validate(&[]).is_err());
}

#[test]
fn rejects_zero_quantity_and_negative_prices() {
    assert!(validate(&[item("a", 100, 0)]).is_err());
    assert!(validate(&[item("b", -1, 1)]).is_err());
    assert!(validate(&[item("c", 100, 2)]).is_ok());
}

#[test]
fn prices_lines() {
    assert_eq!(line_total(&item("a", 250, 3)), 750);
}

#[test]
fn rounds_coupons_half_to_even() {
    assert_eq!(coupon_cents(1000, 15), 150);
    // 10 cents * 5% = 0.5 -> the policy sends an exact half to the even cent (0).
    assert_eq!(coupon_cents(10, 5), 0);
    // 30 cents * 5% = 1.5 -> 2.
    assert_eq!(coupon_cents(30, 5), 2);
}

#[test]
fn rounds_tax_half_to_even() {
    assert_eq!(tax_cents(10, 500), 0);  // 0.5 -> 0
    assert_eq!(tax_cents(30, 500), 2);  // 1.5 -> 2
    assert_eq!(tax_cents(50, 500), 2);  // 2.5 -> 2
    assert_eq!(tax_cents(100, 500), 5); // 5.0 -> 5
}

#[test]
fn sums_subtotals() {
    let cart = Cart::new(vec![item("a", 250, 2), item("b", 300, 1)], 500);
    assert_eq!(cart.subtotal(), 800);
}

#[test]
fn renders_receipt_lines() {
    let cart = Cart::new(vec![item("A", 750, 1), item("B", 50, 1)], 0);
    assert_eq!(render(&cart), "A  $7.50\nB  $0.50");
}

#[test]
fn summarises_the_order() {
    let cart = Cart::new(vec![item("a", 800, 1)], 500);
    assert_eq!(summary(&cart), "subtotal=$8.00 tax=$0.40");
}
