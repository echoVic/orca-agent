use orca_core::tool_types::ToolResult;

pub fn render(result: &ToolResult) -> Option<String> {
    result
        .file_change_preview
        .as_deref()
        .map(|preview| preview.display_text())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolRequest};

    use super::*;

    #[test]
    fn renders_committed_preview_without_rereading_later_workspace_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "old\nsame\n").unwrap();
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(
                r#"{"path":"notes.txt","old_text":"old","new_text":"committed"}"#.to_string(),
            ),
        };
        let result = orca_tools::edit::execute(&request, dir.path());
        fs::write(&path, "external\nsame\n").unwrap();

        let rendered = render(&result).expect("committed edit preview");

        assert!(rendered.contains("--- a/notes.txt"));
        assert!(rendered.contains("+++ b/notes.txt"));
        assert!(rendered.contains("-old"));
        assert!(rendered.contains("+committed"));
        assert!(!rendered.contains("external"));
    }
}
