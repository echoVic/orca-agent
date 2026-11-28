use std::io;
use std::path::Path;

use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};

use crate::{
    resolve_workspace_read_path,
    workspace_read::{listing_entry, open_directory, open_root},
};

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let resolved = match resolve_workspace_read_path(cwd, request.target.as_deref()) {
        Ok(path) => path,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let entries = match open_directory(&resolved.root, &resolved.canonical)
        .and_then(|dir| dir.read_names())
    {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ToolResult::completed_kind(
                request,
                "(empty)".into(),
                false,
                ToolResultKind::Empty,
            );
        }
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to list {}: {error}", resolved.path.display()),
                None,
            );
        }
    };

    let mut names = entries
        .into_iter()
        .filter_map(|name| {
            let path = resolved.path.join(&name);
            listing_entry(&resolved.root, &path)?;
            Some((name.to_string_lossy().into_owned(), path))
        })
        .collect::<Vec<_>>();
    names.sort();
    #[cfg(all(test, unix))]
    crate::workspace_read::listing_test::before_emission();
    if let Err(error) = open_root(&resolved.root) {
        return ToolResult::failed(request, error.to_string(), None);
    }
    let names = names
        .into_iter()
        .filter_map(|(label, path)| listing_entry(&resolved.root, &path).map(|_| label))
        .collect::<Vec<_>>();
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
        let workspace = crate::workspace_tempdir().unwrap();
        let neighbor = crate::workspace_tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let linked = root.join("linked");
        fs::create_dir(neighbor.path().join("notes")).unwrap();
        crate::tests::symlink(neighbor.path(), &linked).unwrap();

        assert_eq!(listing_entry(&root, &linked.join("notes")), None);
    }

    #[cfg(unix)]
    #[test]
    fn list_files_rechecks_selected_entries_before_emission() {
        let workspace = crate::workspace_tempdir().unwrap();
        let neighbor = crate::workspace_tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let changing = root.join("changing.txt");
        fs::write(&changing, "original").unwrap();
        fs::write(root.join("keep.txt"), "inside").unwrap();
        let outside = neighbor.path().join("outside.txt");
        fs::write(&outside, "outside").unwrap();
        crate::workspace_read::listing_test::BEFORE_EMISSION.with(|hook| {
            hook.replace(Some(Box::new(move || {
                fs::remove_file(&changing).unwrap();
                crate::tests::symlink(outside, changing).unwrap();
            })));
        });
        let request = ToolRequest {
            id: "list".into(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: None,
            raw_arguments: None,
        };
        let result = execute(&request, &root, 4096);
        assert_eq!(result.output.as_deref(), Some("keep.txt"));
    }

    #[test]
    fn missing_directory_completes_with_empty_listing() {
        let cwd = temp_dir("list-files-missing");
        fs::create_dir_all(cwd.join(".orca")).expect("create temp workspace");
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
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "orca-{prefix}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
