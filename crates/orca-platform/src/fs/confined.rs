use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Open a regular file without following any component of its absolute path.
/// On Unix the leaf is opened nonblocking before its type is checked again.
pub fn open_confined_regular(path: &Path) -> io::Result<File> {
    let (parent, name) = split(path)?;
    platform::Directory::open(parent, false)?.open_regular(name)
}

/// Publish a private file in a confined directory. A retry may reuse an
/// identical file, but will never overwrite a different result.
pub fn write_confined_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let (parent, name) = split(path)?;
    let directory = platform::Directory::open(parent, true)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let staged = format!(".orca-{}-{sequence}.tmp", std::process::id());
    match directory.publish(OsStr::new(&staged), name, contents) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            use std::io::Read;
            let mut file = directory.open_regular(name)?;
            // A successful retry must still restrict a file from an older
            // version if it lacks the current private permissions.
            let mut saved = Vec::new();
            (&mut file)
                .take(contents.len() as u64 + 1)
                .read_to_end(&mut saved)?;
            if saved != contents {
                return Err(error);
            }
            super::atomic::restrict_to_owner(&file)
        }
        result => result,
    }
}

/// Remove a directory of regular sidecar files without following links.
/// A missing directory is already removed. Unexpected entries are reported.
pub fn remove_confined_files(path: &Path) -> io::Result<()> {
    let (parent, name) = split(path)?;
    let directory = match platform::Directory::open(parent, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match directory.remove_files(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub fn confined_files_size(path: &Path) -> io::Result<u64> {
    let directory = match platform::Directory::open(path, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    directory.files_size()
}

fn split(path: &Path) -> io::Result<(&Path, &OsStr)> {
    path.parent().zip(path.file_name()).ok_or_else(invalid_path)
}

fn invalid_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "expected an absolute path without parent traversal",
    )
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "expected a regular file")
}

#[cfg(unix)]
mod platform {
    use std::io::Write;

    use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags};

    use super::*;

    const DIRECTORY: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const LEAF: OFlags = OFlags::NOFOLLOW
        .union(OFlags::CLOEXEC)
        .union(OFlags::NONBLOCK);
    const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);

    pub(super) struct Directory(File);

    impl Directory {
        pub(super) fn open(path: &Path, create: bool) -> io::Result<Self> {
            // macOS provides these system paths as fixed aliases into /private.
            #[cfg(target_os = "macos")]
            let native;
            #[cfg(target_os = "macos")]
            let path = if ["/var", "/tmp"]
                .iter()
                .any(|prefix| path.starts_with(prefix))
            {
                native =
                    Path::new("/private").join(path.strip_prefix("/").map_err(|_| invalid_path())?);
                &native
            } else {
                path
            };
            let mut parts = path.components();
            if parts.next() != Some(Component::RootDir) {
                return Err(invalid_path());
            }
            let mut directory = Self(fs::open("/", DIRECTORY, Mode::empty())?.into());
            for part in parts {
                let Component::Normal(name) = part else {
                    return Err(invalid_path());
                };
                directory = match directory.child(name) {
                    Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                        match fs::mkdirat(&directory.0, name, Mode::RWXU) {
                            Ok(()) | Err(rustix::io::Errno::EXIST) => directory.child(name)?,
                            Err(error) => return Err(error.into()),
                        }
                    }
                    result => result?,
                };
            }
            Ok(directory)
        }

        fn child(&self, name: &OsStr) -> io::Result<Self> {
            Ok(Self(
                fs::openat(&self.0, name, DIRECTORY, Mode::empty())?.into(),
            ))
        }

        fn regular_metadata(&self, name: impl rustix::path::Arg) -> io::Result<fs::Stat> {
            let metadata = fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if !FileType::from_raw_mode(metadata.st_mode).is_file() {
                return Err(not_regular());
            }
            Ok(metadata)
        }

        pub(super) fn open_regular(&self, name: &OsStr) -> io::Result<File> {
            self.regular_metadata(name)?;
            let file = File::from(fs::openat(
                &self.0,
                name,
                OFlags::RDONLY | LEAF,
                Mode::empty(),
            )?);
            if !file.metadata()?.is_file() {
                return Err(not_regular());
            }
            Ok(file)
        }

        pub(super) fn publish(
            &self,
            staged: &OsStr,
            name: &OsStr,
            contents: &[u8],
        ) -> io::Result<()> {
            let mut file = File::from(fs::openat(
                &self.0,
                staged,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | LEAF,
                PRIVATE_FILE,
            )?);
            let result = (|| {
                fs::fchmod(&file, PRIVATE_FILE)?;
                file.write_all(contents)?;
                file.sync_all()?;
                fs::linkat(&self.0, staged, &self.0, name, AtFlags::empty())?;
                Ok(())
            })();
            let cleanup = fs::unlinkat(&self.0, staged, AtFlags::empty());
            result.and_then(|()| Ok(cleanup?))
        }

        pub(super) fn remove_files(&self, name: &OsStr) -> io::Result<()> {
            let child = self.child(name)?;
            for entry in Dir::read_from(&child.0)? {
                let entry = entry?;
                let file = entry.file_name();
                if matches!(file.to_bytes(), b"." | b"..") {
                    continue;
                }
                child.regular_metadata(file)?;
                fs::unlinkat(&child.0, file, AtFlags::empty())?;
            }
            fs::unlinkat(&self.0, name, AtFlags::REMOVEDIR)?;
            Ok(())
        }

        pub(super) fn files_size(&self) -> io::Result<u64> {
            let mut bytes = 0u64;
            for entry in Dir::read_from(&self.0)? {
                let entry = entry?;
                let name = entry.file_name();
                if !matches!(name.to_bytes(), b"." | b"..") {
                    bytes = bytes.saturating_add(self.regular_metadata(name)?.st_size as u64);
                }
            }
            Ok(bytes)
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::fs;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::PathBuf;

    use crate::fs::{PathPolicy, VerifiedPath, open_nofollow_nonblocking};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileDispositionInfo, ReOpenFile, SetFileInformationByHandle,
    };

    use super::*;

    // Every directory remains open without delete sharing during the operation.
    // Opening one component at a time keeps the checked parents in place.
    pub(super) struct Directory {
        path: PathBuf,
        _parents: Vec<VerifiedPath>,
    }

    impl Directory {
        pub(super) fn open(path: &Path, create: bool) -> io::Result<Self> {
            let path = crate::fs::extended_length_path(path);
            let policy = PathPolicy::windows_sandbox().with_unc_paths(true);
            policy
                .identity(path.to_str().ok_or_else(invalid_path)?)
                .map_err(io_error)?;
            let mut parts = path.components();
            let Some(Component::Prefix(prefix)) = parts.next() else {
                return Err(invalid_path());
            };
            if parts.next() != Some(Component::RootDir) {
                return Err(invalid_path());
            }
            let mut current = PathBuf::from(prefix.as_os_str());
            current.push("\\");
            let mut parents = vec![pin(policy, &current)?];
            for part in parts {
                let Component::Normal(name) = part else {
                    return Err(invalid_path());
                };
                current.push(name);
                let directory = match pin(policy, &current) {
                    Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                        match fs::create_dir(&current) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(error) => return Err(error),
                        }
                        pin(policy, &current)?
                    }
                    result => result?,
                };
                parents.push(directory);
            }
            Ok(Self {
                path: current,
                _parents: parents,
            })
        }

        pub(super) fn open_regular(&self, name: &OsStr) -> io::Result<File> {
            let path = self.path.join(name);
            PathPolicy::windows_sandbox()
                .with_unc_paths(true)
                .identity(path.to_str().ok_or_else(invalid_path)?)
                .map_err(io_error)?;
            let file = open_nofollow_nonblocking(&path).map_err(io_error)?;
            if !file.metadata()?.is_file() {
                return Err(not_regular());
            }
            Ok(file)
        }

        pub(super) fn publish(
            &self,
            staged: &OsStr,
            name: &OsStr,
            contents: &[u8],
        ) -> io::Result<()> {
            use std::io::Write;
            use std::os::windows::fs::OpenOptionsExt;
            let staged = self.path.join(staged);
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&staged)?;
            let result = (|| {
                super::super::atomic::restrict_to_owner(&file)?;
                file.write_all(contents)?;
                file.sync_all()?;
                fs::hard_link(&staged, self.path.join(name))
            })();
            drop(file);
            let cleanup = fs::remove_file(staged);
            result.and(cleanup)
        }

        pub(super) fn remove_files(&self, name: &OsStr) -> io::Result<()> {
            let path = self.path.join(name);
            let policy = PathPolicy::windows_sandbox().with_unc_paths(true);
            let directory = pin_with_access(policy, &path, FILE_READ_ATTRIBUTES | DELETE)?;
            let child = Self {
                path,
                _parents: Vec::new(),
            };
            for entry in fs::read_dir(&child.path)? {
                let entry = entry?;
                delete_by_handle(&child.open_regular(&entry.file_name())?)?;
            }
            mark_for_removal(directory.file())
        }

        pub(super) fn files_size(&self) -> io::Result<u64> {
            fs::read_dir(&self.path)?.try_fold(0u64, |bytes, entry| {
                let file = self.open_regular(&entry?.file_name())?;
                Ok(bytes.saturating_add(file.metadata()?.len()))
            })
        }
    }

    fn pin(policy: PathPolicy, path: &Path) -> io::Result<VerifiedPath> {
        pin_with_access(policy, path, FILE_READ_ATTRIBUTES)
    }

    fn pin_with_access(policy: PathPolicy, path: &Path, access: u32) -> io::Result<VerifiedPath> {
        let directory = policy
            .open_no_follow_shared(path, access, FILE_SHARE_READ | FILE_SHARE_WRITE)
            .map_err(io_error)?;
        if !directory.file().metadata()?.is_dir()
            || directory.identity()
                != &policy
                    .identity(path.to_str().ok_or_else(invalid_path)?)
                    .map_err(io_error)?
        {
            return Err(invalid_path());
        }
        Ok(directory)
    }

    fn delete_by_handle(file: &File) -> io::Result<()> {
        let handle = unsafe {
            ReOpenFile(
                file.as_raw_handle(),
                DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        mark_for_removal(&unsafe { File::from_raw_handle(handle) })
    }

    fn mark_for_removal(file: &File) -> io::Result<()> {
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        let result = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&raw const disposition).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn io_error(error: crate::PlatformError) -> io::Error {
        let kind = match error {
            crate::PlatformError::Io { kind, .. } => kind,
            _ => io::ErrorKind::InvalidInput,
        };
        io::Error::new(kind, error)
    }
}
