use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxDenialDiagnostic {
    pub denied_path: Option<PathBuf>,
    pub suggested_write_root: Option<PathBuf>,
    pub message: String,
}

pub fn diagnose_sandbox_denial(
    cwd: &Path,
    stdout: &str,
    stderr: &str,
) -> Option<SandboxDenialDiagnostic> {
    let combined = if stdout.is_empty() {
        stderr.to_string()
    } else if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };
    if !is_likely_sandbox_denied(&combined) {
        return None;
    }

    let denied_path = extract_denied_path(&combined);
    let suggested_write_root = denied_path.as_deref().and_then(suggest_write_root);
    let message = build_message(cwd, denied_path.as_deref(), suggested_write_root.as_deref());

    Some(SandboxDenialDiagnostic {
        denied_path,
        suggested_write_root,
        message,
    })
}

fn is_likely_sandbox_denied(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("operation not permitted")
        || lower.contains("permission denied")
        || lower.contains("read-only file system")
}

fn extract_denied_path(text: &str) -> Option<PathBuf> {
    extract_single_quoted_absolute_path(text)
        .or_else(|| extract_double_quoted_absolute_path(text))
        .or_else(|| extract_prefixed_path(text, "touch: "))
        .or_else(|| extract_prefixed_path(text, "mkdir: "))
        .or_else(|| extract_colon_separated_absolute_path(text))
}

fn extract_single_quoted_absolute_path(text: &str) -> Option<PathBuf> {
    extract_quoted_absolute_path(text, '\'')
}

fn extract_double_quoted_absolute_path(text: &str) -> Option<PathBuf> {
    extract_quoted_absolute_path(text, '"')
}

fn extract_quoted_absolute_path(text: &str, quote: char) -> Option<PathBuf> {
    let mut remaining = text;
    while let Some(start) = remaining.find(quote) {
        let after_start = &remaining[start + quote.len_utf8()..];
        let Some(end) = after_start.find(quote) else {
            return None;
        };
        let candidate = &after_start[..end];
        if candidate.starts_with('/') {
            return Some(PathBuf::from(candidate));
        }
        remaining = &after_start[end + quote.len_utf8()..];
    }
    None
}

fn extract_prefixed_path(text: &str, prefix: &str) -> Option<PathBuf> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(prefix)?;
        let path = rest
            .split_once(": ")
            .map(|(path, _)| path)
            .unwrap_or(rest)
            .trim();
        path.starts_with('/').then(|| PathBuf::from(path))
    })
}

fn extract_colon_separated_absolute_path(text: &str) -> Option<PathBuf> {
    text.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("operation not permitted")
            && !lower.contains("permission denied")
            && !lower.contains("read-only file system")
        {
            return None;
        }
        let parts = line.split(": ").collect::<Vec<_>>();
        parts
            .iter()
            .rev()
            .map(|part| part.trim_matches(|ch| ch == '\'' || ch == '"' || ch == ' '))
            .find(|part| part.starts_with('/'))
            .map(PathBuf::from)
    })
}

fn suggest_write_root(path: &Path) -> Option<PathBuf> {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect::<Vec<_>>();
    if let Some(git_index) = components.iter().position(|component| component == ".git") {
        let mut root = PathBuf::new();
        for component in components.iter().take(git_index + 1) {
            root.push(component);
        }
        return Some(root);
    }
    path.parent().map(Path::to_path_buf)
}

fn build_message(cwd: &Path, denied_path: Option<&Path>, suggested_root: Option<&Path>) -> String {
    let mut parts = Vec::new();
    parts.push("sandbox denied filesystem access".to_string());

    if let Some(path) = denied_path {
        parts.push(format!("denied path: {}", path.display()));
    }
    if let Some(root) = suggested_root {
        parts.push(format!("suggested write root: {}", root.display()));
    }
    if let Some(path) = denied_path
        && path
            .components()
            .any(|component| component.as_os_str() == ".git")
    {
        parts.push(
            "git index.lock errors with Operation not permitted are usually not a stale git lock"
                .to_string(),
        );
        if let Some(repo_root) = git_repo_root_from_denied_path(path)
            && cwd.starts_with(&repo_root)
            && cwd != repo_root
        {
            parts.push(format!(
                "cwd {} is nested under repo root {}, so the workspace root may not include .git",
                cwd.display(),
                repo_root.display()
            ));
        }
    }
    parts.join("; ")
}

fn git_repo_root_from_denied_path(path: &Path) -> Option<PathBuf> {
    let mut root = PathBuf::new();
    for component in path.components() {
        if component.as_os_str() == ".git" {
            return Some(root);
        }
        root.push(component);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnoses_git_index_lock_as_sandbox_denial_not_stale_lock() {
        let cwd = Path::new("/repo/web");
        let stderr = "fatal: Unable to create '/repo/.git/index.lock': Operation not permitted";

        let diagnostic = diagnose_sandbox_denial(cwd, "", stderr).expect("diagnostic");

        assert_eq!(
            diagnostic.denied_path,
            Some(PathBuf::from("/repo/.git/index.lock"))
        );
        assert_eq!(
            diagnostic.suggested_write_root,
            Some(PathBuf::from("/repo/.git"))
        );
        assert!(diagnostic.message.contains("sandbox"));
        assert!(diagnostic.message.contains("not a stale git lock"));
        assert!(diagnostic.message.contains("/repo"));
        assert!(diagnostic.message.contains("/repo/web"));
    }

    #[test]
    fn forged_process_output_remains_explanatory_only() {
        let cwd = Path::new("/repo/web");
        let diagnostic = diagnose_sandbox_denial(
            cwd,
            "Unable to create '/etc/sensitive': Operation not permitted",
            "",
        )
        .expect("diagnostic");

        assert_eq!(
            diagnostic.denied_path,
            Some(PathBuf::from("/etc/sensitive"))
        );
        // No authority-bearing receipt is produced by this parser. Callers
        // must obtain a SandboxDenialReceipt directly from the backend.
        assert!(diagnostic.message.contains("sandbox denied"));
    }
}
