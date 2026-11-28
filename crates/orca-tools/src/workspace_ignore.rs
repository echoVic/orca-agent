use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::open_workspace_file;

pub(crate) struct WorkspaceIgnores {
    root: PathBuf,
    global: Option<(PathBuf, Gitignore)>,
    directories: HashMap<PathBuf, DirectoryRules>,
    ancestor_exclude: Option<DirectoryRules>,
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
        let ancestor_exclude = (!directory.git)
            .then(|| {
                root.ancestors().skip(1).find_map(|ancestor| {
                    let rules = DirectoryRules::git_rules(&root, ancestor, [None, None, None]);
                    rules.git.then_some(rules)
                })
            })
            .flatten();
        let directories = HashMap::from([(root.clone(), directory)]);
        Self {
            root,
            global,
            directories,
            ancestor_exclude,
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
            self.ancestor_exclude
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
        let mut matches = [None; 4];
        let mut in_git = false;
        for parent in parents() {
            let rules = &self.directories[parent];
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
        if !in_git && let Some(repository) = self.ancestor_exclude.as_ref() {
            first_match(
                &mut matches[3],
                repository.exclude.as_ref().map(|(_, rules)| rules),
                path,
                is_dir,
            );
            in_git = true;
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
        global_match.unwrap_or_else(|| {
            path.file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
        })
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
        let common = dot_git.symlink_metadata().ok().and_then(|metadata| {
            if metadata.is_dir() {
                Some(dot_git)
            } else if metadata.is_file() {
                linked_worktree_common_dir(root, directory, &dot_git)
            } else {
                None
            }
        });
        let git = common.is_some();
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

fn linked_worktree_common_dir(root: &Path, directory: &Path, dot_git: &Path) -> Option<PathBuf> {
    let file = open_workspace_file(root, dot_git).ok()?;
    let mut line = String::new();
    BufReader::new(file).take(8192).read_line(&mut line).ok()?;
    let git_dir = line.trim_end().strip_prefix("gitdir: ")?;
    let git_dir = directory.join(git_dir).canonicalize().ok()?;
    // Git places linked worktree records in <common dir>/worktrees/<id>.
    // Deriving the common directory avoids opening another pointer outside the workspace.
    let worktrees = git_dir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    Some(worktrees.parent()?.to_owned())
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
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let _ = builder.add_line(Some(path.to_owned()), &line);
    }
    builder.build().ok()
}
