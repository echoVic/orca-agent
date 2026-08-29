use std::fs::File;
use std::path::{Path, PathBuf};

use crate::PlatformError;

#[derive(Clone, Debug)]
pub struct PathIdentity {
    prefix: WindowsPrefix,
    components: Vec<IdentityComponent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WindowsPrefix {
    Drive {
        key: char,
        display: String,
        extended: bool,
    },
    Unc {
        server_key: String,
        share_key: String,
        display: String,
        extended: bool,
    },
    Device {
        key: String,
        display: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IdentityComponent {
    key: String,
    display: String,
}

impl PartialEq for PathIdentity {
    fn eq(&self, other: &Self) -> bool {
        prefix_identity(&self.prefix) == prefix_identity(&other.prefix)
            && self
                .components
                .iter()
                .map(|component| &component.key)
                .eq(other.components.iter().map(|component| &component.key))
    }
}

impl Eq for PathIdentity {}

impl PathIdentity {
    pub fn windows(path: &str) -> Result<Self, PlatformError> {
        PathPolicy::windows_identity().identity(path)
    }

    pub fn root_display(&self) -> &str {
        match &self.prefix {
            WindowsPrefix::Drive { display, .. }
            | WindowsPrefix::Unc { display, .. }
            | WindowsPrefix::Device { display, .. } => display,
        }
    }

    pub fn is_within(&self, root: &Self) -> bool {
        prefix_identity(&self.prefix) == prefix_identity(&root.prefix)
            && self.components.len() >= root.components.len()
            && self
                .components
                .iter()
                .zip(&root.components)
                .all(|(candidate, expected)| candidate.key == expected.key)
    }

    pub fn storage_key(&self) -> String {
        let mut key = match &self.prefix {
            WindowsPrefix::Drive { key, .. } => format!("drive:{key}"),
            WindowsPrefix::Unc {
                server_key,
                share_key,
                ..
            } => format!("unc:{server_key}/{share_key}"),
            WindowsPrefix::Device { key, .. } => format!("device:{key}"),
        };
        for component in &self.components {
            key.push('/');
            key.push_str(&component.key);
        }
        key
    }

    pub fn display_path(&self) -> PathBuf {
        let mut path = self.root_display().to_string();
        for component in &self.components {
            if !path.ends_with('\\') {
                path.push('\\');
            }
            path.push_str(&component.display);
        }
        PathBuf::from(path)
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.components
            .iter()
            .map(|component| component.display.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathPolicy {
    allow_unc_paths: bool,
    allow_alternate_data_streams: bool,
    allow_device_namespaces: bool,
    allow_reparse_points: bool,
}

impl PathPolicy {
    pub fn windows_sandbox() -> Self {
        Self {
            allow_unc_paths: false,
            allow_alternate_data_streams: false,
            allow_device_namespaces: false,
            allow_reparse_points: false,
        }
    }

    fn windows_identity() -> Self {
        Self::windows_sandbox().with_unc_paths(true)
    }

    pub fn with_unc_paths(mut self, allow: bool) -> Self {
        self.allow_unc_paths = allow;
        self
    }

    pub fn with_alternate_data_streams(mut self, allow: bool) -> Self {
        self.allow_alternate_data_streams = allow;
        self
    }

    pub fn with_device_namespaces(mut self, allow: bool) -> Self {
        self.allow_device_namespaces = allow;
        self
    }

    pub fn with_reparse_points(mut self, allow: bool) -> Self {
        self.allow_reparse_points = allow;
        self
    }

    pub fn identity(self, path: &str) -> Result<PathIdentity, PlatformError> {
        parse_windows_path(path, self)
    }

    #[cfg(windows)]
    pub fn open_no_follow(self, path: &Path) -> Result<VerifiedPath, PlatformError> {
        windows::open_no_follow(path, self, windows::DEFAULT_ACCESS_MODE)
    }

    #[cfg(windows)]
    pub fn open_no_follow_with_access(
        self,
        path: &Path,
        access_mode: u32,
    ) -> Result<VerifiedPath, PlatformError> {
        windows::open_no_follow(path, self, access_mode)
    }
}

pub struct VerifiedPath {
    file: File,
    identity: PathIdentity,
    source: PathBuf,
}

impl std::fmt::Debug for VerifiedPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedPath")
            .field("identity", &self.identity)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl VerifiedPath {
    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn identity(&self) -> &PathIdentity {
        &self.identity
    }

    pub fn source(&self) -> &Path {
        &self.source
    }
}

fn parse_windows_path(path: &str, policy: PathPolicy) -> Result<PathIdentity, PlatformError> {
    if path.is_empty() {
        return invalid(path, "path is empty");
    }
    let normalized = path.replace('/', "\\");
    let upper = normalized.to_ascii_uppercase();

    let (prefix, remainder) = if upper.starts_with(r"\\?\UNC\") {
        if !policy.allow_unc_paths {
            return invalid(path, "UNC paths are disabled by policy");
        }
        parse_unc_prefix(path, &normalized[8..], true)?
    } else if upper.starts_with(r"\\?\") {
        parse_extended_drive_prefix(path, &normalized[4..])?
    } else if upper.starts_with(r"\\.\") {
        if !policy.allow_device_namespaces {
            return invalid(path, "device namespaces are disabled by policy");
        }
        parse_device_prefix(path, &normalized[4..])?
    } else if let Some(unc_path) = normalized.strip_prefix(r"\\") {
        if !policy.allow_unc_paths {
            return invalid(path, "UNC paths are disabled by policy");
        }
        parse_unc_prefix(path, unc_path, false)?
    } else {
        parse_drive_prefix(path, &normalized)?
    };

    let components = parse_components(path, remainder, policy)?;
    Ok(PathIdentity { prefix, components })
}

fn parse_drive_prefix<'a>(
    original: &str,
    path: &'a str,
) -> Result<(WindowsPrefix, &'a str), PlatformError> {
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return invalid(
            original,
            "expected an absolute drive path such as C:\\\\repo",
        );
    }
    let letter = (bytes[0] as char).to_ascii_uppercase();
    Ok((
        WindowsPrefix::Drive {
            key: letter,
            display: format!("{letter}:\\"),
            extended: false,
        },
        &path[3..],
    ))
}

fn parse_extended_drive_prefix<'a>(
    original: &str,
    path: &'a str,
) -> Result<(WindowsPrefix, &'a str), PlatformError> {
    let upper = path.to_ascii_uppercase();
    if upper.starts_with("GLOBALROOT\\") || upper.starts_with("DEVICE\\") {
        return invalid(
            original,
            "device namespaces are not extended-length file paths",
        );
    }
    let (mut prefix, remainder) = parse_drive_prefix(original, path)?;
    if let WindowsPrefix::Drive {
        display, extended, ..
    } = &mut prefix
    {
        *display = format!(r"\\?\{display}");
        *extended = true;
    }
    Ok((prefix, remainder))
}

fn parse_unc_prefix<'a>(
    original: &str,
    path: &'a str,
    extended: bool,
) -> Result<(WindowsPrefix, &'a str), PlatformError> {
    let mut parts = path.splitn(3, '\\');
    let server = parts.next().unwrap_or_default();
    let share = parts.next().unwrap_or_default();
    let remainder = parts.next().unwrap_or_default();
    if server.is_empty() || share.is_empty() {
        return invalid(original, "UNC paths require both server and share names");
    }
    validate_component(original, server, false, false)?;
    validate_component(original, share, false, false)?;
    let display = if extended {
        format!(r"\\?\UNC\{server}\{share}")
    } else {
        format!(r"\\{server}\{share}")
    };
    Ok((
        WindowsPrefix::Unc {
            server_key: identity_key(server),
            share_key: identity_key(share),
            display,
            extended,
        },
        remainder,
    ))
}

fn parse_device_prefix<'a>(
    original: &str,
    path: &'a str,
) -> Result<(WindowsPrefix, &'a str), PlatformError> {
    let mut parts = path.splitn(2, '\\');
    let device = parts.next().unwrap_or_default();
    if device.is_empty() {
        return invalid(original, "device namespace requires a device name");
    }
    Ok((
        WindowsPrefix::Device {
            key: identity_key(device),
            display: format!(r"\\.\{device}"),
        },
        parts.next().unwrap_or_default(),
    ))
}

fn parse_components(
    original: &str,
    remainder: &str,
    policy: PathPolicy,
) -> Result<Vec<IdentityComponent>, PlatformError> {
    let mut components = Vec::new();
    for component in remainder
        .split('\\')
        .filter(|component| !component.is_empty())
    {
        if component == "." {
            continue;
        }
        if component == ".." {
            return invalid(
                original,
                "parent traversal is rejected before canonicalization",
            );
        }
        validate_component(
            original,
            component,
            policy.allow_alternate_data_streams,
            true,
        )?;
        components.push(IdentityComponent {
            key: identity_key(component),
            display: component.to_string(),
        });
    }
    Ok(components)
}

fn validate_component(
    original: &str,
    component: &str,
    allow_alternate_data_streams: bool,
    reject_reserved_device_names: bool,
) -> Result<(), PlatformError> {
    if component.ends_with(['.', ' ']) {
        return invalid(
            original,
            "components ending in a dot or space are ambiguous on Windows",
        );
    }
    if component.chars().any(|character| {
        character <= '\u{1f}'
            || character == '<'
            || character == '>'
            || character == '"'
            || character == '|'
            || character == '?'
            || character == '*'
    }) {
        return invalid(original, "component contains a Windows-reserved character");
    }
    if !allow_alternate_data_streams && component.contains(':') {
        return invalid(original, "alternate data streams are disabled by policy");
    }
    let file_name = component.split(':').next().unwrap_or(component);
    let base = file_name.split('.').next().unwrap_or(file_name);
    if reject_reserved_device_names && is_reserved_device_name(base) {
        return invalid(original, "component uses a reserved Windows device name");
    }
    Ok(())
}

fn is_reserved_device_name(value: &str) -> bool {
    let value = value.to_ascii_uppercase();
    matches!(value.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || value
            .strip_prefix("COM")
            .or_else(|| value.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn identity_key(value: &str) -> String {
    value.to_lowercase()
}

#[derive(Eq, PartialEq)]
enum PrefixIdentity<'a> {
    // DOS and extended-length spellings (C:\\... and \\?\\C:\\...) address
    // the same Windows namespace. The syntax marker must not split capability
    // or setup-receipt identity for one workspace.
    Drive(char),
    Unc(&'a str, &'a str),
    Device(&'a str),
}

fn prefix_identity(prefix: &WindowsPrefix) -> PrefixIdentity<'_> {
    match prefix {
        WindowsPrefix::Drive { key, .. } => PrefixIdentity::Drive(*key),
        WindowsPrefix::Unc {
            server_key,
            share_key,
            ..
        } => PrefixIdentity::Unc(server_key, share_key),
        WindowsPrefix::Device { key, .. } => PrefixIdentity::Device(key),
    }
}

fn invalid<T>(path: &str, reason: &str) -> Result<T, PlatformError> {
    Err(PlatformError::InvalidPathIdentity {
        path: path.to_string(),
        reason: reason.to_string(),
    })
}

#[cfg(windows)]
mod windows {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };

    use super::*;

    pub(super) const DEFAULT_ACCESS_MODE: u32 = FILE_READ_ATTRIBUTES;

    pub(super) fn open_no_follow(
        path: &Path,
        policy: PathPolicy,
        access_mode: u32,
    ) -> Result<VerifiedPath, PlatformError> {
        let text = path
            .to_str()
            .ok_or_else(|| PlatformError::InvalidPathIdentity {
                path: path.to_string_lossy().into_owned(),
                reason: "path is not valid UTF-8".to_string(),
            })?;
        policy.identity(text)?;

        let file = std::fs::OpenOptions::new()
            .access_mode(access_mode)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(|error| {
                PlatformError::io("open path without following reparse points", error)
            })?;
        let metadata = file
            .metadata()
            .map_err(|error| PlatformError::io("inspect no-follow path metadata", error))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && !policy.allow_reparse_points
        {
            return Err(PlatformError::ReparsePointRejected {
                path: path.to_path_buf(),
            });
        }

        let final_path = final_path_from_handle(&file)?;
        let identity = policy.identity(&final_path)?;
        Ok(VerifiedPath {
            file,
            identity,
            source: path.to_path_buf(),
        })
    }

    fn final_path_from_handle(file: &File) -> Result<String, PlatformError> {
        let handle = file.as_raw_handle();
        let flags = FILE_NAME_NORMALIZED | VOLUME_NAME_DOS;
        let required = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, flags) };
        if required == 0 {
            return Err(PlatformError::io(
                "measure final path from handle",
                std::io::Error::last_os_error(),
            ));
        }
        let mut buffer = vec![0_u16; required as usize + 1];
        let written = unsafe {
            GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, flags)
        };
        if written == 0 {
            return Err(PlatformError::io(
                "read final path from handle",
                std::io::Error::last_os_error(),
            ));
        }
        if written as usize >= buffer.len() {
            return Err(PlatformError::Io {
                kind: std::io::ErrorKind::InvalidData,
                message: "final path changed while reading its handle identity".to_string(),
            });
        }
        String::from_utf16(&buffer[..written as usize]).map_err(|error| PlatformError::Io {
            kind: std::io::ErrorKind::InvalidData,
            message: format!("final path from handle is not valid UTF-16: {error}"),
        })
    }
}
