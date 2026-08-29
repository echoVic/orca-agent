use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspacePathError {
    InvalidRoot(String),
    InvalidCwd(String),
    OutsideWorkspace,
    RootChanged,
}

/// Canonical identity of a workspace root. The identity is established once
/// and all operation cwd values are checked against it before launch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceIdentity {
    root: PathBuf,
    #[cfg(unix)]
    root_fingerprint: (u64, u64),
}

impl WorkspaceIdentity {
    pub fn new(root: &Path) -> Result<Self, WorkspacePathError> {
        let canonical = root
            .canonicalize()
            .map_err(|error| WorkspacePathError::InvalidRoot(error.to_string()))?;
        if !canonical.is_dir() {
            return Err(WorkspacePathError::InvalidRoot(format!(
                "workspace root is not a directory: {}",
                canonical.display()
            )));
        }
        #[cfg(unix)]
        let root_fingerprint = {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&canonical)
                .map_err(|error| WorkspacePathError::InvalidRoot(error.to_string()))?;
            (metadata.dev(), metadata.ino())
        };
        Ok(Self {
            root: canonical,
            #[cfg(unix)]
            root_fingerprint,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve_cwd(&self, cwd: &Path) -> Result<PathBuf, WorkspacePathError> {
        self.validate_unchanged()?;
        let canonical = cwd
            .canonicalize()
            .map_err(|error| WorkspacePathError::InvalidCwd(error.to_string()))?;
        if !canonical.is_dir() {
            return Err(WorkspacePathError::InvalidCwd(format!(
                "cwd is not a directory: {}",
                canonical.display()
            )));
        }
        if !canonical.starts_with(&self.root) {
            return Err(WorkspacePathError::OutsideWorkspace);
        }
        self.validate_unchanged()?;
        Ok(canonical)
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.validate_unchanged().is_ok()
            && path
                .canonicalize()
                .map(|canonical| canonical.starts_with(&self.root))
                .unwrap_or(false)
    }

    fn validate_unchanged(&self) -> Result<(), WorkspacePathError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata =
                std::fs::metadata(&self.root).map_err(|_| WorkspacePathError::RootChanged)?;
            if (metadata.dev(), metadata.ino()) != self.root_fingerprint {
                return Err(WorkspacePathError::RootChanged);
            }
        }
        Ok(())
    }
}

/// Attach a stable directory identity to a child command. On Unix the child
/// changes directory through an already-open directory fd after fork, so a
/// rename/symlink replacement between validation and exec cannot redirect its
/// cwd. Windows currently performs the strict path/reparse validation and
/// leaves native handle-based cwd assignment to the platform adapter.
pub(crate) fn attach_stable_cwd(command: &mut Command, cwd: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::process::CommandExt;

        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(cwd)?;
        if !directory.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("cwd is not a directory: {}", cwd.display()),
            ));
        }

        // `current_dir` is set to a stable absolute directory so the platform
        // launcher never resolves the user path again. The fd is inherited
        // until this pre-exec hook runs, then the child enters that directory
        // by object identity.
        command.current_dir("/");
        // SAFETY: the fd is opened before fork and remains valid in the child
        // until this hook calls fchdir; no allocation or path lookup occurs.
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(directory.as_raw_fd()) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        return Ok(());
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(cwd)?;
        if directory.metadata()?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("cwd must not be a reparse point: {}", cwd.display()),
            ));
        }
        command.current_dir(cwd);
        return Ok(());
    }

    #[cfg(not(any(unix, windows)))]
    {
        command.current_dir(cwd);
        Ok(())
    }
}

impl From<WorkspacePathError> for io::Error {
    fn from(error: WorkspacePathError) -> Self {
        let kind = match error {
            WorkspacePathError::OutsideWorkspace | WorkspacePathError::RootChanged => {
                io::ErrorKind::PermissionDenied
            }
            WorkspacePathError::InvalidRoot(_) | WorkspacePathError::InvalidCwd(_) => {
                io::ErrorKind::InvalidInput
            }
        };
        io::Error::new(kind, format!("workspace path rejected: {error:?}"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::{WorkspaceIdentity, WorkspacePathError, attach_stable_cwd};

    #[test]
    fn cwd_must_remain_inside_canonical_workspace() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&outside).expect("outside");
        let identity = WorkspaceIdentity::new(&workspace).expect("identity");

        assert!(identity.resolve_cwd(&workspace).is_ok());
        assert_eq!(
            identity.resolve_cwd(&outside),
            Err(WorkspacePathError::OutsideWorkspace)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cwd_is_checked_after_canonicalization() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        let link = workspace.join("link");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let identity = WorkspaceIdentity::new(&workspace).expect("identity");

        assert_eq!(
            identity.resolve_cwd(&link),
            Err(WorkspacePathError::OutsideWorkspace)
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacing_workspace_root_after_identity_creation_is_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&outside).expect("outside");
        let identity = WorkspaceIdentity::new(&workspace).expect("identity");

        std::fs::remove_dir(&workspace).expect("remove workspace");
        std::os::unix::fs::symlink(&outside, &workspace).expect("replace root");

        assert_eq!(
            identity.resolve_cwd(&workspace),
            Err(WorkspacePathError::RootChanged)
        );
    }

    #[cfg(unix)]
    #[test]
    fn stable_cwd_uses_open_directory_identity_after_path_replacement() {
        use std::process::Stdio;

        let temp = tempfile::tempdir().expect("tempdir");
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        std::fs::create_dir_all(&original).expect("original");
        std::fs::create_dir_all(&replacement).expect("replacement");
        std::fs::write(original.join("marker"), "original").expect("marker");
        std::fs::write(replacement.join("marker"), "replacement").expect("replacement marker");

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("pwd; cat marker")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        attach_stable_cwd(&mut command, &original).expect("attach stable cwd");

        let moved = temp.path().join("original-moved");
        std::fs::rename(&original, &moved).expect("move original");
        std::os::unix::fs::symlink(&replacement, &original).expect("replace original");

        let output = command.output().expect("run child");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("original-moved"), "{stdout:?}");
        assert!(stdout.ends_with("original"), "{stdout:?}");
    }
}
