use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use orca_core::capability::{
    EnforcementState, SandboxEnforcementDecision, SandboxProbeEvidence, SandboxProbeStatus,
};

#[cfg(target_os = "macos")]
pub mod seatbelt;

// Compiled on all platforms so the bwrap argv builder can be unit tested off
// Linux; only the Linux platform block actually launches bwrap.
pub mod bwrap;
// The launch helpers are only invoked from the non-macOS (Linux) platform
// block; keep the module compiled everywhere for testing without warnings.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod linux;

const PROTECTED_METADATA_DIRS: [&str; 3] = [".git", ".agents", ".codex"];
const MAX_PROBE_DIAGNOSTIC_BYTES: usize = 4096;

pub fn is_protected_metadata_root(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| PROTECTED_METADATA_DIRS.contains(&name))
}

pub fn is_safe_metadata_writable_root(path: &Path) -> bool {
    if !is_protected_metadata_root(path) {
        return false;
    }
    match std::fs::symlink_metadata(path) {
        // A path that does not exist yet cannot be a symlink escape; the
        // sandbox platform layers re-check and canonicalize at launch time.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Ok(metadata) => !metadata.file_type().is_symlink(),
        Err(_) => false,
    }
}

#[cfg(all(test, unix))]
mod metadata_root_tests {
    use super::*;

    #[test]
    fn metadata_grant_rejects_symlinked_metadata_directory() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let metadata = workspace.join(".git");
        std::fs::create_dir_all(&metadata).unwrap();
        let outside = parent.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let agents_link = workspace.join(".agents");
        std::os::unix::fs::symlink(&outside, &agents_link).unwrap();

        assert!(is_safe_metadata_writable_root(
            &metadata.canonicalize().unwrap()
        ));
        assert!(is_safe_metadata_writable_root(&workspace.join(".codex")));
        // A symlink with a protected name must never become a writable grant.
        assert!(!is_safe_metadata_writable_root(&agents_link));
    }
}

/// Platform read roots a Linux shell runtime needs when the sandbox root is a
/// fresh tmpfs. Exposed here so the pure `bwrap` argv builder (compiled on all
/// platforms) can consult the same list the Linux backend uses.
pub(crate) fn linux_platform_default_read_roots() -> Vec<PathBuf> {
    linux::platform_default_read_roots()
}

pub struct WorkspaceWriteSandboxCommandContext<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub readable_roots: &'a [PathBuf],
    pub additional_roots: &'a [PathBuf],
    /// Roots explicitly escalated to write workspace metadata
    /// (`.git`/`.agents`/`.codex`). Unlike `additional_roots`, these are the
    /// only grants allowed to override the default metadata protection.
    pub metadata_writable_roots: &'a [PathBuf],
    pub denied_roots: &'a [PathBuf],
    pub network_access: bool,
    pub exclude_tmpdir_env_var: bool,
    pub exclude_slash_tmp: bool,
    pub allowed_unix_socket_roots: &'a [PathBuf],
}

pub struct ReadOnlySandboxCommandContext<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub readable_roots: &'a [PathBuf],
    pub additional_roots: &'a [PathBuf],
    pub metadata_writable_roots: &'a [PathBuf],
    pub denied_roots: &'a [PathBuf],
    pub network_access: bool,
    pub allow_global_read: bool,
    pub allowed_unix_socket_roots: &'a [PathBuf],
}

pub fn bash_command(command: &str, cwd: &Path) -> Command {
    workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
        command,
        cwd,
        readable_roots: &[],
        additional_roots: &[],
        metadata_writable_roots: &[],
        denied_roots: &[],
        network_access: true,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
        allowed_unix_socket_roots: &[],
    })
}

pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
    let mut command = platform::plain_bash_command(command, cwd);
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

pub fn bash_command_with_additional_roots(
    command: &str,
    cwd: &Path,
    additional_roots: &[PathBuf],
) -> Command {
    workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
        command,
        cwd,
        readable_roots: &[],
        additional_roots,
        metadata_writable_roots: &[],
        denied_roots: &[],
        network_access: true,
        exclude_tmpdir_env_var: false,
        exclude_slash_tmp: false,
        allowed_unix_socket_roots: &[],
    })
}

pub fn workspace_write_bash_command(context: WorkspaceWriteSandboxCommandContext<'_>) -> Command {
    let mut command = platform::workspace_write_bash_command(context);
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
    let mut command = platform::read_only_bash_command(context);
    crate::process::prepare_non_interactive_command(&mut command);
    command
}

pub fn platform_default_read_roots() -> Vec<PathBuf> {
    platform::platform_default_read_roots()
}

/// Report whether this crate's sandbox command builders can enforce a
/// non-dangerous profile on the current host. A command that cannot be
/// enforced must be rejected by the broker instead of being mislabeled as a
/// successful sandbox launch.
pub fn enforcement_decision() -> SandboxEnforcementDecision {
    platform_enforcement_decision()
}

#[cfg(target_os = "macos")]
fn platform_enforcement_decision() -> SandboxEnforcementDecision {
    seatbelt::enforcement_decision().clone()
}

#[cfg(target_os = "linux")]
fn platform_enforcement_decision() -> SandboxEnforcementDecision {
    linux::enforcement_decision(std::path::Path::new(".")).clone()
}

#[cfg(target_os = "windows")]
fn platform_enforcement_decision() -> SandboxEnforcementDecision {
    // The Windows runtime owns the AppContainer/Job launch path; these generic
    // builders intentionally return an error command.
    SandboxEnforcementDecision::new(
        EnforcementState::Unavailable,
        "windows-sandbox-adapter",
        vec![SandboxProbeEvidence {
            backend: "windows-sandbox-adapter".to_string(),
            executable: None,
            status: SandboxProbeStatus::UnsupportedPlatform,
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: None,
        }],
    )
}

#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
fn platform_enforcement_decision() -> SandboxEnforcementDecision {
    SandboxEnforcementDecision::new(
        EnforcementState::Unavailable,
        "unsupported-platform",
        vec![SandboxProbeEvidence {
            backend: "unsupported-platform".to_string(),
            executable: None,
            status: SandboxProbeStatus::UnsupportedPlatform,
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: None,
        }],
    )
}

pub fn enforcement_state() -> EnforcementState {
    enforcement_decision().state
}

pub fn enforcement_evidence() -> Vec<SandboxProbeEvidence> {
    enforcement_decision().probes
}

pub fn enforcement_unavailable_message(backend: &str, evidence: &[SandboxProbeEvidence]) -> String {
    let details = if evidence.is_empty() {
        format!("{backend} did not provide probe evidence")
    } else {
        evidence
            .iter()
            .map(format_probe_evidence)
            .collect::<Vec<_>>()
            .join("; ")
    };
    format!("no OS-enforced sandbox backend on this host: {details} (backend={backend})")
}

pub fn enforcement_unavailable_remediation(evidence: &[SandboxProbeEvidence]) -> String {
    if evidence
        .iter()
        .any(|probe| probe.status == SandboxProbeStatus::BackendMissing)
    {
        return "install or enable the missing platform sandbox backend; folder trust does not install sandbox support".to_string();
    }
    if evidence
        .iter()
        .any(|probe| probe.status == SandboxProbeStatus::ProbeDenied)
    {
        return "run Orca on a host that permits OS sandbox enforcement; a parent sandbox or container may forbid nested profiles, and /trust does not change that boundary".to_string();
    }
    if evidence
        .iter()
        .any(|probe| probe.status == SandboxProbeStatus::ProbeFailed)
    {
        return "inspect the backend executable and OS error; /trust only controls project configuration and cannot repair sandbox startup".to_string();
    }
    "use a platform with a supported OS sandbox backend; /trust does not enable kernel enforcement"
        .to_string()
}

pub(crate) fn command_probe_evidence(
    backend: impl Into<String>,
    executable: Option<PathBuf>,
    output: std::io::Result<Output>,
) -> SandboxProbeEvidence {
    let backend = backend.into();
    match output {
        Ok(output) => completed_probe_evidence(
            backend,
            executable,
            output.status.success(),
            output.status.code(),
            exit_signal(&output.status),
            &output.stderr,
        ),
        Err(error) => SandboxProbeEvidence {
            backend,
            executable,
            status: if error.kind() == std::io::ErrorKind::NotFound {
                SandboxProbeStatus::BackendMissing
            } else {
                SandboxProbeStatus::ProbeFailed
            },
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: Some(error.to_string()),
        },
    }
}

fn completed_probe_evidence(
    backend: String,
    executable: Option<PathBuf>,
    success: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
    stderr: &[u8],
) -> SandboxProbeEvidence {
    SandboxProbeEvidence {
        backend,
        executable,
        status: if success {
            SandboxProbeStatus::Available
        } else {
            SandboxProbeStatus::ProbeDenied
        },
        exit_code,
        signal,
        stderr: bounded_probe_text(stderr),
        io_error: None,
    }
}

fn format_probe_evidence(evidence: &SandboxProbeEvidence) -> String {
    let executable = evidence
        .executable
        .as_deref()
        .map(|path| format!(" at {}", path.display()))
        .unwrap_or_default();
    let mut detail = match evidence.status {
        SandboxProbeStatus::Available => {
            format!("{} probe succeeded{executable}", evidence.backend)
        }
        SandboxProbeStatus::BackendMissing => {
            format!("{} backend is missing{executable}", evidence.backend)
        }
        SandboxProbeStatus::ProbeDenied => {
            let termination = match (evidence.exit_code, evidence.signal) {
                (Some(code), _) => format!("exited {code}"),
                (None, Some(signal)) => format!("terminated by signal {signal}"),
                (None, None) => "terminated unsuccessfully".to_string(),
            };
            format!("{} probe {termination}{executable}", evidence.backend)
        }
        SandboxProbeStatus::ProbeFailed => {
            format!("{} probe could not start{executable}", evidence.backend)
        }
        SandboxProbeStatus::UnsupportedPlatform => {
            format!("{} is unsupported on this platform", evidence.backend)
        }
    };
    if let Some(stderr) = evidence.stderr.as_deref() {
        detail.push_str(&format!(": {stderr}"));
    } else if let Some(error) = evidence.io_error.as_deref() {
        detail.push_str(&format!(": {error}"));
    }
    detail
}

fn bounded_probe_text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(bytes);
    let mut result = text
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect::<String>();
    if result.len() > MAX_PROBE_DIAGNOSTIC_BYTES {
        let mut boundary = MAX_PROBE_DIAGNOSTIC_BYTES;
        while !result.is_char_boundary(boundary) {
            boundary -= 1;
        }
        result.truncate(boundary);
        result.push_str("...");
    }
    let result = result.trim().to_string();
    (!result.is_empty()).then_some(result)
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod enforcement_evidence_tests {
    use super::*;

    #[test]
    fn rejected_probe_preserves_backend_status_and_stderr() {
        let evidence = completed_probe_evidence(
            "seatbelt".to_string(),
            Some(PathBuf::from("/usr/bin/sandbox-exec")),
            false,
            Some(7),
            None,
            b"profile rejected",
        );

        assert_eq!(evidence.status, SandboxProbeStatus::ProbeDenied);
        assert_eq!(evidence.exit_code, Some(7));
        assert_eq!(evidence.signal, None);
        assert_eq!(evidence.stderr.as_deref(), Some("profile rejected"));
        let message = enforcement_unavailable_message("seatbelt", std::slice::from_ref(&evidence));
        assert!(message.contains("backend=seatbelt"));
        assert!(message.contains("seatbelt probe exited 7"));
        assert!(message.contains("profile rejected"));
    }

    #[test]
    fn missing_probe_executable_has_a_distinct_reason() {
        let evidence = command_probe_evidence(
            "bwrap",
            Some(PathBuf::from("bwrap")),
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")),
        );

        assert_eq!(evidence.status, SandboxProbeStatus::BackendMissing);
        assert!(
            enforcement_unavailable_remediation(std::slice::from_ref(&evidence))
                .contains("folder trust does not install sandbox support")
        );
    }

    #[test]
    fn probe_start_failure_and_unsupported_platform_remain_distinct() {
        let failed = command_probe_evidence(
            "seatbelt",
            Some(PathBuf::from("/usr/bin/sandbox-exec")),
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "launch blocked",
            )),
        );
        assert_eq!(failed.status, SandboxProbeStatus::ProbeFailed);
        assert_eq!(failed.io_error.as_deref(), Some("launch blocked"));

        let unsupported = SandboxProbeEvidence {
            backend: "platform-process".to_string(),
            executable: None,
            status: SandboxProbeStatus::UnsupportedPlatform,
            exit_code: None,
            signal: None,
            stderr: None,
            io_error: None,
        };
        assert!(
            enforcement_unavailable_message("platform-process", std::slice::from_ref(&unsupported))
                .contains("unsupported on this platform")
        );
    }

    #[test]
    fn compatibility_state_matches_the_typed_decision() {
        assert_eq!(enforcement_state(), enforcement_decision().state);
        assert_eq!(enforcement_evidence(), enforcement_decision().probes);
    }
}

#[cfg(test)]
pub fn seatbelt_available() -> bool {
    let available = platform::seatbelt_available();
    available
}

#[cfg(test)]
pub(crate) fn sandbox_test_parent(prefix: &str) -> tempfile::TempDir {
    #[cfg(target_os = "macos")]
    {
        let home = PathBuf::from(
            std::env::var_os("HOME").expect("HOME is required for macOS Seatbelt tests"),
        )
        .canonicalize()
        .expect("canonical macOS HOME");
        for root in [
            Some(PathBuf::from("/tmp")),
            std::env::var_os("TMPDIR").map(PathBuf::from),
        ]
        .into_iter()
        .flatten()
        {
            let root = root.canonicalize().unwrap_or(root);
            assert!(
                !home.starts_with(&root),
                "macOS Seatbelt fixtures require HOME outside temporary allow root {}",
                root.display()
            );
        }
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(home)
            .expect("sandbox parent outside temporary allow roots")
    }
    #[cfg(not(target_os = "macos"))]
    {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("sandbox parent")
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn workspace_write_bash_command(
        context: WorkspaceWriteSandboxCommandContext<'_>,
    ) -> Command {
        crate::sandbox::seatbelt::workspace_write_bash_command(context)
    }

    pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
        crate::sandbox::seatbelt::read_only_bash_command(context)
    }

    pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    pub fn platform_default_read_roots() -> Vec<PathBuf> {
        crate::sandbox::seatbelt::platform_default_read_roots()
    }

    #[cfg(test)]
    pub fn seatbelt_available() -> bool {
        crate::sandbox::seatbelt::enforced_available()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::Output;

        #[test]
        fn sandbox_blocks_writes_outside_workspace() {
            if !seatbelt_available() {
                return;
            }

            let parent = crate::sandbox::sandbox_test_parent("sandbox-module-deny-");
            let workspace_path = parent.path().join("workspace");
            std::fs::create_dir(&workspace_path).unwrap();
            let outside = parent.path().join("blocked.txt");

            let output: Output = bash_command(
                &format!("printf blocked > {}", outside.display()),
                &workspace_path,
            )
            .output()
            .unwrap();

            assert!(!output.status.success());
            assert!(!outside.exists());
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    #[cfg(target_os = "linux")]
    fn canonicalize_all(roots: &[PathBuf]) -> Vec<PathBuf> {
        roots
            .iter()
            .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn linux_sensitive_denied_roots() -> Vec<PathBuf> {
        let mut roots = dirs::home_dir()
            .map(|home| vec![home.join(".ssh"), home.join(".orca")])
            .unwrap_or_default();
        if let Some(config_dir) = orca_core::config::folder_trust::config_dir()
            && !roots.contains(&config_dir)
        {
            roots.push(config_dir);
        }
        roots.retain(|path| path.exists());
        roots
    }

    #[cfg(target_os = "linux")]
    pub fn workspace_write_bash_command(
        context: WorkspaceWriteSandboxCommandContext<'_>,
    ) -> Command {
        use crate::sandbox::bwrap::{LinuxReadScope, LinuxSandboxPolicy};
        use crate::sandbox::linux::{LinuxSandboxRequest, sandbox_command};

        let cwd = context
            .cwd
            .canonicalize()
            .unwrap_or_else(|_| context.cwd.to_path_buf());

        // Writable: the workspace cwd plus any explicit additional roots, plus
        // temp dirs unless excluded.
        let additional_roots = canonicalize_all(context.additional_roots);
        let metadata_writable_roots = canonicalize_all(
            &context
                .metadata_writable_roots
                .iter()
                .filter(|root| is_safe_metadata_writable_root(root))
                .cloned()
                .collect::<Vec<_>>(),
        );
        let mut writable_roots = vec![cwd.clone()];
        for root in &additional_roots {
            if !writable_roots.contains(root) {
                writable_roots.push(root.clone());
            }
        }
        let metadata_protection_roots = writable_roots.clone();
        if !context.exclude_slash_tmp {
            writable_roots.push(PathBuf::from("/tmp"));
        }
        if !context.exclude_tmpdir_env_var
            && let Some(tmpdir) = std::env::var_os("TMPDIR").map(PathBuf::from)
        {
            let tmpdir = tmpdir.canonicalize().unwrap_or(tmpdir);
            if !writable_roots.contains(&tmpdir) {
                writable_roots.push(tmpdir);
            }
        }

        // Protect workspace metadata (readable, not writable) unless the caller
        // explicitly granted that exact metadata directory through the
        // dedicated escalation channel. A broad ordinary writable root never
        // re-opens workspace metadata.
        let mut read_only_roots = Vec::new();
        for name in PROTECTED_METADATA_DIRS {
            let metadata = cwd.join(name);
            let canonical_metadata = metadata.canonicalize().unwrap_or_else(|_| metadata.clone());
            if metadata.exists() && !metadata_writable_roots.contains(&canonical_metadata) {
                read_only_roots.push(canonical_metadata);
            }
        }

        let mut denied_roots = canonicalize_all(context.denied_roots);
        // The selected cwd is an explicit user capability. Do not mask it
        // when a custom ORCA_HOME happens to contain the project; sibling
        // sensitive roots such as `.ssh` and `.orca` remain denied.
        for root in linux_sensitive_denied_roots()
            .into_iter()
            .filter(|root| !cwd.starts_with(root))
        {
            if !denied_roots.contains(&root) {
                denied_roots.push(root);
            }
        }

        let request = LinuxSandboxRequest {
            command: context.command.to_string(),
            policy: LinuxSandboxPolicy {
                cwd,
                read_scope: LinuxReadScope::Global,
                readable_roots: canonicalize_all(context.readable_roots),
                allowed_unix_socket_roots: canonicalize_all(context.allowed_unix_socket_roots),
                writable_roots,
                metadata_protection_roots,
                metadata_writable_roots,
                read_only_roots,
                denied_roots,
                network_access: context.network_access,
            },
            // Workspace-write is still a security-sensitive profile. A
            // partially enforced Landlock ruleset must never become a host
            // shell, even when the profile allows writes inside the workspace.
            strict: true,
        };
        sandbox_command(request)
    }

    #[cfg(target_os = "windows")]
    fn resolved_windows_shell_command(script: &str, cwd: &Path) -> Command {
        let shell = match orca_platform::shell::ShellResolver::for_current_host()
            .resolve_from_environment()
        {
            Ok(shell) => shell,
            Err(_) => {
                let mut command = Command::new("cmd.exe");
                command
                    .args(["/D", "/S", "/C", "exit /b 1"])
                    .current_dir(cwd);
                return command;
            }
        };
        let spec = shell.command(script);
        let mut command = Command::new(spec.program);
        command.args(spec.args).current_dir(cwd);
        command
    }

    #[cfg(target_os = "windows")]
    fn unsupported_windows_sandbox_command(cwd: &Path) -> Command {
        let shell =
            orca_platform::shell::ShellResolver::for_current_host().resolve_from_environment();
        let script = match shell.as_ref().map(|shell| shell.kind()) {
            Ok(orca_platform::shell::ShellKind::Cmd) => {
                "echo Windows sandbox commands must be launched through the runtime Windows sandbox 1>&2 & exit /b 1"
            }
            Ok(orca_platform::shell::ShellKind::PowerShell(_)) => {
                "Write-Error 'Windows sandbox commands must be launched through the runtime Windows sandbox'; exit 1"
            }
            Ok(
                orca_platform::shell::ShellKind::Posix | orca_platform::shell::ShellKind::GitBash,
            ) => {
                "printf '%s\\n' 'Windows sandbox commands must be launched through the runtime Windows sandbox' >&2; exit 1"
            }
            Err(_) => "exit /b 1",
        };
        resolved_windows_shell_command(script, cwd)
    }

    #[cfg(target_os = "linux")]
    pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    #[cfg(target_os = "windows")]
    pub fn workspace_write_bash_command(
        context: WorkspaceWriteSandboxCommandContext<'_>,
    ) -> Command {
        unsupported_windows_sandbox_command(context.cwd)
    }

    #[cfg(target_os = "windows")]
    pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
        unsupported_windows_sandbox_command(context.cwd)
    }

    #[cfg(target_os = "windows")]
    pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
        resolved_windows_shell_command(command, cwd)
    }

    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    pub fn workspace_write_bash_command(
        context: WorkspaceWriteSandboxCommandContext<'_>,
    ) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(context.command).current_dir(context.cwd);
        cmd
    }

    #[cfg(target_os = "linux")]
    pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
        use crate::sandbox::bwrap::{LinuxReadScope, LinuxSandboxPolicy};
        use crate::sandbox::linux::{LinuxSandboxRequest, sandbox_command};

        let cwd = context
            .cwd
            .canonicalize()
            .unwrap_or_else(|_| context.cwd.to_path_buf());

        // Additional roots are writable even in read-only mode (e.g. an
        // explicitly granted output directory), matching the Seatbelt profile.
        let writable_roots = canonicalize_all(context.additional_roots);
        let metadata_protection_roots = writable_roots.clone();
        let metadata_writable_roots = canonicalize_all(
            &context
                .metadata_writable_roots
                .iter()
                .filter(|root| is_safe_metadata_writable_root(root))
                .cloned()
                .collect::<Vec<_>>(),
        );

        let mut read_only_roots = Vec::new();
        for name in PROTECTED_METADATA_DIRS {
            let metadata = cwd.join(name);
            let canonical_metadata = metadata.canonicalize().unwrap_or_else(|_| metadata.clone());
            if metadata.exists() && !metadata_writable_roots.contains(&canonical_metadata) {
                read_only_roots.push(canonical_metadata);
            }
        }

        // A restricted read scope (allow_global_read == false) is fail-closed:
        // only listed roots are visible.
        let read_scope = if context.allow_global_read {
            LinuxReadScope::Global
        } else {
            LinuxReadScope::Restricted
        };

        let mut denied_roots = canonicalize_all(context.denied_roots);
        // The selected cwd is an explicit user capability. Do not mask it
        // when a custom ORCA_HOME happens to contain the project; sibling
        // sensitive roots such as `.ssh` and `.orca` remain denied.
        for root in linux_sensitive_denied_roots()
            .into_iter()
            .filter(|root| !cwd.starts_with(root))
        {
            if !denied_roots.contains(&root) {
                denied_roots.push(root);
            }
        }

        let request = LinuxSandboxRequest {
            command: context.command.to_string(),
            policy: LinuxSandboxPolicy {
                cwd,
                read_scope,
                readable_roots: canonicalize_all(context.readable_roots),
                allowed_unix_socket_roots: canonicalize_all(context.allowed_unix_socket_roots),
                writable_roots,
                metadata_protection_roots,
                metadata_writable_roots,
                read_only_roots,
                denied_roots,
                network_access: context.network_access,
            },
            // Every non-dangerous profile must be fully enforced. Global read
            // is an intentional capability, but it does not make additional
            // writes or network filtering safe to run best-effort.
            strict: true,
        };
        sandbox_command(request)
    }

    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(context.command).current_dir(context.cwd);
        cmd
    }

    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd
    }

    #[cfg(all(test, target_os = "linux"))]
    mod linux_tests {
        use super::*;

        #[test]
        fn plain_commands_use_the_native_posix_shell() {
            let cwd = std::env::current_dir().expect("current directory");
            let command = plain_bash_command("printf orca", &cwd);

            assert_eq!(command.get_program(), "sh");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [
                    std::ffi::OsStr::new("-c"),
                    std::ffi::OsStr::new("printf orca")
                ]
            );
            assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
        }
    }

    pub fn platform_default_read_roots() -> Vec<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            crate::sandbox::linux::platform_default_read_roots()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }
    }

    #[cfg(test)]
    pub fn seatbelt_available() -> bool {
        false
    }

    #[cfg(all(test, target_os = "windows"))]
    mod windows_tests {
        use super::*;

        #[test]
        fn plain_commands_use_the_resolved_windows_shell() {
            let cwd = std::env::current_dir().expect("current directory");
            let command = plain_bash_command("Write-Output orca", &cwd);
            let program = command.get_program().to_string_lossy().to_ascii_lowercase();
            assert!(!program.ends_with("\\sh"));
            assert!(!program.ends_with("/sh"));
            assert!(program.ends_with(".exe"));
        }

        #[test]
        fn restricted_compatibility_commands_fail_closed() {
            let cwd = std::env::current_dir().expect("current directory");
            let command = workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
                command: "Write-Output orca",
                cwd: &cwd,
                readable_roots: &[],
                additional_roots: &[],
                metadata_writable_roots: &[],
                denied_roots: &[],
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
                allowed_unix_socket_roots: &[],
            });
            let program = command.get_program().to_string_lossy().to_ascii_lowercase();
            assert!(!program.ends_with("\\sh"));
            assert!(!program.ends_with("/sh"));
        }
    }
}
