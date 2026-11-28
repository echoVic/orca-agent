use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::{open_workspace_file, workspace_read::open_directory};

pub(crate) struct WorkspaceIgnores {
    root: PathBuf,
    global: Option<(PathBuf, Gitignore)>,
    directories: HashMap<PathBuf, DirectoryRules>,
    ancestors: Vec<DirectoryRules>,
}

struct DirectoryRules {
    rules: [Option<Gitignore>; 3],
    git: bool,
    exclude: Option<(PathBuf, Gitignore)>,
}

impl WorkspaceIgnores {
    pub(crate) fn new(root: PathBuf, global: Option<&Path>) -> Self {
        let global = global.and_then(|file| {
            let path = file.canonicalize().ok()?;
            let rules = read_rules(&root, path.parent()?, &path)?;
            Some((path, rules))
        });
        let directory = DirectoryRules::load(&root, &root);
        let mut ancestors = Vec::new();
        if !directory.git
            && let Some((repo, repository)) = root.ancestors().skip(1).find_map(|ancestor| {
                let rules = DirectoryRules::git_rules(ancestor, ancestor, [None, None, None]);
                rules.git.then_some((ancestor, rules))
            })
        {
            for ancestor in root.ancestors().skip(1) {
                let rules = [None, Some(".ignore"), Some(".gitignore")]
                    .map(|name| read_rules(ancestor, ancestor, &ancestor.join(name?)));
                if ancestor == repo {
                    ancestors.push(DirectoryRules {
                        rules,
                        ..repository
                    });
                    break;
                }
                ancestors.push(DirectoryRules {
                    rules,
                    git: false,
                    exclude: None,
                });
            }
        }
        let directories = HashMap::from([(root.clone(), directory)]);
        Self {
            root,
            global,
            directories,
            ancestors,
        }
    }

    pub(crate) fn read_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        roots.extend(self.global.iter().map(|(path, _)| path.clone()));
        roots.extend(
            self.directories[&self.root]
                .exclude
                .iter()
                .map(|(path, _)| path.clone()),
        );
        roots.extend(
            self.ancestors
                .iter()
                .filter_map(|rules| rules.exclude.as_ref())
                .map(|(path, _)| path.clone()),
        );
        roots
    }

    /// Check the entry and the directories a recursive search walks through.
    pub(crate) fn ignored_below(&mut self, base: &Path, path: &Path) -> bool {
        self.entry_ignored(path, false)
            || path
                .ancestors()
                .skip(1)
                .take_while(|parent| *parent != base)
                .any(|parent| self.entry_ignored(parent, true))
    }

    pub(crate) fn entry_ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        if !path.starts_with(&self.root) {
            return true;
        }
        let parents = || {
            path.ancestors()
                .skip(1)
                .take_while(|parent| parent.starts_with(&self.root))
        };
        for parent in parents() {
            self.directories
                .entry(parent.to_owned())
                .or_insert_with(|| DirectoryRules::load(&self.root, parent));
        }
        if path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
        {
            return true;
        }
        let mut matches = [None; 4];
        let mut in_git = false;
        for rules in parents()
            .map(|parent| &self.directories[parent])
            .chain(&self.ancestors)
        {
            for (index, matcher) in rules.rules.iter().enumerate() {
                if index < 2 || !in_git {
                    first_match(&mut matches[index], matcher.as_ref(), path, is_dir);
                }
            }
            if !in_git {
                first_match(
                    &mut matches[3],
                    rules.exclude.as_ref().map(|(_, rules)| rules),
                    path,
                    is_dir,
                );
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
            first_match(
                &mut global_match,
                self.global.as_ref().map(|(_, rules)| rules),
                path,
                is_dir,
            );
        }
        global_match.unwrap_or(false)
    }
}

impl DirectoryRules {
    fn load(root: &Path, directory: &Path) -> Self {
        let rules = [".rgignore", ".ignore", ".gitignore"]
            .map(|name| read_rules(directory, root, &directory.join(name)));
        Self::git_rules(root, directory, rules)
    }

    fn git_rules(root: &Path, directory: &Path, rules: [Option<Gitignore>; 3]) -> Self {
        let dot_git = directory.join(".git");
        let (git, common) = match dot_git.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() && open_directory(root, &dot_git).is_ok() => {
                (true, Some(dot_git))
            }
            Ok(metadata) if metadata.is_file() => match first_line(root, &dot_git) {
                Some(line) if line.starts_with("gitdir: ") => (
                    true,
                    associated_git_dir(root, directory, &dot_git, &line[8..]),
                ),
                _ => (false, None),
            },
            _ => (false, None),
        };
        let exclude = common.and_then(|common| {
            let path = common.join("info/exclude");
            let rules = read_rules(directory, &common, &path)?;
            Some((path, rules))
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
) -> Option<PathBuf> {
    let git_dir = directory.join(pointer).canonicalize().ok()?;
    let worktrees = git_dir
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "worktrees"));
    if let Some(common) = worktrees.and_then(Path::parent) {
        let backlink = first_line(&git_dir, &git_dir.join("gitdir"));
        if backlink
            .is_some_and(|path| git_dir.join(path).canonicalize().ok().as_deref() == Some(dot_git))
        {
            return Some(common.to_owned());
        }
    }
    if git_dir.starts_with(root) || core_worktree(&git_dir).as_deref() == Some(directory) {
        return Some(git_dir);
    }
    None
}

fn first_line(root: &Path, path: &Path) -> Option<String> {
    let file = open_workspace_file(root, path).ok()?;
    let mut line = String::new();
    let mut input = BufReader::new(file).take(8192);
    input.read_line(&mut line).ok()?;
    if input.limit() == 0 {
        return None;
    }
    Some(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn core_worktree(git_dir: &Path) -> Option<PathBuf> {
    let config = open_workspace_file(git_dir, &git_dir.join("config")).ok()?;
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

fn read_rules(base: &Path, root: &Path, path: &Path) -> Option<Gitignore> {
    let file: File = open_workspace_file(root, path).ok()?;
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
