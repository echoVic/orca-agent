//! Opt-in disk retention, independent of transcript health and replay limits.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use orca_platform::fs::ExclusiveFileLock;
use serde::Serialize;

use super::{assets, local, session_index, writer};

#[derive(Clone, Debug, Default)]
pub struct SessionRetentionPolicy {
    /// Target disk usage across active and archived transcripts plus images.
    /// Only archived sessions are eligible; a quota may remain unmet.
    pub max_bytes: Option<u64>,
    pub older_than_days: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionRetentionCandidate {
    pub path: PathBuf,
    pub bytes: u64,
    pub modified_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionRetentionReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub quota_satisfied: bool,
    pub candidates: Vec<SessionRetentionCandidate>,
    pub deleted: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub applied: bool,
}

#[derive(Debug)]
pub struct SessionRetentionError {
    pub report: SessionRetentionReport,
    pub failed_path: PathBuf,
    pub transcript_removed: bool,
    source: io::Error,
}

impl std::fmt::Display for SessionRetentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for path in &self.report.deleted {
            writeln!(f, "deleted: {}", path.display())?;
        }
        for path in &self.report.skipped {
            writeln!(f, "skipped: {}", path.display())?;
        }
        write!(
            f,
            "retention failed at {} (transcript removed: {}): {}",
            self.failed_path.display(),
            self.transcript_removed,
            self.source
        )
    }
}

impl std::error::Error for SessionRetentionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn retention_error(
    report: &SessionRetentionReport,
    path: &Path,
    transcript_removed: bool,
    source: io::Error,
) -> io::Error {
    io::Error::new(
        source.kind(),
        SessionRetentionError {
            report: report.clone(),
            failed_path: path.to_path_buf(),
            transcript_removed,
            source,
        },
    )
}

fn session_bytes(path: &Path) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::other("retention requires a regular transcript"));
    }
    let mut bytes = metadata.len();
    let root = assets::directory(path);
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(bytes),
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(io::Error::other(
                "retention refuses a linked asset directory",
            ));
        }
        _ => {}
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(io::Error::other("retention refuses non-regular assets"));
        }
        bytes = bytes.saturating_add(entry.metadata()?.len());
    }
    Ok(bytes)
}

pub(super) fn acquire_idle_owner(
    path: &Path,
) -> io::Result<(ExclusiveFileLock, ExclusiveFileLock)> {
    let plain = if path.extension().and_then(|ext| ext.to_str()) == Some("zst") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    // Older runtimes kept the compressed-path lease after restoring JSONL.
    // Check both identities until all such owners have exited.
    let plain_owner = ExclusiveFileLock::try_acquire(&plain.with_extension("surface-owner.lock"))
        .map_err(io::Error::other)?;
    let compressed_owner =
        ExclusiveFileLock::try_acquire(&plain.with_extension("jsonl.surface-owner.lock"))
            .map_err(io::Error::other)?;
    Ok((plain_owner, compressed_owner))
}

/// Dry-run by default at the call site. Applying a policy never invalidates a
/// live session: only explicitly archived, unlocked, unchanged files are removed.
pub fn retain_sessions(
    policy: &SessionRetentionPolicy,
    apply: bool,
) -> io::Result<SessionRetentionReport> {
    if apply && policy.max_bytes.is_none() && policy.older_than_days.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "retention requires an explicit age or quota policy",
        ));
    }
    let mut entries = Vec::new();
    for root in [local::sessions_dir(), local::archive_dir()] {
        if root.exists() {
            local::collect_session_files(&root, &mut |path| entries.push(path.to_path_buf()))?;
        }
    }
    let mut bytes_before = 0u64;
    let mut archived = Vec::new();
    for path in entries {
        let bytes = session_bytes(&path)?;
        bytes_before = bytes_before.saturating_add(bytes);
        if path.starts_with(local::archive_dir()) {
            archived.push(SessionRetentionCandidate {
                modified_at: fs::metadata(&path)?.modified()?.into(),
                path,
                bytes,
            });
        }
    }
    archived.sort_by(|a, b| a.modified_at.cmp(&b.modified_at).then(a.path.cmp(&b.path)));
    let now = Utc::now();
    let mut projected_bytes = bytes_before;
    let mut candidates = Vec::new();
    for entry in archived {
        let expired = policy.older_than_days.is_some_and(|days| {
            now.signed_duration_since(entry.modified_at)
                .num_seconds()
                .max(0) as u64
                >= days.saturating_mul(86_400)
        });
        if expired
            || policy
                .max_bytes
                .is_some_and(|limit| projected_bytes > limit)
        {
            projected_bytes = projected_bytes.saturating_sub(entry.bytes);
            candidates.push(entry);
        }
    }
    let mut report = SessionRetentionReport {
        bytes_before,
        bytes_after: if apply { bytes_before } else { projected_bytes },
        quota_satisfied: false,
        candidates,
        deleted: Vec::new(),
        skipped: Vec::new(),
        applied: apply,
    };
    if apply {
        for candidate in &report.candidates {
            // Same lease used by the hosted runtime; acquiring it here prevents
            // a concurrent resume from racing the retention decision.
            let Ok(_owner) = acquire_idle_owner(&candidate.path) else {
                report.skipped.push(candidate.path.clone());
                continue;
            };
            let _append = writer::acquire_file_lock(&candidate.path)
                .map_err(|error| retention_error(&report, &candidate.path, false, error))?;
            let unchanged = fs::metadata(&candidate.path)
                .and_then(|meta| meta.modified())
                .is_ok_and(|time| DateTime::<Utc>::from(time) == candidate.modified_at)
                && session_bytes(&candidate.path).ok() == Some(candidate.bytes);
            if !unchanged {
                report.skipped.push(candidate.path.clone());
                continue;
            }
            fs::remove_file(&candidate.path)
                .map_err(|error| retention_error(&report, &candidate.path, false, error))?;
            assets::remove_directory(&candidate.path)
                .map_err(|error| retention_error(&report, &candidate.path, true, error))?;
            let _ = session_index::remove_path(&candidate.path);
            report.bytes_after = report.bytes_after.saturating_sub(candidate.bytes);
            report.deleted.push(candidate.path.clone());
        }
    }
    report.quota_satisfied = policy
        .max_bytes
        .is_none_or(|limit| report.bytes_after <= limit);
    Ok(report)
}
