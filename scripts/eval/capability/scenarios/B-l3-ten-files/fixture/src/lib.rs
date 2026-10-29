//! A small order-pricing pipeline.
//!
//! The modules are intentionally narrow: each owns one step of the pipeline, and `report` /
//! `receipt` compose them. The public surface is re-exported here so downstream code never reaches
//! into the modules directly.

pub mod cart;
pub mod currency;
pub mod discount;
pub mod items;
pub mod pricing;
pub mod receipt;
pub mod report;
pub mod rounding;
pub mod tax;
pub mod validation;

pub use cart::Cart;
pub use currency::format_cents;
pub use discount::coupon_cents;
pub use items::Item;
pub use pricing::line_total;
pub use receipt::render;
pub use report::summary;
pub use rounding::round_half_up;
pub use tax::tax_cents;
pub use validation::validate;
