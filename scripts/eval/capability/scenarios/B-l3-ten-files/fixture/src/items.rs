//! Line items.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub sku: String,
    pub price_cents: i64,
    pub qty: u32,
}

impl Item {
    pub fn new(sku: impl Into<String>, price_cents: i64, qty: u32) -> Self {
        Self { sku: sku.into(), price_cents, qty }
    }
}
