//! Workspace path containment, bounded regular-file reads and sha256 helpers.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::RewindError;

/// The outcome of a symlink-safe, size-bounded read of a workspace file.
pub(crate) enum BoundedFile {
    /// The path does not exist (or vanished while opening it).
    Missing,
    /// A regular file with its raw bytes and unix mode.
    Regular { bytes: Vec<u8>, mode: Option<u32> },
    /// A regular file above the requested limit; only its size is reported.
    TooLarge { observed_bytes: u64 },
    /// An existing path that is not a regular file (symlink, directory, ...).
    NotRegular,
}

/// Read a workspace file without following symlinks and without reading more
/// than `limit + 1` bytes.
pub(crate) fn read_regular_file(path: &Path, limit: u64) -> io::Result<BoundedFile> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BoundedFile::Missing),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Ok(BoundedFile::NotRegular);
    }
    if metadata.len() > limit {
        return Ok(BoundedFile::TooLarge {
            observed_bytes: metadata.len(),
        });
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BoundedFile::Missing),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Ok(BoundedFile::TooLarge {
            observed_bytes: bytes.len() as u64,
        });
    }
    Ok(BoundedFile::Regular {
        bytes,
        mode: mode_of(&metadata),
    })
}

/// Directories the workspace observer never captures, and that a restore must
/// therefore never write into: the transcript is untrusted audit data (§6.3,
/// §12.3).
pub(crate) const EXCLUDED_DIRECTORIES: [&str; 4] = [".git", ".orca", "target", "node_modules"];

/// If `rel` lies inside an excluded directory, return that directory name.
///
/// Only *parent* components are inspected: the final component is the file
/// name, so a regular file literally named `target` remains restorable.
pub(crate) fn excluded_directory(rel: &str) -> Option<&'static str> {
    let mut components = Path::new(rel)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        });
    let mut current = components.next()?;
    for next in components {
        if let Some(name) = current.to_str()
            && let Some(excluded) = EXCLUDED_DIRECTORIES.iter().find(|dir| **dir == name)
        {
            return Some(excluded);
        }
        current = next;
    }
    None
}

/// Hex-encode a sha256 digest of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(Sha256::digest(bytes).as_slice())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// True when `digest` is exactly 64 lowercase hex characters.
pub(crate) fn is_valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Validate a digest, mapping failures to [`RewindError::InvalidDigest`].
pub(crate) fn validate_digest(digest: &str) -> io::Result<()> {
    if is_valid_digest(digest) {
        Ok(())
    } else {
        Err(RewindError::InvalidDigest {
            digest: digest.to_string(),
        }
        .into())
    }
}

/// Current sha256 of a regular file.
///
/// Returns `Ok(None)` when the file does not exist and
/// [`RewindError::NotRegular`] for symlinks, directories and special files.
pub fn file_sha256(path: &Path) -> io::Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Err(RewindError::NotRegular {
            path: path.to_path_buf(),
        }
        .into());
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut reader = io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Some(hex_lower(hasher.finalize().as_slice())))
}

/// Resolve a workspace-relative path to a normalized absolute path inside
/// `root`, refusing to escape it.
///
/// Rejected inputs:
/// * absolute paths, Windows prefixes and empty paths;
/// * any `..` component (`.` components are normalized away);
/// * a root that is a symlink or not a directory;
/// * any existing path component that is a symlink, or a non-final component
///   that is not a directory, or a final component that is not a regular file.
///
/// A path whose intermediate components do not exist is returned lexically
/// (it cannot contain a symlink below a missing directory); the caller (the
/// applier) will then fail with a normal `NotFound` IO error.
pub fn resolve_inside(root: &Path, rel: &str) -> io::Result<PathBuf> {
    if rel.is_empty() {
        return Err(escape(rel, "empty path"));
    }
    if rel.contains('\0') {
        return Err(escape(rel, "NUL byte"));
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() || has_windows_prefix(rel) {
        // Attack samples include Windows drive/UNC spellings even when the
        // process runs on a unix host, so those forms are rejected everywhere.
        return Err(escape(rel, "absolute path"));
    }

    let mut components = Vec::new();
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => components.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => return Err(escape(rel, "parent-directory component")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(escape(rel, "absolute path"));
            }
        }
    }
    if components.is_empty() {
        return Err(escape(rel, "empty path"));
    }
    // A relative path that still carries a root separator on another platform
    // (for example a lone backslash on unix) is harmless, but a Windows drive
    // or UNC prefix must never be accepted even when running on unix.
    if cfg!(windows) && rel_path.has_root() {
        return Err(escape(rel, "absolute path"));
    }

    let root_metadata = fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.file_type().is_dir() {
        return Err(RewindError::RootNotDirectory {
            root: root.to_path_buf(),
        }
        .into());
    }
    let mut current = fs::canonicalize(root)?;

    let last = components.len() - 1;
    for (index, component) in components.iter().enumerate() {
        let candidate = current.join(component);
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    return Err(escape(rel, "symlink component"));
                }
                if index == last {
                    if !file_type.is_file() {
                        return Err(escape(rel, "final component is not a regular file"));
                    }
                } else if !file_type.is_dir() {
                    return Err(escape(rel, "non-directory component"));
                }
                current = candidate;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Nothing below a missing component can be a symlink; keep the
                // remaining components lexically so the caller gets a proper
                // NotFound (or can create the parent) instead of a false escape.
                for rest in &components[index..] {
                    current.push(rest);
                }
                return Ok(current);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(current)
}

/// Atomically replace `path` with `bytes`: temp file in the same directory,
/// fsync, rename, then fsync the directory. `mode` is applied on unix before
/// publication so the restored file keeps its captured permissions.
pub(crate) fn atomic_publish(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = mode {
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(mode))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_dir(parent)?;
    Ok(())
}

pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Detect Windows absolute spellings on every platform: a leading drive letter
/// (`C:`, `c:/...`, `C:\\...`) or a UNC/rooted backslash prefix.
fn has_windows_prefix(rel: &str) -> bool {
    if rel.starts_with('\\') {
        return true;
    }
    let bytes = rel.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    // Drive-letter prefixes appearing after another separator (for example a
    // path smuggled through a transcript as `src/C:/evil`).
    rel.split(['/', '\\']).any(|part| {
        let part = part.as_bytes();
        part.len() >= 2 && part[0].is_ascii_alphabetic() && part[1] == b':'
    })
}

pub(crate) fn escape(path: &str, reason: &str) -> io::Error {
    RewindError::PathEscape {
        path: path.to_string(),
        reason: reason.to_string(),
    }
    .into()
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode_of(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::rewind_error;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(sha256_hex(b"").len(), 64);
        assert!(is_valid_digest(&sha256_hex(b"x")));
        assert!(!is_valid_digest("ABCDEF"));
        assert!(!is_valid_digest(&"A".repeat(64)));
    }

    #[test]
    fn resolve_inside_accepts_relative_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        fs::create_dir_all(root.join("src/nested")).expect("dirs");
        fs::write(root.join("src/nested/a.rs"), b"x").expect("write");

        let resolved = resolve_inside(root, "src/nested/a.rs").expect("resolved");
        assert!(resolved.is_absolute());
        assert!(resolved.starts_with(fs::canonicalize(root).expect("canonical root")));
        assert_eq!(
            resolved,
            fs::canonicalize(root).unwrap().join("src/nested/a.rs")
        );

        let normalized = resolve_inside(root, "./src/./nested/a.rs").expect("normalized");
        assert_eq!(normalized, resolved);
    }

    #[test]
    fn resolve_inside_rejects_parent_dir_and_absolute_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        for bad in [
            "../evil",
            "a/../../evil",
            "",
            "/etc/passwd",
            "/",
            "C:\\Windows\\system32",
            "c:/windows/system32",
            "\\\\server\\share\\evil.txt",
        ] {
            let error = resolve_inside(root, bad).expect_err("must reject");
            assert!(
                rewind_error(&error).is_some_and(RewindError::is_path_escape),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn resolve_inside_rejects_non_directory_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = temp.path().join("root.txt");
        fs::write(&file, b"x").expect("write");
        let error = resolve_inside(&file, "a.txt").expect_err("must reject");
        assert!(
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<RewindError>())
                .is_some_and(|inner| matches!(inner, RewindError::RootNotDirectory { .. }))
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_inside_rejects_symlinked_directory_and_final_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret.txt"), b"secret").expect("write");
        symlink(outside.path(), temp.path().join("link_dir")).expect("symlink dir");
        fs::write(temp.path().join("real.txt"), b"real").expect("write");
        symlink(temp.path().join("real.txt"), temp.path().join("link_file")).expect("symlink file");

        for bad in ["link_dir/secret.txt", "link_file"] {
            let error = resolve_inside(temp.path(), bad).expect_err("must reject symlink");
            assert!(
                rewind_error(&error).is_some_and(RewindError::is_path_escape),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn file_sha256_hashes_regular_files_and_reports_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = temp.path().join("a.txt");
        fs::write(&file, b"hello").expect("write");
        assert_eq!(
            file_sha256(&file).expect("hash").as_deref(),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        assert!(
            file_sha256(&temp.path().join("nope.txt"))
                .expect("missing")
                .is_none()
        );

        let error = file_sha256(temp.path()).expect_err("directories are not hashable");
        assert!(
            error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<RewindError>())
                .is_some_and(|inner| matches!(inner, RewindError::NotRegular { .. }))
        );
    }

    #[test]
    fn excluded_directory_checks_parent_components_only() {
        assert_eq!(excluded_directory(".git/config"), Some(".git"));
        assert_eq!(excluded_directory("src/target/debug/x"), Some("target"));
        assert_eq!(
            excluded_directory("node_modules/pkg/index.js"),
            Some("node_modules")
        );
        assert_eq!(excluded_directory(".orca/rewind/x"), Some(".orca"));
        assert_eq!(excluded_directory("src/main.rs"), None);
        assert_eq!(excluded_directory("target"), None);
        assert_eq!(excluded_directory(".gitignore"), None);
        assert_eq!(excluded_directory("src/targeted/x"), None);
    }

    #[test]
    fn read_regular_file_is_bounded_and_rejects_non_regular() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file = temp.path().join("a.txt");
        fs::write(&file, b"hello").expect("write");
        match read_regular_file(&file, 16).expect("read") {
            BoundedFile::Regular { bytes, mode } => {
                assert_eq!(bytes, b"hello");
                #[cfg(unix)]
                assert!(mode.is_some());
                #[cfg(not(unix))]
                assert!(mode.is_none());
            }
            _ => panic!("expected regular file"),
        }
        assert!(matches!(
            read_regular_file(&file, 2).expect("read"),
            BoundedFile::TooLarge { observed_bytes: 5 }
        ));
        assert!(matches!(
            read_regular_file(temp.path(), 16).expect("read"),
            BoundedFile::NotRegular
        ));
        assert!(matches!(
            read_regular_file(&temp.path().join("missing"), 16).expect("read"),
            BoundedFile::Missing
        ));
    }
}
