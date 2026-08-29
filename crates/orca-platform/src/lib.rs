#![deny(deprecated)]

pub mod error;
pub mod fs;
pub mod host;
pub mod process;
pub mod shell;
pub mod terminal;

pub use error::PlatformError;
