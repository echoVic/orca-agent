//! `orca rewind`: headless restore of files and/or the conversation to a
//! previously captured user turn.
//!
//! The durable records (`session.snapshot` / `session.rewind`) live in the
//! session transcript; this command is the non-interactive counterpart of the
//! TUI `/rewind` picker. It never requires a live runtime thread: it opens the
//! saved session, plans the restore, applies it, and appends a `session.rewind`
//! record so a later `resume` sees the truncated conversation.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::rewind::{
    ConversationRewindReport, RewindFilesReport, RewindMode, RewindReport, RewindSkip, RewindStats,
    RewindWarning,
};
use orca_rewind::{
    Applier, BlobEncoding, BlobRef, PlanOptions, PreImage, RewindStore, SnapshotFileView,
    SnapshotView,
};

use crate::thread_store::{
    ConversationRewindRecord, FileSnapshotRecord, RewindFilesSummary, SessionRewindRecord,
    SessionStore, SessionWriter,
};

/// Parsed `orca rewind` arguments.
#[derive(Clone, Debug)]
pub struct RewindCommandRequest {
    pub session: Option<String>,
    pub last: bool,
    pub at: Option<String>,
    pub mode: RewindMode,
    pub dry_run: bool,
    pub json: bool,
    pub force: bool,
}

pub fn run(request: RewindCommandRequest) -> i32 {
    match execute(&request) {
        Ok(report) => {
            if request.json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(error) => {
                        eprintln!("orca: failed to serialize rewind report: {error}");
                        return 1;
                    }
                }
            } else {
                print_human(&report, request.dry_run);
            }
            0
        }
        Err(error) => {
            eprintln!("orca: {error}");
            if error.kind() == io::ErrorKind::InvalidInput {
                return 2;
            }
            1
        }
    }
}

fn execute(request: &RewindCommandRequest) -> io::Result<RewindReport> {
    if request.dry_run && request.force {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--force cannot be combined with --dry-run",
        ));
    }
    if request.mode.is_summarize() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "summarize rewind modes are not implemented yet; use --mode code, conversation, or both",
        ));
    }
    let selector = if request.last {
        "latest".to_string()
    } else {
        request
            .session
            .clone()
            .unwrap_or_else(|| "latest".to_string())
    };
    let transcript = SessionStore::new().load_session(&selector)?;
    let path = transcript.path.clone();
    let session_id = transcript.meta.session_id.clone();
    let session_root = normalize_root(Path::new(&transcript.meta.cwd));

    let target = match request.at.as_deref() {
        Some(id) => id.to_string(),
        None => crate::thread_store::read_effective_user_messages(&path)?
            .last()
            .map(|message| message.message_id.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "session has no rewindable user message",
                )
            })?,
    };

    let snapshots = crate::thread_store::read_snapshots(&path)?;
    let target_snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.at_message_id == target)
        .cloned();

    let mut report = RewindReport {
        target_message_id: target.clone(),
        mode: request.mode.as_str().to_string(),
        dry_run: request.dry_run,
        ..RewindReport::default()
    };

    // ---- conversation side -------------------------------------------------
    let cut = if request.mode.includes_conversation() {
        let cut = crate::thread_store::read_transcript_before_message(&path, &target)?;
        report.ordinal = cut.ordinal;
        report.conversation = Some(ConversationRewindReport {
            kind: "truncate".to_string(),
            kept_messages: cut.kept_messages,
        });
        Some(cut)
    } else {
        // Keep `ordinal` informative for code-only reports.
        report.ordinal = target_snapshot
            .as_ref()
            .map(|snapshot| snapshot.ordinal)
            .unwrap_or_default();
        None
    };

    // ---- file side ---------------------------------------------------------
    let mut files_summary = None;
    let mut files_report = RewindFilesReport::default();
    if request.mode.includes_files() {
        let snapshot = target_snapshot.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no file snapshot recorded for message '{target}'; use --mode conversation"
                ),
            )
        })?;
        let snapshot_root = normalize_root(Path::new(&snapshot.root));
        if snapshot_root != session_root {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "snapshot root {} does not match the session workspace {}; refusing to restore across workspaces",
                    snapshot_root.display(),
                    session_root.display()
                ),
            ));
        }
        if !snapshot_root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "workspace {} is no longer available; only --mode conversation can run",
                    snapshot_root.display()
                ),
            ));
        }
        let store_dir = rewind_store_dir(&session_id)?;
        let store = RewindStore::open(&store_dir)?;
        let views = snapshot_views(&snapshots);
        let plan = orca_rewind::build_plan(
            &snapshot_root,
            &store,
            &views,
            PlanOptions {
                target_ordinal: snapshot.ordinal,
            },
        )?;

        files_report = RewindFilesReport {
            changed: plan.writes.len() as u64,
            deleted: plan.deletes.len() as u64,
            unchanged: plan.unchanged,
            skipped: plan
                .skipped
                .iter()
                .map(|skip| RewindSkip {
                    path: skip.path.clone(),
                    reason: skip.reason.clone(),
                })
                .collect(),
        };
        report.stats = RewindStats {
            insertions: plan.insertions,
            deletions: plan.deletions,
            omitted: plan.stats_omitted,
        };
        report
            .warnings
            .extend(plan.warnings.iter().map(|warning| RewindWarning {
                kind: warning.kind.clone(),
                paths: warning.paths.clone(),
                detail: None,
            }));
        let untracked: Vec<String> = plan
            .writes
            .iter()
            .filter(|write| write.untracked_change)
            .map(|write| write.path.clone())
            .chain(
                plan.deletes
                    .iter()
                    .filter(|delete| delete.untracked_change)
                    .map(|delete| delete.path.clone()),
            )
            .collect();
        if !untracked.is_empty() {
            report.warnings.push(RewindWarning {
                kind: "untracked_change".to_string(),
                paths: untracked,
                detail: Some(
                    "files changed outside tracked tool writes; overwriting requires --force"
                        .to_string(),
                ),
            });
        }
        if !request.dry_run {
            if report.has_blocking_warnings() && !request.force {
                let mut paths = Vec::new();
                for warning in &report.warnings {
                    if warning.kind == "untracked_change" {
                        paths.extend(warning.paths.iter().cloned());
                    }
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "refusing to overwrite files changed outside tracked writes: {}; pass --force to continue",
                        paths.join(", ")
                    ),
                ));
            }
            let applier = Applier::new(snapshot_root.clone(), store, store_dir.join("journals"));
            let applied = applier.apply(&plan)?;
            files_report.changed = applied.applied;
            files_report.deleted = applied.deleted;
            for skip in applied.skipped {
                if !files_report
                    .skipped
                    .iter()
                    .any(|existing| existing.path == skip.path)
                {
                    files_report.skipped.push(RewindSkip {
                        path: skip.path,
                        reason: skip.reason,
                    });
                }
            }
            if applied.partial {
                report.warnings.push(RewindWarning {
                    kind: "partial".to_string(),
                    paths: Vec::new(),
                    detail: Some(format!(
                        "restore stopped after {} files; journal {} can be rolled back",
                        applied.applied, applied.op_id
                    )),
                });
            }
            report.op_id = Some(applied.op_id);
        }
        files_summary = Some(RewindFilesSummary {
            restored: files_report.changed as u32,
            deleted: files_report.deleted as u32,
            skipped: files_report.skipped.len() as u32,
        });
    }
    report.files = files_report;

    // ---- durable rewind record --------------------------------------------
    if !request.dry_run {
        let (kept_messages, kept_compactions, kept_summaries) = match cut.as_ref() {
            Some(cut) => (cut.kept_messages, cut.kept_compactions, cut.kept_summaries),
            // Code-only rewinds leave the conversation untouched; a `None`
            // conversation record makes the loader ignore these counts.
            None => (0, 0, 0),
        };
        let rewind_id = format!("rw_{}", uuid::Uuid::now_v7());
        let record = SessionRewindRecord {
            schema_version: 1,
            session_id: session_id.clone(),
            rewind_id: rewind_id.clone(),
            at_message_id: target.clone(),
            kept_messages,
            kept_compactions,
            kept_summaries,
            conversation: cut.as_ref().map(|_| ConversationRewindRecord {
                kind: "truncate".to_string(),
                summary: None,
                summary_usage: None,
            }),
            files: files_summary,
            recorded_at: chrono::Utc::now(),
        };
        let mut writer = SessionWriter::append_to_existing(path.clone())?;
        writer.append_session_rewind(record)?;
        drop(writer);
        report.rewind_id = Some(rewind_id);
    }

    Ok(report)
}

fn print_human(report: &RewindReport, dry_run: bool) {
    let prefix = if dry_run { "Would rewind" } else { "Rewound" };
    println!(
        "{prefix} to {} (ordinal {}) in {} mode",
        report.target_message_id, report.ordinal, report.mode
    );
    if let Some(conversation) = report.conversation.as_ref() {
        println!(
            "  conversation: keep {} messages ({})",
            conversation.kept_messages, conversation.kind
        );
    }
    if report.files.changed + report.files.deleted > 0 || !report.files.skipped.is_empty() {
        println!(
            "  files: {} restored (+{} -{}), {} deleted, {} unchanged, {} skipped",
            report.files.changed,
            report.stats.insertions,
            report.stats.deletions,
            report.files.deleted,
            report.files.unchanged,
            report.files.skipped.len()
        );
        for skip in &report.files.skipped {
            println!("    skipped {}: {}", skip.path, skip.reason);
        }
    }
    for warning in &report.warnings {
        println!(
            "  warning: {}{}{}",
            warning.kind,
            if warning.paths.is_empty() {
                String::new()
            } else {
                format!(" ({})", warning.paths.join(", "))
            },
            warning
                .detail
                .as_ref()
                .map(|detail| format!(" — {detail}"))
                .unwrap_or_default()
        );
    }
}

fn snapshot_views(snapshots: &[FileSnapshotRecord]) -> Vec<SnapshotView> {
    snapshots
        .iter()
        .map(|snapshot| SnapshotView {
            ordinal: snapshot.ordinal,
            at_message_id: snapshot.at_message_id.clone(),
            files: snapshot
                .files
                .iter()
                .map(|(path, entry)| {
                    let pre = match entry.pre.as_ref() {
                        Some(blob) => PreImage::Blob(BlobRef {
                            sha256: blob.sha256.clone(),
                            size: blob.size,
                            mode: blob.mode,
                            encoding: if blob.encoding == "zstd" {
                                BlobEncoding::Zstd
                            } else {
                                BlobEncoding::Raw
                            },
                        }),
                        None => PreImage::Missing,
                    };
                    (
                        path.clone(),
                        SnapshotFileView {
                            pre,
                            post_sha256: entry.post_sha256.clone(),
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        })
        .collect()
}

fn normalize_root(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn rewind_store_dir(session_id: &str) -> io::Result<PathBuf> {
    let home = orca_core::home::orca_home().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot resolve the Orca home; set ORCA_HOME",
        )
    })?;
    Ok(home.join("rewind").join("sessions").join(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_store::{
        BlobRef as StoredBlobRef, FileSnapshotRecord, SessionRecord, SnapshotFileEntry,
    };
    use orca_core::conversation::Message;
    use orca_core::thread_identity::TurnId;
    use sha2::Digest;

    struct Fixture {
        home: tempfile::TempDir,
        cwd: tempfile::TempDir,
        session_id: String,
        path: PathBuf,
        writer: SessionWriter,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().expect("home");
        orca_core::home::install_test_orca_home(Some(home.path().to_path_buf()));
        let cwd = tempfile::tempdir().expect("cwd");
        let meta = crate::history::create_meta(cwd.path(), "mock", None, "rewind cli contract");
        let session_id = meta.session_id.clone();
        let writer = SessionWriter::start_from_meta(meta).expect("start transcript");
        let path = writer.path().to_path_buf();
        Fixture {
            home,
            cwd,
            session_id,
            path,
            writer,
        }
    }

    fn store_dir(fixture: &Fixture) -> PathBuf {
        fixture
            .home
            .path()
            .join("rewind")
            .join("sessions")
            .join(&fixture.session_id)
    }

    fn append_user_message(fixture: &mut Fixture, prompt: &str) -> String {
        fixture.writer.enter_turn(TurnId::new());
        let message = Message::user(prompt.to_string());
        fixture
            .writer
            .append_message(&message)
            .expect("append user");
        fixture
            .writer
            .last_committed_message_id()
            .expect("committed user id")
    }

    /// Record a checkpoint for `path` with `before` as its pre-image and
    /// `after` as the post-write digest, then leave the workspace at `after`.
    fn record_checkpoint(
        fixture: &mut Fixture,
        message_id: &str,
        relative_path: &str,
        before: &[u8],
        after: &[u8],
    ) {
        std::fs::write(fixture.cwd.path().join(relative_path), after).expect("agent write");
        let store = RewindStore::open(&store_dir(fixture)).expect("open store");
        let blob = store.put_bytes(before, None).expect("pre-image blob");
        let mut files = BTreeMap::new();
        files.insert(
            relative_path.to_string(),
            SnapshotFileEntry {
                pre: Some(StoredBlobRef {
                    sha256: blob.sha256,
                    size: blob.size,
                    mode: blob.mode,
                    encoding: "raw".to_string(),
                }),
                post_sha256: Some(orca_core::hex::lower(&sha2::Sha256::digest(after))),
            },
        );
        fixture
            .writer
            .append_file_snapshot(FileSnapshotRecord {
                schema_version: 1,
                snapshot_id: "snap_test".to_string(),
                session_id: fixture.session_id.clone(),
                at_message_id: message_id.to_string(),
                turn_id: None,
                ordinal: 0,
                root: fixture
                    .cwd
                    .path()
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string(),
                workspace_fingerprint: None,
                complete: true,
                start_digest: None,
                end_digest: None,
                files,
                change_stats: None,
                untracked: Vec::new(),
                limitations: Vec::new(),
                recorded_at: chrono::Utc::now(),
            })
            .expect("append snapshot");
    }

    fn request(
        fixture: &Fixture,
        mode: RewindMode,
        at: &str,
        dry_run: bool,
    ) -> RewindCommandRequest {
        RewindCommandRequest {
            session: Some(fixture.session_id.clone()),
            last: false,
            at: Some(at.to_string()),
            mode,
            dry_run,
            json: false,
            force: false,
        }
    }

    #[test]
    fn dry_run_plans_a_restore_without_touching_the_workspace() {
        let mut fixture = fixture();
        let message_id = append_user_message(&mut fixture, "edit a.txt");
        record_checkpoint(&mut fixture, &message_id, "a.txt", b"before\n", b"after\n");

        let report =
            execute(&request(&fixture, RewindMode::Code, &message_id, true)).expect("dry-run plan");
        assert_eq!(report.files.changed, 1);
        assert_eq!(report.files.unchanged, 0);
        assert_eq!(
            std::fs::read_to_string(fixture.cwd.path().join("a.txt")).unwrap(),
            "after\n",
            "dry-run must not touch the workspace"
        );
        assert!(report.rewind_id.is_none(), "dry-run writes no record");
        let transcript = crate::thread_store::load_session(&fixture.session_id).unwrap();
        assert!(
            !std::fs::read_to_string(&fixture.path)
                .unwrap()
                .contains("session.rewind"),
            "dry-run must not append a rewind record"
        );
        assert_eq!(transcript.messages.len(), 1);
    }

    #[test]
    fn apply_restores_files_and_appends_a_rewind_record() {
        let mut fixture = fixture();
        let message_id = append_user_message(&mut fixture, "edit a.txt");
        record_checkpoint(&mut fixture, &message_id, "a.txt", b"before\n", b"after\n");

        let report = execute(&request(&fixture, RewindMode::Both, &message_id, false))
            .expect("apply rewind");
        assert_eq!(report.files.changed, 1);
        assert!(report.rewind_id.is_some());
        assert_eq!(
            std::fs::read_to_string(fixture.cwd.path().join("a.txt")).unwrap(),
            "before\n",
            "the pre-image is restored"
        );
        let raw = std::fs::read_to_string(&fixture.path).unwrap();
        assert!(raw.contains("\"session.rewind\""));
        // The conversation was rewound strictly before the target user message.
        let transcript = crate::thread_store::load_session(&fixture.session_id).unwrap();
        assert_eq!(transcript.messages.len(), 0);
        let bytes = std::fs::read(&fixture.path).unwrap();
        let records: Vec<SessionRecord> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("record line"))
            .collect();
        assert!(
            records
                .iter()
                .any(|record| matches!(record, SessionRecord::SessionRewind(_))),
            "the durable rewind marker is appended"
        );
    }

    #[test]
    fn conversation_only_rewind_keeps_files_untouched() {
        let mut fixture = fixture();
        let first = append_user_message(&mut fixture, "first");
        let second = append_user_message(&mut fixture, "second");
        assert_ne!(first, second);

        let report = execute(&request(&fixture, RewindMode::Conversation, &second, false))
            .expect("conversation rewind");
        assert_eq!(report.files.changed, 0);
        assert_eq!(
            report
                .conversation
                .expect("conversation report")
                .kept_messages,
            1
        );
        let transcript = crate::thread_store::load_session(&fixture.session_id).unwrap();
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(
            transcript.messages[0].content_str(),
            Some("first"),
            "only history before the target is kept"
        );
    }

    #[test]
    fn legacy_session_without_checkpoints_reports_a_clear_error() {
        let mut fixture = fixture();
        let message_id = append_user_message(&mut fixture, "no checkpoint");
        let error = execute(&request(&fixture, RewindMode::Code, &message_id, true))
            .expect_err("no snapshot");
        assert!(error.to_string().contains("no file snapshot"));
        let error = execute(&request(&fixture, RewindMode::Both, "item_missing", true))
            .expect_err("unknown target");
        assert!(error.to_string().contains("no saved message matches"));
    }
}
