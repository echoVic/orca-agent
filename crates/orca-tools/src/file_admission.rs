use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::ops::ControlFlow;
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use orca_core::retained_output::{
    RETAINED_OUTPUT_READ_CHUNK_BYTES, RetainedOutput, RetainedOutputSnapshot,
};
use orca_core::tool_types::{FileChangePreview, truncate_output};
use similar::TextDiff;

pub const MAX_EDIT_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DIFF_INPUT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DIFF_OUTPUT_BYTES: usize = 256 * 1024;
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);
const MICRO_COMPACTION_MARKER: &str = "\n[... tool output micro-compacted ...]\n";

#[derive(Debug)]
pub enum FileAdmissionError {
    Io(io::Error),
    NotRegularFile,
    TooLarge {
        observed_bytes: u64,
        max_bytes: u64,
    },
    GrewWhileReading {
        declared_bytes: u64,
        observed_bytes: u64,
    },
    InvalidUtf8,
    Cancelled,
}

impl FileAdmissionError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Io(error) if error.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for FileAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::NotRegularFile => formatter.write_str("not a regular file"),
            Self::TooLarge {
                observed_bytes,
                max_bytes,
            } => write!(
                formatter,
                "file is too large ({observed_bytes} bytes; maximum {max_bytes} bytes)"
            ),
            Self::GrewWhileReading {
                declared_bytes,
                observed_bytes,
            } => write!(
                formatter,
                "file grew while being read ({declared_bytes} to at least {observed_bytes} bytes)"
            ),
            Self::InvalidUtf8 => formatter.write_str("file does not contain valid UTF-8"),
            Self::Cancelled => formatter.write_str("file read cancelled"),
        }
    }
}

impl std::error::Error for FileAdmissionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for FileAdmissionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub struct FileStreamOutcome<T> {
    pub value: T,
    pub bytes_read: u64,
    pub declared_bytes: u64,
    pub reached_eof: bool,
}

pub fn stream_utf8_file<T, F>(
    path: &Path,
    initial: T,
    should_cancel: impl Fn() -> bool,
    on_chunk: F,
) -> Result<FileStreamOutcome<T>, FileAdmissionError>
where
    F: FnMut(&mut T, &[u8]) -> ControlFlow<()>,
{
    let (file, declared_bytes) = open_regular_file(path)?;
    stream_utf8_reader(file, declared_bytes, initial, should_cancel, on_chunk)
}

pub fn read_text_file_with_limit(
    path: &Path,
    max_bytes: usize,
    should_cancel: impl Fn() -> bool,
) -> Result<String, FileAdmissionError> {
    let (file, declared_bytes) = open_regular_file(path)?;
    let max_bytes = max_bytes as u64;
    if declared_bytes > max_bytes {
        return Err(FileAdmissionError::TooLarge {
            observed_bytes: declared_bytes,
            max_bytes,
        });
    }
    read_utf8_reader_with_limit(file, declared_bytes, max_bytes, should_cancel)
}

/// Opens the regular file at `path` without blocking on a FIFO or device,
/// and returns it with its length.
pub(crate) fn open_regular_file(path: &Path) -> Result<(File, u64), FileAdmissionError> {
    let path_metadata = path.symlink_metadata()?;
    if !path_metadata.is_file() && !path_metadata.file_type().is_symlink() {
        return Err(FileAdmissionError::NotRegularFile);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(FileAdmissionError::NotRegularFile);
    }
    Ok((file, metadata.len()))
}

fn read_utf8_reader_with_limit(
    mut reader: impl Read,
    declared_bytes: u64,
    max_bytes: u64,
    should_cancel: impl Fn() -> bool,
) -> Result<String, FileAdmissionError> {
    let initial_capacity = usize::try_from(declared_bytes.min(max_bytes)).unwrap_or(usize::MAX);
    let mut bytes = Vec::with_capacity(initial_capacity);
    let mut buffer = [0_u8; RETAINED_OUTPUT_READ_CHUNK_BYTES];
    loop {
        if should_cancel() {
            return Err(FileAdmissionError::Cancelled);
        }
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        let observed_bytes = (bytes.len() as u64).saturating_add(read as u64);
        if observed_bytes > max_bytes {
            return Err(FileAdmissionError::TooLarge {
                observed_bytes,
                max_bytes,
            });
        }
        if observed_bytes > declared_bytes {
            return Err(FileAdmissionError::GrewWhileReading {
                declared_bytes,
                observed_bytes,
            });
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    String::from_utf8(bytes).map_err(|_| FileAdmissionError::InvalidUtf8)
}

fn stream_utf8_reader<T, F>(
    mut reader: impl Read,
    declared_bytes: u64,
    mut value: T,
    should_cancel: impl Fn() -> bool,
    mut on_chunk: F,
) -> Result<FileStreamOutcome<T>, FileAdmissionError>
where
    F: FnMut(&mut T, &[u8]) -> ControlFlow<()>,
{
    let mut validator = Utf8Validator::default();
    let mut bytes_read = 0_u64;
    let mut buffer = [0_u8; RETAINED_OUTPUT_READ_CHUNK_BYTES];
    loop {
        if should_cancel() {
            return Err(FileAdmissionError::Cancelled);
        }
        let read = match reader.read(&mut buffer) {
            Ok(0) => {
                validator.finish()?;
                return Ok(FileStreamOutcome {
                    value,
                    bytes_read,
                    declared_bytes,
                    reached_eof: true,
                });
            }
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        bytes_read = bytes_read.saturating_add(read as u64);
        validator.push(&buffer[..read])?;
        if on_chunk(&mut value, &buffer[..read]).is_break() {
            return Ok(FileStreamOutcome {
                value,
                bytes_read,
                declared_bytes,
                reached_eof: false,
            });
        }
    }
}

#[derive(Default)]
struct Utf8Validator {
    pending: Vec<u8>,
}

impl Utf8Validator {
    fn push(&mut self, bytes: &[u8]) -> Result<(), FileAdmissionError> {
        if self.pending.is_empty() {
            return self.validate(bytes);
        }
        let mut joined = Vec::with_capacity(self.pending.len().saturating_add(bytes.len()));
        joined.extend_from_slice(&self.pending);
        joined.extend_from_slice(bytes);
        self.pending.clear();
        self.validate(&joined)
    }

    fn validate(&mut self, bytes: &[u8]) -> Result<(), FileAdmissionError> {
        match std::str::from_utf8(bytes) {
            Ok(_) => Ok(()),
            Err(error) if error.error_len().is_some() => Err(FileAdmissionError::InvalidUtf8),
            Err(error) => {
                self.pending
                    .extend_from_slice(&bytes[error.valid_up_to()..]);
                if self.pending.len() <= 3 {
                    Ok(())
                } else {
                    Err(FileAdmissionError::InvalidUtf8)
                }
            }
        }
    }

    fn finish(self) -> Result<(), FileAdmissionError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(FileAdmissionError::InvalidUtf8)
        }
    }
}

pub struct BoundedTextOutput {
    max_bytes: usize,
    observed_bytes: usize,
    prefix_only: Option<Vec<u8>>,
    retained: Option<RetainedOutput>,
}

impl BoundedTextOutput {
    pub fn new(max_bytes: usize) -> Self {
        if max_bytes <= MICRO_COMPACTION_MARKER.len() + 2 {
            Self {
                max_bytes,
                observed_bytes: 0,
                prefix_only: Some(Vec::with_capacity(max_bytes)),
                retained: None,
            }
        } else {
            Self {
                max_bytes,
                observed_bytes: 0,
                prefix_only: None,
                retained: Some(RetainedOutput::new(max_bytes)),
            }
        }
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.observed_bytes = self.observed_bytes.saturating_add(bytes.len());
        if let Some(prefix) = &mut self.prefix_only {
            let remaining = self.max_bytes.saturating_sub(prefix.len());
            prefix.extend_from_slice(&bytes[..remaining.min(bytes.len())]);
        } else if let Some(retained) = &mut self.retained {
            retained.append(bytes);
        }
    }

    pub fn retained_bytes(&self) -> usize {
        self.prefix_only.as_ref().map_or_else(
            || {
                self.retained
                    .as_ref()
                    .map_or(0, RetainedOutput::retained_bytes)
            },
            Vec::len,
        )
    }

    pub fn finish(self) -> (String, bool) {
        if let Some(prefix) = self.prefix_only {
            let truncated = self.observed_bytes > prefix.len();
            return (valid_utf8_prefix(&prefix).to_string(), truncated);
        }

        let snapshot = self
            .retained
            .expect("head/tail retention must exist outside prefix mode")
            .into_snapshot();
        if !snapshot.is_truncated() {
            return (
                String::from_utf8(snapshot.bytes)
                    .expect("bounded text input must be validated before rendering"),
                false,
            );
        }
        (render_micro_compacted(snapshot, self.max_bytes), true)
    }
}

fn render_micro_compacted(snapshot: RetainedOutputSnapshot, max_bytes: usize) -> String {
    let side_budget = (max_bytes - MICRO_COMPACTION_MARKER.len()) / 2;
    let split = snapshot.retained_head_bytes().min(snapshot.bytes.len());
    let head_end = side_budget.min(split);
    let tail_start = snapshot.bytes.len().saturating_sub(side_budget).max(split);
    let head = valid_utf8_prefix(&snapshot.bytes[..head_end]);
    let tail = valid_utf8_suffix(&snapshot.bytes[tail_start..]);
    format!("{head}{MICRO_COMPACTION_MARKER}{tail}")
}

fn valid_utf8_prefix(bytes: &[u8]) -> &str {
    match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()])
            .expect("valid UTF-8 prefix must decode"),
    }
}

fn valid_utf8_suffix(bytes: &[u8]) -> &str {
    let mut start = 0;
    while start < bytes.len() && bytes[start] & 0b1100_0000 == 0b1000_0000 {
        start += 1;
    }
    std::str::from_utf8(&bytes[start..]).expect("validated UTF-8 suffix must decode")
}

pub fn build_file_change_preview(
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> FileChangePreview {
    if before.is_some_and(|text| text.len() > MAX_DIFF_INPUT_BYTES)
        || after.is_some_and(|text| text.len() > MAX_DIFF_INPUT_BYTES)
    {
        return FileChangePreview::Omitted {
            path: path.to_string(),
            max_input_bytes: MAX_DIFF_INPUT_BYTES,
        };
    }

    let mut config = TextDiff::configure();
    config.timeout(DIFF_TIMEOUT);
    let diff = config.diff_lines(before.unwrap_or(""), after.unwrap_or(""));
    let full = diff
        .unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    let (text, truncated) = truncate_output(full, MAX_DIFF_OUTPUT_BYTES);
    FileChangePreview::UnifiedDiff { text, truncated }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Cursor;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn whole_file_reader_rejects_growth_beyond_descriptor_metadata() {
        let error = read_utf8_reader_with_limit(Cursor::new(b"abcd"), 3, 16, || false)
            .expect_err("growth beyond descriptor metadata must fail");

        assert!(matches!(
            error,
            FileAdmissionError::GrewWhileReading {
                declared_bytes: 3,
                observed_bytes: 4
            }
        ));
    }

    #[test]
    fn streamed_utf8_accepts_codepoint_split_across_read_chunks() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("split.txt");
        let text = format!("{}€", "x".repeat(RETAINED_OUTPUT_READ_CHUNK_BYTES - 1));
        std::fs::write(&path, &text).expect("write UTF-8 fixture");

        let output = stream_utf8_file(
            &path,
            Vec::new(),
            || false,
            |bytes, chunk| {
                bytes.extend_from_slice(chunk);
                ControlFlow::Continue(())
            },
        )
        .expect("stream split UTF-8");

        assert_eq!(String::from_utf8(output.value).expect("valid UTF-8"), text);
        assert!(output.reached_eof);
    }

    #[test]
    fn streamed_utf8_rejects_invalid_bytes_outside_retained_output() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("invalid.txt");
        let mut bytes = vec![b'x'; RETAINED_OUTPUT_READ_CHUNK_BYTES * 2];
        bytes[RETAINED_OUTPUT_READ_CHUNK_BYTES + 10] = 0xff;
        std::fs::write(&path, bytes).expect("write invalid UTF-8 fixture");

        let error = stream_utf8_file(&path, (), || false, |_, _| ControlFlow::Continue(()))
            .expect_err("invalid UTF-8 must fail even outside retained output");

        assert!(matches!(error, FileAdmissionError::InvalidUtf8));
    }

    #[test]
    fn streamed_read_observes_cancellation_between_chunks() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("cancel.txt");
        std::fs::write(&path, vec![b'x'; RETAINED_OUTPUT_READ_CHUNK_BYTES * 4])
            .expect("write cancellation fixture");
        let polls = Cell::new(0);

        let error = stream_utf8_file(
            &path,
            0usize,
            || {
                polls.set(polls.get() + 1);
                polls.get() > 1
            },
            |chunks, _| {
                *chunks += 1;
                ControlFlow::Continue(())
            },
        )
        .expect_err("second chunk poll must cancel the read");

        assert!(matches!(error, FileAdmissionError::Cancelled));
        assert_eq!(polls.get(), 2);
    }

    #[test]
    fn bounded_text_matches_existing_micro_compaction_shape() {
        let mut output = BoundedTextOutput::new(128);
        output.append(format!("HEAD{}TAIL", "x".repeat(512)).as_bytes());

        let (text, truncated) = output.finish();

        assert!(truncated);
        assert!(text.starts_with("HEAD"));
        assert!(text.ends_with("TAIL"));
        assert!(text.contains("tool output micro-compacted"));
        assert!(text.len() <= 128);
    }

    #[test]
    fn tiny_bounded_text_keeps_only_a_valid_prefix() {
        let mut output = BoundedTextOutput::new(3);
        output.append("abcdef".as_bytes());

        assert_eq!(output.finish(), ("abc".to_string(), true));
    }

    #[test]
    fn file_change_preview_omits_oversized_inputs() {
        let oversized = "x".repeat(MAX_DIFF_INPUT_BYTES + 1);

        assert_eq!(
            build_file_change_preview("large.txt", Some(&oversized), Some("small")),
            FileChangePreview::Omitted {
                path: "large.txt".to_string(),
                max_input_bytes: MAX_DIFF_INPUT_BYTES,
            }
        );
    }

    #[test]
    fn file_change_preview_bounds_rendered_unified_diff() {
        let before = format!("{}\n", "a".repeat(512 * 1024));
        let after = format!("{}\n", "b".repeat(512 * 1024));

        let FileChangePreview::UnifiedDiff { text, truncated } =
            build_file_change_preview("large-line.txt", Some(&before), Some(&after))
        else {
            panic!("inputs below the preview ceiling should render");
        };

        assert!(truncated);
        assert!(text.len() <= MAX_DIFF_OUTPUT_BYTES);
        assert!(text.contains("tool output micro-compacted"));
    }
}
