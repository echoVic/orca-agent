//! `run_script`: JavaScript the model writes that calls read-only tools
//! itself, so one model round does the work of several.

pub mod host;

use std::sync::OnceLock;

use crate::workflow::host::WorkflowHost;

/// Whether this machine has a Node to run scripts on. Asked once: the answer
/// decides what a session advertises, and that must not change mid-session.
pub(crate) fn node_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(WorkflowHost::node_available)
}
