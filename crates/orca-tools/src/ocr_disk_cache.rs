//! Recognized document text kept on disk under `ORCA_HOME/ocr_cache`, keyed
//! by the file's content, so that a copy of a file, a subagent in another
//! process, or a later session does not pay for the recognition again.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::deepseek_web::OcrResult;

#[cfg(not(test))]
const CACHE_SUBDIR: &str = "ocr_cache";
/// The most the cache keeps on disk. The least recently used entries go first.
const MAX_DISK_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// The key of a recognition of `file_bytes`. It comes from the content, not
/// the path, so a copy of a file hits and a changed file misses.
pub(crate) fn content_key(file_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"orca-ocr-v1\npurpose:text_recognition\n");
    hasher.update(file_bytes);
    orca_core::hex::lower(&hasher.finalize())
}

/// The recognition stored under `key`, if a readable one is there.
pub(crate) fn load(key: &str) -> Option<OcrResult> {
    load_in(&cache_dir()?, key)
}

/// Keeps `result` under `key`. A failure to write only means the next read
/// pays again, so it is ignored.
pub(crate) fn store(key: &str, result: &OcrResult) {
    if let Some(dir) = cache_dir() {
        store_in(&dir, key, result, MAX_DISK_CACHE_BYTES);
    }
}

pub(crate) fn entry_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

fn load_in(dir: &Path, key: &str) -> Option<OcrResult> {
    let path = entry_path(dir, key);
    let result = serde_json::from_slice::<OcrResult>(&fs::read(&path).ok()?).ok()?;
    // A hit counts as a use, so pruning keeps the entry.
    if let Ok(file) = fs::File::options().write(true).open(&path) {
        let _ = file.set_modified(SystemTime::now());
    }
    Some(result)
}

fn store_in(dir: &Path, key: &str, result: &OcrResult, max_bytes: u64) {
    let Ok(json) = serde_json::to_vec(result) else {
        return;
    };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    // Written beside the entry and renamed into place, so that a reader in
    // another process sees a whole entry or none.
    let temp = dir.join(format!(".{key}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = fs::File::create(&temp).and_then(|mut file| file.write_all(&json));
    if written.is_err() || fs::rename(&temp, entry_path(dir, key)).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    prune(dir, max_bytes);
}

/// Removes the least recently used entries until the rest fit in
/// `max_bytes`.
fn prune(dir: &Path, max_bytes: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".json") && !name.starts_with('.'))
        })
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                (
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    metadata.len(),
                    entry.path(),
                )
            })
        })
        .collect::<Vec<_>>();
    let mut total = files.iter().map(|(_, len, _)| len).sum::<u64>();
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

#[cfg(not(test))]
fn cache_dir() -> Option<PathBuf> {
    orca_core::home::orca_home().map(|home| home.join(CACHE_SUBDIR))
}

#[cfg(test)]
thread_local! {
    static TEST_ROOT: tempfile::TempDir = tempfile::tempdir().expect("an isolated OCR cache");
    static TEST_ROOT_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Each test thread gets its own cache, unless it names one.
#[cfg(test)]
fn cache_dir() -> Option<PathBuf> {
    TEST_ROOT_OVERRIDE
        .with(|root| root.borrow().clone())
        .or_else(|| Some(TEST_ROOT.with(|root| root.path().join("ocr_cache"))))
}

/// Points this thread's cache at `root`, or back at its own when `None`.
#[cfg(test)]
pub(crate) fn set_test_cache_root(root: Option<PathBuf>) {
    TEST_ROOT_OVERRIDE.with(|cell| *cell.borrow_mut() = root);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::deepseek_web::{OcrPage, OcrResult, OcrUpload};

    fn sample_result() -> OcrResult {
        OcrResult {
            pages: vec![OcrPage {
                number: 1,
                text: "cached text".to_string(),
                complete: true,
            }],
            page_count: Some(1),
        }
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .expect("read the cache dir")
            .map(|entry| {
                entry
                    .expect("a cache entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn prune_keeps_the_newest_entries_under_the_cap() {
        let dir = tempfile::tempdir().expect("a cache dir");
        let oldest = SystemTime::now() - Duration::from_secs(100);
        for (age, name) in ["a", "b", "c", "d"].into_iter().enumerate() {
            let path = dir.path().join(format!("{name}.json"));
            fs::write(&path, [b'x'; 10]).expect("write an entry");
            fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(oldest + Duration::from_secs(age as u64 * 10)))
                .expect("date the entry");
        }

        prune(dir.path(), 25);

        assert_eq!(names(dir.path()), ["c.json", "d.json"]);
    }

    #[test]
    fn concurrent_stores_of_one_entry_leave_it_whole() {
        let dir = tempfile::tempdir().expect("a cache dir");
        let result = sample_result();

        let writers = (0..8)
            .map(|_| {
                let dir = dir.path().to_path_buf();
                let result = result.clone();
                std::thread::spawn(move || store_in(&dir, "key", &result, MAX_DISK_CACHE_BYTES))
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().expect("a writer");
        }

        assert_eq!(load_in(dir.path(), "key"), Some(result));
        assert_eq!(names(dir.path()), ["key.json"], "no temp file is left");
    }

    #[test]
    fn a_hit_counts_as_a_use() {
        let dir = tempfile::tempdir().expect("a cache dir");
        store_in(dir.path(), "key", &sample_result(), MAX_DISK_CACHE_BYTES);
        let path = entry_path(dir.path(), "key");
        let old = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(old))
            .expect("age the entry");

        assert!(load_in(dir.path(), "key").is_some());

        let touched = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("mtime");
        assert!(touched > old + Duration::from_secs(3000), "{touched:?}");
    }

    #[test]
    fn the_content_key_comes_from_the_uploaded_bytes() {
        let original = b"%PDF-1.4 the same bytes";
        let upload = OcrUpload::read("a.pdf", &original[..], original.len() as u64, 1 << 20)
            .expect("an upload");

        assert_eq!(content_key(upload.file_bytes()), content_key(original));
        assert_ne!(content_key(original), content_key(b"%PDF-1.4 other bytes"));
        assert_eq!(content_key(original).len(), 64);
    }
}
