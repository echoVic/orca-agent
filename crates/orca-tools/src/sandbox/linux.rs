//! Linux OS sandbox: bubblewrap (primary) with a Landlock + seccomp in-process
//! fallback. Mirrors the macOS Seatbelt backend's `Command`-returning API.
//!
//! # Strategy
//!
//! 1. If a usable `bwrap` binary is on `PATH`, wrap the shell command with it
//!    (filesystem + network isolation via mount/network namespaces). This is
//!    the strongest and most portable option.
//! 2. Otherwise prepare Landlock filesystem rules plus a seccomp network
//!    filter in the parent, then apply them in a `pre_exec` hook so only the
//!    forked child is restricted before it execs the shell.
//! 3. If neither backend can enforce the policy and strict mode is requested,
//!    fail closed: return a command that exits non-zero without running the
//!    user's command.
//!
//! `pre_exec` runs in the forked child between `fork` and `exec`, so all path
//! opening, ruleset construction, and filter compilation happens in the parent.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use orca_core::capability::{
    EnforcementState, SandboxEnforcementDecision, SandboxProbeEvidence, SandboxProbeStatus,
};

use crate::sandbox::bwrap::{
    LinuxReadScope, LinuxSandboxPolicy, build_bwrap_argv, effective_read_only_roots,
};

const BWRAP_BACKEND: &str = "bwrap";
const LANDLOCK_BACKEND: &str = "landlock";
const SECCOMP_BACKEND: &str = "seccomp";
const LINUX_BACKEND_SET: &str = "bwrap+landlock+seccomp";

/// Resolved, canonicalized sandbox request shared by both backends.
pub(crate) struct LinuxSandboxRequest {
    pub command: String,
    pub policy: LinuxSandboxPolicy,
    /// When true and no backend can enforce the policy, fail closed instead of
    /// running the command unsandboxed.
    pub strict: bool,
}

/// Platform read roots a Linux shell runtime needs when the root is a tmpfs
/// (restricted-read policies). Kept in sync with the bwrap builder.
pub fn platform_default_read_roots() -> Vec<PathBuf> {
    [
        "/bin",
        "/sbin",
        "/usr",
        "/lib",
        "/lib64",
        "/etc",
        "/nix/store",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|path| path.exists())
    .collect()
}

/// Probe the exact Linux backend that production launches use.  Merely
/// finding `bwrap` or compiling a Landlock ruleset is insufficient: the
/// runner may forbid user namespaces or filesystem restriction entirely.
/// Keep this result stable for the process so receipts do not change midway
/// through a run.
pub fn enforced_available(cwd: &Path) -> bool {
    enforcement_decision(cwd).state == EnforcementState::Enforced
}

pub fn enforcement_evidence(cwd: &Path) -> &'static [SandboxProbeEvidence] {
    &enforcement_decision(cwd).probes
}

pub fn enforcement_decision(cwd: &Path) -> &'static SandboxEnforcementDecision {
    static DECISION: OnceLock<SandboxEnforcementDecision> = OnceLock::new();
    DECISION.get_or_init(|| probe_enforcement(cwd))
}

fn probe_enforcement(cwd: &Path) -> SandboxEnforcementDecision {
    let bwrap = probe_bwrap(cwd);
    if bwrap.available() {
        return SandboxEnforcementDecision::new(
            EnforcementState::Enforced,
            BWRAP_BACKEND,
            vec![bwrap],
        );
    }

    #[cfg(target_os = "linux")]
    {
        let landlock = result_probe_evidence(
            LANDLOCK_BACKEND,
            linux_landlock::probe_landlock_supported().map_err(|error| error.to_string()),
            SandboxProbeStatus::ProbeDenied,
        );
        let seccomp = result_probe_evidence(
            SECCOMP_BACKEND,
            linux_landlock::build_seccomp_filter(false)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            SandboxProbeStatus::ProbeFailed,
        );
        return fallback_enforcement_decision(bwrap, landlock, seccomp);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let unsupported = |backend: &str| SandboxProbeEvidence {
            backend: backend.to_string(),
            executable: None,
            status: SandboxProbeStatus::UnsupportedPlatform,
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: None,
        };
        SandboxEnforcementDecision::new(
            EnforcementState::Unavailable,
            LINUX_BACKEND_SET,
            vec![
                bwrap,
                unsupported(LANDLOCK_BACKEND),
                unsupported(SECCOMP_BACKEND),
            ],
        )
    }
}

fn fallback_enforcement_decision(
    bwrap: SandboxProbeEvidence,
    landlock: SandboxProbeEvidence,
    seccomp: SandboxProbeEvidence,
) -> SandboxEnforcementDecision {
    let state = if landlock.available() && seccomp.available() {
        EnforcementState::Enforced
    } else {
        EnforcementState::Unavailable
    };
    let backend = if state == EnforcementState::Enforced {
        "landlock+seccomp"
    } else {
        LINUX_BACKEND_SET
    };
    SandboxEnforcementDecision::new(state, backend, vec![bwrap, landlock, seccomp])
}

/// Probe bubblewrap once and reuse the result for both capability reporting
/// and command selection.  Keeping these decisions coupled prevents a failed
/// bwrap network/user-namespace setup from being reported as Landlock-ready
/// while the launcher still chooses bwrap on the next request.
fn bwrap_enforced_available(cwd: &Path) -> bool {
    let decision = enforcement_decision(cwd);
    decision.state == EnforcementState::Enforced && decision.backend == BWRAP_BACKEND
}

fn probe_bwrap(cwd: &Path) -> SandboxProbeEvidence {
    let Some(bwrap) = bwrap_path(cwd) else {
        return SandboxProbeEvidence {
            backend: BWRAP_BACKEND.to_string(),
            executable: Some(PathBuf::from("bwrap")),
            status: SandboxProbeStatus::BackendMissing,
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: None,
        };
    };
    let request = LinuxSandboxRequest {
        command: "true".to_string(),
        policy: LinuxSandboxPolicy {
            cwd: cwd.to_path_buf(),
            read_scope: LinuxReadScope::Global,
            readable_roots: Vec::new(),
            writable_roots: Vec::new(),
            metadata_protection_roots: Vec::new(),
            metadata_writable_roots: Vec::new(),
            read_only_roots: Vec::new(),
            denied_roots: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            network_access: false,
        },
        strict: true,
    };
    let output = bwrap_command(bwrap.clone(), &request).output();
    super::command_probe_evidence(BWRAP_BACKEND, Some(bwrap), output)
}

fn result_probe_evidence(
    backend: &str,
    result: Result<(), String>,
    failure_status: SandboxProbeStatus,
) -> SandboxProbeEvidence {
    match result {
        Ok(()) => SandboxProbeEvidence {
            backend: backend.to_string(),
            executable: None,
            status: SandboxProbeStatus::Available,
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: None,
        },
        Err(error) => SandboxProbeEvidence {
            backend: backend.to_string(),
            executable: None,
            status: failure_status,
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: Some(error),
        },
    }
}

/// Build a `Command` that runs `request.command` under the strongest available
/// Linux sandbox backend.
pub(crate) fn sandbox_command(request: LinuxSandboxRequest) -> Command {
    if request
        .policy
        .denied_roots
        .iter()
        .any(|root| !root.exists())
    {
        return fail_closed_command("one or more sandbox deny paths do not exist");
    }
    if bwrap_enforced_available(&request.policy.cwd) {
        let Some(bwrap) = bwrap_path(&request.policy.cwd) else {
            return fail_closed_command("bubblewrap disappeared after backend probe");
        };
        return bwrap_command(bwrap, &request);
    }

    // No bwrap: try the in-process Landlock + seccomp backend. Policies that
    // require namespace mounts (nested read-only or denied paths) are rejected
    // by landlock_command below. Any backend failure is fail-closed: a
    // non-dangerous request must never become a plain host shell.
    match landlock_command(&request) {
        Ok(command) => command,
        Err(_) => fail_closed_command("no compatible Linux sandbox backend is available"),
    }
}

fn policy_requires_bwrap(request: &LinuxSandboxRequest) -> bool {
    let policy = &request.policy;
    if !policy.network_access && !policy.allowed_unix_socket_roots.is_empty() {
        return true;
    }
    let writable_overlap = effective_read_only_roots(policy)
        .iter()
        .filter(|root| root.exists())
        .any(|read_only| {
            policy
                .writable_roots
                .iter()
                .chain(&policy.metadata_writable_roots)
                .any(|writable| paths_overlap(read_only, writable))
        });
    if writable_overlap {
        return true;
    }

    let denied = policy
        .denied_roots
        .iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if denied.is_empty() {
        return false;
    }
    if policy.read_scope == LinuxReadScope::Global {
        return true;
    }

    let mut accessible = vec![policy.cwd.as_path()];
    accessible.extend(policy.readable_roots.iter().map(PathBuf::as_path));
    accessible.extend(policy.writable_roots.iter().map(PathBuf::as_path));
    accessible.extend(policy.metadata_writable_roots.iter().map(PathBuf::as_path));
    accessible.extend(
        policy
            .allowed_unix_socket_roots
            .iter()
            .map(PathBuf::as_path),
    );
    denied
        .iter()
        .any(|denied| accessible.iter().any(|root| paths_overlap(denied, root)))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn bwrap_command(bwrap: PathBuf, request: &LinuxSandboxRequest) -> Command {
    let argv = build_bwrap_argv(&request.policy, &request.command);
    let mut command = Command::new(bwrap);
    command.args(argv);
    command.current_dir(&request.policy.cwd);
    command
}

/// Locate an executable `bwrap` outside the workspace. A repository-controlled
/// binary must never become the security boundary for that same repository.
fn bwrap_path(cwd: &Path) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    find_bwrap_on_path(&path_var, cwd)
}

fn find_bwrap_on_path(path_var: &OsStr, cwd: &Path) -> Option<PathBuf> {
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    std::env::split_paths(path_var).find_map(|dir| {
        let candidate = if dir.is_absolute() {
            dir.join("bwrap")
        } else {
            cwd.join(dir).join("bwrap")
        };
        let absolute = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        if candidate.starts_with(cwd) || absolute.starts_with(&canonical_cwd) {
            return None;
        }
        is_executable_file(&absolute).then_some(absolute)
    })
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn plain_command(command: &str, cwd: &std::path::Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);
    cmd
}

/// A command that always fails without running anything, used for fail-closed
/// strict mode when no sandbox backend is available.
fn fail_closed_command(reason: &'static str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("echo \"orca: refusing to run: $ORCA_SANDBOX_ERROR\" >&2; exit 126")
        .env("ORCA_SANDBOX_ERROR", reason);
    cmd
}

#[cfg(not(target_os = "linux"))]
fn landlock_command(_request: &LinuxSandboxRequest) -> io::Result<Command> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Landlock is only available on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn landlock_command(request: &LinuxSandboxRequest) -> io::Result<Command> {
    use std::os::unix::process::CommandExt;

    if policy_requires_bwrap(request) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "policy contains nested read-only or denied paths that require bubblewrap",
        ));
    }
    linux_landlock::probe_landlock_supported()?;
    let prepared = linux_landlock::PreparedSandbox::new(request)?;
    let mut prepared = Some(prepared);
    let mut cmd = plain_command(&request.command, &request.policy.cwd);

    // SAFETY: path opening, ruleset construction, and BPF compilation happened
    // in the parent. The child only applies the prepared kernel objects before
    // exec, avoiding allocator and filesystem work after fork.
    unsafe {
        cmd.pre_exec(move || {
            prepared
                .take()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EPERM))?
                .apply()
        });
    }
    Ok(cmd)
}

#[cfg(target_os = "linux")]
mod linux_landlock {
    use std::io;
    use std::path::PathBuf;

    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };
    use seccompiler::BpfProgram;

    use super::LinuxSandboxRequest;

    /// Kernel objects prepared before fork and applied by the child in
    /// `pre_exec`. No path traversal or BPF compilation happens after fork.
    pub(super) struct PreparedSandbox {
        ruleset: landlock::RulesetCreated,
        network_filter: Option<BpfProgram>,
        strict: bool,
    }

    impl PreparedSandbox {
        pub(super) fn new(request: &LinuxSandboxRequest) -> io::Result<Self> {
            let policy = &request.policy;
            let abi = ABI::V5;
            let compatibility = if request.strict {
                CompatLevel::HardRequirement
            } else {
                CompatLevel::BestEffort
            };
            let access_rw = AccessFs::from_all(abi);
            let access_ro = AccessFs::from_read(abi);
            let mut readable_roots = policy.readable_roots.clone();
            push_unique(&mut readable_roots, policy.cwd.clone());
            for root in super::platform_default_read_roots() {
                push_unique(&mut readable_roots, root);
            }
            for root in &policy.writable_roots {
                push_unique(&mut readable_roots, root.clone());
            }
            for root in &policy.metadata_writable_roots {
                push_unique(&mut readable_roots, root.clone());
            }
            for root in &policy.allowed_unix_socket_roots {
                push_unique(&mut readable_roots, root.clone());
            }

            let mut ruleset = landlock::Ruleset::default()
                .set_compatibility(compatibility)
                .handle_access(access_rw)
                .map_err(landlock_prepare_error)?
                .create()
                .map_err(landlock_prepare_error)?;

            match policy.read_scope {
                super::LinuxReadScope::Global => {
                    ruleset = add_path_rule(ruleset, "/", access_ro, false)?;
                }
                super::LinuxReadScope::Restricted => {
                    let existing: Vec<&PathBuf> =
                        readable_roots.iter().filter(|root| root.exists()).collect();
                    for root in existing {
                        ruleset = add_path_rule(ruleset, root, access_ro, false)?;
                    }
                }
            }

            // Always allow read+write on /dev/null; most tools need it.
            if PathBuf::from("/dev/null").exists() {
                ruleset = add_path_rule(ruleset, "/dev/null", access_rw, true)?;
            }

            let writable: Vec<&PathBuf> = policy
                .writable_roots
                .iter()
                .chain(&policy.metadata_writable_roots)
                .filter(|r| r.exists())
                .collect();
            for root in writable {
                ruleset = add_path_rule(ruleset, root, access_rw, false)?;
            }

            let network_filter = build_seccomp_filter(policy.network_access)
                .map_err(|error| io::Error::new(io::ErrorKind::Unsupported, error))?;

            Ok(Self {
                ruleset,
                network_filter: Some(network_filter),
                strict: request.strict,
            })
        }

        pub(super) fn apply(self) -> io::Result<()> {
            set_no_new_privs()?;
            if let Some(filter) = self.network_filter {
                seccompiler::apply_filter(&filter).map_err(|_| sandbox_apply_error())?;
            }
            let status = self
                .ruleset
                .restrict_self()
                .map_err(|_| sandbox_apply_error())?;
            if status.ruleset == RulesetStatus::NotEnforced
                || (self.strict && status.ruleset != RulesetStatus::FullyEnforced)
            {
                return Err(sandbox_apply_error());
            }
            Ok(())
        }
    }

    fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }

    /// Add a rule using an explicitly opened descriptor.  The convenience
    /// `path_beneath_rules` iterator silently drops paths that cannot be
    /// opened, which would turn a malformed policy into a weaker sandbox.
    /// Keeping the descriptor alive in `PathBeneath` also means the kernel
    /// rule remains tied to the opened filesystem object, not a later path
    /// lookup.
    fn add_path_rule(
        ruleset: landlock::RulesetCreated,
        path: impl AsRef<std::path::Path>,
        access: landlock::BitFlags<AccessFs>,
        file: bool,
    ) -> io::Result<landlock::RulesetCreated> {
        let path = path.as_ref();
        let fd = PathFd::new(path).map_err(|error| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("failed to open Landlock root {}: {error}", path.display()),
            )
        })?;
        let access = if file {
            access & AccessFs::from_file(ABI::V5)
        } else {
            access
        };
        ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(landlock_prepare_error)
    }

    fn landlock_prepare_error(error: landlock::RulesetError) -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("failed to prepare Landlock ruleset: {error}"),
        )
    }

    fn sandbox_apply_error() -> io::Error {
        io::Error::from_raw_os_error(libc::EPERM)
    }

    fn set_no_new_privs() -> io::Result<()> {
        // SAFETY: prctl with PR_SET_NO_NEW_PRIVS and constant args is safe.
        let result = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Probe whether Landlock can be enforced on the running kernel by creating
    /// and immediately abandoning a ruleset. Cheap and side-effect free.
    pub(super) fn probe_landlock_supported() -> io::Result<()> {
        let abi = ABI::V1;
        let access_rw = AccessFs::from_all(abi);
        landlock::Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(access_rw)
            .and_then(|ruleset| ruleset.create())
            .map(|_| ())
            .map_err(|err| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("landlock unavailable: {err}"),
                )
            })
    }

    pub(super) fn build_seccomp_filter(network_access: bool) -> Result<BpfProgram, String> {
        use std::collections::BTreeMap;

        use seccompiler::{
            SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
            SeccompRule, TargetArch,
        };

        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

        // Prevent stealing file descriptors or bypassing syscall filtering via
        // io_uring, matching the hardened Codex Linux filter.
        for nr in [
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            rules.insert(nr, vec![]);
        }

        if !network_access {
            // Deny outbound/inbound network syscalls. `recvfrom` is intentionally
            // left allowed so socketpair-based subprocess IPC (e.g. cargo) works.
            // IP sockets cannot be created or connected inside this policy; an
            // inherited IP-socket fd would remain an explicit parent-owned
            // capability rather than a new network escape.
            for nr in [
                libc::SYS_connect,
                libc::SYS_accept,
                libc::SYS_accept4,
                libc::SYS_bind,
                libc::SYS_listen,
                libc::SYS_getpeername,
                libc::SYS_getsockname,
                libc::SYS_shutdown,
                libc::SYS_sendto,
                libc::SYS_sendmmsg,
                libc::SYS_recvmmsg,
                libc::SYS_getsockopt,
                libc::SYS_setsockopt,
            ] {
                rules.insert(nr, vec![]);
            }

            // Allow only AF_UNIX sockets so local IPC keeps working while IP
            // networking is blocked.
            let unix_only = SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_UNIX as u64,
                )
                .map_err(|err| format!("seccomp condition failed: {err}"))?,
            ])
            .map_err(|err| format!("seccomp rule failed: {err}"))?;
            rules.insert(libc::SYS_socket, vec![unix_only.clone()]);
            rules.insert(libc::SYS_socketpair, vec![unix_only]);
        }

        let target_arch = if cfg!(target_arch = "x86_64") {
            TargetArch::x86_64
        } else if cfg!(target_arch = "aarch64") {
            TargetArch::aarch64
        } else {
            return Err("unsupported architecture for seccomp network filter".to_string());
        };

        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(libc::EPERM as u32),
            target_arch,
        )
        .map_err(|err| format!("seccomp filter build failed: {err}"))?;
        filter
            .try_into()
            .map_err(|err| format!("seccomp program conversion failed: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(backend: &str, status: SandboxProbeStatus) -> SandboxProbeEvidence {
        SandboxProbeEvidence {
            backend: backend.to_string(),
            executable: None,
            status,
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: None,
        }
    }

    #[test]
    fn fallback_decision_preserves_failed_bwrap_and_selects_landlock_seccomp() {
        let decision = fallback_enforcement_decision(
            probe(BWRAP_BACKEND, SandboxProbeStatus::BackendMissing),
            probe(LANDLOCK_BACKEND, SandboxProbeStatus::Available),
            probe(SECCOMP_BACKEND, SandboxProbeStatus::Available),
        );

        assert_eq!(decision.state, EnforcementState::Enforced);
        assert_eq!(decision.backend, "landlock+seccomp");
        assert_eq!(decision.probes.len(), 3);
        assert_eq!(
            decision.probes[0].status,
            SandboxProbeStatus::BackendMissing
        );
    }

    #[test]
    fn fallback_decision_reports_each_failed_backend() {
        let decision = fallback_enforcement_decision(
            probe(BWRAP_BACKEND, SandboxProbeStatus::ProbeDenied),
            probe(LANDLOCK_BACKEND, SandboxProbeStatus::ProbeDenied),
            probe(SECCOMP_BACKEND, SandboxProbeStatus::ProbeFailed),
        );

        assert_eq!(decision.state, EnforcementState::Unavailable);
        assert_eq!(decision.backend, LINUX_BACKEND_SET);
        assert_eq!(
            decision
                .probes
                .iter()
                .map(|probe| probe.status)
                .collect::<Vec<_>>(),
            vec![
                SandboxProbeStatus::ProbeDenied,
                SandboxProbeStatus::ProbeDenied,
                SandboxProbeStatus::ProbeFailed,
            ]
        );
    }

    #[test]
    fn strict_mode_without_backend_fails_closed() {
        // When bwrap is absent and Landlock is unavailable (non-Linux host),
        // strict mode must produce a non-zero-exit command, never a plain run.
        if bwrap_path(Path::new(".")).is_some() {
            return; // environment has bwrap; fallback path not exercised
        }
        let request = LinuxSandboxRequest {
            command: "echo should-not-run".to_string(),
            policy: LinuxSandboxPolicy {
                cwd: PathBuf::from("."),
                read_scope: LinuxReadScope::Restricted,
                readable_roots: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                writable_roots: Vec::new(),
                metadata_protection_roots: Vec::new(),
                metadata_writable_roots: Vec::new(),
                read_only_roots: Vec::new(),
                denied_roots: Vec::new(),
                network_access: false,
            },
            strict: true,
        };
        let mut command = sandbox_command(request);
        // Only meaningful where we actually fall through to fail-closed.
        #[cfg(not(target_os = "linux"))]
        {
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(126));
        }
        #[cfg(target_os = "linux")]
        {
            let _ = &mut command;
        }
    }

    #[test]
    fn bwrap_lookup_rejects_workspace_controlled_binary() {
        let workspace = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        for dir in [workspace.path(), external.path()] {
            let binary = dir.join("bwrap");
            std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let mut permissions = binary.metadata().unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(binary, permissions).unwrap();
            }
        }
        let path = std::env::join_paths([workspace.path(), external.path()]).unwrap();

        assert_eq!(
            find_bwrap_on_path(&path, workspace.path()),
            Some(external.path().join("bwrap").canonicalize().unwrap())
        );
    }

    #[test]
    fn nested_read_only_and_denied_paths_require_bwrap() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        std::fs::create_dir(workspace.path().join("secret")).unwrap();
        let request = LinuxSandboxRequest {
            command: "true".to_string(),
            policy: LinuxSandboxPolicy {
                cwd: workspace.path().to_path_buf(),
                read_scope: LinuxReadScope::Global,
                readable_roots: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                writable_roots: vec![workspace.path().to_path_buf()],
                metadata_protection_roots: vec![workspace.path().to_path_buf()],
                metadata_writable_roots: Vec::new(),
                read_only_roots: vec![workspace.path().join(".git")],
                denied_roots: Vec::new(),
                network_access: false,
            },
            strict: false,
        };
        assert!(policy_requires_bwrap(&request));

        let mut denied = request;
        denied.policy.read_only_roots.clear();
        denied.policy.denied_roots = vec![workspace.path().join("secret")];
        assert!(policy_requires_bwrap(&denied));
    }

    #[test]
    fn nested_read_only_policy_without_backend_fails_closed_for_all_non_dangerous_modes() {
        if bwrap_path(Path::new(".")).is_some() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let metadata = workspace.path().join(".git");
        let marker = workspace.path().join("must-run");
        std::fs::create_dir(&metadata).unwrap();
        let request = LinuxSandboxRequest {
            command: format!("touch {}", marker.display()),
            policy: LinuxSandboxPolicy {
                cwd: workspace.path().to_path_buf(),
                read_scope: LinuxReadScope::Global,
                readable_roots: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                writable_roots: vec![workspace.path().to_path_buf()],
                metadata_protection_roots: vec![workspace.path().to_path_buf()],
                metadata_writable_roots: Vec::new(),
                read_only_roots: vec![metadata],
                denied_roots: Vec::new(),
                network_access: true,
            },
            strict: false,
        };

        let output = sandbox_command(request).output().unwrap();

        assert_eq!(output.status.code(), Some(126));
        assert!(!marker.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nested_read_only_policy_without_backend_fails_closed_when_strict() {
        if bwrap_path(Path::new(".")).is_some() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let metadata = workspace.path().join(".git");
        let marker = workspace.path().join("must-not-run");
        std::fs::create_dir(&metadata).unwrap();
        let request = LinuxSandboxRequest {
            command: format!("touch {}", marker.display()),
            policy: LinuxSandboxPolicy {
                cwd: workspace.path().to_path_buf(),
                read_scope: LinuxReadScope::Global,
                readable_roots: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                writable_roots: vec![workspace.path().to_path_buf()],
                metadata_protection_roots: vec![workspace.path().to_path_buf()],
                metadata_writable_roots: Vec::new(),
                read_only_roots: vec![metadata],
                denied_roots: Vec::new(),
                network_access: true,
            },
            strict: true,
        };

        let output = sandbox_command(request).output().unwrap();

        assert_eq!(output.status.code(), Some(126));
        assert!(!marker.exists());
    }

    #[test]
    fn unix_socket_exceptions_require_bwrap_when_network_is_disabled() {
        let workspace = tempfile::tempdir().unwrap();
        let socket_root = tempfile::tempdir().unwrap();
        let request = LinuxSandboxRequest {
            command: "true".to_string(),
            policy: LinuxSandboxPolicy {
                cwd: workspace.path().to_path_buf(),
                read_scope: LinuxReadScope::Restricted,
                readable_roots: Vec::new(),
                allowed_unix_socket_roots: vec![socket_root.path().to_path_buf()],
                writable_roots: Vec::new(),
                metadata_protection_roots: Vec::new(),
                metadata_writable_roots: Vec::new(),
                read_only_roots: Vec::new(),
                denied_roots: Vec::new(),
                network_access: false,
            },
            strict: true,
        };

        assert!(policy_requires_bwrap(&request));
    }
}
