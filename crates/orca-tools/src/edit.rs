use std::fs;
use std::path::Path;

use orca_core::tool_types::{FileChangePreview, ToolRequest, ToolResult};

use crate::file_admission::{
    MAX_EDIT_FILE_BYTES, build_file_change_preview, read_text_file_with_limit,
};
use crate::resolve_workspace_path;

pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    execute_or_cancel(request, cwd, || false)
}

pub fn execute_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let plan = match plan_edit(request, cwd, &should_cancel) {
        Ok(plan) => plan,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    if should_cancel() {
        return ToolResult::failed(request, "file edit cancelled", None);
    }
    let preview = build_file_change_preview(
        &plan.relative_path,
        Some(&plan.contents),
        Some(&plan.updated),
    );
    if let Err(error) = fs::write(&plan.path, &plan.updated) {
        return ToolResult::failed(
            request,
            format!("failed to write {}: {error}", plan.path.display()),
            None,
        );
    }

    ToolResult::completed(request, format!("edited {}", plan.relative_path), false)
        .with_file_change_preview(preview)
}

/// The diff this edit would make, without writing it, for an approval to
/// show. `None` when the edit could not apply.
pub fn preview(request: &ToolRequest, cwd: &Path) -> Option<FileChangePreview> {
    let plan = plan_edit(request, cwd, &|| false).ok()?;
    Some(build_file_change_preview(
        &plan.relative_path,
        Some(&plan.contents),
        Some(&plan.updated),
    ))
}

struct PlannedEdit {
    path: std::path::PathBuf,
    relative_path: String,
    contents: String,
    updated: String,
}

fn plan_edit(
    request: &ToolRequest,
    cwd: &Path,
    should_cancel: &dyn Fn() -> bool,
) -> Result<PlannedEdit, String> {
    let (path_str, old, new) = parse_edit_args(request)?;
    let path = resolve_workspace_path(cwd, Some(&path_str))?;
    if !is_inside_workspace(cwd, &path) {
        return Err("edit target is outside the workspace".to_string());
    }
    if old.is_empty() {
        return Err("edit old text cannot be empty".to_string());
    }
    let contents = read_text_file_with_limit(&path, MAX_EDIT_FILE_BYTES, should_cancel)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let matches = contents.matches(&*old).count();
    if matches == 0 {
        return Err("edit old text was not found".to_string());
    }
    if matches > 1 {
        return Err("edit old text matched multiple locations".to_string());
    }
    let updated_bytes = contents
        .len()
        .saturating_sub(old.len())
        .saturating_add(new.len());
    if updated_bytes > MAX_EDIT_FILE_BYTES {
        return Err(format!(
            "edited file would be too large ({updated_bytes} bytes; maximum {MAX_EDIT_FILE_BYTES} bytes)"
        ));
    }
    let updated = contents.replacen(&*old, &new, 1);
    let relative_path = path
        .strip_prefix(cwd)
        .unwrap_or(&path)
        .display()
        .to_string();
    Ok(PlannedEdit {
        path,
        relative_path,
        contents,
        updated,
    })
}

fn parse_edit_args(request: &ToolRequest) -> Result<(String, String, String), String> {
    if let Some(raw) = request.raw_arguments.as_deref() {
        let args: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("invalid edit arguments: {e}"))?;
        let path = args["path"]
            .as_str()
            .ok_or("edit requires 'path' argument")?
            .to_string();
        let old_text = args["old_text"]
            .as_str()
            .ok_or("edit requires 'old_text' argument")?
            .to_string();
        let new_text = args["new_text"]
            .as_str()
            .ok_or("edit requires 'new_text' argument")?
            .to_string();
        return Ok((path, old_text, new_text));
    }

    let spec = request.target.as_deref().ok_or("edit spec is required")?;
    let (path_part, replacement_part) = spec
        .split_once("::")
        .ok_or("edit spec must be: <path> :: <old> => <new>")?;
    let (old, new) = replacement_part
        .split_once("=>")
        .ok_or("edit replacement must be: <old> => <new>")?;

    Ok((
        path_part.trim().to_string(),
        old.trim().to_string(),
        new.trim().to_string(),
    ))
}

fn is_inside_workspace(cwd: &Path, path: &Path) -> bool {
    let Ok(workspace) = fs::canonicalize(cwd) else {
        return false;
    };
    let parent = path.parent().unwrap_or(path);
    let Ok(parent) = fs::canonicalize(parent) else {
        return false;
    };

    parent.starts_with(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{FileChangePreview, ToolName, ToolRequest, ToolStatus};
    use std::fs;

    fn make_request(target: Option<&str>, raw_arguments: Option<&str>) -> ToolRequest {
        ToolRequest {
            id: "test-edit".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: target.map(|s| s.to_string()),
            raw_arguments: raw_arguments.map(|s| s.to_string()),
        }
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orca-edit-test-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn successful_edit_via_raw_arguments() {
        let dir = temp_dir("raw-args");
        let file = dir.join("test.txt");
        fs::write(&file, "hello world\n").unwrap();

        let raw = r#"{"path":"test.txt","old_text":"hello","new_text":"hi"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(fs::read_to_string(&file).unwrap(), "hi world\n");
    }

    #[test]
    fn successful_edit_via_dsl_target() {
        let dir = temp_dir("dsl");
        let file = dir.join("note.txt");
        fs::write(&file, "foo bar baz\n").unwrap();

        let req = make_request(Some("note.txt :: foo => qux"), None);
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(fs::read_to_string(&file).unwrap(), "qux bar baz\n");
    }

    #[test]
    fn fails_when_old_text_not_found() {
        let dir = temp_dir("not-found");
        let file = dir.join("test.txt");
        fs::write(&file, "hello world\n").unwrap();

        let raw = r#"{"path":"test.txt","old_text":"missing","new_text":"x"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.as_deref().unwrap().contains("not found"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello world\n");
    }

    #[test]
    fn fails_when_old_text_matches_multiple() {
        let dir = temp_dir("multi-match");
        let file = dir.join("test.txt");
        fs::write(&file, "aaa\naaa\n").unwrap();

        let raw = r#"{"path":"test.txt","old_text":"aaa","new_text":"bbb"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.as_deref().unwrap().contains("multiple"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "aaa\naaa\n");
    }

    #[test]
    fn fails_when_old_text_is_empty() {
        let dir = temp_dir("empty-old");
        let file = dir.join("test.txt");
        fs::write(&file, "content\n").unwrap();

        let raw = r#"{"path":"test.txt","old_text":"","new_text":"x"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.as_deref().unwrap().contains("empty"));
    }

    #[test]
    fn fails_when_file_does_not_exist() {
        let dir = temp_dir("no-file");

        let raw = r#"{"path":"nonexistent.txt","old_text":"x","new_text":"y"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.as_deref().unwrap().contains("failed to read"));
    }

    #[test]
    fn fails_with_invalid_json_arguments() {
        let dir = temp_dir("bad-json");

        let req = make_request(None, Some("not json"));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("invalid edit arguments")
        );
    }

    #[test]
    fn raw_arguments_takes_precedence_over_target() {
        let dir = temp_dir("precedence");
        let file = dir.join("a.txt");
        fs::write(&file, "old content\n").unwrap();

        let raw = r#"{"path":"a.txt","old_text":"old","new_text":"new"}"#;
        let req = ToolRequest {
            id: "test".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some("a.txt :: something => else".to_string()),
            raw_arguments: Some(raw.to_string()),
        };
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(fs::read_to_string(&file).unwrap(), "new content\n");
    }

    #[test]
    fn oversized_file_is_rejected_before_exact_match_scan() {
        const EXPECTED_EDIT_LIMIT_BYTES: u64 = 16 * 1024 * 1024;

        let dir = temp_dir("oversized");
        let file = dir.join("large.txt");
        let handle = fs::File::create(&file).expect("create sparse fixture");
        handle
            .set_len(EXPECTED_EDIT_LIMIT_BYTES + 1)
            .expect("size sparse fixture");
        let raw = r#"{"path":"large.txt","old_text":"missing","new_text":"replacement"}"#;
        let req = make_request(None, Some(raw));

        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("too large")),
            "unexpected error: {:?}",
            result.error
        );
        assert_eq!(
            fs::metadata(file)
                .expect("oversized fixture metadata")
                .len(),
            EXPECTED_EDIT_LIMIT_BYTES + 1
        );
    }

    #[test]
    fn preview_shows_the_diff_an_edit_would_make_without_writing() {
        let dir = temp_dir("dry-run");
        let file = dir.join("pages.py");
        fs::write(&file, "return total // per_page\n").expect("write fixture");
        let raw = r#"{"path":"pages.py","old_text":"total // per_page","new_text":"-(-total // per_page)"}"#;
        let req = make_request(None, Some(raw));

        let preview = preview(&req, &dir).expect("a valid edit has a preview");

        let FileChangePreview::UnifiedDiff { text, .. } = preview else {
            panic!("a small edit renders a unified diff");
        };
        assert!(text.contains("-return total // per_page"));
        assert!(text.contains("+return -(-total // per_page)"));
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "return total // per_page\n",
            "a preview must not change the file"
        );
    }

    #[test]
    fn preview_of_an_edit_that_cannot_apply_is_none() {
        let dir = temp_dir("dry-run-miss");
        fs::write(dir.join("pages.py"), "a\n").expect("write fixture");
        let raw = r#"{"path":"pages.py","old_text":"missing","new_text":"b"}"#;

        assert!(preview(&make_request(None, Some(raw)), &dir).is_none());
    }

    #[test]
    fn successful_edit_emits_committed_file_change_preview() {
        let dir = temp_dir("preview");
        let file = dir.join("preview.txt");
        fs::write(&file, "old\nsame\n").expect("write preview fixture");
        let raw = r#"{"path":"preview.txt","old_text":"old","new_text":"new"}"#;
        let req = make_request(None, Some(raw));

        let result = execute(&req, &dir);

        let preview = result.file_change_preview.expect("successful edit preview");
        let FileChangePreview::UnifiedDiff { text, truncated } = preview.as_ref() else {
            panic!("small edit should render a unified diff");
        };
        assert!(!*truncated);
        assert!(text.contains("-old"));
        assert!(text.contains("+new"));
    }
}
