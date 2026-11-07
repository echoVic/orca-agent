//! Two-phase restore application with a crash-safe JSONL journal (§12.1).
//!
//! An apply writes an `intent` line (fsync), then for every file backs up the
//! current content under `<journal_dir>/<op_id>/pre/<sha256>`, publishes the
//! new content with a same-directory temp file + fsync + rename, appends an
//! `applied` line (fsync) and only after all writes deletes the tombstoned
//! paths. A successful run appends `committed` and removes the journal; a
//! failed run returns `partial: true`, keeps journal + backups and can be
//! undone with [`Applier::rollback`].

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MAX_SNAPSHOT_FILE_BYTES;
use crate::error::RewindError;
use crate::path::{
    BoundedFile, atomic_publish, excluded_directory, file_sha256, read_regular_file,
    resolve_inside, sha256_hex, sync_dir, validate_digest,
};
use crate::plan::{PlanSkip, PlannedDelete, PlannedWrite, RestorePlan, skip_reason};
use crate::store::RewindStore;

/// Result of [`Applier::apply`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Operation id (`rw_<uuid>`); required to [`Applier::rollback`].
    pub op_id: String,
    /// Files successfully written.
    pub applied: u64,
    /// Files successfully deleted.
    pub deleted: u64,
    /// Files deliberately not touched (plan skips plus runtime blob/path skips).
    pub skipped: Vec<PlanSkip>,
    /// `true` when an IO error stopped the apply; journal and backups are kept.
    pub partial: bool,
}

/// Result of [`Applier::rollback`].
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RollbackReport {
    /// Files written back from their `pre/` backup (including recreated deletes).
    pub restored: u64,
    /// Files removed because they did not exist before the apply.
    pub removed: u64,
}

/// Crash-safe restore applier.
#[derive(Debug)]
pub struct Applier {
    root: PathBuf,
    store: RewindStore,
    journal_dir: PathBuf,
}

struct PendingWrite<'a> {
    planned: &'a PlannedWrite,
    abs: PathBuf,
    bytes: Vec<u8>,
}

struct PendingDelete<'a> {
    planned: &'a PlannedDelete,
    abs: PathBuf,
}

impl Applier {
    /// `root` must be the same normalized workspace root used for planning;
    /// `journal_dir` is typically `<session_root>/journals`.
    pub fn new(root: PathBuf, store: RewindStore, journal_dir: PathBuf) -> Self {
        Self {
            root,
            store,
            journal_dir,
        }
    }

    /// Apply a plan.
    ///
    /// Path safety and the per-write CAS check (`plan.current_sha256` versus
    /// the disk) run before anything is written; a mismatch is an error and no
    /// journal is created. Missing/corrupt blobs and unsafe paths are skipped
    /// without interrupting the rest. Any other IO error stops the apply and
    /// returns a `partial: true` report with the journal retained.
    pub fn apply(&self, plan: &RestorePlan) -> io::Result<ApplyReport> {
        let op_id = new_op_id();
        let mut report = ApplyReport {
            op_id: op_id.clone(),
            skipped: plan.skipped.clone(),
            ..ApplyReport::default()
        };

        let mut writes: Vec<PendingWrite<'_>> = Vec::new();
        for planned in &plan.writes {
            if let Some(excluded) = excluded_directory(&planned.path) {
                report.skipped.push(PlanSkip {
                    path: planned.path.clone(),
                    reason: format!("excluded_path: {excluded}"),
                });
                continue;
            }
            let abs = match resolve_inside(&self.root, &planned.path) {
                Ok(abs) => abs,
                Err(error) => {
                    report.skipped.push(PlanSkip {
                        path: planned.path.clone(),
                        reason: skip_reason(&error),
                    });
                    continue;
                }
            };
            let current = match file_sha256(&abs) {
                Ok(current) => current,
                Err(error)
                    if crate::rewind_error(&error).is_some_and(RewindError::is_not_regular) =>
                {
                    report.skipped.push(PlanSkip {
                        path: planned.path.clone(),
                        reason: "not_regular".to_string(),
                    });
                    continue;
                }
                Err(error) => return Err(error),
            };
            if current != planned.current_sha256 {
                return Err(RewindError::ConcurrentWorkspaceChange {
                    path: planned.path.clone(),
                    expected: planned.current_sha256.clone(),
                    actual: current,
                }
                .into());
            }
            let bytes = match self.store.read_blob(&planned.target) {
                Ok(bytes) => bytes,
                Err(error) => {
                    report.skipped.push(PlanSkip {
                        path: planned.path.clone(),
                        reason: format!("blob_unreadable: {error}"),
                    });
                    continue;
                }
            };
            writes.push(PendingWrite {
                planned,
                abs,
                bytes,
            });
        }

        let mut deletes: Vec<PendingDelete<'_>> = Vec::new();
        for planned in &plan.deletes {
            if let Some(excluded) = excluded_directory(&planned.path) {
                report.skipped.push(PlanSkip {
                    path: planned.path.clone(),
                    reason: format!("excluded_path: {excluded}"),
                });
                continue;
            }
            let abs = match resolve_inside(&self.root, &planned.path) {
                Ok(abs) => abs,
                Err(error) => {
                    report.skipped.push(PlanSkip {
                        path: planned.path.clone(),
                        reason: skip_reason(&error),
                    });
                    continue;
                }
            };
            match file_sha256(&abs) {
                Ok(Some(_)) => deletes.push(PendingDelete { planned, abs }),
                Ok(None) => report.skipped.push(PlanSkip {
                    path: planned.path.clone(),
                    reason: "already_missing".to_string(),
                }),
                Err(error)
                    if crate::rewind_error(&error).is_some_and(RewindError::is_not_regular) =>
                {
                    report.skipped.push(PlanSkip {
                        path: planned.path.clone(),
                        reason: "not_regular".to_string(),
                    });
                }
                Err(error) => return Err(error),
            }
        }

        if writes.is_empty() && deletes.is_empty() {
            return Ok(report);
        }

        fs::create_dir_all(&self.journal_dir)?;
        let op_dir = self.journal_dir.join(&op_id);
        let pre_dir = op_dir.join("pre");
        fs::create_dir_all(&pre_dir)?;
        let journal_path = self.journal_path(&op_id);
        let mut journal = JournalWriter::create(&journal_path)?;
        let intent = JournalEntry::Intent {
            op_id: op_id.clone(),
            root: self.root.display().to_string(),
            plan_digest: plan_digest(plan),
            writes: writes
                .iter()
                .map(|pending| pending.planned.path.clone())
                .collect(),
            deletes: deletes
                .iter()
                .map(|pending| pending.planned.path.clone())
                .collect(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(error) = journal.append(&intent) {
            cleanup_quietly(&journal_path, &op_dir);
            return Err(error);
        }

        for pending in &writes {
            if let Err(error) = self.apply_write(pending, &pre_dir, &mut journal) {
                report.partial = true;
                let _ = journal.append(&JournalEntry::Failed {
                    path: pending.planned.path.clone(),
                    message: error.to_string(),
                });
                return Ok(report);
            }
            report.applied += 1;
        }

        for pending in &deletes {
            if let Err(error) = self.apply_delete(pending, &pre_dir, &mut journal) {
                report.partial = true;
                let _ = journal.append(&JournalEntry::Failed {
                    path: pending.planned.path.clone(),
                    message: error.to_string(),
                });
                return Ok(report);
            }
            report.deleted += 1;
        }

        if let Err(error) = journal.append(&JournalEntry::Committed {
            op_id: op_id.clone(),
        }) {
            report.partial = true;
            let _ = error;
            return Ok(report);
        }
        drop(journal);
        cleanup_quietly(&journal_path, &op_dir);
        Ok(report)
    }

    /// Undo the files recorded as applied in `op_id`'s journal, newest first.
    ///
    /// Overwritten files are restored from `pre/<sha256>`; files that did not
    /// exist before are removed. Entries whose previous content could not be
    /// backed up (oversized) are left as-is. On success the journal and its
    /// backups are deleted.
    pub fn rollback(&self, op_id: &str) -> io::Result<RollbackReport> {
        validate_op_id(op_id)?;
        let journal_path = self.journal_path(op_id);
        let content = fs::read_to_string(&journal_path)?;
        let mut applied = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: JournalEntry =
                serde_json::from_str(line).map_err(|error| RewindError::JournalCorrupt {
                    detail: format!("{line:?}: {error}"),
                })?;
            if matches!(entry, JournalEntry::Applied { .. }) {
                applied.push(entry);
            }
        }

        let op_dir = self.journal_dir.join(op_id);
        let pre_dir = op_dir.join("pre");
        let mut report = RollbackReport::default();
        for entry in applied.iter().rev() {
            let JournalEntry::Applied { path, previous, .. } = entry else {
                continue;
            };
            let abs = resolve_inside(&self.root, path)?;
            match previous {
                PreviousState::Absent => match fs::remove_file(&abs) {
                    Ok(()) => report.removed += 1,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                },
                PreviousState::Saved { sha256, mode } => {
                    validate_digest(sha256)?;
                    let backup_path = pre_dir.join(sha256);
                    let bytes = fs::read(&backup_path)?;
                    let actual = sha256_hex(&bytes);
                    if actual != *sha256 {
                        return Err(RewindError::CorruptBackup {
                            sha256: sha256.clone(),
                        }
                        .into());
                    }
                    atomic_publish(&abs, &bytes, *mode)?;
                    report.restored += 1;
                }
                PreviousState::Omitted => {}
            }
        }

        cleanup_quietly(&journal_path, &op_dir);
        Ok(report)
    }

    /// Return the op id of an unfinished journal, if one exists.
    ///
    /// A journal is unfinished when its last non-empty line is not `committed`
    /// (or the file cannot be parsed as a journal at all). Completed journals
    /// that were not cleaned up after a crash are not reported.
    pub fn pending_journal(&self) -> io::Result<Option<String>> {
        let entries = match fs::read_dir(&self.journal_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut pending = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(last) = content.lines().rev().find(|line| !line.trim().is_empty()) else {
                continue;
            };
            let committed = matches!(
                serde_json::from_str::<JournalEntry>(last),
                Ok(JournalEntry::Committed { .. })
            );
            if committed {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                pending.push(stem.to_string());
            }
        }
        pending.sort();
        Ok(pending.into_iter().next())
    }

    fn journal_path(&self, op_id: &str) -> PathBuf {
        self.journal_dir.join(format!("{op_id}.jsonl"))
    }

    fn apply_write(
        &self,
        pending: &PendingWrite<'_>,
        pre_dir: &Path,
        journal: &mut JournalWriter,
    ) -> io::Result<()> {
        let previous = self.backup(&pending.abs, pre_dir)?;
        // Fall back to the mode the file already had when the blob did not
        // capture one (e.g. a blob produced by `put_bytes(..., None)`).
        let mode = pending.planned.target.mode.or(match &previous {
            PreviousState::Saved { mode, .. } => *mode,
            _ => None,
        });
        atomic_publish(&pending.abs, &pending.bytes, mode)?;
        journal.append(&JournalEntry::Applied {
            path: pending.planned.path.clone(),
            action: JournalAction::Write,
            sha256: pending.planned.target.sha256.clone(),
            previous,
        })
    }

    fn apply_delete(
        &self,
        pending: &PendingDelete<'_>,
        pre_dir: &Path,
        journal: &mut JournalWriter,
    ) -> io::Result<()> {
        let previous = self.backup(&pending.abs, pre_dir)?;
        match fs::remove_file(&pending.abs) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let sha256 = match &previous {
            PreviousState::Saved { sha256, .. } => sha256.clone(),
            _ => String::new(),
        };
        journal.append(&JournalEntry::Applied {
            path: pending.planned.path.clone(),
            action: JournalAction::Delete,
            sha256,
            previous,
        })
    }

    fn backup(&self, abs: &Path, pre_dir: &Path) -> io::Result<PreviousState> {
        match read_regular_file(abs, MAX_SNAPSHOT_FILE_BYTES)? {
            BoundedFile::Missing => Ok(PreviousState::Absent),
            BoundedFile::TooLarge { .. } | BoundedFile::NotRegular => Ok(PreviousState::Omitted),
            BoundedFile::Regular { bytes, mode } => {
                let sha256 = sha256_hex(&bytes);
                let path = pre_dir.join(&sha256);
                if fs::symlink_metadata(&path).is_err() {
                    let mut temporary = tempfile::NamedTempFile::new_in(pre_dir)?;
                    temporary.write_all(&bytes)?;
                    temporary.as_file().sync_all()?;
                    match temporary.persist_noclobber(&path) {
                        Ok(_) => {}
                        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.error),
                    }
                    sync_dir(pre_dir)?;
                }
                Ok(PreviousState::Saved { sha256, mode })
            }
        }
    }
}

/// One JSONL journal record.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalEntry {
    Intent {
        op_id: String,
        root: String,
        plan_digest: String,
        writes: Vec<String>,
        deletes: Vec<String>,
        recorded_at: String,
    },
    Applied {
        path: String,
        action: JournalAction,
        sha256: String,
        previous: PreviousState,
    },
    Failed {
        path: String,
        message: String,
    },
    Committed {
        op_id: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalAction {
    Write,
    Delete,
}

/// What the path looked like before this operation touched it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum PreviousState {
    /// The path did not exist; a rollback removes it.
    Absent,
    /// Backed up under `pre/<sha256>`; a rollback writes it back.
    Saved { sha256: String, mode: Option<u32> },
    /// Too large to back up; a rollback cannot restore this path.
    Omitted,
}

struct JournalWriter {
    file: File,
}

impl JournalWriter {
    fn create(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        Ok(Self { file })
    }

    fn append(&mut self, entry: &JournalEntry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_all()
    }
}

fn new_op_id() -> String {
    format!("rw_{}", Uuid::now_v7().simple())
}

fn validate_op_id(op_id: &str) -> io::Result<()> {
    let valid = !op_id.is_empty()
        && op_id.len() <= 128
        && op_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(RewindError::InvalidOperationId {
            op_id: op_id.to_string(),
        }
        .into())
    }
}

fn plan_digest(plan: &RestorePlan) -> String {
    let mut buffer = String::new();
    for write in &plan.writes {
        buffer.push_str("write\0");
        buffer.push_str(&write.path);
        buffer.push('\0');
        buffer.push_str(&write.target.sha256);
        buffer.push('\0');
        buffer.push_str(write.current_sha256.as_deref().unwrap_or("-"));
        buffer.push('\n');
    }
    for delete in &plan.deletes {
        buffer.push_str("delete\0");
        buffer.push_str(&delete.path);
        buffer.push('\n');
    }
    sha256_hex(buffer.as_bytes())
}

fn cleanup_quietly(journal_path: &Path, op_dir: &Path) {
    let _ = fs::remove_file(journal_path);
    let _ = fs::remove_dir_all(op_dir);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::plan::{
        PlanOptions, PlanSkip, PlannedDelete, PlannedWrite, RestorePlan, SnapshotFileView,
        SnapshotView, build_plan,
    };
    use crate::store::{BlobRef, PreImage, RewindStore};

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        journal_dir: PathBuf,
        store: RewindStore,
    }

    impl Fixture {
        fn applier(&self) -> Applier {
            Applier::new(
                self.root.clone(),
                RewindStore::open(self.store.root()).expect("store handle"),
                self.journal_dir.clone(),
            )
        }

        fn blob(&self, bytes: &[u8]) -> BlobRef {
            self.store.put_bytes(bytes, None).expect("put blob")
        }

        fn sha(&self, bytes: &[u8]) -> String {
            self.blob(bytes).sha256
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(path, bytes).expect("write file");
        }

        fn read(&self, rel: &str) -> Vec<u8> {
            fs::read(self.root.join(rel)).expect("read file")
        }
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        fs::create_dir_all(&root).expect("workspace");
        let session_root = temp.path().join("sessions/s1");
        let store = RewindStore::open(&session_root).expect("store");
        Fixture {
            journal_dir: session_root.join("journals"),
            _temp: temp,
            root,
            store,
        }
    }

    fn write_op(
        planned: &mut RestorePlan,
        blob: BlobRef,
        path: &str,
        current_sha256: Option<String>,
    ) {
        planned.writes.push(PlannedWrite {
            path: path.to_string(),
            target: blob,
            current_sha256,
            untracked_change: false,
        });
    }

    #[test]
    fn applies_writes_then_deletes_and_cleans_journal() {
        let fixture = fixture();
        fixture.write("a.txt", b"old");
        fixture.write("b.txt", b"delete me");
        let mut plan = RestorePlan::default();
        write_op(
            &mut plan,
            fixture.blob(b"new"),
            "a.txt",
            Some(fixture.sha(b"old")),
        );
        plan.deletes.push(PlannedDelete {
            path: "b.txt".to_string(),
            untracked_change: false,
        });

        let applier = fixture.applier();
        let report = applier.apply(&plan).expect("apply");
        assert!(!report.partial, "{:?}", report.skipped);
        assert_eq!(report.applied, 1);
        assert_eq!(report.deleted, 1);
        assert!(report.op_id.starts_with("rw_"));
        assert_eq!(fixture.read("a.txt"), b"new");
        assert!(!fixture.root.join("b.txt").exists());
        assert!(applier.pending_journal().expect("pending").is_none());
        assert!(
            !applier.journal_path(&report.op_id).exists(),
            "successful apply removes its journal"
        );
        // A committed operation cannot be rolled back.
        assert!(applier.rollback(&report.op_id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn restores_the_captured_mode() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = fixture();
        fixture.write("script.sh", b"old");
        let blob = fixture
            .store
            .put_bytes(b"#!/bin/sh\n", Some(0o640))
            .expect("blob");
        let mut plan = RestorePlan::default();
        write_op(&mut plan, blob, "script.sh", Some(fixture.sha(b"old")));

        fixture.applier().apply(&plan).expect("apply");
        let mode = fs::metadata(fixture.root.join("script.sh"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn partial_apply_keeps_journal_and_rolls_back() {
        let fixture = fixture();
        fixture.write("a.txt", b"old");
        let mut plan = RestorePlan::default();
        write_op(
            &mut plan,
            fixture.blob(b"restored"),
            "a.txt",
            Some(fixture.sha(b"old")),
        );
        // The parent directory does not exist, so this write fails with ENOENT.
        write_op(&mut plan, fixture.blob(b"never"), "missing_dir/b.txt", None);

        let applier = fixture.applier();
        let report = applier.apply(&plan).expect("apply returns partial report");
        assert!(report.partial);
        assert_eq!(report.applied, 1);
        assert_eq!(fixture.read("a.txt"), b"restored");

        let pending = applier
            .pending_journal()
            .expect("pending")
            .expect("journal");
        assert_eq!(pending, report.op_id);

        let journal = fs::read_to_string(applier.journal_path(&report.op_id)).expect("journal");
        let kinds: Vec<&str> = journal
            .lines()
            .map(|line| serde_json::from_str::<JournalEntry>(line).expect("journal line"))
            .map(|entry| match entry {
                JournalEntry::Intent { .. } => "intent",
                JournalEntry::Applied { .. } => "applied",
                JournalEntry::Failed { .. } => "failed",
                JournalEntry::Committed { .. } => "committed",
            })
            .collect();
        assert_eq!(kinds, vec!["intent", "applied", "failed"]);

        let rollback = applier.rollback(&report.op_id).expect("rollback");
        assert_eq!(rollback.restored, 1);
        assert_eq!(rollback.removed, 0);
        assert_eq!(fixture.read("a.txt"), b"old");
        assert!(applier.pending_journal().expect("pending").is_none());
        assert!(!applier.journal_path(&report.op_id).exists());
    }

    #[test]
    fn rollback_removes_files_created_by_the_partial_apply() {
        let fixture = fixture();
        let mut plan = RestorePlan::default();
        write_op(&mut plan, fixture.blob(b"created"), "new.txt", None);
        write_op(&mut plan, fixture.blob(b"never"), "missing_dir/b.txt", None);

        let applier = fixture.applier();
        let report = applier.apply(&plan).expect("partial apply");
        assert!(report.partial);
        assert_eq!(report.applied, 1);
        assert_eq!(fixture.read("new.txt"), b"created");

        let rollback = applier.rollback(&report.op_id).expect("rollback");
        assert_eq!(rollback.restored, 0);
        assert_eq!(rollback.removed, 1);
        assert!(!fixture.root.join("new.txt").exists());
    }

    #[test]
    fn rollback_recreates_a_deleted_file_from_the_journal() {
        let fixture = fixture();
        let op_id = "rw_delete_test";
        let content = b"delete me";
        let sha256 = fixture.sha(content);
        let op_dir = fixture.journal_dir.join(op_id);
        let pre_dir = op_dir.join("pre");
        fs::create_dir_all(&pre_dir).expect("pre dir");
        fs::write(pre_dir.join(&sha256), content).expect("backup");
        let mut journal = String::new();
        journal.push_str(
            &serde_json::to_string(&JournalEntry::Intent {
                op_id: op_id.to_string(),
                root: fixture.root.display().to_string(),
                plan_digest: "digest".to_string(),
                writes: Vec::new(),
                deletes: vec!["b.txt".to_string()],
                recorded_at: "2026-11-07T00:00:00Z".to_string(),
            })
            .expect("intent"),
        );
        journal.push('\n');
        journal.push_str(
            &serde_json::to_string(&JournalEntry::Applied {
                path: "b.txt".to_string(),
                action: JournalAction::Delete,
                sha256: sha256.clone(),
                previous: PreviousState::Saved {
                    sha256: sha256.clone(),
                    mode: Some(0o644),
                },
            })
            .expect("applied"),
        );
        journal.push('\n');
        fs::create_dir_all(&fixture.journal_dir).expect("journal dir");
        fs::write(fixture.journal_dir.join(format!("{op_id}.jsonl")), journal).expect("journal");

        let applier = fixture.applier();
        assert_eq!(
            applier.pending_journal().expect("pending").as_deref(),
            Some(op_id)
        );

        let rollback = applier.rollback(op_id).expect("rollback");
        assert_eq!(rollback.restored, 1);
        assert_eq!(rollback.removed, 0);
        assert_eq!(fixture.read("b.txt"), content);
        assert!(applier.pending_journal().expect("pending").is_none());
    }

    #[test]
    fn rejects_a_concurrent_workspace_change() {
        let fixture = fixture();
        fixture.write("a.txt", b"changed on disk");
        let mut plan = RestorePlan::default();
        write_op(
            &mut plan,
            fixture.blob(b"rewrite"),
            "a.txt",
            Some(fixture.sha(b"planned state")),
        );

        let applier = fixture.applier();
        let error = applier.apply(&plan).expect_err("CAS mismatch");
        assert!(
            crate::rewind_error(&error).is_some_and(RewindError::is_concurrent_change),
            "{error}"
        );
        assert_eq!(fixture.read("a.txt"), b"changed on disk");
        assert!(applier.pending_journal().expect("pending").is_none());
        assert!(
            !applier.journal_dir.exists(),
            "no journal is created on CAS failure"
        );
    }

    #[test]
    fn skips_a_missing_blob_without_interrupting() {
        let fixture = fixture();
        fixture.write("a.txt", b"old");
        let mut plan = RestorePlan::default();
        write_op(
            &mut plan,
            BlobRef {
                sha256: "9".repeat(64),
                size: 3,
                mode: None,
                encoding: crate::store::BlobEncoding::Raw,
            },
            "a.txt",
            Some(fixture.sha(b"old")),
        );

        let applier = fixture.applier();
        let report = applier.apply(&plan).expect("apply");
        assert!(!report.partial);
        assert_eq!(report.applied, 0);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.starts_with("blob_unreadable:"));
        assert_eq!(fixture.read("a.txt"), b"old");
        assert!(applier.pending_journal().expect("pending").is_none());
    }

    #[test]
    fn skips_deletes_of_files_that_already_vanished() {
        let fixture = fixture();
        let mut plan = RestorePlan::default();
        plan.deletes.push(PlannedDelete {
            path: "gone.txt".to_string(),
            untracked_change: false,
        });

        let report = fixture.applier().apply(&plan).expect("apply");
        assert_eq!(report.applied, 0);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, "already_missing");
    }

    #[test]
    fn skips_unsafe_planned_paths() {
        let fixture = fixture();
        let mut plan = RestorePlan::default();
        write_op(&mut plan, fixture.blob(b"evil"), "../escape.txt", None);

        let report = fixture.applier().apply(&plan).expect("apply");
        assert_eq!(report.applied, 0);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.starts_with("path_escape:"));
    }

    #[test]
    fn empty_plan_returns_an_id_without_touching_disk() {
        let fixture = fixture();
        let applier = fixture.applier();
        let report = applier.apply(&RestorePlan::default()).expect("apply");
        assert!(!report.op_id.is_empty());
        assert_eq!(report.applied, 0);
        assert_eq!(report.deleted, 0);
        assert!(!report.partial);
        assert!(!applier.journal_dir.exists());
        assert!(applier.pending_journal().expect("pending").is_none());
    }

    #[test]
    fn rollback_rejects_unsafe_or_unknown_op_ids() {
        let fixture = fixture();
        let applier = fixture.applier();
        let overlong = "x".repeat(129);
        for op_id in ["", "../evil", "a/b", overlong.as_str()] {
            let error = applier.rollback(op_id).expect_err("invalid op id");
            assert!(
                crate::rewind_error(&error)
                    .is_some_and(|cause| matches!(cause, RewindError::InvalidOperationId { .. })),
                "{op_id:?}: {error}"
            );
        }
        let error = applier.rollback("rw_unknown").expect_err("unknown journal");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn pending_journal_ignores_committed_and_foreign_files() {
        let fixture = fixture();
        fs::create_dir_all(&fixture.journal_dir).expect("journal dir");
        fs::write(fixture.journal_dir.join("readme.txt"), b"not a journal").expect("foreign");
        fs::write(fixture.journal_dir.join("empty.jsonl"), b"").expect("empty");
        let committed = format!(
            "{}\n",
            serde_json::to_string(&JournalEntry::Committed {
                op_id: "rw_done".to_string()
            })
            .expect("committed")
        );
        fs::write(fixture.journal_dir.join("rw_done.jsonl"), committed).expect("committed");

        let applier = fixture.applier();
        assert!(applier.pending_journal().expect("pending").is_none());
    }

    #[test]
    fn skips_planned_targets_that_are_directories() {
        let fixture = fixture();
        fs::create_dir_all(fixture.root.join("dir")).expect("dir");
        let mut plan = RestorePlan::default();
        write_op(&mut plan, fixture.blob(b"data"), "dir", None);

        let report = fixture.applier().apply(&plan).expect("apply");
        assert_eq!(report.applied, 0);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.starts_with("path_escape:"));
    }

    #[test]
    fn capture_plan_apply_round_trip() {
        use std::collections::BTreeMap;

        let fixture = fixture();
        // State when the target message was submitted: a.txt and c.txt exist,
        // b.txt does not.
        fixture.write("a.txt", b"before the turn");
        fixture.write("c.txt", b"deleted during the turn");
        let a_pre = fixture
            .store
            .put_pre_image(&fixture.root.join("a.txt"))
            .expect("capture a");
        let c_pre = fixture
            .store
            .put_pre_image(&fixture.root.join("c.txt"))
            .expect("capture c");

        // The turn: rewrite a.txt, create b.txt, delete c.txt.
        fixture.write("a.txt", b"after the turn");
        fixture.write("b.txt", b"created by the turn");
        fs::remove_file(fixture.root.join("c.txt")).expect("delete c");

        let mut files = BTreeMap::new();
        files.insert(
            "a.txt".to_string(),
            SnapshotFileView {
                pre: a_pre,
                post_sha256: Some(fixture.sha(b"after the turn")),
            },
        );
        files.insert(
            "b.txt".to_string(),
            SnapshotFileView {
                pre: PreImage::Missing,
                post_sha256: Some(fixture.sha(b"created by the turn")),
            },
        );
        files.insert(
            "c.txt".to_string(),
            SnapshotFileView {
                pre: c_pre.clone(),
                post_sha256: None,
            },
        );
        let snapshot = SnapshotView {
            ordinal: 1,
            at_message_id: "item_1".to_string(),
            files,
        };

        let plan = build_plan(
            &fixture.root,
            &fixture.store,
            &[snapshot],
            PlanOptions { target_ordinal: 1 },
        )
        .expect("plan");
        assert_eq!(plan.writes.len(), 2, "a.txt and c.txt are rewritten");
        assert_eq!(plan.deletes.len(), 1, "b.txt is removed");
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        let report = fixture.applier().apply(&plan).expect("apply");
        assert!(!report.partial);
        assert_eq!(report.applied, 2);
        assert_eq!(report.deleted, 1);
        assert_eq!(fixture.read("a.txt"), b"before the turn");
        assert_eq!(fixture.read("c.txt"), b"deleted during the turn");
        assert!(!fixture.root.join("b.txt").exists());
        let PreImage::Blob(c_blob) = &c_pre else {
            panic!("c.txt pre-image should be a blob");
        };
        assert_eq!(
            fixture.store.read_blob(c_blob).expect("c blob"),
            b"deleted during the turn"
        );
    }

    #[test]
    fn untracked_plan_skips_are_preserved_in_the_report() {
        let fixture = fixture();
        let mut plan = RestorePlan::default();
        plan.skipped.push(PlanSkip {
            path: "x.txt".to_string(),
            reason: "blob_unreadable: gone".to_string(),
        });
        let report = fixture.applier().apply(&plan).expect("apply");
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].path, "x.txt");
    }
}
