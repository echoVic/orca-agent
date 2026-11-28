use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use orca_platform::fs::{FileAccess, VerifiedDirectory};

/// Open the canonical path selected when the runtime accepted this workspace.
pub fn open_root(root: &Path) -> io::Result<VerifiedDirectory> {
    VerifiedDirectory::open(root).map_err(|error| {
        if matches!(error.kind(), io::ErrorKind::NotADirectory)
            || cfg!(unix) && error.raw_os_error() == Some(libc::ELOOP)
            || cfg!(windows) && matches!(error.raw_os_error(), Some(681 | 4395))
        {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "workspace path changed while Orca was running: {}",
                    root.display()
                ),
            )
        } else {
            error
        }
    })
}

pub(crate) fn canonical_read_path(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("path escapes workspace via symlink: {}", path.display()),
        ));
    }
    Ok(canonical)
}

pub(crate) fn open_read_file(path: &crate::ResolvedWorkspacePath) -> io::Result<(File, u64)> {
    let file = open_workspace_file(&path.root, &path.canonical)?;
    let size = file.metadata()?.len();
    Ok((file, size))
}

/// Open a resolved file from its canonical root, without following any links
/// that may have replaced components since resolution.
pub fn open_workspace_file(root: &Path, path: &Path) -> io::Result<File> {
    let (parent, name) = path
        .parent()
        .zip(path.file_name())
        .filter(|(parent, _)| parent.starts_with(root))
        .ok_or_else(not_regular)?;
    let changed = |error: io::Error| changed_path(path, error.kind());
    let directory = open_below(open_root(root)?, root, parent).map_err(changed)?;
    let file_type = path.symlink_metadata().map_err(changed)?.file_type();
    if file_type.is_symlink() {
        return Err(changed(io::ErrorKind::PermissionDenied.into()));
    }
    if !file_type.is_file() {
        return Err(not_regular());
    }
    open_file_in(&directory, name).map_err(changed)
}

pub(crate) fn open_file_in(directory: &VerifiedDirectory, name: &OsStr) -> io::Result<File> {
    let file = directory.open_file(name, FileAccess::Read)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

fn changed_path(path: &Path, kind: io::ErrorKind) -> io::Error {
    io::Error::new(
        kind,
        format!("path changed while it was opened: {}", path.display()),
    )
}

pub(crate) fn open_directory(root: &Path, path: &Path) -> io::Result<VerifiedDirectory> {
    open_below(open_root(root)?, root, path)
}

fn open_below(
    mut directory: VerifiedDirectory,
    root: &Path,
    path: &Path,
) -> io::Result<VerifiedDirectory> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| io::ErrorKind::PermissionDenied)?;
    for part in relative {
        directory = directory.child(part)?;
    }
    Ok(directory)
}

/// Check the real location of a listing entry and report whether it is a directory.
pub(crate) fn listing_entry(root: &Path, path: &Path) -> Option<bool> {
    let canonical = canonical_read_path(root, path).ok()?;
    let is_directory = if canonical == root {
        open_root(root).ok()?;
        true
    } else {
        let parent = open_directory(root, canonical.parent()?).ok()?;
        parent.entry_is_directory(canonical.file_name()?).ok()?
    };
    (canonical_read_path(root, path).ok()? == canonical).then_some(is_directory)
}

#[cfg(all(test, unix))]
pub(crate) mod listing_test {
    use std::cell::RefCell;

    thread_local! {
        pub(crate) static BEFORE_EMISSION: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    pub(crate) fn before_emission() {
        if let Some(change) = BEFORE_EMISSION.with(|hook| hook.take()) {
            change();
        }
    }
}
