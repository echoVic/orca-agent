use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const WORKTREE_GIT_TIMEOUT: Duration = Duration::from_secs(120);
const WORKTREE_GIT_RETAINED_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeOutcome {
    pub path: PathBuf,
    pub preserved: bool,
}

#[derive(Debug)]
pub struct WorktreeGuard {
    repo_root: PathBuf,
    path: PathBuf,
}

impl WorktreeGuard {
    pub fn create(cwd: &Path) -> io::Result<Self> {
        let repo_root = git_output(cwd, &["rev-parse", "--show-toplevel"])?;
        let repo_root = PathBuf::from(repo_root.trim());
        let base = repo_root.join(".orca").join("worktrees");
        fs::create_dir_all(&base)?;
        let path = base.join(format!("subagent-{}", uuid::Uuid::new_v4()));
        let path_text = path.display().to_string();
        git_status(
            &repo_root,
            &["worktree", "add", "--detach", &path_text, "HEAD"],
        )?;
        Ok(Self { repo_root, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn finish(self) -> io::Result<WorktreeOutcome> {
        let Self { repo_root, path } = self;
        Self::finish_existing(repo_root, path)
    }

    pub fn finish_existing(repo_root: PathBuf, path: PathBuf) -> io::Result<WorktreeOutcome> {
        let status = git_output(&path, &["status", "--short"])?;
        let dirty = !status.trim().is_empty();
        if !dirty {
            let path_text = path.display().to_string();
            git_status(&repo_root, &["worktree", "remove", "--force", &path_text])?;
        }
        Ok(WorktreeOutcome {
            path,
            preserved: dirty,
        })
    }
}

fn git_output(cwd: &Path, args: &[&str]) -> io::Result<String> {
    let output = run_git(cwd, args)?;
    if output.status.success() && !output.timed_out {
        Ok(output.stdout_text())
    } else {
        Err(git_error(args, &output))
    }
}

fn git_status(cwd: &Path, args: &[&str]) -> io::Result<()> {
    let output = run_git(cwd, args)?;
    if output.status.success() && !output.timed_out {
        Ok(())
    } else {
        Err(git_error(args, &output))
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> io::Result<orca_tools::process::CommandOutput> {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    orca_tools::process::prepare_non_interactive_command(&mut command);
    let (child, process_job, _receipt) =
        orca_tools::process::spawn_user_trusted(command, "worktree:git", cwd)?;
    orca_tools::process::wait_for_child_output_with_timeout_or_cancel_and_limit(
        child,
        process_job,
        WORKTREE_GIT_TIMEOUT,
        || false,
        WORKTREE_GIT_RETAINED_BYTES,
    )
}

fn git_error(args: &[&str], output: &orca_tools::process::CommandOutput) -> io::Error {
    let stderr = output.stderr_text();
    let stdout = output.stdout_text();
    let reason = if output.timed_out {
        format!("timed out after {}s", WORKTREE_GIT_TIMEOUT.as_secs())
    } else {
        format!("exited with {}", output.status)
    };
    io::Error::other(format!(
        "git {} {reason}: {}{}",
        args.join(" "),
        stdout.trim(),
        stderr.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_git_output_is_bounded_at_ingress() {
        let repo = tempfile::tempdir().expect("repo");
        let init = Command::new("git")
            .arg("init")
            .current_dir(repo.path())
            .status()
            .expect("git init");
        assert!(init.success());

        let output = run_git(
            repo.path(),
            &[
                "-c",
                "alias.noisy=!yes x | tr -d '\\n' | head -c 262144",
                "noisy",
            ],
        )
        .expect("run noisy git alias");

        assert!(output.status.success());
        assert_eq!(output.stdout_observed_bytes, 262_144);
        assert_eq!(output.stdout.len(), WORKTREE_GIT_RETAINED_BYTES);
        assert_eq!(
            output.stdout_omitted_bytes,
            262_144 - WORKTREE_GIT_RETAINED_BYTES
        );
    }
}
