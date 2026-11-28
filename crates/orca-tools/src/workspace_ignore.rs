use std::cell::RefCell;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use orca_platform::fs::VerifiedDirectory;

use crate::{
    open_workspace_file,
    workspace_read::{open_directory, open_file_in},
};

pub(crate) struct IgnoreReads<'a> {
    pub stop: &'a dyn Fn() -> bool,
    failure: RefCell<Option<io::Error>>,
}

impl<'a> IgnoreReads<'a> {
    pub fn new(stop: &'a dyn Fn() -> bool) -> Self {
        Self {
            stop,
            failure: RefCell::new(None),
        }
    }

    fn optional<T>(&self, result: io::Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                if crate::grep::is_resource_failure(&error) {
                    self.failure.replace(Some(error));
                }
                None
            }
        }
    }

    pub fn check(&self) -> io::Result<()> {
        self.failure.take().map_or(Ok(()), Err)
    }
}

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
    pub(crate) fn new(
        root: &'a Path,
        base: &Path,
        global: Option<&Path>,
        reads: &IgnoreReads<'_>,
    ) -> Self {
        let global = global.and_then(|file| {
            let path = reads.optional(file.canonicalize())?;
            read_rules(
                root,
                &path,
                reads.optional(open_workspace_file(path.parent()?, &path))?,
                reads,
            )
        });
        let mut ancestors = Vec::new();
        for parent in base
            .ancestors()
            .skip(1)
            .take_while(|path| path.starts_with(root))
        {
            if (reads.stop)() {
                break;
            }
            if let Some(directory) = reads.optional(open_directory(root, parent)) {
                ancestors.push(DirectoryRules::load(root, parent, &directory, reads));
            }
        }
        if !repo_marker(root)
            && let Some(repo) = root
                .ancestors()
                .skip(1)
                .take_while(|_| !(reads.stop)())
                .find(|ancestor| repo_marker(ancestor))
        {
            for ancestor in root.ancestors().skip(1).take_while(|_| !(reads.stop)()) {
                if let Some(directory) = reads.optional(open_directory(ancestor, ancestor)) {
                    let rules = [None, Some(".ignore"), Some(".gitignore")]
                        .map(|name| read_rule_in(&directory, ancestor, name?, reads));
                    ancestors.push(if ancestor == repo {
                        DirectoryRules::git_rules(repo, repo, rules, reads)
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

    pub(crate) fn directory(
        &self,
        path: &Path,
        handle: &VerifiedDirectory,
        reads: &IgnoreReads<'_>,
    ) -> DirectoryRules {
        DirectoryRules::load(self.root, path, handle, reads)
    }

    pub(crate) fn entry_ignored(
        &self,
        path: &Path,
        is_dir: bool,
        hidden: bool,
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
        global_match.unwrap_or(hidden)
    }
}

fn repo_marker(directory: &Path) -> bool {
    directory
        .join(".git")
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_dir() || meta.is_file())
}

impl DirectoryRules {
    fn load(
        root: &Path,
        directory: &Path,
        handle: &VerifiedDirectory,
        reads: &IgnoreReads<'_>,
    ) -> Self {
        let rules = [".rgignore", ".ignore", ".gitignore"]
            .map(|name| read_rule_in(handle, directory, name, reads));
        Self::git_rules(root, directory, rules, reads)
    }

    fn git_rules(
        root: &Path,
        directory: &Path,
        rules: [Option<Gitignore>; 3],
        reads: &IgnoreReads<'_>,
    ) -> Self {
        let dot_git = directory.join(".git");
        let (git, common) = match dot_git.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => {
                let git = reads.optional(open_directory(root, &dot_git));
                (git.is_some(), git.map(|handle| (dot_git, handle)))
            }
            Ok(metadata) if metadata.is_file() => match reads
                .optional(open_workspace_file(root, &dot_git))
                .and_then(|file| first_line(file, reads))
            {
                Some(line) if line.starts_with("gitdir: ") => (
                    true,
                    associated_git_dir(root, directory, &dot_git, &line[8..], reads),
                ),
                _ => (false, None),
            },
            _ => (false, None),
        };
        let exclude = common.and_then(|(common, handle)| {
            let file = reads.optional(open_file_in(
                &reads.optional(handle.child_for_read("info".as_ref()))?,
                "exclude".as_ref(),
            ))?;
            read_rules(directory, &common.join("info/exclude"), file, reads)
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
    reads: &IgnoreReads<'_>,
) -> Option<(PathBuf, VerifiedDirectory)> {
    let git_dir = reads.optional(directory.join(pointer).canonicalize())?;
    let worktrees = git_dir
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "worktrees"));
    if let Some(common) = worktrees.and_then(Path::parent)
        && let Some(handle) = reads.optional(open_directory(common, common))
        && let Some(record) =
            reads.optional(handle.child_for_read("worktrees".as_ref()).and_then(|dir| {
                dir.child_for_read(git_dir.file_name().expect("worktree record name"))
            }))
        && let Some(backlink) = reads.optional(open_file_in(&record, "gitdir".as_ref()))
        && first_line(backlink, reads).is_some_and(|path| {
            reads.optional(git_dir.join(path).canonicalize()).as_deref() == Some(dot_git)
        })
    {
        return Some((common.to_owned(), handle));
    }
    let handle = reads.optional(open_directory(&git_dir, &git_dir))?;
    if git_dir.starts_with(root)
        || core_worktree(&git_dir, &handle, reads).as_deref() == Some(directory)
    {
        return Some((git_dir, handle));
    }
    None
}

fn first_line(file: File, reads: &IgnoreReads<'_>) -> Option<String> {
    let mut line = String::new();
    let mut input = BufReader::new(file).take(8192);
    reads.optional(input.read_line(&mut line))?;
    if input.limit() == 0 {
        return None;
    }
    Some(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn core_worktree(
    git_dir: &Path,
    handle: &VerifiedDirectory,
    reads: &IgnoreReads<'_>,
) -> Option<PathBuf> {
    let config = reads.optional(open_file_in(handle, "config".as_ref()))?;
    reads.optional(
        git_dir
            .join(core_config_path(config, "worktree", reads)?)
            .canonicalize(),
    )
}

pub(crate) fn global_excludes_path(workspace: &Path, reads: &IgnoreReads<'_>) -> Option<PathBuf> {
    let home = dirs::home_dir();
    let env_path = |name| {
        std::env::var_os(name)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    };
    let xdg = env_path("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|p| p.join(".config")));
    let paths = [
        env_path("GIT_CONFIG_GLOBAL"),
        home.as_ref().map(|p| p.join(".gitconfig")),
        xdg.as_ref().map(|p| p.join("git/config")),
        Some(env_path("GIT_CONFIG_SYSTEM").unwrap_or_else(|| PathBuf::from("/etc/gitconfig"))),
    ];
    paths
        .into_iter()
        .flatten()
        .take_while(|_| !(reads.stop)())
        .find_map(|path| {
            let path = workspace.join(path);
            let parent = reads.optional(path.parent()?.canonicalize())?;
            let directory = reads.optional(VerifiedDirectory::open_for_read(&parent))?;
            let file = reads.optional(open_file_in(&directory, path.file_name()?))?;
            let path = core_config_path(file, "excludesfile", reads)?;
            if let Some(relative) = path.strip_prefix("~/") {
                return Some(home.as_ref()?.join(relative));
            }
            Some(workspace.join(path))
        })
        .or_else(|| xdg.map(|path| workspace.join(path).join("git/ignore")))
}

fn core_config_path(file: File, key_name: &str, reads: &IgnoreReads<'_>) -> Option<String> {
    let contents = read_bounded(file, reads)?;
    let contents = std::str::from_utf8(&contents).ok()?;
    let mut in_core = false;
    let mut path = None;
    for line in contents.trim_start_matches('\u{feff}').lines() {
        if (reads.stop)() {
            return None;
        }
        let line = line.trim();
        if let Some((section, _)) = line.strip_prefix('[').and_then(|line| line.split_once(']')) {
            in_core = section.trim().eq_ignore_ascii_case("core");
        } else if in_core
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case(key_name)
        {
            path = config_path(value);
        }
    }
    path
}

fn read_bounded(mut file: File, reads: &IgnoreReads<'_>) -> Option<Vec<u8>> {
    const MAX_BYTES: usize = 1024 * 1024;
    let size = reads.optional(file.metadata())?.len();
    if size > MAX_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    while bytes.len() < MAX_BYTES && !(reads.stop)() {
        let limit = buffer.len().min(MAX_BYTES - bytes.len());
        let n = match file.read(&mut buffer[..limit]) {
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return reads.optional(Err(error)),
        };
        if n == 0 {
            return Some(bytes);
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
    (!(reads.stop)() && reads.optional(file.metadata())?.len() <= MAX_BYTES as u64).then_some(bytes)
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

fn read_rule_in(
    directory: &VerifiedDirectory,
    base: &Path,
    name: &str,
    reads: &IgnoreReads<'_>,
) -> Option<Gitignore> {
    let file = reads.optional(open_file_in(directory, name.as_ref()))?;
    read_rules(base, &base.join(name), file, reads)
}

fn read_rules(base: &Path, path: &Path, file: File, reads: &IgnoreReads<'_>) -> Option<Gitignore> {
    let bytes = read_bounded(file, reads)?;
    let mut builder = GitignoreBuilder::new(base);
    for (index, line) in bytes.as_slice().lines().map_while(Result::ok).enumerate() {
        if (reads.stop)() {
            return None;
        }
        let pattern = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(&line)
        } else {
            &line
        };
        let _ = builder.add_line(Some(path.to_owned()), pattern);
    }
    builder.build().ok()
}
