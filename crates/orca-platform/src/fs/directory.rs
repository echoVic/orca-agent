use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Component, Path};

use crate::PlatformError;

/// Identity obtained from an open filesystem handle.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity(u64, u64);

#[derive(Clone, Copy, Debug)]
pub struct FileLinks {
    pub identity: FileIdentity,
    pub count: u64,
}

pub fn file_links(file: &File) -> io::Result<FileLinks> {
    platform::file_links(file)
}

/// Permission bits used to restore the file where the host supports Unix modes.
pub fn file_mode(file: &File) -> io::Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(Some(file.metadata()?.permissions().mode()))
    }
    #[cfg(windows)]
    {
        let _ = file;
        Ok(None)
    }
}

#[derive(Clone, Copy)]
pub enum FileAccess {
    Read,
    Update,
    Delete,
}

/// A directory reached without following links in any component. Child names
/// must be single components. Unix operations are relative to its descriptor.
pub struct VerifiedDirectory(platform::Directory);

impl VerifiedDirectory {
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected an absolute directory",
            ));
        }
        platform::open_directory(path).map(Self)
    }

    pub fn child(&self, name: &OsStr) -> io::Result<Self> {
        validate_name(name)?;
        self.0.child(name).map(Self)
    }

    pub fn identity(&self) -> io::Result<FileIdentity> {
        Ok(file_links(self.0.file())?.identity)
    }

    pub fn open_file(&self, name: &OsStr, access: FileAccess) -> io::Result<File> {
        validate_name(name)?;
        self.0.open_file(name, access)
    }

    pub fn create_directory(&self, name: &OsStr) -> io::Result<()> {
        validate_name(name)?;
        self.0.create_directory(name)
    }

    pub fn create_file(&self, name: &OsStr) -> io::Result<File> {
        validate_name(name)?;
        self.0.create_file(name)
    }

    /// Removes the checked file from this directory.
    pub fn remove_file(&self, name: &OsStr, file: &File) -> io::Result<()> {
        validate_name(name)?;
        self.0.remove_file(name, file)
    }

    pub fn ignores_case(&self, path: &Path) -> bool {
        self.0.ignores_case(path)
    }
}

fn validate_name(name: &OsStr) -> io::Result<()> {
    let mut components = Path::new(name).components();
    if matches!(components.next(), Some(Component::Normal(component)) if component == name)
        && components.next().is_none()
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one path component",
        ))
    }
}

fn as_io(error: PlatformError) -> io::Error {
    let kind = match &error {
        PlatformError::Io { kind, .. } => *kind,
        _ => io::ErrorKind::InvalidInput,
    };
    io::Error::new(kind, error)
}

pub fn stale_path() -> io::Error {
    io::Error::other("path changed after patch preparation")
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    pub(super) struct Directory(File);

    pub(super) fn open_directory(path: &Path) -> io::Result<Directory> {
        let mut directory = Directory(super::super::open_nofollow(Path::new("/")).map_err(as_io)?);
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => directory = directory.child(name)?,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory path is not normalized",
                    ));
                }
            }
        }
        Ok(directory)
    }

    pub(super) fn file_links(file: &File) -> io::Result<FileLinks> {
        let metadata = file.metadata()?;
        Ok(FileLinks {
            identity: FileIdentity(metadata.dev(), metadata.ino()),
            count: metadata.nlink(),
        })
    }

    impl Directory {
        pub(super) fn file(&self) -> &File {
            &self.0
        }

        fn open(&self, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
            let name = CString::new(name.as_bytes())?;
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                    0o666 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        pub(super) fn child(&self, name: &OsStr) -> io::Result<Self> {
            self.open(name, libc::O_RDONLY | libc::O_DIRECTORY)
                .map(Self)
        }

        pub(super) fn open_file(&self, name: &OsStr, access: FileAccess) -> io::Result<File> {
            let flags = match access {
                FileAccess::Read | FileAccess::Delete => libc::O_RDONLY,
                FileAccess::Update => libc::O_RDWR,
            };
            self.open(name, flags)
        }

        pub(super) fn create_directory(&self, name: &OsStr) -> io::Result<()> {
            let name = CString::new(name.as_bytes())?;
            let result = unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o777) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(super) fn create_file(&self, name: &OsStr) -> io::Result<File> {
            self.open(name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)
        }

        pub(super) fn remove_file(&self, name: &OsStr, file: &File) -> io::Result<()> {
            let expected = file_links(file)?;
            let current = file_links(&self.open_file(name, FileAccess::Read)?)?;
            if expected.identity != current.identity || current.count > 1 {
                return Err(stale_path());
            }
            let name = CString::new(name.as_bytes())?;
            // Unix unlinkat takes a name, leaving a window for a replacement
            // after the identity check. It never follows the final component.
            let result = unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(super) fn ignores_case(&self, path: &Path) -> bool {
            #[cfg(target_os = "macos")]
            {
                let case_sensitive =
                    unsafe { libc::fpathconf(self.0.as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
                if case_sensitive >= 0 {
                    return case_sensitive == 0;
                }
            }
            // Look up alternate spellings without writing a probe.
            for directory in path.ancestors() {
                let Some(name) = directory.file_name().and_then(OsStr::to_str) else {
                    continue;
                };
                let mut alternate = name.to_lowercase();
                if alternate == name {
                    alternate = name.to_uppercase();
                }
                if alternate == name {
                    continue;
                }
                let Ok(actual) = directory.metadata() else {
                    continue;
                };
                return directory
                    .with_file_name(alternate)
                    .metadata()
                    .is_ok_and(|other| actual.dev() == other.dev() && actual.ino() == other.ino());
            }
            false
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::fs::{PathPolicy, VerifiedPath};
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::io::AsRawHandle;
    use std::path::PathBuf;
    use std::sync::Arc;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_CASE_SENSITIVE_INFO, FILE_DISPOSITION_INFO,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FileCaseSensitiveInfo,
        FileDispositionInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        SetFileInformationByHandle,
    };

    pub(super) struct Directory(Arc<DirectoryHandle>);

    struct DirectoryHandle {
        path: VerifiedPath,
        _parent: Option<Arc<DirectoryHandle>>,
    }

    pub(super) fn open_directory(path: &Path) -> io::Result<Directory> {
        let mut parts = path.components();
        let (Some(Component::Prefix(prefix)), Some(Component::RootDir)) =
            (parts.next(), parts.next())
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected an absolute Windows directory",
            ));
        };
        let mut root = PathBuf::from(prefix.as_os_str());
        root.push("\\");
        let mut directory = Directory::open(&root, None)?;
        for part in parts {
            let Component::Normal(name) = part else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory path is not normalized",
                ));
            };
            directory = directory.child(name)?;
        }
        Ok(directory)
    }

    pub(super) fn file_links(file: &File) -> io::Result<FileLinks> {
        let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let info = unsafe { info.assume_init() };
        Ok(FileLinks {
            identity: FileIdentity(
                u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            ),
            count: u64::from(info.nNumberOfLinks),
        })
    }

    impl Directory {
        fn open(path: &Path, parent: Option<Arc<DirectoryHandle>>) -> io::Result<Self> {
            let path = PathPolicy::windows_sandbox()
                .open_no_follow_pinned(path, FILE_READ_ATTRIBUTES)
                .map_err(as_io)?;
            if !path.file().metadata()?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "expected a directory",
                ));
            }
            Ok(Self(Arc::new(DirectoryHandle {
                path,
                _parent: parent,
            })))
        }

        pub(super) fn file(&self) -> &File {
            self.0.path.file()
        }

        pub(super) fn child(&self, name: &OsStr) -> io::Result<Self> {
            Self::open(&self.0.path.source().join(name), Some(self.0.clone()))
        }

        pub(super) fn open_file(&self, name: &OsStr, access: FileAccess) -> io::Result<File> {
            let access = match access {
                FileAccess::Read => GENERIC_READ,
                FileAccess::Update => GENERIC_READ | GENERIC_WRITE,
                FileAccess::Delete => GENERIC_READ | DELETE,
            };
            PathPolicy::windows_sandbox()
                .open_no_follow_pinned(&self.0.path.source().join(name), access)
                .map(VerifiedPath::into_file)
                .map_err(as_io)
        }

        // The verified parent chain stays open without delete sharing across
        // these calls. Exclusive file creation refuses any existing leaf.
        pub(super) fn create_directory(&self, name: &OsStr) -> io::Result<()> {
            std::fs::create_dir(self.new_path(name)?)
        }

        pub(super) fn create_file(&self, name: &OsStr) -> io::Result<File> {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(self.new_path(name)?)
        }

        fn new_path(&self, name: &OsStr) -> io::Result<PathBuf> {
            let path = self.0.path.source().join(name);
            let text = path.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8")
            })?;
            PathPolicy::windows_sandbox()
                .identity(text)
                .map_err(as_io)?;
            Ok(path)
        }

        pub(super) fn remove_file(&self, _name: &OsStr, file: &File) -> io::Result<()> {
            if file_links(file)?.count > 1 {
                return Err(stale_path());
            }
            let info = FILE_DISPOSITION_INFO { DeleteFile: true };
            let result = unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfo,
                    (&raw const info).cast(),
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            };
            if result == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(super) fn ignores_case(&self, _path: &Path) -> bool {
            let mut info = MaybeUninit::<FILE_CASE_SENSITIVE_INFO>::uninit();
            let result = unsafe {
                GetFileInformationByHandleEx(
                    self.file().as_raw_handle(),
                    FileCaseSensitiveInfo,
                    info.as_mut_ptr().cast(),
                    size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
                )
            };
            result == 0 || unsafe { info.assume_init() }.Flags & 1 == 0
        }
    }
}
