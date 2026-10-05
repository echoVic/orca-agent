#![deny(deprecated)]

mod auth;
pub mod client;
mod connection_stop;
mod legacy_sse;
pub mod oauth;
mod registry;
mod sse;
pub mod transport;

pub use auth::{MCP_AUTH_REQUIRED, McpAuthKind, is_auth_required};
pub use client::McpRequestError;
pub use registry::{
    McpChangeSubscription, McpPromptExpansion, McpRegistry, McpRegistrySnapshot, McpServerState,
    McpServerStatus, canonical_server_name, initialize_registry,
};
pub use transport::{
    McpElicitationHandler, McpElicitationMode, McpElicitationRequest, McpElicitationResponse,
};
