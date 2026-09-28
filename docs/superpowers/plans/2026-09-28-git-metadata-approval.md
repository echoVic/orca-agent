# git 写操作单条授权 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 auto-edit 和 suggest 下，识别出的 git 写命令经一次审批后，只对这一条命令放开工作区的 `.git`（`config`、`config.worktree`、`hooks`、`modules` 仍只读）；沙箱拒绝写元数据目录时，提示里点名 `request_permissions`。

**Architecture:** 新增纯逻辑模块 `git_write_command.rs`：识别命令、判定这次 bash 调用能否走快速审批、给出提示文字和只读例外。`execute_bash` 在执行前询问（auto-edit）或读取审批闸门留下的标记（suggest），批准后经 `TerminalExecRequest` 的单条授权字段，把 `.git` 和只读例外一路传到沙箱构建；Seatbelt 用追加在授权之后的 deny 规则、Linux 用只读 bind 实现例外。"本会话"开关放在线程扩展存储里；权限请求不带目录，所以什么也不会被持久化。

**Tech Stack:** Rust 2024；`orca-runtime`（bash 工具、终端服务、审批闸门、权限叠加层、工具路由）、`orca-tools` 沙箱（Seatbelt、bwrap）、`orca-tui` 的 surface 集成测试；`serde_json`、`tempfile`、`cargo-nextest`。

**Spec:** `docs/superpowers/specs/2026-09-28-git-metadata-approval-design.md`

## Global Constraints

- 基线：分支 `git-metadata-approval`，从 `e1a8dfef`（v0.5.3）开出；spec 已在 `563146f7` 提交。
- 只在 bash 沙箱为 `WorkspaceWrite` 时生效（auto-edit、suggest，目录已 trust）；plan、full-auto 行为不变。
- Windows 上不启用：`git_metadata_write_for_bash` 在 Windows 上恒为 `None`。
- 授权只对被批准的那一次执行生效：**不得**写入 `TurnPermissionOverlay` 的目录授权，也不得持久化为会话级目录授权。
- 单条授权时 `<git_dir>/config`、`<git_dir>/config.worktree`、`<git_dir>/hooks`、`<git_dir>/modules` 保持只读。
- 现有 `request_permissions`、权限 profile 的 `.git` 授权语义不变（整块放开）。
- 权限请求的 `id` 必须等于这次工具调用的 id；`permissions` 为 `RequestPermissionProfile { file_system: None, network: None }`。
- 不根据命令输出授权；`sandbox_diagnostic` 仍标注 non-authoritative、不授予任何权限。
- 代码注释、测试名沿用周边风格：英文，解释"为什么"。
- 每个任务结束运行该任务的定向测试再提交；Task 1、2、4 新增的 `pub(crate)` 项在 Task 5 接入前可能出现 `dead_code` 警告，属预期，Task 8 时必须清零。
- 不 push；Task 8 结束后由用户决定合并与发布。

---

### Task 1: git 命令识别器

**Files:**
- Create: `crates/orca-runtime/src/git_write_command.rs`
- Modify: `crates/orca-runtime/src/lib.rs`（注册模块）

**Interfaces:**
- Produces:
  - `pub(crate) enum GitCommandClass { ReadOnly, WritesMetadata, Refused, Unrecognized }`（`Clone, Copy, Debug, Eq, PartialEq`）
  - `pub(crate) fn classify_git_command(command: &str, cwd: &Path) -> GitCommandClass`

- [ ] **Step 1: 注册模块并写失败的测试**

在 `crates/orca-runtime/src/lib.rs` 的模块声明中（与 `pub(crate) mod runtime_approval;` 等相邻处）加入：

```rust
pub(crate) mod git_write_command;
```

创建 `crates/orca-runtime/src/git_write_command.rs`，先只放类型、一个恒返回 `ReadOnly` 的桩函数和测试：

```rust
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

pub(crate) fn classify_git_command(_command: &str, _cwd: &Path) -> GitCommandClass {
    GitCommandClass::ReadOnly
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
        assert_eq!(class("LANG=C git commit -m x"), GitCommandClass::WritesMetadata);
        assert_eq!(class("env LC_ALL=C git add ."), GitCommandClass::WritesMetadata);
        assert_eq!(class("command git commit -m x"), GitCommandClass::WritesMetadata);
        assert_eq!(class("time git gc"), GitCommandClass::WritesMetadata);
        assert_eq!(class("nice -n 5 git gc"), GitCommandClass::WritesMetadata);
        assert_eq!(class("/usr/bin/git commit -m x"), GitCommandClass::WritesMetadata);
        assert_eq!(class("git -C sub commit -m x"), GitCommandClass::WritesMetadata);
        assert_eq!(class("git -C ./sub/../sub status"), GitCommandClass::ReadOnly);
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p orca-runtime --lib git_write_command -- --test-threads=1`
Expected: FAIL。`reading_git_commands_and_other_commands_are_read_only` 通过（桩返回 `ReadOnly`），其余 5 个测试各在第一条用例处失败，例如 `left: ReadOnly, right: WritesMetadata`。

- [ ] **Step 3: 实现识别器**

用下面的实现替换桩函数 `classify_git_command`（测试模块保持不变）：

```rust
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
        "add" | "am" | "bisect" | "checkout" | "cherry-pick" | "commit" | "gc" | "merge"
        | "mv" | "rebase" | "reset" | "restore" | "revert" | "rm" | "switch"
        | "update-index" | "update-ref" => WritesMetadata,
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
        | "for-each-ref" | "rev-list" | "name-rev" | "merge-base" | "diff-tree"
        | "diff-index" | "diff-files" | "check-ignore" | "check-attr" | "count-objects"
        | "cherry" | "range-diff" | "format-patch" | "archive" | "whatchanged" | "var"
        | "help" | "version" | "fsck" => ReadOnly,
        _ => Unrecognized,
    }
}

fn branch_class(arguments: &[String]) -> GitCommandClass {
    const WRITES: &[&str] = &[
        "-d", "-D", "--delete", "-m", "-M", "--move", "-c", "-C", "--copy", "-f", "--force",
        "-u", "--set-upstream-to", "--unset-upstream", "--edit-description", "-t", "--track",
        "--no-track", "--create-reflog",
    ];
    const LISTS: &[&str] = &[
        "-l", "--list", "-a", "--all", "-r", "--remotes", "--contains", "--no-contains",
        "--merged", "--no-merged", "--points-at", "--show-current", "-v", "-vv", "--verbose",
        "--format", "--sort", "--column", "--no-column", "--color", "--no-color",
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
        "-d", "--delete", "-a", "--annotate", "-s", "--sign", "-u", "--local-user", "-f",
        "--force", "-m", "--message", "-F", "--file",
    ];
    const READS: &[&str] = &[
        "-l", "--list", "-v", "--verify", "-n", "--contains", "--no-contains", "--merged",
        "--no-merged", "--points-at", "--sort", "--format", "--column", "--no-column",
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
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p orca-runtime --lib git_write_command -- --test-threads=1`
Expected: PASS（6 passed）。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/git_write_command.rs
git commit -m "feat(runtime): recognize a bash command that writes git metadata"
```

---

### Task 2: 判定这次 bash 调用能否快速审批

**Files:**
- Modify: `crates/orca-runtime/src/git_write_command.rs`
- Modify: `crates/orca-runtime/src/runtime_normal_tool.rs:768`（`resolve_workdir` 改为 `pub(crate)`）

**Interfaces:**
- Consumes: Task 1 的 `classify_git_command`、`GitCommandClass`；`crate::server::bash_sandbox_for_cwd(config: &RunConfig, cwd: &Path) -> Result<CommandExecSandbox, String>`；`crate::runtime_normal_tool::resolve_workdir(base: &Path, workdir: Option<&Path>) -> Result<PathBuf, String>`。
- Produces:
  - `pub(crate) struct GitMetadataWrite { pub(crate) command: String, pub(crate) git_dir: PathBuf }`（`Clone, Debug, Eq, PartialEq`）
  - `pub(crate) fn git_metadata_write_for_bash(config: &RunConfig, workspace: &Path, request: &ToolRequest) -> Option<GitMetadataWrite>`
  - `pub(crate) fn read_only_git_metadata(git_dir: &Path) -> Vec<PathBuf>`，按顺序返回 `config`、`config.worktree`、`hooks`、`modules`
  - `pub(crate) fn git_metadata_write_reason(write: &GitMetadataWrite) -> String`（auto-edit 权限窗文字）
  - `pub(crate) fn git_metadata_approval_note(write: &GitMetadataWrite) -> String`（suggest 审批窗文字）

- [ ] **Step 1: 写失败的测试**

`crates/orca-runtime/src/runtime_normal_tool.rs:768`：把 `fn resolve_workdir(` 改为 `pub(crate) fn resolve_workdir(`。

在 `git_write_command.rs` 的 `use` 之后加入桩（让测试能编译）：

```rust
use orca_core::config::RunConfig;
use orca_core::tool_types::{ToolName, ToolRequest};

/// The `.git` a bash request would write, when one approval can let it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitMetadataWrite {
    pub(crate) command: String,
    pub(crate) git_dir: PathBuf,
}

pub(crate) fn git_metadata_write_for_bash(
    _config: &RunConfig,
    _workspace: &Path,
    _request: &ToolRequest,
) -> Option<GitMetadataWrite> {
    None
}
```

在文件末尾追加测试模块：

```rust
#[cfg(all(test, unix))]
mod approvable_tests {
    use std::path::{Path, PathBuf};

    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::RunConfig;
    use orca_core::config::folder_trust::{TrustLevel, set_trust_with_config_dir};
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

    #[test]
    fn untrusted_folders_subdirectories_partial_layouts_and_reads_cannot_be_approved() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let commit = bash(serde_json::json!({"command": "git commit -m x"}));

        // Untrusted: bash runs read-only.
        let untrusted = repository(home.path(), "untrusted");
        assert_eq!(
            git_metadata_write_for_bash(&config(&untrusted, ApprovalMode::AutoEdit), &untrusted, &commit),
            None
        );

        let root = repository(home.path(), "repo");
        set_trust_with_config_dir(&root, home.path(), TrustLevel::Trusted).unwrap();
        let auto_edit = config(&root, ApprovalMode::AutoEdit);

        // A subdirectory workdir: only it is writable, not the root's `.git`.
        std::fs::create_dir(root.join("sub")).unwrap();
        let in_sub = bash(serde_json::json!({"command": "git commit -m x", "workdir": "sub"}));
        assert_eq!(git_metadata_write_for_bash(&auto_edit, &root, &in_sub), None);

        // A read, a refused write, and another tool.
        let status = bash(serde_json::json!({"command": "git status"}));
        assert_eq!(git_metadata_write_for_bash(&auto_edit, &root, &status), None);
        let config_write = bash(serde_json::json!({"command": "git config core.hooksPath x"}));
        assert_eq!(git_metadata_write_for_bash(&auto_edit, &root, &config_write), None);
        let mut read_file = bash(serde_json::json!({"command": "git commit -m x"}));
        read_file.name = ToolName::ReadFile;
        assert_eq!(git_metadata_write_for_bash(&auto_edit, &root, &read_file), None);

        // Without `hooks`, the grant could not keep a new hooks directory
        // read-only on Linux.
        std::fs::remove_dir(root.join(".git/hooks")).unwrap();
        assert_eq!(git_metadata_write_for_bash(&auto_edit, &root, &commit), None);
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p orca-runtime --lib approvable_tests -- --test-threads=1`
Expected: FAIL。`a_git_write_at_the_root_of_a_trusted_repository_can_be_approved` 失败：`left: None, right: Some(GitMetadataWrite { … })`；另一个测试通过（桩恒返回 `None`）。

- [ ] **Step 3: 实现**

用下面的实现替换桩 `git_metadata_write_for_bash`，并加入另外三个函数：

```rust
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
    let is = |path: PathBuf, directory: bool| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            if directory {
                metadata.is_dir()
            } else {
                metadata.is_file()
            }
        })
    };
    (is(git_dir.clone(), true) && is(git_dir.join("config"), false) && is(git_dir.join("hooks"), true))
        .then(|| GitMetadataWrite {
            command: command.to_string(),
            git_dir,
        })
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
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p orca-runtime --lib git_write_command -- --test-threads=1 && cargo test -p orca-runtime --lib approvable_tests -- --test-threads=1`
Expected: PASS（识别器 6 个 + 判定 2 个）。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-runtime/src/git_write_command.rs crates/orca-runtime/src/runtime_normal_tool.rs
git commit -m "feat(runtime): tell when a bash git write can be approved for one run"
```

---

### Task 3: 沙箱在 `.git` 授权内保留只读路径

**Files:**
- Modify: `crates/orca-tools/src/sandbox/mod.rs`（`WorkspaceWriteSandboxCommandContext` 字段、`bash_command`、`bash_command_with_additional_roots`、Linux `workspace_write_bash_command`、Windows 测试的构造、Linux 测试）
- Modify: `crates/orca-tools/src/sandbox/seatbelt.rs`（`WorkspaceWriteProfileContext` 字段、`workspace_write_bash_command`、`build_workspace_write_profile`、新规则函数、所有构造处、测试）

**Interfaces:**
- Produces: `WorkspaceWriteSandboxCommandContext.metadata_read_only_paths: &'a [PathBuf]`——位于已授权元数据根里、仍须只读的路径。

- [ ] **Step 1: 加字段（行为不变）并写失败的测试**

`crates/orca-tools/src/sandbox/mod.rs` 的 `WorkspaceWriteSandboxCommandContext`，在 `metadata_writable_roots` 之后加入：

```rust
    /// Paths inside a granted metadata root that stay read-only (a git grant
    /// keeps `config` and `hooks` closed). Applied after the grant.
    pub metadata_read_only_paths: &'a [PathBuf],
```

给每一处 `WorkspaceWriteSandboxCommandContext {` 字面量加 `metadata_read_only_paths: &[],`（`mod.rs` 的 `bash_command`、`bash_command_with_additional_roots`、`windows_tests`；`seatbelt.rs` 第 84、103 行与测试中的各处；`crates/orca-runtime/src/shell_session.rs:518` 暂时也写 `&[]`，Task 4 再接实值）。用 `grep -rn 'WorkspaceWriteSandboxCommandContext {' crates` 找全。

`seatbelt.rs` 的 `WorkspaceWriteProfileContext`，在 `metadata_writable_roots` 之后加入 `metadata_read_only_paths: &'a [PathBuf],`，并给所有 `WorkspaceWriteProfileContext {` 字面量加 `metadata_read_only_paths: &[],`（第 148 行与测试中各处，`grep -n 'WorkspaceWriteProfileContext {' crates/orca-tools/src/sandbox/seatbelt.rs`）；`build_workspace_write_profile` 开头的解构 `let WorkspaceWriteProfileContext { … } = context;` 里加上 `metadata_read_only_paths,`（先不使用，加 `let _ = metadata_read_only_paths;`，Step 3 删除）。

在 `seatbelt.rs` 的 `mod tests` 中（`workspace_write_profile_allows_explicit_metadata_write_root` 之后）加入：

```rust
    #[test]
    fn workspace_write_profile_keeps_read_only_paths_inside_a_metadata_grant() {
        let workspace = TempDir::new().unwrap();
        let git_dir = workspace.path().join(".git");
        let read_only = [git_dir.join("config"), git_dir.join("hooks")];
        let profile = workspace_write_profile(WorkspaceWriteProfileContext {
            cwd: workspace.path(),
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: std::slice::from_ref(&git_dir),
            metadata_read_only_paths: &read_only,
            denied_roots: &[],
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        });
        let allow_git = format!(r#"(allow file-write* (subpath "{}"))"#, git_dir.display());
        for path in &read_only {
            let deny = format!(r#"(deny file-write* (subpath "{}"))"#, path.display());
            let deny_at = profile.find(&deny).unwrap_or_else(|| panic!("{deny} missing: {profile}"));
            assert!(
                deny_at > profile.find(&allow_git).unwrap(),
                "a read-only path must come after the grant it narrows: {profile}"
            );
        }
    }

    #[test]
    fn metadata_grant_writes_git_but_not_its_read_only_paths() {
        assert_seatbelt_available();
        let parent = crate::sandbox::sandbox_test_parent("seatbelt-git-grant-");
        let workspace = parent.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".git/hooks")).unwrap();
        std::fs::write(workspace.join(".git/config"), "[core]\n").unwrap();
        let git_dir = workspace.join(".git").canonicalize().unwrap();
        let read_only = [git_dir.join("config"), git_dir.join("hooks")];

        let output = workspace_write_bash_command(WorkspaceWriteSandboxCommandContext {
            command: "printf ok > .git/probe; printf x >> .git/config; printf y > .git/hooks/pre-commit; true",
            cwd: &workspace,
            readable_roots: &[],
            additional_roots: &[],
            metadata_writable_roots: std::slice::from_ref(&git_dir),
            metadata_read_only_paths: &read_only,
            denied_roots: &[],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
            allowed_unix_socket_roots: &[],
        })
        .output()
        .unwrap();

        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(std::fs::read_to_string(git_dir.join("probe")).unwrap(), "ok");
        assert_eq!(std::fs::read_to_string(git_dir.join("config")).unwrap(), "[core]\n");
        assert!(!git_dir.join("hooks/pre-commit").exists());
    }
```

在 `mod.rs` 的 `#[cfg(all(test, target_os = "linux"))] mod linux_tests`（位于 `mod platform` 内）加入：

```rust
        #[test]
        fn metadata_grant_writes_git_but_not_its_read_only_paths() {
            let parent = crate::sandbox::sandbox_test_parent("bwrap-git-grant-");
            let workspace = parent.path().join("workspace");
            std::fs::create_dir_all(workspace.join(".git/hooks")).unwrap();
            std::fs::write(workspace.join(".git/config"), "[core]\n").unwrap();
            if !crate::sandbox::linux::enforced_available(&workspace) {
                return;
            }
            let git_dir = workspace.join(".git").canonicalize().unwrap();
            let read_only = [git_dir.join("config"), git_dir.join("hooks")];

            let output = crate::sandbox::workspace_write_bash_command(
                WorkspaceWriteSandboxCommandContext {
                    command: "printf ok > .git/probe; printf x >> .git/config; printf y > .git/hooks/pre-commit; true",
                    cwd: &workspace,
                    readable_roots: &[],
                    additional_roots: &[],
                    metadata_writable_roots: std::slice::from_ref(&git_dir),
                    metadata_read_only_paths: &read_only,
                    denied_roots: &[],
                    network_access: false,
                    exclude_tmpdir_env_var: false,
                    exclude_slash_tmp: false,
                    allowed_unix_socket_roots: &[],
                },
            )
            .output()
            .unwrap();

            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(std::fs::read_to_string(git_dir.join("probe")).unwrap(), "ok");
            assert_eq!(std::fs::read_to_string(git_dir.join("config")).unwrap(), "[core]\n");
            assert!(!git_dir.join("hooks/pre-commit").exists());
        }
```

- [ ] **Step 2: 运行测试，确认失败**

Run（macOS）: `cargo test -p orca-tools --lib metadata_grant -- --test-threads=1 && cargo test -p orca-tools --lib keeps_read_only_paths -- --test-threads=1`
Expected: FAIL。profile 测试报 `(deny file-write* (subpath ".../.git/config")) missing`；执行测试报 `left: "[core]\nx"`（config 被写入）。Linux 上同名测试在有 bwrap 时同样失败。

- [ ] **Step 3: 实现**

`seatbelt.rs`：删除 Step 1 的 `let _ = metadata_read_only_paths;`；在 `append_metadata_write_allow_rules(&mut profile, metadata_writable_roots);` 之后紧接着加入 `append_metadata_read_only_rules(&mut profile, metadata_read_only_paths);`，并在 `append_metadata_write_allow_rules` 函数之后新增：

```rust
/// Paths inside a granted metadata root that stay read-only. Emitted after the
/// grant, so they win over it.
fn append_metadata_read_only_rules(profile: &mut SeatbeltProfileBuilder, paths: &[PathBuf]) {
    for path in paths {
        let path = profile.path_parameter("METADATA_READ_ONLY", path);
        profile.push_rule(format!("(deny file-write* (literal {path}))"));
        profile.push_rule(format!("(deny file-write* (subpath {path}))"));
    }
}
```

在 `seatbelt.rs` 的 `workspace_write_bash_command` 中（`canonical_metadata_writable_roots` 之后）加入：

```rust
    let canonical_metadata_read_only_paths = context
        .metadata_read_only_paths
        .iter()
        .map(|path| normalize_path_for_seatbelt(path))
        .collect::<Vec<_>>();
```

并把传给 `build_workspace_write_profile` 的 `metadata_read_only_paths: &[]` 改为 `metadata_read_only_paths: &canonical_metadata_read_only_paths`。

`mod.rs` 的 Linux `workspace_write_bash_command`：在构造 `read_only_roots` 的 `for name in PROTECTED_METADATA_DIRS { … }` 循环之后加入：

```rust
        // A granted `.git` keeps its config and hooks read-only. The ro-bind
        // covers only paths that exist, which the runtime checks before it
        // grants.
        for path in canonicalize_all(context.metadata_read_only_paths) {
            if !read_only_roots.contains(&path) {
                read_only_roots.push(path);
            }
        }
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p orca-tools --lib -- --test-threads=1`
Expected: PASS（全部 orca-tools 库测试，含新增的 2 个 macOS 测试；Linux 上含新增的 bwrap 测试）。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-tools/src/sandbox/mod.rs crates/orca-tools/src/sandbox/seatbelt.rs crates/orca-runtime/src/shell_session.rs
git commit -m "feat(sandbox): keep read-only paths inside a granted metadata root"
```

---

### Task 4: 单条授权一路传到沙箱

**Files:**
- Modify: `crates/orca-runtime/src/shell_session.rs`（`ShellSessionCommand` 字段；第 518 行传实值；所有字面量）
- Modify: `crates/orca-runtime/src/terminal_service.rs`（`TerminalExecRequest` 字段；`prepare_shell_command`；测试辅助与新测试）
- Modify: `crates/orca-runtime/src/runtime_normal_tool.rs:227`、`crates/orca-runtime/src/controller.rs:2267`、`crates/orca-runtime/src/server/command_exec_manager.rs` 及其他 `ShellSessionCommand {` / `TerminalExecRequest {` 字面量

**Interfaces:**
- Consumes: Task 2 的 `read_only_git_metadata`；Task 3 的 `metadata_read_only_paths`。
- Produces:
  - `ShellSessionCommand.metadata_read_only_paths: Vec<PathBuf>`
  - `TerminalExecRequest.git_metadata_grant: Option<&'a Path>`——本次执行被批准写入的 `.git`

- [ ] **Step 1: 加字段并写失败的测试**

`crates/orca-runtime/src/shell_session.rs` 的 `ShellSessionCommand`，在 `allowed_unix_socket_roots` 之后加入：

```rust
    /// Paths that stay read-only inside a one-shot `.git` grant, so a granted
    /// command cannot plant code that runs outside the sandbox later.
    pub metadata_read_only_paths: Vec<PathBuf>,
```

给所有 `ShellSessionCommand {` 字面量加 `metadata_read_only_paths: Vec::new(),`（`grep -rn 'ShellSessionCommand {' crates src`，共 23 处；`terminal_service.rs:1214` 这一处在 Step 3 改为实值）。

`shell_session.rs:518` 的 `WorkspaceWriteSandboxCommandContext` 中，把 Task 3 暂写的 `metadata_read_only_paths: &[]` 改为 `metadata_read_only_paths: &command.metadata_read_only_paths,`。

`crates/orca-runtime/src/terminal_service.rs` 的 `TerminalExecRequest`，在 `execution_deadline` 之后加入：

```rust
    /// A `.git` the user allowed this one command to write; see
    /// `crate::git_write_command`.
    pub(crate) git_metadata_grant: Option<&'a Path>,
```

给三处 `TerminalExecRequest {` 字面量（`runtime_normal_tool.rs:227`、`controller.rs:2267`、`terminal_service.rs` 测试辅助 `request_with_lifetime`）加 `git_metadata_grant: None,`。

在 `terminal_service.rs` 的测试模块中（`exec_returns_completed_output` 之前）加入：

```rust
    #[test]
    fn a_git_metadata_grant_opens_git_for_one_command_and_keeps_its_code_paths_read_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git_dir = temp.path().join(".git");
        let overlay = TurnPermissionOverlay::default();
        let mut exec = request("git commit -m x", temp.path(), &overlay, ShellTerminalMode::pipe());
        exec.git_metadata_grant = Some(&git_dir);

        let (command, metadata_writable_directories, _, _) =
            prepare_shell_command(exec).expect("prepare");

        assert_eq!(metadata_writable_directories, vec![git_dir.clone()]);
        assert_eq!(
            command.metadata_read_only_paths,
            vec![
                git_dir.join("config"),
                git_dir.join("config.worktree"),
                git_dir.join("hooks"),
                git_dir.join("modules"),
            ]
        );
        assert!(
            overlay.metadata_writable_directories().is_empty(),
            "the grant never enters the turn's overlay"
        );
    }

    #[test]
    fn without_a_git_metadata_grant_nothing_is_opened_or_narrowed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let overlay = TurnPermissionOverlay::default();

        let (command, metadata_writable_directories, _, _) = prepare_shell_command(request(
            "git status",
            temp.path(),
            &overlay,
            ShellTerminalMode::pipe(),
        ))
        .expect("prepare");

        assert!(metadata_writable_directories.is_empty());
        assert!(command.metadata_read_only_paths.is_empty());
    }
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p orca-runtime --lib git_metadata_grant -- --test-threads=1`
Expected: FAIL。第一个测试报 `left: [], right: [".../.git"]`；第二个通过。

- [ ] **Step 3: 实现**

`terminal_service.rs` 的 `prepare_shell_command`：在 `for root in request.permission_overlay.metadata_writable_directories() { … }` 之后加入：

```rust
    // One command's `.git` grant: it opens `.git` for this launch only and
    // never enters the turn's overlay.
    let metadata_read_only_paths = match request.git_metadata_grant {
        Some(git_dir) => {
            push_unique_path(&mut sandbox.metadata_writable_roots, git_dir.to_path_buf());
            crate::git_write_command::read_only_git_metadata(git_dir)
        }
        None => Vec::new(),
    };
```

并在函数末尾的 `ShellSessionCommand { … }` 中把 `metadata_read_only_paths: Vec::new(),` 改为 `metadata_read_only_paths,`。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p orca-runtime --lib git_metadata_grant -- --test-threads=1 && cargo test -p orca-runtime --lib terminal_service -- --test-threads=1`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add -u crates/
git commit -m "feat(runtime): carry a one-command .git grant to the sandbox"
```

---

### Task 5: auto-edit 下运行前询问，"本会话"开关

**Files:**
- Modify: `crates/orca-runtime/src/git_write_command.rs`（`GitMetadataWriteSession`）
- Modify: `crates/orca-runtime/src/runtime_tool_call.rs`（`RuntimeNormalToolInvocation` 字段、`snapshot`、`clone_for_request`、`normal_invocation`、新 builder）
- Modify: `crates/orca-runtime/src/tool_router.rs:366-392`（为 bash 接入开关）
- Modify: `crates/orca-runtime/src/runtime_normal_tool.rs`（`execute_bash` 询问与授权）
- Test: `crates/orca-tui/src/surface_client.rs`（端到端测试，放在含 `run_through_dispatch` 的测试模块）

**Interfaces:**
- Consumes: Task 2 的 `git_metadata_write_for_bash`、`git_metadata_write_reason`；Task 4 的 `TerminalExecRequest.git_metadata_grant`。
- Produces:
  - `pub(crate) struct GitMetadataWriteSession`，方法 `approved(&self) -> bool`、`approve(&self)`
  - `RuntimeNormalToolInvocation.git_metadata_write_session: Option<Arc<GitMetadataWriteSession>>` 与 `with_git_metadata_write_session(self, Option<Arc<GitMetadataWriteSession>>) -> Self`
  - 测试辅助 `git_sandbox_fixture`、`spawn_git_turn`、`wait_for_git_permission`、`assert_git_turn_succeeded`、`commit_count`（Task 6 复用）

- [ ] **Step 1: 写失败的端到端测试**

在 `crates/orca-tui/src/surface_client.rs` 中含 `fn run_through_dispatch` 的测试模块里（`typed_ordinary_turn_routes_permission_through_runtime_surface` 之后）加入辅助函数与两个测试：

```rust
    /// A trusted git repository under a private ORCA_HOME, for bash runs that
    /// need the real workspace sandbox. `None` when this host has no enforced
    /// sandbox or no git.
    #[cfg(unix)]
    fn git_sandbox_fixture(
        approval_mode: orca_core::approval_types::ApprovalMode,
    ) -> Option<(tempfile::TempDir, std::path::PathBuf, RunConfig)> {
        if orca_tools::sandbox::enforcement_state()
            != orca_core::capability::EnforcementState::Enforced
            || std::process::Command::new("git").arg("--version").output().is_err()
        {
            eprintln!("skipping git sandbox test: no enforced sandbox or no git on this host");
            return None;
        }
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "Orca Test"],
            &["config", "user.email", "orca@example.com"],
            &["config", "commit.gpgsign", "false"],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        // Some git installs skip the sample hooks; the grant needs the directory.
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        let repo = repo.canonicalize().unwrap();
        orca_core::config::folder_trust::set_trust_with_config_dir(
            &repo,
            home.path(),
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(repo.clone());
        config.history_mode = HistoryMode::Record;
        config.approval_mode = approval_mode;
        Some((home, repo, config))
    }

    #[cfg(unix)]
    fn spawn_git_turn(
        thread: &RuntimeThreadHandle,
        config: &RunConfig,
        controller: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
        prompt: &str,
    ) -> mpsc::Receiver<io::Result<TuiHostedOperationOutcome>> {
        let (result_tx, result_rx) = mpsc::bounded(1);
        let thread = thread.clone();
        let config = config.clone();
        let controller = controller.clone();
        let event_tx = event_tx.clone();
        let prompt = prompt.to_string();
        std::thread::spawn(move || {
            let result = run_through_dispatch(
                &thread,
                HostedTurnRequest::new(prompt.as_str()),
                config,
                &controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        result_rx
    }

    #[cfg(unix)]
    fn wait_for_git_permission(
        event_rx: &mpsc::Receiver<TuiEvent>,
    ) -> (crate::protocol::TuiInteractionKey, String) {
        loop {
            if let TuiEvent::PermissionApprovalNeeded { key, preview, .. } = event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("git write permission event")
            {
                return (key, preview.unwrap_or_default());
            }
        }
    }

    #[cfg(unix)]
    fn assert_git_turn_succeeded(result_rx: &mpsc::Receiver<io::Result<TuiHostedOperationOutcome>>) {
        let outcome = result_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("git turn finished without a second prompt")
            .expect("git turn");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
    }

    #[cfg(unix)]
    fn commit_count(repo: &std::path::Path) -> usize {
        let output = std::process::Command::new("git")
            .args(["rev-list", "--count", "--all"])
            .current_dir(repo)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0)
    }

    #[cfg(unix)]
    #[test]
    fn auto_edit_git_write_asks_once_and_opens_git_for_that_command_only() {
        let _guard = crate::test_support::lock_process_env();
        let Some((home, repo, config)) =
            git_sandbox_fixture(orca_core::approval_types::ApprovalMode::AutoEdit)
        else {
            return;
        };
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "git write approval")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();

        // The commit is approved; the hook write in the same command is not.
        let turn = spawn_git_turn(
            &thread,
            &config,
            &controller,
            &event_tx,
            "bash git commit --allow-empty -m first && printf x > .git/hooks/probe",
        );
        let (key, preview) = wait_for_git_permission(&event_rx);
        assert!(preview.contains(".git"), "{preview}");
        assert!(
            controller
                .respond_surface_interaction(
                    &key,
                    &crate::protocol::TuiInteractionResponse::Permission(
                        crate::protocol::TuiPermissionDecision::AllowOnce
                    )
                )
                .expect("permission response")
        );
        assert_git_turn_succeeded(&turn);
        assert_eq!(commit_count(&repo), 1);
        assert!(!repo.join(".git/hooks/probe").exists(), "hooks stay read-only");

        // The next command gets no grant and no prompt.
        let turn = spawn_git_turn(&thread, &config, &controller, &event_tx, "bash printf x > .git/probe");
        assert_git_turn_succeeded(&turn);
        assert!(!repo.join(".git/probe").exists(), "the grant covered one command");
        assert!(
            !event_rx
                .try_iter()
                .any(|event| matches!(event, TuiEvent::PermissionApprovalNeeded { .. }))
        );

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[cfg(unix)]
    #[test]
    fn denied_or_session_allowed_git_writes_follow_the_answer() {
        let _guard = crate::test_support::lock_process_env();
        let Some((home, repo, config)) =
            git_sandbox_fixture(orca_core::approval_types::ApprovalMode::AutoEdit)
        else {
            return;
        };
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "git write answers")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();
        let respond = |key: &crate::protocol::TuiInteractionKey,
                       decision: crate::protocol::TuiPermissionDecision| {
            assert!(
                controller
                    .respond_surface_interaction(
                        key,
                        &crate::protocol::TuiInteractionResponse::Permission(decision)
                    )
                    .expect("permission response")
            );
        };

        // Denied: nothing runs.
        let turn = spawn_git_turn(&thread, &config, &controller, &event_tx, "bash git commit --allow-empty -m denied");
        let (key, _) = wait_for_git_permission(&event_rx);
        respond(&key, crate::protocol::TuiPermissionDecision::Deny);
        let _ = turn.recv_timeout(Duration::from_secs(20)).expect("denied turn finished");
        assert_eq!(commit_count(&repo), 0);

        // Allowed for the session: the next git write does not ask.
        let turn = spawn_git_turn(&thread, &config, &controller, &event_tx, "bash git commit --allow-empty -m first");
        let (key, _) = wait_for_git_permission(&event_rx);
        respond(&key, crate::protocol::TuiPermissionDecision::AllowSession);
        assert_git_turn_succeeded(&turn);
        let turn = spawn_git_turn(&thread, &config, &controller, &event_tx, "bash git commit --allow-empty -m second");
        assert_git_turn_succeeded(&turn);
        assert_eq!(commit_count(&repo), 2);

        // The session switch is not a directory grant.
        let turn = spawn_git_turn(&thread, &config, &controller, &event_tx, "bash printf x > .git/probe");
        assert_git_turn_succeeded(&turn);
        assert!(!repo.join(".git/probe").exists());

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p orca-tui --lib git_write -- --test-threads=1`
Expected: FAIL。两个测试都在 `wait_for_git_permission` 处超时：`git write permission event: Timeout`（还没有任何询问）。

- [ ] **Step 3: 实现开关、接线与询问**

`git_write_command.rs` 追加：

```rust
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
        self.approved.store(true, std::sync::atomic::Ordering::Release);
    }
}
```

`runtime_tool_call.rs` 的 `RuntimeNormalToolInvocation`，在 `terminal_service` 之后加入：

```rust
    /// Whether this session lets git writes run without asking (bash only).
    pub(crate) git_metadata_write_session:
        Option<Arc<crate::git_write_command::GitMetadataWriteSession>>,
```

在 `snapshot()` 的 `Self { … }` 与测试辅助 `normal_invocation` 的字面量中加 `git_metadata_write_session: None,`；在 `clone_for_request` 中加 `git_metadata_write_session: self.git_metadata_write_session.clone(),`；在 `with_owner` 之前加入：

```rust
    pub(crate) fn with_git_metadata_write_session(
        mut self,
        session: Option<Arc<crate::git_write_command::GitMetadataWriteSession>>,
    ) -> Self {
        self.git_metadata_write_session = session;
        self
    }
```

`tool_router.rs`：在 `let terminal_service = … ;` 之后加入：

```rust
                // Whether this session already lets git writes run without
                // asking. It lives with the thread, like the terminal service.
                let git_metadata_write_session = (execution_request.name
                    == tool_types::ToolName::Bash)
                    .then(|| {
                        extension_stores
                            .map(|stores| stores.thread_store())
                            .unwrap_or(&self.runtime.thread_extensions)
                            .get_or_init(crate::git_write_command::GitMetadataWriteSession::default)
                    });
```

并把 `.with_terminal_service(terminal_service)` 之后接上 `.with_git_metadata_write_session(git_metadata_write_session)`。

`runtime_normal_tool.rs`：在 `use` 区加入

```rust
use crate::protocol::{PermissionGrantScope, RequestPermissionProfile};
use crate::runtime_permission::{
    RuntimePermissionContext, RuntimePermissionRequest, RuntimePermissionRequestHandler,
};
```

（与已有的 `use crate::protocol::PermissionResponseDecision;`、`use crate::runtime_permission::{…}` 合并）。在 `execute_bash` 中 `let Some(service) = invocation.terminal_service.as_ref() else { … };` 之后加入：

```rust
    let git_metadata_grant = match git_metadata_approval(invocation, context) {
        GitMetadataApproval::NotApplicable => None,
        GitMetadataApproval::Granted(git_dir) => Some(git_dir),
        GitMetadataApproval::Denied(git_dir) => {
            return ToolResult::denied(
                &invocation.request,
                format!(
                    "the user declined letting this git command write {}",
                    git_dir.display()
                ),
            );
        }
        GitMetadataApproval::Cancelled => {
            return ToolResult::cancelled_before_start(
                &invocation.request,
                "cancelled while waiting for approval to write .git",
            );
        }
        GitMetadataApproval::Failed(error) => {
            return ToolResult::failed_before_start(
                &invocation.request,
                format!("the approval to write .git failed: {error}"),
                None,
            );
        }
    };
```

把 `TerminalExecRequest { … git_metadata_grant: None, … }` 改为 `git_metadata_grant: git_metadata_grant.as_deref(),`。在 `execute_bash` 之后新增：

```rust
enum GitMetadataApproval {
    NotApplicable,
    Granted(PathBuf),
    Denied(PathBuf),
    Cancelled,
    Failed(String),
}

/// Asks, at most once per command, whether a recognized git write may run with
/// the workspace's `.git` writable. The grant covers this command only; see
/// `crate::git_write_command`.
fn git_metadata_approval(
    invocation: &RuntimeNormalToolInvocation,
    context: &RuntimeNormalToolWorkerContext<'_>,
) -> GitMetadataApproval {
    let Some(write) = crate::git_write_command::git_metadata_write_for_bash(
        &invocation.config,
        &invocation.cwd,
        &invocation.request,
    ) else {
        return GitMetadataApproval::NotApplicable;
    };
    if invocation
        .git_metadata_write_session
        .as_ref()
        .is_some_and(|session| session.approved())
    {
        return GitMetadataApproval::Granted(write.git_dir);
    }
    let Some(handler) = context.permission_handler else {
        return GitMetadataApproval::NotApplicable;
    };
    let request = RuntimePermissionRequest {
        // The runtime binds a foreground request to its tool call by this id.
        id: invocation.request.id.clone(),
        reason: Some(crate::git_write_command::git_metadata_write_reason(&write)),
        // No directory: the grant is applied to this one command here, and a
        // session answer must not persist a `.git` grant.
        permissions: RequestPermissionProfile {
            file_system: None,
            network: None,
        },
        context: RuntimePermissionContext::foreground(
            crate::surface::SurfacePermissionOrigin::Bash,
        ),
    };
    match handler.request_permissions(&request) {
        Ok(response) if response.decision == PermissionResponseDecision::Deny => {
            GitMetadataApproval::Denied(write.git_dir)
        }
        Ok(response) => {
            if response.scope == PermissionGrantScope::Session
                && let Some(session) = invocation.git_metadata_write_session.as_ref()
            {
                session.approve();
            }
            GitMetadataApproval::Granted(write.git_dir)
        }
        Err(_) if context.cancel.is_cancelled() => GitMetadataApproval::Cancelled,
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
            GitMetadataApproval::Cancelled
        }
        Err(error) => GitMetadataApproval::Failed(error.to_string()),
    }
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p orca-tui --lib git_write -- --test-threads=1 && cargo test -p orca-runtime --lib runtime_normal_tool -- --test-threads=1`
Expected: PASS（2 个端到端测试 + runtime_normal_tool 现有测试）。没有强制沙箱或没有 git 的主机会跳过这两个测试；在 macOS 或装了 bwrap 的 Linux 上再跑一次 `cargo test -p orca-tui --lib git_write -- --test-threads=1 --nocapture`，确认输出里**没有** `skipping git sandbox test`，即测试真正执行了。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add -u crates/
git commit -m "feat(runtime): ask once before a git write in auto-edit and grant it that run"
```

---

### Task 6: suggest 下并入原审批，只问一次

**Files:**
- Modify: `crates/orca-runtime/src/runtime_permission.rs:250`（`TurnPermissionOverlay` 标记）
- Modify: `crates/orca-runtime/src/tool_execution.rs:1375` 附近（`handle_approval` 的 `Ask` 分支）
- Modify: `crates/orca-runtime/src/runtime_normal_tool.rs`（`git_metadata_approval` 读取标记）
- Test: `crates/orca-tui/src/surface_client.rs`

**Interfaces:**
- Consumes: Task 2 的 `git_metadata_write_for_bash`、`git_metadata_approval_note`；Task 5 的测试辅助与 `git_metadata_approval`。
- Produces: `TurnPermissionOverlay::approve_git_metadata_write(&mut self, tool_call_id: &str)`、`TurnPermissionOverlay::git_metadata_write_approved(&self, tool_call_id: &str) -> bool`

- [ ] **Step 1: 写失败的端到端测试**

在 Task 5 的测试之后加入：

```rust
    #[cfg(unix)]
    #[test]
    fn suggest_names_git_in_its_approval_and_asks_only_once() {
        let _guard = crate::test_support::lock_process_env();
        let Some((home, repo, config)) =
            git_sandbox_fixture(orca_core::approval_types::ApprovalMode::Suggest)
        else {
            return;
        };
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "suggest git write")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();

        let turn = spawn_git_turn(&thread, &config, &controller, &event_tx, "bash git commit --allow-empty -m first");
        let (key, preview) = loop {
            match event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("approval event")
            {
                TuiEvent::ApprovalNeeded { key, preview, .. } => break (key, preview.unwrap_or_default()),
                TuiEvent::PermissionApprovalNeeded { .. } => {
                    panic!("suggest asked for .git separately from the command approval")
                }
                _ => {}
            }
        };
        assert!(preview.contains(".git"), "{preview}");
        assert!(
            controller
                .respond_surface_interaction(
                    &key,
                    &crate::protocol::TuiInteractionResponse::Approval(true)
                )
                .expect("approval response")
        );
        assert_git_turn_succeeded(&turn);
        assert_eq!(commit_count(&repo), 1);
        assert!(
            !event_rx
                .try_iter()
                .any(|event| matches!(event, TuiEvent::PermissionApprovalNeeded { .. }))
        );

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p orca-tui --lib suggest_names_git -- --test-threads=1`
Expected: FAIL。`preview.contains(".git")` 断言失败（审批预览为空），或在批准后弹出第二次 `PermissionApprovalNeeded`、导致 `git turn finished without a second prompt: Timeout`。

- [ ] **Step 3: 实现**

`runtime_permission.rs` 的 `TurnPermissionOverlay` 加字段（放在 `preapproved_tool_call_id` 之后）：

```rust
    /// Tool calls whose interactive approval named the `.git` they write, so
    /// bash does not ask a second time.
    git_metadata_write_approvals: Vec<String>,
```

在 `impl TurnPermissionOverlay` 中（`consume_preapproved_tool_call_id` 之后）加入：

```rust
    pub(crate) fn approve_git_metadata_write(&mut self, tool_call_id: &str) {
        if !self.git_metadata_write_approved(tool_call_id) {
            self.git_metadata_write_approvals
                .push(tool_call_id.to_string());
        }
    }

    pub(crate) fn git_metadata_write_approved(&self, tool_call_id: &str) -> bool {
        self.git_metadata_write_approvals
            .iter()
            .any(|approved| approved == tool_call_id)
    }
```

`tool_execution.rs` 的 `handle_approval` 中，`RuntimeApprovalDecision::Ask(mut approval) => {` 分支里，在 `if approval.preview.is_none() { … }` 之后加入：

```rust
                    // A git write is approved together with the `.git` it
                    // writes, so suggest asks once; bash reads the mark.
                    let git_metadata_write =
                        crate::git_write_command::git_metadata_write_for_bash(
                            config,
                            cwd,
                            &invocation.effective,
                        );
                    if let Some(write) = git_metadata_write.as_ref() {
                        approval.preview =
                            Some(crate::git_write_command::git_metadata_approval_note(write));
                    }
```

并把同一分支里的 `ApprovalDecision::Allow if event_error.is_none() => {}` 改为：

```rust
                        ApprovalDecision::Allow if event_error.is_none() => {
                            if git_metadata_write.is_some() {
                                permission_overlay.approve_git_metadata_write(&tool_request.id);
                            }
                        }
```

`runtime_normal_tool.rs` 的 `git_metadata_approval`：把"本会话已允许"的判断扩展为同时接受审批标记：

```rust
    // suggest's approval already named this `.git`, or the session allows it.
    if invocation
        .permission_overlay
        .git_metadata_write_approved(&invocation.request.id)
        || invocation
            .git_metadata_write_session
            .as_ref()
            .is_some_and(|session| session.approved())
    {
        return GitMetadataApproval::Granted(write.git_dir);
    }
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p orca-tui --lib git_write -- --test-threads=1 && cargo test -p orca-tui --lib suggest_names_git -- --test-threads=1 && cargo test -p orca-runtime --lib tool_execution -- --test-threads=1`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add -u crates/
git commit -m "feat(runtime): let suggest approve a git write and its .git in one prompt"
```

---

### Task 7: 兜底提示点名 `request_permissions`

**Files:**
- Modify: `crates/orca-runtime/src/sandbox_denial.rs`（`build_message`、测试）

**Interfaces:**
- Consumes: `orca_tools::sandbox::is_protected_metadata_root(path: &Path) -> bool`。

- [ ] **Step 1: 写失败的测试**

在 `sandbox_denial.rs` 的 `diagnoses_git_index_lock_as_sandbox_denial_not_stale_lock` 末尾加入：

```rust
        assert!(
            diagnostic
                .message
                .contains(r#"call request_permissions with fileSystem.write ["/repo/.git"]"#),
            "{}",
            diagnostic.message
        );
```

在 `forged_process_output_remains_explanatory_only` 末尾加入：

```rust
        // Only protected metadata gets the hint; `/etc` is not workspace metadata.
        assert!(!diagnostic.message.contains("request_permissions"));
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p orca-runtime --lib sandbox_denial -- --test-threads=1`
Expected: FAIL。`diagnoses_git_index_lock_as_sandbox_denial_not_stale_lock` 在新断言处失败；另一个测试通过。

- [ ] **Step 3: 实现**

`build_message` 中，把

```rust
    if let Some(root) = suggested_root {
        parts.push(format!("suggested write root: {}", root.display()));
    }
```

改为

```rust
    if let Some(root) = suggested_root {
        parts.push(format!("suggested write root: {}", root.display()));
        // Metadata stays read-only in the sandbox; this names the way to ask.
        if orca_tools::sandbox::is_protected_metadata_root(root) {
            parts.push(format!(
                "to allow it, call request_permissions with fileSystem.write [\"{}\"]",
                root.display()
            ));
        }
    }
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p orca-runtime --lib sandbox_denial -- --test-threads=1`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-runtime/src/sandbox_denial.rs
git commit -m "fix(runtime): name request_permissions when the sandbox keeps .git read-only"
```

---

### Task 8: 完整验证与收尾

**Files:**
- Modify: `docs/superpowers/specs/2026-09-28-git-metadata-approval-design.md`（状态行）

- [ ] **Step 1: 格式与 clippy**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked 2>/tmp/git-approval-clippy.log; echo "clippy exit=$?"`
Expected: fmt 无输出；clippy exit=0。然后确认没有任何警告落在本分支改动的行上：

```bash
python3 - <<'EOF'
import re, subprocess
log = open('/tmp/git-approval-clippy.log').read()
locations = re.findall(r'--> ([^:\s]+):(\d+):\d+', log)
diff = subprocess.run(['git', 'diff', '-U0', 'e1a8dfef'], capture_output=True, text=True).stdout
ranges, current = {}, None
for line in diff.splitlines():
    if line.startswith('+++ b/'):
        current = line[6:]
    match = re.match(r'@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@', line)
    if match and current:
        start, count = int(match.group(1)), int(match.group(2) or 1)
        ranges.setdefault(current, []).append((start, start + max(count, 1) - 1))
hits = sorted({(f, int(l)) for f, l in locations if any(a <= int(l) <= b for a, b in ranges.get(f, []))})
print('warnings on changed lines:', hits)
EOF
```

Expected: `warnings on changed lines: []`。

- [ ] **Step 2: 完整测试**

Run: `cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0 && cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`
Expected: 全部通过（v0.5.3 时为 3456 个测试，本分支新增约 15 个）。

- [ ] **Step 3: 更新 spec 状态行**

把 spec 第 3 行改为：

```markdown
> 状态：已实现（分支 `git-metadata-approval`，2026-09-28），待合并。基线 `e1a8dfef`（v0.5.3）。
```

- [ ] **Step 4: 提交**

```bash
git add docs/superpowers/specs/2026-09-28-git-metadata-approval-design.md
git commit -m "docs(spec): mark the git write approval as implemented"
```

不 push；由用户决定合并到 main 与发版。
