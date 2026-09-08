use std::fs::File;
use std::path::Path;

use crate::PlatformError;

pub fn open_nofollow(path: &Path) -> Result<File, PlatformError> {
    platform::open(path)
}

pub fn open_nofollow_nonblocking(path: &Path) -> Result<File, PlatformError> {
    platform::open_nonblocking(path)
}

/// Read without sharing or changing the file handle's sequential cursor.
pub fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    platform::read_at(file, buffer, offset)
}

#[cfg(unix)]
mod platform {
    use std::os::unix::fs::{FileExt, OpenOptionsExt};

    use super::*;

    pub(super) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
        file.read_at(buffer, offset)
    }

    pub(super) fn open(path: &Path) -> Result<File, PlatformError> {
        open_with_flags(path, libc::O_CLOEXEC | libc::O_NOFOLLOW)
    }

    pub(super) fn open_nonblocking(path: &Path) -> Result<File, PlatformError> {
        open_with_flags(path, libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
    }

    fn open_with_flags(path: &Path, flags: i32) -> Result<File, PlatformError> {
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(PlatformError::ReparsePointRejected {
                path: path.to_path_buf(),
            });
        }
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(path)
            .map_err(|error| PlatformError::io("open file without following links", error))
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::fs::{FileExt, MetadataExt, OpenOptionsExt};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    use super::*;

    pub(super) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
        file.seek_read(buffer, offset)
    }

    pub(super) fn open(path: &Path) -> Result<File, PlatformError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| {
                PlatformError::io("open file without following reparse points", error)
            })?;
        let metadata = file
            .metadata()
            .map_err(|error| PlatformError::io("inspect no-follow file metadata", error))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(PlatformError::ReparsePointRejected {
                path: path.to_path_buf(),
            });
        }
        Ok(file)
    }

    pub(super) fn open_nonblocking(path: &Path) -> Result<File, PlatformError> {
        open(path)
    }
}
