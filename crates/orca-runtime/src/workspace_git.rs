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
    *workspaces.entry(cwd.clone()).or_insert_with(|| {
        let git_dir = std::env::var_os("GIT_DIR");
        detect_git_work_tree(&cwd, git_dir.as_deref().map(Path::new), None)
    })
}

fn detect_git_work_tree(cwd: &Path, git_dir: Option<&Path>, ceiling: Option<&Path>) -> bool {
    if git_dir.is_some_and(|path| !path.as_os_str().is_empty() && cwd.join(path).is_dir()) {
        return true;
    }
    for directory in cwd.ancestors() {
        let marker = directory.join(".git");
        if marker.is_dir() && marker.join("HEAD").is_file() {
            return true;
        }
        if marker.is_file()
            && std::fs::read_to_string(&marker).is_ok_and(|content| {
                content.strip_prefix("gitdir:").is_some_and(|path| {
                    let path = path.trim();
                    !path.is_empty() && directory.join(path).is_dir()
                })
            })
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
        std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        assert!(detect_git_work_tree(root.path(), None, Some(root.path())));
    }

    #[test]
    fn detects_git_file_for_worktrees_and_submodules() {
        let root = tempfile::tempdir().unwrap();
        let git_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".git"),
            format!("gitdir: {}\n", git_dir.path().display()),
        )
        .unwrap();

        assert!(detect_git_work_tree(root.path(), None, Some(root.path())));
    }

    #[test]
    fn detects_git_directory_in_cwd_ancestor() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let cwd = root.path().join("src/nested");
        std::fs::create_dir_all(&cwd).unwrap();

        assert!(detect_git_work_tree(&cwd, None, Some(root.path())));
    }

    #[test]
    fn detects_non_repo_without_walking_above_fixture_ceiling() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let ceiling = root.path().join("non-repo");
        let cwd = ceiling.join("nested");
        std::fs::create_dir_all(&cwd).unwrap();

        assert!(!detect_git_work_tree(&cwd, None, Some(&ceiling)));
    }

    #[test]
    fn detects_repository_when_git_dir_is_set() {
        let root = tempfile::tempdir().unwrap();

        assert!(detect_git_work_tree(
            root.path(),
            Some(root.path()),
            Some(root.path())
        ));
    }

    #[test]
    fn detects_relative_git_dir_from_cwd() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(root.path().join("metadata")).unwrap();

        assert!(detect_git_work_tree(
            &cwd,
            Some(Path::new("../metadata")),
            Some(root.path())
        ));
    }

    #[test]
    fn ignores_invalid_git_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "metadata").unwrap();
        for git_dir in [Path::new("missing"), Path::new("file"), Path::new("")] {
            assert!(!detect_git_work_tree(
                root.path(),
                Some(git_dir),
                Some(root.path())
            ));
        }
        assert!(!detect_git_work_tree(
            root.path(),
            Some(&root.path().join("missing")),
            Some(root.path())
        ));
    }

    #[test]
    fn invalid_git_dir_keeps_walking_for_repository_markers() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let cwd = root.path().join("nested");
        std::fs::create_dir(&cwd).unwrap();

        assert!(detect_git_work_tree(
            &cwd,
            Some(Path::new("missing")),
            Some(root.path())
        ));
    }

    #[test]
    fn detects_git_file_with_relative_gitdir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("metadata")).unwrap();
        let cwd = root.path().join("worktree/nested");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.path().join("worktree/.git"), "gitdir: ../metadata\n").unwrap();

        assert!(detect_git_work_tree(&cwd, None, Some(root.path())));
    }

    #[test]
    fn rejects_invalid_git_markers() {
        let root = tempfile::tempdir().unwrap();
        let empty_dir = root.path().join("empty-dir");
        std::fs::create_dir_all(empty_dir.join(".git")).unwrap();
        let head_dir = root.path().join("head-dir");
        std::fs::create_dir_all(head_dir.join(".git/HEAD")).unwrap();
        for cwd in [&empty_dir, &head_dir] {
            assert!(
                !detect_git_work_tree(cwd, None, Some(root.path())),
                "{cwd:?}"
            );
        }
        std::fs::create_dir(root.path().join("metadata")).unwrap();
        std::fs::write(root.path().join("not-a-directory"), "metadata").unwrap();
        for content in [
            "gitdir: missing\n",
            "../metadata\n",
            "garbage\n",
            "gitdir:\n",
            "gitdir: ../not-a-directory\n",
        ] {
            let cwd = tempfile::tempdir_in(root.path()).unwrap();
            std::fs::write(cwd.path().join(".git"), content).unwrap();
            assert!(
                !detect_git_work_tree(cwd.path(), None, Some(root.path())),
                "{content:?}"
            );
        }
    }

    #[test]
    fn ignores_invalid_markers_and_keeps_walking() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let cwd = root.path().join("nested");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        assert!(detect_git_work_tree(&cwd, None, Some(root.path())));
        std::fs::remove_dir(cwd.join(".git")).unwrap();
        std::fs::write(cwd.join(".git"), "gitdir: nowhere\n").unwrap();
        assert!(detect_git_work_tree(&cwd, None, Some(root.path())));
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
        std::fs::write(non_repo.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(!is_git_work_tree(&non_repo.path().join(".")));

        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(is_git_work_tree(repo.path()));
        std::fs::remove_file(repo.path().join(".git/HEAD")).unwrap();
        std::fs::remove_dir(repo.path().join(".git")).unwrap();
        assert!(is_git_work_tree(&repo.path().join(".")));
    }
}
