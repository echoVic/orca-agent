use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path};

use crate::PlatformError;

/// Identity obtained from an open filesystem handle.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryEntryKind {
    Directory,
    File,
    Other,
}

pub struct DirectoryEntry {
    pub name: OsString,
    pub kind: DirectoryEntryKind,
    pub hidden: bool,
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

    pub fn read_names(&self) -> io::Result<Vec<OsString>> {
        Ok(self
            .read_entries()?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    }

    pub fn read_entries(&self) -> io::Result<Vec<DirectoryEntry>> {
        self.0.read_entries()
    }

    pub fn entry_is_directory(&self, name: &OsStr) -> io::Result<bool> {
        validate_name(name)?;
        self.0.entry_is_directory(name)
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

    pub fn remove_directory(&self, name: &OsStr) -> io::Result<()> {
        validate_name(name)?;
        self.0.remove_directory(name)
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

#[cfg(all(test, unix))]
#[test]
fn a_verified_directory_lists_the_opened_directory_after_its_name_changes() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let path = root.join("directory");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("original.txt"), "original").unwrap();
    let directory = VerifiedDirectory::open(&path).unwrap();
    std::fs::rename(&path, root.join("moved")).unwrap();
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("replacement.txt"), "replacement").unwrap();

    assert_eq!(directory.read_names().unwrap(), ["original.txt"]);
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn detects_casefold_in_an_empty_directory_with_a_case_sensitive_parent() {
    let Some(path) = std::env::var_os("ORCA_TEST_CASEFOLD_DIRECTORY") else {
        return;
    };
    let path = Path::new(&path);
    let dir = VerifiedDirectory::open(path).unwrap();
    assert!(dir.ignores_case(path));
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;

    #[test]
    fn verified_directory_keeps_its_ancestor_name_in_use() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        fs::create_dir_all(parent.join("child")).unwrap();
        let dir = VerifiedDirectory::open(&parent.join("child")).unwrap();
        assert!(fs::rename(&parent, temp.path().join("renamed")).is_err());
        dir.create_file(OsStr::new("note.txt")).unwrap();
        assert!(parent.join("child/note.txt").is_file());
    }

    #[test]
    fn windows_directory_enumeration_uses_its_open_handle_after_a_reparse_swap() {
        use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("directory");
        let outside = temp.path().join("neighbor");
        fs::create_dir(&path).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("outside.txt"), "outside").unwrap();
        let reader = VerifiedDirectory::open(&path).unwrap();
        assert!(reader.read_names().unwrap().is_empty());
        let writer = fs::OpenOptions::new()
            .access_mode(GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .unwrap();
        let mut target = OsString::from(r"\??\");
        target.push(&outside);
        let target: Vec<u16> = target.encode_wide().collect();
        let len = u16::try_from(target.len() * 2).unwrap();
        const MOUNT_POINT_TAG: u32 = 0xA0000003;
        const SET_REPARSE_POINT: u32 = 0x000900A4;
        const DELETE_REPARSE_POINT: u32 = 0x000900AC;
        let mut reparse = Vec::new();
        reparse.extend(MOUNT_POINT_TAG.to_le_bytes());
        for field in [len + 12, 0, 0, len, len + 2, 0] {
            reparse.extend(field.to_le_bytes());
        }
        reparse.extend(target.into_iter().chain([0, 0]).flat_map(u16::to_le_bytes));
        let reparse_control = |code, data: &[u8]| {
            let mut returned = 0;
            let result = unsafe {
                DeviceIoControl(
                    writer.as_raw_handle(),
                    code,
                    data.as_ptr().cast(),
                    data.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(result, 0, "{}", io::Error::last_os_error());
        };
        reparse_control(SET_REPARSE_POINT, &reparse);
        let redirect_visible_by_name = path.join("outside.txt").is_file();
        let observed = reader.read_names();
        reparse_control(DELETE_REPARSE_POINT, &[3, 0, 0, 160, 0, 0, 0, 0]);
        assert!(redirect_visible_by_name);
        assert!(observed.is_ok_and(|names| names.is_empty()));
    }

    #[test]
    fn deleted_file_name_can_be_reused_while_a_reader_is_still_open() {
        let temp = tempfile::tempdir().unwrap();
        let name = OsStr::new("note.txt");
        fs::write(temp.path().join(name), "first").unwrap();
        let reader = File::open(temp.path().join(name)).unwrap();
        let dir = VerifiedDirectory::open(temp.path()).unwrap();
        let file = dir.open_file(name, FileAccess::Delete).unwrap();
        dir.remove_file(name, &file).unwrap();
        drop(file);
        dir.create_file(name).unwrap();
        assert_eq!(reader.metadata().unwrap().len(), 5);
    }
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

        pub(super) fn read_entries(&self) -> io::Result<Vec<DirectoryEntry>> {
            use rustix::fs::{AtFlags, FileType, statat};
            let mut entries = Vec::new();
            for entry in rustix::fs::Dir::read_from(&self.0)? {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let file_type = match entry.file_type() {
                    FileType::Unknown => {
                        statat(&self.0, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
                            .map(|stat| FileType::from_raw_mode(stat.st_mode))
                            .unwrap_or(FileType::Unknown)
                    }
                    known => known,
                };
                entries.push(DirectoryEntry {
                    name: OsStr::from_bytes(name).to_owned(),
                    kind: match file_type {
                        FileType::Directory => DirectoryEntryKind::Directory,
                        FileType::RegularFile => DirectoryEntryKind::File,
                        _ => DirectoryEntryKind::Other,
                    },
                    hidden: name.starts_with(b"."),
                });
            }
            Ok(entries)
        }

        pub(super) fn entry_is_directory(&self, name: &OsStr) -> io::Result<bool> {
            use rustix::fs::{AtFlags, FileType, statat};
            let entry = statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
            match FileType::from_raw_mode(entry.st_mode) {
                FileType::Symlink => Err(io::ErrorKind::PermissionDenied.into()),
                kind => Ok(kind == FileType::Directory),
            }
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

        pub(super) fn remove_directory(&self, name: &OsStr) -> io::Result<()> {
            let name = CString::new(name.as_bytes())?;
            let result =
                unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
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
            #[cfg(target_os = "linux")]
            {
                // Filesystems such as ext4 and f2fs store case folding on
                // the directory itself, including when it has no entries.
                const FS_CASEFOLD_FL: libc::c_int = 0x4000_0000;
                let mut flags = 0 as libc::c_int;
                let result =
                    unsafe { libc::ioctl(self.0.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) };
                if result == 0 && flags & FS_CASEFOLD_FL != 0 {
                    return true;
                }
            }
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
    use crate::fs::PathPolicy;
    use std::mem::{MaybeUninit, offset_of, size_of};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::PathBuf;
    use std::ptr::null;
    use std::sync::{Arc, Mutex};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE,
        RtlNtStatusToDosError, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_CASE_SENSITIVE_INFO,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileCaseSensitiveInfo,
        FileDispositionInfoEx, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, SYNCHRONIZE,
        SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES;
    const SHARE_READ_WRITE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

    pub(super) struct Directory(Arc<DirectoryHandle>);

    struct DirectoryHandle {
        file: File,
        path: PathBuf,
        _parent: Option<Arc<DirectoryHandle>>,
        enumeration: Mutex<()>,
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
        let verified = PathPolicy::windows_sandbox()
            .with_unc_paths(true)
            .open_no_follow_pinned(&root, DIRECTORY_ACCESS)
            .map_err(as_io)?;
        let mut directory = Directory::new(verified.into_file(), root, None);
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

    fn read_entry_batch(mut bytes: &[u8], entries: &mut Vec<DirectoryEntry>) -> io::Result<()> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid directory entry");
        let field = |bytes: &[u8], start| -> Option<u32> {
            Some(u32::from_ne_bytes(
                bytes.get(start..start + 4)?.try_into().ok()?,
            ))
        };
        loop {
            let next = field(bytes, offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset))
                .ok_or_else(invalid)? as usize;
            let attrs = field(bytes, offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes))
                .ok_or_else(invalid)?;
            let len = field(bytes, offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength))
                .ok_or_else(invalid)? as usize;
            let start = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            let end = start.checked_add(len).ok_or_else(invalid)?;
            if len % 2 != 0 || (next != 0 && next < end) {
                return Err(invalid());
            }
            let wide = bytes
                .get(start..end)
                .ok_or_else(invalid)?
                .chunks_exact(2)
                .map(|part| u16::from_ne_bytes([part[0], part[1]]))
                .collect::<Vec<_>>();
            let name = OsString::from_wide(&wide);
            if name != "." && name != ".." {
                let kind = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    DirectoryEntryKind::Other
                } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    DirectoryEntryKind::Directory
                } else {
                    DirectoryEntryKind::File
                };
                entries.push(DirectoryEntry {
                    hidden: attrs & FILE_ATTRIBUTE_HIDDEN != 0
                        || name.as_encoded_bytes().starts_with(b"."),
                    name,
                    kind,
                });
            }
            if next == 0 {
                return Ok(());
            }
            bytes = bytes.get(next..).ok_or_else(invalid)?;
        }
    }

    impl Directory {
        fn new(file: File, path: PathBuf, parent: Option<Arc<DirectoryHandle>>) -> Self {
            Self(Arc::new(DirectoryHandle {
                file,
                path,
                _parent: parent,
                enumeration: Mutex::new(()),
            }))
        }

        pub(super) fn file(&self) -> &File {
            &self.0.file
        }

        pub(super) fn read_entries(&self) -> io::Result<Vec<DirectoryEntry>> {
            // The enumeration cursor belongs to this handle.
            let _guard = self.0.enumeration.lock().unwrap();
            let mut entries = Vec::new();
            let mut class = FileIdBothDirectoryRestartInfo;
            let mut buffer = [0u64; 2048];
            loop {
                let result = unsafe {
                    GetFileInformationByHandleEx(
                        self.file().as_raw_handle(),
                        class,
                        buffer.as_mut_ptr().cast(),
                        size_of::<[u64; 2048]>() as u32,
                    )
                };
                if result == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                        return Ok(entries);
                    }
                    return Err(error);
                }
                class = FileIdBothDirectoryInfo;
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        buffer.as_ptr().cast::<u8>(),
                        size_of::<[u64; 2048]>(),
                    )
                };
                read_entry_batch(bytes, &mut entries)?;
            }
        }

        pub(super) fn entry_is_directory(&self, name: &OsStr) -> io::Result<bool> {
            let file =
                self.open_child(name, FILE_READ_ATTRIBUTES, FILE_OPEN, 0, SHARE_READ_WRITE)?;
            Ok(file.metadata()?.is_dir())
        }

        pub(super) fn child(&self, name: &OsStr) -> io::Result<Self> {
            let file = self.open_child(
                name,
                DIRECTORY_ACCESS,
                FILE_OPEN,
                FILE_DIRECTORY_FILE,
                SHARE_READ_WRITE,
            )?;
            Ok(Self::new(
                file,
                self.0.path.join(name),
                Some(self.0.clone()),
            ))
        }

        pub(super) fn open_file(&self, name: &OsStr, access: FileAccess) -> io::Result<File> {
            let access = match access {
                FileAccess::Read => GENERIC_READ,
                FileAccess::Update => GENERIC_READ | GENERIC_WRITE,
                FileAccess::Delete => GENERIC_READ | DELETE,
            };
            self.open_child(
                name,
                access,
                FILE_OPEN,
                FILE_NON_DIRECTORY_FILE,
                SHARE_READ_WRITE,
            )
        }

        pub(super) fn create_directory(&self, name: &OsStr) -> io::Result<()> {
            self.open_child(
                name,
                FILE_READ_ATTRIBUTES,
                FILE_CREATE,
                FILE_DIRECTORY_FILE,
                SHARE_READ_WRITE,
            )
            .map(drop)
        }

        pub(super) fn create_file(&self, name: &OsStr) -> io::Result<File> {
            self.open_child(
                name,
                GENERIC_READ | GENERIC_WRITE,
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE,
                SHARE_READ_WRITE,
            )
        }

        fn open_child(
            &self,
            name: &OsStr,
            access: u32,
            disposition: u32,
            options: u32,
            sharing: u32,
        ) -> io::Result<File> {
            // Apply the same Windows naming rules as absolute workspace paths.
            let path = self.0.path.join(name);
            let text = path.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8")
            })?;
            PathPolicy::windows_sandbox()
                .with_unc_paths(true)
                .identity(text)
                .map_err(as_io)?;
            let mut wide: Vec<_> = name.encode_wide().collect();
            let length = u16::try_from(wide.len() * size_of::<u16>()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "child name is too long")
            })?;
            let name = UNICODE_STRING {
                Length: length,
                MaximumLength: length,
                Buffer: wide.as_mut_ptr(),
            };
            let attributes = OBJECT_ATTRIBUTES {
                Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
                RootDirectory: self.file().as_raw_handle(),
                ObjectName: &name,
                Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
                ..Default::default()
            };
            let mut handle = std::ptr::null_mut();
            let mut status_block = IO_STATUS_BLOCK::default();
            let status = unsafe {
                NtCreateFile(
                    &mut handle,
                    access | SYNCHRONIZE,
                    &attributes,
                    &mut status_block,
                    null(),
                    FILE_ATTRIBUTE_NORMAL,
                    sharing,
                    disposition,
                    options | FILE_SYNCHRONOUS_IO_NONALERT,
                    null(),
                    0,
                )
            };
            if status < 0 {
                return Err(io::Error::from_raw_os_error(
                    unsafe { RtlNtStatusToDosError(status) } as i32,
                ));
            }
            Ok(unsafe { File::from_raw_handle(handle) })
        }

        pub(super) fn remove_directory(&self, name: &OsStr) -> io::Result<()> {
            let file = self.open_child(
                name,
                DELETE,
                FILE_OPEN,
                FILE_DIRECTORY_FILE,
                SHARE_READ_WRITE | FILE_SHARE_DELETE,
            )?;
            Self::unlink(&file)
        }

        pub(super) fn remove_file(&self, _name: &OsStr, file: &File) -> io::Result<()> {
            if file_links(file)?.count > 1 {
                return Err(stale_path());
            }
            Self::unlink(file)
        }

        fn unlink(file: &File) -> io::Result<()> {
            let info = FILE_DISPOSITION_INFO_EX {
                Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
            };
            let result = unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfoEx,
                    (&raw const info).cast(),
                    size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
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
