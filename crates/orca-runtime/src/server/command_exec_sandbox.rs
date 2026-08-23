use std::collections::HashMap;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{DEFAULT_PERMISSION_PROFILE_GLOB_SCAN_MAX_DEPTH, RunConfig};
use walkdir::WalkDir;

use crate::protocol;
use crate::shell_session::ShellSandboxMode;
use orca_core::config::ActivePermissionProfile;

fn shell_sandbox_mode_from_command_policy(
    policy: &protocol::CommandSandboxPolicy,
) -> ShellSandboxMode {
    match policy {
        protocol::CommandSandboxPolicy::DangerFullAccess
        | protocol::CommandSandboxPolicy::ExternalSandbox { .. } => {
            ShellSandboxMode::DangerFullAccess
        }
        protocol::CommandSandboxPolicy::ReadOnly { network_access } => ShellSandboxMode::ReadOnly {
            network_access: *network_access,
            allow_global_read: true,
        },
        protocol::CommandSandboxPolicy::WorkspaceWrite {
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
            ..
        } => ShellSandboxMode::WorkspaceWrite {
            network_access: *network_access,
            exclude_tmpdir_env_var: *exclude_tmpdir_env_var,
            exclude_slash_tmp: *exclude_slash_tmp,
        },
        protocol::CommandSandboxPolicy::Default | protocol::CommandSandboxPolicy::Other => {
            ShellSandboxMode::WorkspaceWrite {
                network_access: true,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            }
        }
    }
}

/// Derive the bash sandbox for a working directory from the config's active
/// permission profile, exactly as `command/exec` does for the JSONL server.
/// Frontends that execute bash themselves (e.g. the TUI) use this so profile
/// modes and roots behave the same across entry points.
pub fn bash_sandbox_for_cwd(
    config: &RunConfig,
    cwd: &std::path::Path,
) -> Result<CommandExecSandbox, String> {
    let runtime_workspace_roots = config.runtime_workspace_roots.clone().unwrap_or_default();
    let profile = config.active_permission_profile.as_ref();
    let options = protocol::CommandExecOptions::default();
    command_exec_sandbox_mode(
        config,
        &options,
        profile,
        cwd,
        &runtime_workspace_roots,
        std::env::var_os("TMPDIR").map(PathBuf::from).as_deref(),
    )
}

pub(crate) fn command_exec_sandbox_mode(
    config: &RunConfig,
    options: &protocol::CommandExecOptions,
    thread_permission_profile: Option<&ActivePermissionProfile>,
    cwd: &std::path::Path,
    runtime_workspace_roots: &[PathBuf],
    tmpdir: Option<&std::path::Path>,
) -> Result<CommandExecSandbox, String> {
    let resolved = command_exec_sandbox_mode_inner(
        config,
        options,
        thread_permission_profile,
        cwd,
        runtime_workspace_roots,
        tmpdir,
    )?;
    if should_apply_folder_trust_gate(config.approval_mode, options, thread_permission_profile) {
        Ok(apply_folder_trust_gate(
            resolved,
            orca_core::config::folder_trust::is_trusted(cwd),
            cwd,
        ))
    } else {
        Ok(resolved)
    }
}

fn uses_default_folder_policy(
    options: &protocol::CommandExecOptions,
    thread_permission_profile: Option<&ActivePermissionProfile>,
) -> bool {
    options.permission_profile.is_none()
        && options.sandbox_policy == protocol::CommandSandboxPolicy::Default
        && thread_permission_profile.is_none()
}

fn should_apply_folder_trust_gate(
    approval_mode: ApprovalMode,
    options: &protocol::CommandExecOptions,
    thread_permission_profile: Option<&ActivePermissionProfile>,
) -> bool {
    approval_mode != ApprovalMode::FullAuto
        && uses_default_folder_policy(options, thread_permission_profile)
}

/// Unknown and explicitly untrusted folders get a strict default. Explicit
/// sandbox/profile selections remain authoritative and continue through the
/// existing approval and capability paths.
fn apply_folder_trust_gate(
    sandbox: CommandExecSandbox,
    folder_is_trusted: bool,
    cwd: &Path,
) -> CommandExecSandbox {
    if folder_is_trusted {
        return sandbox;
    }

    let mut additional_readable_roots = sandbox.additional_readable_roots;
    push_unique_path(&mut additional_readable_roots, cwd.to_path_buf());
    for root in orca_tools::sandbox::platform_default_read_roots() {
        push_unique_path(&mut additional_readable_roots, root);
    }

    CommandExecSandbox {
        mode: ShellSandboxMode::ReadOnly {
            network_access: false,
            allow_global_read: false,
        },
        additional_readable_roots,
        additional_writable_roots: Vec::new(),
        metadata_writable_roots: Vec::new(),
        denied_writable_roots: sandbox.denied_writable_roots,
        allowed_unix_socket_roots: Vec::new(),
        network_policy_domains: HashMap::new(),
    }
}

fn command_exec_sandbox_mode_inner(
    config: &RunConfig,
    options: &protocol::CommandExecOptions,
    thread_permission_profile: Option<&ActivePermissionProfile>,
    cwd: &std::path::Path,
    runtime_workspace_roots: &[PathBuf],
    tmpdir: Option<&std::path::Path>,
) -> Result<CommandExecSandbox, String> {
    if let Some(profile) = options.permission_profile.as_deref() {
        return shell_sandbox_mode_from_permission_profile(
            config,
            profile,
            cwd,
            runtime_workspace_roots,
            tmpdir,
        );
    }
    if options.sandbox_policy != protocol::CommandSandboxPolicy::Default {
        return Ok(CommandExecSandbox::new(
            shell_sandbox_mode_from_command_policy(&options.sandbox_policy),
        ));
    }
    if let Some(profile) = thread_permission_profile {
        let inherited_profile = profile.extends.as_deref().unwrap_or(&profile.id);
        return shell_sandbox_mode_from_permission_profile(
            config,
            inherited_profile,
            cwd,
            runtime_workspace_roots,
            tmpdir,
        );
    }
    Ok(CommandExecSandbox::new(default_sandbox_mode(
        config.approval_mode,
    )))
}

fn default_sandbox_mode(mode: ApprovalMode) -> ShellSandboxMode {
    match mode {
        ApprovalMode::Plan => ShellSandboxMode::ReadOnly {
            network_access: false,
            allow_global_read: true,
        },
        ApprovalMode::Suggest | ApprovalMode::AutoEdit => ShellSandboxMode::WorkspaceWrite {
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
        ApprovalMode::FullAuto => ShellSandboxMode::DangerFullAccess,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandExecSandbox {
    pub mode: ShellSandboxMode,
    pub additional_readable_roots: Vec<PathBuf>,
    pub additional_writable_roots: Vec<PathBuf>,
    pub metadata_writable_roots: Vec<PathBuf>,
    pub denied_writable_roots: Vec<PathBuf>,
    pub allowed_unix_socket_roots: Vec<PathBuf>,
    pub network_policy_domains: HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
}

impl CommandExecSandbox {
    fn new(mode: ShellSandboxMode) -> Self {
        Self {
            mode,
            additional_readable_roots: Vec::new(),
            additional_writable_roots: Vec::new(),
            metadata_writable_roots: Vec::new(),
            denied_writable_roots: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            network_policy_domains: HashMap::new(),
        }
    }
}

fn shell_sandbox_mode_from_permission_profile(
    config: &RunConfig,
    profile: &str,
    cwd: &std::path::Path,
    runtime_workspace_roots: &[PathBuf],
    tmpdir: Option<&std::path::Path>,
) -> Result<CommandExecSandbox, String> {
    let resolved =
        resolve_permission_profile(config, profile, cwd, runtime_workspace_roots, tmpdir)?;
    let mut mode = match resolved.builtin.as_deref() {
        Some("read-only") => ShellSandboxMode::ReadOnly {
            network_access: false,
            allow_global_read: false,
        },
        Some("workspace") => ShellSandboxMode::WorkspaceWrite {
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
        Some("danger-full-access") => ShellSandboxMode::DangerFullAccess,
        Some(_) | None => return Err(format!("unknown command/exec permissionProfile: {profile}")),
    };
    if let Some(network_access) = resolved.network_access {
        mode = match mode {
            ShellSandboxMode::WorkspaceWrite {
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                ..
            } => ShellSandboxMode::WorkspaceWrite {
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            },
            ShellSandboxMode::ReadOnly {
                allow_global_read, ..
            } => ShellSandboxMode::ReadOnly {
                network_access,
                allow_global_read,
            },
            ShellSandboxMode::DangerFullAccess => ShellSandboxMode::DangerFullAccess,
        };
    }
    let mut additional_readable_roots = resolved.additional_readable_roots;
    if matches!(
        mode,
        ShellSandboxMode::ReadOnly {
            allow_global_read: false,
            ..
        }
    ) {
        for root in orca_tools::sandbox::platform_default_read_roots() {
            push_unique_path(&mut additional_readable_roots, root);
        }
        if !resolved.additional_writable_roots.is_empty()
            || !resolved.denied_writable_roots.is_empty()
        {
            mode = match mode {
                ShellSandboxMode::ReadOnly { network_access, .. } => ShellSandboxMode::ReadOnly {
                    network_access,
                    allow_global_read: true,
                },
                other => other,
            };
        }
    }
    Ok(CommandExecSandbox {
        mode,
        additional_readable_roots,
        additional_writable_roots: resolved.additional_writable_roots,
        metadata_writable_roots: resolved.metadata_writable_roots,
        denied_writable_roots: resolved.denied_writable_roots,
        allowed_unix_socket_roots: resolved.allowed_unix_socket_roots,
        network_policy_domains: resolved.network_policy_domains,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedPermissionProfile {
    builtin: Option<String>,
    additional_readable_roots: Vec<PathBuf>,
    additional_writable_roots: Vec<PathBuf>,
    metadata_writable_roots: Vec<PathBuf>,
    denied_writable_roots: Vec<PathBuf>,
    allowed_unix_socket_roots: Vec<PathBuf>,
    network_access: Option<bool>,
    network_policy_domains: HashMap<String, orca_core::config::PermissionProfileNetworkAccess>,
}

fn resolve_permission_profile(
    config: &RunConfig,
    profile: &str,
    cwd: &std::path::Path,
    runtime_workspace_roots: &[PathBuf],
    tmpdir: Option<&std::path::Path>,
) -> Result<ResolvedPermissionProfile, String> {
    let mut current = normalize_permission_profile_name(profile).map(str::to_string);
    let mut seen = Vec::new();
    let mut additional_readable_roots = Vec::new();
    let mut additional_writable_roots = Vec::new();
    let mut metadata_writable_roots = Vec::new();
    let mut denied_writable_roots = Vec::new();
    let mut allowed_unix_socket_roots = Vec::new();
    let mut network_access = None;
    let mut network_policy_domains = HashMap::new();
    while let Some(name) = current {
        if is_builtin_permission_profile_name(&name) {
            return Ok(ResolvedPermissionProfile {
                builtin: Some(name),
                additional_readable_roots,
                additional_writable_roots,
                metadata_writable_roots,
                denied_writable_roots,
                allowed_unix_socket_roots,
                network_access,
                network_policy_domains,
            });
        }
        if seen.iter().any(|seen_name| seen_name == &name) {
            seen.push(name);
            return Err(format!(
                "command/exec permissionProfile cycle: {}",
                seen.join(" -> ")
            ));
        }
        seen.push(name.clone());
        let Some(profile) = config.permission_profiles.get(&name) else {
            return Err(format!("unknown command/exec permissionProfile: {name}"));
        };
        for (domain, access) in profile.network.domains.entries() {
            network_policy_domains
                .entry(domain.to_string())
                .or_insert(*access);
        }
        for (path, access) in profile.network.unix_sockets.entries() {
            if matches!(
                access,
                orca_core::config::PermissionProfileNetworkAccess::Allow
            ) {
                push_unique_path(&mut allowed_unix_socket_roots, path.to_path_buf());
            }
        }
        let glob_scan_max_depth = profile
            .filesystem
            .glob_scan_max_depth()
            .or_else(|| inherited_permission_profile_glob_scan_max_depth(config, profile, &seen))
            .unwrap_or(DEFAULT_PERMISSION_PROFILE_GLOB_SCAN_MAX_DEPTH);
        for (path, access) in profile.filesystem.entries() {
            if contains_glob_chars(path) {
                for pattern in materialize_permission_profile_glob_patterns(
                    &cwd.display().to_string(),
                    runtime_workspace_roots,
                    path,
                ) {
                    let roots =
                        expand_permission_profile_filesystem_glob(&pattern, glob_scan_max_depth)?;
                    for root in roots {
                        if access.allows_read() {
                            push_unique_path(&mut additional_readable_roots, root.clone());
                        }
                        if access.allows_write() {
                            push_writable_root(
                                &mut additional_writable_roots,
                                &mut metadata_writable_roots,
                                root.clone(),
                            );
                        }
                        if access.denies_write() {
                            push_unique_path(&mut denied_writable_roots, root);
                        }
                    }
                }
                continue;
            }
            let workspace_roots = materialize_workspace_roots_paths(
                &cwd.display().to_string(),
                runtime_workspace_roots,
                path,
            );
            let mut roots = Vec::new();
            for root in workspace_roots {
                roots.extend(materialize_profile_special_path(root, tmpdir)?);
            }
            for root in roots {
                if access.allows_read() && !additional_readable_roots.contains(&root) {
                    additional_readable_roots.push(root.clone());
                }
                if access.allows_write() {
                    push_writable_root(
                        &mut additional_writable_roots,
                        &mut metadata_writable_roots,
                        root.clone(),
                    );
                }
                if access.denies_write() && !denied_writable_roots.contains(&root) {
                    denied_writable_roots.push(root);
                }
            }
        }
        if network_access.is_none() {
            network_access = profile.network.enabled;
        }
        current = profile
            .extends
            .as_deref()
            .and_then(normalize_permission_profile_name)
            .map(str::to_string);
    }
    Ok(ResolvedPermissionProfile {
        builtin: None,
        additional_readable_roots,
        additional_writable_roots,
        metadata_writable_roots,
        denied_writable_roots,
        allowed_unix_socket_roots,
        network_access,
        network_policy_domains,
    })
}

fn inherited_permission_profile_glob_scan_max_depth(
    config: &RunConfig,
    profile: &orca_core::config::PermissionProfileConfig,
    seen: &[String],
) -> Option<usize> {
    let mut current = profile
        .extends
        .as_deref()
        .and_then(normalize_permission_profile_name)
        .map(str::to_string);
    let mut seen = seen.to_vec();
    while let Some(name) = current {
        if is_builtin_permission_profile_name(&name)
            || seen.iter().any(|seen_name| seen_name == &name)
        {
            return None;
        }
        seen.push(name.clone());
        let profile = config.permission_profiles.get(&name)?;
        if let Some(depth) = profile.filesystem.glob_scan_max_depth() {
            return Some(depth);
        }
        current = profile
            .extends
            .as_deref()
            .and_then(normalize_permission_profile_name)
            .map(str::to_string);
    }
    None
}

fn contains_glob_chars(path: &std::path::Path) -> bool {
    path.to_string_lossy()
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn materialize_permission_profile_glob_patterns(
    cwd: &str,
    runtime_workspace_roots: &[PathBuf],
    path: &Path,
) -> Vec<PathBuf> {
    materialize_workspace_roots_paths(cwd, runtime_workspace_roots, path)
}

fn expand_permission_profile_filesystem_glob(
    pattern: &Path,
    max_depth: usize,
) -> Result<Vec<PathBuf>, String> {
    let Some((search_root, relative_pattern)) = split_permission_profile_glob(pattern) else {
        return Err(format!(
            "command/exec permissionProfile filesystem glob is too broad to scan safely: {}",
            pattern.display()
        ));
    };
    if !search_root.is_dir() {
        return Ok(Vec::new());
    }
    let matcher = GlobBuilder::new(&relative_pattern)
        .literal_separator(true)
        .allow_unclosed_class(true)
        .build()
        .map_err(|error| {
            format!(
                "invalid command/exec permissionProfile filesystem glob {}: {error}",
                pattern.display()
            )
        })?
        .compile_matcher();
    let mut matches = Vec::new();
    for entry in WalkDir::new(&search_root)
        .follow_links(false)
        .max_depth(max_depth)
    {
        let entry = entry.map_err(|error| {
            format!(
                "failed to scan command/exec permissionProfile filesystem glob {}: {error}",
                pattern.display()
            )
        })?;
        let file_type = entry.file_type();
        if !(file_type.is_file() || file_type.is_dir() || file_type.is_symlink()) {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(&search_root).unwrap_or(path);
        if matcher.is_match(relative) {
            push_unique_path(&mut matches, path.to_path_buf());
            if let Ok(canonical) = path.canonicalize() {
                push_unique_path(&mut matches, canonical);
            }
        }
    }
    Ok(matches)
}

fn split_permission_profile_glob(pattern: &Path) -> Option<(PathBuf, String)> {
    let pattern = pattern.to_string_lossy();
    let first_glob_index = pattern
        .char_indices()
        .find_map(|(index, ch)| matches!(ch, '*' | '?' | '[' | ']').then_some(index))?;
    let static_prefix = &pattern[..first_glob_index];
    if static_prefix.is_empty() || static_prefix == "/" {
        return None;
    }
    let search_root_end =
        if static_prefix.ends_with(std::path::MAIN_SEPARATOR) || static_prefix.ends_with('/') {
            static_prefix.len().saturating_sub(1)
        } else {
            static_prefix
                .rfind(std::path::MAIN_SEPARATOR)
                .or_else(|| static_prefix.rfind('/'))?
        };
    if search_root_end == 0 {
        return None;
    }
    let search_root = PathBuf::from(&pattern[..search_root_end]);
    let relative_pattern = pattern[search_root_end + 1..].to_string();
    (!relative_pattern.is_empty()).then_some((search_root, relative_pattern))
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn push_writable_root(
    additional_writable_roots: &mut Vec<PathBuf>,
    metadata_writable_roots: &mut Vec<PathBuf>,
    root: PathBuf,
) {
    if orca_tools::sandbox::is_protected_metadata_root(&root) {
        if orca_tools::sandbox::is_safe_metadata_writable_root(&root) {
            push_unique_path(metadata_writable_roots, root);
        }
    } else {
        push_unique_path(additional_writable_roots, root);
    }
}

fn is_builtin_permission_profile_name(profile: &str) -> bool {
    matches!(profile, "read-only" | "workspace" | "danger-full-access")
}

fn normalize_permission_profile_name(profile: &str) -> Option<&str> {
    profile
        .strip_prefix(':')
        .or(Some(profile))
        .filter(|profile| !profile.is_empty())
}

pub(super) fn materialize_workspace_roots_paths(
    cwd: &str,
    runtime_workspace_roots: &[PathBuf],
    path: &std::path::Path,
) -> Vec<PathBuf> {
    let Some(rest) = path
        .to_str()
        .and_then(|path| path.strip_prefix(":workspace_roots"))
    else {
        return vec![path.to_path_buf()];
    };
    let roots = if runtime_workspace_roots.is_empty() {
        vec![PathBuf::from(cwd)]
    } else {
        runtime_workspace_roots.to_vec()
    };
    let subpath = rest
        .trim_start_matches(std::path::MAIN_SEPARATOR)
        .trim_start_matches('/');
    roots
        .into_iter()
        .map(|root| {
            if subpath.is_empty() {
                return root;
            }
            let mut materialized = root;
            for component in PathBuf::from(subpath).components() {
                if let std::path::Component::Normal(part) = component {
                    materialized.push(part);
                }
            }
            materialized
        })
        .collect()
}

fn materialize_profile_special_path(
    path: PathBuf,
    tmpdir: Option<&std::path::Path>,
) -> Result<Vec<PathBuf>, String> {
    match path.to_str() {
        Some(":root") => Ok(vec![PathBuf::from("/")]),
        Some(":slash_tmp") => Ok(vec![PathBuf::from("/tmp")]),
        Some(":tmpdir") => Ok(tmpdir
            .map(|path| vec![path.to_path_buf()])
            .unwrap_or_default()),
        Some(":minimal") => Ok(orca_tools::sandbox::platform_default_read_roots()),
        _ => Ok(vec![path]),
    }
}

#[cfg(test)]
mod folder_trust_tests {
    use super::*;

    #[test]
    fn default_sandbox_matches_approval_mode() {
        assert_eq!(
            default_sandbox_mode(ApprovalMode::Plan),
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: true,
            }
        );
        assert_eq!(
            default_sandbox_mode(ApprovalMode::AutoEdit),
            ShellSandboxMode::WorkspaceWrite {
                network_access: true,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            }
        );
        assert_eq!(
            default_sandbox_mode(ApprovalMode::FullAuto),
            ShellSandboxMode::DangerFullAccess
        );
    }

    #[test]
    fn full_auto_does_not_get_folder_trust_downgrade() {
        assert!(!should_apply_folder_trust_gate(
            ApprovalMode::FullAuto,
            &protocol::CommandExecOptions::default(),
            None,
        ));
        assert!(should_apply_folder_trust_gate(
            ApprovalMode::AutoEdit,
            &protocol::CommandExecOptions::default(),
            None,
        ));
    }

    #[test]
    fn untrusted_default_is_strict_read_only() {
        let sandbox = CommandExecSandbox {
            mode: ShellSandboxMode::WorkspaceWrite {
                network_access: true,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            additional_readable_roots: vec![PathBuf::from("/inputs")],
            additional_writable_roots: vec![PathBuf::from("/outputs")],
            metadata_writable_roots: vec![PathBuf::from("/workspace/.git")],
            denied_writable_roots: vec![PathBuf::from("/secret")],
            allowed_unix_socket_roots: vec![PathBuf::from("/run/service.sock")],
            network_policy_domains: HashMap::from([(
                "api.example.com".to_string(),
                orca_core::config::PermissionProfileNetworkAccess::Allow,
            )]),
        };

        let cwd = PathBuf::from("/workspace");
        let gated = apply_folder_trust_gate(sandbox, false, &cwd);

        assert_eq!(
            gated.mode,
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: false,
            }
        );
        assert!(
            gated
                .additional_readable_roots
                .contains(&PathBuf::from("/inputs"))
        );
        assert!(gated.additional_readable_roots.contains(&cwd));
        assert!(gated.additional_writable_roots.is_empty());
        assert!(gated.metadata_writable_roots.is_empty());
        assert_eq!(gated.denied_writable_roots, vec![PathBuf::from("/secret")]);
        assert!(gated.allowed_unix_socket_roots.is_empty());
        assert!(gated.network_policy_domains.is_empty());
    }

    #[test]
    fn explicit_policy_and_profile_skip_the_folder_default() {
        let explicit_policy = protocol::CommandExecOptions {
            sandbox_policy: protocol::CommandSandboxPolicy::WorkspaceWrite {
                writable_roots: Vec::new(),
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            ..Default::default()
        };
        assert!(!uses_default_folder_policy(&explicit_policy, None));

        let explicit_profile = protocol::CommandExecOptions {
            permission_profile: Some("locked-down".to_string()),
            ..Default::default()
        };
        assert!(!uses_default_folder_policy(&explicit_profile, None));
        assert!(uses_default_folder_policy(
            &protocol::CommandExecOptions::default(),
            None
        ));
    }
}
