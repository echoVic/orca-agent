use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use orca_platform::fs::{FileAccess, VerifiedDirectory};

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

pub(crate) fn open_read_file(cwd: &Path, path: &Path) -> io::Result<(File, u64, PathBuf)> {
    let root = cwd.canonicalize()?;
    let canonical = canonical_read_path(&root, path)?;
    let file = open_workspace_file(&root, &canonical)?;
    let size = file.metadata()?.len();
    Ok((file, size, canonical))
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
    let file_type = path.symlink_metadata().map_err(changed)?.file_type();
    if file_type.is_symlink() {
        return Err(changed(io::ErrorKind::PermissionDenied.into()));
    }
    if !file_type.is_file() {
        return Err(not_regular());
    }
    let directory = open_directory(root, parent).map_err(changed)?;
    let file = directory
        .open_file(name, FileAccess::Read)
        .map_err(changed)?;
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

fn open_directory(root: &Path, path: &Path) -> io::Result<VerifiedDirectory> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| io::ErrorKind::PermissionDenied)?;
    let mut directory = VerifiedDirectory::open(root)?;
    for part in relative {
        directory = directory.child(part)?;
    }
    Ok(directory)
}

/// Check the real location of a listing entry and report whether it is a directory.
pub(crate) fn listing_entry(root: &Path, path: &Path) -> Option<bool> {
    let canonical = canonical_read_path(root, path).ok()?;
    let parent = if canonical == root {
        root
    } else {
        canonical.parent()?
    };
    let _parent = open_directory(root, parent).ok()?;
    let kind = canonical.symlink_metadata().ok()?.file_type();
    if kind.is_symlink() || canonical_read_path(root, path).ok()? != canonical {
        return None;
    }
    Some(kind.is_dir())
}
