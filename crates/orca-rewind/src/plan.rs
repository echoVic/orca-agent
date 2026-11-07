//! Restore planning: the "undo log" algorithm from §8.3 of the design doc.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::MAX_DIFF_INPUT_BYTES;
use crate::error::RewindError;
use crate::path::{
    BoundedFile, excluded_directory, file_sha256, read_regular_file, resolve_inside,
};
use crate::store::{BlobRef, PreImage, RewindStore};

/// One turn's file snapshot, as projected from a `session.snapshot` record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotView {
    /// Zero-based index of the turn's user message in the conversation.
    pub ordinal: u32,
    /// `ConversationItemId` (`item_...`) of the turn's user message.
    pub at_message_id: String,
    /// Workspace-relative paths (`/` separated) touched during that turn.
    pub files: BTreeMap<String, SnapshotFileView>,
}

/// One path's entry inside a [`SnapshotView`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotFileView {
    /// State of the path before the turn's first write (or a tombstone).
    pub pre: PreImage,
    /// Digest after the turn's last successful write; `None` means the path
    /// did not exist afterwards.
    pub post_sha256: Option<String>,
}

/// Line-level insert/delete counts between two text revisions.
///
/// Direction is `old` -> `new`: `insertions` are lines present in `new` only,
/// `deletions` lines present in `old` only. Used for
/// `session.snapshot.change_stats` and for the plan's confirmation preview.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LineDiffStats {
    pub insertions: u64,
    pub deletions: u64,
}

/// Line-level diff statistics for a text edit.
///
/// `old` is the content before the write and `new` the content after it (the
/// same direction as a snapshot's pre-image -> captured content). Returns
/// `None` when either side is larger than 4 MiB or is not valid UTF-8; the
/// caller should report the statistics as omitted. An empty `old` counts every
/// line of `new` as an insertion, and an empty `new` every line of `old` as a
/// deletion.
pub fn line_diff_stats(old: &[u8], new: &[u8]) -> Option<LineDiffStats> {
    if old.len() as u64 > MAX_DIFF_INPUT_BYTES || new.len() as u64 > MAX_DIFF_INPUT_BYTES {
        return None;
    }
    let old_text = std::str::from_utf8(old).ok()?;
    let new_text = std::str::from_utf8(new).ok()?;
    if old_text.is_empty() {
        return Some(LineDiffStats {
            insertions: count_lines(new_text),
            deletions: 0,
        });
    }
    let mut stats = LineDiffStats::default();
    for change in similar::TextDiff::from_lines(old_text, new_text).iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => stats.insertions = stats.insertions.saturating_add(1),
            similar::ChangeTag::Delete => stats.deletions = stats.deletions.saturating_add(1),
            similar::ChangeTag::Equal => {}
        }
    }
    Some(stats)
}

/// Planner knobs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanOptions {
    /// Ordinal of the target user message; the plan restores files to the
    /// state they had when that message was submitted.
    pub target_ordinal: u32,
}

/// A complete, side-effect-free restore plan.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RestorePlan {
    pub writes: Vec<PlannedWrite>,
    pub deletes: Vec<PlannedDelete>,
    pub skipped: Vec<PlanSkip>,
    /// Paths already at their target state (no action needed).
    pub unchanged: u64,
    /// Lines the restore adds (current disk content -> pre-image).
    pub insertions: u64,
    /// Lines the restore removes (current disk content -> pre-image).
    pub deletions: u64,
    /// `true` when at least one touched path could not be diffed (binary,
    /// oversized, deleted, unreadable).
    pub stats_omitted: bool,
    /// Non-fatal conditions the caller must surface before applying.
    pub warnings: Vec<PlanWarning>,
}

/// Write `target` (a pre-image blob) to `path`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedWrite {
    pub path: String,
    pub target: BlobRef,
    /// Disk state observed while planning; the applier CAS-checks it before
    /// writing. `None` means the path did not exist.
    pub current_sha256: Option<String>,
    /// The disk state no longer matches the agent's recorded last write.
    pub untracked_change: bool,
}

/// Delete `path` (it did not exist when the target message was submitted).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedDelete {
    pub path: String,
    pub untracked_change: bool,
}

/// A path the plan refuses to act on.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanSkip {
    pub path: String,
    /// Stable prefix: `path_escape: ...`, `not_regular`, `too_large`,
    /// `blob_unreadable: ...` or `io_error: ...`.
    pub reason: String,
}

/// A grouped, non-fatal planning condition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanWarning {
    /// Stable kind: `untracked_change`, `path_escape`, `not_regular` or
    /// `missing_blob`.
    pub kind: String,
    pub paths: Vec<String>,
}

/// Build the restore plan for `options.target_ordinal`.
///
/// Every path first touched at or after the target turn is reverted to its
/// pre-image from that earliest touch; paths never touched are not planned.
/// The disk is only read, never written.
pub fn build_plan(
    root: &Path,
    store: &RewindStore,
    snapshots: &[SnapshotView],
    options: PlanOptions,
) -> io::Result<RestorePlan> {
    let mut ordered: Vec<&SnapshotView> = snapshots
        .iter()
        .filter(|snapshot| snapshot.ordinal >= options.target_ordinal)
        .collect();
    ordered.sort_by_key(|snapshot| snapshot.ordinal);

    // The first snapshot touching a path supplies the target state (its `pre`);
    // the last snapshot touching it supplies the expected current state.
    let mut sources: BTreeMap<String, PreImage> = BTreeMap::new();
    let mut expected: BTreeMap<String, Option<String>> = BTreeMap::new();
    for snapshot in &ordered {
        for (path, entry) in &snapshot.files {
            sources
                .entry(path.clone())
                .or_insert_with(|| entry.pre.clone());
            expected.insert(path.clone(), entry.post_sha256.clone());
        }
    }

    let mut plan = RestorePlan::default();
    let mut untracked_paths = Vec::new();
    let mut escape_paths = Vec::new();
    let mut not_regular_paths = Vec::new();
    let mut missing_blob_paths = Vec::new();
    let mut excluded_paths = Vec::new();

    for (path, pre) in sources {
        let expected_sha256 = expected.get(&path).cloned().flatten();
        if let Some(excluded) = excluded_directory(&path) {
            plan.skipped.push(PlanSkip {
                path: path.clone(),
                reason: format!("excluded_path: {excluded}"),
            });
            excluded_paths.push(path);
            continue;
        }
        let abs = match resolve_inside(root, &path) {
            Ok(abs) => abs,
            Err(error) => {
                plan.skipped.push(PlanSkip {
                    path: path.clone(),
                    reason: skip_reason(&error),
                });
                escape_paths.push(path);
                continue;
            }
        };
        let current = match file_sha256(&abs) {
            Ok(current) => current,
            Err(error) => {
                plan.skipped.push(PlanSkip {
                    path: path.clone(),
                    reason: skip_reason(&error),
                });
                not_regular_paths.push(path);
                continue;
            }
        };

        match pre {
            PreImage::Missing => {
                if current.is_none() {
                    plan.unchanged += 1;
                    continue;
                }
                let untracked_change = current.as_deref() != expected_sha256.as_deref();
                if untracked_change {
                    untracked_paths.push(path.clone());
                }
                plan.deletes.push(PlannedDelete {
                    path,
                    untracked_change,
                });
                // Deleting lines can never be rendered as a textual diff.
                plan.stats_omitted = true;
            }
            PreImage::Blob(blob) => {
                // Validate the blob up front: the plan must not promise a write
                // it cannot perform, and the diff needs the pre-image bytes.
                let pre_bytes = match store.read_blob(&blob) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        plan.skipped.push(PlanSkip {
                            path: path.clone(),
                            reason: format!("blob_unreadable: {error}"),
                        });
                        missing_blob_paths.push(path);
                        continue;
                    }
                };
                if current.as_deref() == Some(blob.sha256.as_str()) {
                    plan.unchanged += 1;
                    continue;
                }
                let untracked_change = current.as_deref() != expected_sha256.as_deref();
                if untracked_change {
                    untracked_paths.push(path.clone());
                }
                record_text_stats(&mut plan, &pre_bytes, current.as_deref(), &abs);
                plan.writes.push(PlannedWrite {
                    path,
                    target: blob,
                    current_sha256: current,
                    untracked_change,
                });
            }
        }
    }

    push_warning(&mut plan, "untracked_change", untracked_paths);
    push_warning(&mut plan, "path_escape", escape_paths);
    push_warning(&mut plan, "not_regular", not_regular_paths);
    push_warning(&mut plan, "missing_blob", missing_blob_paths);
    push_warning(&mut plan, "excluded_path", excluded_paths);
    Ok(plan)
}

fn push_warning(plan: &mut RestorePlan, kind: &str, paths: Vec<String>) {
    if !paths.is_empty() {
        plan.warnings.push(PlanWarning {
            kind: kind.to_string(),
            paths,
        });
    }
}

pub(crate) fn skip_reason(error: &io::Error) -> String {
    match crate::rewind_error(error) {
        Some(RewindError::PathEscape { reason, .. }) => format!("path_escape: {reason}"),
        Some(RewindError::NotRegular { .. }) => "not_regular".to_string(),
        Some(RewindError::TooLarge { .. }) => "too_large".to_string(),
        Some(other) => format!("invalid: {other}"),
        None => format!("io_error: {error}"),
    }
}

/// Accumulate `+/-` line statistics for the restore direction
/// (old = current disk content, new = pre-image).
fn record_text_stats(
    plan: &mut RestorePlan,
    pre_bytes: &[u8],
    current_sha256: Option<&str>,
    abs: &Path,
) {
    if pre_bytes.len() as u64 > MAX_DIFF_INPUT_BYTES {
        plan.stats_omitted = true;
        return;
    }
    let Ok(pre_text) = std::str::from_utf8(pre_bytes) else {
        plan.stats_omitted = true;
        return;
    };
    let current_bytes = match current_sha256 {
        // Creating a file: every pre-image line is an insertion.
        None => {
            plan.insertions = plan.insertions.saturating_add(count_lines(pre_text));
            return;
        }
        Some(_) => match read_regular_file(abs, MAX_DIFF_INPUT_BYTES) {
            Ok(BoundedFile::Regular { bytes, .. }) => bytes,
            _ => {
                plan.stats_omitted = true;
                return;
            }
        },
    };
    match line_diff_stats(&current_bytes, pre_bytes) {
        Some(stats) => {
            plan.insertions = plan.insertions.saturating_add(stats.insertions);
            plan.deletions = plan.deletions.saturating_add(stats.deletions);
        }
        None => plan.stats_omitted = true,
    }
}

fn count_lines(text: &str) -> u64 {
    if text.is_empty() {
        0
    } else {
        text.lines().count() as u64
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::store::{BlobEncoding, BlobRef, RewindStore};

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        store: RewindStore,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        fs::create_dir_all(&root).expect("workspace");
        let store = RewindStore::open(&temp.path().join("sessions/s1")).expect("store");
        Fixture {
            _temp: temp,
            root,
            store,
        }
    }

    impl Fixture {
        fn blob(&self, bytes: &[u8]) -> BlobRef {
            self.store.put_bytes(bytes, None).expect("put blob")
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent");
            }
            fs::write(path, bytes).expect("write workspace file");
        }

        fn plan(&self, snapshots: &[SnapshotView], target: u32) -> RestorePlan {
            build_plan(
                &self.root,
                &self.store,
                snapshots,
                PlanOptions {
                    target_ordinal: target,
                },
            )
            .expect("plan")
        }
    }

    fn sha(fixture: &Fixture, bytes: &[u8]) -> String {
        fixture.blob(bytes).sha256
    }

    fn snapshot(
        ordinal: u32,
        files: impl IntoIterator<Item = (&'static str, PreImage, Option<String>)>,
    ) -> SnapshotView {
        let mut map = BTreeMap::new();
        for (path, pre, post) in files {
            map.insert(
                path.to_string(),
                SnapshotFileView {
                    pre,
                    post_sha256: post,
                },
            );
        }
        SnapshotView {
            ordinal,
            at_message_id: format!("item_{ordinal}"),
            files: map,
        }
    }

    #[test]
    fn uses_earliest_pre_image_per_path() {
        let fixture = fixture();
        let current = b"version 3";
        fixture.write("src/a.rs", current);
        let pre1 = fixture.blob(b"version 1");
        let pre2 = fixture.blob(b"version 2");
        let snapshots = [
            snapshot(
                1,
                [(
                    "src/a.rs",
                    PreImage::Blob(pre1.clone()),
                    Some(fixture.blob(b"version 2").sha256.clone()),
                )],
            ),
            snapshot(
                2,
                [(
                    "src/a.rs",
                    PreImage::Blob(pre2.clone()),
                    Some(sha(&fixture, current)),
                )],
            ),
        ];

        let plan = fixture.plan(&snapshots, 1);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.writes[0].target.sha256, pre1.sha256);
        assert_eq!(plan.writes[0].path, "src/a.rs");
        assert_eq!(
            plan.writes[0].current_sha256.as_deref(),
            Some(sha(&fixture, current).as_str())
        );
        assert!(!plan.writes[0].untracked_change);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        // Targeting turn 2 uses the second pre-image instead.
        let plan = fixture.plan(&snapshots, 2);
        assert_eq!(plan.writes[0].target.sha256, pre2.sha256);
    }

    #[test]
    fn tombstone_deletes_files_created_after_target() {
        let fixture = fixture();
        fixture.write("new.txt", b"created by the agent");
        let snapshots = [snapshot(
            2,
            [(
                "new.txt",
                PreImage::Missing,
                Some(sha(&fixture, b"created by the agent")),
            )],
        )];

        let plan = fixture.plan(&snapshots, 2);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.deletes.len(), 1);
        assert_eq!(plan.deletes[0].path, "new.txt");
        assert!(!plan.deletes[0].untracked_change);
        assert!(plan.stats_omitted);
        assert!(plan.unchanged == 0);
    }

    #[test]
    fn tombstone_recreates_files_deleted_after_target() {
        let fixture = fixture();
        let original = fixture.blob(b"original content");
        let snapshots = [snapshot(
            3,
            [("gone.txt", PreImage::Blob(original.clone()), None)],
        )];

        let plan = fixture.plan(&snapshots, 3);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.writes[0].target.sha256, original.sha256);
        assert!(plan.writes[0].current_sha256.is_none());
        assert!(
            !plan.writes[0].untracked_change,
            "a missing file matches the recorded `None` post"
        );
    }

    #[test]
    fn already_restored_paths_are_unchanged() {
        let fixture = fixture();
        fixture.write("same.txt", b"already here");
        let blob = fixture.blob(b"already here");
        let snapshots = [snapshot(
            4,
            [(
                "same.txt",
                PreImage::Blob(blob),
                Some(sha(&fixture, b"already here")),
            )],
        )];

        let plan = fixture.plan(&snapshots, 4);
        assert_eq!(plan.unchanged, 1);
        assert!(plan.writes.is_empty());
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn flags_untracked_changes_but_still_plans_the_write() {
        let fixture = fixture();
        fixture.write("user.txt", b"user edited this");
        let pre = fixture.blob(b"agent original");
        let snapshots = [snapshot(
            5,
            [(
                "user.txt",
                PreImage::Blob(pre.clone()),
                // The agent's last recorded write differs from the disk.
                Some(sha(&fixture, b"agent final")),
            )],
        )];

        let plan = fixture.plan(&snapshots, 5);
        assert_eq!(plan.writes.len(), 1);
        assert!(plan.writes[0].untracked_change);
        let warning = plan
            .warnings
            .iter()
            .find(|warning| warning.kind == "untracked_change")
            .expect("untracked warning");
        assert_eq!(warning.paths, vec!["user.txt".to_string()]);
    }

    #[test]
    fn line_diff_stats_counts_replacements_and_pure_edits() {
        // Equal-size replacement: one line swapped for another.
        assert_eq!(
            line_diff_stats(b"a\nb\nc\n", b"a\nx\nc\n"),
            Some(LineDiffStats {
                insertions: 1,
                deletions: 1
            })
        );
        // Pure insertion (no deletion).
        assert_eq!(
            line_diff_stats(b"a\nc\n", b"a\nb\nc\n"),
            Some(LineDiffStats {
                insertions: 1,
                deletions: 0
            })
        );
        // Pure deletion (no insertion).
        assert_eq!(
            line_diff_stats(b"a\nb\nc\n", b"a\nc\n"),
            Some(LineDiffStats {
                insertions: 0,
                deletions: 1
            })
        );
        assert_eq!(
            line_diff_stats(b"same\n", b"same\n"),
            Some(LineDiffStats::default())
        );
    }

    #[test]
    fn line_diff_stats_rejects_binary_and_oversized_inputs() {
        let binary = [0x00u8, 0xff, 0xfe];
        assert_eq!(line_diff_stats(&binary, b"text"), None);
        assert_eq!(line_diff_stats(b"text", &binary), None);
        let oversized = vec![b'a'; (MAX_DIFF_INPUT_BYTES + 1) as usize];
        assert_eq!(line_diff_stats(&oversized, b"text"), None);
        assert_eq!(line_diff_stats(b"text", &oversized), None);
        assert_eq!(
            line_diff_stats(b"", b""),
            Some(LineDiffStats {
                insertions: 0,
                deletions: 0
            })
        );
    }

    #[test]
    fn line_diff_stats_treats_empty_old_as_insertions() {
        assert_eq!(
            line_diff_stats(b"", b"one\ntwo\nthree"),
            Some(LineDiffStats {
                insertions: 3,
                deletions: 0
            })
        );
        assert_eq!(
            line_diff_stats(b"one\ntwo\n", b""),
            Some(LineDiffStats {
                insertions: 0,
                deletions: 2
            })
        );
    }

    #[test]
    fn snapshots_before_the_target_are_ignored() {
        let fixture = fixture();
        let pre = fixture.blob(b"old");
        let snapshots = [snapshot(
            1,
            [("old.txt", PreImage::Blob(pre), Some(sha(&fixture, b"old")))],
        )];
        let plan = fixture.plan(&snapshots, 2);
        assert!(plan.writes.is_empty());
        assert!(plan.deletes.is_empty());
        assert_eq!(plan.unchanged, 0);
    }

    #[test]
    fn delete_of_already_missing_file_is_unchanged() {
        let fixture = fixture();
        let snapshots = [snapshot(5, [("absent.txt", PreImage::Missing, None)])];
        let plan = fixture.plan(&snapshots, 5);
        assert_eq!(plan.unchanged, 1);
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn computes_restore_direction_line_stats() {
        let fixture = fixture();
        fixture.write("a.txt", b"a\nb\nc\n");
        let pre = fixture.blob(b"a\nc\nd\n");
        let snapshots = [snapshot(
            6,
            [(
                "a.txt",
                PreImage::Blob(pre),
                Some(sha(&fixture, b"a\nb\nc\n")),
            )],
        )];

        let plan = fixture.plan(&snapshots, 6);
        assert_eq!((plan.insertions, plan.deletions), (1, 1));
        assert!(!plan.stats_omitted);
    }

    #[test]
    fn created_files_count_lines_as_insertions() {
        let fixture = fixture();
        let pre = fixture.blob(b"line one\nline two\n");
        let snapshots = [snapshot(
            7,
            [(
                "created.txt",
                PreImage::Blob(pre),
                Some(sha(&fixture, b"line one\nline two\n")),
            )],
        )];
        let plan = fixture.plan(&snapshots, 7);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(plan.insertions, 2);
        assert_eq!(plan.deletions, 0);
        assert!(!plan.stats_omitted);
    }

    #[test]
    fn binary_content_marks_stats_omitted() {
        let fixture = fixture();
        let binary: Vec<u8> = vec![0x00, 0xff, 0xfe, 0x80, 0x01];
        let pre = fixture.blob(&binary);
        let snapshots = [snapshot(
            8,
            [(
                "image.png",
                PreImage::Blob(pre),
                Some(sha(&fixture, &binary)),
            )],
        )];
        let plan = fixture.plan(&snapshots, 8);
        assert_eq!(plan.writes.len(), 1);
        assert!(plan.stats_omitted);
    }

    #[test]
    fn missing_blob_is_skipped_with_warning() {
        let fixture = fixture();
        let missing = BlobRef {
            sha256: "7".repeat(64),
            size: 3,
            mode: None,
            encoding: BlobEncoding::Raw,
        };
        let snapshots = [snapshot(9, [("lost.txt", PreImage::Blob(missing), None)])];
        let plan = fixture.plan(&snapshots, 9);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].reason.starts_with("blob_unreadable:"));
        let warning = plan
            .warnings
            .iter()
            .find(|warning| warning.kind == "missing_blob")
            .expect("missing blob warning");
        assert_eq!(warning.paths, vec!["lost.txt".to_string()]);
    }

    #[test]
    fn skips_paths_inside_excluded_directories() {
        let fixture = fixture();
        let pre = fixture.blob(b"git hooks");
        let snapshots = [
            snapshot(
                1,
                [(".git/hooks/pre-commit", PreImage::Blob(pre.clone()), None)],
            ),
            snapshot(
                2,
                [(
                    "node_modules/pkg/index.js",
                    PreImage::Blob(pre.clone()),
                    None,
                )],
            ),
            snapshot(3, [("src/target/generated.rs", PreImage::Blob(pre), None)]),
        ];
        let plan = fixture.plan(&snapshots, 0);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.skipped.len(), 3);
        assert!(
            plan.skipped
                .iter()
                .all(|skip| skip.reason.starts_with("excluded_path:"))
        );
        let warning = plan
            .warnings
            .iter()
            .find(|warning| warning.kind == "excluded_path")
            .expect("excluded path warning");
        assert_eq!(warning.paths.len(), 3);
    }

    #[test]
    fn refuses_paths_that_escape_the_workspace() {
        let fixture = fixture();
        let pre = fixture.blob(b"evil");
        let snapshots = [
            snapshot(1, [("../outside.txt", PreImage::Blob(pre.clone()), None)]),
            snapshot(2, [("/etc/passwd", PreImage::Blob(pre), None)]),
        ];
        let plan = fixture.plan(&snapshots, 0);
        assert!(plan.writes.is_empty(), "{:?}", plan.writes);
        assert_eq!(plan.skipped.len(), 2);
        assert!(
            plan.skipped
                .iter()
                .all(|skip| skip.reason.starts_with("path_escape:"))
        );
        let warning = plan
            .warnings
            .iter()
            .find(|warning| warning.kind == "path_escape")
            .expect("path escape warning");
        assert_eq!(warning.paths.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_paths_crossing_symlinked_directories() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret.txt"), b"secret").expect("write");
        symlink(outside.path(), fixture.root.join("link")).expect("symlink");
        let pre = fixture.blob(b"replacement");
        let snapshots = [snapshot(
            2,
            [("link/secret.txt", PreImage::Blob(pre), None)],
        )];

        let plan = fixture.plan(&snapshots, 2);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].reason.starts_with("path_escape:"));
    }

    #[test]
    fn plan_never_writes_to_disk() {
        let fixture = fixture();
        fixture.write("untouched.txt", b"content");
        let pre = fixture.blob(b"restored");
        let snapshots = [snapshot(
            1,
            [(
                "untouched.txt",
                PreImage::Blob(pre),
                Some(sha(&fixture, b"content")),
            )],
        )];
        let plan = fixture.plan(&snapshots, 1);
        assert_eq!(plan.writes.len(), 1);
        assert_eq!(
            fs::read(fixture.root.join("untouched.txt")).expect("read"),
            b"content"
        );
    }
}
