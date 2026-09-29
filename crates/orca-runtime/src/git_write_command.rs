//! Recognizes shell commands that write a repository's git metadata.
//!
//! auto-edit and suggest run `bash` in a sandbox that keeps the workspace's
//! `.git` read-only. A command recognized here as a git write may run once with
//! `.git` writable after a single approval. The parser only follows plain
//! commands: anything it cannot follow is `Unrecognized` and runs as before, so
//! a miss costs an approval prompt, never a wider grant.

use std::path::{Component, Path, PathBuf};

use orca_core::config::RunConfig;
use orca_core::tool_types::{ToolName, ToolRequest};

/// The `.git` a bash request would write, when one approval can let it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitMetadataWrite {
    pub(crate) command: String,
    pub(crate) git_dir: PathBuf,
}

/// The `.git` a bash request would write, when one approval can let it: the
/// command is a recognized git write, bash runs it in the workspace-write
/// sandbox, and its working directory is a repository root whose `config` and
/// `hooks` exist (Linux can keep only existing paths read-only).
pub(crate) fn git_metadata_write_for_bash(
    config: &RunConfig,
    workspace: &Path,
    request: &ToolRequest,
) -> Option<GitMetadataWrite> {
    // The Windows sandbox denies `.git` as a whole and cannot open part of it.
    if cfg!(windows) || request.name != ToolName::Bash {
        return None;
    }
    let arguments: serde_json::Value =
        serde_json::from_str(request.raw_arguments.as_deref()?).ok()?;
    let command = arguments.get("command")?.as_str()?.trim();
    let workdir = arguments
        .get("workdir")
        .and_then(serde_json::Value::as_str)
        .map(Path::new);
    let cwd = crate::runtime_normal_tool::resolve_workdir(workspace, workdir).ok()?;
    if classify_git_command(command, &cwd) != GitCommandClass::WritesMetadata {
        return None;
    }
    let sandbox = crate::server::bash_sandbox_for_cwd(config, &cwd).ok()?;
    if !matches!(
        sandbox.mode,
        crate::shell_session::ShellSandboxMode::WorkspaceWrite { .. }
    ) {
        return None;
    }
    let git_dir = cwd.join(".git");
    // A permission profile can already grant `.git` write as a whole (its
    // resolved sandbox lists it in `metadata_writable_roots`). That grant
    // opens `.git` for every command, not just this one, and keeps its
    // meaning: this path must leave it alone rather than narrowing it down
    // to a single approved command.
    if metadata_roots_contain(&sandbox.metadata_writable_roots, &git_dir) {
        return None;
    }
    let is = |path: PathBuf, directory: bool| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            }
        })
    };
    (is(git_dir.clone(), true)
        && is(git_dir.join("config"), false)
        && is(git_dir.join("hooks"), true))
    .then(|| GitMetadataWrite {
        command: command.to_string(),
        git_dir,
    })
}

/// `path`, canonicalized, or `path` itself when it cannot be (it does not
/// exist, for instance). Used to compare two roots that may each be spelled
/// with or without symlinks resolved.
fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `git_dir` is already one of `roots`, comparing each root both as
/// written and canonicalized. Two sources can already grant `.git` as a
/// whole before this module ever sees a command: a permission profile's
/// `CommandExecSandbox::metadata_writable_roots`, and a turn's
/// `request_permissions`-granted `TurnPermissionOverlay::metadata_writable_directories`.
/// Either may spell the same directory differently than the `git_dir` this
/// module computes from `cwd`, so a literal-only comparison could miss an
/// existing grant.
fn metadata_roots_contain(roots: &[PathBuf], git_dir: &Path) -> bool {
    let canonical_git_dir = canonical_or_self(git_dir);
    roots
        .iter()
        .any(|root| root == git_dir || canonical_or_self(root) == canonical_git_dir)
}

/// Whether `overlay` already makes `git_dir` writable: a `request_permissions`
/// grant earlier this turn put it in `metadata_writable_directories()`. Like a
/// permission profile's grant, that opens `.git` as a whole for the rest of
/// the turn and keeps that meaning — this module's one-command path must
/// leave it alone: no prompt, no suggest note or mark, no narrowing.
pub(crate) fn git_dir_writable_in_overlay(
    overlay: &crate::runtime_permission::TurnPermissionOverlay,
    git_dir: &Path,
) -> bool {
    metadata_roots_contain(overlay.metadata_writable_directories(), git_dir)
}

/// The parts of a granted `.git` that stay read-only. Everything that can make
/// code run outside the sandbox later lives here (hooks, and config's
/// fsmonitor, hooksPath, pager, aliases, and filters; `modules` holds each
/// submodule's own), and a commit or merge never writes them.
pub(crate) fn read_only_git_metadata(git_dir: &Path) -> Vec<PathBuf> {
    ["config", "config.worktree", "hooks", "modules"]
        .iter()
        .map(|name| git_dir.join(name))
        .collect()
}

/// What the auto-edit permission prompt says.
pub(crate) fn git_metadata_write_reason(write: &GitMetadataWrite) -> String {
    format!(
        "`{}` writes {}. Allow it for this command only? Its config, hooks, and modules stay read-only. Allowing it for the session lets later git writes run without asking, each for its own command.",
        write.command,
        write.git_dir.display()
    )
}

/// What suggest's approval shows, so approving the command also approves the
/// `.git` it writes.
pub(crate) fn git_metadata_approval_note(write: &GitMetadataWrite) -> String {
    format!(
        "This command writes {} for this run only; its config, hooks, and modules stay read-only.",
        write.git_dir.display()
    )
}

/// Whether the user let git writes run without asking for the rest of this
/// session. Kept in the thread's extension store, so each session thread (and
/// each subagent thread) has its own, and a restart starts without it. It
/// records no directory: every approved command still gets its own grant.
#[derive(Debug, Default)]
pub(crate) struct GitMetadataWriteSession {
    approved: std::sync::atomic::AtomicBool,
}

impl GitMetadataWriteSession {
    pub(crate) fn approved(&self) -> bool {
        self.approved.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn approve(&self) {
        self.approved
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitCommandClass {
    /// No git invocation, or only ones that read the repository.
    ReadOnly,
    /// At least one git invocation writes `.git`, and none is refused.
    WritesMetadata,
    /// A git invocation that is never approved this way: it reaches config,
    /// hooks, another repository, or the network.
    Refused,
    /// A shape this parser does not follow: substitution, eval, a heredoc, a
    /// nested shell, or an unknown git subcommand or option.
    Unrecognized,
}

pub(crate) fn classify_git_command(command: &str, cwd: &Path) -> GitCommandClass {
    let Ok(commands) = simple_commands(command) else {
        return GitCommandClass::Unrecognized;
    };
    let mut class = GitCommandClass::ReadOnly;
    for words in &commands {
        match classify_simple(words, cwd) {
            GitCommandClass::ReadOnly => {}
            GitCommandClass::WritesMetadata => class = GitCommandClass::WritesMetadata,
            other => return other,
        }
    }
    class
}

/// Splits `command` into simple commands of unquoted words. `Err` marks a
/// shape this parser does not follow.
fn simple_commands(command: &str) -> Result<Vec<Vec<String>>, ()> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' => {
                let escaped = chars.next().ok_or(())?;
                if escaped != '\n' {
                    word.push(escaped);
                    in_word = true;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next().ok_or(())? {
                        '\'' => break,
                        quoted => word.push(quoted),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next().ok_or(())? {
                        '"' => break,
                        '`' => return Err(()),
                        '$' if chars.peek() == Some(&'(') => return Err(()),
                        '\\' => word.push(chars.next().ok_or(())?),
                        quoted => word.push(quoted),
                    }
                }
            }
            '`' => return Err(()),
            '$' if chars.peek() == Some(&'(') => return Err(()),
            '<' if chars.peek() == Some(&'<') => return Err(()),
            // `2>&1` and `<&0` duplicate a descriptor; the `&` is not a separator.
            '&' if word.ends_with('>') || word.ends_with('<') => word.push('&'),
            ';' | '\n' | '(' | ')' | '|' | '&' => {
                if matches!(character, '|' | '&') && matches!(chars.peek(), Some('|' | '&')) {
                    chars.next();
                }
                end_word(&mut words, &mut word, &mut in_word);
                end_command(&mut commands, &mut words);
            }
            space if space.is_whitespace() => end_word(&mut words, &mut word, &mut in_word),
            other => {
                word.push(other);
                in_word = true;
            }
        }
    }
    end_word(&mut words, &mut word, &mut in_word);
    end_command(&mut commands, &mut words);
    Ok(commands)
}

fn end_word(words: &mut Vec<String>, word: &mut String, in_word: &mut bool) {
    if *in_word {
        words.push(std::mem::take(word));
        *in_word = false;
    }
}

fn end_command(commands: &mut Vec<Vec<String>>, words: &mut Vec<String>) {
    if !words.is_empty() {
        commands.push(std::mem::take(words));
    }
}

fn classify_simple(words: &[String], cwd: &Path) -> GitCommandClass {
    let words = strip_redirections(words);
    let mut index = 0;
    while let Some(name) = words.get(index).and_then(|word| assignment_name(word)) {
        if name.starts_with("GIT_") {
            return GitCommandClass::Refused;
        }
        index += 1;
    }
    let index = match skip_wrappers(&words, index) {
        Ok(index) => index,
        Err(class) => return class,
    };
    let Some(program) = words.get(index) else {
        return GitCommandClass::ReadOnly;
    };
    let arguments = &words[index + 1..];
    match program.as_str() {
        "eval" => GitCommandClass::Unrecognized,
        "sh" | "bash" | "zsh" | "dash" | "ksh"
            if arguments
                .iter()
                .any(|argument| argument.starts_with('-') && argument.contains('c')) =>
        {
            GitCommandClass::Unrecognized
        }
        "git" => classify_git(arguments, cwd),
        path if path.ends_with("/git") => classify_git(arguments, cwd),
        _ => GitCommandClass::ReadOnly,
    }
}

/// Drops redirections (`> file`, `2>&1`, `<in`), which are not arguments.
fn strip_redirections(words: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut skip_target = false;
    for word in words {
        if std::mem::take(&mut skip_target) {
            continue;
        }
        match redirection(word) {
            Some(needs_target) => skip_target = needs_target,
            None => kept.push(word.clone()),
        }
    }
    kept
}

/// `Some(true)` for a redirection whose target is the next word.
fn redirection(word: &str) -> Option<bool> {
    let rest = word.trim_start_matches(|character: char| character.is_ascii_digit());
    let rest = rest.strip_prefix('&').unwrap_or(rest);
    [">>", ">|", ">&", "<&", "<>", ">", "<"]
        .iter()
        .find_map(|operator| rest.strip_prefix(*operator))
        .map(str::is_empty)
}

fn assignment_name(word: &str) -> Option<&str> {
    let (name, _) = word.split_once('=')?;
    let mut characters = name.chars();
    let first = characters.next()?;
    ((first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric()))
    .then_some(name)
}

/// Skips `command`, `time`, `nohup`, `nice`, and `env` in front of a program.
fn skip_wrappers(words: &[String], mut index: usize) -> Result<usize, GitCommandClass> {
    loop {
        match words.get(index).map(String::as_str) {
            Some("command" | "time" | "nohup") => {
                index += 1;
                while words.get(index).is_some_and(|word| word.starts_with('-')) {
                    index += 1;
                }
            }
            Some("nice") => {
                index += 1;
                while let Some(word) = words.get(index) {
                    if word == "-n" || word == "--adjustment" {
                        index += 2;
                    } else if word.starts_with('-') {
                        index += 1;
                    } else {
                        break;
                    }
                }
            }
            Some("env") => {
                index += 1;
                while let Some(word) = words.get(index) {
                    if matches!(word.as_str(), "-u" | "--unset" | "-C" | "--chdir") {
                        index += 2;
                    } else if word.starts_with('-') {
                        index += 1;
                    } else if let Some(name) = assignment_name(word) {
                        if name.starts_with("GIT_") {
                            return Err(GitCommandClass::Refused);
                        }
                        index += 1;
                    } else {
                        break;
                    }
                }
            }
            _ => return Ok(index),
        }
    }
}

fn classify_git(arguments: &[String], cwd: &Path) -> GitCommandClass {
    const IGNORED: &[&str] = &[
        "--no-pager",
        "-p",
        "--paginate",
        "-P",
        "--no-optional-locks",
        "--literal-pathspecs",
        "--glob-pathspecs",
        "--noglob-pathspecs",
        "--icase-pathspecs",
        "--no-replace-objects",
    ];
    const INFORMATIONAL: &[&str] = &[
        "--version",
        "--help",
        "-h",
        "--html-path",
        "--man-path",
        "--info-path",
    ];
    // Each redirects where git reads its configuration or which repository it
    // writes, which a `.git` grant for this workspace must not follow.
    const REFUSED: &[&str] = &[
        "-c",
        "--config-env",
        "--exec-path",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--super-prefix",
        "--bare",
        "--attr-source",
    ];
    let mut index = 0;
    while let Some(argument) = arguments.get(index).map(String::as_str) {
        if !argument.starts_with('-') {
            break;
        }
        if IGNORED.contains(&argument) {
            index += 1;
        } else if INFORMATIONAL.contains(&argument) {
            return GitCommandClass::ReadOnly;
        } else if argument == "-C" {
            match arguments.get(index + 1) {
                Some(directory) if stays_inside(cwd, directory) => index += 2,
                Some(_) => return GitCommandClass::Refused,
                None => return GitCommandClass::Unrecognized,
            }
        } else if REFUSED.contains(&option_name(argument)) {
            return GitCommandClass::Refused;
        } else {
            return GitCommandClass::Unrecognized;
        }
    }
    match arguments.get(index) {
        Some(subcommand) => classify_subcommand(subcommand, &arguments[index + 1..]),
        None => GitCommandClass::ReadOnly,
    }
}

fn classify_subcommand(subcommand: &str, arguments: &[String]) -> GitCommandClass {
    use GitCommandClass::{ReadOnly, Refused, Unrecognized, WritesMetadata};
    match subcommand {
        "add" | "am" | "bisect" | "checkout" | "cherry-pick" | "commit" | "gc" | "merge" | "mv"
        | "rebase" | "reset" | "restore" | "revert" | "rm" | "switch" | "update-index"
        | "update-ref" => WritesMetadata,
        "apply" if has_any(arguments, &["--index", "--cached", "--3way", "-3"]) => WritesMetadata,
        "apply" => ReadOnly,
        "stash" => match first_positional(arguments) {
            Some("list" | "show") => ReadOnly,
            _ => WritesMetadata,
        },
        "branch" => branch_class(arguments),
        "tag" => tag_class(arguments),
        "notes" => match first_positional(arguments) {
            None | Some("list" | "show") => ReadOnly,
            _ => WritesMetadata,
        },
        "reflog" => match first_positional(arguments) {
            Some("expire" | "delete") => WritesMetadata,
            _ => ReadOnly,
        },
        "config" => config_class(arguments),
        "remote" => match first_positional(arguments) {
            None | Some("show" | "get-url") => ReadOnly,
            _ => Refused,
        },
        // fetch, pull, push, and clone also need the network, which auto-edit
        // keeps closed, so a `.git` grant alone would not let them run.
        "submodule" | "worktree" | "hook" | "filter-branch" | "sparse-checkout" | "init"
        | "clone" | "fetch" | "pull" | "push" | "maintenance" | "replace" | "credential"
        | "daemon" => Refused,
        "status" | "log" | "diff" | "show" | "blame" | "grep" | "rev-parse" | "describe"
        | "ls-files" | "ls-tree" | "ls-remote" | "cat-file" | "shortlog" | "show-ref"
        | "for-each-ref" | "rev-list" | "name-rev" | "merge-base" | "diff-tree" | "diff-index"
        | "diff-files" | "check-ignore" | "check-attr" | "count-objects" | "cherry"
        | "range-diff" | "format-patch" | "archive" | "whatchanged" | "var" | "help"
        | "version" | "fsck" => ReadOnly,
        _ => Unrecognized,
    }
}

fn branch_class(arguments: &[String]) -> GitCommandClass {
    const WRITES: &[&str] = &[
        "-d",
        "-D",
        "--delete",
        "-m",
        "-M",
        "--move",
        "-c",
        "-C",
        "--copy",
        "-f",
        "--force",
        "-u",
        "--set-upstream-to",
        "--unset-upstream",
        "--edit-description",
        "-t",
        "--track",
        "--no-track",
        "--create-reflog",
    ];
    const LISTS: &[&str] = &[
        "-l",
        "--list",
        "-a",
        "--all",
        "-r",
        "--remotes",
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "--show-current",
        "-v",
        "-vv",
        "--verbose",
        "--format",
        "--sort",
        "--column",
        "--no-column",
        "--color",
        "--no-color",
    ];
    if has_any(arguments, WRITES) {
        GitCommandClass::WritesMetadata
    } else if has_any(arguments, LISTS) || first_positional(arguments).is_none() {
        GitCommandClass::ReadOnly
    } else {
        GitCommandClass::WritesMetadata
    }
}

fn tag_class(arguments: &[String]) -> GitCommandClass {
    const WRITES: &[&str] = &[
        "-d",
        "--delete",
        "-a",
        "--annotate",
        "-s",
        "--sign",
        "-u",
        "--local-user",
        "-f",
        "--force",
        "-m",
        "--message",
        "-F",
        "--file",
    ];
    const READS: &[&str] = &[
        "-l",
        "--list",
        "-v",
        "--verify",
        "-n",
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "--sort",
        "--format",
        "--column",
        "--no-column",
    ];
    if has_any(arguments, WRITES) {
        GitCommandClass::WritesMetadata
    } else if has_any(arguments, READS) || first_positional(arguments).is_none() {
        GitCommandClass::ReadOnly
    } else {
        GitCommandClass::WritesMetadata
    }
}

fn config_class(arguments: &[String]) -> GitCommandClass {
    const READS: &[&str] = &[
        "--get",
        "--get-all",
        "--get-regexp",
        "--get-urlmatch",
        "--get-color",
        "--get-colorbool",
        "-l",
        "--list",
    ];
    if has_any(arguments, READS) || matches!(first_positional(arguments), Some("get" | "list")) {
        GitCommandClass::ReadOnly
    } else {
        GitCommandClass::Refused
    }
}

fn option_name(argument: &str) -> &str {
    argument.split_once('=').map_or(argument, |(name, _)| name)
}

fn has_any(arguments: &[String], options: &[&str]) -> bool {
    arguments
        .iter()
        .any(|argument| options.contains(&option_name(argument)))
}

fn first_positional(arguments: &[String]) -> Option<&str> {
    arguments
        .iter()
        .map(String::as_str)
        .find(|argument| !argument.starts_with('-'))
}

/// Whether `directory`, taken relative to `cwd`, stays inside `cwd`. Lexical:
/// a symlink could still lead out, and the sandbox is what stops that.
fn stays_inside(cwd: &Path, directory: &str) -> bool {
    let mut resolved = PathBuf::new();
    for component in cwd.join(directory).components() {
        match component {
            Component::ParentDir => {
                if !resolved.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved.starts_with(cwd)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{GitCommandClass, classify_git_command};

    fn class(command: &str) -> GitCommandClass {
        classify_git_command(command, Path::new("/repo"))
    }

    #[test]
    fn git_writes_that_need_the_metadata_directory_are_recognized() {
        for command in [
            "git commit -m 'fix: x'",
            "git add -A",
            "git rm --cached a.txt",
            "git mv a b",
            "git checkout -b feature",
            "git switch main",
            "git restore --staged a.txt",
            "git reset --hard HEAD~1",
            "git merge feature",
            "git rebase main",
            "git cherry-pick abc123",
            "git revert HEAD",
            "git stash",
            "git stash pop",
            "git branch feature",
            "git branch -D feature",
            "git tag v1.0",
            "git tag -d v1.0",
            "git am patch.mbox",
            "git apply --index fix.patch",
            "git gc",
            "git update-index --refresh",
            "git update-ref refs/heads/x HEAD",
            "git bisect start",
            "git notes add -m note",
            "git reflog expire --all",
        ] {
            assert_eq!(class(command), GitCommandClass::WritesMetadata, "{command}");
        }
    }

    #[test]
    fn reading_git_commands_and_other_commands_are_read_only() {
        for command in [
            "git status",
            "git log --oneline -5",
            "git diff HEAD~1",
            "git show HEAD",
            "git blame src/lib.rs",
            "git grep TODO",
            "git rev-parse HEAD",
            "git config --get user.name",
            "git config --list",
            "git remote -v",
            "git branch",
            "git branch -a",
            "git branch --contains HEAD",
            "git tag",
            "git tag -l 'v*'",
            "git stash list",
            "git notes show HEAD",
            "git reflog",
            "git apply fix.patch",
            "git --no-pager log",
            "cargo test",
            "echo git commit",
            "",
        ] {
            assert_eq!(class(command), GitCommandClass::ReadOnly, "{command}");
        }
    }

    #[test]
    fn git_commands_that_reach_config_hooks_or_other_repositories_are_refused() {
        for command in [
            "git config core.hooksPath .husky",
            "git config --unset core.fsmonitor",
            "git remote add origin https://example.com/repo.git",
            "git submodule update --init",
            "git worktree add ../other",
            "git hook run pre-commit",
            "git filter-branch --tree-filter true",
            "git sparse-checkout set src",
            "git init",
            "git -c core.fsmonitor=true status",
            "git --config-env=core.pager=PAGER log",
            "git --exec-path=/tmp commit",
            "git --git-dir=/tmp/x commit -m x",
            "git --work-tree=/tmp commit -m x",
            "GIT_DIR=/tmp/x git commit -m x",
            "env GIT_CONFIG_GLOBAL=/tmp/c git commit -m x",
            "git -C /elsewhere commit -m x",
            "git -C ../sibling commit -m x",
            "git fetch origin",
            "git pull",
            "git push origin main",
            "git clone https://example.com/repo.git",
        ] {
            assert_eq!(class(command), GitCommandClass::Refused, "{command}");
        }
    }

    #[test]
    fn shapes_the_parser_does_not_follow_are_unrecognized() {
        for command in [
            "git commit -m \"$(date)\"",
            "git commit -m `date`",
            "eval git commit -m x",
            "sh -c 'git commit -m x'",
            "bash -lc \"git add -A\"",
            "git commit -F - <<EOF\nmsg\nEOF",
            "git co feature",
            "git --unknown-flag commit",
            "git commit -m 'unterminated",
        ] {
            assert_eq!(class(command), GitCommandClass::Unrecognized, "{command}");
        }
    }

    #[test]
    fn compound_commands_combine_their_parts() {
        assert_eq!(
            class("git add -A && git commit -m 'x; y'"),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(
            class("cargo fmt && git commit -am x | tee log.txt"),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(class("(cd src && git status)"), GitCommandClass::ReadOnly);
        assert_eq!(
            class("git commit -m x 2>&1; git log -1"),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(
            class("git commit --allow-empty -m x && printf y > .git/hooks/probe"),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(
            class("git commit -m x && git config user.name y"),
            GitCommandClass::Refused
        );
        assert_eq!(
            class("git commit -m x && bash -c 'true'"),
            GitCommandClass::Unrecognized
        );
    }

    #[test]
    fn wrappers_assignments_and_paths_to_git_are_seen_through() {
        assert_eq!(
            class("LANG=C git commit -m x"),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(
            class("env LC_ALL=C git add ."),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(
            class("command git commit -m x"),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(class("time git gc"), GitCommandClass::WritesMetadata);
        assert_eq!(class("nice -n 5 git gc"), GitCommandClass::WritesMetadata);
        assert_eq!(
            class("/usr/bin/git commit -m x"),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(
            class("git -C sub commit -m x"),
            GitCommandClass::WritesMetadata
        );
        assert_eq!(
            class("git -C ./sub/../sub status"),
            GitCommandClass::ReadOnly
        );
    }
}

#[cfg(test)]
mod overlay_tests {
    use std::path::{Path, PathBuf};

    use crate::protocol::{RequestFileSystemPermissions, RequestPermissionProfile};
    use crate::runtime_permission::TurnPermissionOverlay;

    use super::git_dir_writable_in_overlay;

    #[test]
    fn a_default_overlay_has_not_granted_git_dir() {
        let overlay = TurnPermissionOverlay::default();
        assert!(!git_dir_writable_in_overlay(
            &overlay,
            Path::new("/repo/.git")
        ));
    }

    #[test]
    fn an_overlay_that_merged_a_git_dir_write_has_granted_it() {
        let git_dir = PathBuf::from("/repo/.git");
        let mut overlay = TurnPermissionOverlay::default();
        overlay.merge_permissions(&RequestPermissionProfile {
            file_system: Some(RequestFileSystemPermissions {
                write: Some(vec![git_dir.clone()]),
                ..Default::default()
            }),
            ..Default::default()
        });

        assert!(git_dir_writable_in_overlay(&overlay, &git_dir));
        // A different repository's `.git` was not granted by this overlay.
        assert!(!git_dir_writable_in_overlay(
            &overlay,
            Path::new("/other/.git")
        ));
    }
}

#[cfg(all(test, unix))]
mod approvable_tests {
    use std::path::{Path, PathBuf};

    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::folder_trust::{TrustLevel, set_trust_with_config_dir};
    use orca_core::config::{
        ActivePermissionProfile, PermissionProfileConfig, PermissionProfileFileAccess, RunConfig,
    };
    use orca_core::tool_types::{ToolName, ToolRequest};

    use super::{GitMetadataWrite, git_metadata_write_for_bash};

    /// A repository root with the `.git` layout `git init` leaves.
    fn repository(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n").unwrap();
        root.canonicalize().unwrap()
    }

    fn config(root: &Path, mode: ApprovalMode) -> RunConfig {
        let mut config = crate::runtime_tool_call::tests::test_config();
        config.cwd = Some(root.to_path_buf());
        config.approval_mode = mode;
        config
    }

    fn bash(arguments: serde_json::Value) -> ToolRequest {
        ToolRequest {
            id: "bash-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: None,
            raw_arguments: Some(arguments.to_string()),
        }
    }

    #[test]
    fn a_git_write_at_the_root_of_a_trusted_repository_can_be_approved() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let root = repository(home.path(), "repo");
        set_trust_with_config_dir(&root, home.path(), TrustLevel::Trusted).unwrap();
        let request = bash(serde_json::json!({"command": "git commit -m x"}));

        for mode in [ApprovalMode::AutoEdit, ApprovalMode::Suggest] {
            assert_eq!(
                git_metadata_write_for_bash(&config(&root, mode), &root, &request),
                Some(GitMetadataWrite {
                    command: "git commit -m x".to_string(),
                    git_dir: root.join(".git"),
                }),
                "{mode:?}"
            );
        }
        // plan runs bash read-only, and full-auto runs it without a sandbox.
        for mode in [ApprovalMode::Plan, ApprovalMode::FullAuto] {
            assert_eq!(
                git_metadata_write_for_bash(&config(&root, mode), &root, &request),
                None,
                "{mode:?}"
            );
        }
    }

    /// A permission profile's `.git` grant is already an "open the whole
    /// thing" grant (spec 2026-09-28-git-metadata-approval-design.md §4): the
    /// one-command path must find nothing to do, not narrow it.
    #[test]
    fn a_git_write_already_granted_by_a_permission_profile_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let root = repository(home.path(), "repo");
        set_trust_with_config_dir(&root, home.path(), TrustLevel::Trusted).unwrap();
        let request = bash(serde_json::json!({"command": "git commit -m x"}));

        let mut granted = config(&root, ApprovalMode::AutoEdit);
        granted.permission_profiles.insert(
            "metadata".to_string(),
            PermissionProfileConfig {
                extends: Some(":workspace".to_string()),
                filesystem: std::collections::HashMap::from([(
                    root.join(".git"),
                    PermissionProfileFileAccess::Write,
                )])
                .into(),
                ..Default::default()
            },
        );
        granted.active_permission_profile = Some(ActivePermissionProfile {
            id: "metadata".to_string(),
            extends: None,
        });

        assert_eq!(
            git_metadata_write_for_bash(&granted, &root, &request),
            None,
            "an existing profile grant must be left exactly as it was"
        );
        // Sanity: the same repository and command are still approvable
        // without the profile.
        assert!(
            git_metadata_write_for_bash(&config(&root, ApprovalMode::AutoEdit), &root, &request)
                .is_some()
        );
    }

    #[test]
    fn untrusted_folders_subdirectories_partial_layouts_and_reads_cannot_be_approved() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let commit = bash(serde_json::json!({"command": "git commit -m x"}));

        // Untrusted: bash runs read-only.
        let untrusted = repository(home.path(), "untrusted");
        assert_eq!(
            git_metadata_write_for_bash(
                &config(&untrusted, ApprovalMode::AutoEdit),
                &untrusted,
                &commit
            ),
            None
        );

        let root = repository(home.path(), "repo");
        set_trust_with_config_dir(&root, home.path(), TrustLevel::Trusted).unwrap();
        let auto_edit = config(&root, ApprovalMode::AutoEdit);

        // A subdirectory workdir: only it is writable, not the root's `.git`.
        std::fs::create_dir(root.join("sub")).unwrap();
        let in_sub = bash(serde_json::json!({"command": "git commit -m x", "workdir": "sub"}));
        assert_eq!(
            git_metadata_write_for_bash(&auto_edit, &root, &in_sub),
            None
        );

        // A read, a refused write, and another tool.
        let status = bash(serde_json::json!({"command": "git status"}));
        assert_eq!(
            git_metadata_write_for_bash(&auto_edit, &root, &status),
            None
        );
        let config_write = bash(serde_json::json!({"command": "git config core.hooksPath x"}));
        assert_eq!(
            git_metadata_write_for_bash(&auto_edit, &root, &config_write),
            None
        );
        let mut read_file = bash(serde_json::json!({"command": "git commit -m x"}));
        read_file.name = ToolName::ReadFile;
        assert_eq!(
            git_metadata_write_for_bash(&auto_edit, &root, &read_file),
            None
        );

        // Without `hooks`, the grant could not keep a new hooks directory
        // read-only on Linux.
        std::fs::remove_dir(root.join(".git/hooks")).unwrap();
        assert_eq!(
            git_metadata_write_for_bash(&auto_edit, &root, &commit),
            None
        );
    }
}
