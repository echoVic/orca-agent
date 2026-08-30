//! Durable, lease-fenced delivery journal for detached subagent activity.
//!
//! The relay is intentionally a delivery journal rather than a second task
//! state machine.  Records are append-only, ordered per attempt, and safe to
//! replay after a process restart.  The runtime surface remains responsible
//! for deciding which records become user-visible state.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use orca_core::task_types::TaskType;
use orca_platform::fs::{AtomicWritePolicy, ExclusiveFileLock, atomic_write, open_nofollow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::surface::{Sha256Digest, SurfaceCommitId};

/// Maximum encoded record payload, excluding the four-byte length and checksum.
pub const MAX_RELAY_RECORD_BYTES: usize = 64 * 1024;
/// Maximum encoded bytes retained by one attempt relay.
pub const MAX_RELAY_ATTEMPT_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum records returned by one relay page.
pub const MAX_RELAY_PAGE_RECORDS: usize = 256;
/// Maximum encoded bytes returned by one relay page.
pub const MAX_RELAY_PAGE_BYTES: usize = 1024 * 1024;
const RELAY_SCHEMA_VERSION: u16 = 1;
const CHECKSUM_BYTES: usize = 32;
const FRAME_PREFIX_BYTES: usize = std::mem::size_of::<u32>();
const MAX_LEASE_MARKER_BYTES: usize = 8 * 1024;
const MAX_QUARANTINE_MARKER_BYTES: usize = 8 * 1024;
const RELAY_DIRECTORY: &str = "subagent-relay";

/// Task kinds whose activity may be written to a relay.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayTaskType {
    MainSession,
    Workflow,
    Subagent,
    Shell,
    Monitor,
}

impl From<TaskType> for RelayTaskType {
    fn from(value: TaskType) -> Self {
        match value {
            TaskType::MainSession => Self::MainSession,
            TaskType::Workflow => Self::Workflow,
            TaskType::Subagent => Self::Subagent,
            TaskType::Shell => Self::Shell,
            TaskType::Monitor => Self::Monitor,
        }
    }
}

/// The public part of a task execution lease used to fence relay writes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RelayLease {
    task_id: String,
    task_type: RelayTaskType,
    owner_id: String,
    lease_epoch: u64,
    attempt_id: String,
}

impl RelayLease {
    /// Constructs a validated relay lease.  Epoch zero is reserved for an
    /// unleased task and cannot authorize a write.
    pub fn new(
        task_id: impl Into<String>,
        task_type: RelayTaskType,
        owner_id: impl Into<String>,
        lease_epoch: u64,
        attempt_id: impl Into<String>,
    ) -> Result<Self, RelayError> {
        let lease = Self {
            task_id: task_id.into(),
            task_type,
            owner_id: owner_id.into(),
            lease_epoch,
            attempt_id: attempt_id.into(),
        };
        lease.validate()?;
        Ok(lease)
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn task_type(&self) -> RelayTaskType {
        self.task_type
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    fn validate(&self) -> Result<(), RelayError> {
        validate_identity(&self.task_id, "task id")?;
        validate_owner_id(&self.owner_id)?;
        validate_identity(&self.attempt_id, "attempt id")?;
        if self.lease_epoch == 0 {
            return Err(RelayError::InvalidLease(
                "lease epoch must be non-zero".into(),
            ));
        }
        Ok(())
    }
}

/// One immutable activity envelope in the delivery journal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RelayRecord {
    pub schema_version: u16,
    pub task_id: String,
    pub task_type: RelayTaskType,
    pub owner_id: String,
    pub lease_epoch: u64,
    pub attempt_id: String,
    pub source_sequence: u64,
    pub surface_commit_id: SurfaceCommitId,
    pub payload: Vec<u8>,
    pub digest: Sha256Digest,
}

impl RelayRecord {
    /// Builds a record and computes its semantic digest.
    pub fn new(
        lease: &RelayLease,
        source_sequence: u64,
        surface_commit_id: SurfaceCommitId,
        payload: Vec<u8>,
    ) -> Self {
        let mut record = Self {
            schema_version: RELAY_SCHEMA_VERSION,
            task_id: lease.task_id.clone(),
            task_type: lease.task_type,
            owner_id: lease.owner_id.clone(),
            lease_epoch: lease.lease_epoch,
            attempt_id: lease.attempt_id.clone(),
            source_sequence,
            surface_commit_id,
            payload,
            digest: Sha256Digest::new([0; 32]),
        };
        record.digest = record.compute_digest();
        record
    }

    /// Returns the canonical semantic digest for this record.
    pub fn compute_digest(&self) -> Sha256Digest {
        let mut hasher = Sha256::new();
        hasher.update(b"orca.subagent-relay.record.v1\0");
        put_u16(&mut hasher, self.schema_version);
        put_string(&mut hasher, &self.task_id);
        hasher.update([task_type_tag(self.task_type)]);
        put_string(&mut hasher, &self.owner_id);
        put_u64(&mut hasher, self.lease_epoch);
        put_string(&mut hasher, &self.attempt_id);
        put_u64(&mut hasher, self.source_sequence);
        hasher.update(self.surface_commit_id.as_bytes());
        put_bytes(&mut hasher, &self.payload);
        Sha256Digest::new(hasher.finalize().into())
    }

    /// Validates identity, sequence and semantic digest fields.
    pub fn validate(&self, lease: &RelayLease) -> Result<(), RelayError> {
        lease.validate()?;
        if self.schema_version != RELAY_SCHEMA_VERSION {
            return Err(RelayError::UnsupportedSchema(self.schema_version));
        }
        if self.task_id != lease.task_id
            || self.task_type != lease.task_type
            || self.attempt_id != lease.attempt_id
        {
            return Err(RelayError::CrossTaskAccess);
        }
        if self.owner_id != lease.owner_id || self.lease_epoch != lease.lease_epoch {
            return Err(RelayError::StaleLease {
                expected_epoch: lease.lease_epoch,
                observed_epoch: self.lease_epoch,
            });
        }
        if self.source_sequence == 0 {
            return Err(RelayError::InvalidRecord(
                "source sequence must be non-zero".into(),
            ));
        }
        if self.digest != self.compute_digest() {
            return Err(RelayError::DigestMismatch {
                source_sequence: self.source_sequence,
            });
        }
        if self.encoded_payload_len()? > MAX_RELAY_RECORD_BYTES {
            return Err(RelayError::RecordTooLarge {
                observed: self.encoded_payload_len()?,
                maximum: MAX_RELAY_RECORD_BYTES,
            });
        }
        Ok(())
    }

    fn encoded_payload_len(&self) -> Result<usize, RelayError> {
        serde_json::to_vec(self)
            .map(|bytes| bytes.len())
            .map_err(|error| RelayError::Serialization(error.to_string()))
    }

    fn equivalent(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.task_id == other.task_id
            && self.task_type == other.task_type
            && self.owner_id == other.owner_id
            && self.lease_epoch == other.lease_epoch
            && self.attempt_id == other.attempt_id
            && self.source_sequence == other.source_sequence
            && self.surface_commit_id == other.surface_commit_id
            && self.payload == other.payload
            && self.digest == other.digest
    }
}

/// Result of appending one source event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppendResult {
    Appended {
        source_sequence: u64,
        digest: Sha256Digest,
    },
    AlreadyApplied {
        source_sequence: u64,
        digest: Sha256Digest,
    },
}

/// A bounded page read from the relay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayPage {
    pub records: Vec<RelayRecord>,
    pub next_source_sequence: Option<u64>,
    pub has_more: bool,
    pub encoded_bytes: usize,
}

/// Identity used by a parent actor to read a historical attempt without
/// claiming the worker's write lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayReadTarget {
    task_id: String,
    task_type: RelayTaskType,
    attempt_id: String,
}

impl RelayReadTarget {
    pub fn new(
        task_id: impl Into<String>,
        task_type: RelayTaskType,
        attempt_id: impl Into<String>,
    ) -> Result<Self, RelayError> {
        let target = Self {
            task_id: task_id.into(),
            task_type,
            attempt_id: attempt_id.into(),
        };
        target.validate()?;
        Ok(target)
    }

    fn validate(&self) -> Result<(), RelayError> {
        validate_identity(&self.task_id, "task id")?;
        validate_identity(&self.attempt_id, "attempt id")?;
        Ok(())
    }
}

/// Errors returned by relay admission, scanning and persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayError {
    InvalidIdentity(String),
    InvalidLease(String),
    UnsupportedSchema(u16),
    CrossTaskAccess,
    ContainmentViolation,
    StaleLease {
        expected_epoch: u64,
        observed_epoch: u64,
    },
    LeaseMarkerConflict,
    SequenceGap {
        expected: u64,
        observed: u64,
    },
    SequenceConflict {
        source_sequence: u64,
    },
    CommitConflict {
        surface_commit_id: SurfaceCommitId,
    },
    DigestMismatch {
        source_sequence: u64,
    },
    InvalidRecord(String),
    IncompleteTail {
        offset: u64,
    },
    Corrupt {
        offset: u64,
        reason: String,
    },
    Quarantined(String),
    RecordTooLarge {
        observed: usize,
        maximum: usize,
    },
    AttemptTooLarge {
        observed: u64,
        maximum: u64,
    },
    PageLimit,
    Serialization(String),
    Io(String),
}

impl fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(value) => write!(formatter, "invalid relay identity: {value}"),
            Self::InvalidLease(value) => write!(formatter, "invalid relay lease: {value}"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported relay schema version {version}")
            }
            Self::CrossTaskAccess => formatter.write_str("relay record targets another task"),
            Self::ContainmentViolation => {
                formatter.write_str("relay path escaped its task-session root")
            }
            Self::StaleLease {
                expected_epoch,
                observed_epoch,
            } => write!(
                formatter,
                "stale relay lease: expected epoch {expected_epoch}, observed {observed_epoch}"
            ),
            Self::LeaseMarkerConflict => formatter.write_str("relay lease marker conflicts"),
            Self::SequenceGap { expected, observed } => {
                write!(
                    formatter,
                    "relay sequence gap: expected {expected}, observed {observed}"
                )
            }
            Self::SequenceConflict { source_sequence } => {
                write!(formatter, "relay sequence {source_sequence} conflicts")
            }
            Self::CommitConflict { surface_commit_id } => write!(
                formatter,
                "relay commit {} conflicts with an existing record",
                display_commit_id(surface_commit_id)
            ),
            Self::DigestMismatch { source_sequence } => {
                write!(
                    formatter,
                    "relay digest mismatch at sequence {source_sequence}"
                )
            }
            Self::InvalidRecord(value) => write!(formatter, "invalid relay record: {value}"),
            Self::IncompleteTail { offset } => {
                write!(
                    formatter,
                    "relay has an incomplete final record at offset {offset}"
                )
            }
            Self::Corrupt { offset, reason } => {
                write!(formatter, "corrupt relay at offset {offset}: {reason}")
            }
            Self::Quarantined(reason) => write!(formatter, "relay is quarantined: {reason}"),
            Self::RecordTooLarge { observed, maximum } => write!(
                formatter,
                "relay record is {observed} bytes; maximum is {maximum}"
            ),
            Self::AttemptTooLarge { observed, maximum } => write!(
                formatter,
                "relay attempt is {observed} bytes; maximum is {maximum}"
            ),
            Self::PageLimit => formatter.write_str("relay page limit reached"),
            Self::Serialization(value) => write!(formatter, "relay serialization failed: {value}"),
            Self::Io(value) => write!(formatter, "relay I/O failed: {value}"),
        }
    }
}

impl std::error::Error for RelayError {}

/// One task/attempt relay.  Instances are cheap handles; every mutation
/// reacquires the OS lock and revalidates the persisted lease marker.
pub struct SubagentEventRelay {
    relay_path: PathBuf,
    lease_path: PathBuf,
    current_lease_path: PathBuf,
    quarantine_path: PathBuf,
    lock_path: PathBuf,
    lease: RelayLease,
}

/// Read-only relay handle.  It deliberately ignores the current task lease so
/// an actor can drain a prior attempt after a worker takeover.
pub struct SubagentEventRelayReader {
    relay_path: PathBuf,
    quarantine_path: PathBuf,
    lock_path: PathBuf,
    target: RelayReadTarget,
}

impl fmt::Debug for SubagentEventRelayReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubagentEventRelayReader")
            .field("task_id", &self.target.task_id)
            .field("attempt_id", &self.target.attempt_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SubagentEventRelay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubagentEventRelay")
            .field("task_id", &self.lease.task_id)
            .field("attempt_id", &self.lease.attempt_id)
            .field("lease_epoch", &self.lease.lease_epoch)
            .finish_non_exhaustive()
    }
}

impl SubagentEventRelay {
    /// Opens the relay below a validated task-session root.
    ///
    /// A higher epoch is treated as a lease takeover.  Only that newly fenced
    /// owner may truncate an incomplete EOF frame; complete invalid frames are
    /// quarantined and are never truncated.
    pub fn open(root: &Path, lease: RelayLease) -> Result<Self, RelayError> {
        lease.validate()?;
        let root = prepare_root(root)?;
        let task_dir = root.join(&lease.task_id);
        ensure_directory(&task_dir, &root)?;
        let relay_dir = task_dir.join(RELAY_DIRECTORY);
        ensure_directory(&relay_dir, &root)?;
        let relay_path = relay_dir.join(format!("attempt-{}.relay", lease.attempt_id));
        let lease_path = relay_dir.join(format!("attempt-{}.lease", lease.attempt_id));
        let current_lease_path = relay_dir.join("current.lease");
        let quarantine_path = relay_dir.join(format!("attempt-{}.quarantine", lease.attempt_id));
        let lock_path = relay_dir.join("task.lock");
        for path in [
            &relay_path,
            &lease_path,
            &current_lease_path,
            &quarantine_path,
            &lock_path,
        ] {
            reject_symlink(path)?;
            ensure_contained(&root, path)?;
        }

        let relay = Self {
            relay_path,
            lease_path,
            current_lease_path,
            quarantine_path,
            lock_path,
            lease: lease.clone(),
        };
        let _lock = relay.acquire_lock()?;
        relay.ensure_not_quarantined()?;

        let marker = relay.read_lease_marker()?;
        let current_marker = relay.read_current_lease_marker()?;
        if let Some(current_marker) = current_marker.as_ref() {
            relay.validate_task_marker(current_marker)?;
        }
        if let Some(marker) = marker.as_ref() {
            relay.validate_marker(marker)?;
        }
        let scan = match relay.scan_records() {
            Ok(scan) => scan,
            Err(error @ RelayError::Corrupt { .. })
            | Err(error @ RelayError::RecordTooLarge { .. })
            | Err(error @ RelayError::AttemptTooLarge { .. }) => {
                relay.quarantine(&error);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let (prior_epoch, prior_owner) = highest_observed_lease(
            current_marker.as_ref(),
            marker.as_ref(),
            scan.max_lease_epoch.zip(scan.latest_owner.as_deref()),
        )?;
        if lease.lease_epoch < prior_epoch
            || (lease.lease_epoch == prior_epoch
                && prior_owner.is_some_and(|owner| owner != lease.owner_id))
        {
            return Err(RelayError::StaleLease {
                expected_epoch: prior_epoch,
                observed_epoch: lease.lease_epoch,
            });
        }
        if marker.as_ref().is_some_and(|marker| {
            marker.lease_epoch == lease.lease_epoch && marker.attempt_id != lease.attempt_id
        }) || current_marker.as_ref().is_some_and(|marker| {
            marker.lease_epoch == lease.lease_epoch && marker.attempt_id != lease.attempt_id
        }) {
            return Err(RelayError::LeaseMarkerConflict);
        }

        let takeover = lease.lease_epoch > prior_epoch;
        if takeover && let Some(offset) = scan.incomplete_tail_offset {
            let mut file = relay.open_append_file()?;
            file.set_len(scan.valid_end).map_err(io_error)?;
            file.seek(SeekFrom::End(0)).map_err(io_error)?;
            file.sync_data().map_err(io_error)?;
            debug_assert_eq!(offset >= scan.valid_end, true);
        } else if let Some(offset) = scan.incomplete_tail_offset {
            // Keep the incomplete tail visible to the current owner.  A live
            // owner must not repair its own un-fenced write.
            let _ = offset;
        }

        let expected_marker = LeaseMarker::from(&lease);
        if marker.as_ref() != Some(&expected_marker) {
            relay.write_lease_marker()?;
        }
        if current_marker.as_ref() != Some(&expected_marker) {
            relay.write_current_lease_marker()?;
        }
        Ok(relay)
    }

    /// Returns the on-disk relay path for maintenance/testing.  It is never
    /// included in surface or user-facing payloads.
    pub fn path(&self) -> &Path {
        &self.relay_path
    }

    /// Returns whether a prior scan quarantined this relay.
    pub fn is_quarantined(&self) -> bool {
        self.quarantine_path.exists()
    }

    /// Appends one record after lease, sequence, digest and size validation.
    pub fn append(&self, record: RelayRecord) -> Result<AppendResult, RelayError> {
        let _lock = self.acquire_lock()?;
        self.ensure_not_quarantined()?;
        let marker = self
            .read_lease_marker()?
            .ok_or(RelayError::LeaseMarkerConflict)?;
        self.validate_current_marker(&marker)?;
        let current_marker = self
            .read_current_lease_marker()?
            .ok_or(RelayError::LeaseMarkerConflict)?;
        self.validate_task_marker(&current_marker)?;
        self.validate_current_task_marker(&current_marker)?;
        let scan = match self.scan_records() {
            Ok(scan) => scan,
            Err(error @ RelayError::Corrupt { .. })
            | Err(error @ RelayError::RecordTooLarge { .. })
            | Err(error @ RelayError::AttemptTooLarge { .. }) => {
                self.quarantine(&error);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if let Some(offset) = scan.incomplete_tail_offset {
            return Err(RelayError::IncompleteTail { offset });
        }
        record.validate(&self.lease)?;

        if let Some(existing) = scan
            .records
            .iter()
            .find(|existing| existing.source_sequence == record.source_sequence)
        {
            if existing.equivalent(&record) {
                return Ok(AppendResult::AlreadyApplied {
                    source_sequence: record.source_sequence,
                    digest: record.digest,
                });
            }
            return Err(RelayError::SequenceConflict {
                source_sequence: record.source_sequence,
            });
        }
        if scan
            .records
            .iter()
            .any(|existing| existing.surface_commit_id == record.surface_commit_id)
        {
            return Err(RelayError::CommitConflict {
                surface_commit_id: record.surface_commit_id.clone(),
            });
        }
        let expected = scan
            .records
            .last()
            .map_or(1, |record| record.source_sequence.saturating_add(1));
        if record.source_sequence != expected {
            return Err(RelayError::SequenceGap {
                expected,
                observed: record.source_sequence,
            });
        }
        let frame = encode_frame(&record)?;
        let observed =
            scan.valid_end
                .checked_add(frame.len() as u64)
                .ok_or(RelayError::AttemptTooLarge {
                    observed: u64::MAX,
                    maximum: MAX_RELAY_ATTEMPT_BYTES,
                })?;
        if observed > MAX_RELAY_ATTEMPT_BYTES {
            return Err(RelayError::AttemptTooLarge {
                observed,
                maximum: MAX_RELAY_ATTEMPT_BYTES,
            });
        }
        let mut file = self.open_append_file()?;
        file.seek(SeekFrom::End(0)).map_err(io_error)?;
        file.write_all(&frame).map_err(io_error)?;
        file.flush().map_err(io_error)?;
        file.sync_data().map_err(io_error)?;
        Ok(AppendResult::Appended {
            source_sequence: record.source_sequence,
            digest: record.digest,
        })
    }

    /// Reads records after `after_sequence`, bounded by page count and bytes.
    pub fn read_page(&self, after_sequence: u64) -> Result<RelayPage, RelayError> {
        let _lock = self.acquire_lock()?;
        self.ensure_not_quarantined()?;
        let marker = self
            .read_lease_marker()?
            .ok_or(RelayError::LeaseMarkerConflict)?;
        self.validate_current_marker(&marker)?;
        let current_marker = self
            .read_current_lease_marker()?
            .ok_or(RelayError::LeaseMarkerConflict)?;
        self.validate_task_marker(&current_marker)?;
        self.validate_current_task_marker(&current_marker)?;
        let scan = match self.scan_records() {
            Ok(scan) => scan,
            Err(error @ RelayError::Corrupt { .. })
            | Err(error @ RelayError::RecordTooLarge { .. })
            | Err(error @ RelayError::AttemptTooLarge { .. }) => {
                self.quarantine(&error);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        page_from_scan(scan, after_sequence)
    }

    fn acquire_lock(&self) -> Result<ExclusiveFileLock, RelayError> {
        acquire_no_follow_lock(&self.lock_path)
    }

    fn open_append_file(&self) -> Result<File, RelayError> {
        reject_symlink(&self.relay_path)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).append(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        options.open(&self.relay_path).map_err(io_error)
    }

    fn read_lease_marker(&self) -> Result<Option<LeaseMarker>, RelayError> {
        if !self.lease_path.exists() {
            return Ok(None);
        }
        reject_symlink(&self.lease_path)?;
        let file =
            open_nofollow(&self.lease_path).map_err(|error| RelayError::Io(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take((MAX_LEASE_MARKER_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > MAX_LEASE_MARKER_BYTES {
            return Err(RelayError::Corrupt {
                offset: 0,
                reason: "lease marker exceeds limit".into(),
            });
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| RelayError::Corrupt {
                offset: 0,
                reason: format!("invalid lease marker: {error}"),
            })
    }

    fn read_current_lease_marker(&self) -> Result<Option<LeaseMarker>, RelayError> {
        if !self.current_lease_path.exists() {
            return Ok(None);
        }
        reject_symlink(&self.current_lease_path)?;
        let file = open_nofollow(&self.current_lease_path)
            .map_err(|error| RelayError::Io(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take((MAX_LEASE_MARKER_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > MAX_LEASE_MARKER_BYTES {
            return Err(RelayError::Corrupt {
                offset: 0,
                reason: "current lease marker exceeds limit".into(),
            });
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| RelayError::Corrupt {
                offset: 0,
                reason: format!("invalid current lease marker: {error}"),
            })
    }

    fn write_lease_marker(&self) -> Result<(), RelayError> {
        let marker = LeaseMarker::from(&self.lease);
        let bytes = serde_json::to_vec(&marker)
            .map_err(|error| RelayError::Serialization(error.to_string()))?;
        atomic_write(&self.lease_path, &bytes, AtomicWritePolicy::NoFollow)
            .map_err(|error| RelayError::Io(error.to_string()))
    }

    fn write_current_lease_marker(&self) -> Result<(), RelayError> {
        let marker = LeaseMarker::from(&self.lease);
        let bytes = serde_json::to_vec(&marker)
            .map_err(|error| RelayError::Serialization(error.to_string()))?;
        atomic_write(
            &self.current_lease_path,
            &bytes,
            AtomicWritePolicy::NoFollow,
        )
        .map_err(|error| RelayError::Io(error.to_string()))
    }

    fn validate_marker(&self, marker: &LeaseMarker) -> Result<(), RelayError> {
        validate_marker_fields(marker)?;
        if marker.schema_version != RELAY_SCHEMA_VERSION {
            return Err(RelayError::UnsupportedSchema(marker.schema_version));
        }
        if marker.task_id != self.lease.task_id
            || marker.task_type != self.lease.task_type
            || marker.attempt_id != self.lease.attempt_id
        {
            return Err(RelayError::CrossTaskAccess);
        }
        Ok(())
    }

    fn validate_task_marker(&self, marker: &LeaseMarker) -> Result<(), RelayError> {
        validate_marker_fields(marker)?;
        if marker.schema_version != RELAY_SCHEMA_VERSION {
            return Err(RelayError::UnsupportedSchema(marker.schema_version));
        }
        if marker.task_id != self.lease.task_id || marker.task_type != self.lease.task_type {
            return Err(RelayError::CrossTaskAccess);
        }
        Ok(())
    }

    fn validate_current_task_marker(&self, marker: &LeaseMarker) -> Result<(), RelayError> {
        if marker.owner_id != self.lease.owner_id || marker.lease_epoch != self.lease.lease_epoch {
            return Err(RelayError::StaleLease {
                expected_epoch: marker.lease_epoch,
                observed_epoch: self.lease.lease_epoch,
            });
        }
        if marker.attempt_id != self.lease.attempt_id {
            return Err(RelayError::SequenceConflict { source_sequence: 0 });
        }
        Ok(())
    }

    fn validate_current_marker(&self, marker: &LeaseMarker) -> Result<(), RelayError> {
        self.validate_marker(marker)?;
        if marker.owner_id != self.lease.owner_id || marker.lease_epoch != self.lease.lease_epoch {
            return Err(RelayError::StaleLease {
                expected_epoch: marker.lease_epoch,
                observed_epoch: self.lease.lease_epoch,
            });
        }
        Ok(())
    }

    fn scan_records(&self) -> Result<ScanResult, RelayError> {
        if !self.relay_path.exists() {
            return Ok(ScanResult::default());
        }
        reject_symlink(&self.relay_path)?;
        let mut file =
            open_nofollow(&self.relay_path).map_err(|error| RelayError::Io(error.to_string()))?;
        let file_len = file.metadata().map_err(io_error)?.len();
        if file_len > MAX_RELAY_ATTEMPT_BYTES {
            return Err(RelayError::AttemptTooLarge {
                observed: file_len,
                maximum: MAX_RELAY_ATTEMPT_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(file_len as usize);
        Read::by_ref(&mut file)
            .take(MAX_RELAY_ATTEMPT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_RELAY_ATTEMPT_BYTES {
            return Err(RelayError::AttemptTooLarge {
                observed: bytes.len() as u64,
                maximum: MAX_RELAY_ATTEMPT_BYTES,
            });
        }
        scan_bytes(&bytes, &self.lease)
    }

    fn ensure_not_quarantined(&self) -> Result<(), RelayError> {
        if !self.quarantine_path.exists() {
            return Ok(());
        }
        reject_symlink(&self.quarantine_path)?;
        let file = open_nofollow(&self.quarantine_path)
            .map_err(|error| RelayError::Io(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take((MAX_QUARANTINE_MARKER_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > MAX_QUARANTINE_MARKER_BYTES {
            return Err(RelayError::Corrupt {
                offset: 0,
                reason: "quarantine marker exceeds limit".into(),
            });
        }
        let reason = String::from_utf8_lossy(&bytes).trim().to_string();
        Err(RelayError::Quarantined(if reason.is_empty() {
            "relay was quarantined".into()
        } else {
            reason
        }))
    }

    fn quarantine(&self, error: &RelayError) {
        if self.quarantine_path.exists() {
            return;
        }
        let _ = atomic_write(
            &self.quarantine_path,
            error.to_string().as_bytes(),
            AtomicWritePolicy::NoFollow,
        );
    }
}

fn validate_marker_fields(marker: &LeaseMarker) -> Result<(), RelayError> {
    validate_identity(&marker.task_id, "task id")?;
    validate_identity(&marker.attempt_id, "attempt id")?;
    validate_owner_id(&marker.owner_id)?;
    if marker.lease_epoch == 0 {
        return Err(RelayError::InvalidLease(
            "lease marker epoch must be non-zero".into(),
        ));
    }
    Ok(())
}

impl SubagentEventRelayReader {
    /// Opens a historical attempt for draining without acquiring its writer
    /// lease.  The reader never truncates or rewrites source relay bytes.
    pub fn open(root: &Path, target: RelayReadTarget) -> Result<Self, RelayError> {
        target.validate()?;
        let root = prepare_existing_root(root)?;
        let task_dir = root.join(&target.task_id);
        ensure_existing_directory(&task_dir, &root)?;
        let relay_dir = task_dir.join(RELAY_DIRECTORY);
        ensure_existing_directory(&relay_dir, &root)?;
        let relay_path = relay_dir.join(format!("attempt-{}.relay", target.attempt_id));
        let quarantine_path = relay_dir.join(format!("attempt-{}.quarantine", target.attempt_id));
        let lock_path = relay_dir.join("task.lock");
        for path in [&relay_path, &quarantine_path, &lock_path] {
            reject_symlink(path)?;
            ensure_contained(&root, path)?;
        }
        let reader = Self {
            relay_path,
            quarantine_path,
            lock_path,
            target,
        };
        reader.ensure_not_quarantined()?;
        Ok(reader)
    }

    pub fn path(&self) -> &Path {
        &self.relay_path
    }

    /// Permanently fences a reader after a frame passes transport checks but
    /// fails the typed activity schema. This prevents the actor from retrying
    /// the same poisonous payload on every idle tick.
    pub(crate) fn quarantine_corrupt(&self, error: &RelayError) {
        self.quarantine(error);
    }

    pub fn read_page(&self, after_sequence: u64) -> Result<RelayPage, RelayError> {
        // Writers hold this same lock across frame append and fsync. Without
        // taking it here, a reader can observe a valid prefix while the
        // checksum suffix is still being written and quarantine a healthy
        // relay permanently.
        let _lock = acquire_no_follow_lock(&self.lock_path)?;
        self.ensure_not_quarantined()?;
        let scan = match self.scan_records() {
            Ok(scan) => scan,
            Err(error @ RelayError::Corrupt { .. })
            | Err(error @ RelayError::RecordTooLarge { .. })
            | Err(error @ RelayError::AttemptTooLarge { .. }) => {
                self.quarantine(&error);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        page_from_scan(scan, after_sequence)
    }

    fn scan_records(&self) -> Result<ScanResult, RelayError> {
        if !self.relay_path.exists() {
            return Ok(ScanResult::default());
        }
        reject_symlink(&self.relay_path)?;
        let mut file =
            open_nofollow(&self.relay_path).map_err(|error| RelayError::Io(error.to_string()))?;
        let file_len = file.metadata().map_err(io_error)?.len();
        if file_len > MAX_RELAY_ATTEMPT_BYTES {
            return Err(RelayError::AttemptTooLarge {
                observed: file_len,
                maximum: MAX_RELAY_ATTEMPT_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(file_len as usize);
        Read::by_ref(&mut file)
            .take(MAX_RELAY_ATTEMPT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_RELAY_ATTEMPT_BYTES {
            return Err(RelayError::AttemptTooLarge {
                observed: bytes.len() as u64,
                maximum: MAX_RELAY_ATTEMPT_BYTES,
            });
        }
        let marker_lease = RelayLease::new(
            self.target.task_id.clone(),
            self.target.task_type,
            "reader",
            u64::MAX,
            self.target.attempt_id.clone(),
        )?;
        scan_bytes(&bytes, &marker_lease)
    }

    fn ensure_not_quarantined(&self) -> Result<(), RelayError> {
        if !self.quarantine_path.exists() {
            return Ok(());
        }
        reject_symlink(&self.quarantine_path)?;
        let file = open_nofollow(&self.quarantine_path)
            .map_err(|error| RelayError::Io(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take((MAX_QUARANTINE_MARKER_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > MAX_QUARANTINE_MARKER_BYTES {
            return Err(RelayError::Corrupt {
                offset: 0,
                reason: "quarantine marker exceeds limit".into(),
            });
        }
        let reason = String::from_utf8_lossy(&bytes).trim().to_string();
        Err(RelayError::Quarantined(if reason.is_empty() {
            "relay was quarantined".into()
        } else {
            reason
        }))
    }

    fn quarantine(&self, error: &RelayError) {
        if self.quarantine_path.exists() {
            return;
        }
        let _ = atomic_write(
            &self.quarantine_path,
            error.to_string().as_bytes(),
            AtomicWritePolicy::NoFollow,
        );
    }
}

#[derive(Clone, Debug, Default)]
struct ScanResult {
    records: Vec<RelayRecord>,
    valid_end: u64,
    incomplete_tail_offset: Option<u64>,
    max_lease_epoch: Option<u64>,
    latest_owner: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct LeaseMarker {
    schema_version: u16,
    task_id: String,
    task_type: RelayTaskType,
    owner_id: String,
    lease_epoch: u64,
    attempt_id: String,
}

impl From<&RelayLease> for LeaseMarker {
    fn from(lease: &RelayLease) -> Self {
        Self {
            schema_version: RELAY_SCHEMA_VERSION,
            task_id: lease.task_id.clone(),
            task_type: lease.task_type,
            owner_id: lease.owner_id.clone(),
            lease_epoch: lease.lease_epoch,
            attempt_id: lease.attempt_id.clone(),
        }
    }
}

fn highest_observed_lease<'a>(
    current_marker: Option<&'a LeaseMarker>,
    attempt_marker: Option<&'a LeaseMarker>,
    scanned: Option<(u64, &'a str)>,
) -> Result<(u64, Option<&'a str>), RelayError> {
    let mut highest_epoch = 0;
    let mut highest_owner = None;
    for (epoch, owner) in [
        current_marker.map(|marker| (marker.lease_epoch, marker.owner_id.as_str())),
        attempt_marker.map(|marker| (marker.lease_epoch, marker.owner_id.as_str())),
        scanned,
    ]
    .into_iter()
    .flatten()
    {
        if epoch > highest_epoch {
            highest_epoch = epoch;
            highest_owner = Some(owner);
        } else if epoch == highest_epoch
            && highest_owner.is_some_and(|previous_owner| previous_owner != owner)
        {
            return Err(RelayError::LeaseMarkerConflict);
        }
    }
    Ok((highest_epoch, highest_owner))
}

fn scan_bytes(bytes: &[u8], lease: &RelayLease) -> Result<ScanResult, RelayError> {
    let mut result = ScanResult::default();
    let mut offset = 0usize;
    let mut expected_sequence = 1u64;
    let mut commits = HashSet::<SurfaceCommitId>::new();
    let mut epochs = HashMap::<u64, String>::new();
    let mut previous_lease_epoch = None;
    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        if remaining < FRAME_PREFIX_BYTES {
            result.incomplete_tail_offset = Some(offset as u64);
            break;
        }
        let frame_len = u32::from_be_bytes(
            bytes[offset..offset + FRAME_PREFIX_BYTES]
                .try_into()
                .expect("length prefix has four bytes"),
        ) as usize;
        if frame_len == 0 || frame_len > MAX_RELAY_RECORD_BYTES {
            return Err(RelayError::RecordTooLarge {
                observed: frame_len,
                maximum: MAX_RELAY_RECORD_BYTES,
            });
        }
        let total = FRAME_PREFIX_BYTES
            .checked_add(frame_len)
            .and_then(|value| value.checked_add(CHECKSUM_BYTES))
            .ok_or(RelayError::RecordTooLarge {
                observed: usize::MAX,
                maximum: MAX_RELAY_RECORD_BYTES,
            })?;
        if remaining < total {
            result.incomplete_tail_offset = Some(offset as u64);
            break;
        }
        let payload_start = offset + FRAME_PREFIX_BYTES;
        let payload_end = payload_start + frame_len;
        let payload = &bytes[payload_start..payload_end];
        let checksum = &bytes[payload_end..payload_end + CHECKSUM_BYTES];
        let expected_checksum: [u8; 32] = Sha256::digest(payload).into();
        if checksum != expected_checksum {
            return Err(RelayError::Corrupt {
                offset: offset as u64,
                reason: "frame checksum mismatch".into(),
            });
        }
        let record: RelayRecord =
            serde_json::from_slice(payload).map_err(|error| RelayError::Corrupt {
                offset: offset as u64,
                reason: format!("invalid record payload: {error}"),
            })?;
        validate_scanned_record(
            &record,
            lease,
            expected_sequence,
            &mut commits,
            &mut epochs,
            &mut previous_lease_epoch,
        )
        .map_err(|error| RelayError::Corrupt {
            offset: offset as u64,
            reason: error.to_string(),
        })?;
        result.max_lease_epoch = Some(
            result
                .max_lease_epoch
                .map_or(record.lease_epoch, |epoch| epoch.max(record.lease_epoch)),
        );
        if result
            .max_lease_epoch
            .is_some_and(|epoch| epoch == record.lease_epoch)
        {
            result.latest_owner = Some(record.owner_id.clone());
        }
        result.records.push(record);
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or(RelayError::SequenceGap {
                expected: u64::MAX,
                observed: u64::MAX,
            })?;
        offset += total;
        result.valid_end = offset as u64;
    }
    Ok(result)
}

fn page_from_scan(scan: ScanResult, after_sequence: u64) -> Result<RelayPage, RelayError> {
    let mut records = Vec::new();
    let mut encoded_bytes = 0usize;
    let mut has_more = false;
    for record in scan
        .records
        .into_iter()
        .filter(|record| record.source_sequence > after_sequence)
    {
        let size = frame_size(&record)?;
        if records.len() == MAX_RELAY_PAGE_RECORDS
            || (encoded_bytes > 0 && encoded_bytes.saturating_add(size) > MAX_RELAY_PAGE_BYTES)
        {
            has_more = true;
            break;
        }
        if size > MAX_RELAY_PAGE_BYTES {
            return Err(RelayError::PageLimit);
        }
        encoded_bytes = encoded_bytes.saturating_add(size);
        records.push(record);
    }
    Ok(RelayPage {
        next_source_sequence: records.last().map(|record| record.source_sequence),
        records,
        has_more,
        encoded_bytes,
    })
}

fn validate_scanned_record(
    record: &RelayRecord,
    lease: &RelayLease,
    expected_sequence: u64,
    commits: &mut HashSet<SurfaceCommitId>,
    epochs: &mut HashMap<u64, String>,
    previous_lease_epoch: &mut Option<u64>,
) -> Result<(), RelayError> {
    if record.schema_version != RELAY_SCHEMA_VERSION {
        return Err(RelayError::UnsupportedSchema(record.schema_version));
    }
    if record.task_id != lease.task_id
        || record.task_type != lease.task_type
        || record.attempt_id != lease.attempt_id
    {
        return Err(RelayError::CrossTaskAccess);
    }
    if record.lease_epoch == 0 || record.lease_epoch > lease.lease_epoch {
        return Err(RelayError::StaleLease {
            expected_epoch: lease.lease_epoch,
            observed_epoch: record.lease_epoch,
        });
    }
    if let Some(previous_epoch) = previous_lease_epoch
        && record.lease_epoch < *previous_epoch
    {
        return Err(RelayError::StaleLease {
            expected_epoch: *previous_epoch,
            observed_epoch: record.lease_epoch,
        });
    }
    *previous_lease_epoch = Some(record.lease_epoch);
    validate_owner_id(&record.owner_id)?;
    if let Some(previous_owner) = epochs.get(&record.lease_epoch)
        && previous_owner != &record.owner_id
    {
        return Err(RelayError::LeaseMarkerConflict);
    }
    epochs.insert(record.lease_epoch, record.owner_id.clone());
    if record.source_sequence != expected_sequence {
        return Err(RelayError::SequenceGap {
            expected: expected_sequence,
            observed: record.source_sequence,
        });
    }
    if !commits.insert(record.surface_commit_id.clone()) {
        return Err(RelayError::CommitConflict {
            surface_commit_id: record.surface_commit_id.clone(),
        });
    }
    if record.digest != record.compute_digest() {
        return Err(RelayError::DigestMismatch {
            source_sequence: record.source_sequence,
        });
    }
    Ok(())
}

fn encode_frame(record: &RelayRecord) -> Result<Vec<u8>, RelayError> {
    let payload =
        serde_json::to_vec(record).map_err(|error| RelayError::Serialization(error.to_string()))?;
    if payload.len() > MAX_RELAY_RECORD_BYTES {
        return Err(RelayError::RecordTooLarge {
            observed: payload.len(),
            maximum: MAX_RELAY_RECORD_BYTES,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| RelayError::RecordTooLarge {
        observed: payload.len(),
        maximum: MAX_RELAY_RECORD_BYTES,
    })?;
    let mut frame = Vec::with_capacity(FRAME_PREFIX_BYTES + payload.len() + CHECKSUM_BYTES);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&Sha256::digest(&payload));
    Ok(frame)
}

fn frame_size(record: &RelayRecord) -> Result<usize, RelayError> {
    encode_frame(record).map(|frame| frame.len())
}

fn prepare_root(root: &Path) -> Result<PathBuf, RelayError> {
    reject_symlink(root)?;
    fs::create_dir_all(root).map_err(io_error)?;
    reject_symlink(root)?;
    fs::canonicalize(root).map_err(io_error)
}

fn prepare_existing_root(root: &Path) -> Result<PathBuf, RelayError> {
    reject_symlink(root)?;
    if !root.is_dir() {
        return Err(RelayError::Io(format!(
            "relay root does not exist: {}",
            root.display()
        )));
    }
    fs::canonicalize(root).map_err(io_error)
}

fn ensure_directory(path: &Path, root: &Path) -> Result<(), RelayError> {
    reject_symlink(path)?;
    fs::create_dir_all(path).map_err(io_error)?;
    reject_symlink(path)?;
    if !path.is_dir() {
        return Err(RelayError::InvalidIdentity(format!(
            "relay path is not a directory: {}",
            path.display()
        )));
    }
    ensure_contained(root, path)
}

fn ensure_existing_directory(path: &Path, root: &Path) -> Result<(), RelayError> {
    reject_symlink(path)?;
    if !path.is_dir() {
        return Err(RelayError::Io(format!(
            "relay directory does not exist: {}",
            path.display()
        )));
    }
    ensure_contained(root, path)
}

fn ensure_contained(root: &Path, path: &Path) -> Result<(), RelayError> {
    let parent = path.parent().unwrap_or(path);
    let canonical_parent = fs::canonicalize(parent).map_err(io_error)?;
    if !canonical_parent.starts_with(root) {
        return Err(RelayError::ContainmentViolation);
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), RelayError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                Err(RelayError::ContainmentViolation)
            } else {
                Ok(())
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn validate_identity(value: &str, label: &str) -> Result<(), RelayError> {
    if value.is_empty() || value.len() > 256 {
        return Err(RelayError::InvalidIdentity(format!(
            "{label} must contain 1..=256 bytes"
        )));
    }
    if value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(RelayError::InvalidIdentity(format!(
            "{label} contains path or control characters"
        )));
    }
    Ok(())
}

fn validate_owner_id(value: &str) -> Result<(), RelayError> {
    if value.is_empty() || value.len() > 1024 {
        return Err(RelayError::InvalidIdentity(
            "owner id must contain 1..=1024 bytes".into(),
        ));
    }
    if value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(RelayError::InvalidIdentity(
            "owner id contains control characters".into(),
        ));
    }
    Ok(())
}

fn acquire_no_follow_lock(path: &Path) -> Result<ExclusiveFileLock, RelayError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path).map_err(io_error)?;
    ExclusiveFileLock::acquire_file(path, file).map_err(|error| RelayError::Io(error.to_string()))
}

fn io_error(error: impl ToString) -> RelayError {
    RelayError::Io(error.to_string())
}

fn put_u16(hasher: &mut Sha256, value: u16) {
    hasher.update(value.to_be_bytes());
}

fn put_u64(hasher: &mut Sha256, value: u64) {
    hasher.update(value.to_be_bytes());
}

fn put_string(hasher: &mut Sha256, value: &str) {
    put_bytes(hasher, value.as_bytes());
}

fn put_bytes(hasher: &mut Sha256, value: &[u8]) {
    put_u64(hasher, value.len() as u64);
    hasher.update(value);
}

fn task_type_tag(task_type: RelayTaskType) -> u8 {
    match task_type {
        RelayTaskType::MainSession => 1,
        RelayTaskType::Workflow => 2,
        RelayTaskType::Subagent => 3,
        RelayTaskType::Shell => 4,
        RelayTaskType::Monitor => 5,
    }
}

fn display_commit_id(commit_id: &SurfaceCommitId) -> String {
    commit_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn lease(root: &Path, owner: &str, epoch: u64) -> (PathBuf, RelayLease) {
        let task_root = root.to_path_buf();
        let lease =
            RelayLease::new("task-1", RelayTaskType::Subagent, owner, epoch, "attempt-1").unwrap();
        (task_root, lease)
    }

    fn commit_id(seed: u16) -> SurfaceCommitId {
        let mut bytes = [0; 16];
        bytes[..2].copy_from_slice(&seed.to_be_bytes());
        bytes[6] = 0x70 | ((seed as u8) & 0x0f);
        bytes[8] = 0x80 | ((seed as u8) & 0x3f);
        SurfaceCommitId::try_from_bytes(bytes).unwrap()
    }

    #[test]
    fn duplicate_commit_is_idempotent_but_conflicting_digest_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (path, lease) = lease(root.path(), "owner-a", 1);
        let relay = SubagentEventRelay::open(&path, lease.clone()).unwrap();
        let record = RelayRecord::new(&lease, 1, commit_id(1), b"started".to_vec());
        assert!(matches!(
            relay.append(record.clone()),
            Ok(AppendResult::Appended { .. })
        ));
        assert!(matches!(
            relay.append(record),
            Ok(AppendResult::AlreadyApplied { .. })
        ));
        let commit_conflict = RelayRecord::new(&lease, 2, commit_id(1), b"reused".to_vec());
        assert!(matches!(
            relay.append(commit_conflict),
            Err(RelayError::CommitConflict { .. })
        ));
        let mut conflict = RelayRecord::new(&lease, 1, commit_id(1), b"changed".to_vec());
        conflict.digest = conflict.compute_digest();
        assert!(matches!(
            relay.append(conflict),
            Err(RelayError::SequenceConflict { source_sequence: 1 })
        ));
    }

    #[test]
    fn sequence_gaps_and_cross_task_records_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let (_, lease) = lease(root.path(), "owner-a", 1);
        let relay = SubagentEventRelay::open(root.path(), lease.clone()).unwrap();
        let gap = RelayRecord::new(&lease, 2, commit_id(2), b"progress".to_vec());
        assert!(matches!(
            relay.append(gap),
            Err(RelayError::SequenceGap {
                expected: 1,
                observed: 2
            })
        ));
        let other = RelayLease::new(
            "other-task",
            RelayTaskType::Subagent,
            "owner-a",
            1,
            "attempt-1",
        )
        .unwrap();
        let record = RelayRecord::new(&other, 1, commit_id(3), b"x".to_vec());
        assert!(matches!(
            relay.append(record),
            Err(RelayError::CrossTaskAccess)
        ));
    }

    #[test]
    fn stale_owner_is_fenced_after_higher_epoch_takeover() {
        let root = tempfile::tempdir().unwrap();
        let (_, first_lease) = lease(root.path(), "owner-a", 1);
        let first = SubagentEventRelay::open(root.path(), first_lease.clone()).unwrap();
        first
            .append(RelayRecord::new(
                &first_lease,
                1,
                commit_id(1),
                b"x".to_vec(),
            ))
            .unwrap();
        let (_, second_lease) = lease(root.path(), "owner-b", 2);
        let second = SubagentEventRelay::open(root.path(), second_lease.clone()).unwrap();
        second
            .append(RelayRecord::new(
                &second_lease,
                2,
                commit_id(2),
                b"y".to_vec(),
            ))
            .unwrap();
        assert!(matches!(
            first.append(RelayRecord::new(
                &first_lease,
                3,
                commit_id(3),
                b"z".to_vec()
            )),
            Err(RelayError::StaleLease { .. })
        ));
    }

    #[test]
    fn a_new_attempt_can_take_over_the_task_after_a_previous_attempt() {
        let root = tempfile::tempdir().unwrap();
        let (_, first_lease) = lease(root.path(), "owner-a", 1);
        let first = SubagentEventRelay::open(root.path(), first_lease.clone()).unwrap();
        first
            .append(RelayRecord::new(
                &first_lease,
                1,
                commit_id(1),
                b"started".to_vec(),
            ))
            .unwrap();

        let second_lease =
            RelayLease::new("task-1", RelayTaskType::Subagent, "owner-b", 2, "attempt-2").unwrap();
        let second = SubagentEventRelay::open(root.path(), second_lease.clone()).unwrap();
        second
            .append(RelayRecord::new(
                &second_lease,
                1,
                commit_id(2),
                b"restarted".to_vec(),
            ))
            .unwrap();
        assert_eq!(second.read_page(0).unwrap().records.len(), 1);
        assert!(matches!(
            first.read_page(0),
            Err(RelayError::StaleLease { .. })
        ));
    }

    #[test]
    fn same_epoch_cannot_replace_the_current_attempt() {
        let root = tempfile::tempdir().unwrap();
        let (_, first_lease) = lease(root.path(), "owner-a", 1);
        SubagentEventRelay::open(root.path(), first_lease).unwrap();
        let replacement =
            RelayLease::new("task-1", RelayTaskType::Subagent, "owner-a", 1, "attempt-2").unwrap();
        assert!(matches!(
            SubagentEventRelay::open(root.path(), replacement),
            Err(RelayError::LeaseMarkerConflict)
        ));
    }

    #[test]
    fn reader_drains_a_prior_attempt_after_a_new_writer_takes_over() {
        let root = tempfile::tempdir().unwrap();
        let (_, first_lease) = lease(root.path(), "owner-a", 1);
        let first = SubagentEventRelay::open(root.path(), first_lease.clone()).unwrap();
        first
            .append(RelayRecord::new(
                &first_lease,
                1,
                commit_id(1),
                b"started".to_vec(),
            ))
            .unwrap();

        let second_lease =
            RelayLease::new("task-1", RelayTaskType::Subagent, "owner-b", 2, "attempt-2").unwrap();
        SubagentEventRelay::open(root.path(), second_lease).unwrap();

        let reader = SubagentEventRelayReader::open(
            root.path(),
            RelayReadTarget::new("task-1", RelayTaskType::Subagent, "attempt-1").unwrap(),
        )
        .unwrap();
        let page = reader.read_page(0).unwrap();
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].source_sequence, 1);
    }

    #[test]
    fn reader_rejects_missing_task_without_creating_relay_paths() {
        let root = tempfile::tempdir().unwrap();
        let target = RelayReadTarget::new("task-1", RelayTaskType::Subagent, "attempt-1").unwrap();
        assert!(matches!(
            SubagentEventRelayReader::open(root.path(), target),
            Err(RelayError::Io(_))
        ));
        assert!(!root.path().join("task-1").exists());
    }

    #[test]
    fn relay_enforces_record_attempt_and_page_bounds() {
        let root = tempfile::tempdir().unwrap();
        let (_, lease_value) = lease(root.path(), "owner-a", 1);
        let relay = SubagentEventRelay::open(root.path(), lease_value.clone()).unwrap();
        let oversized = RelayRecord::new(
            &lease_value,
            1,
            commit_id(1),
            vec![b'x'; MAX_RELAY_RECORD_BYTES],
        );
        assert!(matches!(
            relay.append(oversized),
            Err(RelayError::RecordTooLarge { .. })
        ));

        for sequence in 1..=MAX_RELAY_PAGE_RECORDS as u64 + 1 {
            relay
                .append(RelayRecord::new(
                    &lease_value,
                    sequence,
                    commit_id(sequence as u16),
                    b"x".to_vec(),
                ))
                .unwrap();
        }
        let page = relay.read_page(0).unwrap();
        assert_eq!(page.records.len(), MAX_RELAY_PAGE_RECORDS);
        assert!(page.has_more);
        let tail = relay
            .read_page(page.next_source_sequence.expect("page cursor"))
            .unwrap();
        assert_eq!(tail.records.len(), 1);

        let limited_root = tempfile::tempdir().unwrap();
        let (_, limited_lease) = lease(limited_root.path(), "owner-a", 1);
        let limited = SubagentEventRelay::open(limited_root.path(), limited_lease.clone()).unwrap();
        let path = limited.path().to_path_buf();
        File::create(&path)
            .unwrap()
            .set_len(MAX_RELAY_ATTEMPT_BYTES + 1)
            .unwrap();
        assert!(matches!(
            limited.read_page(0),
            Err(RelayError::AttemptTooLarge { .. })
        ));
        assert!(limited.is_quarantined());
    }

    #[test]
    fn page_stops_before_its_encoded_byte_limit() {
        let root = tempfile::tempdir().unwrap();
        let (_, lease) = lease(root.path(), "owner-a", 1);
        let relay = SubagentEventRelay::open(root.path(), lease.clone()).unwrap();
        for sequence in 1..=20 {
            relay
                .append(RelayRecord::new(
                    &lease,
                    sequence,
                    commit_id(sequence as u16),
                    vec![b'x'; 14_000],
                ))
                .unwrap();
        }
        let page = relay.read_page(0).unwrap();
        assert!(page.encoded_bytes <= MAX_RELAY_PAGE_BYTES);
        assert!(page.records.len() < 20);
        assert!(page.has_more);
    }

    #[test]
    fn concurrent_appends_are_serialized_and_ordered() {
        let root = tempfile::tempdir().unwrap();
        let (_, lease) = lease(root.path(), "owner-a", 1);
        let relay =
            std::sync::Arc::new(SubagentEventRelay::open(root.path(), lease.clone()).unwrap());
        let first = relay.clone();
        let second = relay.clone();
        let lease1 = lease.clone();
        let a = thread::spawn(move || {
            first.append(RelayRecord::new(&lease1, 1, commit_id(1), b"a".to_vec()))
        });
        let lease2 =
            RelayLease::new("task-1", RelayTaskType::Subagent, "owner-a", 1, "attempt-1").unwrap();
        let lease2_for_thread = lease2.clone();
        let b = thread::spawn(move || {
            second.append(RelayRecord::new(
                &lease2_for_thread,
                2,
                commit_id(2),
                b"b".to_vec(),
            ))
        });
        let _ = a.join().unwrap();
        if matches!(
            b.join().unwrap(),
            Err(RelayError::SequenceGap {
                expected: 1,
                observed: 2
            })
        ) {
            relay
                .append(RelayRecord::new(&lease2, 2, commit_id(2), b"b".to_vec()))
                .unwrap();
        }
        let page = relay.read_page(0).unwrap();
        assert_eq!(page.records.len(), 2);
        assert_eq!(page.records[0].source_sequence, 1);
        assert_eq!(page.records[1].source_sequence, 2);
    }

    #[test]
    fn complete_corruption_is_quarantined_without_truncation() {
        let root = tempfile::tempdir().unwrap();
        let (_, lease) = lease(root.path(), "owner-a", 1);
        let relay = SubagentEventRelay::open(root.path(), lease.clone()).unwrap();
        relay
            .append(RelayRecord::new(&lease, 1, commit_id(1), b"x".to_vec()))
            .unwrap();
        let path = relay.path().to_path_buf();
        let before = fs::read(&path).unwrap();
        let mut bytes = before.clone();
        *bytes.last_mut().unwrap() ^= 0xFF;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            relay.read_page(0),
            Err(RelayError::Corrupt { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(relay.is_quarantined());
    }

    #[test]
    fn corrupt_middle_record_is_quarantined_without_rewriting_source() {
        let root = tempfile::tempdir().unwrap();
        let (_, lease) = lease(root.path(), "owner-a", 1);
        let relay = SubagentEventRelay::open(root.path(), lease.clone()).unwrap();
        relay
            .append(RelayRecord::new(&lease, 1, commit_id(1), b"first".to_vec()))
            .unwrap();
        relay
            .append(RelayRecord::new(
                &lease,
                2,
                commit_id(2),
                b"second".to_vec(),
            ))
            .unwrap();
        let path = relay.path().to_path_buf();
        let mut bytes = fs::read(&path).unwrap();
        let first_payload_len =
            u32::from_be_bytes(bytes[..FRAME_PREFIX_BYTES].try_into().unwrap()) as usize;
        bytes[FRAME_PREFIX_BYTES + first_payload_len] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();

        assert!(matches!(
            relay.read_page(0),
            Err(RelayError::Corrupt { offset: 0, .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(relay.is_quarantined());
    }

    #[test]
    fn relay_history_rejects_regressing_lease_epochs() {
        let root = tempfile::tempdir().unwrap();
        let (_, lease_two) = lease(root.path(), "owner-a", 2);
        let relay = SubagentEventRelay::open(root.path(), lease_two.clone()).unwrap();
        relay
            .append(RelayRecord::new(
                &lease_two,
                1,
                commit_id(1),
                b"current".to_vec(),
            ))
            .unwrap();

        let lease_one =
            RelayLease::new("task-1", RelayTaskType::Subagent, "owner-a", 1, "attempt-1").unwrap();
        let stale_record = RelayRecord::new(&lease_one, 2, commit_id(2), b"stale".to_vec());
        let mut file = OpenOptions::new().append(true).open(relay.path()).unwrap();
        file.write_all(&encode_frame(&stale_record).unwrap())
            .unwrap();
        file.sync_all().unwrap();

        let (_, lease_three) = lease(root.path(), "owner-b", 3);
        assert!(matches!(
            SubagentEventRelay::open(root.path(), lease_three),
            Err(RelayError::Corrupt { .. })
        ));
        assert!(relay.is_quarantined());
    }

    #[test]
    fn incomplete_tail_is_repaired_only_by_a_new_epoch() {
        let root = tempfile::tempdir().unwrap();
        let (_, lease_one) = lease(root.path(), "owner-a", 1);
        let first = SubagentEventRelay::open(root.path(), lease_one.clone()).unwrap();
        first
            .append(RelayRecord::new(&lease_one, 1, commit_id(1), b"x".to_vec()))
            .unwrap();
        let path = first.path().to_path_buf();
        let original_len = fs::metadata(&path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0, 0, 0, 20, 1, 2]).unwrap();
        file.sync_all().unwrap();
        assert!(matches!(first.read_page(0), Ok(RelayPage { .. })));
        assert!(matches!(
            first.append(RelayRecord::new(&lease_one, 2, commit_id(2), b"y".to_vec())),
            Err(RelayError::IncompleteTail { .. })
        ));
        let (_, lease_two) = lease(root.path(), "owner-b", 2);
        let second = SubagentEventRelay::open(root.path(), lease_two.clone()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), original_len);
        second
            .append(RelayRecord::new(&lease_two, 2, commit_id(2), b"y".to_vec()))
            .unwrap();
    }

    #[test]
    fn path_identity_rejects_traversal_and_symlinked_task_directory() {
        let root = tempfile::tempdir().unwrap();
        let invalid = RelayLease::new(
            "../outside",
            RelayTaskType::Subagent,
            "owner-a",
            1,
            "attempt-1",
        );
        assert!(matches!(invalid, Err(RelayError::InvalidIdentity(_))));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path(), root.path().join("task-1")).unwrap();
            let lease =
                RelayLease::new("task-1", RelayTaskType::Subagent, "owner-a", 1, "attempt-1")
                    .unwrap();
            assert!(matches!(
                SubagentEventRelay::open(root.path(), lease),
                Err(RelayError::ContainmentViolation)
            ));
        }
    }
}
