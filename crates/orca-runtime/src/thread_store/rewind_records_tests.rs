//! Durable contract for the `/rewind` transcript records.
//!
//! Proves:
//! - `session.rewind` markers fold into the loaded transcript (kept counts cut
//!   messages/compactions/summaries) and later appends still replay correctly.
//! - Code-only rewind records (`conversation: None`) never cut the transcript.
//! - Unknown conversation rewind kinds fail closed.
//! - `session.snapshot` records are audit-only: they never change messages.
//! - `read_transcript_until_effective` / `read_transcript_before_message`
//!   operate on the rewind-folded effective history, not the raw log.
//! - `read_effective_user_messages` enumerates effective user prompts
//!   (including pinned headless prompts) and skips internal image-analysis
//!   sidecars.
//! - Old transcripts without the new records load unchanged.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::Utc;
use orca_core::conversation::{IMAGE_ANALYSIS_MESSAGE_PREFIX, Message};
use orca_core::thread_identity::TurnId;

use super::writer::read_transcript;
use super::{
    BlobRef, ConversationRewindRecord, FileSnapshotRecord, RewindFilesSummary, SessionRewindRecord,
    SessionTranscript, SessionWriter,
};

struct Fixture {
    _cwd: tempfile::TempDir,
    session_id: String,
    path: PathBuf,
    writer: SessionWriter,
}

impl Fixture {
    fn new() -> Self {
        let cwd = tempfile::tempdir().expect("temporary cwd");
        let meta = crate::history::create_meta(cwd.path(), "mock", None, "rewind contract");
        let session_id = meta.session_id.clone();
        let writer = SessionWriter::start_from_meta(meta).expect("start transcript");
        let path = writer.path().to_path_buf();
        Self {
            _cwd: cwd,
            session_id,
            path,
            writer,
        }
    }

    fn transcript(&self) -> SessionTranscript {
        read_transcript(&self.path).expect("load transcript")
    }

    fn message_ids(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .expect("read transcript bytes")
            .lines()
            .filter_map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).ok()?;
                if value["type"] != "conversation.message" {
                    return None;
                }
                value["id"].as_str().map(str::to_string)
            })
            .collect()
    }

    fn last_message_id(&self) -> String {
        self.message_ids().pop().expect("persisted message id")
    }

    fn append_user(&mut self, content: &str) -> String {
        self.writer.enter_turn(TurnId::new());
        self.writer
            .append_message(&Message::user(content.to_string()))
            .expect("append user message");
        self.last_message_id()
    }

    fn append_assistant(&mut self, content: &str) -> String {
        self.writer
            .append_message(&Message::Assistant {
                content: Some(content.to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            })
            .expect("append assistant message");
        self.last_message_id()
    }

    fn append_turn(&mut self, user: &str, assistant: &str) -> (String, String) {
        let user_id = self.append_user(user);
        let assistant_id = self.append_assistant(assistant);
        (user_id, assistant_id)
    }

    fn append_rewind(&mut self, at_message_id: &str, kept_messages: u32) {
        self.writer
            .append_session_rewind(rewind_record(
                &self.session_id,
                at_message_id,
                kept_messages,
            ))
            .expect("append session.rewind record");
    }
}

fn rewind_record(session_id: &str, at_message_id: &str, kept_messages: u32) -> SessionRewindRecord {
    SessionRewindRecord {
        schema_version: 1,
        session_id: session_id.to_string(),
        rewind_id: "rw_contract_1".to_string(),
        at_message_id: at_message_id.to_string(),
        kept_messages,
        kept_compactions: 0,
        kept_summaries: 0,
        conversation: Some(ConversationRewindRecord {
            kind: "truncate".to_string(),
            summary: None,
            summary_usage: None,
        }),
        files: Some(RewindFilesSummary {
            restored: 1,
            deleted: 0,
            skipped: 0,
        }),
        recorded_at: Utc::now(),
    }
}

fn snapshot_record(session_id: &str, at_message_id: &str) -> FileSnapshotRecord {
    FileSnapshotRecord {
        schema_version: 1,
        snapshot_id: "snap_contract_1".to_string(),
        session_id: session_id.to_string(),
        at_message_id: at_message_id.to_string(),
        turn_id: Some("turn_contract_1".to_string()),
        ordinal: 3,
        root: "/tmp/workspace".to_string(),
        workspace_fingerprint: Some("dev:1,ino:2".to_string()),
        complete: true,
        start_digest: Some("sha256:start".to_string()),
        end_digest: Some("sha256:end".to_string()),
        files: BTreeMap::from([(
            "src/a.rs".to_string(),
            super::SnapshotFileEntry {
                pre: Some(BlobRef {
                    sha256: "ab12".to_string(),
                    size: 812,
                    mode: Some(0o644),
                    encoding: "raw".to_string(),
                }),
                post_sha256: Some("cd34".to_string()),
            },
        )]),
        change_stats: Some(super::SnapshotChangeStats {
            files: 1,
            insertions: 41,
            deletions: 7,
            omitted: false,
        }),
        untracked: vec!["docs/x.md".to_string()],
        limitations: vec!["observation_budget".to_string()],
        recorded_at: Utc::now(),
    }
}

fn message_text(message: &Message) -> String {
    match message {
        Message::System { content, .. }
        | Message::User { content, .. }
        | Message::Tool { content, .. } => content.clone(),
        Message::Assistant { content, .. } => content.clone().unwrap_or_default(),
    }
}

fn texts(transcript: &SessionTranscript) -> Vec<String> {
    transcript.messages.iter().map(message_text).collect()
}

#[test]
fn rewind_marker_truncates_folded_transcript_and_later_appends_survive() {
    crate::history::with_redirected_orca_home("rewind-fold", |_home| {
        let mut fixture = Fixture::new();
        let (user_one, assistant_one) = fixture.append_turn("first", "first reply");
        fixture.append_turn("second", "second reply");
        assert_eq!(fixture.message_ids().len(), 4);

        fixture.append_rewind(&assistant_one, 2);

        let transcript = fixture.transcript();
        assert_eq!(
            texts(&transcript),
            ["first".to_string(), "first reply".to_string()]
        );

        // Append-only: a message written after the marker belongs to the
        // rewound history and must replay after the cut.
        let post_rewind = fixture.append_user("after rewind");
        assert_ne!(post_rewind, user_one);
        let transcript = fixture.transcript();
        assert_eq!(
            texts(&transcript),
            [
                "first".to_string(),
                "first reply".to_string(),
                "after rewind".to_string()
            ]
        );
    });
}

#[test]
fn snapshot_records_are_audit_only_and_code_only_rewinds_do_not_truncate() {
    crate::history::with_redirected_orca_home("rewind-audit", |_home| {
        let mut fixture = Fixture::new();
        let (user_one, _assistant_one) = fixture.append_turn("one", "one reply");
        fixture
            .writer
            .append_file_snapshot(snapshot_record(&fixture.session_id, &user_one))
            .expect("append session.snapshot record");

        let snapshots = super::read_snapshots(&fixture.path).expect("read snapshots");
        assert_eq!(snapshots.len(), 1);
        let snapshot = &snapshots[0];
        assert_eq!(snapshot.snapshot_id, "snap_contract_1");
        assert_eq!(snapshot.at_message_id, user_one);
        assert_eq!(snapshot.ordinal, 3);
        assert!(snapshot.complete);
        let entry = snapshot.files.get("src/a.rs").expect("file entry");
        let pre = entry.pre.as_ref().expect("pre-image reference");
        assert_eq!(pre.sha256, "ab12");
        assert_eq!(pre.size, 812);
        assert_eq!(pre.mode, Some(0o644));
        assert_eq!(pre.encoding, "raw");
        assert_eq!(entry.post_sha256.as_deref(), Some("cd34"));
        assert_eq!(snapshots[0].untracked, ["docs/x.md"]);
        assert_eq!(snapshots[0].limitations, ["observation_budget"]);

        // The audit record never changes the folded conversation.
        assert_eq!(
            texts(&fixture.transcript()),
            ["one".to_string(), "one reply".to_string()]
        );

        // A code-only rewind (files restored, no conversation cut) is audit
        // data: kept counts are zero and must not truncate the transcript.
        let mut code_only = rewind_record(&fixture.session_id, &user_one, 0);
        code_only.conversation = None;
        fixture
            .writer
            .append_session_rewind(code_only)
            .expect("append code-only rewind record");
        assert_eq!(
            texts(&fixture.transcript()),
            ["one".to_string(), "one reply".to_string()]
        );
    });
}

#[test]
fn read_transcript_until_effective_uses_the_rewound_history() {
    crate::history::with_redirected_orca_home("rewind-until-effective", |_home| {
        let mut fixture = Fixture::new();
        let (_user_one, assistant_one) = fixture.append_turn("first", "first reply");
        let (user_two, _) = fixture.append_turn("second", "second reply");
        let ids = fixture.message_ids();
        assert_eq!(ids.len(), 4);

        fixture.append_rewind(&assistant_one, 2);
        let post_rewind = fixture.append_user("after rewind");

        // The effective history is [first, first reply, after rewind]; the
        // third raw message (user_two) was rewound away.
        let effective = super::read_transcript_until_effective(&fixture.path, &post_rewind)
            .expect("effective transcript through the appended message");
        assert_eq!(
            texts(&effective),
            [
                "first".to_string(),
                "first reply".to_string(),
                "after rewind".to_string()
            ]
        );

        let until_first_turn =
            super::read_transcript_until_effective(&fixture.path, &assistant_one)
                .expect("until first turn");
        assert_eq!(
            texts(&until_first_turn),
            ["first".to_string(), "first reply".to_string()]
        );

        // A message that a later rewind removed is not part of the effective
        // history, so it can never be a boundary.
        let error = super::read_transcript_until_effective(&fixture.path, &user_two)
            .expect_err("rewound-away message must not match");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            error
                .to_string()
                .contains(&format!("no saved message matches '{user_two}'")),
            "unexpected error: {error}"
        );
    });
}

#[test]
fn unknown_conversation_rewind_kind_fails_closed() {
    crate::history::with_redirected_orca_home("rewind-unknown-kind", |_home| {
        let mut fixture = Fixture::new();
        let (user_one, _) = fixture.append_turn("hello", "hi");
        let mut record = rewind_record(&fixture.session_id, &user_one, 1);
        record.conversation = Some(ConversationRewindRecord {
            kind: "summarize_from".to_string(),
            summary: Some("summarized".to_string()),
            summary_usage: None,
        });
        fixture
            .writer
            .append_session_rewind(record)
            .expect("append reserved-kind rewind record");

        let error =
            read_transcript(&fixture.path).expect_err("unknown rewind kind must fail closed");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("summarize_from"),
            "error must name the unsupported kind: {error}"
        );
    });
}

#[test]
fn transcripts_without_rewind_records_load_unchanged() {
    crate::history::with_redirected_orca_home("rewind-legacy", |_home| {
        let mut fixture = Fixture::new();
        fixture.append_turn("legacy user", "legacy assistant");

        let transcript = fixture.transcript();
        assert_eq!(
            texts(&transcript),
            ["legacy user".to_string(), "legacy assistant".to_string()]
        );
        assert!(
            super::read_snapshots(&fixture.path)
                .expect("read snapshots")
                .is_empty()
        );
        let effective =
            super::read_effective_user_messages(&fixture.path).expect("effective user messages");
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].ordinal, 0);
        assert_eq!(effective[0].prompt, "legacy user");
    });
}

/// A persisted internal image-analysis sidecar: a complete record that
/// `is_internal_image_analysis` recognizes, not a human prompt.
fn internal_image_analysis_record() -> String {
    format!(
        "{IMAGE_ANALYSIS_MESSAGE_PREFIX}{}]\n{}",
        "a".repeat(64),
        serde_json::json!({
            "trust": "untrusted_observation_data",
            "description": "Save button",
            "query": "inspect this window",
            "image_count": 1,
            "provenance": serde_json::Value::Null,
        })
    )
}

#[test]
fn effective_user_messages_include_pinned_prompts_and_skip_internal_image_analysis() {
    crate::history::with_redirected_orca_home("rewind-effective-users", |_home| {
        let mut fixture = Fixture::new();
        let (first, _) = fixture.append_turn("first prompt", "reply one");
        // Headless `orca exec --save-history` persists real prompts as pinned
        // user turns; those must stay rewindable.
        fixture
            .writer
            .append_message(&Message::pinned_user("pinned prompt".to_string()))
            .expect("append pinned user message");
        let pinned = fixture.last_message_id();
        // Internal image-analysis sidecars are observations, not prompts.
        fixture.append_user(&internal_image_analysis_record());
        let (second, _) = fixture.append_turn("second prompt", "reply two");

        let effective =
            super::read_effective_user_messages(&fixture.path).expect("effective user messages");
        assert_eq!(
            effective
                .iter()
                .map(|message| (
                    message.ordinal,
                    message.prompt.as_str(),
                    message.message_id.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                (0, "first prompt", first.as_str()),
                (1, "pinned prompt", pinned.as_str()),
                (2, "second prompt", second.as_str()),
            ]
        );
    });
}

#[test]
fn effective_user_messages_drop_rewound_messages() {
    crate::history::with_redirected_orca_home("rewind-effective-drop", |_home| {
        let mut fixture = Fixture::new();
        fixture.append_turn("first prompt", "reply one");
        let (_user_two, assistant_two) = fixture.append_turn("second prompt", "reply two");
        fixture.append_turn("third prompt", "reply three");

        // Keep the first four messages: the third user message is rewound away.
        fixture.append_rewind(&assistant_two, 4);

        let transcript = fixture.transcript();
        assert_eq!(texts(&transcript).len(), 4);

        let effective =
            super::read_effective_user_messages(&fixture.path).expect("effective user messages");
        assert_eq!(
            effective
                .iter()
                .map(|message| message.prompt.as_str())
                .collect::<Vec<_>>(),
            ["first prompt", "second prompt"]
        );
    });
}

#[test]
fn transcript_before_message_returns_self_consistent_resumable_cut() {
    crate::history::with_redirected_orca_home("rewind-cut", |_home| {
        let mut fixture = Fixture::new();
        let (first, _) = fixture.append_turn("first prompt", "reply one");
        let (second, _) = fixture.append_turn("second prompt", "reply two");
        fixture.append_turn("third prompt", "reply three");

        let cut = super::read_transcript_before_message(&fixture.path, &second)
            .expect("cut before the second user message");
        assert_eq!(cut.prompt, "second prompt");
        assert_eq!(cut.ordinal, 1);
        assert_eq!(cut.kept_messages, 2);
        assert_eq!(cut.kept_compactions, 0);
        assert_eq!(cut.kept_summaries, 0);
        assert_eq!(cut.transcript.messages.len(), 2);
        assert_eq!(
            texts(&cut.transcript),
            ["first prompt".to_string(), "reply one".to_string()]
        );

        // The cut can be turned straight into a resumable conversation.
        let conversation =
            crate::history::resume_conversation(&cut.transcript, "system prompt".to_string());
        assert!(!conversation.messages.is_empty());

        // Cutting before the first user message is valid and keeps nothing.
        let first_cut = super::read_transcript_before_message(&fixture.path, &first)
            .expect("cut before first message");
        assert_eq!(first_cut.kept_messages, 0);
        assert!(first_cut.transcript.messages.is_empty());
        assert_eq!(first_cut.ordinal, 0);

        // A message that a later rewind removed is not part of the effective
        // history.
        fixture.append_rewind(&second, 2);
        let third_id = fixture.message_ids()[5].clone();
        let error = super::read_transcript_before_message(&fixture.path, &third_id)
            .expect_err("rewound-away message must not match");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    });
}
