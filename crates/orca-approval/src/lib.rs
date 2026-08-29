#![deny(deprecated)]

pub mod confirm;
pub mod policy;

pub use confirm::{prompt_user, prompt_user_with_io};
pub use policy::ApprovalPolicy;
