//! Pure construction of a bubblewrap (`bwrap`) argument vector from a resolved
//! sandbox policy.
//!
//! This module is compiled on every platform so the argv-building logic can be
//! unit tested off Linux. The actual process launch and the in-process
//! Landlock/seccomp fallback live in the Linux-only `sandbox::linux` module.
//!
//! # Filesystem model
//!
//! bubblewrap builds a fresh mount namespace by applying bind mounts in order.
//! We rely on that ordering:
//!
//! - Global-read policies start from `--ro-bind / /`, which makes the entire
//!   host tree visible but read-only. Writable roots are then re-bound with
//!   `--bind`, which remounts them read-write on top of the read-only base.
//! - Restricted-read policies start from `--tmpfs /`, an empty root, and then
//!   bind only the explicitly readable roots (plus platform defaults) with
//!   `--ro-bind`, so nothing outside the allow list is even visible.
//! - Denied roots are masked last with `--tmpfs`, hiding their contents and
//!   preventing writes regardless of any broader grant above them.
//!
//! # Network model
//!
//! When network access is disabled the sandbox gets `--unshare-net`, an empty
//! network namespace with no route to the outside world. Pathname Unix sockets
//! that were bound into the filesystem keep working because they are filesystem
//! objects rather than network-namespaced endpoints, which preserves the
//! configured Unix-socket allow list without granting TCP/IP access.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// How the sandboxed filesystem view is constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxReadScope {
    /// The entire host filesystem is readable (`--ro-bind / /`).
    Global,
    /// Only explicitly listed roots are readable (`--tmpfs /` base).
    Restricted,
}

/// A resolved Linux sandbox policy shared by the bwrap and Landlock backends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinuxSandboxPolicy {
    pub cwd: PathBuf,
    pub read_scope: LinuxReadScope,
    /// Roots granted read-only access (only consulted for `Restricted` scope).
    pub readable_roots: Vec<PathBuf>,
    /// Filesystem roots containing explicitly allowed pathname Unix sockets.
    pub allowed_unix_socket_roots: Vec<PathBuf>,
    /// Roots granted read-write access.
    pub writable_roots: Vec<PathBuf>,
    /// Ordinary writable roots whose existing protected metadata descendants
    /// must be discovered and re-bound read-only. This excludes implicit temp
    /// roots, which would otherwise require an unbounded scan on every launch.
    pub metadata_protection_roots: Vec<PathBuf>,
    /// Protected metadata roots granted read-write access through the dedicated
    /// escalation channel. Keeping provenance separate prevents an ordinary
    /// writable-root grant from being mistaken for metadata authority.
    pub metadata_writable_roots: Vec<PathBuf>,
    /// Roots that must stay read-only even when they fall under a writable
    /// root (re-bound with `--ro-bind` after the writable binds). Used to
    /// protect workspace metadata such as `.git` while preserving reads.
    pub read_only_roots: Vec<PathBuf>,
    /// Roots hidden entirely (masked with an empty tmpfs), applied last so they
    /// win over any broader grant.
    pub denied_roots: Vec<PathBuf>,
    /// When false, the command runs in an isolated (empty) network namespace.
    pub network_access: bool,
}

pub(crate) fn effective_read_only_roots(policy: &LinuxSandboxPolicy) -> Vec<PathBuf> {
    let mut roots = policy.read_only_roots.clone();
    for writable_root in &policy.metadata_protection_roots {
        if let Some(metadata) = writable_root
            .ancestors()
            .find(|path| crate::sandbox::is_protected_metadata_root(path))
        {
            let canonical = metadata
                .canonicalize()
                .unwrap_or_else(|_| metadata.to_path_buf());
            if !is_explicit_metadata_writable_root(policy, metadata, &canonical) {
                push_unique(&mut roots, canonical);
            }
        }

        let mut entries = WalkDir::new(writable_root).follow_links(false).into_iter();
        while let Some(entry) = entries.next() {
            match entry {
                Ok(entry) if crate::sandbox::is_protected_metadata_root(entry.path()) => {
                    let canonical = entry
                        .path()
                        .canonicalize()
                        .unwrap_or_else(|_| entry.path().to_path_buf());
                    if !is_explicit_metadata_writable_root(policy, entry.path(), &canonical) {
                        push_unique(&mut roots, canonical);
                    }
                    if entry.file_type().is_dir() {
                        entries.skip_current_dir();
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    // A subtree that cannot be inspected must not silently
                    // inherit a broader writable grant.
                    let unreadable = error
                        .path()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| writable_root.clone());
                    let canonical = unreadable
                        .canonicalize()
                        .unwrap_or_else(|_| unreadable.clone());
                    push_unique(&mut roots, canonical);
                }
            }
        }
    }
    roots
}

fn is_explicit_metadata_writable_root(
    policy: &LinuxSandboxPolicy,
    metadata: &Path,
    canonical_metadata: &Path,
) -> bool {
    crate::sandbox::is_safe_metadata_writable_root(metadata)
        && policy.metadata_writable_roots.iter().any(|root| {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            canonical == canonical_metadata
        })
}

/// Build the full `bwrap` argument vector (excluding the leading `bwrap`
/// program name) for the given policy and shell command.
///
/// The returned argv ends with `sh -c <command>`; callers prepend the resolved
/// `bwrap` binary path.
pub fn build_bwrap_argv(policy: &LinuxSandboxPolicy, command: &str) -> Vec<String> {
    // Process isolation. `--die-with-parent` guarantees the sandbox tears down
    // if the agent process exits; the unshares drop the child into private
    // user/pid/ipc/uts namespaces. Network is unshared separately below.
    let mut argv = vec![
        "--die-with-parent".to_string(),
        "--unshare-user".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
        "--unshare-uts".to_string(),
        "--new-session".to_string(),
        "--cap-drop".to_string(),
        "ALL".to_string(),
    ];

    if !policy.network_access {
        argv.push("--unshare-net".to_string());
    }

    // Base filesystem view.
    match policy.read_scope {
        LinuxReadScope::Global => {
            argv.push("--ro-bind".to_string());
            argv.push("/".to_string());
            argv.push("/".to_string());
        }
        LinuxReadScope::Restricted => {
            argv.push("--tmpfs".to_string());
            argv.push("/".to_string());
            // A tmpfs root does not contain the host's parent directories.
            // Create destination parents before binding absolute workspace
            // roots such as `/tmp/.tmp123/workspace`; otherwise bwrap accepts
            // the bind list but the final `--chdir` fails with ENOENT.
            for parent in restricted_mount_parent_dirs(policy) {
                argv.push("--dir".to_string());
                argv.push(path_arg(&parent));
            }
            for root in restricted_read_roots(policy) {
                push_ro_bind_if_exists(&mut argv, &root);
            }
        }
    }

    // A real /dev and /proc are required by most shells and tools. These are
    // applied after the base bind so they are not shadowed by `--ro-bind / /`.
    argv.push("--dev".to_string());
    argv.push("/dev".to_string());
    argv.push("--proc".to_string());
    argv.push("/proc".to_string());

    // Re-bind writable roots read-write on top of the read-only base.
    for root in &policy.writable_roots {
        if root.exists() {
            argv.push("--bind".to_string());
            argv.push(path_arg(root));
            argv.push(path_arg(root));
        }
    }
    for root in &policy.metadata_writable_roots {
        if root.exists() {
            argv.push("--bind".to_string());
            argv.push(path_arg(root));
            argv.push(path_arg(root));
        }
    }

    // Re-assert read-only roots after writable binds so protected metadata
    // (e.g. `.git`) stays readable but not writable even under a writable root.
    for root in effective_read_only_roots(policy) {
        if root.exists() {
            argv.push("--ro-bind".to_string());
            argv.push(path_arg(&root));
            argv.push(path_arg(&root));
        }
    }

    // Mask denied roots last so they override any broader grant above.
    for root in &policy.denied_roots {
        if root.is_dir() {
            argv.push("--tmpfs".to_string());
            argv.push(path_arg(root));
        } else if root.exists() {
            argv.push("--ro-bind".to_string());
            argv.push("/dev/null".to_string());
            argv.push(path_arg(root));
        }
    }

    argv.push("--chdir".to_string());
    argv.push(path_arg(&policy.cwd));

    argv.push("--".to_string());
    argv.push("sh".to_string());
    argv.push("-c".to_string());
    argv.push(command.to_string());

    argv
}

/// The readable roots for a restricted-read policy: the workspace cwd, any
/// explicitly readable roots, all writable roots (a writable root must also be
/// readable), plus platform defaults needed by the shell runtime.
fn restricted_read_roots(policy: &LinuxSandboxPolicy) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    push_unique(&mut roots, policy.cwd.clone());
    for root in &policy.readable_roots {
        push_unique(&mut roots, root.clone());
    }
    for root in &policy.allowed_unix_socket_roots {
        push_unique(&mut roots, root.clone());
    }
    for root in &policy.writable_roots {
        push_unique(&mut roots, root.clone());
    }
    for root in &policy.metadata_writable_roots {
        push_unique(&mut roots, root.clone());
    }
    for root in crate::sandbox::linux_platform_default_read_roots() {
        push_unique(&mut roots, root);
    }
    roots
}

fn restricted_mount_parent_dirs(policy: &LinuxSandboxPolicy) -> Vec<PathBuf> {
    let mut parents = Vec::new();
    let mut add_parents = |root: &Path| {
        for parent in root.ancestors().skip(1) {
            if parent == Path::new("/") {
                break;
            }
            push_unique(&mut parents, parent.to_path_buf());
        }
    };

    add_parents(&policy.cwd);
    for root in policy
        .readable_roots
        .iter()
        .chain(&policy.allowed_unix_socket_roots)
        .chain(&policy.writable_roots)
        .chain(&policy.metadata_writable_roots)
        .chain(&policy.read_only_roots)
        .chain(&policy.denied_roots)
    {
        add_parents(root);
    }

    // bwrap requires parents to exist in the order they are created.
    parents.sort_by_key(|path| path.components().count());
    parents
}

fn push_ro_bind_if_exists(argv: &mut Vec<String>, root: &Path) {
    if root.exists() {
        argv.push("--ro-bind".to_string());
        argv.push(path_arg(root));
        argv.push(path_arg(root));
    }
}

fn push_unique(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots.contains(&root) {
        roots.push(root);
    }
}

fn path_arg(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_slash_tmp_path() -> PathBuf {
        PathBuf::from("/tmp")
    }

    fn base_policy() -> LinuxSandboxPolicy {
        LinuxSandboxPolicy {
            cwd: PathBuf::from("/work"),
            read_scope: LinuxReadScope::Global,
            readable_roots: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            writable_roots: Vec::new(),
            metadata_protection_roots: Vec::new(),
            metadata_writable_roots: Vec::new(),
            read_only_roots: Vec::new(),
            denied_roots: Vec::new(),
            network_access: true,
        }
    }

    fn argv_string(argv: &[String]) -> String {
        argv.join(" ")
    }

    #[test]
    fn global_scope_binds_root_read_only() {
        let argv = build_bwrap_argv(&base_policy(), "echo hi");
        let joined = argv_string(&argv);
        assert!(joined.contains("--ro-bind / /"));
        assert!(joined.contains("--dev /dev"));
        assert!(joined.contains("--proc /proc"));
        assert!(joined.ends_with("-- sh -c echo hi"));
    }

    #[test]
    fn disabled_network_unshares_net() {
        let mut policy = base_policy();
        policy.network_access = false;
        let argv = build_bwrap_argv(&policy, "true");
        assert!(argv.iter().any(|arg| arg == "--unshare-net"));
    }

    #[test]
    fn enabled_network_does_not_unshare_net() {
        let argv = build_bwrap_argv(&base_policy(), "true");
        assert!(!argv.iter().any(|arg| arg == "--unshare-net"));
    }

    #[test]
    fn chdir_targets_the_workspace_cwd() {
        let argv = build_bwrap_argv(&base_policy(), "true");
        let joined = argv_string(&argv);
        assert!(joined.contains("--chdir /work"));
    }

    #[test]
    fn restricted_scope_uses_tmpfs_root() {
        let mut policy = base_policy();
        policy.read_scope = LinuxReadScope::Restricted;
        let argv = build_bwrap_argv(&policy, "true");
        let joined = argv_string(&argv);
        assert!(joined.contains("--tmpfs /"));
        assert!(!joined.contains("--ro-bind / /"));
    }

    #[test]
    fn restricted_policy_creates_absolute_mount_parents_before_chdir() {
        let mut policy = base_policy();
        policy.read_scope = LinuxReadScope::Restricted;
        policy.cwd = platform_slash_tmp_path().join(".tmp123/workspace");

        let argv = build_bwrap_argv(&policy, "true");
        let joined = argv_string(&argv);
        assert!(joined.contains("--dir /tmp"));
        assert!(joined.contains("--dir /tmp/.tmp123"));
        assert!(joined.contains("--chdir /tmp/.tmp123/workspace"));
    }

    #[test]
    fn restricted_scope_binds_allowed_unix_socket_roots() {
        let socket_root = tempfile::tempdir().unwrap();
        let mut policy = base_policy();
        policy.read_scope = LinuxReadScope::Restricted;
        policy.allowed_unix_socket_roots = vec![socket_root.path().to_path_buf()];

        let joined = argv_string(&build_bwrap_argv(&policy, "true"));
        let root = socket_root.path().display();
        assert!(joined.contains(&format!("--ro-bind {root} {root}")));
    }

    #[test]
    fn read_only_roots_are_rebound_after_writable_roots() {
        // Use real existing dirs so the exists() guards pass.
        let mut policy = base_policy();
        policy.cwd = PathBuf::from("/");
        policy.writable_roots = vec![platform_slash_tmp_path()];
        policy.read_only_roots = vec![PathBuf::from("/etc")];
        let argv = build_bwrap_argv(&policy, "true");
        let joined = argv_string(&argv);
        if joined.contains("--bind /tmp /tmp") && joined.contains("--ro-bind /etc /etc") {
            let write_pos = joined.find("--bind /tmp /tmp").unwrap();
            let ro_pos = joined.find("--ro-bind /etc /etc").unwrap();
            assert!(
                ro_pos > write_pos,
                "read-only rebind must come after writable bind: {joined}"
            );
        }
    }

    #[test]
    fn ordinary_external_writable_root_rebinds_its_metadata_read_only() {
        let external = tempfile::tempdir().unwrap();
        let metadata = external.path().join(".git");
        std::fs::create_dir(&metadata).unwrap();
        let mut policy = base_policy();
        policy.writable_roots = vec![external.path().to_path_buf()];
        policy.metadata_protection_roots = policy.writable_roots.clone();

        let argv = build_bwrap_argv(&policy, "true");
        let metadata = path_arg(&metadata.canonicalize().unwrap());
        let read_only_position = argv
            .windows(3)
            .position(|args| args == ["--ro-bind", metadata.as_str(), metadata.as_str()]);

        assert!(read_only_position.is_some());
    }

    #[test]
    fn ordinary_exact_metadata_writable_root_is_rebound_read_only() {
        let external = tempfile::tempdir().unwrap();
        let metadata = external.path().join(".git");
        std::fs::create_dir(&metadata).unwrap();
        let mut policy = base_policy();
        policy.writable_roots = vec![metadata.clone()];
        policy.metadata_protection_roots = policy.writable_roots.clone();

        let argv = build_bwrap_argv(&policy, "true");
        let metadata = path_arg(&metadata.canonicalize().unwrap());
        let read_only_position = argv
            .windows(3)
            .position(|args| args == ["--ro-bind", metadata.as_str(), metadata.as_str()]);

        assert!(read_only_position.is_some());
    }

    #[test]
    fn ordinary_writable_root_rebinds_nested_metadata_read_only() {
        let external = tempfile::tempdir().unwrap();
        let metadata = external.path().join("project").join(".git");
        std::fs::create_dir_all(&metadata).unwrap();
        let mut policy = base_policy();
        policy.writable_roots = vec![external.path().to_path_buf()];
        policy.metadata_protection_roots = policy.writable_roots.clone();

        let argv = build_bwrap_argv(&policy, "true");
        let metadata = path_arg(&metadata.canonicalize().unwrap());
        let read_only_position = argv
            .windows(3)
            .position(|args| args == ["--ro-bind", metadata.as_str(), metadata.as_str()]);

        assert!(read_only_position.is_some());
    }

    #[test]
    fn explicit_metadata_writable_root_is_not_rebound_read_only() {
        let external = tempfile::tempdir().unwrap();
        let metadata = external.path().join(".git");
        std::fs::create_dir(&metadata).unwrap();
        let mut policy = base_policy();
        policy.writable_roots = vec![external.path().to_path_buf()];
        policy.metadata_protection_roots = policy.writable_roots.clone();
        policy.metadata_writable_roots = vec![metadata.clone()];

        let argv = build_bwrap_argv(&policy, "true");
        let metadata = path_arg(&metadata);
        let read_only_position = argv
            .windows(3)
            .position(|args| args == ["--ro-bind", metadata.as_str(), metadata.as_str()]);

        assert!(read_only_position.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_metadata_does_not_treat_its_writable_target_as_an_explicit_grant() {
        let external = tempfile::tempdir().unwrap();
        let metadata = external.path().join(".git");
        std::os::unix::fs::symlink(external.path(), &metadata).unwrap();
        let mut policy = base_policy();
        let target = external.path().canonicalize().unwrap();
        policy.writable_roots = vec![target.clone()];
        policy.metadata_protection_roots = policy.writable_roots.clone();

        let argv = build_bwrap_argv(&policy, "true");
        let target = path_arg(&target);
        let read_only_position = argv
            .windows(3)
            .position(|args| args == ["--ro-bind", target.as_str(), target.as_str()]);

        assert!(read_only_position.is_some());
    }

    #[test]
    fn command_is_passed_verbatim_after_separator() {
        let argv = build_bwrap_argv(&base_policy(), "printf 'a b' && ls");
        let separator = argv.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(argv[separator + 1], "sh");
        assert_eq!(argv[separator + 2], "-c");
        assert_eq!(argv[separator + 3], "printf 'a b' && ls");
    }

    #[test]
    fn denied_files_are_masked_with_a_file_bind() {
        let temp = tempfile::tempdir().unwrap();
        let denied = temp.path().join("secret.txt");
        std::fs::write(&denied, "secret").unwrap();
        let mut policy = base_policy();
        policy.denied_roots.push(denied.clone());

        let argv = build_bwrap_argv(&policy, "true");
        let joined = argv_string(&argv);

        assert!(
            joined.contains(&format!("--ro-bind /dev/null {}", denied.display())),
            "denied file must use a file bind: {joined}"
        );
        assert!(
            !joined.contains(&format!("--tmpfs {}", denied.display())),
            "bubblewrap tmpfs mounts cannot target files: {joined}"
        );
    }
}
