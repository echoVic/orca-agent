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

/// Writes a file only its owner can read or write, such as one holding a
/// credential: whatever mode an earlier version had, the file replacing it
/// is owner-only, and so is it before any byte is written.
pub fn atomic_write_private(
    destination: &Path,
    contents: &[u8],
    policy: AtomicWritePolicy,
) -> Result<(), PlatformError> {
    atomic_write_impl(destination, policy, true, |temporary| {
        #[cfg(unix)]
        restrict_to_owner(temporary)?;
        temporary.write_all(contents)
    })
}

#[cfg(unix)]
pub(super) fn restrict_to_owner(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
pub(super) fn restrict_to_owner(file: &File) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile, WRITE_DAC,
    };

    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let acl_file = unsafe { File::from_raw_handle(handle) };
    let descriptor = PrivateDescriptor::new()?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl = null_mut();
    if unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted) }
        == 0
    {
        Err(io::Error::last_os_error())
    } else if present == 0 || acl.is_null() {
        Err(io::Error::other("owner-only access list is missing"))
    } else {
        let status = unsafe {
            SetSecurityInfo(
                acl_file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl,
                null(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }
}

#[cfg(windows)]
struct PrivateDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl PrivateDescriptor {
    fn new() -> io::Result<Self> {
        use std::ptr::null_mut;
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

        let mut descriptor = null_mut();
        // OW denotes the file's owner. Protect the ACL from inherited entries.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                windows_sys::core::w!("D:P(A;;FA;;;OW)"),
                1,
                &mut descriptor,
                null_mut(),
            )
        };
        if converted == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(descriptor))
        }
    }
}

#[cfg(windows)]
impl Drop for PrivateDescriptor {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
    }
}

#[cfg(windows)]
pub(super) use platform::create_new_private;

pub fn atomic_write_with<F>(
    destination: &Path,
    policy: AtomicWritePolicy,
    write_contents: F,
) -> Result<(), PlatformError>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    atomic_write_impl(destination, policy, false, write_contents)
}

fn atomic_write_impl<F>(
    destination: &Path,
    policy: AtomicWritePolicy,
    private: bool,
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
    let (mut temporary, temporary_path) = create_temporary(parent, file_name, private)?;
    let mut cleanup = TempCleanup::new(temporary_path.clone());

    if !private && let Some(permissions) = existing.permissions {
        temporary
            .set_permissions(permissions)
            .map_err(|error| PlatformError::io("preserve destination permissions", error))?;
    }
    write_contents(&mut temporary)
        .map_err(|error| PlatformError::io("write atomic temporary file", error))?;
    flush_file(&temporary)?;
    drop(temporary);

    platform::replace(&temporary_path, destination, existing.existed, private)?;
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
    private: bool,
) -> Result<(File, PathBuf), PlatformError> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut name = std::ffi::OsString::from(".");
        name.push(file_name);
        name.push(format!(".orca-{}-{sequence}.tmp", std::process::id()));
        let path = parent.join(name);
        match platform::create_new(&path, private) {
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

    pub(super) fn create_new(path: &Path, private: bool) -> io::Result<File> {
        let mut options = OpenOptions::new();
        if private {
            options.mode(0o600);
        }
        options
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
        _private: bool,
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

    pub(super) fn create_new(path: &Path, private: bool) -> io::Result<File> {
        if private {
            use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
            return create_new_private(
                path,
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            );
        }
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
    }

    pub(in crate::fs) fn create_new_private(
        path: &Path,
        access: u32,
        share: u32,
    ) -> io::Result<File> {
        use std::os::windows::io::FromRawHandle;
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
        };

        let path = wide_path(path);
        if path[..path.len() - 1].contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file path contains a null character",
            ));
        }
        let descriptor = PrivateDescriptor::new()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                share,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_handle(handle) })
        }
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
        private: bool,
    ) -> Result<(), PlatformError> {
        let temporary = wide_path(temporary);
        let destination = wide_path(destination);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = if destination_existed && !private {
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
                        MOVEFILE_WRITE_THROUGH
                            | if private && destination_existed {
                                windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                            } else {
                                0
                            },
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

    /// `MoveFileExW` and `ReplaceFileW` take a long path only in its
    /// extended-length form.
    fn wide_path(path: &Path) -> Vec<u16> {
        crate::fs::extended_length_path(path)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect()
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
        fn private_atomic_replacement_keeps_a_protected_access_list() {
            use windows_sys::Win32::Foundation::LocalFree;
            use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
            use windows_sys::Win32::Security::{
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, SE_DACL_PROTECTED,
            };

            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("record");
            std::fs::write(&path, b"old").unwrap();
            atomic_write_private(&path, b"new", AtomicWritePolicy::NoFollow).unwrap();
            let file = File::open(path).unwrap();
            let mut descriptor = std::ptr::null_mut();
            let status = unsafe {
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert_eq!(status, 0);
            let mut control = 0;
            let mut revision = 0;
            let valid = unsafe {
                let valid = GetSecurityDescriptorControl(descriptor, &mut control, &mut revision);
                LocalFree(descriptor);
                valid
            };
            assert_ne!(valid, 0);
            assert_ne!(control & SE_DACL_PROTECTED, 0);
        }

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
