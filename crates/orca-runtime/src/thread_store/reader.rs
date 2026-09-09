use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::assets;
use super::types::{SessionRecord, StoredSessionHealth, StoredSessionHealthIssue};
use super::writer::{
    MAX_SESSION_LINE_BYTES, open_regular_history_file, parse_session_record, read_bounded_line,
    session_health_error,
};

/// A scan budget is an inspection policy, never a transcript validity limit.
#[derive(Clone, Copy)]
pub(crate) struct InspectionBudget {
    pub bytes: u64,
    pub records: usize,
}

pub(crate) const INDEX_BUDGET: InspectionBudget = InspectionBudget {
    bytes: 8 * 1024 * 1024,
    records: 10_000,
};

/// Pins both the file identity and byte boundary across appends and atomic
/// metadata rewrites. Each replay has an independent positional read cursor.
#[derive(Clone)]
pub(crate) struct SessionRecordSnapshot {
    file: Arc<File>,
    path: PathBuf,
    encoded_bytes: u64,
}

impl SessionRecordSnapshot {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = open_regular_history_file(path)?;
        let encoded_bytes = file.metadata()?.len();
        Ok(Self {
            file: Arc::new(file),
            path: path.to_path_buf(),
            encoded_bytes,
        })
    }

    pub fn records(&self) -> io::Result<SessionRecords> {
        SessionRecords::from_snapshot(self, None)
    }
}

struct SnapshotReader {
    file: Arc<File>,
    offset: u64,
}

impl Read for SnapshotReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = orca_platform::fs::read_at(&self.file, buffer, self.offset)?;
        self.offset += count as u64;
        Ok(count)
    }
}

pub(crate) struct SessionRecords {
    reader: Box<dyn BufRead>,
    path: PathBuf,
    compressed: bool,
    budget: Option<InspectionBudget>,
    line: Vec<u8>,
    offset: u64,
    pub line_number: u64,
    done: bool,
    pub health: StoredSessionHealth,
    pub issue: Option<StoredSessionHealthIssue>,
}

pub(crate) fn iter_records(path: &Path) -> io::Result<SessionRecords> {
    SessionRecords::open(path, None)
}

impl SessionRecords {
    pub fn open(path: &Path, budget: Option<InspectionBudget>) -> io::Result<Self> {
        Self::from_snapshot(&SessionRecordSnapshot::open(path)?, budget)
    }

    fn from_snapshot(
        snapshot: &SessionRecordSnapshot,
        budget: Option<InspectionBudget>,
    ) -> io::Result<Self> {
        let path = &snapshot.path;
        let file = SnapshotReader {
            file: Arc::clone(&snapshot.file),
            offset: 0,
        }
        .take(snapshot.encoded_bytes);
        let compressed = path.extension().and_then(|s| s.to_str()) == Some("zst");
        let reader: Box<dyn BufRead> = if compressed {
            let decoder = zstd::stream::read::Decoder::new(file)?;
            Box::new(BufReader::new(decoder))
        } else {
            Box::new(BufReader::new(file))
        };
        Ok(Self {
            reader,
            path: path.to_path_buf(),
            compressed,
            budget,
            line: Vec::new(),
            offset: 0,
            line_number: 0,
            done: false,
            health: StoredSessionHealth::Healthy,
            issue: None,
        })
    }

    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.line)
            .unwrap_or("")
            .trim_end_matches(['\r', '\n'])
    }

    fn stop(&mut self, health: StoredSessionHealth, code: &str, offset: u64) {
        self.done = true;
        self.health = health;
        self.issue = Some(StoredSessionHealthIssue {
            code: code.to_owned(),
            line: Some(self.line_number),
            offset: Some(offset),
        });
    }

    fn fail(&mut self, code: &str, offset: u64) -> Option<io::Result<SessionRecord>> {
        self.stop(StoredSessionHealth::Quarantined, code, offset);
        Some(Err(session_health_error(
            &self.path,
            self.health,
            self.issue.as_ref(),
        )))
    }
}

impl Iterator for SessionRecords {
    type Item = io::Result<SessionRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            // Probe EOF before reporting a partial scan at an exact boundary.
            match self.reader.fill_buf() {
                Ok([]) => {
                    self.done = true;
                    return None;
                }
                Err(_) => {
                    return self.fail(
                        if self.compressed {
                            "zstd_stream"
                        } else {
                            "read_error"
                        },
                        self.offset,
                    );
                }
                _ => {}
            }
            if let Some(budget) = self.budget
                && (self.offset >= budget.bytes || self.line_number >= budget.records as u64)
            {
                self.stop(
                    StoredSessionHealth::InspectionLimited,
                    "inspection_budget",
                    self.offset,
                );
                return None;
            }
            self.line.clear();
            let bound = self.budget.map_or(MAX_SESSION_LINE_BYTES, |budget| {
                MAX_SESSION_LINE_BYTES.min(budget.bytes.saturating_sub(self.offset) as usize)
            });
            let (count, terminated) =
                match read_bounded_line(&mut self.reader, &mut self.line, bound) {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        self.done = true;
                        return None;
                    }
                    Err(_) => {
                        return self.fail(
                            if self.compressed {
                                "zstd_stream"
                            } else {
                                "read_error"
                            },
                            self.offset,
                        );
                    }
                };
            let start = self.offset;
            self.offset = self.offset.saturating_add(count as u64);
            self.line_number += 1;
            if self.line.len() > bound {
                if bound < MAX_SESSION_LINE_BYTES {
                    self.stop(
                        StoredSessionHealth::InspectionLimited,
                        "inspection_budget",
                        start,
                    );
                    return None;
                }
                return self.fail("record_bytes_limit", start);
            }
            if self.line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let text = match std::str::from_utf8(&self.line) {
                Ok(text) => text.trim_end_matches(['\r', '\n']),
                Err(_) => return self.fail("invalid_utf8", start),
            };
            let parsed = parse_session_record(text).or_else(|code| {
                let Ok(value) = serde_json::from_str(text) else {
                    return Err(code);
                };
                if !assets::is_asset_record(&value) {
                    return Err(code);
                }
                let value = assets::hydrate(&self.path, value, self.budget.is_none())
                    .map_err(|_| "invalid_image_asset")?;
                let record: SessionRecord =
                    serde_json::from_value(value).map_err(|_| "malformed_asset_record")?;
                super::writer::validate_session_record(record)
            });
            match parsed {
                Ok(record) => {
                    return Some(Ok(record));
                }
                Err("incomplete_record") if !terminated && !self.compressed => {
                    self.stop(
                        StoredSessionHealth::RecoverableTail,
                        "recoverable_tail",
                        start,
                    );
                    return None;
                }
                Err(code) => return self.fail(code, start),
            }
        }
        None
    }
}
