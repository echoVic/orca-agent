use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use orca_platform::fs::VerifiedDirectory;

use crate::{
    open_workspace_file,
    workspace_read::{open_directory, open_file_in},
};

pub(crate) struct WorkspaceIgnores<'a> {
    root: &'a Path,
    global: Option<Gitignore>,
    ancestors: Vec<DirectoryRules>,
}

pub(crate) struct DirectoryRules {
    rules: [Option<Gitignore>; 3],
    git: bool,
    exclude: Option<Gitignore>,
}

pub(crate) struct IgnoreDirectory<'a> {
    pub rules: DirectoryRules,
    pub parent: Option<&'a IgnoreDirectory<'a>>,
}

impl<'a> WorkspaceIgnores<'a> {
    pub(crate) fn new(root: &'a Path, base: &Path, global: Option<&Path>) -> Self {
        let global = global.and_then(|file| {
            let path = file.canonicalize().ok()?;
            read_rules(
                root,
                &path,
                open_workspace_file(path.parent()?, &path).ok()?,
            )
        });
        let mut ancestors = Vec::new();
        for parent in base
            .ancestors()
            .skip(1)
            .take_while(|path| path.starts_with(root))
        {
            if let Ok(directory) = open_directory(root, parent) {
                ancestors.push(DirectoryRules::load(root, parent, &directory));
            }
        }
        if !repo_marker(root)
            && let Some(repo) = root
                .ancestors()
                .skip(1)
                .find(|ancestor| repo_marker(ancestor))
        {
            for ancestor in root.ancestors().skip(1) {
                if let Ok(directory) = open_directory(ancestor, ancestor) {
                    let rules = [None, Some(".ignore"), Some(".gitignore")]
                        .map(|name| read_rule_in(&directory, ancestor, name?));
                    ancestors.push(if ancestor == repo {
                        DirectoryRules::git_rules(repo, repo, rules)
                    } else {
                        DirectoryRules {
                            rules,
                            git: false,
                            exclude: None,
                        }
                    });
                }
                if ancestor == repo {
                    break;
                }
            }
        }
        Self {
            root,
            global,
            ancestors,
        }
    }

    pub(crate) fn directory(&self, path: &Path, handle: &VerifiedDirectory) -> DirectoryRules {
        DirectoryRules::load(self.root, path, handle)
    }

    pub(crate) fn entry_ignored(
        &self,
        path: &Path,
        is_dir: bool,
        current: &IgnoreDirectory<'_>,
    ) -> bool {
        let local =
            std::iter::successors(Some(current), |entry| entry.parent).map(|entry| &entry.rules);
        let mut matches = [None; 4];
        let mut in_git = false;
        for rules in local.chain(&self.ancestors) {
            for (index, matcher) in rules.rules.iter().enumerate() {
                if index < 2 || !in_git {
                    first_match(&mut matches[index], matcher.as_ref(), path, is_dir);
                }
            }
            if !in_git {
                first_match(&mut matches[3], rules.exclude.as_ref(), path, is_dir);
            }
            in_git |= rules.git;
        }
        if !in_git {
            matches[2..].fill(None);
        }
        if let Some(ignored) = matches.into_iter().flatten().next() {
            return ignored;
        }
        let mut global_match = None;
        if in_git {
            first_match(&mut global_match, self.global.as_ref(), path, is_dir);
        }
        global_match.unwrap_or(false)
    }
}

fn repo_marker(directory: &Path) -> bool {
    directory
        .join(".git")
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_dir() || meta.is_file())
}

impl DirectoryRules {
    fn load(root: &Path, directory: &Path, handle: &VerifiedDirectory) -> Self {
        let rules = [".rgignore", ".ignore", ".gitignore"]
            .map(|name| read_rule_in(handle, directory, name));
        Self::git_rules(root, directory, rules)
    }

    fn git_rules(root: &Path, directory: &Path, rules: [Option<Gitignore>; 3]) -> Self {
        let dot_git = directory.join(".git");
        let (git, common) = match dot_git.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => {
                let git = open_directory(root, &dot_git).ok();
                (git.is_some(), git.map(|handle| (dot_git, handle)))
            }
            Ok(metadata) if metadata.is_file() => match open_workspace_file(root, &dot_git)
                .ok()
                .and_then(first_line)
            {
                Some(line) if line.starts_with("gitdir: ") => (
                    true,
                    associated_git_dir(root, directory, &dot_git, &line[8..]),
                ),
                _ => (false, None),
            },
            _ => (false, None),
        };
        let exclude = common.and_then(|(common, handle)| {
            let file =
                open_file_in(&handle.child("info".as_ref()).ok()?, "exclude".as_ref()).ok()?;
            read_rules(directory, &common.join("info/exclude"), file)
        });
        Self {
            rules,
            git,
            exclude,
        }
    }
}

fn associated_git_dir(
    root: &Path,
    directory: &Path,
    dot_git: &Path,
    pointer: &str,
) -> Option<(PathBuf, VerifiedDirectory)> {
    let git_dir = directory.join(pointer).canonicalize().ok()?;
    let worktrees = git_dir
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "worktrees"));
    if let Some(common) = worktrees.and_then(Path::parent)
        && let Ok(handle) = open_directory(common, common)
        && let Ok(record) = handle
            .child("worktrees".as_ref())
            .and_then(|dir| dir.child(git_dir.file_name().expect("worktree record name")))
        && let Ok(backlink) = open_file_in(&record, "gitdir".as_ref())
        && first_line(backlink)
            .is_some_and(|path| git_dir.join(path).canonicalize().ok().as_deref() == Some(dot_git))
    {
        return Some((common.to_owned(), handle));
    }
    let handle = open_directory(&git_dir, &git_dir).ok()?;
    if git_dir.starts_with(root) || core_worktree(&git_dir, &handle).as_deref() == Some(directory) {
        return Some((git_dir, handle));
    }
    None
}

fn first_line(file: File) -> Option<String> {
    let mut line = String::new();
    let mut input = BufReader::new(file).take(8192);
    input.read_line(&mut line).ok()?;
    if input.limit() == 0 {
        return None;
    }
    Some(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn core_worktree(git_dir: &Path, handle: &VerifiedDirectory) -> Option<PathBuf> {
    let config = open_file_in(handle, "config".as_ref()).ok()?;
    let mut in_core = false;
    let mut worktree = None;
    // Only this repository's own setting can associate its excludes with a worktree.
    // Includes can point elsewhere and are deliberately not read.
    const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
    let mut contents = String::new();
    config
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut contents)
        .ok()?;
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return None;
    }
    for line in contents.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if let Some((section, _)) = line.strip_prefix('[').and_then(|line| line.split_once(']')) {
            in_core = section.trim().eq_ignore_ascii_case("core");
        } else if in_core
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("worktree")
        {
            worktree = config_path(value);
        }
    }
    git_dir.join(worktree?).canonicalize().ok()
}

fn config_path(value: &str) -> Option<String> {
    let mut path = String::new();
    let mut quoted = false;
    let mut chars = value.trim_start().chars();
    let mut end = 0;
    while let Some(ch) = chars.next() {
        let ch = match ch {
            '"' => {
                quoted = !quoted;
                continue;
            }
            '#' | ';' if !quoted => break,
            '\\' => match chars.next()? {
                ch @ ('\\' | '"') => ch,
                'n' => '\n',
                't' => '\t',
                'b' => '\u{8}',
                _ => return None,
            },
            ch => ch,
        };
        path.push(ch);
        if quoted || !ch.is_whitespace() {
            end = path.len();
        }
    }
    path.truncate(end);
    (!quoted && !path.is_empty()).then_some(path)
}

fn first_match(result: &mut Option<bool>, matcher: Option<&Gitignore>, path: &Path, is_dir: bool) {
    if result.is_none()
        && let Some(matcher) = matcher
    {
        let matched = matcher.matched(path, is_dir);
        if !matched.is_none() {
            *result = Some(matched.is_ignore());
        }
    }
}

fn read_rule_in(directory: &VerifiedDirectory, base: &Path, name: &str) -> Option<Gitignore> {
    let file = open_file_in(directory, name.as_ref()).ok()?;
    read_rules(base, &base.join(name), file)
}

fn read_rules(base: &Path, path: &Path, file: File) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(base);
    for (index, line) in BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .enumerate()
    {
        let pattern = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(&line)
        } else {
            &line
        };
        let _ = builder.add_line(Some(path.to_owned()), pattern);
    }
    builder.build().ok()
}
