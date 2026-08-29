#![deny(deprecated)]

pub mod client;
pub mod transport;

pub use client::{McpRegistry, McpRequestError, canonical_server_name, initialize_registry};
pub use transport::{
    McpElicitationHandler, McpElicitationMode, McpElicitationRequest, McpElicitationResponse,
};
