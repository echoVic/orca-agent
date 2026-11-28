use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};
use serde::Deserialize;

use crate::{
    ResolvedWorkspacePath, resolve_workspace_read_path,
    workspace_read::{listing_entry, open_directory, open_root},
};

#[derive(Deserialize)]
struct GlobArgs {
    pattern: Option<String>,
    query: Option<String>,
    mode: Option<GlobMode>,
    path: Option<String>,
    head_limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum GlobMode {
    Glob,
    Fuzzy,
}

const MAX_FUZZY_VISITS: usize = 10_000;
const MAX_FUZZY_MATCHES: usize = 200;
const DEFAULT_GLOB_HEAD_LIMIT: usize = 500;

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    execute_or_cancel(request, cwd, max_bytes, || false)
}

/// Like [`execute`], but stops walking once `should_cancel` says so.
pub fn execute_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let should_cancel = &should_cancel as &dyn Fn() -> bool;
    let cancelled = || ToolResult::cancelled(request, "glob was cancelled", None);
    let args = match parse_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let base = args.path.as_deref().unwrap_or(".");
    let resolved = match resolve_workspace_read_path(cwd, Some(base)) {
        Ok(path) => path,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let Some(is_directory) = listing_entry(&resolved.root, &resolved.path) else {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    };

    if matches!(args.mode, Some(GlobMode::Fuzzy)) {
        return execute_fuzzy(request, cwd, &resolved, &args, max_bytes, should_cancel)
            .unwrap_or_else(cancelled);
    }

    let pattern = args
        .pattern
        .as_deref()
        .expect("glob mode requires parsed pattern");
    let matcher = match build_matcher(pattern) {
        Ok(matcher) => matcher,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let mut matches = if is_directory {
        match match_tree(cwd, &resolved, &matcher, should_cancel) {
            Some(matches) => matches,
            None => return cancelled(),
        }
    } else {
        match_file(cwd, &resolved.path, &matcher)
    };

    matches.sort();
    matches.dedup();
    if matches.is_empty() {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }

    let output = match paginate_output(&resolved, matches, &args) {
        Ok(output) => output,
        Err(error) => return ToolResult::failed(request, error.to_string(), None),
    };
    let (output, truncated) = truncate_output(output, max_bytes);
    ToolResult::completed_kind(
        request,
        output,
        truncated,
        if truncated {
            ToolResultKind::Truncated
        } else {
            ToolResultKind::Success
        },
    )
}

fn parse_args(request: &ToolRequest) -> Result<GlobArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "missing glob arguments JSON".to_string())?;
    let args: GlobArgs = serde_json::from_str(raw)
        .map_err(|error| format!("invalid glob arguments JSON: {error}"))?;
    let mode = args.mode.unwrap_or(GlobMode::Glob);
    match mode {
        GlobMode::Glob => {
            if args
                .pattern
                .as_deref()
                .is_none_or(|pattern| pattern.trim().is_empty())
            {
                return Err("missing required glob argument: pattern".to_string());
            }
        }
        GlobMode::Fuzzy => {
            if args
                .query
                .as_deref()
                .is_none_or(|query| query.trim().is_empty())
            {
                return Err("missing required glob fuzzy argument: query".to_string());
            }
        }
    }
    Ok(args)
}

fn execute_fuzzy(
    request: &ToolRequest,
    cwd: &Path,
    resolved: &ResolvedWorkspacePath,
    args: &GlobArgs,
    max_bytes: usize,
    should_cancel: &dyn Fn() -> bool,
) -> Option<ToolResult> {
    let query = args
        .query
        .as_deref()
        .expect("fuzzy mode requires parsed query")
        .trim();
    let mut matches = Vec::new();
    let mut visited = 0;
    walk_listing(resolved, true, should_cancel, |relative, is_directory| {
        let path = resolved.path.join(relative);
        let candidate = relative_to_workspace(cwd, &path);
        let candidate = if is_directory {
            format!("{candidate}/")
        } else {
            candidate
        };
        if let Some(score) = fuzzy_score(&candidate, query) {
            matches.push((score, candidate, path));
        }
        visited += 1;
        visited < MAX_FUZZY_VISITS
    })?;
    matches.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.len().cmp(&right.1.len()))
            .then_with(|| left.1.cmp(&right.1))
    });
    matches.dedup_by(|left, right| left.1 == right.1);
    let matches = matches
        .into_iter()
        .take(MAX_FUZZY_MATCHES)
        .map(|(_, label, path)| (label, path))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Some(ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        ));
    }
    let output = match paginate_output(resolved, matches, args) {
        Ok(output) => output,
        Err(error) => return Some(ToolResult::failed(request, error.to_string(), None)),
    };
    let (output, truncated) = truncate_output(output, max_bytes);
    Some(ToolResult::completed_kind(
        request,
        output,
        truncated,
        if truncated {
            ToolResultKind::Truncated
        } else {
            ToolResultKind::Success
        },
    ))
}

fn paginate_output(
    resolved: &ResolvedWorkspacePath,
    lines: Vec<(String, PathBuf)>,
    args: &GlobArgs,
) -> std::io::Result<String> {
    let total = lines.len();
    let offset = args.offset.unwrap_or(0);
    let limit = match args.head_limit {
        Some(0) => usize::MAX,
        Some(limit) => limit,
        None => DEFAULT_GLOB_HEAD_LIMIT,
    };
    let next_offset = (total.saturating_sub(offset) > limit).then(|| offset + limit);

    #[cfg(all(test, unix))]
    crate::workspace_read::listing_test::before_emission();
    open_root(&resolved.root)?;
    let mut output = lines
        .into_iter()
        .skip(offset)
        .take(limit)
        .filter_map(|(label, path)| listing_entry(&resolved.root, &path).map(|_| label))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(next_offset) = next_offset {
        let notice = if offset == 0 {
            format!("[Showing first {limit} results; use offset={next_offset} to continue]")
        } else {
            let end = next_offset.min(total);
            format!(
                "[Showing results {}-{} of {total}; use offset={next_offset} to continue]",
                offset + 1,
                end
            )
        };
        if output.is_empty() {
            output = notice;
        } else {
            output.push('\n');
            output.push_str(&notice);
        }
    }
    Ok(output)
}

fn build_matcher(pattern: &str) -> Result<globset::GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    let glob = Glob::new(pattern).map_err(|error| format!("invalid glob pattern: {error}"))?;
    builder.add(glob);
    builder
        .build()
        .map_err(|error| format!("invalid glob pattern: {error}"))
}

/// The paths under `search_root` that match, or `None` when told to stop.
fn match_tree(
    cwd: &Path,
    resolved: &ResolvedWorkspacePath,
    matcher: &globset::GlobSet,
    should_cancel: &dyn Fn() -> bool,
) -> Option<Vec<(String, PathBuf)>> {
    let mut matches = Vec::new();
    walk_listing(resolved, false, should_cancel, |relative, _| {
        if matcher.is_match(relative) {
            let path = resolved.path.join(relative);
            matches.push((relative_to_workspace(cwd, &path), path));
        }
        true
    })?;
    Some(matches)
}

fn match_file(cwd: &Path, path: &Path, matcher: &globset::GlobSet) -> Vec<(String, PathBuf)> {
    let name = path.file_name().map(PathBuf::from).unwrap_or_default();
    if matcher.is_match(name) {
        vec![(relative_to_workspace(cwd, path), path.to_owned())]
    } else {
        Vec::new()
    }
}

fn walk_listing(
    resolved: &ResolvedWorkspacePath,
    skip_hidden: bool,
    should_cancel: &dyn Fn() -> bool,
    mut visit: impl FnMut(&Path, bool) -> bool,
) -> Option<()> {
    let mut directories = vec![PathBuf::new()];
    while let Some(relative) = directories.pop() {
        if should_cancel() {
            return None;
        }
        let directory = resolved.canonical.join(&relative);
        let Ok(names) = open_directory(&resolved.root, &directory).and_then(|dir| dir.read_names())
        else {
            continue;
        };
        for name in names {
            if should_cancel() {
                return None;
            }
            if skip_hidden && name.as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let child = relative.join(name);
            let path = resolved.path.join(&child);
            let Some(is_directory) = listing_entry(&resolved.root, &path) else {
                continue;
            };
            if !visit(&child, is_directory) {
                return Some(());
            }
            if is_directory {
                // A directory replaced with a link will fail the next verified open.
                directories.push(child);
            }
        }
    }
    Some(())
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    let candidate_lower = candidate.to_lowercase();
    let query_lower = query.to_lowercase();
    if candidate_lower.contains(&query_lower) {
        return candidate_lower.find(&query_lower);
    }
    subsequence_score(&candidate_lower, &query_lower)
}

fn subsequence_score(candidate: &str, query: &str) -> Option<usize> {
    let mut score = 0;
    let mut last_match = 0;
    let mut chars = candidate.char_indices();
    for query_char in query.chars() {
        let mut matched = None;
        for (index, candidate_char) in chars.by_ref() {
            if candidate_char == query_char {
                matched = Some(index);
                break;
            }
        }
        let index = matched?;
        score += index.saturating_sub(last_match);
        last_match = index + query_char.len_utf8();
    }
    Some(score)
}

fn relative_to_workspace(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::workspace_tempdir as tempdir;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolResultKind, ToolStatus};

    use super::*;

    #[cfg(unix)]
    #[test]
    fn glob_skips_entries_whose_real_location_is_outside() {
        let workspace = tempdir().unwrap();
        let neighbor = tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let linked = root.join("linked");
        fs::write(neighbor.path().join("notes.txt"), "outside listing").unwrap();
        crate::tests::symlink(neighbor.path(), &linked).unwrap();

        assert_eq!(listing_entry(&root, &linked.join("notes.txt")), None);
    }

    #[cfg(unix)]
    #[test]
    fn glob_rechecks_selected_entries_before_emission() {
        let workspace = tempdir().unwrap();
        let neighbor = tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("notes.txt"), "original").unwrap();
        fs::write(root.join("keep.txt"), "inside").unwrap();
        fs::write(neighbor.path().join("notes.txt"), "outside").unwrap();
        let outside = neighbor.path().to_owned();
        let old = root.join("old");
        crate::workspace_read::listing_test::BEFORE_EMISSION.with(|hook| {
            hook.replace(Some(Box::new(move || {
                fs::rename(&sub, old).unwrap();
                crate::tests::symlink(outside, sub).unwrap();
            })));
        });
        let request = ToolRequest {
            id: "glob-page".into(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(r#"{"pattern":"**/*.txt","head_limit":2}"#.into()),
        };
        let result = execute(&request, &root, 4096);
        assert_eq!(result.output.as_deref(), Some("keep.txt"));
    }

    #[test]
    fn a_walk_told_to_stop_is_cancelled() {
        let cwd = tempdir().expect("temp workspace");
        fs::write(cwd.path().join("a.txt"), "hello").expect("write match");
        for arguments in [
            serde_json::json!({ "pattern": "*.txt" }),
            serde_json::json!({ "mode": "fuzzy", "query": "a" }),
        ] {
            let request = ToolRequest {
                id: "glob-cancel".to_string(),
                name: ToolName::Glob,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(arguments.to_string()),
            };

            let result = execute_or_cancel(&request, cwd.path(), 100_000, || true);

            assert_eq!(result.status, ToolStatus::Cancelled, "{arguments}");
            assert_eq!(result.error.as_deref(), Some("glob was cancelled"));
        }
    }

    #[test]
    fn glob_defaults_to_first_500_results() {
        let cwd = tempdir().expect("temp workspace");
        let files_dir = cwd.path().join("files");
        fs::create_dir_all(&files_dir).expect("create files dir");
        for index in 0..520 {
            fs::write(files_dir.join(format!("{index:03}.txt")), "hello").expect("write match");
        }
        let request = ToolRequest {
            id: "glob-page".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "pattern": "*.txt",
                    "path": "files"
                })
                .to_string(),
            ),
        };

        let result = execute(&request, cwd.path(), 100_000);
        let output = result.output.as_deref().expect("glob output");
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(lines.len(), 501);
        assert_eq!(lines[0], "files/000.txt");
        assert_eq!(lines[499], "files/499.txt");
        assert_eq!(
            lines[500],
            "[Showing first 500 results; use offset=500 to continue]"
        );
    }

    #[test]
    fn glob_respects_explicit_offset_and_head_limit() {
        let cwd = tempdir().expect("temp workspace");
        let files_dir = cwd.path().join("files");
        fs::create_dir_all(&files_dir).expect("create files dir");
        for index in 0..520 {
            fs::write(files_dir.join(format!("{index:03}.txt")), "hello").expect("write match");
        }
        let request = ToolRequest {
            id: "glob-page".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "pattern": "*.txt",
                    "path": "files",
                    "head_limit": 10,
                    "offset": 500
                })
                .to_string(),
            ),
        };

        let result = execute(&request, cwd.path(), 100_000);
        let output = result.output.as_deref().expect("glob output");
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[0], "files/500.txt");
        assert_eq!(lines[9], "files/509.txt");
        assert_eq!(
            lines[10],
            "[Showing results 501-510 of 520; use offset=510 to continue]"
        );
    }

    #[test]
    fn fuzzy_mode_returns_ranked_relative_matches() {
        let cwd = tempdir().expect("temp workspace");
        fs::create_dir_all(cwd.path().join("src/runtime/config")).expect("create nested dir");
        fs::create_dir_all(cwd.path().join("src/render")).expect("create alternate dir");
        fs::write(cwd.path().join("src/runtime/config/mod.rs"), "hello").expect("write match");
        fs::write(cwd.path().join("src/render/component.rs"), "hello").expect("write alternate");
        let request = ToolRequest {
            id: "glob-fuzzy".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "mode": "fuzzy",
                    "query": "rcm"
                })
                .to_string(),
            ),
        };

        let result = execute(&request, cwd.path(), 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, ToolResultKind::Success);
        let output = result.output.expect("fuzzy output");
        assert!(
            output
                .lines()
                .any(|line| line == "src/runtime/config/mod.rs"),
            "expected fuzzy query to match path initials, got {output}"
        );
    }

    #[test]
    fn fuzzy_mode_scopes_search_path() {
        let cwd = tempdir().expect("temp workspace");
        fs::create_dir_all(cwd.path().join("src/runtime/config")).expect("create runtime dir");
        fs::create_dir_all(cwd.path().join("docs/runtime/config")).expect("create docs dir");
        fs::write(cwd.path().join("src/runtime/config/mod.rs"), "hello").expect("write src");
        fs::write(cwd.path().join("docs/runtime/config/mod.md"), "hello").expect("write docs");
        let request = ToolRequest {
            id: "glob-fuzzy-path".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "mode": "fuzzy",
                    "query": "rcm",
                    "path": "docs"
                })
                .to_string(),
            ),
        };

        let result = execute(&request, cwd.path(), 4096);

        assert_eq!(result.status, ToolStatus::Completed);
        let output = result.output.expect("fuzzy output");
        assert!(
            output
                .lines()
                .any(|line| line == "docs/runtime/config/mod.md")
        );
        assert!(!output.lines().any(|line| line.starts_with("src/")));
    }
}
