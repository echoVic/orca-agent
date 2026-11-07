//! Per-file unified-diff previews for the `/rewind` confirmation page.
//!
//! The TUI shows what a restore will change before the user confirms. The plan
//! already carries line counts, but a preview needs the actual text, which only
//! the store knows. This module is read-only: it never touches the workspace
//! and is capped both by file count and by rendered diff size so a pathological
//! repository cannot flood the UI.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::MAX_DIFF_INPUT_BYTES;
use crate::error::RewindError;
use crate::path::{BoundedFile, read_regular_file, resolve_inside};
use crate::plan::{RestorePlan, line_diff_stats};
use crate::store::{BlobRef, RewindStore};

/// Context lines shown around each change in the generated unified diff.
const CONTEXT_RADIUS: usize = 3;

/// One file's unified-diff preview, ready to render.
///
/// `diff` is a standard unified diff with `---`/`+++` headers and `@@` hunks in
/// the direction of the *restore*: the previous side is the current disk
/// content (`a/<path>`, or `/dev/null` when the restore creates the file) and
/// the new side is the planned content (`b/<path>`, or `/dev/null` when the
/// restore deletes it). Identical content yields an empty diff.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileDiffPreview {
    pub path: String,
    pub diff: String,
    /// The rendered diff exceeded the per-file byte cap and was cut at a line
    /// boundary; its counts still describe the full diff.
    pub truncated: bool,
    /// Either side was not valid UTF-8, or was larger than 4 MiB; `diff` is
    /// empty and the counts are zero.
    pub binary: bool,
    pub insertions: u64,
    pub deletions: u64,
}

/// Build unified-diff previews for a restore plan.
///
/// Iterates `plan.writes` in plan order and then `plan.deletes`, stopping after
/// `max_files` entries. Every rendered diff is capped at
/// `max_diff_bytes_per_file` **rendered text** bytes, cut back to the last
/// complete line (never splitting a UTF-8 character). Files whose content is
/// not valid UTF-8 or is larger than 4 MiB are reported as `binary` with an
/// empty diff and zero counts.
///
/// Preview generation is best-effort: a pre-image blob that cannot be read
/// (for example evicted by GC after planning) skips that file, so the result
/// may be shorter than the plan. Unsafe paths and workspace read failures are
/// still I/O errors; the caller can drop previews and keep the plan.
pub fn plan_diff_previews(
    root: &Path,
    store: &RewindStore,
    plan: &RestorePlan,
    max_files: usize,
    max_diff_bytes_per_file: usize,
) -> io::Result<Vec<FileDiffPreview>> {
    if max_files == 0 {
        return Ok(Vec::new());
    }
    let mut previews = Vec::with_capacity(max_files.min(plan.writes.len() + plan.deletes.len()));
    for write in &plan.writes {
        if previews.len() >= max_files {
            return Ok(previews);
        }
        if let Some(preview) = write_preview(
            root,
            store,
            &write.path,
            &write.target,
            max_diff_bytes_per_file,
        )? {
            previews.push(preview);
        }
    }
    for delete in &plan.deletes {
        if previews.len() >= max_files {
            break;
        }
        previews.push(delete_preview(root, &delete.path, max_diff_bytes_per_file)?);
    }
    Ok(previews)
}

/// What the workspace path currently holds, bounded like a diff input.
enum CurrentFile {
    Missing,
    Regular(Vec<u8>),
    TooLarge,
}

fn read_current(path: &Path) -> io::Result<CurrentFile> {
    match read_regular_file(path, MAX_DIFF_INPUT_BYTES)? {
        BoundedFile::Missing => Ok(CurrentFile::Missing),
        BoundedFile::Regular { bytes, .. } => Ok(CurrentFile::Regular(bytes)),
        BoundedFile::TooLarge { .. } => Ok(CurrentFile::TooLarge),
        BoundedFile::NotRegular => Err(RewindError::NotRegular {
            path: path.to_path_buf(),
        }
        .into()),
    }
}

fn write_preview(
    root: &Path,
    store: &RewindStore,
    relative: &str,
    target: &BlobRef,
    max_diff_bytes: usize,
) -> io::Result<Option<FileDiffPreview>> {
    let abs = resolve_inside(root, relative)?;
    let current = read_current(&abs)?;
    // The pre-image is what will be written, so it is the "new" side. A blob
    // above the diff input cap cannot be rendered as text; skip decoding it.
    if target.size > MAX_DIFF_INPUT_BYTES || matches!(current, CurrentFile::TooLarge) {
        return Ok(Some(binary_preview(relative)));
    }
    let Ok(pre) = store.read_blob(target) else {
        // A blob can disappear between planning and previewing (retention GC).
        // The preview is cosmetic, so skip the file instead of failing the
        // whole confirmation page.
        return Ok(None);
    };
    Ok(Some(match current {
        CurrentFile::Missing => render_preview(relative, b"", true, &pre, false, max_diff_bytes),
        CurrentFile::Regular(bytes) => {
            render_preview(relative, &bytes, false, &pre, false, max_diff_bytes)
        }
        CurrentFile::TooLarge => binary_preview(relative),
    }))
}

fn delete_preview(
    root: &Path,
    relative: &str,
    max_diff_bytes: usize,
) -> io::Result<FileDiffPreview> {
    let abs = resolve_inside(root, relative)?;
    match read_current(&abs)? {
        CurrentFile::Regular(bytes) => Ok(render_preview(
            relative,
            &bytes,
            false,
            b"",
            true,
            max_diff_bytes,
        )),
        // Already gone: the plan's delete is a no-op and shows no changes.
        CurrentFile::Missing => Ok(render_preview(
            relative,
            b"",
            true,
            b"",
            true,
            max_diff_bytes,
        )),
        CurrentFile::TooLarge => Ok(binary_preview(relative)),
    }
}

fn binary_preview(relative: &str) -> FileDiffPreview {
    FileDiffPreview {
        path: relative.to_string(),
        binary: true,
        ..FileDiffPreview::default()
    }
}

fn render_preview(
    relative: &str,
    old: &[u8],
    old_missing: bool,
    new: &[u8],
    new_missing: bool,
    max_diff_bytes: usize,
) -> FileDiffPreview {
    let (Ok(old_text), Ok(new_text)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return binary_preview(relative);
    };
    // Size and UTF-8 are already validated for both sides, so the helper
    // cannot return `None` here; the counts describe the full diff even when
    // the rendered text is truncated below.
    let stats = line_diff_stats(old, new).unwrap_or_default();
    let old_header = if old_missing {
        "/dev/null".to_string()
    } else {
        format!("a/{relative}")
    };
    let new_header = if new_missing {
        "/dev/null".to_string()
    } else {
        format!("b/{relative}")
    };
    let full = similar::TextDiff::from_lines(old_text, new_text)
        .unified_diff()
        .context_radius(CONTEXT_RADIUS)
        .header(&old_header, &new_header)
        .to_string();
    let (diff, truncated) = truncate_at_line_boundary(full, max_diff_bytes);
    FileDiffPreview {
        path: relative.to_string(),
        diff,
        truncated,
        binary: false,
        insertions: stats.insertions,
        deletions: stats.deletions,
    }
}

/// Cut `diff` down to at most `max_bytes`, back to the last complete line and
/// never inside a UTF-8 character. Returns the text and whether it was cut.
fn truncate_at_line_boundary(diff: String, max_bytes: usize) -> (String, bool) {
    if diff.len() <= max_bytes {
        return (diff, false);
    }
    if max_bytes == 0 {
        return (String::new(), true);
    }
    let mut end = max_bytes.min(diff.len());
    while end > 0 && !diff.is_char_boundary(end) {
        end -= 1;
    }
    let cut = diff[..end].rfind('\n').map(|index| index + 1).unwrap_or(0);
    let mut truncated = diff;
    truncated.truncate(cut);
    (truncated, true)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::plan::{PlannedDelete, PlannedWrite, RestorePlan};
    use crate::store::BlobRef;

    const KIB: usize = 1024;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        store: RewindStore,
    }

    impl Fixture {
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
            fs::write(path, bytes).expect("write workspace file");
        }

        fn previews(
            &self,
            plan: &RestorePlan,
            max_files: usize,
            max_diff_bytes_per_file: usize,
        ) -> Vec<FileDiffPreview> {
            plan_diff_previews(
                &self.root,
                &self.store,
                plan,
                max_files,
                max_diff_bytes_per_file,
            )
            .expect("previews")
        }
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

    fn write_op(plan: &mut RestorePlan, blob: BlobRef, path: &str, current: Option<String>) {
        plan.writes.push(PlannedWrite {
            path: path.to_string(),
            target: blob,
            current_sha256: current,
            untracked_change: false,
        });
    }

    fn delete_op(plan: &mut RestorePlan, path: &str) {
        plan.deletes.push(PlannedDelete {
            path: path.to_string(),
            untracked_change: false,
        });
    }

    #[test]
    fn previews_a_modification_in_restore_direction() {
        let fixture = fixture();
        let current = b"one\ntwo\nthree\n";
        fixture.write("src/a.txt", current);
        let pre = fixture.blob(b"one\nTWO\nthree\n");
        let mut plan = RestorePlan::default();
        write_op(&mut plan, pre, "src/a.txt", Some(fixture.sha(current)));

        let previews = fixture.previews(&plan, 10, 64 * KIB);
        assert_eq!(previews.len(), 1);
        let preview = &previews[0];
        assert_eq!(preview.path, "src/a.txt");
        assert!(!preview.binary);
        assert!(!preview.truncated);
        assert!(preview.diff.contains("--- a/src/a.txt\n"));
        assert!(preview.diff.contains("+++ b/src/a.txt\n"));
        assert!(preview.diff.contains("@@ -1,3 +1,3 @@"));
        assert!(preview.diff.contains("-two\n"));
        assert!(preview.diff.contains("+TWO\n"));
        assert_eq!((preview.insertions, preview.deletions), (1, 1));
    }

    #[test]
    fn previews_a_new_file_with_dev_null_header() {
        let fixture = fixture();
        let pre = fixture.blob(b"hello\nworld\n");
        let mut plan = RestorePlan::default();
        write_op(&mut plan, pre, "new.txt", None);

        let previews = fixture.previews(&plan, 10, 64 * KIB);
        assert_eq!(previews.len(), 1);
        let preview = &previews[0];
        assert!(!preview.binary);
        assert!(preview.diff.contains("--- /dev/null\n"));
        assert!(preview.diff.contains("+++ b/new.txt\n"));
        assert!(preview.diff.contains("+hello\n"));
        assert!(preview.diff.contains("+world\n"));
        assert_eq!((preview.insertions, preview.deletions), (2, 0));
    }

    #[test]
    fn previews_a_deletion_with_dev_null_header() {
        let fixture = fixture();
        fixture.write("gone.txt", b"bye\nnow\n");
        let mut plan = RestorePlan::default();
        delete_op(&mut plan, "gone.txt");

        let previews = fixture.previews(&plan, 10, 64 * KIB);
        assert_eq!(previews.len(), 1);
        let preview = &previews[0];
        assert!(!preview.binary);
        assert!(preview.diff.contains("--- a/gone.txt\n"));
        assert!(preview.diff.contains("+++ /dev/null\n"));
        assert!(preview.diff.contains("-bye\n"));
        assert!(preview.diff.contains("-now\n"));
        assert_eq!((preview.insertions, preview.deletions), (0, 2));
    }

    #[test]
    fn marks_non_utf8_and_oversized_content_binary() {
        let fixture = fixture();
        // Current file is not UTF-8.
        fixture.write("blob.bin", &[0x00, 0xff, 0xfe, 0x01]);
        let text_pre = fixture.blob(b"plain text\n");
        let mut plan = RestorePlan::default();
        write_op(&mut plan, text_pre, "blob.bin", Some("0".repeat(64)));

        // Pre-image itself is over the 4 MiB diff cap.
        let oversized_pre = fixture.blob(&vec![b'a'; (MAX_DIFF_INPUT_BYTES + 1) as usize]);
        write_op(&mut plan, oversized_pre, "big.txt", None);

        // Deletion of a binary file.
        fixture.write("binary_del.bin", &[0x80, 0x81, 0x82]);
        delete_op(&mut plan, "binary_del.bin");

        let previews = fixture.previews(&plan, 10, 64 * KIB);
        assert_eq!(previews.len(), 3);
        for preview in &previews {
            assert!(preview.binary, "{} should be binary", preview.path);
            assert!(preview.diff.is_empty(), "{}", preview.path);
            assert_eq!((preview.insertions, preview.deletions), (0, 0));
            assert!(!preview.truncated);
        }
    }

    #[test]
    fn skips_files_whose_blob_disappeared() {
        let fixture = fixture();
        fixture.write("a.txt", b"current\n");
        let mut plan = RestorePlan::default();
        write_op(
            &mut plan,
            BlobRef {
                sha256: "8".repeat(64),
                size: 7,
                mode: None,
                encoding: crate::store::BlobEncoding::Raw,
            },
            "a.txt",
            Some(fixture.sha(b"current\n")),
        );
        // The missing blob is skipped, not an error: previews may be shorter
        // than the plan because they are cosmetic.
        let previews = fixture.previews(&plan, 10, 64 * KIB);
        assert!(previews.is_empty());
    }

    #[test]
    fn truncates_a_long_diff_at_a_utf8_line_boundary() {
        let fixture = fixture();
        // Multi-byte characters make character-boundary handling observable.
        let current = "旧行一\n旧行二\n旧行三\n旧行四\n旧行五\n".repeat(8);
        let pre = "新行一\n新行二\n新行三\n新行四\n新行五\n".repeat(8);
        fixture.write("long.txt", current.as_bytes());
        let pre_blob = fixture.blob(pre.as_bytes());
        let mut plan = RestorePlan::default();
        write_op(
            &mut plan,
            pre_blob,
            "long.txt",
            Some(fixture.sha(current.as_bytes())),
        );

        let full = fixture.previews(&plan, 10, 1024 * KIB);
        assert!(!full[0].truncated);
        assert!(full[0].diff.len() > 120);

        let cap = 120;
        let previews = fixture.previews(&plan, 10, cap);
        let preview = &previews[0];
        assert!(preview.truncated);
        assert!(preview.diff.len() <= cap);
        assert!(preview.diff.is_empty() || preview.diff.ends_with('\n'));
        assert!(
            std::str::from_utf8(preview.diff.as_bytes()).is_ok(),
            "truncated diff must stay valid UTF-8"
        );
        // Counts always describe the full diff, not the truncated text.
        assert_eq!(preview.insertions, full[0].insertions);
        assert_eq!(preview.deletions, full[0].deletions);

        // A zero cap drops everything but still reports the truncation.
        let empty = fixture.previews(&plan, 10, 0);
        assert!(empty[0].truncated);
        assert!(empty[0].diff.is_empty());
    }

    #[test]
    fn empty_plan_and_max_files_cap() {
        let fixture = fixture();
        let empty = fixture.previews(&RestorePlan::default(), 10, 64 * KIB);
        assert!(empty.is_empty());

        fixture.write("a.txt", b"a\n");
        fixture.write("b.txt", b"b\n");
        fixture.write("c.txt", b"c\n");
        let mut plan = RestorePlan::default();
        write_op(
            &mut plan,
            fixture.blob(b"a2\n"),
            "a.txt",
            Some(fixture.sha(b"a\n")),
        );
        write_op(
            &mut plan,
            fixture.blob(b"b2\n"),
            "b.txt",
            Some(fixture.sha(b"b\n")),
        );
        delete_op(&mut plan, "c.txt");

        let two = fixture.previews(&plan, 2, 64 * KIB);
        assert_eq!(two.len(), 2);
        // Writes come first, in plan order; deletes are only reached afterwards.
        assert_eq!(two[0].path, "a.txt");
        assert_eq!(two[1].path, "b.txt");

        let all = fixture.previews(&plan, 10, 64 * KIB);
        assert_eq!(
            all.iter()
                .map(|preview| preview.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a.txt", "b.txt", "c.txt"]
        );

        assert!(fixture.previews(&plan, 0, 64 * KIB).is_empty());
    }
}
