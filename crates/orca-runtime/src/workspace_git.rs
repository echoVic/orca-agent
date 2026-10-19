use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Keep prompt and provider tool exposure stable for each cwd for the life of the process.
pub(crate) fn is_git_work_tree(cwd: &Path) -> bool {
    static WORKSPACES: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cwd = cwd
        .canonicalize()
        .or_else(|_| std::path::absolute(cwd))
        .unwrap_or_else(|_| cwd.to_path_buf());
    let mut workspaces = WORKSPACES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *workspaces
        .entry(cwd.clone())
        .or_insert_with(|| detect_git_work_tree(&cwd, std::env::var_os("GIT_DIR").is_some(), None))
}

fn detect_git_work_tree(cwd: &Path, git_dir_set: bool, ceiling: Option<&Path>) -> bool {
    if git_dir_set {
        return true;
    }
    for directory in cwd.ancestors() {
        if std::fs::metadata(directory.join(".git"))
            .is_ok_and(|entry| entry.is_dir() || entry.is_file())
        {
            return true;
        }
        if Some(directory) == ceiling {
            break;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_git_directory_at_workspace_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();

        assert!(detect_git_work_tree(root.path(), false, Some(root.path())));
    }

    #[test]
    fn detects_git_file_for_worktrees_and_submodules() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".git"), "gitdir: /unused/worktree\n").unwrap();

        assert!(detect_git_work_tree(root.path(), false, Some(root.path())));
    }

    #[test]
    fn detects_git_directory_in_cwd_ancestor() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let cwd = root.path().join("src/nested");
        std::fs::create_dir_all(&cwd).unwrap();

        assert!(detect_git_work_tree(&cwd, false, Some(root.path())));
    }

    #[test]
    fn detects_non_repo_without_walking_above_fixture_ceiling() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let ceiling = root.path().join("non-repo");
        let cwd = ceiling.join("nested");
        std::fs::create_dir_all(&cwd).unwrap();

        assert!(!detect_git_work_tree(&cwd, false, Some(&ceiling)));
    }

    #[test]
    fn detects_repository_when_git_dir_is_set() {
        let root = tempfile::tempdir().unwrap();

        assert!(detect_git_work_tree(root.path(), true, Some(root.path())));
    }

    #[test]
    fn git_work_tree_decision_is_memoized_by_canonical_cwd() {
        let non_repo = tempfile::tempdir().unwrap();
        assert!(
            non_repo
                .path()
                .ancestors()
                .all(|dir| !dir.join(".git").exists())
        );
        assert!(!is_git_work_tree(non_repo.path()));
        std::fs::create_dir(non_repo.path().join(".git")).unwrap();
        assert!(!is_git_work_tree(&non_repo.path().join(".")));

        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        assert!(is_git_work_tree(repo.path()));
        std::fs::remove_dir(repo.path().join(".git")).unwrap();
        assert!(is_git_work_tree(&repo.path().join(".")));
    }
}
