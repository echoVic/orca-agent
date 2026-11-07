//! Structured causes behind the [`io::Error`] values this crate returns.
//!
//! The public API is deliberately [`std::io::Result`]-shaped so the runtime can
//! keep using its existing error plumbing. Every failure that is not a plain
//! OS error carries a [`RewindError`] as the `io::Error`'s inner error and can
//! be recovered with [`rewind_error`].

use std::fmt;
use std::io;
use std::path::PathBuf;

/// What went wrong inside the rewind storage layer.
#[derive(Debug)]
pub enum RewindError {
    /// A file was larger than [`crate::MAX_SNAPSHOT_FILE_BYTES`].
    TooLarge {
        path: PathBuf,
        observed_bytes: u64,
        limit: u64,
    },
    /// A path exists but is not a regular file (symlink, directory, device...).
    NotRegular { path: PathBuf },
    /// A snapshot path was absolute, contained `..`, escaped the root or
    /// crossed a symlink.
    PathEscape { path: String, reason: String },
    /// The workspace root is a symlink or not a directory.
    RootNotDirectory { root: PathBuf },
    /// A digest string was not 64 lowercase hex characters.
    InvalidDigest { digest: String },
    /// A blob decoded to a different number of bytes than the reference said.
    SizeMismatch {
        sha256: String,
        expected: u64,
        actual: u64,
    },
    /// A blob's content did not hash to the reference digest.
    DigestMismatch { expected: String, actual: String },
    /// A blob could not be read or decoded (missing, truncated, corrupt).
    CorruptBlob { sha256: String, detail: String },
    /// The workspace changed between planning and applying a restore.
    ConcurrentWorkspaceChange {
        path: String,
        expected: Option<String>,
        actual: Option<String>,
    },
    /// A rollback operation id was not a safe file stem.
    InvalidOperationId { op_id: String },
    /// A journal/backup file was malformed.
    JournalCorrupt { detail: String },
    /// A backup file did not hash to the digest recorded for it.
    CorruptBackup { sha256: String },
}

impl RewindError {
    /// The [`io::ErrorKind`] this failure is reported under.
    pub fn io_kind(&self) -> io::ErrorKind {
        match self {
            Self::TooLarge { .. }
            | Self::SizeMismatch { .. }
            | Self::DigestMismatch { .. }
            | Self::CorruptBlob { .. }
            | Self::ConcurrentWorkspaceChange { .. }
            | Self::JournalCorrupt { .. }
            | Self::CorruptBackup { .. } => io::ErrorKind::InvalidData,
            Self::NotRegular { .. }
            | Self::PathEscape { .. }
            | Self::RootNotDirectory { .. }
            | Self::InvalidDigest { .. }
            | Self::InvalidOperationId { .. } => io::ErrorKind::InvalidInput,
        }
    }

    /// True for the "file above the snapshot cap" capture refusal.
    pub fn is_too_large(&self) -> bool {
        matches!(self, Self::TooLarge { .. })
    }

    /// True when the workspace moved underneath a planned restore.
    pub fn is_concurrent_change(&self) -> bool {
        matches!(self, Self::ConcurrentWorkspaceChange { .. })
    }

    /// True for a structurally invalid workspace-relative path.
    pub fn is_path_escape(&self) -> bool {
        matches!(self, Self::PathEscape { .. })
    }

    /// True when the path exists but is a symlink, directory or special file.
    pub fn is_not_regular(&self) -> bool {
        matches!(self, Self::NotRegular { .. })
    }
}

impl fmt::Display for RewindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge {
                path,
                observed_bytes,
                limit,
            } => write!(
                f,
                "{} is too large to snapshot ({observed_bytes} bytes, limit {limit})",
                path.display()
            ),
            Self::NotRegular { path } => {
                write!(f, "{} is not a regular file", path.display())
            }
            Self::PathEscape { path, reason } => {
                write!(f, "refusing path {path:?}: {reason}")
            }
            Self::RootNotDirectory { root } => {
                write!(f, "workspace root {} is not a directory", root.display())
            }
            Self::InvalidDigest { digest } => {
                write!(f, "invalid sha256 digest {digest:?}")
            }
            Self::SizeMismatch {
                sha256,
                expected,
                actual,
            } => write!(
                f,
                "blob {sha256} decoded to {actual} bytes, expected {expected}"
            ),
            Self::DigestMismatch { expected, actual } => {
                write!(f, "blob digest mismatch: expected {expected}, got {actual}")
            }
            Self::CorruptBlob { sha256, detail } => {
                write!(f, "blob {sha256} is unreadable: {detail}")
            }
            Self::ConcurrentWorkspaceChange {
                path,
                expected,
                actual,
            } => write!(
                f,
                "workspace changed while the restore was planned: {path} is {:?}, planned {:?}",
                actual, expected
            ),
            Self::InvalidOperationId { op_id } => {
                write!(f, "invalid rewind operation id {op_id:?}")
            }
            Self::JournalCorrupt { detail } => {
                write!(f, "rewind journal is corrupt: {detail}")
            }
            Self::CorruptBackup { sha256 } => {
                write!(f, "rewind backup {sha256} failed digest verification")
            }
        }
    }
}

impl std::error::Error for RewindError {}

impl From<RewindError> for io::Error {
    fn from(error: RewindError) -> Self {
        io::Error::new(error.io_kind(), error)
    }
}

/// Recover the structured cause from an [`io::Error`] returned by this crate.
///
/// Returns `None` for plain OS errors (permission denied, missing parent...).
pub fn rewind_error(error: &io::Error) -> Option<&RewindError> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<RewindError>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewind_error_round_trips_through_io_error() {
        let error: io::Error = RewindError::TooLarge {
            path: PathBuf::from("/tmp/big"),
            observed_bytes: 17,
            limit: 16,
        }
        .into();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let recovered = rewind_error(&error).expect("structured cause");
        assert!(recovered.is_too_large());
        assert!(recovered.to_string().contains("too large"));
    }

    #[test]
    fn plain_io_errors_have_no_structured_cause() {
        let error = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(rewind_error(&error).is_none());
    }
}
