//! Content-addressed pre-image blob store (`<session_root>/objects/`).

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RewindError;
use crate::path::{BoundedFile, read_regular_file, sha256_hex, sync_dir, validate_digest};
use crate::{MAX_SNAPSHOT_FILE_BYTES, ZSTD_THRESHOLD_BYTES};

/// zstd compression level used for text pre-images (§6.3).
const ZSTD_LEVEL: i32 = 3;

/// Slack allowed when reading the *encoded* blob file before decoding. Raw
/// blobs are exactly `size` bytes; zstd frames may be a few bytes larger than
/// their input, so the read is bounded rather than exact.
const ENCODED_READ_ALLOWANCE: u64 = 1024 * 1024;

/// How a blob's bytes are stored on disk.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobEncoding {
    /// Bytes stored verbatim.
    Raw,
    /// zstd frame of the bytes (only chosen for UTF-8 text >= 4 KiB).
    Zstd,
}

/// An immutable, content-addressed pre-image.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Lowercase hex sha256 of the *decoded* content.
    pub sha256: String,
    /// Decoded content length in bytes.
    pub size: u64,
    /// Unix permission bits captured from the source file.
    pub mode: Option<u32>,
    pub encoding: BlobEncoding,
}

/// The state of a path at snapshot time.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PreImage {
    /// The path did not exist: restoring the target deletes it (tombstone).
    Missing,
    /// The path existed with this content.
    Blob(BlobRef),
}

/// Summary of a [`RewindStore::gc`] sweep.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct GcReport {
    pub removed_blobs: u64,
    pub removed_bytes: u64,
}

/// Per-session pre-image store.
///
/// Layout: `<root>/objects/<2-hex>/<sha256>` plus a `<root>/tmp/` scratch
/// directory used to publish blobs with an atomic rename.
#[derive(Debug)]
pub struct RewindStore {
    root: PathBuf,
}

impl RewindStore {
    /// Open (creating if needed) the store for one session.
    ///
    /// `session_root` is `$ORCA_HOME/rewind/sessions/<session_id>/`.
    pub fn open(session_root: &Path) -> io::Result<Self> {
        let store = Self {
            root: session_root.to_path_buf(),
        };
        fs::create_dir_all(store.objects_dir())?;
        fs::create_dir_all(store.tmp_dir())?;
        Ok(store)
    }

    /// The session-scoped store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn blob_path(&self, sha256: &str) -> io::Result<PathBuf> {
        validate_digest(sha256)?;
        // Two-hex-character shard keeps directory entries bounded; the digest
        // is validated as ASCII hex before slicing.
        Ok(self.objects_dir().join(&sha256[..2]).join(sha256))
    }

    /// Store `bytes` and return their reference.
    ///
    /// Encoding is content-derived, so a repeated `put` is a no-op: text of at
    /// least [`ZSTD_THRESHOLD_BYTES`] is zstd-compressed at level 3, everything
    /// else (shorter text, binary) is stored raw. The blob file is fsync'd
    /// before publication and the shard directory is fsync'd afterwards.
    pub fn put_bytes(&self, bytes: &[u8], mode: Option<u32>) -> io::Result<BlobRef> {
        let size = bytes.len() as u64;
        let sha256 = sha256_hex(bytes);
        let encoding = if size >= ZSTD_THRESHOLD_BYTES && std::str::from_utf8(bytes).is_ok() {
            BlobEncoding::Zstd
        } else {
            BlobEncoding::Raw
        };
        let blob = BlobRef {
            sha256: sha256.clone(),
            size,
            mode,
            encoding,
        };
        let target = self.blob_path(&sha256)?;
        if fs::symlink_metadata(&target).is_ok() {
            return Ok(blob);
        }
        let payload = match encoding {
            BlobEncoding::Raw => bytes.to_vec(),
            BlobEncoding::Zstd => zstd::stream::encode_all(bytes, ZSTD_LEVEL)?,
        };
        let shard = target
            .parent()
            .expect("blob path always has a shard directory")
            .to_path_buf();
        fs::create_dir_all(&shard)?;
        fs::create_dir_all(self.tmp_dir())?;
        write_synced(&self.tmp_dir(), &target, &payload)?;
        sync_dir(&shard)?;
        sync_dir(&self.objects_dir())?;
        Ok(blob)
    }

    /// Snapshot a workspace file's pre-image.
    ///
    /// `symlink_metadata` decides what the path is: a regular file is read with
    /// a [`MAX_SNAPSHOT_FILE_BYTES`] bound and stored via [`Self::put_bytes`],
    /// a missing path becomes [`PreImage::Missing`] (tombstone), and a symlink,
    /// directory or special file is an error.
    pub fn put_pre_image(&self, abs: &Path) -> io::Result<PreImage> {
        match read_regular_file(abs, MAX_SNAPSHOT_FILE_BYTES)? {
            BoundedFile::Missing => Ok(PreImage::Missing),
            BoundedFile::Regular { bytes, mode } => {
                Ok(PreImage::Blob(self.put_bytes(&bytes, mode)?))
            }
            BoundedFile::TooLarge { observed_bytes } => Err(RewindError::TooLarge {
                path: abs.to_path_buf(),
                observed_bytes,
                limit: MAX_SNAPSHOT_FILE_BYTES,
            }
            .into()),
            BoundedFile::NotRegular => Err(RewindError::NotRegular {
                path: abs.to_path_buf(),
            }
            .into()),
        }
    }

    /// Decode a blob after verifying its length and sha256.
    ///
    /// A missing, truncated, corrupt or digest-mismatched blob is reported as
    /// [`RewindError::CorruptBlob`] / [`RewindError::SizeMismatch`] /
    /// [`RewindError::DigestMismatch`]; callers skip such a file rather than
    /// writing bad content.
    pub fn read_blob(&self, blob: &BlobRef) -> io::Result<Vec<u8>> {
        validate_digest(&blob.sha256)?;
        let path = self.blob_path(&blob.sha256)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(corrupt(&blob.sha256, "blob file is missing"));
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() {
            return Err(corrupt(&blob.sha256, "blob path is not a regular file"));
        }
        let file = fs::File::open(&path)?;
        let mut payload = Vec::new();
        file.take(blob.size.saturating_add(ENCODED_READ_ALLOWANCE))
            .read_to_end(&mut payload)?;

        let decoded = match blob.encoding {
            BlobEncoding::Raw => {
                if payload.len() as u64 != blob.size {
                    return Err(RewindError::SizeMismatch {
                        sha256: blob.sha256.clone(),
                        expected: blob.size,
                        actual: payload.len() as u64,
                    }
                    .into());
                }
                payload
            }
            BlobEncoding::Zstd => {
                let decoder = zstd::stream::read::Decoder::new(payload.as_slice())
                    .map_err(|error| corrupt(&blob.sha256, format!("zstd frame: {error}")))?;
                let mut decoded = Vec::new();
                decoder
                    .take(blob.size.saturating_add(1))
                    .read_to_end(&mut decoded)
                    .map_err(|error| corrupt(&blob.sha256, format!("zstd decode: {error}")))?;
                if decoded.len() as u64 != blob.size {
                    return Err(RewindError::SizeMismatch {
                        sha256: blob.sha256.clone(),
                        expected: blob.size,
                        actual: decoded.len() as u64,
                    }
                    .into());
                }
                decoded
            }
        };

        let actual = sha256_hex(&decoded);
        if actual != blob.sha256 {
            return Err(RewindError::DigestMismatch {
                expected: blob.sha256.clone(),
                actual,
            }
            .into());
        }
        Ok(decoded)
    }

    /// Delete every blob under `objects/` whose sha256 is not in `live_sha256`.
    ///
    /// Deletion is unconditional (the caller owns retention timing). Symlinks
    /// and directories encountered under `objects/` are left alone; stray
    /// regular files directly under `objects/` are collected as well.
    pub fn gc(&self, live_sha256: &BTreeSet<String>) -> io::Result<GcReport> {
        let mut report = GcReport::default();
        let entries = match fs::read_dir(self.objects_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_dir() {
                for child in fs::read_dir(&path)? {
                    let child = child?;
                    let child_path = child.path();
                    let child_metadata = fs::symlink_metadata(&child_path)?;
                    if !child_metadata.file_type().is_file() {
                        continue;
                    }
                    let Some(name) = child_path.file_name().and_then(|name| name.to_str()) else {
                        continue;
                    };
                    if live_sha256.contains(name) {
                        continue;
                    }
                    remove_counted(&child_path, child_metadata.len(), &mut report)?;
                }
            } else if metadata.file_type().is_file() {
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if live_sha256.contains(name) {
                    continue;
                }
                remove_counted(&path, metadata.len(), &mut report)?;
            }
        }
        Ok(report)
    }
}

fn remove_counted(path: &Path, bytes: u64, report: &mut GcReport) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            report.removed_blobs += 1;
            report.removed_bytes = report.removed_bytes.saturating_add(bytes);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn corrupt(sha256: &str, detail: impl Into<String>) -> io::Error {
    RewindError::CorruptBlob {
        sha256: sha256.to_string(),
        detail: detail.into(),
    }
    .into()
}

/// Publish `bytes` at `target` via a temp file in `scratch_dir` + rename.
fn write_synced(scratch_dir: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(scratch_dir)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(target).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_store() -> (tempfile::TempDir, RewindStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RewindStore::open(&temp.path().join("sessions/s1")).expect("store");
        (temp, store)
    }

    fn zstd_magic(bytes: &[u8]) -> bool {
        bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
    }

    #[test]
    fn open_creates_objects_and_tmp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_root = temp.path().join("sessions/s1");
        let store = RewindStore::open(&session_root).expect("store");
        assert!(store.root().join("objects").is_dir());
        assert!(store.root().join("tmp").is_dir());
        assert_eq!(store.root(), session_root);
    }

    #[test]
    fn round_trips_small_text_as_raw() {
        let (_temp, store) = open_store();
        let blob = store.put_bytes(b"hello", Some(0o644)).expect("put");
        assert_eq!(blob.encoding, BlobEncoding::Raw);
        assert_eq!(blob.size, 5);
        assert_eq!(blob.mode, Some(0o644));
        assert_eq!(
            blob.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(store.read_blob(&blob).expect("read"), b"hello");
        let stored = fs::read(store.blob_path(&blob.sha256).unwrap()).expect("blob file");
        assert_eq!(stored, b"hello");
    }

    #[test]
    fn round_trips_large_text_through_zstd() {
        let (_temp, store) = open_store();
        let text = "fn main() { println!(\"rewind\"); }\n".repeat(512);
        assert!(text.len() as u64 > ZSTD_THRESHOLD_BYTES);
        let blob = store.put_bytes(text.as_bytes(), None).expect("put");
        assert_eq!(blob.encoding, BlobEncoding::Zstd);
        assert_eq!(blob.size, text.len() as u64);
        assert_eq!(store.read_blob(&blob).expect("read"), text.as_bytes());
        let stored = fs::read(store.blob_path(&blob.sha256).unwrap()).expect("blob file");
        assert!(zstd_magic(&stored), "large text should be a zstd frame");
        assert!(
            stored.len() < text.len(),
            "zstd should shrink repetitive text"
        );
    }

    #[test]
    fn round_trips_binary_as_raw() {
        let (_temp, store) = open_store();
        let binary: Vec<u8> = (0..=255u8).cycle().take(8192).collect();
        assert!(std::str::from_utf8(&binary).is_err());
        let blob = store.put_bytes(&binary, None).expect("put");
        assert_eq!(blob.encoding, BlobEncoding::Raw);
        assert_eq!(store.read_blob(&blob).expect("read"), binary);
    }

    #[test]
    fn dedupes_identical_content_and_keeps_requested_mode() {
        let (_temp, store) = open_store();
        let first = store.put_bytes(b"same bytes", Some(0o644)).expect("put");
        let second = store.put_bytes(b"same bytes", Some(0o755)).expect("put");
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(second.mode, Some(0o755));
        let mut blobs = 0;
        for shard in fs::read_dir(store.objects_dir()).expect("objects") {
            let shard = shard.expect("entry").path();
            blobs += fs::read_dir(shard).expect("shard").count();
        }
        assert_eq!(blobs, 1, "identical content must be stored once");
    }

    #[test]
    fn pre_image_captures_content_and_mode() {
        let (_temp, store) = open_store();
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("script.sh");
        fs::write(&file, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("chmod");
            let PreImage::Blob(blob) = store.put_pre_image(&file).expect("capture") else {
                panic!("expected blob");
            };
            assert_eq!(blob.mode, Some(0o755));
            assert_eq!(store.read_blob(&blob).expect("read"), b"#!/bin/sh\n");
        }
        #[cfg(not(unix))]
        {
            let PreImage::Blob(blob) = store.put_pre_image(&file).expect("capture") else {
                panic!("expected blob");
            };
            assert_eq!(blob.mode, None);
        }
    }

    #[test]
    fn missing_file_is_a_tombstone() {
        let (_temp, store) = open_store();
        let dir = tempfile::tempdir().expect("dir");
        assert_eq!(
            store
                .put_pre_image(&dir.path().join("absent.txt"))
                .expect("capture"),
            PreImage::Missing
        );
    }

    #[test]
    fn oversized_pre_image_is_refused() {
        let (_temp, store) = open_store();
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("huge.bin");
        let handle = fs::File::create(&file).expect("create");
        handle
            .set_len(MAX_SNAPSHOT_FILE_BYTES + 1)
            .expect("truncate");
        drop(handle);
        let error = store.put_pre_image(&file).expect_err("too large");
        let cause = crate::rewind_error(&error).expect("structured cause");
        assert!(cause.is_too_large(), "{cause}");
        assert!(error.to_string().contains("too large"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_directory_pre_images_are_refused() {
        use std::os::unix::fs::symlink;

        let (_temp, store) = open_store();
        let dir = tempfile::tempdir().expect("dir");
        fs::write(dir.path().join("real.txt"), b"x").expect("write");
        symlink(dir.path().join("real.txt"), dir.path().join("link.txt")).expect("symlink");

        let error = store
            .put_pre_image(&dir.path().join("link.txt"))
            .expect_err("symlink");
        assert!(matches!(
            crate::rewind_error(&error),
            Some(RewindError::NotRegular { .. })
        ));
        let error = store.put_pre_image(dir.path()).expect_err("directory");
        assert!(matches!(
            crate::rewind_error(&error),
            Some(RewindError::NotRegular { .. })
        ));
    }

    #[test]
    fn read_blob_detects_tampering() {
        let (_temp, store) = open_store();
        // Same-length replacement: only the digest can catch it.
        let blob = store.put_bytes(b"trust me", None).expect("put");
        let path = store.blob_path(&blob.sha256).unwrap();
        fs::write(&path, b"tampered").expect("tamper");
        let error = store.read_blob(&blob).expect_err("must detect tampering");
        assert!(matches!(
            crate::rewind_error(&error),
            Some(RewindError::DigestMismatch { .. })
        ));

        // Truncation is caught by the length check before hashing.
        let other = store.put_bytes(b"hello world", None).expect("put");
        fs::write(store.blob_path(&other.sha256).unwrap(), b"ell").expect("truncate");
        let error = store.read_blob(&other).expect_err("size mismatch");
        assert!(matches!(
            crate::rewind_error(&error),
            Some(RewindError::SizeMismatch { .. })
        ));
    }

    #[test]
    fn read_blob_rejects_missing_and_traversal_digests() {
        let (_temp, store) = open_store();
        let missing = BlobRef {
            sha256: "0".repeat(64),
            size: 1,
            mode: None,
            encoding: BlobEncoding::Raw,
        };
        let error = store.read_blob(&missing).expect_err("missing blob");
        assert!(matches!(
            crate::rewind_error(&error),
            Some(RewindError::CorruptBlob { .. })
        ));

        let traversal = BlobRef {
            sha256: "../../etc/passwd".to_string(),
            size: 1,
            mode: None,
            encoding: BlobEncoding::Raw,
        };
        let error = store.read_blob(&traversal).expect_err("invalid digest");
        assert!(matches!(
            crate::rewind_error(&error),
            Some(RewindError::InvalidDigest { .. })
        ));
    }

    #[test]
    fn gc_removes_unreferenced_blobs_only() {
        let (_temp, store) = open_store();
        let live = store.put_bytes(b"live", None).expect("put live");
        let dead = store.put_bytes(b"dead bytes", None).expect("put dead");
        let dead_len = fs::metadata(store.blob_path(&dead.sha256).unwrap())
            .expect("metadata")
            .len();

        let mut live_set = BTreeSet::new();
        live_set.insert(live.sha256.clone());
        let report = store.gc(&live_set).expect("gc");
        assert_eq!(report.removed_blobs, 1);
        assert_eq!(report.removed_bytes, dead_len);
        assert!(store.read_blob(&dead).is_err());
        assert_eq!(store.read_blob(&live).expect("live"), b"live");

        let report = store.gc(&live_set).expect("gc again");
        assert_eq!(report.removed_blobs, 0);
    }
}
