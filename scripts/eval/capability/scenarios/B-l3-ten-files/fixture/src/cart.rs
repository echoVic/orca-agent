//! The cart.

use crate::items::Item;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cart {
    pub items: Vec<Item>,
    pub tax_bps: u32,
}

impl Cart {
    pub fn new(items: Vec<Item>, tax_bps: u32) -> Self {
        Self { items, tax_bps }
    }

    /// Sum of every line total.
    pub fn subtotal(&self) -> i64 {
        let _ = self;
        todo!("implement subtotal")
    }
}
