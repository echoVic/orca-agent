use std::fs::{self, OpenOptions};
use std::io::Write;

use base64::Engine;
use orca_core::conversation::{ImageDetail, ImageInput, ImageSource, Message};
use orca_core::thread_identity::TurnId;

use super::assets;
use super::reader::INDEX_BUDGET;
use super::retention::{SessionRetentionPolicy, retain_sessions};
use super::types::{SessionRecord, StoredMessage, StoredSessionHealth};
use super::writer::{
    MAX_SESSION_LINE_BYTES, read_session_meta, read_transcript, scan_session, write_record_line,
};
use super::{JsonlThreadStore, SessionWriter};

fn image_message(size: usize) -> Message {
    Message::user_with_images(
        "inspect image".into(),
        vec![ImageInput {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: base64::engine::general_purpose::STANDARD.encode(vec![23; size]),
            },
            detail: ImageDetail::High,
        }],
    )
}

#[test]
fn large_image_is_stored_once_and_round_trips_through_resume() {
    crate::history::with_redirected_orca_home("asset-round-trip", |home| {
        let mut writer = SessionWriter::start(home, "mock", None, "image").unwrap();
        writer.enter_turn(TurnId::new());
        let message = image_message(5 * 1024 * 1024);
        for _ in 0..3 {
            writer.append_message(&message).unwrap();
        }
        let root = assets::directory(writer.path());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert!(fs::metadata(writer.path()).unwrap().len() < 8 * 1024);
        let raw = fs::read_to_string(writer.path()).unwrap();
        assert!(raw.contains("session.record_with_assets"));
        assert!(raw.contains("\"sha256\""));
        let transcript = read_transcript(writer.path()).unwrap();
        assert_eq!(transcript.messages.len(), 3);
        for restored in transcript.messages {
            let Message::User { images, .. } = restored else {
                panic!("user image");
            };
            let ImageSource::Base64 { data, .. } = &images[0].source else {
                panic!("hydrated");
            };
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
                vec![23; 5 * 1024 * 1024]
            );
        }
        let mut resumed = SessionWriter::append_to_existing(writer.path().to_path_buf()).unwrap();
        resumed.enter_turn(TurnId::new());
        resumed
            .append_message(&Message::user("next".into()))
            .unwrap();
        assert_eq!(read_transcript(resumed.path()).unwrap().messages.len(), 4);
    });
}

#[test]
fn asset_codec_preserves_surface_payload_and_rejects_corruption() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("surface.jsonl");
    let data = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
    let source = serde_json::json!({
        "Base64": { "media_type": "image/png", "data": data, "digest": [7, 8, 9] }
    });
    let original = serde_json::json!({
        "events": [
            { "request": { "source": source.clone() }},
            { "admitted": { "source": source }},
        ]
    });
    let stored = assets::externalize(&path, original.clone()).unwrap();
    assert_eq!(fs::read_dir(assets::directory(&path)).unwrap().count(), 1);
    assert_eq!(
        assets::hydrate(&path, stored.clone(), true).unwrap(),
        original
    );
    let mut malicious = stored.clone();
    malicious["assets"][0]["sha256"] = serde_json::json!("../../outside");
    assert!(assets::hydrate(&path, malicious, true).is_err());
    let mut malicious = stored.clone();
    malicious["assets"][0]["pointer"] = serde_json::json!("/events/1");
    assert!(assets::hydrate(&path, malicious, true).is_err());
    let blob = fs::read_dir(assets::directory(&path))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&blob, [9u8; 4]).unwrap();
    assert!(assets::hydrate(&path, stored.clone(), true).is_err());
    // Catalog reads never allocate or fetch image data.
    assert!(assets::hydrate(&path, stored.clone(), false).is_ok());
    fs::remove_file(blob).unwrap();
    assert!(assets::hydrate(&path, stored, true).is_err());
}

#[test]
fn missing_asset_prevents_append_without_changing_transcript() {
    crate::history::with_redirected_orca_home("asset-missing", |home| {
        let mut writer = SessionWriter::start(home, "mock", None, "missing image").unwrap();
        writer.enter_turn(TurnId::new());
        writer.append_message(&image_message(1024)).unwrap();
        let before = fs::read(writer.path()).unwrap();
        assets::remove_directory(writer.path()).unwrap();
        assert!(SessionWriter::append_to_existing(writer.path().to_path_buf()).is_err());
        assert_eq!(fs::read(writer.path()).unwrap(), before);
    });
}

#[test]
fn reading_session_meta_does_not_hydrate_later_images() {
    crate::history::with_redirected_orca_home("asset-meta", |home| {
        let mut writer = SessionWriter::start(home, "mock", None, "metadata only").unwrap();
        writer.enter_turn(TurnId::new());
        writer.append_message(&image_message(1024)).unwrap();
        assets::remove_directory(writer.path()).unwrap();
        assert_eq!(
            read_session_meta(writer.path()).unwrap().title,
            "metadata only"
        );
        assert!(read_transcript(writer.path()).is_err());
    });
}

#[test]
fn legacy_inline_images_remain_readable() {
    crate::history::with_redirected_orca_home("asset-legacy", |home| {
        let writer = SessionWriter::start(home, "mock", None, "legacy").unwrap();
        let record = SessionRecord::Message {
            id: None,
            turn_id: None,
            message: StoredMessage::from(&image_message(2 * 1024 * 1024)),
        };
        let mut file = OpenOptions::new().append(true).open(writer.path()).unwrap();
        write_record_line(&mut file, &record).unwrap();
        assert!(!assets::directory(writer.path()).exists());
        assert_eq!(read_transcript(writer.path()).unwrap().messages.len(), 1);
        assert!(SessionWriter::append_to_existing(writer.path().to_path_buf()).is_ok());
    });
}

#[test]
fn assets_survive_fork_rename_archive_compress_and_portable_export() {
    crate::history::with_redirected_orca_home("asset-lifecycle", |home| {
        let store = JsonlThreadStore::new();
        let mut parent = SessionWriter::start(home, "mock", None, "parent").unwrap();
        parent.enter_turn(TurnId::new());
        parent.append_message(&image_message(1024 * 1024)).unwrap();
        let parent_id = parent.session_id().unwrap();
        let loaded = read_transcript(parent.path()).unwrap();
        let mut fork = SessionWriter::start_from_meta(crate::history::create_fork_meta(
            home,
            "mock",
            None,
            "fork",
            parent_id.clone(),
        ))
        .unwrap();
        fork.enter_turn(TurnId::new());
        fork.append_message(&loaded.messages[0]).unwrap();
        let fork_id = fork.session_id().unwrap();
        let fork_path = fork.path().to_path_buf();
        drop(fork);
        drop(parent);
        store.delete_session(&parent_id).unwrap();
        assert_eq!(store.load_session(&fork_id).unwrap().messages.len(), 1);
        store.rename_session(&fork_id, "renamed").unwrap();
        let archive = store.archive_session(&fork_id).unwrap();
        assert!(!assets::directory(&fork_path).exists());
        assert!(assets::directory(&archive).exists());
        let compressed = store.compress_session(&fork_id).unwrap();
        assert_eq!(assets::directory(&compressed), assets::directory(&archive));
        let resumed = SessionWriter::append_to_existing(compressed).unwrap();
        assert_eq!(
            read_transcript(resumed.path()).unwrap().meta.title,
            "renamed"
        );
        let export = home.join("portable.jsonl");
        store.export_session(&fork_id, &export).unwrap();
        store.delete_session(&fork_id).unwrap();
        assert!(!assets::directory(&archive).exists());
        assert_eq!(read_transcript(&export).unwrap().messages.len(), 1);
    });
}

#[test]
fn record_limit_is_64_mib_and_rejection_does_not_modify_the_file() {
    assert_eq!(MAX_SESSION_LINE_BYTES, 64 * 1024 * 1024);
    crate::history::with_redirected_orca_home("record-limit", |home| {
        let mut writer = SessionWriter::start(home, "mock", None, "limit").unwrap();
        writer.enter_turn(TurnId::new());
        let before = fs::read(writer.path()).unwrap();
        assert!(
            writer
                .append_message(&Message::user("x".repeat(MAX_SESSION_LINE_BYTES)))
                .is_err()
        );
        assert_eq!(fs::read(writer.path()).unwrap(), before);
        assert!(writer.conversation_records().is_empty());
        assert_eq!(
            scan_session(writer.path()).unwrap().health,
            StoredSessionHealth::Healthy
        );
    });
}

#[test]
fn large_session_streams_past_former_encoded_decoded_and_record_count_limits() {
    crate::history::with_redirected_orca_home("large-session", |home| {
        let store = JsonlThreadStore::new();
        let writer = SessionWriter::start(home, "mock", None, "large").unwrap();
        let id = writer.session_id().unwrap();
        let path = writer.path().to_path_buf();
        // A replaceable plan keeps the replay projection small even though
        // its history exceeds both former whole-session byte limits.
        let record = SessionRecord::PlanState {
            explanation: Some("x".repeat(1024 * 1024)),
            plan: Vec::new(),
        };
        let mut line = Vec::new();
        write_record_line(&mut line, &record).unwrap();
        let mut file =
            std::io::BufWriter::new(OpenOptions::new().append(true).open(&path).unwrap());
        for _ in 0..130 {
            file.write_all(&line).unwrap();
        }
        for _ in 0..100_001 {
            file.write_all(b"{\"type\":\"event.sequence.reserved\",\"next_seq\":123}\n")
                .unwrap();
        }
        file.flush().unwrap();
        drop(file);
        assert!(fs::metadata(&path).unwrap().len() > 128 * 1024 * 1024);
        let scan = scan_session(&path).unwrap();
        assert_eq!(scan.health, StoredSessionHealth::InspectionLimited);
        assert!(!scan.health.blocks_mutation());
        assert!(scan.records.len() < INDEX_BUDGET.records);
        assert_eq!(read_transcript(&path).unwrap().next_event_seq, 123);
        let mut resumed = SessionWriter::append_to_existing(path.clone()).unwrap();
        resumed.enter_turn(TurnId::new());
        resumed
            .append_message(&Message::user("after old limits".into()))
            .unwrap();
        assert_eq!(store.load_session(&id).unwrap().messages.len(), 1);
        drop(resumed);
        drop(writer);
        let compressed = store.compress_session(&id).unwrap();
        assert!(fs::metadata(&compressed).unwrap().len() < 64 * 1024 * 1024);
        assert_eq!(read_transcript(&compressed).unwrap().messages.len(), 1);
        assert!(SessionWriter::append_to_existing(compressed).is_ok());
    });
}

#[test]
fn corruption_after_index_budget_still_blocks_authoritative_replay() {
    crate::history::with_redirected_orca_home("large-corrupt", |home| {
        let writer = SessionWriter::start(home, "mock", None, "corrupt tail").unwrap();
        let mut file = OpenOptions::new().append(true).open(writer.path()).unwrap();
        // Whitespace consumes inspection budget without retained records.
        for _ in 0..INDEX_BUDGET.records {
            file.write_all(b"\n").unwrap();
        }
        file.write_all(b"{bad}\n").unwrap();
        assert_eq!(
            scan_session(writer.path()).unwrap().health,
            StoredSessionHealth::InspectionLimited
        );
        assert!(read_transcript(writer.path()).is_err());
        assert!(SessionWriter::append_to_existing(writer.path().to_path_buf()).is_err());
    });
}

#[test]
fn retention_is_opt_in_archived_only_and_accounts_for_images() {
    crate::history::with_redirected_orca_home("retention", |home| {
        let store = JsonlThreadStore::new();
        let active = SessionWriter::start(home, "mock", None, "active").unwrap();
        let mut archived = SessionWriter::start(home, "mock", None, "archive").unwrap();
        archived.enter_turn(TurnId::new());
        archived.append_message(&image_message(4096)).unwrap();
        let id = archived.session_id().unwrap();
        drop(archived);
        let archived_path = store.archive_session(&id).unwrap();
        let policy = SessionRetentionPolicy {
            max_bytes: Some(0),
            older_than_days: None,
        };
        let preview = retain_sessions(&policy, false).unwrap();
        assert_eq!(preview.candidates.len(), 1);
        assert!(preview.candidates[0].bytes >= 4096);
        assert!(preview.deleted.is_empty());
        assert_eq!(
            preview.bytes_after,
            preview.bytes_before - preview.candidates[0].bytes,
        );
        let feasible = retain_sessions(
            &SessionRetentionPolicy {
                max_bytes: Some(preview.bytes_after),
                older_than_days: None,
            },
            false,
        )
        .unwrap();
        assert!(feasible.quota_satisfied);
        assert!(archived_path.exists());
        let lease = orca_platform::fs::ExclusiveFileLock::acquire(
            &archived_path.with_extension("surface-owner.lock"),
        )
        .unwrap();
        let blocked = retain_sessions(&policy, true).unwrap();
        assert_eq!(blocked.skipped, [archived_path.clone()]);
        drop(lease);
        let applied = retain_sessions(&policy, true).unwrap();
        assert_eq!(applied.deleted, [archived_path.clone()]);
        assert!(
            !applied.quota_satisfied,
            "active session is protected even above quota"
        );
        assert!(active.path().exists());
        assert!(!assets::directory(&archived_path).exists());
        assert!(retain_sessions(&SessionRetentionPolicy::default(), true).is_err());
    });
}

#[cfg(unix)]
#[test]
fn asset_symlinks_are_never_followed() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("session.jsonl");
    let original = serde_json::json!({"type":"base64", "media_type":"image/png", "data":"AQID"});
    let stored = assets::externalize(&path, original).unwrap();
    let blob = fs::read_dir(assets::directory(&path))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let outside = root.path().join("outside");
    fs::rename(&blob, &outside).unwrap();
    symlink(&outside, &blob).unwrap();
    assert!(assets::hydrate(&path, stored, true).is_err());
    assert_eq!(fs::read(outside).unwrap(), [1, 2, 3]);
}
