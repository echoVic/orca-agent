//! Paths for the Win32 calls Rust's own file APIs do not make.

use std::path::{Path, PathBuf};

/// The path to hand a Win32 call that takes no plain path past `MAX_PATH`.
///
/// Rust's file APIs pass a long path in its extended-length form (`\\?\`) on
/// their own; `MoveFileExW` and `ReplaceFileW`, called directly here and by
/// tempfile, do not, and fail with "the system cannot find the path
/// specified". From 248 characters on, the length at which Rust switches too,
/// this is the extended-length spelling of the path's absolute form. A
/// shorter path, one already extended, one with no absolute form, and every
/// path on other platforms come back as they are.
pub fn extended_length_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        windows::extended_length_path(path)
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    /// Rust's own threshold (`std::sys::path::windows::get_long_path`): the
    /// `MAX_PATH` of `CreateDirectoryW`, below the 260 of the other calls.
    const LEGACY_MAX_PATH: usize = 248;

    pub(super) fn extended_length_path(path: &Path) -> PathBuf {
        if path.as_os_str().encode_wide().count() < LEGACY_MAX_PATH {
            return path.to_path_buf();
        }
        let Ok(absolute) = std::path::absolute(path) else {
            return path.to_path_buf();
        };
        let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        let starts_with = |prefix: &str| {
            let prefix: Vec<u16> = prefix.encode_utf16().collect();
            wide.starts_with(&prefix)
        };
        let extended: Vec<u16> = if starts_with(r"\\?\") || starts_with(r"\\.\") {
            wide
        } else if starts_with(r"\\") {
            r"\\?\UNC\"
                .encode_utf16()
                .chain(wide[2..].iter().copied())
                .collect()
        } else if wide.len() >= 3 && wide[1] == u16::from(b':') && wide[2] == u16::from(b'\\') {
            r"\\?\".encode_utf16().chain(wide.iter().copied()).collect()
        } else {
            return path.to_path_buf();
        };
        PathBuf::from(OsString::from_wide(&extended))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An atomic write creates and then replaces a file whose path, and whose
    /// temporary sibling's, run past `MAX_PATH`: on Windows both the move and
    /// the replacement go through the extended-length form.
    #[test]
    fn an_atomic_write_creates_and_replaces_a_file_past_max_path() {
        let root = tempfile::tempdir().expect("a scratch directory");
        let directory = root.path().join("d".repeat(100)).join("e".repeat(100));
        std::fs::create_dir_all(&directory).expect("a deep directory");
        let destination = directory.join(format!("{}.json", "f".repeat(64)));
        assert!(destination.as_os_str().len() > 260);

        for content in [&b"first"[..], &b"second"[..]] {
            crate::fs::atomic_write(
                &destination,
                content,
                crate::fs::AtomicWritePolicy::NoFollow,
            )
            .expect("an atomic write past MAX_PATH");
            assert_eq!(std::fs::read(&destination).expect("the file"), content);
        }
    }

    #[test]
    fn a_short_path_comes_back_as_it_is() {
        let path = Path::new("short").join("file.json");
        assert_eq!(extended_length_path(&path), path);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_long_path_comes_back_as_it_is_outside_windows() {
        let path = Path::new("/tmp").join("d".repeat(300));
        assert_eq!(extended_length_path(&path), path);
    }

    #[cfg(windows)]
    #[test]
    fn a_long_drive_path_gets_the_extended_length_prefix() {
        let path = PathBuf::from(format!(r"C:\{}\file.json", "d".repeat(250)));
        assert_eq!(
            extended_length_path(&path),
            PathBuf::from(format!(r"\\?\C:\{}\file.json", "d".repeat(250)))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_long_unc_path_gets_the_unc_extended_length_prefix() {
        let path = PathBuf::from(format!(r"\\server\share\{}", "d".repeat(250)));
        assert_eq!(
            extended_length_path(&path),
            PathBuf::from(format!(r"\\?\UNC\server\share\{}", "d".repeat(250)))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_long_path_already_extended_comes_back_as_it_is() {
        let path = PathBuf::from(format!(r"\\?\C:\{}", "d".repeat(250)));
        assert_eq!(extended_length_path(&path), path);
    }
}
