//! Recognizes shell commands that write a repository's git metadata.
//!
//! auto-edit and suggest run `bash` in a sandbox that keeps the workspace's
//! `.git` read-only. A command recognized here as a git write may run once with
//! `.git` writable after a single approval. The parser only follows plain
//! commands: anything it cannot follow is `Unrecognized` and runs as before, so
//! a miss costs an approval prompt, never a wider grant.

use std::path::{Component, Path, PathBuf};

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
