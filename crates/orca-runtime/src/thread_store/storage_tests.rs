use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use orca_core::conversation::{Conversation, ImageDetail, ImageInput, ImageSource, Message};
use orca_core::thread_identity::TurnId;
use orca_core::tool_images::SUPERSEDED_TOOL_IMAGE_NOTE;

use super::assets::{self, HydrateImages};
use super::local::sessions_dir;
use super::reader::INDEX_BUDGET;
use super::retention::{SessionRetentionPolicy, retain_sessions};
use super::types::{
    SessionRecord, SortDirection, StoredMessage, StoredSessionHealth, ThreadSortKey, ThreadStore,
};
use super::writer::{
    MAX_SESSION_LINE_BYTES, read_records, read_session_meta, read_transcript, scan_session,
    write_record_line,
};
use super::{JsonlThreadStore, SessionWriter};

const SESSION_META: &str = r#"{"type":"session.meta","schema_version":1,"session_id":"compat","cwd":"/workspace","provider":"mock","title":"compat","created_at":"2026-11-27T00:00:00Z"}"#;
const FIRST_MESSAGE: &str = r#"{"type":"conversation.message","message":{"role":"user","content":"before compatibility marker"}}"#;
const SECOND_MESSAGE: &str = r#"{"type":"conversation.message","message":{"role":"user","content":"after compatibility marker"}}"#;
const UNKNOWN_RECORD: &str = r#"{"type":"future.thing","x":1}"#;
const SKIPPABLE_RECORD: &str = r#"{"type":"future.thing","ignorable":true}"#;

fn write_compat_session(directory: &Path, meta: &str, between: &str) -> PathBuf {
    fs::create_dir_all(directory).unwrap();
    let path = directory.join("compat.jsonl");
    fs::write(
        &path,
        format!("{meta}\n{FIRST_MESSAGE}\n{between}{SECOND_MESSAGE}\n"),
    )
    .unwrap();
    path
}

fn write_session_with_later_schema_two_meta() -> PathBuf {
    let newer = SESSION_META.replace(r#""schema_version":1"#, r#""schema_version":2"#);
    write_compat_session(&sessions_dir(), SESSION_META, &format!("{newer}\n"))
}

#[test]
fn resume_writer_refuses_a_later_schema_two_meta() {
    let root = tempfile::tempdir().unwrap();
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = write_session_with_later_schema_two_meta();
        let compressed = path.with_extension("jsonl.zst");
        let source = fs::read(&path).unwrap();
        let original = zstd::stream::encode_all(source.as_slice(), 0).unwrap();
        fs::write(&compressed, &original).unwrap();
        fs::remove_file(&path).unwrap();

        let error = SessionWriter::append_to_existing(compressed.clone())
            .err()
            .expect("resume must reject the newer transcript before restoring it");
        assert!(error.to_string().contains("upgrade Orca"), "{error}");
        assert!(!path.exists());
        assert_eq!(fs::read(compressed).unwrap(), original);
    });
}

#[test]
fn rename_refuses_a_later_schema_two_meta() {
    let root = tempfile::tempdir().unwrap();
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = write_session_with_later_schema_two_meta();
        let original = fs::read(&path).unwrap();
        let error = JsonlThreadStore::new()
            .rename_session("compat", "new title")
            .unwrap_err();
        assert!(error.to_string().contains("upgrade Orca"), "{error}");
        assert_eq!(fs::read(path).unwrap(), original);
    });
}

#[test]
fn export_refuses_a_later_schema_two_meta() {
    let root = tempfile::tempdir().unwrap();
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = write_session_with_later_schema_two_meta();
        let original = fs::read(&path).unwrap();
        let target = root.path().join("export.jsonl");
        let error = JsonlThreadStore::new()
            .export_session("compat", &target)
            .unwrap_err();
        assert!(error.to_string().contains("upgrade Orca"), "{error}");
        assert!(!target.exists());
        assert_eq!(fs::read(path).unwrap(), original);
    });
}

#[test]
fn an_unknown_record_type_reports_newer_version() {
    let root = tempfile::tempdir().unwrap();
    let path = write_compat_session(root.path(), SESSION_META, &format!("{UNKNOWN_RECORD}\n"));
    let scan = scan_session(&path).unwrap();
    assert_eq!(scan.health.as_str(), "newer_version");
    let issue = scan.health_issue.unwrap();
    assert_eq!(issue.code, "unknown_record_type");
    assert_eq!(issue.record_type.as_deref(), Some("future.thing"));
    let error = read_transcript(&path).unwrap_err().to_string();
    for text in ["newer version of Orca", "future.thing", "upgrade"] {
        assert!(error.contains(text), "{error}");
    }
}

#[test]
fn an_ignorable_unknown_record_is_skipped() {
    let root = tempfile::tempdir().unwrap();
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = write_compat_session(
            &sessions_dir(),
            SESSION_META,
            concat!(r#"{"type":"future.thing","ignorable":true,"x":1}"#, "\n"),
        );
        assert_eq!(
            scan_session(&path).unwrap().health,
            StoredSessionHealth::Healthy
        );
        let transcript = read_transcript(&path).unwrap();
        assert!(
            matches!(transcript.messages.as_slice(), [Message::User { content: before, .. }, Message::User { content: after, .. }]
            if before == "before compatibility marker" && after == "after compatibility marker")
        );
        let hits = JsonlThreadStore::new()
            .search_sessions("future.thing", false)
            .unwrap();
        assert!(hits.is_empty(), "{hits:?}");
    });
}

#[test]
fn a_transcript_with_only_skipped_records_is_not_healthy() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("compat.jsonl");
    for contents in [SKIPPABLE_RECORD, FIRST_MESSAGE, " \n", ""] {
        fs::write(&path, contents).unwrap();
        let scan = scan_session(&path).unwrap();
        assert_eq!(scan.health, StoredSessionHealth::Quarantined, "{contents}");
        assert_eq!(scan.health_issue.unwrap().code, "missing_session_meta");
    }

    fs::write(
        &path,
        format!("{SKIPPABLE_RECORD}\n").repeat(INDEX_BUDGET.records + 1),
    )
    .unwrap();
    let scan = scan_session(&path).unwrap();
    assert_eq!(scan.health, StoredSessionHealth::InspectionLimited);
    assert_eq!(scan.health_issue.unwrap().code, "inspection_budget");
}

#[test]
fn export_refuses_a_transcript_without_metadata() {
    let root = tempfile::tempdir().unwrap();
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = sessions_dir().join("compat.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = format!("{SKIPPABLE_RECORD}\n\n");
        fs::write(&path, &original).unwrap();
        let store = JsonlThreadStore::new();
        let target = root.path().join("export.jsonl");
        let error = store.export_session("compat", &target).unwrap_err();
        assert!(
            error.to_string().contains("missing_session_meta"),
            "{error}"
        );
        assert!(!target.exists());

        let error = store
            .update_thread_metadata(
                "compat",
                super::types::ThreadMetadataPatch {
                    title: Some("new title".to_owned()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("missing_session_meta"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    });
}

#[test]
fn only_boolean_true_marks_a_record_ignorable() {
    let root = tempfile::tempdir().unwrap();
    for flag in [r#""yes""#, "1"] {
        let path = write_compat_session(
            root.path(),
            SESSION_META,
            &format!("{{\"type\":\"future.thing\",\"ignorable\":{flag}}}\n"),
        );
        let scan = scan_session(&path).unwrap();
        assert_eq!(scan.health.as_str(), "newer_version", "ignorable: {flag}");
        assert_eq!(scan.health_issue.unwrap().code, "unknown_record_type");
    }
}

#[test]
fn a_repeated_ignorable_key_is_malformed() {
    let root = tempfile::tempdir().unwrap();
    let record = r#"{"type":"future.thing","ignorable":false,"ignorable":true}"#;
    let path = write_compat_session(root.path(), SESSION_META, &format!("{record}\n"));
    let scan = scan_session(&path).unwrap();
    assert_eq!(scan.health, StoredSessionHealth::Quarantined);
    assert_eq!(scan.health_issue.unwrap().code, "malformed_record");
    assert!(!super::reader::is_ignorable_search_line(record));
}

#[test]
fn a_repeated_key_in_a_newer_meta_is_malformed() {
    let root = tempfile::tempdir().unwrap();
    for meta in [
        SESSION_META.replace(
            r#""schema_version":1"#,
            r#""schema_version":2,"added":1,"added":2"#,
        ),
        r#"{"type":"session.meta","schema_version":2,"metadata":{},"metadata":[]}"#.to_owned(),
    ] {
        let path = write_compat_session(root.path(), &meta, "");
        let scan = scan_session(&path).unwrap();
        assert_eq!(scan.health, StoredSessionHealth::Quarantined);
        assert_eq!(scan.health_issue.unwrap().code, "malformed_record");
    }
}

#[test]
fn a_newer_schema_version_reports_newer_version() {
    let root = tempfile::tempdir().unwrap();
    let meta = SESSION_META.replace(r#""schema_version":1"#, r#""schema_version":2"#);
    let path = write_compat_session(root.path(), &meta, "");
    let scan = scan_session(&path).unwrap();
    assert_eq!(scan.health.as_str(), "newer_version");
    assert_eq!(scan.health_issue.unwrap().code, "newer_schema_version");

    let changed_shape = r#"{"type":"session.meta","schema_version":2,"metadata":{}}"#;
    let path = write_compat_session(root.path(), changed_shape, "");
    assert_eq!(
        scan_session(&path).unwrap().health.as_str(),
        "newer_version"
    );

    let duplicated_version = SESSION_META.replace(
        r#""schema_version":1"#,
        r#""schema_version":1,"schema_version":2"#,
    );
    let path = write_compat_session(root.path(), &duplicated_version, "");
    let scan = scan_session(&path).unwrap();
    assert_eq!(scan.health, StoredSessionHealth::Quarantined);
    assert_eq!(scan.health_issue.unwrap().code, "malformed_record");
}

#[test]
fn newer_schema_metadata_remains_visible_in_project_listings() {
    let root = tempfile::tempdir().unwrap();
    let meta = format!(
        "{},\"parent_id\":\"parent-thread\"}}",
        SESSION_META
            .replace(r#""schema_version":1"#, r#""schema_version":2"#)
            .trim_end_matches('}')
    );
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = write_compat_session(&sessions_dir(), &meta, "");
        let page = crate::history::list_session_page_in_cwd(0, 20, "/workspace").unwrap();
        let summary = page
            .sessions
            .iter()
            .find(|summary| summary.path == path)
            .expect("session stays in its project");
        assert_eq!(summary.health.as_str(), "newer_version");
        assert_eq!(summary.parent_id.as_deref(), Some("parent-thread"));
        assert!(summary.is_child_thread());
    });
}

#[test]
fn session_meta_accepts_unknown_fields() {
    let root = tempfile::tempdir().unwrap();
    let meta = format!(
        "{},\"added_later\":true}}",
        SESSION_META.trim_end_matches('}')
    );
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = write_compat_session(&sessions_dir(), &meta, "");
        assert_eq!(
            scan_session(&path).unwrap().health,
            StoredSessionHealth::Healthy
        );

        let store = JsonlThreadStore::new();
        store.rename_session("compat", "renamed").unwrap();
        let export = root.path().join("portable.jsonl");
        store.export_session("compat", &export).unwrap();
        for transcript in [path, export] {
            let bytes = fs::read(transcript).unwrap();
            let meta: serde_json::Value =
                serde_json::from_slice(bytes.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
            assert_eq!(meta["added_later"], true);
            assert_eq!(meta["title"], "renamed");
        }
    });
}

#[test]
fn a_known_type_with_bad_fields_still_quarantines() {
    let root = tempfile::tempdir().unwrap();
    for record in [
        r#"{"type":"conversation.message","message":42,"ignorable":true}"#,
        r#"{"type":"session.record_with_assets","ignorable":true}"#,
    ] {
        let path = write_compat_session(root.path(), SESSION_META, &format!("{record}\n"));
        let scan = scan_session(&path).unwrap();
        assert_eq!(scan.health, StoredSessionHealth::Quarantined);
        assert_eq!(
            scan.health_issue.unwrap().code,
            if record.contains("with_assets") {
                "invalid_image_asset"
            } else {
                "malformed_record"
            }
        );
    }
}

#[test]
fn the_first_problem_in_the_file_decides_its_health() {
    let root = tempfile::tempdir().unwrap();
    let path = write_compat_session(root.path(), SESSION_META, &format!("{UNKNOWN_RECORD}\n"));
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(br#"{"type":"conversation.message","message":{"role":"user""#)
        .unwrap();
    assert_eq!(
        scan_session(&path).unwrap().health.as_str(),
        "newer_version"
    );

    let path = write_compat_session(
        root.path(),
        SESSION_META,
        &format!("not JSON\n{UNKNOWN_RECORD}\n"),
    );
    assert_eq!(
        scan_session(&path).unwrap().health,
        StoredSessionHealth::Quarantined
    );
}

#[test]
fn listing_reports_newer_version() {
    let root = tempfile::tempdir().unwrap();
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = write_compat_session(
            &sessions_dir(),
            SESSION_META,
            &format!("{UNKNOWN_RECORD}\n"),
        );
        let store = JsonlThreadStore::new();
        let sessions = store
            .list_sessions_with_archived(usize::MAX, false)
            .unwrap();
        let summary = sessions
            .iter()
            .find(|summary| summary.path == path)
            .unwrap();
        assert_eq!(
            serde_json::to_value(summary).unwrap()["health"],
            "newer_version"
        );

        let page = store
            .search_threads(
                "compatibility marker",
                None,
                20,
                false,
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
            )
            .unwrap();
        assert!(page.data.is_empty());
        assert!(
            page.diagnostics
                .iter()
                .any(
                    |diagnostic| diagnostic.storage_identity == summary.storage_identity
                        && diagnostic.health.as_str() == "newer_version"
                )
        );

        let target = root.path().join("export.jsonl");
        let error = store
            .export_session(&summary.storage_identity, &target)
            .unwrap_err();
        assert!(error.to_string().contains("upgrade Orca"));
        assert!(!target.exists());
    });
}

#[test]
fn resume_of_a_newer_session_is_refused_with_the_upgrade_message() {
    let root = tempfile::tempdir().unwrap();
    crate::history::with_test_orca_home(root.path(), |_| {
        let path = write_compat_session(
            &sessions_dir(),
            SESSION_META,
            &format!("{UNKNOWN_RECORD}\n"),
        );
        let before = fs::read(&path).unwrap();
        let store = JsonlThreadStore::new();
        for error in [
            store.load_session("compat").unwrap_err(),
            store.rename_session("compat", "new title").unwrap_err(),
        ] {
            assert!(error.to_string().contains("upgrade Orca"), "{error}");
        }
        assert_eq!(fs::read(path).unwrap(), before);
    });
}

#[test]
fn a_rewrite_keeps_an_ignorable_line_in_place() {
    const SKIPPED_LINE: &str =
        " \t{ \"x\": [1,  2], \"ignorable\": true, \"type\": \"future.thing\" } \t\r\n";
    for compressed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        crate::history::with_test_orca_home(root.path(), |_| {
            let path = write_compat_session(&sessions_dir(), SESSION_META, SKIPPED_LINE);
            let store = JsonlThreadStore::new();
            let path = if compressed {
                store.compress_session("compat").unwrap()
            } else {
                path
            };
            assert_eq!(store.rename_session("compat", "renamed").unwrap(), path);

            let bytes = fs::read(&path).unwrap();
            let rewritten = if compressed {
                zstd::stream::decode_all(bytes.as_slice()).unwrap()
            } else {
                bytes
            };
            let export = root.path().join("portable.jsonl");
            store.export_session("compat", &export).unwrap();
            for bytes in [rewritten, fs::read(export).unwrap()] {
                let lines = bytes
                    .split_inclusive(|byte| *byte == b'\n')
                    .collect::<Vec<_>>();
                assert_eq!(lines.len(), 4);
                assert_eq!(lines[2], SKIPPED_LINE.as_bytes());
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(lines[0]).unwrap()["title"],
                    "renamed"
                );
                for (index, expected) in [(1, FIRST_MESSAGE), (3, SECOND_MESSAGE)] {
                    let actual = serde_json::from_slice::<serde_json::Value>(lines[index]).unwrap();
                    let expected = serde_json::from_str::<serde_json::Value>(expected).unwrap();
                    assert_eq!(actual["message"]["content"], expected["message"]["content"]);
                }
            }
        });
    }
}

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
        assets::hydrate(&path, stored.clone(), HydrateImages::All).unwrap(),
        original
    );
    let mut malicious = stored.clone();
    malicious["assets"][0]["sha256"] = serde_json::json!("../../outside");
    for images in [
        HydrateImages::All,
        HydrateImages::NotToolMessages,
        HydrateImages::None,
    ] {
        assert!(assets::hydrate(&path, malicious.clone(), images).is_err());
    }
    let mut malicious = stored.clone();
    malicious["assets"][0]["pointer"] = serde_json::json!("/events/1");
    for images in [
        HydrateImages::All,
        HydrateImages::NotToolMessages,
        HydrateImages::None,
    ] {
        assert!(assets::hydrate(&path, malicious.clone(), images).is_err());
    }
    let blob = fs::read_dir(assets::directory(&path))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&blob, [9u8; 4]).unwrap();
    assert!(assets::hydrate(&path, stored.clone(), HydrateImages::All).is_err());
    // Catalog reads never allocate or fetch image data.
    assert!(assets::hydrate(&path, stored.clone(), HydrateImages::None).is_ok());
    fs::remove_file(blob).unwrap();
    assert!(assets::hydrate(&path, stored, HydrateImages::All).is_err());
}

#[test]
fn hydration_skips_tool_images_using_the_nearest_role() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("roles.jsonl");
    let source =
        |data| serde_json::json!({"type": "base64", "media_type": "image/png", "data": data});
    let tool = source(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    );
    let user = source(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg1LD5DwAB+AFlawEwEQAAAABJRU5ErkJggg==",
    );
    let original = serde_json::json!({
        "role": "tool",
        "message": {"images": [{"source": tool.clone()}]},
        "messages": [
            {"role": "user", "images": [{"source": user.clone()}]},
            {"role": "tool", "images": [{"source": tool}]},
        ]
    });
    let stored = assets::externalize(&path, original.clone()).unwrap();
    let digest = stored["assets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|asset| asset["pointer"] == "/message/images/0/source/data")
        .unwrap()["sha256"]
        .as_str()
        .unwrap();
    let blob = assets::directory(&path).join(digest);
    fs::remove_file(blob).unwrap();
    let hydrated = assets::hydrate(&path, stored, HydrateImages::NotToolMessages).unwrap();
    assert_eq!(hydrated["message"]["images"][0]["source"]["data"], "");
    assert_eq!(hydrated["messages"][1]["images"][0]["source"]["data"], "");
    assert_eq!(hydrated["messages"][0]["images"][0]["source"], user);
}

#[test]
fn an_externalized_asset_records_the_sha256_of_its_bytes() {
    // The SHA-256 of "abc", the digest a stored record names its blob by.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("digest.jsonl");
    let original = serde_json::json!({
        "images": [{
            "type": "base64",
            "media_type": "image/png",
            "data": base64::engine::general_purpose::STANDARD.encode(b"abc"),
        }]
    });
    let stored = assets::externalize(&path, original.clone()).unwrap();
    assert_eq!(stored["assets"][0]["sha256"], ABC_SHA256);
    assert_eq!(stored["assets"][0]["bytes"], 3);
    // The blob is filed under that digest, and reading it back passes the
    // digest check.
    assert_eq!(
        fs::read(assets::directory(&path).join(ABC_SHA256)).unwrap(),
        b"abc"
    );
    assert_eq!(
        assets::hydrate(&path, stored, HydrateImages::All).unwrap(),
        original
    );
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
fn spill_files_are_removed_with_their_session() {
    crate::history::with_redirected_orca_home("spill-lifecycle", |home| {
        let writer = SessionWriter::start(home, "mock", None, "spill").unwrap();
        let id = writer.session_id().unwrap();
        let spill = home.join("session-results").join(&id).join("spill");
        fs::create_dir_all(&spill).unwrap();
        fs::write(spill.join("call.txt"), "result").unwrap();
        drop(writer);
        JsonlThreadStore::new().delete_session(&id).unwrap();
        assert!(!spill.exists());
    });
}

#[test]
fn spill_deletion_can_be_retried_after_image_cleanup_fails() {
    crate::history::with_redirected_orca_home("spill-delete-retry", |home| {
        let writer = SessionWriter::start(home, "mock", None, "spill").unwrap();
        let id = writer.session_id().unwrap();
        let transcript = writer.path().to_owned();
        let blocked = assets::directory(&transcript);
        fs::write(&blocked, "not an image directory").unwrap();
        let spill = super::spill::directory(&id).unwrap();
        fs::create_dir_all(&spill).unwrap();
        fs::write(spill.join("call.txt"), "saved result").unwrap();
        drop(writer);
        let store = JsonlThreadStore::new();
        assert!(store.delete_session(&id).is_err());
        assert!(transcript.exists());
        fs::remove_file(blocked).unwrap();
        store.delete_session(&id).unwrap();
        assert!(!spill.exists());
        assert!(!transcript.exists());
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

fn tool_result_image(size: usize) -> ImageInput {
    ImageInput {
        source: ImageSource::Base64 {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(vec![42; size]),
        },
        detail: ImageDetail::High,
    }
}

fn find_stored_tool_message(records: Vec<SessionRecord>) -> Option<StoredMessage> {
    records.into_iter().find_map(|record| match record {
        SessionRecord::Message {
            message: message @ StoredMessage::Tool { .. },
            ..
        } => Some(message),
        _ => None,
    })
}

fn append_tool_png(writer: &mut SessionWriter) -> ImageInput {
    let image = orca_core::tool_images::tool_image(
        "image/png",
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".to_string(),
    ).unwrap();
    writer.enter_turn(TurnId::new());
    writer
        .append_message(&Message::Tool {
            tool_call_id: "call_1".to_string(),
            content: "screenshot taken".to_string(),
            images: vec![image.clone()],
            terminal: None,
            pinned: false,
        })
        .unwrap();
    image
}

fn fully_hydrated_tool_images(path: &std::path::Path) -> Vec<ImageInput> {
    let records = super::reader::iter_records(path, HydrateImages::All)
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    let message = find_stored_tool_message(records).expect("tool message");
    Message::from(message).images().to_vec()
}

#[test]
fn export_keeps_tool_images() {
    crate::history::with_redirected_orca_home("tool-image-export", |home| {
        let store = JsonlThreadStore::new();
        let mut writer = SessionWriter::start(home, "mock", None, "export tool image").unwrap();
        let image = append_tool_png(&mut writer);
        let export = home.join("portable.jsonl");
        store
            .export_session(&writer.session_id().unwrap(), &export)
            .unwrap();
        // Portable exports retain bytes without referring to the source blobs.
        fs::remove_dir_all(assets::directory(writer.path())).unwrap();
        assert_eq!(fully_hydrated_tool_images(&export), vec![image]);
    });
}

#[test]
fn rewriting_thread_meta_keeps_tool_images() {
    use super::{ThreadMetadataPatch, ThreadStore};
    crate::history::with_redirected_orca_home("tool-image-rewrite", |home| {
        let store = JsonlThreadStore::new();
        let mut writer = SessionWriter::start(home, "mock", None, "rewrite tool image").unwrap();
        let image = append_tool_png(&mut writer);
        store
            .update_thread_metadata(
                &writer.session_id().unwrap(),
                ThreadMetadataPatch {
                    title: Some("updated title".to_string()),
                    ..ThreadMetadataPatch::default()
                },
            )
            .unwrap();
        assert_eq!(
            read_session_meta(writer.path()).unwrap().title,
            "updated title"
        );
        assert_eq!(fully_hydrated_tool_images(writer.path()), vec![image]);
    });
}

#[test]
fn compressing_and_restoring_a_session_keeps_tool_images() {
    crate::history::with_redirected_orca_home("tool-image-compress", |home| {
        let store = JsonlThreadStore::new();
        let mut writer = SessionWriter::start(home, "mock", None, "compress tool image").unwrap();
        let image = append_tool_png(&mut writer);
        let id = writer.session_id().unwrap();
        drop(writer);
        let compressed = store.compress_session(&id).unwrap();
        assert_eq!(fully_hydrated_tool_images(&compressed), vec![image.clone()]);
        let restored = SessionWriter::append_to_existing(compressed).unwrap();
        assert_eq!(fully_hydrated_tool_images(restored.path()), vec![image]);
    });
}

#[test]
fn a_tool_image_is_stored_as_an_asset_not_inline() {
    crate::history::with_redirected_orca_home("tool-image-asset", |home| {
        let mut writer = SessionWriter::start(home, "mock", None, "tool image").unwrap();
        writer.enter_turn(TurnId::new());
        let image = tool_result_image(4096);
        writer
            .append_message(&Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "screenshot taken".to_string(),
                images: vec![image.clone()],
                terminal: None,
                pinned: false,
            })
            .unwrap();

        let ImageSource::Base64 { data, .. } = &image.source else {
            panic!("expected a base64 image source");
        };
        let raw = fs::read_to_string(writer.path()).unwrap();
        assert!(!raw.contains(data.as_str()));

        // Resume readers leave the tool-image slot empty.
        let tool_message =
            find_stored_tool_message(read_records(writer.path()).unwrap()).expect("tool message");
        let Message::Tool { images, .. } = Message::from(tool_message) else {
            unreachable!("find_stored_tool_message only returns Tool records");
        };
        assert!(matches!(&images[0].source, ImageSource::Base64 { data, .. } if data.is_empty()));

        // An on-demand hydration still retrieves the original asset.
        let stored = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .find(assets::is_asset_record)
            .expect("asset record");
        let record: SessionRecord = serde_json::from_value(
            assets::hydrate(writer.path(), stored, HydrateImages::All).unwrap(),
        )
        .unwrap();
        let tool_message = find_stored_tool_message(vec![record]).unwrap();
        assert_eq!(Message::from(tool_message).images(), &[image]);
    });
}

#[test]
fn resume_does_not_read_tool_image_blobs() {
    crate::history::with_redirected_orca_home("tool-image-no-read", |home| {
        let store = JsonlThreadStore::new();
        let mut writer =
            SessionWriter::start(home, "mock", None, "resume without tool blobs").unwrap();
        writer.enter_turn(TurnId::new());
        let user_message = Message::user_with_images("inspect image".to_string(), vec![orca_core::tool_images::tool_image("image/png", "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg1LD5DwAB+AFlawEwEQAAAABJRU5ErkJggg==".to_string()).unwrap()]);
        writer.append_message(&user_message).unwrap();
        writer
            .append_message(&Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "screenshot taken".to_string(),
                images: vec![orca_core::tool_images::tool_image("image/png", "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".to_string()).unwrap()],
                terminal: None,
                pinned: false,
            })
            .unwrap();
        let root = assets::directory(writer.path());
        let blob = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| fs::metadata(path).unwrap().len() == 68)
            .expect("tool image blob");
        fs::remove_file(blob).unwrap();

        let transcript = store.load_session(&writer.session_id().unwrap()).unwrap();
        assert_eq!(transcript.messages[0].images(), user_message.images());
        let Message::Tool {
            images, content, ..
        } = &transcript.messages[1]
        else {
            panic!("expected a tool message");
        };
        assert!(images.is_empty());
        assert!(content.ends_with(orca_core::tool_images::RESUMED_TOOL_IMAGE_NOTE));

        let user_blob = fs::read_dir(root).unwrap().next().unwrap().unwrap().path();
        fs::remove_file(user_blob).unwrap();
        assert!(store.load_session(&writer.session_id().unwrap()).is_err());
    });
}

#[test]
fn a_resumed_session_drops_tool_images_with_a_note() {
    crate::history::with_redirected_orca_home("tool-image-resume", |home| {
        let store = JsonlThreadStore::new();
        let mut writer = SessionWriter::start(home, "mock", None, "resume tool image").unwrap();
        writer.enter_turn(TurnId::new());
        writer
            .append_message(&Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "screenshot taken".to_string(),
                images: vec![tool_result_image(4096)],
                terminal: None,
                pinned: false,
            })
            .unwrap();
        let id = writer.session_id().unwrap();

        let transcript = store.load_session(&id).unwrap();
        let tool_message = transcript
            .messages
            .into_iter()
            .find(|message| matches!(message, Message::Tool { .. }))
            .expect("a tool message");
        let Message::Tool {
            images, content, ..
        } = tool_message
        else {
            unreachable!("matched a Tool message above");
        };
        assert!(images.is_empty());
        assert!(content.ends_with("\n[image omitted: not kept across session resume]"));
    });
}

#[test]
fn history_without_tool_images_loads_unchanged() {
    crate::history::with_redirected_orca_home("tool-image-legacy", |home| {
        let writer = SessionWriter::start(home, "mock", None, "legacy tool").unwrap();
        let mut file = OpenOptions::new().append(true).open(writer.path()).unwrap();
        // Hand-written, pre-images-field record: no "images" key at all.
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "conversation.message",
                "message": {
                    "role": "tool",
                    "tool_call_id": "call_1",
                    "content": "legacy output"
                }
            })
        )
        .unwrap();

        let tool_message =
            find_stored_tool_message(read_records(writer.path()).unwrap()).expect("tool message");
        let StoredMessage::Tool {
            images, content, ..
        } = tool_message
        else {
            unreachable!("find_stored_tool_message only returns Tool records");
        };
        assert!(images.is_empty());
        assert_eq!(content, "legacy output");
    });
}

#[test]
fn a_compaction_snapshot_keeps_only_the_newest_tool_images() {
    crate::history::with_redirected_orca_home("tool-image-snapshot", |home| {
        let mut writer = SessionWriter::start(home, "mock", None, "snapshot tool images").unwrap();
        writer.enter_turn(TurnId::new());
        // Five distinct images: the older result holds two, the newer three.
        let images = (1..=5)
            .map(|index| tool_result_image(4096 + index))
            .collect::<Vec<_>>();
        let tool_message =
            |tool_call_id: &str, content: &str, images: &[ImageInput]| Message::Tool {
                tool_call_id: tool_call_id.to_string(),
                content: content.to_string(),
                images: images.to_vec(),
                terminal: None,
                pinned: false,
            };
        let older = tool_message("call_1", "first screenshots", &images[..2]);
        let newer = tool_message("call_2", "second screenshots", &images[2..]);
        writer.append_message(&older).unwrap();
        writer.append_message(&newer).unwrap();
        let mut conversation = Conversation::new();
        for (id, content, result_images) in [
            ("call_1", "first screenshots", &images[..2]),
            ("call_2", "second screenshots", &images[2..]),
        ] {
            let request = orca_core::tool_types::ToolRequest {
                id: id.to_string(),
                name: orca_core::tool_types::ToolName::ReadFile,
                action: orca_core::approval_types::ActionKind::Read,
                target: None,
                raw_arguments: None,
            };
            let result =
                orca_core::tool_types::ToolResult::completed(&request, content.to_string(), false)
                    .with_images(result_images.to_vec());
            conversation.add_tool_result_with_terminal(&result, content.to_string());
        }
        let identity = crate::session::ManualCompactionPersistenceIdentity {
            operation_id: crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .expect("generated UUID is v7"),
            snapshot_id: uuid::Uuid::now_v7().to_string(),
        };

        writer
            .append_manual_compaction_snapshot(&identity, 2, "local_truncation", &conversation)
            .unwrap();

        // The live conversation already retains only the newest three images.
        let live_images = conversation
            .messages
            .iter()
            .flat_map(|message| message.images().iter().cloned())
            .collect::<Vec<_>>();
        assert_eq!(live_images, images[2..]);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { content, .. } if content.ends_with(SUPERSEDED_TOOL_IMAGE_NOTE))
        );

        let resumed_records = read_records(writer.path()).unwrap();
        for record in resumed_records {
            let messages = match record {
                SessionRecord::Message { message, .. } => vec![message],
                SessionRecord::ManualCompactionSnapshot(snapshot) => snapshot.messages,
                _ => Vec::new(),
            };
            for message in messages {
                if let StoredMessage::Tool { images, .. } = message {
                    assert!(images.iter().all(|image| matches!(&image.source, ImageSource::Base64 { data, .. } if data.is_empty())));
                }
            }
        }
        // Explicit on-demand hydration verifies persisted originals and the snapshot.
        let records = fs::read_to_string(writer.path())
            .unwrap()
            .lines()
            .map(|line| {
                let value = serde_json::from_str(line).unwrap();
                serde_json::from_value::<SessionRecord>(
                    assets::hydrate(writer.path(), value, HydrateImages::All).unwrap(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        // Each tool result's own record still holds its images: all five.
        let recorded_images = records
            .iter()
            .filter_map(|record| match record {
                SessionRecord::Message {
                    message: StoredMessage::Tool { images, .. },
                    ..
                } => Some(images.clone()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(recorded_images, images);

        // The snapshot keeps only the newest three; the older message says why.
        let snapshot = records
            .into_iter()
            .find_map(|record| match record {
                SessionRecord::ManualCompactionSnapshot(snapshot) => Some(snapshot),
                _ => None,
            })
            .expect("a compaction snapshot");
        let [
            StoredMessage::Tool {
                content: older_content,
                images: older_images,
                ..
            },
            StoredMessage::Tool {
                content: newer_content,
                images: newer_images,
                ..
            },
        ] = snapshot.messages.as_slice()
        else {
            panic!("expected the two tool messages in the snapshot");
        };
        assert!(older_images.is_empty());
        assert_eq!(
            older_content,
            &format!("first screenshots\n{SUPERSEDED_TOOL_IMAGE_NOTE}")
        );
        assert_eq!(newer_images.as_slice(), &images[2..]);
        assert_eq!(newer_content, "second screenshots");
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
    assert!(assets::hydrate(&path, stored, HydrateImages::All).is_err());
    assert_eq!(fs::read(outside).unwrap(), [1, 2, 3]);
}
