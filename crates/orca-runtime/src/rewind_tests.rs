//! `/rewind` runtime contract: durable `session.snapshot` / `session.rewind`
//! records, the effective-history readers, the checkpoint list projection and
//! the file restore round trip.
//!
//! These tests deliberately use the transcript writer and the rewind readers
//! plus `orca-rewind`'s store and planner, so they exercise the same durable
//! representation the runtime capture and coordinator produce. They live
//! inside the crate because the thread-store rewind readers are `pub(crate)`.

use std::path::{Path, PathBuf};

use crate::history::{SessionWriter, TestOrcaHomeGuard, load_session, redirect_test_orca_home};
use crate::thread_store::{
    BlobRef, ConversationRewindRecord, FileSnapshotRecord, SessionRewindRecord,
    SnapshotChangeStats, SnapshotFileEntry, read_effective_user_messages, read_snapshots,
    read_transcript_before_message,
};
use orca_core::approval_types::ActionKind;
use orca_core::conversation::{Conversation, Message};
use orca_core::thread_identity::TurnId;
use orca_core::tool_types::{ToolName, ToolRequest};
use orca_rewind::{
    Applier, PlanOptions, PreImage, RewindStore, SnapshotFileView, SnapshotView, build_plan,
    file_sha256,
};
use tempfile::TempDir;

struct Fixture {
    _home_guard: TestOrcaHomeGuard,
    home: TempDir,
    cwd: TempDir,
    writer: SessionWriter,
    session_id: String,
    transcript: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temporary ORCA_HOME");
        let home_guard = redirect_test_orca_home(home.path());
        let cwd = tempfile::tempdir().expect("workspace");
        let writer = SessionWriter::start(
            cwd.path(),
            "mock",
            Some("mock-model".to_string()),
            "rewind contract",
        )
        .expect("writer");
        let transcript = load_session("latest").expect("transcript");
        let transcript_path = transcript.path.clone();
        let session_id = transcript.meta.session_id.clone();
        Self {
            _home_guard: home_guard,
            home,
            cwd,
            writer,
            session_id,
            transcript: transcript_path,
        }
    }

    fn cwd(&self) -> &Path {
        self.cwd.path()
    }

    fn append_turn(&mut self, prompt: &str, reply: &str) {
        self.writer.enter_turn(TurnId::new());
        self.writer
            .append_message(&Message::user(prompt.to_string()))
            .expect("user message");
        self.writer
            .append_message(&Message::Assistant {
                content: Some(reply.to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            })
            .expect("assistant message");
    }

    fn users(&self) -> Vec<(String, String)> {
        read_effective_user_messages(&self.transcript)
            .expect("effective users")
            .into_iter()
            .map(|user| (user.message_id, user.prompt))
            .collect()
    }

    fn store(&self) -> RewindStore {
        let store_dir = self
            .home
            .path()
            .join("rewind")
            .join("sessions")
            .join(&self.session_id);
        RewindStore::open(&store_dir).expect("rewind store")
    }

    fn store_dir(&self) -> PathBuf {
        self.home
            .path()
            .join("rewind")
            .join("sessions")
            .join(&self.session_id)
    }

    fn write_workspace_file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.cwd().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directory");
        }
        std::fs::write(&path, contents).expect("write workspace file");
        path
    }

    fn snapshot_record(
        &self,
        at_message_id: String,
        ordinal: u32,
        files: Vec<(String, SnapshotFileEntry)>,
        change_stats: Option<SnapshotChangeStats>,
    ) -> FileSnapshotRecord {
        FileSnapshotRecord {
            schema_version: 1,
            snapshot_id: format!("snap_{ordinal}"),
            session_id: self.session_id.clone(),
            at_message_id,
            turn_id: None,
            ordinal,
            root: self
                .cwd()
                .canonicalize()
                .expect("canonical cwd")
                .to_string_lossy()
                .into_owned(),
            workspace_fingerprint: None,
            complete: true,
            start_digest: None,
            end_digest: None,
            files: files.into_iter().collect(),
            change_stats,
            untracked: Vec::new(),
            limitations: Vec::new(),
            recorded_at: chrono::Utc::now(),
        }
    }
}

fn blob_ref(pre: &PreImage) -> BlobRef {
    match pre {
        PreImage::Missing => panic!("tombstones are encoded as a missing entry"),
        PreImage::Blob(blob) => BlobRef {
            sha256: blob.sha256.clone(),
            size: blob.size,
            mode: blob.mode,
            encoding: match blob.encoding {
                orca_rewind::BlobEncoding::Raw => "raw".to_string(),
                orca_rewind::BlobEncoding::Zstd => "zstd".to_string(),
            },
        },
    }
}

fn snapshot_view(record: &FileSnapshotRecord) -> SnapshotView {
    SnapshotView {
        ordinal: record.ordinal,
        at_message_id: record.at_message_id.clone(),
        files: record
            .files
            .iter()
            .map(|(path, entry)| {
                let pre = match &entry.pre {
                    None => PreImage::Missing,
                    Some(blob) => PreImage::Blob(orca_rewind::BlobRef {
                        sha256: blob.sha256.clone(),
                        size: blob.size,
                        mode: blob.mode,
                        encoding: match blob.encoding.as_str() {
                            "zstd" => orca_rewind::BlobEncoding::Zstd,
                            _ => orca_rewind::BlobEncoding::Raw,
                        },
                    }),
                };
                (
                    path.clone(),
                    SnapshotFileView {
                        pre,
                        post_sha256: entry.post_sha256.clone(),
                    },
                )
            })
            .collect(),
    }
}

/// A turn that modified one existing file and created another: the durable
/// record round-trips, the planner produces the undo plan, and applying it
/// restores the file and removes the created one.
#[test]
fn snapshot_round_trips_and_plan_restores_the_turn() {
    let mut fixture = Fixture::new();
    fixture.append_turn("edit the notes", "I edited the notes.");
    let user_id = fixture.users()[0].0.clone();
    let store = fixture.store();

    let modified = fixture.write_workspace_file("notes.txt", "old line one\nold line two\n");
    let pre_modified = store.put_pre_image(&modified).expect("pre-image");
    fixture.write_workspace_file("notes.txt", "new line\n");

    let created = fixture.cwd().join("created.txt");
    let pre_created = store.put_pre_image(&created).expect("tombstone");
    assert_eq!(pre_created, PreImage::Missing);
    fixture.write_workspace_file("created.txt", "created during the turn\n");
    let post_modified = file_sha256(&modified).expect("hash");
    let post_created = file_sha256(&created).expect("hash");

    fixture
        .writer
        .append_file_snapshot(fixture.snapshot_record(
            user_id.clone(),
            0,
            vec![
                (
                    "notes.txt".to_string(),
                    SnapshotFileEntry {
                        pre: Some(blob_ref(&pre_modified)),
                        post_sha256: post_modified,
                    },
                ),
                (
                    "created.txt".to_string(),
                    SnapshotFileEntry {
                        pre: None,
                        post_sha256: post_created,
                    },
                ),
            ],
            Some(SnapshotChangeStats {
                files: 2,
                insertions: 3,
                deletions: 1,
                omitted: false,
            }),
        ))
        .expect("snapshot record");

    let snapshots = read_snapshots(&fixture.transcript).expect("read snapshots");
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].at_message_id, user_id);
    assert_eq!(snapshots[0].ordinal, 0);
    assert!(snapshots[0].complete);
    assert_eq!(snapshots[0].files.len(), 2);
    assert!(snapshots[0].files["created.txt"].pre.is_none());

    let views = snapshots.iter().map(snapshot_view).collect::<Vec<_>>();
    let plan = build_plan(
        &fixture.cwd().canonicalize().expect("canonical cwd"),
        &store,
        &views,
        PlanOptions { target_ordinal: 0 },
    )
    .expect("plan");
    // Dry run: the plan already carries the authoritative statistics.
    assert_eq!(plan.writes.len(), 1, "notes.txt is written back");
    assert_eq!(plan.deletes.len(), 1, "created.txt is removed");
    assert_eq!(plan.skipped.len(), 0);
    assert!(plan.insertions >= 1, "restoring two old lines adds lines");
    assert!(plan.deletions >= 1);
    assert!(
        !plan
            .warnings
            .iter()
            .any(|warning| warning.kind == "untracked_change")
    );

    let applier = Applier::new(
        fixture.cwd().canonicalize().expect("canonical cwd"),
        store,
        fixture.store_dir().join("journals"),
    );
    let report = applier.apply(&plan).expect("apply");
    assert_eq!(report.applied, 1);
    assert_eq!(report.deleted, 1);
    assert!(!report.partial);
    assert_eq!(
        std::fs::read_to_string(fixture.cwd().join("notes.txt")).expect("restored"),
        "old line one\nold line two\n"
    );
    assert!(!fixture.cwd().join("created.txt").exists());
    assert_eq!(applier.pending_journal().expect("journal check"), None);
}

/// `read_effective_user_messages` only sees real user turns, and
/// `read_transcript_before_message` cuts strictly before the target message
/// while keeping the counts a `session.rewind` record must persist.
#[test]
fn effective_history_readers_agree_with_the_turn_boundaries() {
    let mut fixture = Fixture::new();
    fixture.append_turn("first prompt", "first reply");
    fixture.append_turn("second prompt", "second reply");

    // Pinned user prompts are real turns (headless sessions submit them this
    // way); internal image analysis is not.
    fixture.writer.enter_turn(TurnId::new());
    fixture
        .writer
        .append_message(&Message::pinned_user("pinned context".to_string()))
        .expect("pinned user");
    let internal = format!(
        "[Image analysis:{}]\n{}",
        "a".repeat(64),
        serde_json::json!({
            "trust": "untrusted_observation_data",
            "description": "saved image",
            "query": "second prompt",
            "image_count": 1,
            "provenance": null,
        })
    );
    fixture
        .writer
        .append_message(&Message::user(internal))
        .expect("internal image analysis user");

    let users = fixture.users();
    assert_eq!(
        users.len(),
        3,
        "pinned user prompts are rewindable; internal analysis records are not"
    );
    assert_eq!(users[0].1, "first prompt");
    assert_eq!(users[1].1, "second prompt");
    assert_eq!(users[2].1, "pinned context");
    let ordinals = read_effective_user_messages(&fixture.transcript)
        .expect("effective users")
        .into_iter()
        .map(|user| user.ordinal)
        .collect::<Vec<_>>();
    assert_eq!(ordinals, vec![0, 1, 2]);

    let second_id = users[1].0.clone();
    let cut =
        read_transcript_before_message(&fixture.transcript, &second_id).expect("transcript cut");
    assert_eq!(cut.prompt, "second prompt");
    assert_eq!(cut.ordinal, 1);
    assert_eq!(
        cut.kept_messages as usize,
        cut.transcript.messages.len(),
        "kept counts describe the returned transcript"
    );
    assert_eq!(cut.kept_compactions, 0);
    assert_eq!(cut.kept_summaries, 0);
    assert_eq!(cut.transcript.messages.len(), 2, "first turn only");
    assert!(cut.transcript.messages.iter().all(|message| !matches!(
        message,
        Message::User { content, .. } if content == "second prompt"
    )));

    assert!(
        read_transcript_before_message(&fixture.transcript, "item_missing").is_err(),
        "unknown targets fail closed"
    );
}

/// The checkpoint list projection joins effective user messages with their
/// `session.snapshot` statistics.
#[test]
fn checkpoint_views_join_snapshots_with_user_messages() {
    let mut fixture = Fixture::new();
    fixture.append_turn("inspect the build", "build is fine");
    fixture.append_turn("fix the build", "fixing");
    let users = fixture.users();
    let first_id = users[0].0.clone();

    fixture
        .writer
        .append_file_snapshot(fixture.snapshot_record(
            first_id.clone(),
            0,
            Vec::new(),
            Some(SnapshotChangeStats {
                files: 2,
                insertions: 3,
                deletions: 1,
                omitted: false,
            }),
        ))
        .expect("snapshot record");

    let views = crate::rewind::checkpoint_views(&fixture.transcript).expect("views");
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].message_id, first_id);
    assert_eq!(views[0].ordinal, 0);
    assert_eq!(views[0].prompt_preview, "inspect the build");
    assert!(views[0].has_snapshot);
    assert!(views[0].files_available);
    assert!(views[0].conversation_available);
    assert_eq!(views[0].files_changed, 2);
    assert_eq!(views[0].insertions, 3);
    assert_eq!(views[0].deletions, 1);
    assert!(views[0].recorded_at.is_some());
    assert_eq!(views[1].prompt_preview, "fix the build");
    assert!(!views[1].has_snapshot);
    assert!(!views[1].files_available);
    assert!(
        views[1]
            .unavailable_reasons
            .iter()
            .any(|reason| reason == "snapshot_missing")
    );
}

/// Appending a `session.rewind{kind:"truncate"}` record folds the transcript:
/// later loads only see the kept messages, and continuing the conversation
/// appends normally on top of the folded history.
#[test]
fn conversation_rewind_record_folds_the_transcript() {
    let mut fixture = Fixture::new();
    fixture.append_turn("first prompt", "first reply");
    fixture.append_turn("second prompt", "second reply");
    let users = fixture.users();
    let second_id = users[1].0.clone();
    let cut =
        read_transcript_before_message(&fixture.transcript, &second_id).expect("transcript cut");

    fixture
        .writer
        .append_session_rewind(SessionRewindRecord {
            schema_version: 1,
            session_id: fixture.session_id.clone(),
            rewind_id: "rw_test".to_string(),
            at_message_id: second_id.clone(),
            kept_messages: cut.kept_messages,
            kept_compactions: cut.kept_compactions,
            kept_summaries: cut.kept_summaries,
            conversation: Some(ConversationRewindRecord {
                kind: "truncate".to_string(),
                summary: None,
                summary_usage: None,
            }),
            files: None,
            recorded_at: chrono::Utc::now(),
        })
        .expect("rewind record");

    let folded = load_session(&fixture.session_id).expect("folded transcript");
    assert_eq!(
        folded.messages.len() as u32,
        cut.kept_messages,
        "rewinded messages are not replayed"
    );
    assert_eq!(fixture.users().len(), 1, "only the first turn remains");

    // Continuing after the rewind appends to the folded history.
    fixture.append_turn("third prompt", "third reply");
    let resumed = load_session(&fixture.session_id).expect("resumed transcript");
    assert_eq!(resumed.messages.len(), 4, "(first user+reply) + third turn");
    let prompts = fixture
        .users()
        .into_iter()
        .map(|(_, prompt)| prompt)
        .collect::<Vec<_>>();
    assert_eq!(prompts, vec!["first prompt", "third prompt"]);
}

/// The snapshot record is a plain JSONL append: writing one does not truncate
/// or rewrite earlier records.
#[test]
fn snapshot_records_are_append_only() {
    let mut fixture = Fixture::new();
    fixture.append_turn("first prompt", "first reply");
    let first_id = fixture.users()[0].0.clone();
    let before = std::fs::read_to_string(&fixture.transcript).expect("transcript");
    fixture
        .writer
        .append_file_snapshot(fixture.snapshot_record(first_id, 0, Vec::new(), None))
        .expect("snapshot record");
    let after = std::fs::read_to_string(&fixture.transcript).expect("transcript");
    assert!(after.starts_with(&before), "earlier records are untouched");
    let appended = &after[before.len()..];
    assert!(appended.contains("\"session.snapshot\""));
}

/// The per-turn capture records the pre-image of a write target before the
/// write, dedupes repeat writes of the same file, encodes created files as
/// tombstones, and refuses paths outside the workspace root.
#[test]
fn turn_capture_records_pre_images_before_writes() {
    let mut fixture = Fixture::new();
    fixture.writer.enter_turn(TurnId::new());
    fixture
        .writer
        .append_message(&Message::user("edit the notes".to_string()))
        .expect("user message");
    let user_id = fixture.users()[0].0.clone();

    let cwd = fixture.cwd().canonicalize().expect("canonical cwd");
    let mut config = orca_core::config::RunConfig::default();
    config.cwd = Some(cwd.clone());
    config.history_mode = orca_core::config::HistoryMode::Record;
    let mut conversation = Conversation::new();
    conversation.add_system("system".to_string());
    conversation.add_user("edit the notes".to_string());

    let turn = crate::rewind::begin_turn(
        &config,
        &cwd,
        "turn-1",
        &conversation,
        Some(&mut fixture.writer),
    )
    .expect("a recorded session with history enabled starts a capture");

    fixture.write_workspace_file("notes.txt", "old line\n");
    turn.capture_pre_image(&cwd, &write_request("notes.txt"));
    fixture.write_workspace_file("notes.txt", "new line\n");
    // A second write of the same file in the same turn keeps the first
    // pre-image and refreshes the post digest.
    turn.capture_pre_image(&cwd, &write_request("notes.txt"));

    // A file created during the turn is a tombstone (missing pre-image).
    turn.capture_pre_image(&cwd, &write_request("created.txt"));
    fixture.write_workspace_file("created.txt", "created\n");

    // Non-write tools and paths outside the root are not captured.
    turn.capture_pre_image(
        &cwd,
        &ToolRequest {
            id: "call-read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({"path": "notes.txt"}).to_string()),
        },
    );
    turn.capture_pre_image(&cwd, &write_request("../escape.txt"));
    turn.capture_pre_image(&cwd, &write_request("/tmp/escape.txt"));

    let record = turn
        .finish_record()
        .expect("snapshot")
        .expect("files were captured");
    assert_eq!(record.at_message_id, user_id);
    assert_eq!(record.ordinal, 0);
    assert_eq!(record.files.len(), 2, "one entry per file, not per write");
    assert!(record.files["notes.txt"].pre.is_some());
    assert_eq!(
        record.files["notes.txt"].post_sha256,
        file_sha256(&cwd.join("notes.txt")).expect("hash")
    );
    assert!(record.files["created.txt"].pre.is_none(), "tombstone");
    assert!(record.files["created.txt"].post_sha256.is_some());
    assert!(
        record
            .limitations
            .iter()
            .any(|limitation| limitation == "outside_root"),
        "out-of-root writes are recorded as limitations: {:?}",
        record.limitations
    );
    assert_eq!(record.root, cwd.display().to_string());
    assert!(
        record.untracked.is_empty(),
        "both changed files were captured"
    );
    let stats = record.change_stats.expect("change statistics are recorded");
    assert_eq!(stats.files, 2);
    assert_eq!(
        stats.insertions, 2,
        "one rewritten line plus one line in the created file"
    );
    assert_eq!(stats.deletions, 1, "the rewritten line is removed once");
    assert!(!stats.omitted);
}

fn write_request(path: &str) -> ToolRequest {
    ToolRequest {
        id: format!("call-{path}"),
        name: ToolName::WriteFile,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(serde_json::json!({ "path": path }).to_string()),
    }
}

/// Headless sessions submit their prompt with `PinnedUser` placement. Those
/// turns must still open a checkpoint (correct ordinal, correct message id)
/// and the resulting snapshot must be listable.
#[test]
fn pinned_user_turns_open_a_listable_checkpoint() {
    let mut fixture = Fixture::new();
    fixture.writer.enter_turn(TurnId::new());
    fixture
        .writer
        .append_message(&Message::pinned_user("headless prompt".to_string()))
        .expect("pinned user message");
    let users = fixture.users();
    assert_eq!(
        users.len(),
        1,
        "the pinned prompt is the only rewindable turn"
    );
    assert_eq!(users[0].1, "headless prompt");
    let message_id = users[0].0.clone();

    let cwd = fixture.cwd().canonicalize().expect("canonical cwd");
    let mut config = orca_core::config::RunConfig::default();
    config.cwd = Some(cwd.clone());
    config.history_mode = orca_core::config::HistoryMode::Record;
    let mut conversation = Conversation::new();
    conversation.add_system("system".to_string());
    conversation.add_user_pinned("headless prompt".to_string());

    let turn = crate::rewind::begin_turn(
        &config,
        &cwd,
        "turn-headless",
        &conversation,
        Some(&mut fixture.writer),
    )
    .expect("a pinned-prompt turn opens a checkpoint");

    fixture.write_workspace_file("out.txt", "old\n");
    turn.capture_pre_image(&cwd, &write_request("out.txt"));
    fixture.write_workspace_file("out.txt", "new\n");
    let record = turn
        .finish_record()
        .expect("snapshot")
        .expect("the write was captured");
    assert_eq!(record.at_message_id, message_id);
    assert_eq!(record.ordinal, 0, "the pinned prompt is turn ordinal 0");
    fixture
        .writer
        .append_file_snapshot(record)
        .expect("snapshot record");

    let views = crate::rewind::checkpoint_views(&fixture.transcript).expect("views");
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].message_id, message_id);
    assert_eq!(views[0].ordinal, 0);
    assert!(views[0].has_snapshot);
    assert!(views[0].files_available);
}
