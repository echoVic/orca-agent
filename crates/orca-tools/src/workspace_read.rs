use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use orca_platform::fs::{FileAccess, FileIdentity, VerifiedDirectory};

#[derive(Clone)]
struct RootIdentity {
    canonical: PathBuf,
    identity: FileIdentity,
}

static READ_ROOTS: OnceLock<Mutex<HashMap<PathBuf, RootIdentity>>> = OnceLock::new();

fn read_roots() -> &'static Mutex<HashMap<PathBuf, RootIdentity>> {
    READ_ROOTS.get_or_init(Mutex::default)
}

fn changed_root(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "workspace root changed while Orca was running: {}",
            path.display()
        ),
    )
}

/// Remember the directory selected on first use, including its original spelling.
pub fn pin_read_root(path: &Path) -> io::Result<PathBuf> {
    let key = std::path::absolute(path)?;
    let mut roots = read_roots().lock().unwrap();
    let pin = roots.get(&key);
    let canonical = key.canonicalize().map_err(|error| {
        if pin.is_some() {
            changed_root(path)
        } else {
            error
        }
    })?;
    if let Some(pin) = pin
        && canonical != pin.canonical
    {
        return Err(changed_root(path));
    }
    let known = pin.or_else(|| roots.get(&canonical));
    let identity = VerifiedDirectory::open(&canonical)
        .and_then(|directory| directory.identity())
        .map_err(|error| {
            if known.is_some() {
                changed_root(path)
            } else {
                error
            }
        })?;
    if known.is_some_and(|root| root.identity != identity) {
        return Err(changed_root(path));
    }
    let root = RootIdentity {
        canonical: canonical.clone(),
        identity,
    };
    roots.insert(canonical.clone(), root.clone());
    roots.insert(key, root);
    Ok(canonical)
}

pub(crate) fn open_root(root: &Path) -> io::Result<VerifiedDirectory> {
    let expected = read_roots()
        .lock()
        .unwrap()
        .get(root)
        .map(|pin| pin.identity);
    let directory = VerifiedDirectory::open(root).map_err(|error| {
        if expected.is_some() {
            changed_root(root)
        } else {
            error
        }
    })?;
    if let Some(expected) = expected
        && directory.identity().ok() != Some(expected)
    {
        return Err(changed_root(root));
    }
    Ok(directory)
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
