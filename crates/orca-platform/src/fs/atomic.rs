use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::PlatformError;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicWritePolicy {
    NoFollow,
    ReplaceDestination,
}

pub fn atomic_write(
    destination: &Path,
    contents: &[u8],
    policy: AtomicWritePolicy,
) -> Result<(), PlatformError> {
    atomic_write_with(destination, policy, |temporary| {
        temporary.write_all(contents)
    })
}

pub fn atomic_write_with<F>(
    destination: &Path,
    policy: AtomicWritePolicy,
    write_contents: F,
) -> Result<(), PlatformError>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    #[cfg(windows)]
    let _write_guard = platform::lock_atomic_write()?;
    #[cfg(unix)]
    platform::lock_atomic_write()?;
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = destination
        .file_name()
        .ok_or_else(|| invalid_destination(destination, "destination has no file name"))?;

    let existing = inspect_destination(destination, policy)?;
    let (mut temporary, temporary_path) = create_temporary(parent, file_name)?;
    let mut cleanup = TempCleanup::new(temporary_path.clone());

    if let Some(permissions) = existing.permissions {
        temporary
            .set_permissions(permissions)
            .map_err(|error| PlatformError::io("preserve destination permissions", error))?;
    }
    write_contents(&mut temporary)
        .map_err(|error| PlatformError::io("write atomic temporary file", error))?;
    flush_file(&temporary)?;
    drop(temporary);

    platform::replace(&temporary_path, destination, existing.existed)?;
    cleanup.disarm();
    platform::sync_parent(parent)?;
    Ok(())
}

struct ExistingDestination {
    existed: bool,
    permissions: Option<std::fs::Permissions>,
}

fn inspect_destination(
    destination: &Path,
    policy: AtomicWritePolicy,
) -> Result<ExistingDestination, PlatformError> {
    match destination.symlink_metadata() {
        Ok(metadata) => {
            if is_link_or_reparse(&metadata) {
                if matches!(policy, AtomicWritePolicy::ReplaceDestination)
                    && supports_link_replacement()
                {
                    return Ok(ExistingDestination {
                        existed: true,
                        permissions: None,
                    });
                }
                return Err(PlatformError::ReparsePointRejected {
                    path: destination.to_path_buf(),
                });
            }
            if !metadata.is_file() {
                return Err(invalid_destination(
                    destination,
                    "destination exists and is not a regular file",
                ));
            }
            Ok(ExistingDestination {
                existed: true,
                permissions: Some(metadata.permissions()),
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ExistingDestination {
            existed: false,
            permissions: None,
        }),
        Err(error) => Err(PlatformError::io("inspect atomic destination", error)),
    }
}

#[cfg(unix)]
fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn supports_link_replacement() -> bool {
    true
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn supports_link_replacement() -> bool {
    false
}

fn create_temporary(
    parent: &Path,
    file_name: &std::ffi::OsStr,
) -> Result<(File, PathBuf), PlatformError> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut name = std::ffi::OsString::from(".");
        name.push(file_name);
        name.push(format!(".orca-{}-{sequence}.tmp", std::process::id()));
        let path = parent.join(name);
        match platform::create_new(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PlatformError::io(
                    "create atomic temporary file in destination directory",
                    error,
                ));
            }
        }
    }
    Err(PlatformError::Io {
        kind: io::ErrorKind::AlreadyExists,
        message: "could not allocate a unique atomic temporary file name".to_string(),
    })
}

fn flush_file(file: &File) -> Result<(), PlatformError> {
    platform::flush(file).map_err(|error| PlatformError::io("flush atomic temporary file", error))
}

fn invalid_destination(path: &Path, reason: &str) -> PlatformError {
    PlatformError::InvalidPathIdentity {
        path: path.to_string_lossy().into_owned(),
        reason: reason.to_string(),
    }
}

struct TempCleanup {
    path: PathBuf,
    armed: bool,
}

impl TempCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::os::unix::fs::OpenOptionsExt;

    use super::*;

    pub(super) fn lock_atomic_write() -> Result<(), PlatformError> {
        Ok(())
    }

    pub(super) fn create_new(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
    }

    pub(super) fn flush(file: &File) -> io::Result<()> {
        file.sync_all()
    }

    pub(super) fn replace(
        temporary: &Path,
        destination: &Path,
        _destination_existed: bool,
    ) -> Result<(), PlatformError> {
        std::fs::rename(temporary, destination)
            .map_err(|error| PlatformError::io("atomically replace destination", error))
    }

    pub(super) fn sync_parent(parent: &Path) -> Result<(), PlatformError> {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| PlatformError::io("sync atomic destination directory", error))
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION,
        ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_REMOVE_REPLACED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers,
        MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACEFILE_WRITE_THROUGH, ReplaceFileW,
    };

    use super::*;

    // ReplaceFileW can reject overlapping replacement attempts even when every
    // file handle allows delete sharing. Keep one in-process replacement
    // transaction active at a time; external holders are handled by the
    // bounded retry in `replace`.
    static ATOMIC_WRITE_LOCK: Mutex<()> = Mutex::new(());

    pub(super) fn lock_atomic_write() -> Result<MutexGuard<'static, ()>, PlatformError> {
        ATOMIC_WRITE_LOCK.lock().map_err(|_| PlatformError::Io {
            kind: io::ErrorKind::Other,
            message: "atomic write lock poisoned".to_string(),
        })
    }

    pub(super) fn create_new(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
    }

    pub(super) fn flush(file: &File) -> io::Result<()> {
        let result = unsafe { FlushFileBuffers(file.as_raw_handle()) };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn replace(
        temporary: &Path,
        destination: &Path,
        destination_existed: bool,
    ) -> Result<(), PlatformError> {
        let temporary = wide_path(temporary);
        let destination = wide_path(destination);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = if destination_existed {
                unsafe {
                    ReplaceFileW(
                        destination.as_ptr(),
                        temporary.as_ptr(),
                        std::ptr::null(),
                        REPLACEFILE_WRITE_THROUGH,
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                }
            } else {
                unsafe {
                    MoveFileExW(
                        temporary.as_ptr(),
                        destination.as_ptr(),
                        MOVEFILE_WRITE_THROUGH,
                    )
                }
            };
            if result != 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if !error
                .raw_os_error()
                .is_some_and(|code| retryable_replace_error(code, destination_existed))
                || Instant::now() >= deadline
            {
                return Err(PlatformError::io("atomically replace destination", error));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn sync_parent(_parent: &Path) -> Result<(), PlatformError> {
        Ok(())
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn retryable_replace_error(code: i32, destination_existed: bool) -> bool {
        code == ERROR_SHARING_VIOLATION as i32
            || code == ERROR_LOCK_VIOLATION as i32
            || code == ERROR_UNABLE_TO_REMOVE_REPLACED as i32
            || code == ERROR_UNABLE_TO_MOVE_REPLACEMENT as i32
            || (destination_existed && code == ERROR_FILE_NOT_FOUND as i32)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn retryable_replace_errors_cover_safe_windows_collision_states() {
            for code in [
                ERROR_SHARING_VIOLATION,
                ERROR_LOCK_VIOLATION,
                ERROR_UNABLE_TO_REMOVE_REPLACED,
                ERROR_UNABLE_TO_MOVE_REPLACEMENT,
            ] {
                assert!(retryable_replace_error(code as i32, false));
            }
            assert!(retryable_replace_error(ERROR_FILE_NOT_FOUND as i32, true));
            assert!(!retryable_replace_error(ERROR_FILE_NOT_FOUND as i32, false));
            assert!(!retryable_replace_error(1177, true));
        }
    }
}
