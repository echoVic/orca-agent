use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::sandbox::{ReadOnlySandboxCommandContext, WorkspaceWriteSandboxCommandContext};

static SEATBELT_AVAILABLE: OnceLock<bool> = OnceLock::new();
static SEATBELT_ENFORCED_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Absolute path to the macOS Seatbelt binary. Invoking it by absolute path
/// (rather than resolving `sandbox-exec` via `PATH`) prevents a spoofed
/// executable placed earlier in `PATH` from intercepting sandboxed execution.
const SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";
const SEATBELT_BASE_POLICY: &str = include_str!("seatbelt_base_policy.sbpl");

struct SeatbeltProfile {
    policy: String,
    parameters: Vec<(String, PathBuf)>,
}

struct SeatbeltProfileBuilder {
    policy: String,
    parameters: Vec<(String, PathBuf)>,
}

impl SeatbeltProfileBuilder {
    fn new() -> Self {
        Self {
            policy: SEATBELT_BASE_POLICY.trim_end().to_string(),
            parameters: Vec::new(),
        }
    }

    fn push_rule(&mut self, rule: impl AsRef<str>) {
        let rule = rule.as_ref();
        if !rule.is_empty() {
            self.policy.push('\n');
            self.policy.push_str(rule);
        }
    }

    fn path_parameter(&mut self, prefix: &str, path: &Path) -> String {
        let key = format!("{prefix}_{}", self.parameters.len());
        self.parameters.push((key.clone(), path.to_path_buf()));
        format!(r#"(param "{key}")"#)
    }

    fn finish(mut self) -> SeatbeltProfile {
        self.policy.push('\n');
        SeatbeltProfile {
            policy: self.policy,
            parameters: self.parameters,
        }
    }
}

struct WorkspaceWriteProfileContext<'a> {
    cwd: &'a Path,
    readable_roots: &'a [PathBuf],
    additional_roots: &'a [PathBuf],
    metadata_writable_roots: &'a [PathBuf],
    denied_roots: &'a [PathBuf],
    network_access: bool,
    exclude_tmpdir_env_var: bool,
    exclude_slash_tmp: bool,
    allowed_unix_socket_roots: &'a [PathBuf],
}

struct ReadOnlyProfileContext<'a> {
    cwd: &'a Path,
    readable_roots: &'a [PathBuf],
    additional_roots: &'a [PathBuf],
    metadata_writable_roots: &'a [PathBuf],
    denied_roots: &'a [PathBuf],
    network_access: bool,
    allow_global_read: bool,
    allowed_unix_socket_roots: &'a [PathBuf],
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
    let canonical_cwd = context
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| normalize_path_for_seatbelt(context.cwd));
    let canonical_readable_roots = context
        .readable_roots
        .iter()
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let canonical_additional_roots = context
        .additional_roots
        .iter()
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let canonical_metadata_writable_roots = context
        .metadata_writable_roots
        .iter()
        .filter(|root| crate::sandbox::is_safe_metadata_writable_root(root))
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let canonical_denied_roots = context
        .denied_roots
        .iter()
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let canonical_unix_socket_roots = context
        .allowed_unix_socket_roots
        .iter()
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let profile = build_workspace_write_profile(WorkspaceWriteProfileContext {
        cwd: &canonical_cwd,
        readable_roots: &canonical_readable_roots,
        additional_roots: &canonical_additional_roots,
        metadata_writable_roots: &canonical_metadata_writable_roots,
        denied_roots: &canonical_denied_roots,
        network_access: context.network_access,
        exclude_tmpdir_env_var: context.exclude_tmpdir_env_var,
        exclude_slash_tmp: context.exclude_slash_tmp,
        allowed_unix_socket_roots: &canonical_unix_socket_roots,
    });
    let mut cmd = seatbelt_command(profile);
    cmd.arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(context.command)
        .current_dir(context.cwd)
        .env("ORCA_SANDBOX", "seatbelt");
    cmd
}

pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext<'_>) -> Command {
    let canonical_cwd = context
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| normalize_path_for_seatbelt(context.cwd));
    let canonical_readable_roots = context
        .readable_roots
        .iter()
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let canonical_metadata_writable_roots = context
        .metadata_writable_roots
        .iter()
        .filter(|root| crate::sandbox::is_safe_metadata_writable_root(root))
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let canonical_additional_roots = context
        .additional_roots
        .iter()
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let canonical_denied_roots = context
        .denied_roots
        .iter()
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let canonical_unix_socket_roots = context
        .allowed_unix_socket_roots
        .iter()
        .map(|root| normalize_path_for_seatbelt(root))
        .collect::<Vec<_>>();
    let profile = build_read_only_profile(ReadOnlyProfileContext {
        cwd: &canonical_cwd,
        readable_roots: &canonical_readable_roots,
        additional_roots: &canonical_additional_roots,
        metadata_writable_roots: &canonical_metadata_writable_roots,
        denied_roots: &canonical_denied_roots,
        network_access: context.network_access,
        allow_global_read: context.allow_global_read,
        allowed_unix_socket_roots: &canonical_unix_socket_roots,
    });
    let mut cmd = seatbelt_command(profile);
    cmd.arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(context.command)
        .current_dir(context.cwd)
        .env("ORCA_SANDBOX", "seatbelt");
    cmd
}

fn seatbelt_command(profile: SeatbeltProfile) -> Command {
    let mut command = Command::new(SEATBELT_EXECUTABLE);
    command.arg("-p").arg(profile.policy);
    for (key, value) in profile.parameters {
        let mut definition = OsString::from("-D");
        definition.push(key);
        definition.push("=");
        definition.push(value);
        command.arg(definition);
    }
    command
}

fn normalize_path_for_seatbelt(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = ancestor.canonicalize() {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let Some(name) = ancestor.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return path.to_path_buf();
        };
        ancestor = parent;
    }
}

pub fn available() -> bool {
    *SEATBELT_AVAILABLE.get_or_init(|| {
        Command::new(SEATBELT_EXECUTABLE)
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("true")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    })
}

/// Probe the actual restrictive path used for tool execution. A permissive
/// binary probe is insufficient in privileged/container runtimes where the
/// kernel refuses to install a sandbox profile.
pub fn enforced_available() -> bool {
    *SEATBELT_ENFORCED_AVAILABLE.get_or_init(|| {
        Command::new(SEATBELT_EXECUTABLE)
            .arg("-p")
            .arg(
                "(version 1) (deny default) (allow process*) (allow sysctl-read) (allow file-read* (literal \"/dev/null\"))",
            )
            .arg("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

pub fn platform_default_read_roots() -> Vec<PathBuf> {
    ["/bin", "/sbin", "/usr", "/System", "/Library", "/etc"]
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn build_workspace_write_profile(context: WorkspaceWriteProfileContext<'_>) -> SeatbeltProfile {
    let WorkspaceWriteProfileContext {
        cwd,
        readable_roots,
        additional_roots,
        metadata_writable_roots,
        denied_roots,
        network_access,
        exclude_tmpdir_env_var,
        exclude_slash_tmp,
        allowed_unix_socket_roots,
    } = context;
    let mut profile = SeatbeltProfileBuilder::new();
    let cwd_param = profile.path_parameter("WORKSPACE", cwd);
    append_read_allow_rules(&mut profile, &[cwd.to_path_buf()]);
    append_path_ancestor_metadata_rules(&mut profile, cwd);
    profile.push_rule(format!("(allow file-write* (subpath {cwd_param}))"));
    append_read_allow_rules(&mut profile, &platform_default_read_roots());
    append_read_allow_rules(&mut profile, readable_roots);
    append_read_allow_rules(&mut profile, additional_roots);
    if !exclude_tmpdir_env_var && let Some(path) = std::env::var_os("TMPDIR").map(PathBuf::from) {
        let path = path.canonicalize().unwrap_or(path);
        append_write_allow_rule(&mut profile, "TMPDIR", &path);
    }
    if !exclude_slash_tmp {
        append_write_allow_rule(&mut profile, "SLASH_TMP", Path::new("/tmp"));
        if let Ok(canonical) = Path::new("/tmp").canonicalize()
            && canonical != Path::new("/tmp")
        {
            append_write_allow_rule(&mut profile, "SLASH_TMP", &canonical);
        }
    }
    append_write_allow_rules(&mut profile, "WRITABLE_ROOT", additional_roots);
    append_protected_metadata_name_deny_rule(&mut profile);
    append_protected_workspace_metadata_deny_rules(&mut profile, cwd);
    // Explicit metadata escalation: emitted AFTER the metadata deny rules so a
    // precisely-approved `.git`/`.agents`/`.codex` target can override the
    // default protection. General `additional_roots` are emitted BEFORE the
    // deny rules and can therefore never re-open workspace metadata.
    append_metadata_write_allow_rules(&mut profile, metadata_writable_roots);
    append_access_deny_rules(&mut profile, denied_roots);
    if let Some(home) = dirs::home_dir() {
        append_access_deny_rule(&mut profile, "SSH_DENY", &home.join(".ssh"), false);
        append_access_deny_rule(&mut profile, "ORCA_DENY", &home.join(".orca"), false);
    }
    if network_access {
        profile.push_rule("(allow network-outbound)");
    }
    append_unix_socket_allow_rules(&mut profile, allowed_unix_socket_roots);
    profile.finish()
}

fn build_read_only_profile(context: ReadOnlyProfileContext<'_>) -> SeatbeltProfile {
    let ReadOnlyProfileContext {
        cwd,
        readable_roots,
        additional_roots,
        metadata_writable_roots,
        denied_roots,
        network_access,
        allow_global_read,
        allowed_unix_socket_roots,
    } = context;
    let mut profile = SeatbeltProfileBuilder::new();
    if allow_global_read {
        profile.push_rule("(allow file-read*)");
    }
    profile.push_rule("(allow file-read* (literal \"/\"))");
    append_read_allow_rules(&mut profile, &[cwd.to_path_buf()]);
    append_path_ancestor_metadata_rules(&mut profile, cwd);
    append_read_allow_rules(&mut profile, readable_roots);
    append_read_allow_rules(&mut profile, additional_roots);
    append_write_allow_rules(&mut profile, "WRITABLE_ROOT", additional_roots);
    append_protected_metadata_name_deny_rule(&mut profile);
    append_protected_workspace_metadata_deny_rules(&mut profile, cwd);
    append_metadata_write_allow_rules(&mut profile, metadata_writable_roots);
    append_access_deny_rules(&mut profile, denied_roots);
    if network_access {
        profile.push_rule("(allow network-outbound)");
    }
    append_unix_socket_allow_rules(&mut profile, allowed_unix_socket_roots);
    profile.finish()
}

#[cfg(test)]
fn workspace_write_profile(context: WorkspaceWriteProfileContext<'_>) -> String {
    render_profile_for_tests(build_workspace_write_profile(context))
}

#[cfg(test)]
fn read_only_profile(context: ReadOnlyProfileContext<'_>) -> String {
    render_profile_for_tests(build_read_only_profile(context))
}

#[cfg(test)]
fn render_profile_for_tests(profile: SeatbeltProfile) -> String {
    let mut rendered = profile.policy;
    for (key, value) in profile.parameters {
        rendered = rendered.replace(
            &format!(r#"(param "{key}")"#),
            &format!(r#""{}""#, seatbelt_escape(&value.display().to_string())),
        );
    }
    rendered
}

fn append_unix_socket_allow_rules(
    profile: &mut SeatbeltProfileBuilder,
    allowed_unix_socket_roots: &[PathBuf],
) {
    if allowed_unix_socket_roots.is_empty() {
        return;
    }
    profile.push_rule("(allow system-socket (socket-domain AF_UNIX))");
    for root in allowed_unix_socket_roots {
        let path = profile.path_parameter("UNIX_SOCKET_ROOT", root);
        profile.push_rule(format!(
            "(allow network-bind (local unix-socket (literal {path})))"
        ));
        profile.push_rule(format!(
            "(allow network-bind (local unix-socket (subpath {path})))"
        ));
        profile.push_rule(format!(
            "(allow network-outbound (remote unix-socket (literal {path})))"
        ));
        profile.push_rule(format!(
            "(allow network-outbound (remote unix-socket (subpath {path})))"
        ));
    }
}

fn append_read_allow_rules(profile: &mut SeatbeltProfileBuilder, readable_roots: &[PathBuf]) {
    for root in readable_roots {
        let path = profile.path_parameter("READABLE_ROOT", root);
        profile.push_rule(format!("(allow file-read* (subpath {path}))"));
    }
}

fn append_path_ancestor_metadata_rules(profile: &mut SeatbeltProfileBuilder, path: &Path) {
    for ancestor in path.ancestors() {
        let parameter = profile.path_parameter("PATH_ANCESTOR", ancestor);
        profile.push_rule(format!("(allow file-read-metadata (literal {parameter}))"));
        if ancestor == Path::new("/") {
            break;
        }
    }
}

fn append_write_allow_rules(profile: &mut SeatbeltProfileBuilder, prefix: &str, roots: &[PathBuf]) {
    for root in roots {
        append_write_allow_rule(profile, prefix, root);
    }
}

fn append_write_allow_rule(profile: &mut SeatbeltProfileBuilder, prefix: &str, root: &Path) {
    let path = profile.path_parameter(prefix, root);
    profile.push_rule(format!("(allow file-write* (subpath {path}))"));
}

fn append_metadata_write_allow_rules(profile: &mut SeatbeltProfileBuilder, roots: &[PathBuf]) {
    for root in roots {
        let path = profile.path_parameter("METADATA_WRITABLE_ROOT", root);
        profile.push_rule(format!("(allow file-write* (literal {path}))"));
        profile.push_rule(format!("(allow file-write* (subpath {path}))"));
    }
}

fn append_access_deny_rules(profile: &mut SeatbeltProfileBuilder, denied_roots: &[PathBuf]) {
    for root in denied_roots {
        append_access_deny_rule(profile, "DENIED_ROOT", root, root.is_file());
    }
}

fn append_access_deny_rule(
    profile: &mut SeatbeltProfileBuilder,
    prefix: &str,
    root: &Path,
    literal: bool,
) {
    let path = profile.path_parameter(prefix, root);
    let matcher = if literal { "literal" } else { "subpath" };
    profile.push_rule(format!("(deny file-read* file-write* ({matcher} {path}))"));
}

fn append_protected_metadata_name_deny_rule(profile: &mut SeatbeltProfileBuilder) {
    profile.push_rule(r#"(deny file-write* (regex #"(^|/)\.(git|agents|codex)(/.*)?$"))"#);
}

fn append_protected_workspace_metadata_deny_rules(
    profile: &mut SeatbeltProfileBuilder,
    cwd: &Path,
) {
    for name in [".git", ".agents", ".codex"] {
        let metadata = cwd.join(name);
        let mut protected_paths = vec![metadata.clone()];
        if let Ok(canonical) = metadata.canonicalize()
            && !protected_paths.contains(&canonical)
        {
            protected_paths.push(canonical);
        }
        for path in protected_paths {
            let path = profile.path_parameter("PROTECTED_METADATA", &path);
            profile.push_rule(format!("(deny file-write* (literal {path}))"));
            profile.push_rule(format!("(deny file-write* (subpath {path}))"));
        }
    }
}

#[cfg(test)]
fn seatbelt_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_unix_socket_path(name: &str) -> PathBuf {
        PathBuf::from("/tmp").join(name)
    }
    use std::ffi::OsStr;
    use std::io::Write;
    use std::process::Output;
    use tempfile::TempDir;

    fn assert_seatbelt_available() {
        assert!(
            available(),
            "macOS Seatbelt is required: {SEATBELT_EXECUTABLE} could not compile and run the probe policy"
        );
    }

    #[test]
    fn seatbelt_command_uses_trusted_absolute_executable() {
        let workspace = TempDir::new().unwrap();
        let command = bash_command("true", workspace.path());

        assert_eq!(command.get_program(), OsStr::new(SEATBELT_EXECUTABLE));
    }

    #[test]
    fn filesystem_paths_are_passed_as_seatbelt_parameters() {
        let workspace = TempDir::new().unwrap();
        let injected = workspace
            .path()
            .join("write-root\n(allow file-write* (subpath \"/\"))");
        let command = bash_command_with_additional_roots(
            "true",
            workspace.path(),
            std::slice::from_ref(&injected),
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let policy = args
            .windows(2)
            .find(|pair| pair[0] == "-p")
            .map(|pair| pair[1].as_str())
            .expect("seatbelt profile after -p");

        assert!(
            !policy.contains(&injected.display().to_string()),
            "filesystem path was interpolated into SBPL: {policy}"
        );
        assert!(
            args.iter().any(|arg| {
                arg.starts_with("-DWRITABLE_ROOT_")
                    && arg.ends_with(&injected.display().to_string())
            }),
            "filesystem path was not passed through a Seatbelt parameter: {args:?}"
        );
    }

    #[test]
    fn seatbelt_command_marks_nested_sandbox_environment() {
        let workspace = TempDir::new().unwrap();
        let command = bash_command("true", workspace.path());

        assert!(command.get_envs().any(|(key, value)| {
            key == OsStr::new("ORCA_SANDBOX") && value == Some(OsStr::new("seatbelt"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn path_cannot_override_seatbelt_executable() {
        use std::os::unix::fs::PermissionsExt;

        assert_seatbelt_available();
        let workspace = TempDir::new().unwrap();
        let fake_bin = workspace.path().join("fake-bin");
        std::fs::create_dir(&fake_bin).unwrap();
        let fake_marker = workspace.path().join("fake-seatbelt-ran");
        let fake_seatbelt = fake_bin.join("sandbox-exec");
        std::fs::write(
            &fake_seatbelt,
            format!(
                "#!/bin/sh\nprintf spoofed > '{}'\nexit 99\n",
                fake_marker.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_seatbelt).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_seatbelt, permissions).unwrap();

        let mut command = bash_command("printf actual", workspace.path());
        command.env("PATH", &fake_bin);
        let output = command.output().unwrap();

        assert!(
            output.status.success(),
            "trusted Seatbelt command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "actual");
        assert!(!fake_marker.exists(), "PATH-injected sandbox-exec ran");
    }

    #[test]
    fn seatbelt_child_observes_nested_sandbox_environment() {
        assert_seatbelt_available();
        let workspace = TempDir::new().unwrap();

        let output = bash_command("printf %s \"$ORCA_SANDBOX\"", workspace.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "seatbelt");
    }

    #[test]
    fn invalid_profile_never_executes_original_command() {
        assert_seatbelt_available();
        let workspace = TempDir::new().unwrap();
        let marker = workspace.path().join("invalid-profile-ran");

        let output = Command::new(SEATBELT_EXECUTABLE)
            .arg("-p")
            .arg("(version 1)\n(allow file-read*")
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg("printf ran > \"$ORCA_INVALID_PROFILE_MARKER\"")
            .env("ORCA_INVALID_PROFILE_MARKER", &marker)
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(
            !marker.exists(),
            "invalid profile executed original command"
        );
    }

    #[test]
    fn workspace_write_sandbox_blocks_tcp_when_network_is_disabled() {
        assert_seatbelt_available();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let workspace = TempDir::new().unwrap();

        let output = workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
            command: &format!("/usr/bin/nc -z -w 1 127.0.0.1 {port}"),
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(
            !output.status.success(),
            "network-disabled sandbox connected to local TCP listener"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_client_helper() {
        let Some(socket_path) = std::env::var_os("ORCA_SEATBELT_SOCKET_CLIENT") else {
            return;
        };

        let mut stream = std::os::unix::net::UnixStream::connect(socket_path).unwrap();
        stream.write_all(b"connected").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_write_sandbox_allows_only_configured_unix_socket() {
        use std::io::Read;

        assert_seatbelt_available();
        let workspace = TempDir::new().unwrap();
        let allowed_socket = workspace.path().join("allowed.sock");
        let blocked_socket = workspace.path().join("blocked.sock");
        let allowed_listener = std::os::unix::net::UnixListener::bind(&allowed_socket).unwrap();
        let blocked_listener = std::os::unix::net::UnixListener::bind(&blocked_socket).unwrap();
        let helper = std::env::current_exe().unwrap();
        let helper_command = "\"$ORCA_SEATBELT_SOCKET_HELPER\" --exact sandbox::seatbelt::tests::unix_socket_client_helper --nocapture";

        let mut allowed = workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
            command: helper_command,
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: std::slice::from_ref(&allowed_socket),
        });
        allowed
            .env("ORCA_SEATBELT_SOCKET_HELPER", &helper)
            .env("ORCA_SEATBELT_SOCKET_CLIENT", &allowed_socket);
        let allowed_output = allowed.output().unwrap();
        assert!(
            allowed_output.status.success(),
            "configured Unix socket connection failed: {}",
            String::from_utf8_lossy(&allowed_output.stderr)
        );
        let (mut allowed_stream, _) = allowed_listener.accept().unwrap();
        let mut payload = String::new();
        allowed_stream.read_to_string(&mut payload).unwrap();
        assert_eq!(payload, "connected");

        let mut blocked = workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
            command: helper_command,
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        blocked
            .env("ORCA_SEATBELT_SOCKET_HELPER", &helper)
            .env("ORCA_SEATBELT_SOCKET_CLIENT", &blocked_socket);
        let blocked_output = blocked.output().unwrap();

        assert!(
            !blocked_output.status.success(),
            "unconfigured Unix socket connection unexpectedly succeeded"
        );
        drop(blocked_listener);
    }

    #[test]
    fn sandbox_profile_allows_workspace_and_null_device() {
        let workspace = TempDir::new().unwrap();
        let content = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        assert!(content.contains("(version 1)"));
        assert!(content.contains(&workspace.path().display().to_string()));
        assert!(!content.contains("\n(allow file-read*)\n"));
        assert!(content.contains(r#"(allow file-read* file-write* (literal "/dev/null"))"#));
    }

    #[test]
    fn sandbox_profiles_allow_signalling_child_processes() {
        let workspace = TempDir::new().unwrap();
        let workspace_profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        let read_only_profile = read_only_profile(ReadOnlyProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        for profile in [workspace_profile, read_only_profile] {
            assert!(
                profile.contains("(allow signal (target children))"),
                "sandboxed process managers must be able to terminate their own workers: {profile}"
            );
            assert!(
                !profile.lines().any(|line| line.trim() == "(allow signal)")
                    && !profile.contains("(target others)")
                    && !profile.contains("(target same-sandbox)"),
                "child cleanup must not grant authority to signal unrelated processes: {profile}"
            );
        }
    }

    #[test]
    fn workspace_write_sandbox_can_terminate_a_child_process() {
        assert_seatbelt_available();

        let workspace = TempDir::new().unwrap();
        let output = bash_command(
            "sleep 0.2 & child=$!; kill -TERM \"$child\"; rc=$?; wait \"$child\" || true; exit \"$rc\"",
            workspace.path(),
        )
        .output()
        .unwrap();

        assert!(
            output.status.success(),
            "sandboxed test runners must be able to clean up child workers\nstatus: {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn read_only_sandbox_can_terminate_a_child_process() {
        assert_seatbelt_available();

        let workspace = TempDir::new().unwrap();
        let command = "sleep 0.2 & child=$!; kill -TERM \"$child\"; rc=$?; wait \"$child\" || true; exit \"$rc\"";
        let output = read_only_bash_command(ReadOnlySandboxCommandContext {
            command,
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(
            output.status.success(),
            "read-only sandboxed test runners must be able to clean up child workers\nstatus: {:?}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn platform_default_read_roots_include_shell_runtime_paths() {
        let roots = platform_default_read_roots();

        assert!(roots.contains(&PathBuf::from("/bin")));
        assert!(roots.contains(&PathBuf::from("/usr")));
        assert!(roots.contains(&PathBuf::from("/System")));
    }

    #[test]
    fn profile_denies_sensitive_orca_and_ssh_paths() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });

        assert!(profile.contains("(deny file-read* file-write*"));
        assert!(profile.contains("/.ssh"));
        assert!(profile.contains("/.orca"));
        // deny rules must come AFTER allow rules (last-match-wins in Seatbelt)
        let allow_write_pos = profile.find("(allow file-write*").unwrap();
        let deny_pos = profile.find("(deny file-read* file-write*").unwrap();
        assert!(
            deny_pos > allow_write_pos,
            "deny must come after allow for last-match-wins"
        );
    }

    #[test]
    fn workspace_write_profile_protects_workspace_metadata_by_default() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        let allow_workspace = format!(
            r#"(allow file-write* (subpath "{}"))"#,
            workspace.path().display()
        );
        let deny_git = format!(
            r#"(deny file-write* (subpath "{}"))"#,
            workspace.path().join(".git").display()
        );
        let deny_git_reads = format!(
            r#"(deny file-read* file-write* (subpath "{}"))"#,
            workspace.path().join(".git").display()
        );

        assert!(profile.contains(&deny_git), "{profile}");
        assert!(
            !profile.contains(&deny_git_reads),
            "metadata protection must preserve reads for git commands: {profile}"
        );
        assert!(
            profile.find(&deny_git).unwrap() > profile.find(&allow_workspace).unwrap(),
            "metadata deny must override workspace write: {profile}"
        );
    }

    #[test]
    fn workspace_write_profile_allows_explicit_metadata_write_root() {
        let workspace = TempDir::new().unwrap();
        let git_dir = workspace.path().join(".git");
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: std::slice::from_ref(&git_dir),
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        let deny_git = format!(r#"(deny file-write* (subpath "{}"))"#, git_dir.display());
        let allow_git = format!(r#"(allow file-write* (subpath "{}"))"#, git_dir.display());

        assert!(profile.contains(&deny_git), "{profile}");
        assert!(profile.contains(&allow_git), "{profile}");
        assert!(
            profile.find(&allow_git).unwrap() > profile.find(&deny_git).unwrap(),
            "explicit metadata grant must override default metadata protection: {profile}"
        );
    }

    #[test]
    fn workspace_write_profile_general_root_cannot_override_metadata_protection() {
        let workspace = TempDir::new().unwrap();
        let git_dir = workspace.path().join(".git");
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: std::slice::from_ref(&git_dir),
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        let deny_git = format!(r#"(deny file-write* (subpath "{}"))"#, git_dir.display());
        let allow_git = format!(r#"(allow file-write* (subpath "{}"))"#, git_dir.display());

        assert!(profile.contains(&deny_git), "{profile}");
        assert!(profile.contains(&allow_git), "{profile}");
        assert!(
            profile.find(&allow_git).unwrap() < profile.find(&deny_git).unwrap(),
            "a general additional root must NOT re-open metadata protection (deny must win): {profile}"
        );
    }

    #[test]
    fn workspace_write_sandbox_blocks_workspace_git_writes_by_default() {
        assert_seatbelt_available();

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let git_dir = workspace.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let target = git_dir.join("config");

        let output = bash_command(
            &format!("printf blocked > {}", target.display()),
            workspace.path(),
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[test]
    fn workspace_write_sandbox_allows_workspace_git_reads_by_default() {
        assert_seatbelt_available();

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let git_dir = workspace.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(
            git_dir.join("config"),
            "[core]\nrepositoryformatversion = 0\n",
        )
        .unwrap();

        let output = bash_command("cat .git/config", workspace.path())
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "workspace metadata reads must be allowed for git commands\nstatus: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("repositoryformatversion"));
    }

    #[test]
    fn workspace_write_sandbox_general_root_cannot_write_workspace_git() {
        assert_seatbelt_available();

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let git_dir = workspace.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let target = git_dir.join("config");

        // A general additional writable root that happens to cover `.git` must
        // NOT be able to write workspace metadata; the deny rule wins.
        let output = bash_command_with_additional_roots(
            &format!("printf blocked > {}", target.display()),
            workspace.path(),
            std::slice::from_ref(&git_dir),
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[test]
    fn workspace_write_sandbox_parent_root_cannot_write_workspace_metadata() {
        assert_seatbelt_available();

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();

        for metadata_name in [".git", ".agents", ".codex"] {
            let metadata_dir = workspace.join(metadata_name);
            std::fs::create_dir(&metadata_dir).unwrap();
            let target = metadata_dir.join("blocked.txt");
            let output = bash_command_with_additional_roots(
                &format!("printf blocked > {}", target.display()),
                &workspace,
                &[parent.path().to_path_buf()],
            )
            .output()
            .unwrap();

            assert!(
                !output.status.success(),
                "broad parent grant unexpectedly opened {metadata_name}"
            );
            assert!(!target.exists(), "{metadata_name} write escaped protection");
        }
    }

    #[test]
    fn workspace_write_sandbox_external_root_cannot_write_its_metadata() {
        assert_seatbelt_available();

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = parent.path().join("workspace");
        let external_repo = parent.path().join("external-repo");
        let external_git = external_repo.join(".git");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(&external_git).unwrap();
        let target = external_git.join("blocked.txt");

        let output = bash_command_with_additional_roots(
            &format!("printf blocked > {}", target.display()),
            &workspace,
            std::slice::from_ref(&external_repo),
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[test]
    fn workspace_write_sandbox_metadata_descendant_grant_does_not_escalate() {
        assert_seatbelt_available();

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = parent.path().join("workspace");
        let external_git = parent.path().join("external-repo").join(".git");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(&external_git).unwrap();
        let target = external_git.join("config");

        let output = bash_command_with_additional_roots(
            &format!("printf blocked > {}", target.display()),
            &workspace,
            std::slice::from_ref(&target),
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[test]
    fn workspace_write_sandbox_parent_root_cannot_overwrite_git_pointer_file() {
        assert_seatbelt_available();

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let git_pointer = workspace.join(".git");
        std::fs::write(&git_pointer, "gitdir: ../metadata").unwrap();

        let output = bash_command_with_additional_roots(
            &format!("printf replaced > {}", git_pointer.display()),
            &workspace,
            &[parent.path().to_path_buf()],
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert_eq!(
            std::fs::read_to_string(git_pointer).unwrap(),
            "gitdir: ../metadata"
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_write_sandbox_cannot_write_through_symlinked_workspace_metadata() {
        assert_seatbelt_available();

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = parent.path().join("workspace");
        let metadata_target = parent.path().join("metadata-target");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&metadata_target).unwrap();
        std::os::unix::fs::symlink(&metadata_target, workspace.join(".git")).unwrap();
        let target = metadata_target.join("blocked.txt");
        let symlinked_target = workspace.join(".git").join("blocked.txt");

        let output = bash_command_with_additional_roots(
            &format!("printf blocked > {}", symlinked_target.display()),
            &workspace,
            &[parent.path().to_path_buf()],
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_write_sandbox_rejects_explicit_symlinked_metadata_root() {
        assert_seatbelt_available();

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = parent.path().join("workspace");
        let metadata_target = parent.path().join("external-target");
        let metadata_link = workspace.join(".agents");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&metadata_target).unwrap();
        std::os::unix::fs::symlink(&metadata_target, &metadata_link).unwrap();
        let target = metadata_target.join("blocked.txt");

        let output = workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
            command: &format!("printf blocked > {}", target.display()),
            cwd: &workspace,
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: std::slice::from_ref(&metadata_link),
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[test]
    fn workspace_write_sandbox_allows_explicit_metadata_write_root() {
        assert_seatbelt_available();

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let git_dir = workspace.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let target = git_dir.join("config");

        // Only an explicit metadata escalation may override the default
        // protection and write workspace metadata.
        let output = workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
            command: &format!("printf allowed > {}", target.display()),
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: std::slice::from_ref(&git_dir),
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(output.status.success());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "allowed");
    }

    #[test]
    fn read_only_profile_does_not_allow_workspace_writes() {
        let workspace = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(!profile.contains(&format!(
            r#"(allow file-write* (subpath "{}"))"#,
            workspace.path().display()
        )));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn read_only_profile_allows_additional_write_roots() {
        let workspace = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[extra.path().to_path_buf()],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(!profile.contains(&format!(
            r#"(allow file-write* (subpath "{}"))"#,
            workspace.path().display()
        )));
        assert!(profile.contains(&format!(
            r#"(allow file-write* (subpath "{}"))"#,
            extra.path().display()
        )));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn read_only_sandbox_parent_write_root_cannot_write_workspace_metadata() {
        assert_seatbelt_available();

        let parent = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let workspace = parent.path().join("workspace");
        let git_dir = workspace.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let target = git_dir.join("blocked.txt");

        let output = read_only_bash_command(ReadOnlySandboxCommandContext {
            command: &format!("printf blocked > {}", target.display()),
            cwd: &workspace,
            readable_roots: &[],
            additional_roots: &[parent.path().to_path_buf()],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert!(!target.exists());
    }

    #[test]
    fn read_only_profile_allows_additional_read_roots_without_writes() {
        let readable = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            cwd: readable.path(),
            readable_roots: &[readable.path().to_path_buf()],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(profile.contains(&format!(
            r#"(allow file-read* (subpath "{}"))"#,
            readable.path().display()
        )));
        assert!(!profile.contains(&format!(
            r#"(allow file-write* (subpath "{}"))"#,
            readable.path().display()
        )));
    }

    #[test]
    fn read_only_profile_denies_additional_root_descendant_access() {
        let extra = TempDir::new().unwrap();
        let blocked = extra.path().join("blocked");
        let profile = read_only_profile(ReadOnlyProfileContext {
            cwd: extra.path(),
            readable_roots: &[],
            additional_roots: &[extra.path().to_path_buf()],
            metadata_writable_roots: &[],
            denied_roots: std::slice::from_ref(&blocked),
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        let allow = format!(
            r#"(allow file-write* (subpath "{}"))"#,
            extra.path().display()
        );
        let deny = format!(
            r#"(deny file-read* file-write* (subpath "{}"))"#,
            blocked.display()
        );

        assert!(profile.contains(&allow));
        assert!(profile.contains(&deny));
        assert!(
            profile.find(&deny).unwrap() > profile.find(&allow).unwrap(),
            "deny access rules must come after allow rules"
        );
    }

    #[test]
    fn read_only_profile_uses_literal_deny_rules_for_files() {
        let extra = TempDir::new().unwrap();
        let denied_file = extra.path().join("secret.env");
        std::fs::write(&denied_file, "secret").unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            cwd: extra.path(),
            readable_roots: &[],
            additional_roots: &[extra.path().to_path_buf()],
            metadata_writable_roots: &[],
            denied_roots: std::slice::from_ref(&denied_file),
            network_access: false,
            allow_global_read: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(profile.contains(&format!(
            r#"(deny file-read* file-write* (literal "{}"))"#,
            denied_file.display()
        )));
    }

    #[test]
    fn read_only_profile_can_disable_global_reads() {
        let readable = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            cwd: readable.path(),
            readable_roots: &[readable.path().to_path_buf()],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: false,
            allowed_unix_socket_roots: &[],
        });

        assert!(!profile.contains("\n(allow file-read*)\n"));
        assert!(profile.contains(&format!(
            r#"(allow file-read* (subpath "{}"))"#,
            readable.path().display()
        )));
    }

    #[test]
    fn strict_read_only_sandbox_blocks_reads_outside_allowed_roots() {
        assert_seatbelt_available();

        let parent = crate::sandbox::sandbox_test_parent("seatbelt-outside-deny-");
        let workspace_path = parent.path().join("workspace");
        let readable_path = parent.path().join("readable");
        std::fs::create_dir(&workspace_path).unwrap();
        std::fs::create_dir(&readable_path).unwrap();
        let allowed = readable_path.join("allowed.txt");
        let blocked = parent.path().join("blocked.txt");
        std::fs::write(&allowed, "allowed").unwrap();
        std::fs::write(&blocked, "blocked").unwrap();

        let command_text = format!(
            "cat {} >/dev/null && cat {} >/dev/null",
            allowed.display(),
            blocked.display()
        );
        let output: Output = read_only_bash_command(ReadOnlySandboxCommandContext {
            command: &command_text,
            cwd: &workspace_path,
            readable_roots: &[readable_path],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: false,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(
            !output.status.success(),
            "strict read-only sandbox should reject unlisted reads"
        );
    }

    #[test]
    fn strict_read_only_sandbox_allows_basic_shell_commands() {
        assert_seatbelt_available();

        let workspace = TempDir::new().unwrap();
        let roots = platform_default_read_roots();
        let output = read_only_bash_command(ReadOnlySandboxCommandContext {
            command: "echo hi",
            cwd: workspace.path(),
            readable_roots: &roots,
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: false,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(
            output.status.success(),
            "strict read-only shell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hi\n");
    }

    #[test]
    fn workspace_write_profile_includes_canonical_slash_tmp_rule() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });

        assert!(profile.contains(r#"(allow file-write* (subpath "/tmp"))"#));
        if let Ok(canonical) = Path::new("/tmp").canonicalize()
            && canonical != Path::new("/tmp")
        {
            assert!(
                profile.contains(&format!(
                    r#"(allow file-write* (subpath "{}"))"#,
                    canonical.display()
                )),
                "profile must allow the resolved /tmp path (seatbelt matches resolved paths): {profile}"
            );
        }
    }

    #[test]
    fn workspace_write_sandbox_allows_writes_under_slash_tmp() {
        assert_seatbelt_available();

        let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
        let tmp_target = TempDir::new_in("/tmp").unwrap();
        let target = tmp_target.path().join("allowed.txt");

        let output = bash_command(
            &format!("printf allowed > {}", target.display()),
            workspace.path(),
        )
        .output()
        .unwrap();

        assert!(
            output.status.success(),
            "writes under /tmp must be allowed by the workspace profile\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "allowed");
    }

    #[test]
    fn workspace_write_profile_can_exclude_tmp_writes_and_network() {
        let workspace = TempDir::new().unwrap();
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
            allowed_unix_socket_roots: &[],
        });

        assert!(!profile.contains(r#"(subpath "/tmp")"#));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn workspace_write_profile_allows_configured_unix_sockets_without_full_network() {
        let workspace = TempDir::new().unwrap();
        let socket_root = platform_unix_socket_path("orca-browser.sock");
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
            allowed_unix_socket_roots: &[socket_root],
        });

        assert!(profile.contains("(allow system-socket (socket-domain AF_UNIX))"));
        assert!(
            profile.contains(
                r#"(allow network-bind (local unix-socket (subpath "/tmp/orca-browser.sock")))"#
            ),
            "profile should allow binding the configured unix socket: {profile}"
        );
        assert!(
            profile.contains(r#"(allow network-outbound (remote unix-socket (subpath "/tmp/orca-browser.sock")))"#),
            "profile should allow outbound traffic to the configured unix socket: {profile}"
        );
        assert!(!profile.contains("\n(allow network-outbound)\n"));
    }

    #[test]
    fn read_only_profile_allows_configured_unix_sockets_without_full_network() {
        let socket_root = platform_unix_socket_path("orca-browser.sock");
        let workspace = TempDir::new().unwrap();
        let profile = read_only_profile(ReadOnlyProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: &[],
            denied_roots: &[],
            network_access: false,
            allow_global_read: false,
            allowed_unix_socket_roots: &[socket_root],
        });

        assert!(profile.contains("(allow system-socket (socket-domain AF_UNIX))"));
        assert!(
            profile.contains(r#"(allow network-outbound (remote unix-socket (subpath "/tmp/orca-browser.sock")))"#),
            "profile should allow outbound traffic to the configured unix socket: {profile}"
        );
        assert!(!profile.contains("\n(allow network-outbound)\n"));
    }

    #[test]
    fn sandbox_blocks_writes_outside_workspace() {
        assert_seatbelt_available();

        let parent = crate::sandbox::sandbox_test_parent("seatbelt-outside-deny-");
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

    #[test]
    fn sandbox_allows_writes_to_additional_roots() {
        assert_seatbelt_available();

        let parent = crate::sandbox::sandbox_test_parent("seatbelt-additional-roots-");
        let workspace_path = parent.path().join("workspace");
        let extra = parent.path().join("extra");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&workspace_path).unwrap();
        std::fs::create_dir(&extra).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let extra_file = extra.join("allowed.txt");
        let outside_file = outside.join("blocked.txt");

        let output: Output = bash_command_with_additional_roots(
            &format!(
                "printf allowed > {} && printf blocked > {}",
                extra_file.display(),
                outside_file.display()
            ),
            &workspace_path,
            &[extra],
        )
        .output()
        .unwrap();

        assert!(!output.status.success());
        assert_eq!(std::fs::read_to_string(extra_file).unwrap(), "allowed");
        assert!(!outside_file.exists());
    }

    #[test]
    fn sandbox_allows_basic_shell_commands_and_null_device() {
        assert_seatbelt_available();

        let workspace = TempDir::new().unwrap();
        let output: Output = bash_command("printf ok >/dev/null && printf done", workspace.path())
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "status: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "done");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }
}
