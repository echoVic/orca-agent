use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use crate::display_text::truncate_to_display_width;

const GIT_TIMEOUT: Duration = Duration::from_millis(500);
const GIT_RETAINED_BYTES_PER_STREAM: usize = 2 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitIdentity {
    Branch(String),
    Detached(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceStatus {
    pub(crate) cwd: String,
    pub(crate) git: Option<GitIdentity>,
}

impl GitIdentity {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Branch(branch) => format!("git:{branch}"),
            Self::Detached(commit) => format!("git:@{commit}"),
        }
    }
}

pub(crate) fn compact_cwd(cwd: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(cwd) <= max_width {
        return cwd.to_string();
    }

    let basename = Path::new(cwd)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(cwd);
    let middle = if cwd.starts_with("~/") {
        format!("~/…/{basename}")
    } else if cwd.starts_with('/') {
        format!("/…/{basename}")
    } else {
        format!("…/{basename}")
    };
    if UnicodeWidthStr::width(middle.as_str()) <= max_width {
        return middle;
    }
    if UnicodeWidthStr::width(basename) <= max_width {
        return basename.to_string();
    }
    truncate_to_display_width(basename, max_width)
}

fn display_cwd(workspace: &Path, home: Option<&Path>) -> String {
    let display = home
        .and_then(|home| workspace.strip_prefix(home).ok())
        .map(|relative| {
            if relative.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", relative.display())
            }
        })
        .unwrap_or_else(|| workspace.display().to_string());
    display
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[derive(Debug)]
struct GitCommandResult {
    success: bool,
    exit_code: Option<i32>,
    timed_out: bool,
    output_omitted: bool,
    stdout: String,
}

fn discover_git_identity(
    workspace: &Path,
    mut run: impl FnMut(&Path, &[&str]) -> io::Result<GitCommandResult>,
) -> Option<GitIdentity> {
    let symbolic = run(workspace, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()?;
    if symbolic.timed_out || symbolic.output_omitted {
        return None;
    }
    if symbolic.success {
        return valid_single_line(&symbolic).map(GitIdentity::Branch);
    }
    if symbolic.exit_code != Some(1) {
        return None;
    }

    let detached = run(workspace, &["rev-parse", "--short=8", "HEAD"]).ok()?;
    let commit = valid_single_line(&detached)?;
    (commit.len() == 8 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(GitIdentity::Detached(commit))
}

fn valid_single_line(output: &GitCommandResult) -> Option<String> {
    if !output.success || output.timed_out || output.output_omitted {
        return None;
    }
    let value = output.stdout.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_string())
}

fn run_git(workspace: &Path, args: &[&str]) -> io::Result<GitCommandResult> {
    let mut command = Command::new("git");
    command
        .current_dir(workspace)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    orca_tools::process::prepare_non_interactive_command(&mut command);
    let (child, process_job, _receipt) =
        orca_tools::process::spawn_user_trusted(command, "tui:workspace-status", workspace)?;
    let output = orca_tools::process::wait_for_child_output_with_timeout_or_cancel_and_limit(
        child,
        process_job,
        GIT_TIMEOUT,
        || false,
        GIT_RETAINED_BYTES_PER_STREAM,
    )?;
    Ok(GitCommandResult {
        success: output.status.success(),
        exit_code: output.status.code(),
        timed_out: output.timed_out,
        output_omitted: output.output_was_omitted(),
        stdout: output.stdout_text(),
    })
}

pub(crate) fn snapshot(workspace: &Path) -> WorkspaceStatus {
    WorkspaceStatus {
        cwd: display_cwd(workspace, dirs::home_dir().as_deref()),
        git: discover_git_identity(workspace, run_git),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use unicode_width::UnicodeWidthStr;

    use super::*;

    #[test]
    fn display_cwd_shortens_only_a_component_safe_home_prefix() {
        assert_eq!(
            display_cwd(
                Path::new("/Users/alice/work/project"),
                Some(Path::new("/Users/alice")),
            ),
            "~/work/project"
        );
        assert_eq!(
            display_cwd(
                Path::new("/Users/alice-other/project"),
                Some(Path::new("/Users/alice")),
            ),
            "/Users/alice-other/project"
        );
        assert_eq!(
            display_cwd(Path::new("/Users/alice"), Some(Path::new("/Users/alice"))),
            "~"
        );
    }

    #[test]
    fn display_cwd_replaces_control_characters_without_changing_components() {
        assert_eq!(
            display_cwd(Path::new("/tmp/line\nbreak"), None),
            "/tmp/line break"
        );
    }

    #[test]
    fn compact_cwd_uses_full_middle_basename_then_grapheme_safe_truncation() {
        let cwd = "~/Documents/GitHub/blade-deepseek";
        assert_eq!(compact_cwd(cwd, 40), cwd);
        assert_eq!(compact_cwd(cwd, 22), "~/…/blade-deepseek");
        assert_eq!(compact_cwd(cwd, 14), "blade-deepseek");
        assert_eq!(compact_cwd(cwd, 10), "blade-dee…");
        assert_eq!(compact_cwd(cwd, 0), "");

        let unicode = "~/项目/👍🏽-workspace";
        for width in 0..=20 {
            let compact = compact_cwd(unicode, width);
            assert!(UnicodeWidthStr::width(compact.as_str()) <= width);
            assert!(!compact.contains('�'));
            assert_eq!(
                compact.contains('👍'),
                compact.contains('🏽'),
                "emoji modifier must remain attached: {compact:?}",
            );
        }
    }

    #[test]
    fn git_identity_labels_distinguish_branch_and_detached_head() {
        assert_eq!(
            GitIdentity::Branch("feature/footer".to_string()).label(),
            "git:feature/footer"
        );
        assert_eq!(
            GitIdentity::Detached("5bbb60aa".to_string()).label(),
            "git:@5bbb60aa"
        );
    }

    #[test]
    fn discovery_prefers_symbolic_branch_without_requesting_head() {
        let mut calls = Vec::new();
        let identity = discover_git_identity(Path::new("/workspace"), |cwd, args| {
            calls.push((cwd.to_path_buf(), args.join(" ")));
            Ok(GitCommandResult {
                success: true,
                exit_code: Some(0),
                timed_out: false,
                output_omitted: false,
                stdout: "feature/footer\n".to_string(),
            })
        });

        assert_eq!(
            identity,
            Some(GitIdentity::Branch("feature/footer".to_string()))
        );
        assert_eq!(
            calls,
            vec![(
                PathBuf::from("/workspace"),
                "symbolic-ref --quiet --short HEAD".to_string(),
            )]
        );
    }

    #[test]
    fn discovery_falls_back_to_detached_commit_only_after_symbolic_failure() {
        let mut calls = Vec::new();
        let identity = discover_git_identity(Path::new("/workspace"), |_, args| {
            calls.push(args.join(" "));
            if args[0] == "symbolic-ref" {
                Ok(GitCommandResult {
                    success: false,
                    exit_code: Some(1),
                    timed_out: false,
                    output_omitted: false,
                    stdout: String::new(),
                })
            } else {
                Ok(GitCommandResult {
                    success: true,
                    exit_code: Some(0),
                    timed_out: false,
                    output_omitted: false,
                    stdout: "5bbb60aa\n".to_string(),
                })
            }
        });

        assert_eq!(
            identity,
            Some(GitIdentity::Detached("5bbb60aa".to_string()))
        );
        assert_eq!(
            calls,
            [
                "symbolic-ref --quiet --short HEAD",
                "rev-parse --short=8 HEAD",
            ]
        );
    }

    #[test]
    fn discovery_does_not_treat_fatal_symbolic_ref_error_as_detached_head() {
        let mut calls = Vec::new();
        let identity = discover_git_identity(Path::new("/workspace"), |_, args| {
            calls.push(args.join(" "));
            Ok(GitCommandResult {
                success: false,
                exit_code: Some(128),
                timed_out: false,
                output_omitted: false,
                stdout: String::new(),
            })
        });

        assert_eq!(identity, None);
        assert_eq!(calls, ["symbolic-ref --quiet --short HEAD"]);
    }

    #[test]
    fn discovery_rejects_errors_timeouts_omission_and_malformed_output() {
        let cases = [
            Err(io::Error::new(io::ErrorKind::NotFound, "git missing")),
            Ok(GitCommandResult {
                success: true,
                exit_code: Some(0),
                timed_out: true,
                output_omitted: false,
                stdout: "main".to_string(),
            }),
            Ok(GitCommandResult {
                success: true,
                exit_code: Some(0),
                timed_out: false,
                output_omitted: true,
                stdout: "main".to_string(),
            }),
            Ok(GitCommandResult {
                success: true,
                exit_code: Some(0),
                timed_out: false,
                output_omitted: false,
                stdout: "main\ninjected".to_string(),
            }),
        ];

        for result in cases {
            let mut result = Some(result);
            assert_eq!(
                discover_git_identity(Path::new("/workspace"), |_, _| {
                    result.take().expect("one symbolic-ref result")
                }),
                None
            );
        }
    }

    #[test]
    fn detached_discovery_requires_exactly_eight_hex_characters() {
        for invalid in ["", "abc", "zzzzzzzz", "123456789", "1234\n5678"] {
            let mut call = 0;
            let identity = discover_git_identity(Path::new("/workspace"), |_, _| {
                call += 1;
                Ok(GitCommandResult {
                    success: call == 2,
                    exit_code: Some(if call == 2 { 0 } else { 1 }),
                    timed_out: false,
                    output_omitted: false,
                    stdout: if call == 2 {
                        invalid.to_string()
                    } else {
                        String::new()
                    },
                })
            });
            assert_eq!(identity, None, "{invalid:?}");
        }
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {}", args.join(" "));
    }

    fn committed_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("repo");
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &["config", "user.email", "orca@example.invalid"],
        );
        git(repo.path(), &["config", "user.name", "Orca Test"]);
        std::fs::write(repo.path().join("README.md"), "workspace").expect("fixture");
        git(repo.path(), &["add", "README.md"]);
        git(repo.path(), &["commit", "-qm", "fixture"]);
        git(repo.path(), &["branch", "-M", "footer-test"]);
        repo
    }

    #[test]
    fn snapshot_reads_real_branch_and_detached_head() {
        if !git_available() {
            return;
        }
        let repo = committed_repo();
        assert_eq!(
            snapshot(repo.path()).git,
            Some(GitIdentity::Branch("footer-test".to_string()))
        );

        git(repo.path(), &["checkout", "-q", "--detach", "HEAD"]);
        let detached = snapshot(repo.path()).git.expect("detached identity");
        assert!(matches!(
            detached,
            GitIdentity::Detached(ref commit)
                if commit.len() == 8 && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        ));
    }

    #[test]
    fn snapshot_silently_omits_git_for_non_repo_and_never_fakes_unborn_commit() {
        if !git_available() {
            return;
        }
        let directory = tempfile::tempdir().expect("directory");
        assert_eq!(snapshot(directory.path()).git, None);

        git(directory.path(), &["init", "-q"]);
        assert!(matches!(
            snapshot(directory.path()).git,
            Some(GitIdentity::Branch(_))
        ));
    }
}
