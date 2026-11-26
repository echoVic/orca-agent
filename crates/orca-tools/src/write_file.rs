use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use orca_core::tool_types::{FileChangePreview, ToolRequest, ToolResult};

use crate::edit::crlf_majority;
use crate::file_admission::{
    FileAdmissionError, MAX_DIFF_INPUT_BYTES, build_file_change_preview, open_regular_file,
    read_text_file_with_limit,
};

const LINE_ENDING_SAMPLE_BYTES: usize = 64 * 1024;

pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    let plan = match plan_write(request, cwd, false) {
        Ok(plan) => plan,
        Err(error) => return ToolResult::failed(request, error, None),
    };

    if let Some(parent) = plan.path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ToolResult::failed(request, format!("failed to create directories: {e}"), None);
        }
    }

    match fs::write(&plan.path, &plan.content) {
        Ok(()) => ToolResult::completed(
            request,
            format!(
                "wrote {} bytes to {}",
                plan.content.len(),
                plan.display_path
            ),
            false,
        )
        .with_file_change_preview(plan.preview),
        Err(e) => ToolResult::failed(request, format!("failed to write file: {e}"), None),
    }
}

/// The diff this write would make, without writing it or creating its
/// directory, for an approval to show. `None` when the write could not apply.
pub fn preview(request: &ToolRequest, cwd: &Path) -> Option<FileChangePreview> {
    plan_write(request, cwd, true).ok().map(|plan| plan.preview)
}

struct PlannedWrite {
    path: PathBuf,
    display_path: String,
    content: String,
    preview: FileChangePreview,
}

fn plan_write(
    request: &ToolRequest,
    cwd: &Path,
    preview_only: bool,
) -> Result<PlannedWrite, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "missing arguments".to_string())?;
    let args: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid arguments: {e}"))?;
    let path_str = args["path"]
        .as_str()
        .ok_or_else(|| "missing required parameter: path".to_string())?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| "missing required parameter: content".to_string())?;
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|e| format!("cannot resolve cwd: {e}"))?;

    let joined = canonical_cwd.join(path_str);

    // Normalize by resolving ".." components without filesystem access
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            _ => normalized.push(component),
        }
    }

    if !normalized.starts_with(&canonical_cwd) {
        return Err(format!("path escapes workspace: {}", path_str));
    }

    let before = (content.len() <= MAX_DIFF_INPUT_BYTES)
        .then(|| read_text_file_with_limit(&normalized, MAX_DIFF_INPUT_BYTES, || false));
    let keep_crlf = content.contains('\n')
        && !content.contains("\r\n")
        && match before.as_ref() {
            Some(Ok(text)) => {
                crlf_majority(&text.as_bytes()[..text.len().min(LINE_ENDING_SAMPLE_BYTES)])
            }
            None | Some(Err(FileAdmissionError::TooLarge { .. })) if !preview_only => {
                file_starts_with_crlf_majority(&normalized)
            }
            _ => false,
        };
    let content = if keep_crlf {
        content.replace('\n', "\r\n")
    } else {
        content.to_string()
    };
    let preview = match before {
        Some(Ok(before)) => build_file_change_preview(path_str, Some(&before), Some(&content)),
        Some(Err(error)) if error.is_not_found() => {
            build_file_change_preview(path_str, None, Some(&content))
        }
        _ => FileChangePreview::Omitted {
            path: path_str.to_string(),
            max_input_bytes: MAX_DIFF_INPUT_BYTES,
        },
    };

    Ok(PlannedWrite {
        path: normalized,
        display_path: path_str.to_string(),
        content,
        preview,
    })
}

fn file_starts_with_crlf_majority(path: &Path) -> bool {
    let Ok((file, _)) = open_regular_file(path) else {
        return false;
    };
    let mut sample = Vec::new();
    file.take(LINE_ENDING_SAMPLE_BYTES as u64)
        .read_to_end(&mut sample)
        .is_ok()
        && crlf_majority(&sample)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{FileChangePreview, ToolName, ToolStatus};
    use tempfile::TempDir;

    #[test]
    fn preview_shows_a_new_file_without_creating_it_or_its_directory() {
        let dir = TempDir::new().unwrap();
        let req = make_request("docs/notes.md", "# Notes\n");

        let preview = preview(&req, dir.path()).expect("a valid write has a preview");

        let FileChangePreview::UnifiedDiff { text, .. } = preview else {
            panic!("a small write renders a unified diff");
        };
        assert!(text.contains("+# Notes"));
        assert!(
            !dir.path().join("docs").exists(),
            "a preview must not create directories"
        );
    }

    #[test]
    fn preview_of_a_path_outside_the_workspace_is_none() {
        let dir = TempDir::new().unwrap();
        assert!(preview(&make_request("../escape.txt", "x"), dir.path()).is_none());
    }

    fn make_request(path: &str, content: &str) -> ToolRequest {
        ToolRequest {
            id: "test-1".to_string(),
            name: ToolName::WriteFile,
            action: ActionKind::Write,
            target: Some(path.to_string()),
            raw_arguments: Some(
                serde_json::json!({ "path": path, "content": content }).to_string(),
            ),
        }
    }

    #[test]
    fn writing_lf_text_over_a_crlf_file_keeps_crlf() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let before = "one\r\ntwo\r\n";
        fs::write(&path, before).unwrap();
        let request = make_request("test.txt", "one\nthree\n");

        let result = execute(&request, dir.path());
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(text, "one\r\nthree\r\n");
        assert_eq!(
            result.file_change_preview.as_deref(),
            Some(&build_file_change_preview(
                "test.txt",
                Some(before),
                Some(&text),
            ))
        );
    }

    #[test]
    fn write_file_samples_the_start_of_a_large_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let before = "a!\r\n".repeat(1024 * 1024 / 4) + &"b\n".repeat(10 * 1024 * 1024 / 2);
        fs::write(&path, before).unwrap();
        let request = make_request("test.txt", "one\nthree\n");

        let result = execute(&request, dir.path());

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\r\nthree\r\n");
        assert!(matches!(
            result.file_change_preview.as_deref(),
            Some(FileChangePreview::Omitted { .. })
        ));
    }

    #[test]
    fn overwriting_a_crlf_file_too_large_to_preview_keeps_crlf() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        // Count the last complete pair in the sample and ignore a trailing CR.
        let mut before = "a".repeat(LINE_ENDING_SAMPLE_BYTES - 3);
        before.push_str("\r\n\r\n");
        before.push_str(&"b".repeat(MAX_DIFF_INPUT_BYTES));
        fs::write(&path, before).unwrap();
        let request = make_request("test.txt", "one\nthree\n");

        let result = execute(&request, dir.path());

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\r\nthree\r\n");
        assert!(matches!(
            result.file_change_preview.as_deref(),
            Some(FileChangePreview::Omitted { .. })
        ));
    }

    #[test]
    fn content_with_crlf_is_written_as_given() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "one\r\ntwo\r\n").unwrap();
        let text = "one\r\nthree\n";

        assert_eq!(
            execute(&make_request("test.txt", text), dir.path()).status,
            ToolStatus::Completed
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn a_new_file_is_written_as_given() {
        let dir = TempDir::new().unwrap();
        let text = "one\r\nthree\n";

        assert_eq!(
            execute(&make_request("test.txt", text), dir.path()).status,
            ToolStatus::Completed
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("test.txt")).unwrap(),
            text
        );
    }

    #[test]
    fn write_creates_file() {
        let dir = TempDir::new().unwrap();
        let req = make_request("hello.txt", "world");
        let result = execute(&req, dir.path());
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let req = make_request("a/b/c.txt", "nested");
        let result = execute(&req, dir.path());
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "nested"
        );
    }

    #[test]
    fn write_rejects_path_escape() {
        let dir = TempDir::new().unwrap();
        let req = make_request("../escape.txt", "bad");
        let result = execute(&req, dir.path());
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
    fn overwrite_emits_committed_file_change_preview() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("hello.txt"), "before\n").unwrap();
        let req = make_request("hello.txt", "after\n");

        let result = execute(&req, dir.path());

        let preview = result.file_change_preview.expect("write preview");
        let FileChangePreview::UnifiedDiff { text, truncated } = preview.as_ref() else {
            panic!("small overwrite should render a unified diff");
        };
        assert!(!*truncated);
        assert!(text.contains("-before"));
        assert!(text.contains("+after"));
    }
}
