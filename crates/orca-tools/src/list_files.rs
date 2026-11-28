use std::fs;
use std::io;
use std::path::Path;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};

use crate::{resolve_workspace_path, workspace_read::listing_entry};

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let root = match cwd.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            return ToolResult::failed(request, format!("cannot resolve cwd: {error}"), None);
        }
    };
    let path = match resolve_workspace_path(cwd, request.target.as_deref()) {
        Ok(p) => p,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ToolResult::completed_kind(
                request,
                "(empty)".to_string(),
                false,
                ToolResultKind::Empty,
            );
        }
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to list {}: {error}", path.display()),
                None,
            );
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                if listing_entry(&root, &entry.path()).is_some() {
                    names.push(entry.file_name().to_string_lossy().to_string());
                }
            }
            Err(error) => {
                return ToolResult::failed(request, format!("failed to read entry: {error}"), None);
            }
        }
    }
    names.sort();

    let (output, truncated) = truncate_output(names.join("\n"), max_bytes);
    let kind = if output.is_empty() && !truncated {
        ToolResultKind::Empty
    } else {
        ToolResultKind::Success
    };
    let output = if output.is_empty() && !truncated {
        "(empty)".to_string()
    } else {
        output
    };
    ToolResult::completed_kind(request, output, truncated, kind)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolResultKind, ToolStatus};

    use super::*;

    #[cfg(unix)]
    #[test]
    fn list_files_skips_entries_whose_real_location_is_outside() {
        let workspace = tempfile::tempdir().unwrap();
        let neighbor = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let linked = root.join("linked");
        fs::create_dir(neighbor.path().join("notes")).unwrap();
        crate::tests::symlink(neighbor.path(), &linked).unwrap();

        assert_eq!(listing_entry(&root, &linked.join("notes")), None);
    }

    #[test]
    fn missing_directory_completes_with_empty_listing() {
        let cwd = temp_dir("list-files-missing");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        let request = ToolRequest {
            id: "list-1".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some(".orca/workflows".to_string()),
            raw_arguments: None,
        };

        let result = execute(&request, &cwd, 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::Empty);
        assert_eq!(result.output.as_deref(), Some("(empty)"));
        assert_eq!(result.error, None);
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "orca-{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
