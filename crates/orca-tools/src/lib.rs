#![deny(deprecated)]

use std::path::{Path, PathBuf};

use orca_core::approval_types::ActionKind;
use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{ToolName, ToolOutputTruncation, ToolRequest, ToolResult};
use orca_mcp::{McpElicitationHandler, McpRegistry};

pub mod apply_patch;
pub mod bash;
pub mod deepseek_web;
pub mod edit;
pub mod external;
pub mod file_admission;
pub mod git;
pub mod glob;
pub mod grep;
pub mod list_files;
mod ocr_disk_cache;
pub mod process;
pub mod read_document;
pub mod read_file;
pub mod registry;
pub mod report_blocker;
pub mod sandbox;
pub mod schema;
pub mod skills;
mod spill;
#[cfg(test)]
mod test_http;
pub mod update_goal;
pub mod update_plan;
pub mod web_fetch;
pub mod web_search;
mod workspace_ignore;
mod workspace_read;
pub mod write_file;

pub use workspace_read::{
    open_relative_file, open_root as open_workspace_root, open_workspace_file,
};

pub use registry::{Tool, ToolContext, ToolRegistry, validate_tool_request};

pub fn execute_with_mcp(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
) -> ToolResult {
    execute_with_mcp_and_external(request, cwd, mcp_registry, &[], 120)
}

pub fn execute_with_mcp_and_external(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    shell_timeout_secs: u64,
) -> ToolResult {
    execute_with_mcp_external_and_policy(
        request,
        cwd,
        mcp_registry,
        external_tools,
        ToolOutputTruncation::default(),
        shell_timeout_secs,
    )
}

pub fn execute_with_mcp_external_and_policy(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
) -> ToolResult {
    execute_with_mcp_external_policy_or_cancel(
        request,
        cwd,
        mcp_registry,
        external_tools,
        output_truncation,
        shell_timeout_secs,
        || false,
    )
}

pub fn execute_with_mcp_external_policy_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_with_mcp_external_roots_policy_or_cancel(
        request,
        cwd,
        &[],
        mcp_registry,
        external_tools,
        output_truncation,
        shell_timeout_secs,
        should_cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn execute_with_mcp_external_roots_policy_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[PathBuf],
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
        request,
        cwd,
        additional_roots,
        mcp_registry,
        external_tools,
        output_truncation,
        shell_timeout_secs,
        None,
        should_cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[PathBuf],
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    mcp_elicitation_handler: Option<&dyn McpElicitationHandler>,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_with_mcp_external_roots_policy_or_cancel_and_elicitation_with_profile(
        request,
        cwd,
        additional_roots,
        mcp_registry,
        external_tools,
        output_truncation,
        shell_timeout_secs,
        mcp_elicitation_handler,
        orca_core::capability::ExecutionProfile::Workspace,
        None,
        should_cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn execute_with_mcp_external_roots_policy_or_cancel_and_elicitation_with_profile(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[PathBuf],
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    mcp_elicitation_handler: Option<&dyn McpElicitationHandler>,
    execution_profile: orca_core::capability::ExecutionProfile,
    deepseek_web: Option<deepseek_web::DeepSeekWebAccess>,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    // 0 is the default and sets no administrator cap, so an external tool runs
    // until it exits or is cancelled, like a shell command.
    let shell_timeout =
        (shell_timeout_secs > 0).then(|| std::time::Duration::from_secs(shell_timeout_secs));
    let ctx = ToolContext::new(cwd)
        .with_output_truncation(output_truncation)
        .with_shell_timeout(shell_timeout)
        .with_additional_working_directories(additional_roots.iter().cloned())
        .with_execution_profile(execution_profile)
        .with_deepseek_web(deepseek_web)
        .with_cancel(&should_cancel);
    let ctx = match mcp_elicitation_handler {
        Some(handler) => ctx.with_mcp_elicitation_handler(handler),
        None => ctx,
    };
    execute_in_context(request, mcp_registry, external_tools, ctx)
}

pub fn execute_in_context<'a>(
    request: &ToolRequest,
    mcp_registry: &'a McpRegistry,
    external_tools: &[ExternalToolConfig],
    ctx: ToolContext<'a>,
) -> ToolResult {
    let uses_mcp = tool_uses_mcp_registry(&request.name);
    if !uses_mcp && external_tools.is_empty() {
        return registry::default_tool_registry().execute(request, &ctx);
    }
    let mcp = uses_mcp.then_some(mcp_registry);
    let ctx = if uses_mcp {
        ctx.with_mcp(mcp_registry)
    } else {
        ctx
    };
    registry::tool_registry_with_mcp_and_external(mcp, external_tools).execute(request, &ctx)
}

fn tool_uses_mcp_registry(name: &ToolName) -> bool {
    matches!(
        name,
        ToolName::Mcp(_)
            | ToolName::ListMcpResources
            | ToolName::ListMcpResourceTemplates
            | ToolName::ReadMcpResource
    )
}

pub fn validate_with_mcp_and_external(
    request: &ToolRequest,
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> Result<(), String> {
    let reg = registry::tool_registry_with_mcp_and_external(mcp_registry, external_tools);
    registry::validate_tool_request(&reg, request)
}

pub fn tool_is_available_readonly_concurrent(request: &ToolRequest) -> bool {
    if runtime_owned_tool_requires_controller(&request.name) {
        return false;
    }
    let reg = registry::default_tool_registry();
    reg.resolve(request.name.as_str())
        .map(|resolved| {
            resolved.spec.capabilities.is_read_only()
                && resolved.spec.concurrent_safe
                && resolved.tool.is_concurrent_safe(request)
        })
        .unwrap_or(false)
}

/// Tools only the runtime can execute.
///
/// These read or steer tasks the runtime owns; the registry entry is the
/// fail-closed fallback for a caller that bypassed the runtime, not an
/// implementation. Sending them through the concurrent read-only batch would
/// dispatch them straight to that fallback, so they are excluded here and
/// reach their runtime handler through the normal tool path instead.
fn runtime_owned_tool_requires_controller(name: &ToolName) -> bool {
    matches!(
        name,
        ToolName::TaskList
            | ToolName::TaskReadOutput
            | ToolName::TaskWait
            | ToolName::TaskStop
            | ToolName::SubagentMessage
            | ToolName::WorkflowReadMessages
            | ToolName::WorkflowListTasks
    )
}

pub fn canonical_action_kind(request: &ToolRequest) -> ActionKind {
    canonical_action_kind_with_mcp_and_external(request, None, &[])
}

pub fn canonical_capabilities(
    request: &ToolRequest,
) -> Option<orca_core::tool_types::CapabilitySet> {
    registry::default_tool_registry()
        .resolve(request.name.as_str())
        .map(|resolved| resolved.spec.capabilities.clone())
}

pub fn canonical_action_kind_with_mcp_and_external(
    request: &ToolRequest,
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> ActionKind {
    let reg = registry::tool_registry_with_mcp_and_external(mcp_registry, external_tools);
    reg.resolve(request.name.as_str())
        .map(|resolved| resolved.spec.capabilities.action_kind())
        .unwrap_or(request.action)
}

pub fn should_run_readonly_batch(max_read_parallel: usize, tool_request: &ToolRequest) -> bool {
    tool_is_available_readonly_concurrent(tool_request) && max_read_parallel > 1
}

pub fn collect_readonly_batch(
    max_read_parallel: usize,
    tool_requests: &[ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + max_read_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && tool_is_available_readonly_concurrent(&tool_requests[end]) {
        end += 1;
    }
    end
}

pub fn resolve_workspace_path(cwd: &Path, target: Option<&str>) -> Result<PathBuf, String> {
    resolve_workspace_read_path(cwd, target).map(|resolved| resolved.path)
}

pub(crate) struct ResolvedWorkspacePath {
    pub path: PathBuf,
    pub root: PathBuf,
    pub canonical: PathBuf,
}

pub(crate) fn resolve_workspace_read_path(
    cwd: &Path,
    target: Option<&str>,
) -> Result<ResolvedWorkspacePath, String> {
    resolve_workspace_path_inner(cwd, target, false)
}

/// Resolve a write target using the lexical path shown in edit and patch previews.
pub fn resolve_workspace_write_path(cwd: &Path, target: &str) -> Result<PathBuf, String> {
    resolve_workspace_path_inner(cwd, Some(target), true).map(|resolved| resolved.path)
}

fn resolve_workspace_path_inner(
    cwd: &Path,
    target: Option<&str>,
    allow_missing_parents: bool,
) -> Result<ResolvedWorkspacePath, String> {
    use std::path::Component;

    let target = target.filter(|target| !target.is_empty()).unwrap_or(".");
    #[cfg(windows)]
    reject_drive_relative(target)?;
    #[cfg(windows)]
    let candidate = PathBuf::from(target.replace('/', "\\"));
    #[cfg(not(windows))]
    let candidate = PathBuf::from(target);
    let root = if allow_missing_parents {
        cwd.canonicalize()
            .map_err(|error| format!("cannot resolve cwd: {error}"))?
    } else {
        open_workspace_root(cwd).map_err(|error| error.to_string())?;
        cwd.to_path_buf()
    };
    let joined = join_requested_path(cwd, candidate);

    // Check the lexical target before resolving any components on disk.
    let mut normalized = PathBuf::new();
    let mut has_parent_component = false;
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
                has_parent_component = true;
            }
            Component::CurDir => {}
            _ => normalized.push(component),
        }
    }
    #[cfg(windows)]
    if cwd.is_absolute() {
        normalized = windows_path_in_workspace(cwd, &normalized, target)?;
    }
    #[cfg(target_os = "macos")]
    if !allow_missing_parents && !normalized.starts_with(cwd) {
        normalized = macos_path_in_workspace(cwd, &normalized)
            .ok_or_else(|| format!("path escapes workspace: {target}"))?;
    }
    if !normalized.starts_with(cwd) {
        return Err(format!("path escapes workspace: {target}"));
    }

    let resolve_error = |error| format!("cannot resolve path {target}: {error}");
    let joined = if allow_missing_parents {
        normalized.clone()
    } else if has_parent_component {
        resolve_parent_components(&joined).map_err(resolve_error)?
    } else if Path::new(target).is_absolute() {
        normalized.clone()
    } else {
        joined
    };
    let canonical = match joined.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut parent = joined.parent().ok_or_else(|| resolve_error(error))?;
            let canonical_parent = loop {
                match parent.canonicalize() {
                    Ok(path) if path.is_dir() => break path,
                    Ok(_) => {
                        return Err(format!(
                            "cannot resolve path {target}: a parent is not a directory"
                        ));
                    }
                    Err(error)
                        if allow_missing_parents
                            && error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        parent = parent.parent().ok_or_else(|| resolve_error(error))?;
                    }
                    Err(error) => return Err(resolve_error(error)),
                }
            };
            canonical_parent.join(joined.strip_prefix(parent).expect("checked path parent"))
        }
        Err(error) => return Err(resolve_error(error)),
    };
    #[cfg(windows)]
    let canonical = windows_path_in_workspace(&root, &canonical, target)
        .map_err(|_| format!("path escapes workspace via symlink: {target}"))?;
    let relative = canonical
        .strip_prefix(&root)
        .map_err(|_| format!("path escapes workspace via symlink: {target}"))?;
    // A parent component after a link can lead to a different in-workspace
    // path than the label obtained by removing those components lexically.
    let path =
        if has_parent_component && normalized.canonicalize().ok().as_deref() != Some(&canonical) {
            cwd.join(relative)
        } else {
            normalized
        };
    Ok(ResolvedWorkspacePath {
        path,
        root,
        canonical,
    })
}

#[cfg(not(windows))]
fn join_requested_path(cwd: &Path, candidate: PathBuf) -> PathBuf {
    cwd.join(candidate)
}

#[cfg(windows)]
fn join_requested_path(cwd: &Path, candidate: PathBuf) -> PathBuf {
    if candidate.is_absolute() {
        return candidate;
    }
    // Appending to a verbatim PathBuf with push would remove parent components
    // before we have checked that the directories they refer to exist.
    let mut joined = if candidate.has_root() {
        match cwd.components().next() {
            Some(std::path::Component::Prefix(prefix)) => prefix.as_os_str().to_owned(),
            _ => return candidate,
        }
    } else {
        let mut directory = cwd.as_os_str().to_owned();
        directory.push("\\");
        directory
    };
    joined.push(candidate.as_os_str());
    PathBuf::from(joined)
}

fn resolve_parent_components(path: &Path) -> std::io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        if component == std::path::Component::ParentDir {
            resolved = resolved.canonicalize()?;
            if !resolved.is_dir() {
                return Err(std::io::ErrorKind::NotADirectory.into());
            }
            resolved.pop();
        } else {
            resolved.push(component);
        }
    }
    Ok(resolved)
}

#[cfg(target_os = "macos")]
fn macos_path_in_workspace(cwd: &Path, path: &Path) -> Option<PathBuf> {
    use orca_platform::fs::VerifiedDirectory;
    use unicode_casefold::UnicodeCaseFold;
    use unicode_normalization::UnicodeNormalization;

    let depth = path
        .components()
        .count()
        .checked_sub(cwd.components().count())?;
    let prefix = path.ancestors().nth(depth)?;
    let normalize =
        |path: &Path| -> Option<String> { Some(path.to_str()?.nfc().case_fold().nfc().collect()) };
    let resolved_prefix = prefix.canonicalize().ok()?;
    if normalize(cwd)? != normalize(&resolved_prefix)? {
        return None;
    }
    let expected = open_workspace_root(cwd).ok()?.identity().ok()?;
    let actual = VerifiedDirectory::open(&resolved_prefix)
        .ok()?
        .identity()
        .ok()?;
    (expected == actual).then(|| cwd.join(path.strip_prefix(prefix).expect("checked prefix")))
}

#[cfg(any(test, windows))]
fn reject_drive_relative(path: &str) -> Result<(), String> {
    let spelling = path.replace('/', "\\");
    let spelling = spelling.strip_prefix(r"\\?\").unwrap_or(&spelling);
    let bytes = spelling.as_bytes();
    if bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
        && bytes.get(2) != Some(&b'\\')
    {
        return Err(format!("drive-relative paths are not supported: {path}"));
    }
    Ok(())
}

#[cfg(any(test, windows))]
fn windows_path_identity(path: &str) -> Result<orca_platform::fs::PathIdentity, String> {
    reject_drive_relative(path)?;
    orca_platform::fs::PathIdentity::windows(path).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn windows_path_in_workspace(cwd: &Path, path: &Path, target: &str) -> Result<PathBuf, String> {
    let workspace = windows_path_identity(&cwd.to_string_lossy())?;
    let candidate = windows_path_identity(&path.to_string_lossy())?;
    if !candidate.is_within(&workspace) {
        return Err(format!("path escapes workspace: {target}"));
    }
    let mut path = cwd.to_owned();
    path.extend(candidate.components().skip(workspace.components().count()));
    Ok(path)
}

#[cfg(test)]
pub(crate) fn workspace_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::mcp_types::{McpServerConfig, McpTransportKind};
    use orca_core::tool_types::{
        InterruptSemantics, ReplaySemantics, ToolControlSemantics, ToolStatus, truncate_output,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn a_file_used_as_a_directory_is_an_error() {
        let workspace = workspace_tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::create_dir(workspace.path().join("crates")).unwrap();
        assert!(resolve_workspace_path(workspace.path(), Some("Cargo.toml/../crates")).is_err());
    }

    #[test]
    fn a_missing_intermediate_directory_is_an_error() {
        let workspace = workspace_tempdir().unwrap();
        fs::create_dir(workspace.path().join("crates")).unwrap();
        assert!(resolve_workspace_path(workspace.path(), Some("missing/../crates")).is_err());
        assert!(resolve_workspace_path(workspace.path(), Some("missing/file.txt")).is_err());
        assert!(resolve_workspace_path(workspace.path(), Some("crates/file.txt")).is_ok());
    }

    #[test]
    fn an_empty_path_means_the_workspace_root() {
        let workspace = workspace_tempdir().unwrap();
        assert_eq!(
            resolve_workspace_path(workspace.path(), Some("")).unwrap(),
            workspace.path()
        );
    }

    #[test]
    fn verbatim_and_normal_windows_paths_compare_equal() {
        assert_eq!(
            windows_path_identity(r"\\?\C:\work\repo\src").unwrap(),
            windows_path_identity(r"C:\work/repo\src").unwrap(),
        );
    }

    #[test]
    fn verbatim_unc_and_unc_compare_equal() {
        assert_eq!(
            windows_path_identity(r"\\?\UNC\server\share\repo\src").unwrap(),
            windows_path_identity(r"\\server/share\repo/src").unwrap(),
        );
    }

    #[test]
    fn drive_relative_paths_are_refused() {
        assert_eq!(
            windows_path_identity("C:foo").unwrap_err(),
            "drive-relative paths are not supported: C:foo",
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_read_resolver_accepts_equivalent_root_spellings_by_identity() {
        let fixture = workspace_tempdir().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let root = base.join("Répo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("notes.txt"), "inside").unwrap();
        let mut tested = false;
        for spelling in ["répo", "Re\u{301}po"] {
            let alternate = base.join(spelling).join("notes.txt");
            if alternate.is_file() {
                tested = true;
                let resolved = resolve_workspace_read_path(&root, alternate.to_str()).unwrap();
                assert_eq!(resolved.path, root.join("notes.txt"));
            }
        }
        assert!(tested, "expected a normalization-equivalent path on macOS");
        assert!(resolve_workspace_read_path(&root, base.join("other/notes.txt").to_str()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolver_accepts_normal_and_verbatim_paths_with_mixed_separators() {
        let fixture = workspace_tempdir().unwrap();
        let verbatim = fixture.path().canonicalize().unwrap();
        let plain = Path::new(verbatim.to_str().unwrap().strip_prefix(r"\\?\").unwrap());
        fs::create_dir(plain.join("src")).unwrap();
        for (root, target) in [
            (&verbatim as &Path, plain.join("src")),
            (plain, verbatim.join("src")),
        ] {
            let mixed = target.to_str().unwrap().replace(r"\src", "/src");
            assert_eq!(
                resolve_workspace_path(root, Some(&mixed)).unwrap(),
                root.join("src")
            );
        }
        assert_eq!(
            resolve_workspace_path(plain, Some("C:foo")).unwrap_err(),
            "drive-relative paths are not supported: C:foo",
        );
    }

    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/S", "/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_workspace_reads_refuse_a_swapped_root_junction() {
        let fixture = workspace_tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        let neighbor = fixture.path().join("neighbor");
        fs::create_dir(&cwd).unwrap();
        fs::create_dir(&neighbor).unwrap();
        fs::write(cwd.join("notes.txt"), "inside").unwrap();
        fs::write(neighbor.join("notes.txt"), "outside").unwrap();
        let request = ToolRequest {
            id: "read".into(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".into()),
            raw_arguments: None,
        };
        let accepted = cwd.canonicalize().unwrap();
        fs::rename(&cwd, fixture.path().join("old")).unwrap();
        junction(&cwd, &neighbor);
        let result = read_file::execute(&request, &accepted, 4096);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("workspace path changed while Orca was running")
        );
        fs::remove_dir(cwd).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_workspace_reads_refuse_a_spill_junction_swap() {
        let fixture = workspace_tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        let session = fixture.path().join("session");
        let spill = session.join("spill");
        let neighbor = fixture.path().join("neighbor");
        for dir in [&cwd, &spill, &neighbor.join("spill")] {
            fs::create_dir_all(dir).unwrap();
        }
        let file = spill.join("call.txt");
        fs::write(&file, "saved result").unwrap();
        fs::write(neighbor.join("spill/call.txt"), "outside").unwrap();
        let request = ToolRequest {
            id: "read".into(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some(file.to_str().unwrap().into()),
            raw_arguments: None,
        };
        let read = || read_file::execute_in_session(&request, &cwd, 4096, Some(&spill), || false);
        assert!(read().output.unwrap().contains("saved result"));
        fs::rename(&session, fixture.path().join("old")).unwrap();
        junction(&session, &neighbor);
        let result = read();
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.output.is_none());
        fs::remove_dir(session).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_paths_use_the_workspace_spelling() {
        let plain = Path::new(r"\\server\share\repo");
        let verbatim = Path::new(r"\\?\UNC\server\share\repo");
        for (root, target) in [(plain, verbatim), (verbatim, plain)] {
            let target = target.join("src");
            assert_eq!(
                windows_path_in_workspace(root, &target, "src").unwrap(),
                root.join("src")
            );
        }
    }

    #[cfg(unix)]
    pub(crate) use std::os::unix::fs::symlink;

    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_for_a_link_after_resolution_is_refused() {
        let fixture = workspace_tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap().join("workspace");
        let folder = root.join("folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("file.txt"), "workspace text").unwrap();
        let replacement = fixture.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        fs::write(replacement.join("file.txt"), "neighbor text").unwrap();

        let path = resolve_workspace_path(&root, Some("folder/file.txt")).unwrap();
        fs::rename(&folder, root.join("previous")).unwrap();
        symlink(&replacement, &folder).unwrap();

        assert_eq!(
            open_workspace_file(&root, &path).unwrap_err().to_string(),
            format!("path changed while it was opened: {}", path.display())
        );
    }

    #[cfg(unix)]
    fn read_workspace_notes(root: &Path) -> orca_core::tool_types::ToolResult {
        let request = ToolRequest {
            id: "read".into(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".into()),
            raw_arguments: None,
        };
        read_file::execute(&request, root, 4096)
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_swapped_for_a_link_before_the_first_read_is_refused() {
        let fixture = workspace_tempdir().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let root = base.join("workspace");
        let neighbor = base.join("neighbor");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&neighbor).unwrap();
        fs::write(neighbor.join("notes.txt"), "neighbor text").unwrap();
        let accepted = root.canonicalize().unwrap();
        fs::rename(&root, base.join("previous")).unwrap();
        symlink(&neighbor, &root).unwrap();
        assert_eq!(
            read_workspace_notes(&accepted).error.as_deref().unwrap(),
            format!(
                "workspace path changed while Orca was running: {}",
                accepted.display()
            )
        );
    }

    #[cfg(unix)]
    fn read_after_reclone(new_session: bool) {
        let fixture = workspace_tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("notes.txt"), "original").unwrap();
        let accepted = root.canonicalize().unwrap();
        assert!(
            read_workspace_notes(&accepted)
                .output
                .unwrap()
                .contains("original")
        );
        fs::rename(&root, root.with_file_name("previous")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("notes.txt"), "recloned").unwrap();
        let accepted = if new_session {
            root.canonicalize().unwrap()
        } else {
            accepted
        };
        assert!(
            read_workspace_notes(&accepted)
                .output
                .unwrap()
                .contains("recloned")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_recloned_workspace_at_the_same_path_keeps_working() {
        read_after_reclone(false);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_session_after_a_reclone_reads_normally() {
        read_after_reclone(true);
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_root_swapped_after_resolution_is_refused() {
        let fixture = workspace_tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap().join("workspace");
        let neighbor = fixture.path().join("neighbor");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&neighbor).unwrap();
        fs::write(root.join("notes.txt"), "workspace text").unwrap();
        fs::write(neighbor.join("notes.txt"), "neighbor text").unwrap();
        let path = resolve_workspace_read_path(&root, Some("notes.txt")).unwrap();
        fs::rename(&root, fixture.path().join("previous")).unwrap();
        symlink(&neighbor, &root).unwrap();
        assert!(crate::workspace_read::open_read_file(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_outward_link_in_a_parent_is_refused() {
        let fixture = workspace_tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap().join("workspace");
        let neighbor = fixture.path().join("neighbor");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&neighbor).unwrap();
        fs::write(neighbor.join("file.txt"), "neighbor text").unwrap();
        let link = root.join("folder");
        symlink(&neighbor, &link).unwrap();

        let request = ToolRequest {
            id: "read".into(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("folder/file.txt".into()),
            raw_arguments: None,
        };
        let result = read_file::execute(&request, &root, 4096);
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("escapes workspace")
        );
    }

    #[test]
    fn micro_compact_preserves_head_and_tail() {
        let output = format!("{}{}{}", "a".repeat(80), "middle", "z".repeat(80));
        let (truncated, was_truncated) = truncate_output(output, 80);
        assert!(was_truncated);
        assert!(truncated.starts_with("aaaa"));
        assert!(truncated.contains("micro-compacted"));
        assert!(truncated.ends_with("zzzz"));
    }

    #[test]
    fn default_registry_exposes_builtin_tool_metadata() {
        let reg = registry::default_tool_registry();

        let tool = reg
            .get("read_file")
            .expect("read_file is registered as a tool");
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("README.md".to_string()),
            raw_arguments: None,
        };

        assert_eq!(tool.name(), "read_file");
        assert_eq!(tool.action_kind(), ActionKind::Read);
        assert!(tool.is_read_only(&request));
        assert!(tool.is_concurrent_safe(&request));
        assert_eq!(reg.iter().next().map(|tool| tool.name()), Some("read_file"));

        assert_eq!(
            reg.get("web_search").unwrap().action_kind(),
            ActionKind::Network
        );
        assert_eq!(
            reg.get("subagent").unwrap().action_kind(),
            ActionKind::Agent
        );
    }

    #[test]
    fn registry_control_semantics_resolves_registered_identity_and_aliases() {
        let default_registry = registry::default_tool_registry();
        assert_eq!(
            default_registry.control_semantics(&ToolName::Bash),
            Some(ToolControlSemantics {
                interrupt: InterruptSemantics::CooperativeCancel,
                replay: ReplaySemantics::IndeterminateAfterStart,
            })
        );
        assert_eq!(
            default_registry.control_semantics(&ToolName::plain("list_files")),
            Some(ToolControlSemantics {
                interrupt: InterruptSemantics::WaitForTerminal,
                replay: ReplaySemantics::SafeToRetry,
            })
        );
        assert_eq!(
            default_registry.control_semantics(&ToolName::plain("not_registered")),
            None
        );

        let external_tools = vec![ExternalToolConfig {
            name: "inspect_remote".to_string(),
            description: "external read-like tool".to_string(),
            action_kind: ActionKind::Read,
            command: "printf inspected".to_string(),
            schema: serde_json::json!({}),
        }];
        let registry = registry::tool_registry_with_mcp_and_external(None, &external_tools);
        assert_eq!(
            registry.control_semantics(&ToolName::plain("inspect_remote")),
            Some(ToolControlSemantics {
                interrupt: InterruptSemantics::CooperativeCancel,
                replay: ReplaySemantics::IndeterminateAfterStart,
            })
        );
        assert_eq!(
            registry.control_semantics(&ToolName::ReadFile),
            Some(ToolControlSemantics {
                interrupt: InterruptSemantics::WaitForTerminal,
                replay: ReplaySemantics::SafeToRetry,
            }),
            "dynamic tools cannot shadow a builtin policy"
        );
    }

    #[test]
    fn registry_resolves_list_files_to_discovery_capabilities() {
        let reg = registry::default_tool_registry();
        let resolved = reg.resolve("list_files").expect("list_files alias");

        assert_eq!(resolved.tool.name(), "glob");
        assert!(resolved.spec.capabilities.is_read_only());
        assert_eq!(resolved.requested_name.as_str(), "list_files");
    }

    #[test]
    fn model_visible_tools_hide_list_files_after_glob_exists() {
        let reg = registry::default_tool_registry();
        let names = reg
            .model_visible_tools()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert!(names.contains(&"glob".to_string()));
        assert!(!names.contains(&"list_files".to_string()));
    }

    #[test]
    fn request_user_input_is_not_registered_after_canonical_name_migration() {
        let reg = registry::default_tool_registry();
        assert!(reg.get("request_user_input").is_none());
        assert!(reg.resolve("request_user_input").is_none());
        assert!(
            reg.model_visible_tools()
                .all(|tool| tool.name() != "request_user_input")
        );
    }

    #[test]
    fn ask_user_question_tool_exposes_structured_questionnaire_contract() {
        let reg = registry::default_tool_registry();
        let tool = reg
            .get("ask_user_question")
            .expect("ask_user_question is registered");
        let schema = &tool.spec().input_schema;
        let question_items = &schema["properties"]["questions"];
        let option_items = &question_items["items"]["properties"]["options"];

        assert!(tool.spec().exposure.is_model_visible());
        assert_eq!(tool.spec().name.as_str(), "ask_user_question");
        assert!(
            reg.resolve("AskUserQuestion").is_none(),
            "PascalCase spelling must not remain as an alias"
        );
        assert_eq!(question_items["minItems"], 1);
        assert_eq!(question_items["maxItems"], 4);
        assert_eq!(option_items["minItems"], 2);
        assert_eq!(option_items["maxItems"], 4);
        assert_eq!(
            question_items["items"]["required"],
            serde_json::json!(["header", "question", "options"])
        );
        assert_eq!(
            option_items["items"]["required"],
            serde_json::json!(["label", "description"])
        );
        assert_eq!(
            question_items["items"]["properties"]["multiSelect"]["type"],
            "boolean"
        );

        let request = ToolRequest {
            id: "questionnaire".to_string(),
            name: ToolName::plain("ask_user_question"),
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                r#"{"questions":[{"header":"Runtime","question":"Which path?","options":[{"label":"Reuse","description":"Use the runtime broker"},{"label":"New","description":"Create another path"}],"multiSelect":false}]}"#
                    .to_string(),
            ),
        };

        assert!(tool.is_read_only(&request));
        assert!(!tool.is_concurrent_safe(&request));
        let result = reg.execute(&request, &registry::ToolContext::new(Path::new(".")));
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("interactive TUI session")
        );
    }

    #[test]
    fn skill_tools_are_model_visible_readonly_tools() {
        let reg = registry::default_tool_registry();
        for name in ["list_skills", "read_skill"] {
            let tool = reg.get(name).expect("skill tool is registered");
            let request = ToolRequest {
                id: name.to_string(),
                name: ToolName::plain(name),
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"id":"debugging"}"#.to_string()),
            };

            assert!(tool.spec().exposure.is_model_visible());
            assert!(tool.is_read_only(&request));
            assert!(tool.is_concurrent_safe(&request));
        }
    }

    #[cfg(unix)]
    #[test]
    fn mcp_tool_execution_observes_cancel_callback() {
        let temp_dir = workspace_tempdir().expect("temp dir");
        let server = temp_dir.path().join("slow_mcp_server.sh");
        fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      sleep 5
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }

        let registry = orca_mcp::initialize_registry(
            &[McpServerConfig {
                name: "slow".to_string(),
                transport: McpTransportKind::Stdio,
                command: Some(server.to_string_lossy().into_owned()),
                args: Vec::new(),
                url: None,
                env: Default::default(),
                headers: Default::default(),
                disabled: false,
                capabilities: Default::default(),
                startup_timeout_ms: None,
                tool_timeout_ms: None,
                ..Default::default()
            }],
            None,
        );
        assert!(registry.wait_for_startup(&|| false));
        assert!(
            registry.errors().is_empty(),
            "registry errors: {:?}",
            registry.errors()
        );
        let request = ToolRequest {
            id: "mcp-call".to_string(),
            name: ToolName::Mcp("mcp__slow__wait".to_string()),
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();

        let result = execute_with_mcp_external_policy_or_cancel(
            &request,
            temp_dir.path(),
            &registry,
            &[],
            ToolOutputTruncation::default(),
            30,
            || {
                if started.elapsed() >= Duration::from_millis(100) {
                    cancelled.store(true, Ordering::SeqCst);
                }
                cancelled.load(Ordering::SeqCst)
            },
        );

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "cancelled MCP call took {:?}",
            started.elapsed()
        );
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::Cancelled
        );
        assert_eq!(result.error.as_deref(), Some("MCP tool call cancelled"));
    }

    #[test]
    fn execute_with_mcp_passes_registry_to_resource_tools() {
        let resource = orca_core::mcp_types::McpResource {
            server: "notes".to_string(),
            uri: "memo://orca/one".to_string(),
            name: "memo one".to_string(),
            description: Some("A test memo".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let read_result = orca_core::mcp_types::ReadResourceResult {
            contents: vec![orca_core::mcp_types::McpResourceContent {
                uri: "memo://orca/one".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some("resource body".to_string()),
                blob: None,
            }],
        };
        let registry = McpRegistry::from_static_resources_for_test(
            vec![resource],
            HashMap::from([(
                ("notes".to_string(), "memo://orca/one".to_string()),
                read_result,
            )]),
        );

        let list = execute_with_mcp(
            &ToolRequest {
                id: "list-resources".to_string(),
                name: ToolName::ListMcpResources,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"server":"notes"}"#.to_string()),
            },
            Path::new("."),
            &registry,
        );
        assert_eq!(list.status, ToolStatus::Completed);
        assert!(
            list.output
                .as_deref()
                .unwrap_or_default()
                .contains(r#""uri":"memo://orca/one""#)
        );

        let read = execute_with_mcp(
            &ToolRequest {
                id: "read-resource".to_string(),
                name: ToolName::ReadMcpResource,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"server":"notes","uri":"memo://orca/one"}"#.to_string()),
            },
            Path::new("."),
            &registry,
        );
        assert_eq!(read.status, ToolStatus::Completed);
        assert!(
            read.output
                .as_deref()
                .unwrap_or_default()
                .contains("resource body")
        );
    }

    #[test]
    fn list_mcp_resources_reports_partial_resource_errors() {
        let resource = orca_core::mcp_types::McpResource {
            server: "notes".to_string(),
            uri: "memo://orca/one".to_string(),
            name: "memo one".to_string(),
            description: Some("A test memo".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let registry = McpRegistry::from_resource_listing_for_test(
            vec![resource],
            vec!["broken: resources/list timed out".to_string()],
        );

        let result = execute_with_mcp(
            &ToolRequest {
                id: "list-resources".to_string(),
                name: ToolName::ListMcpResources,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            },
            Path::new("."),
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        let output: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("tool output"))
                .expect("resource listing JSON");
        assert_eq!(output["resources"][0]["server"], "notes");
        assert_eq!(output["resources"][0]["uri"], "memo://orca/one");
        assert_eq!(output["errors"][0], "broken: resources/list timed out");
    }

    #[test]
    fn list_mcp_resources_reports_registry_initialization_errors() {
        let resource = orca_core::mcp_types::McpResource {
            server: "notes".to_string(),
            uri: "memo://orca/one".to_string(),
            name: "memo one".to_string(),
            description: Some("A test memo".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let registry = McpRegistry::from_resource_listing_for_test(vec![resource], Vec::new())
            .with_registry_errors_for_test(vec![
                "failed to start MCP server 'broken': boom".to_string(),
            ]);

        let result = execute_with_mcp(
            &ToolRequest {
                id: "list-resources".to_string(),
                name: ToolName::ListMcpResources,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            },
            Path::new("."),
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        let output: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("tool output"))
                .expect("resource listing JSON");
        assert_eq!(output["resources"][0]["server"], "notes");
        assert_eq!(
            output["errors"][0],
            "failed to start MCP server 'broken': boom"
        );
    }

    #[test]
    fn list_mcp_resource_templates_reports_partial_template_errors() {
        let template = orca_core::mcp_types::McpResourceTemplate {
            server: "docs".to_string(),
            uri_template: "file:///{path}".to_string(),
            name: "workspace file".to_string(),
            description: Some("A file exposed by path".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let registry = McpRegistry::from_resource_template_listing_for_test(
            vec![template],
            vec!["broken: resources/templates/list timed out".to_string()],
        );

        let result = execute_with_mcp(
            &ToolRequest {
                id: "list-resource-templates".to_string(),
                name: ToolName::ListMcpResourceTemplates,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            },
            Path::new("."),
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        let output: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("tool output"))
                .expect("resource template listing JSON");
        assert_eq!(output["resourceTemplates"][0]["server"], "docs");
        assert_eq!(
            output["resourceTemplates"][0]["uriTemplate"],
            "file:///{path}"
        );
        assert_eq!(
            output["errors"][0],
            "broken: resources/templates/list timed out"
        );
    }

    #[test]
    fn list_mcp_resource_templates_reports_registry_initialization_errors() {
        let template = orca_core::mcp_types::McpResourceTemplate {
            server: "docs".to_string(),
            uri_template: "file:///{path}".to_string(),
            name: "workspace file".to_string(),
            description: Some("A file exposed by path".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let registry =
            McpRegistry::from_resource_template_listing_for_test(vec![template], Vec::new())
                .with_registry_errors_for_test(vec![
                    "failed to start MCP server 'broken': boom".to_string(),
                ]);

        let result = execute_with_mcp(
            &ToolRequest {
                id: "list-resource-templates".to_string(),
                name: ToolName::ListMcpResourceTemplates,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            },
            Path::new("."),
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        let output: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("tool output"))
                .expect("resource template listing JSON");
        assert_eq!(output["resourceTemplates"][0]["server"], "docs");
        assert_eq!(
            output["errors"][0],
            "failed to start MCP server 'broken': boom"
        );
    }

    #[test]
    fn external_tool_cannot_shadow_builtin_list_files_alias() {
        let external_tools = vec![ExternalToolConfig {
            name: "list_files".to_string(),
            description: "external list files".to_string(),
            action_kind: ActionKind::Shell,
            command: "echo external".to_string(),
            schema: serde_json::json!({}),
        }];
        let reg = registry::tool_registry_with_mcp_and_external(None, &external_tools);
        let resolved = reg.resolve("list_files").expect("list_files alias");

        assert_eq!(resolved.tool.name(), "glob");
        assert_eq!(resolved.spec.capabilities.action_kind(), ActionKind::Read);
    }

    #[test]
    fn external_tool_runs_to_completion_without_an_administrator_cap() {
        // `shell_timeout_secs = 0` is the default and sets no cap; it must
        // not become a one-second limit on every external tool.
        let temp_dir = workspace_tempdir().unwrap();
        let external_tools = vec![ExternalToolConfig {
            name: "slow_lookup".to_string(),
            description: "takes a moment".to_string(),
            action_kind: ActionKind::Read,
            command: if cfg!(windows) {
                "Start-Sleep -Milliseconds 1500; [Console]::Out.Write('done')".to_string()
            } else {
                "sleep 1.5; printf done".to_string()
            },
            schema: serde_json::json!({}),
        }];
        let request = ToolRequest {
            id: "slow".to_string(),
            name: ToolName::External("slow_lookup".to_string()),
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        let result = execute_with_mcp_and_external(
            &request,
            temp_dir.path(),
            &McpRegistry::default(),
            &external_tools,
            0,
        );

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert_eq!(result.output.as_deref(), Some("done"));
    }

    #[test]
    fn external_tool_stops_at_the_administrator_cap() {
        let temp_dir = workspace_tempdir().unwrap();
        let external_tools = vec![ExternalToolConfig {
            name: "slow_lookup".to_string(),
            description: "takes too long".to_string(),
            action_kind: ActionKind::Read,
            command: if cfg!(windows) {
                "Start-Sleep -Seconds 10".to_string()
            } else {
                "sleep 10".to_string()
            },
            schema: serde_json::json!({}),
        }];
        let request = ToolRequest {
            id: "slow".to_string(),
            name: ToolName::External("slow_lookup".to_string()),
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        let result = execute_with_mcp_and_external(
            &request,
            temp_dir.path(),
            &McpRegistry::default(),
            &external_tools,
            1,
        );

        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some("external tool 'slow_lookup' timed out after 1s")
        );
    }

    #[test]
    fn readonly_batch_ignores_caller_supplied_write_action_for_read_tool() {
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Write,
            target: Some("README.md".to_string()),
            raw_arguments: None,
        };

        assert!(should_run_readonly_batch(2, &request));
    }

    #[test]
    fn canonical_action_kind_ignores_caller_supplied_read_for_shell() {
        let request = ToolRequest {
            id: "bash".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Read,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };

        assert_eq!(canonical_action_kind(&request), ActionKind::Shell);
    }

    #[test]
    fn registry_executes_glob_with_pattern_and_path() {
        let temp_dir = workspace_tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src/bin")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/lib.rs"), "lib").expect("fixture");
        fs::write(temp_dir.path().join("src/bin/main.rs"), "main").expect("fixture");
        fs::write(temp_dir.path().join("src/readme.md"), "readme").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: Some(r#"{"pattern":"**/*.rs","path":"src"}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some("src/bin/main.rs\nsrc/lib.rs")
        );
    }

    #[test]
    fn registry_executes_glob_with_workspace_prefixed_pattern_from_dot_path() {
        let temp_dir = workspace_tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src/bin")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/lib.rs"), "lib").expect("fixture");
        fs::write(temp_dir.path().join("src/bin/main.rs"), "main").expect("fixture");
        fs::write(temp_dir.path().join("README.md"), "readme").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some(".".to_string()),
            raw_arguments: Some(r#"{"pattern":"src/**/*.rs","path":"."}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some("src/bin/main.rs\nsrc/lib.rs")
        );
    }

    #[test]
    fn registry_executes_glob_with_no_matches() {
        let temp_dir = workspace_tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path()).expect("fixture dir");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("missing".to_string()),
            raw_arguments: Some(r#"{"pattern":"*.rs","path":"missing"}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::NoMatches
        );
    }

    #[test]
    fn registry_executes_glob_with_fuzzy_query() {
        let temp_dir = workspace_tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src/runtime/config")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/runtime/config/mod.rs"), "mod").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(r#"{"mode":"fuzzy","query":"rcm"}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, orca_core::tool_types::ToolResultKind::Success);
        let output = result.output.expect("fuzzy output");
        assert!(
            output
                .lines()
                .any(|line| line == "src/runtime/config/mod.rs")
        );
    }

    #[test]
    fn registry_exposes_glob_fuzzy_schema() {
        let reg = registry::default_tool_registry();
        let glob = reg.resolve("glob").expect("glob tool").tool;
        let schema = &glob.spec().input_schema;

        assert_eq!(schema["properties"]["mode"]["anyOf"][0]["enum"][1], "fuzzy");
        assert_eq!(schema["properties"]["query"]["anyOf"][0]["type"], "string");
        assert_eq!(schema["properties"]["query"]["anyOf"][1]["type"], "null");
        assert!(
            schema["anyOf"]
                .as_array()
                .expect("anyOf")
                .iter()
                .any(|entry| entry["required"]
                    .as_array()
                    .expect("required")
                    .iter()
                    .any(|value| value == "query"))
        );
    }

    #[test]
    fn registry_executes_glob_with_truncated_kind() {
        let temp_dir = workspace_tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/alpha.rs"), "alpha").expect("fixture");
        fs::write(temp_dir.path().join("src/bravo.rs"), "bravo").expect("fixture");
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: Some(r#"{"pattern":"*.rs","path":"src"}"#.to_string()),
        };

        let result = glob::execute(&request, temp_dir.path(), 12);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::Truncated
        );
        assert!(result.truncated);
    }

    #[test]
    fn registry_executes_builtin_tool_by_registered_name() {
        let temp_dir = workspace_tempdir().expect("temp dir");
        fs::write(temp_dir.path().join("note.txt"), "hello registry\n").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("note.txt".to_string()),
            raw_arguments: Some(r#"{"path":"note.txt"}"#.to_string()),
        };
        let ctx = registry::ToolContext::new(temp_dir.path());

        let result = reg.execute(&request, &ctx);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("hello registry\n"));
    }
}
