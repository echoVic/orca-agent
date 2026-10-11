use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::assets::{self, HydrateImages};
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

    pub fn records(&self, images: HydrateImages) -> io::Result<SessionRecords> {
        SessionRecords::from_snapshot(self, None, images)
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
    images: HydrateImages,
    line: Vec<u8>,
    offset: u64,
    pub line_number: u64,
    done: bool,
    pub health: StoredSessionHealth,
    pub issue: Option<StoredSessionHealthIssue>,
}

pub(crate) fn iter_records(path: &Path, images: HydrateImages) -> io::Result<SessionRecords> {
    SessionRecords::open(path, None, images)
}

/// Read preview bytes even for tool images omitted by normal session resume.
pub(crate) fn read_tool_message_images(
    path: &Path,
    tool_call_id: &str,
) -> io::Result<Vec<orca_core::conversation::ImageInput>> {
    let mut records = iter_records(path, HydrateImages::None)?;
    while let Some(record) = records.next() {
        if let SessionRecord::Message {
            message:
                super::types::StoredMessage::Tool {
                    tool_call_id: id,
                    images,
                    ..
                },
            ..
        } = record?
            && id == tool_call_id
            && !images.is_empty()
        {
            let value = serde_json::from_str(records.text()).map_err(io::Error::other)?;
            let value = assets::hydrate(path, value, HydrateImages::All)?;
            let record = serde_json::from_value(value).map_err(io::Error::other)?;
            let record =
                super::writer::validate_session_record(record).map_err(io::Error::other)?;
            if let SessionRecord::Message {
                message: super::types::StoredMessage::Tool { images, .. },
                ..
            } = record
            {
                return Ok(images);
            }
        }
    }
    Ok(Vec::new())
}

impl SessionRecords {
    pub fn open(
        path: &Path,
        budget: Option<InspectionBudget>,
        images: HydrateImages,
    ) -> io::Result<Self> {
        Self::from_snapshot(&SessionRecordSnapshot::open(path)?, budget, images)
    }

    fn from_snapshot(
        snapshot: &SessionRecordSnapshot,
        budget: Option<InspectionBudget>,
        images: HydrateImages,
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
            images,
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
                let value = assets::hydrate(&self.path, value, self.images)
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

#[cfg(test)]
mod tests {
    use std::fs;

    use orca_core::conversation::{ImageDetail, ImageInput, ImageSource, Message};

    use super::*;
    use crate::thread_store::types::StoredMessage;

    fn stored_image(path: &Path, message: &Message) -> serde_json::Value {
        let record = SessionRecord::Message {
            id: None,
            turn_id: None,
            message: StoredMessage::from(message),
        };
        assets::externalize(path, serde_json::to_value(record).unwrap()).unwrap()
    }

    fn tool_message(call_id: &str, image: ImageInput) -> Message {
        Message::Tool {
            tool_call_id: call_id.to_string(),
            content: "screenshot".to_string(),
            images: vec![image],
            terminal: None,
            pinned: false,
        }
    }

    fn image(data: &str) -> ImageInput {
        ImageInput {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: data.to_string(),
            },
            detail: ImageDetail::High,
        }
    }

    fn write_records(path: &Path, records: &[serde_json::Value]) {
        let mut file = File::create(path).unwrap();
        for record in records {
            serde_json::to_writer(&mut file, record).unwrap();
            std::io::Write::write_all(&mut file, b"\n").unwrap();
        }
    }

    #[test]
    fn preview_reads_only_the_matching_records_image_assets() {
        for user_image in [false, true] {
            for corrupt in [false, true] {
                let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
                let path = directory.path().join("preview.jsonl");
                let unrelated_image = image("AQIDBA==");
                let unrelated_message = if user_image {
                    Message::user_with_images(
                        "earlier user image".to_string(),
                        vec![unrelated_image],
                    )
                } else {
                    tool_message("earlier-tool", unrelated_image)
                };
                let unrelated = stored_image(&path, &unrelated_message);
                let expected = image("BQYHCA==");
                let target = stored_image(&path, &tool_message("target", expected.clone()));
                write_records(&path, &[unrelated.clone(), target]);
                let blob = assets::directory(&path)
                    .join(unrelated["assets"][0]["sha256"].as_str().unwrap());
                if corrupt {
                    fs::write(blob, [9u8; 4]).unwrap();
                } else {
                    fs::remove_file(blob).unwrap();
                }

                assert_eq!(
                    read_tool_message_images(&path, "target")
                        .expect("unrelated image assets must not block this preview"),
                    vec![expected],
                    "user_image={user_image}, corrupt={corrupt}"
                );
                assert!(
                    read_tool_message_images(&path, "missing-call")
                        .unwrap()
                        .is_empty()
                );
                if !user_image {
                    assert!(read_tool_message_images(&path, "earlier-tool").is_err());
                }
            }
        }
    }

    #[test]
    fn preview_still_rejects_corrupt_matching_image_assets() {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = directory.path().join("preview.jsonl");
        let expected = image("AQIDBA==");
        let record = stored_image(&path, &tool_message("target", expected.clone()));
        write_records(&path, std::slice::from_ref(&record));
        assert_eq!(
            read_tool_message_images(&path, "target").unwrap(),
            vec![expected]
        );
        let blob = assets::directory(&path).join(record["assets"][0]["sha256"].as_str().unwrap());
        fs::write(blob, [9u8; 4]).unwrap();
        assert!(read_tool_message_images(&path, "target").is_err());
    }

    #[test]
    fn preview_still_rejects_invalid_unrelated_asset_manifests() {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = directory.path().join("preview.jsonl");
        let mut unrelated = stored_image(&path, &tool_message("earlier-tool", image("AQIDBA==")));
        let target = stored_image(&path, &tool_message("target", image("BQYHCA==")));
        unrelated["assets"][0]["sha256"] = serde_json::json!("../../outside");
        write_records(&path, &[unrelated, target]);

        assert!(read_tool_message_images(&path, "target").is_err());
    }

    #[test]
    fn preview_reads_inline_images_and_skips_image_free_matching_messages() {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let path = directory.path().join("preview.jsonl");
        let expected = image("AQIDBA==");
        let empty = SessionRecord::Message {
            id: None,
            turn_id: None,
            message: StoredMessage::from(&Message::Tool {
                tool_call_id: "target".to_string(),
                content: "text only".to_string(),
                images: Vec::new(),
                terminal: None,
                pinned: false,
            }),
        };
        let inline = SessionRecord::Message {
            id: None,
            turn_id: None,
            message: StoredMessage::from(&tool_message("target", expected.clone())),
        };
        write_records(
            &path,
            &[
                serde_json::to_value(empty).unwrap(),
                serde_json::to_value(inline).unwrap(),
            ],
        );

        assert_eq!(
            read_tool_message_images(&path, "target").unwrap(),
            vec![expected]
        );
    }
}
