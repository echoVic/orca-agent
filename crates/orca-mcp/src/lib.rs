#![deny(deprecated)]

mod auth;
pub mod client;
mod legacy_sse;
pub mod oauth;
pub mod transport;

pub use auth::{MCP_AUTH_REQUIRED, McpAuthKind, is_auth_required};
pub use client::{
    McpChangeSubscription, McpPromptExpansion, McpRegistry, McpRequestError, McpServerState,
    McpServerStatus, canonical_server_name, initialize_registry,
};
pub use transport::{
    McpElicitationHandler, McpElicitationMode, McpElicitationRequest, McpElicitationResponse,
};
