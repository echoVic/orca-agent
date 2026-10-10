//! Whether this host runs the restricted shells the tests start.

use orca_tools::sandbox::{
    ReadOnlySandboxCommandContext, enforcement_decision, read_only_policy_refusal,
};

/// Why this host refuses every restricted shell, or `None` when it runs
/// them. Where bubblewrap cannot make its namespaces and only Landlock works,
/// as in a default Docker container, each one is refused: its policy hides
/// the Orca home from a global read, which Landlock cannot enforce.
pub fn restricted_shell_refusal() -> Option<String> {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let hidden = scratch.path().join("hidden");
    std::fs::create_dir(&hidden).expect("hidden directory");
    read_only_policy_refusal(
        &ReadOnlySandboxCommandContext {
            command: "true",
            cwd: scratch.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: std::slice::from_ref(&hidden),
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        },
        &enforcement_decision(),
    )
    .map(|refusal| refusal.detail)
}

/// Whether a test of what restricted commands do is skipped: on a host that
/// refuses every restricted shell it has nothing to observe, and says so.
pub fn host_refuses_restricted_shells() -> bool {
    match restricted_shell_refusal() {
        Some(reason) => {
            eprintln!("skipped: this host runs no restricted shell: {reason}");
            true
        }
        None => false,
    }
}
