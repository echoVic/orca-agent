use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

static ORCA_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes direct runtime-host access to the process-wide ORCA_HOME value.
/// Tests in this binary must use Command::env or acquire this helper before
/// reading or mutating ORCA_HOME.
fn with_process_orca_home<T>(home: &Path, run: impl FnOnce() -> T) -> T {
    let _guard = ORCA_HOME_LOCK.lock().expect("ORCA_HOME lock");
    let previous = std::env::var_os("ORCA_HOME");
    unsafe {
        std::env::set_var("ORCA_HOME", home);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    unsafe {
        match previous {
            Some(previous) => std::env::set_var("ORCA_HOME", previous),
            None => std::env::remove_var("ORCA_HOME"),
        }
    }
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn trust_project(home: &std::path::Path, project: &std::path::Path) {
    orca_core::config::folder_trust::set_trust_with_config_dir(
        project,
        home,
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trust project");
}

fn session_documents(home: &Path) -> Vec<(PathBuf, Vec<Value>)> {
    fn collect(path: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, files);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    collect(&home.join("sessions"), &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let records = std::fs::read_to_string(&path)
                .expect("read saved conversation")
                .lines()
                .map(|line| serde_json::from_str(line).expect("valid saved conversation record"))
                .collect();
            (path, records)
        })
        .collect()
}

fn saved_conversation_text(home: &Path) -> String {
    session_documents(home)
        .into_iter()
        .map(|(path, _)| std::fs::read_to_string(path).expect("read saved conversation"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn session_id_from_home(home: &Path) -> String {
    let documents = session_documents(home);
    assert_eq!(documents.len(), 1, "exactly one session document expected");
    documents[0]
        .1
        .iter()
        .find(|record| record["type"] == "session.meta")
        .and_then(|record| record["session_id"].as_str())
        .expect("session metadata with session id")
        .to_string()
}

#[test]
fn history_subcommand_is_not_exposed() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args(["history", "list"])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'history'"));
}

#[test]
fn exec_saves_conversation_transcript() {
    let home = TempDir::new().expect("temp home");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "remember this"])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let transcript = saved_conversation_text(home.path());
    assert!(transcript.contains("remember this"));
    assert!(transcript.contains("mock"));
    assert!(transcript.contains("conversation.message"));
}

#[test]
fn exec_preserves_unbound_at_tokens_as_literal_history() {
    let home = TempDir::new().expect("temp home");
    let project = TempDir::new().expect("temp project");
    std::fs::write(project.path().join("notes.txt"), "alpha\nbeta\ngamma\n")
        .expect("write mentioned file");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .arg("exec")
        .arg("--provider")
        .arg("mock")
        .arg("--cwd")
        .arg(project.path())
        .arg("summarize")
        .arg("@notes.txt#L2")
        .arg("@oai/sky还能逆向吗")
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let transcript = saved_conversation_text(home.path());
    assert!(transcript.contains("summarize @notes.txt#L2 @oai/sky还能逆向吗"));
    assert!(!transcript.contains("<file"));
    assert!(!transcript.contains("beta</file>"));
}

#[test]
fn exec_persists_usage_in_history() {
    let home = TempDir::new().expect("temp home");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "mock_usage"])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let usage = session_documents(home.path())
        .into_iter()
        .flat_map(|(_, records)| records)
        .find(|record| record["type"] == "session.usage")
        .expect("persisted usage record");
    assert_eq!(usage["input_tokens"], 120);
    assert_eq!(usage["output_tokens"], 30);
    assert_eq!(usage["cache_tokens"], 10);
    assert!(usage["estimated_cost_usd"].as_f64().is_some());
}

#[test]
fn exec_resume_injects_prior_conversation() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "first prompt"])
        .output()
        .expect("run first orca");
    assert_eq!(first.status.code(), Some(0));

    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--resume",
            "latest",
            "mock_history_echo",
        ])
        .output()
        .expect("run resumed orca");

    assert_eq!(resumed.status.code(), Some(0));
    let events = parse_jsonl(&resumed.stdout);
    let message = events
        .iter()
        .find(|event| event["type"] == "assistant.message.delta")
        .expect("assistant message");
    let text = message["payload"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("first prompt | mock_history_echo"));
}

#[test]
fn exec_resume_subcommand_continues_session_by_id() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "first prompt"])
        .output()
        .expect("run first orca");
    assert_eq!(first.status.code(), Some(0));

    let session_id = session_id_from_home(home.path());

    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "resume",
            &session_id,
            "mock_history_echo",
        ])
        .output()
        .expect("run resumed orca");

    assert_eq!(resumed.status.code(), Some(0));
    let events = parse_jsonl(&resumed.stdout);
    let message = events
        .iter()
        .find(|event| event["type"] == "assistant.message.delta")
        .expect("assistant message");
    let text = message["payload"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("first prompt | mock_history_echo"));
}

#[test]
fn exec_resume_subcommand_last_continues_latest() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "first prompt"])
        .output()
        .expect("run first orca");
    assert_eq!(first.status.code(), Some(0));

    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "resume",
            "--last",
            "mock_history_echo",
        ])
        .output()
        .expect("run resumed orca");

    assert_eq!(resumed.status.code(), Some(0));
    let events = parse_jsonl(&resumed.stdout);
    let message = events
        .iter()
        .find(|event| event["type"] == "assistant.message.delta")
        .expect("assistant message");
    let text = message["payload"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("first prompt | mock_history_echo"));
}

#[test]
fn exec_resume_after_budget_exhaustion_recounts_budget_scope() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--provider",
            "mock",
            "--max-budget",
            "0.000001",
            "mock_usage",
        ])
        .output()
        .expect("run budget-limited orca");
    assert_eq!(
        first.status.code(),
        Some(4),
        "budget exhaustion is typed, not a generic failure"
    );

    let session_id = session_id_from_home(home.path());

    // The resumed invocation owns a fresh budget scope: the previous run's
    // ceiling does not leak into the continuation, while the session and its
    // prior consumption records stay durable.
    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--provider",
            "mock",
            "resume",
            &session_id,
            "mock_usage",
        ])
        .output()
        .expect("run resumed orca");
    assert_eq!(resumed.status.code(), Some(0));

    let documents = session_documents(home.path());
    assert_eq!(
        documents.len(),
        1,
        "resume appends to the session, never forks"
    );
    let records = &documents[0].1;
    let meta_records = records
        .iter()
        .filter(|record| record["type"] == "session.meta")
        .collect::<Vec<_>>();
    assert_eq!(meta_records.len(), 1, "one durable session identity");
    assert_eq!(
        meta_records[0]["session_id"].as_str(),
        Some(session_id.as_str()),
        "resumed session keeps its identity"
    );
    let user_messages = records
        .iter()
        .filter(|record| {
            record["type"] == "conversation.message" && record["message"]["role"] == "user"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        user_messages.len(),
        2,
        "the exhausted run's accepted input and the continuation input both persist"
    );
}

#[test]
fn session_fork_copies_history_and_keeps_source_durable() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "fork source prompt"])
        .output()
        .expect("run source conversation");
    assert_eq!(first.status.code(), Some(0));

    let source_documents = session_documents(home.path());
    let source_id = source_documents
        .iter()
        .flat_map(|(_, records)| records)
        .find(|record| record["type"] == "session.meta")
        .and_then(|record| record["session_id"].as_str())
        .expect("source session metadata")
        .to_string();
    let source_path = source_documents
        .iter()
        .find(|(_, records)| {
            records.iter().any(|record| {
                record["type"] == "session.meta"
                    && record["session_id"].as_str() == Some(source_id.as_str())
            })
        })
        .map(|(path, _)| path)
        .expect("source session document");
    let source_before = std::fs::read_to_string(source_path).expect("read source document");

    let forked = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--provider",
            "mock",
            "--fork",
            &source_id,
            "fork child prompt",
        ])
        .output()
        .expect("run forked conversation");
    assert_eq!(forked.status.code(), Some(0));

    let documents = session_documents(home.path());
    assert_eq!(
        documents.len(),
        2,
        "source and fork must both remain durable"
    );
    let fork_records = documents
        .iter()
        .map(|(_, records)| records)
        .find(|records| {
            records.iter().any(|record| {
                record["type"] == "session.meta"
                    && record["parent_id"].as_str() == Some(source_id.as_str())
            })
        })
        .expect("fork metadata with source parent");
    let fork_id = fork_records
        .iter()
        .find(|record| record["type"] == "session.meta")
        .and_then(|record| record["session_id"].as_str())
        .expect("fork session id");
    assert_ne!(fork_id, source_id);
    let fork_text = fork_records
        .iter()
        .map(|record| record.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(fork_text.contains("fork source prompt"));
    assert!(fork_text.contains("fork child prompt"));
    let source_after = documents
        .iter()
        .find(|(_, records)| {
            records.iter().any(|record| {
                record["type"] == "session.meta"
                    && record["session_id"].as_str() == Some(source_id.as_str())
            })
        })
        .map(|(path, _)| std::fs::read_to_string(path).expect("read source after fork"))
        .expect("source document after fork");
    assert_eq!(source_after, source_before);
}

#[test]
fn session_archive_and_delete_update_the_durable_catalog() {
    let home = TempDir::new().expect("temp home");
    for prompt in ["archive this conversation", "delete this conversation"] {
        let output = Command::new(env!("CARGO_BIN_EXE_orca"))
            .env("ORCA_HOME", home.path())
            .args(["exec", "--provider", "mock", prompt])
            .output()
            .expect("run saved conversation");
        assert_eq!(output.status.code(), Some(0));
    }
    let documents = session_documents(home.path());
    let session_ids = documents
        .iter()
        .filter_map(|(_, records)| {
            records
                .iter()
                .find(|record| record["type"] == "session.meta")
                .and_then(|record| record["session_id"].as_str())
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();
    assert_eq!(session_ids.len(), 2);

    with_process_orca_home(home.path(), || {
        let archived_path =
            orca_runtime::surface::RuntimeSurfaceHostHandle::archive_saved_session(&session_ids[0])
                .expect("archive saved conversation");
        assert!(archived_path.starts_with(home.path().join("archive")));
        assert!(archived_path.exists());
        assert!(
            orca_runtime::surface::RuntimeSurfaceHostHandle::list_saved_sessions(10)
                .expect("list active conversations")
                .iter()
                .all(|session| session.session_id != session_ids[0])
        );

        let deleted_archive =
            orca_runtime::surface::RuntimeSurfaceHostHandle::delete_saved_session(&session_ids[0])
                .expect("delete archived conversation");
        assert_eq!(deleted_archive, archived_path);
        assert!(!deleted_archive.exists());

        let deleted_active =
            orca_runtime::surface::RuntimeSurfaceHostHandle::delete_saved_session(&session_ids[1])
                .expect("delete active conversation");
        assert!(!deleted_active.exists());
        assert!(
            orca_runtime::surface::RuntimeSurfaceHostHandle::list_saved_sessions(10)
                .expect("list empty conversation catalog")
                .is_empty()
        );
    });
}

#[test]
fn exec_injects_project_instructions_into_system_prompt() {
    let home = TempDir::new().expect("temp home");
    let project = TempDir::new().expect("temp project");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(
        project.path().join("AGENTS.md"),
        "Always prefer contract tests.\n",
    )
    .expect("write AGENTS.md");
    std::fs::create_dir_all(project.path().join(".orca/rules")).expect("create rules dir");
    std::fs::write(
        project.path().join(".orca/rules/010-style.md"),
        "Keep user-facing output concise.\n",
    )
    .expect("write rule");
    trust_project(home.path(), project.path());

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(project.path())
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "instruction probe"])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let transcript = saved_conversation_text(home.path());
    assert!(transcript.contains("<project-instructions>"));
    assert!(transcript.contains("Always prefer contract tests."));
    assert!(transcript.contains("Keep user-facing output concise."));
}

#[test]
fn exec_does_not_persist_explicitly_mentioned_skill_in_system_prompt() {
    let home = TempDir::new().expect("temp home");
    let project = TempDir::new().expect("temp project");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\n",
    )
    .expect("write Cargo.toml");
    let skill_dir = home.path().join("skills/debugging");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: Debugging\ndescription: Find root causes\n---\n\nUse logs first.\n",
    )
    .expect("write skill");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(project.path())
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "please use $debugging"])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let transcript = saved_conversation_text(home.path());
    assert!(
        !transcript.contains("<skills>"),
        "explicit skills should ride the model-only volatile overlay, not the persisted system prompt"
    );
    assert!(
        !transcript.contains(r#"<skill id=\"debugging\""#),
        "explicit skill metadata should not be persisted into history"
    );
    assert!(
        !transcript.contains("Use logs first."),
        "explicit skill body should not be persisted into history"
    );
}

#[test]
fn exec_injects_user_instructions_before_project_instructions() {
    let home = TempDir::new().expect("temp home");
    let project = TempDir::new().expect("temp project");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(home.path().join("AGENTS.md"), "Global instruction\n")
        .expect("write global AGENTS.md");
    std::fs::write(project.path().join("AGENTS.md"), "Project instruction\n")
        .expect("write project AGENTS.md");
    trust_project(home.path(), project.path());

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(project.path())
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "global instruction probe"])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let transcript = saved_conversation_text(home.path());
    let global = transcript.find("Global instruction").expect("global");
    let project = transcript.find("Project instruction").expect("project");
    assert!(global < project);
}

#[test]
fn exec_continue_alias_resumes_latest_conversation() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "alias prompt"])
        .output()
        .expect("run first orca");
    assert_eq!(first.status.code(), Some(0));

    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--continue",
            "mock_history_echo",
        ])
        .output()
        .expect("run continued orca");

    assert_eq!(resumed.status.code(), Some(0));
    let events = parse_jsonl(&resumed.stdout);
    let message = events
        .iter()
        .find(|event| event["type"] == "assistant.message.delta")
        .expect("assistant message");
    let text = message["payload"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("alias prompt | mock_history_echo"));
}

#[test]
fn exec_fork_creates_child_with_parent_metadata() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "fork parent prompt"])
        .output()
        .expect("run parent orca");
    assert_eq!(first.status.code(), Some(0));

    let parent_records = session_documents(home.path());
    assert_eq!(parent_records.len(), 1);
    let parent_id = parent_records[0].1[0]["session_id"]
        .as_str()
        .expect("parent id")
        .to_string();

    let fork = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--provider",
            "mock",
            "--fork",
            "latest",
            "mock_history_echo",
        ])
        .output()
        .expect("run fork");
    assert_eq!(fork.status.code(), Some(0));

    let conversations = session_documents(home.path());
    assert_eq!(conversations.len(), 2);
    let (_, child_records) = conversations
        .iter()
        .find(|(_, records)| records[0]["parent_id"] == parent_id)
        .expect("forked conversation");
    assert_eq!(child_records[0]["forked"], true);
    let child_text = serde_json::to_string(child_records).expect("serialize child records");
    assert!(child_text.contains("fork parent prompt"));
    assert!(child_text.contains("mock_history_echo"));
}

#[test]
fn exec_resume_at_restores_conversation_to_message_boundary() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "first prompt"])
        .output()
        .expect("run first orca");
    assert_eq!(first.status.code(), Some(0));
    let session_id = session_id_from_home(home.path());

    let appended = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--provider",
            "mock",
            "resume",
            &session_id,
            "second prompt",
        ])
        .output()
        .expect("append second prompt");
    assert_eq!(appended.status.code(), Some(0));

    let records = &session_documents(home.path())[0].1;
    let first_user_id = records
        .iter()
        .find(|record| {
            record["type"] == "conversation.message"
                && record["message"]["role"] == "user"
                && record["message"]["content"] == "first prompt"
        })
        .and_then(|record| record["id"].as_str())
        .expect("first user message item id")
        .to_string();

    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "resume",
            &session_id,
            "--resume-at",
            &first_user_id,
            "mock_history_echo",
        ])
        .output()
        .expect("resume at boundary");

    assert_eq!(resumed.status.code(), Some(0));
    let events = parse_jsonl(&resumed.stdout);
    let message = events
        .iter()
        .find(|event| event["type"] == "assistant.message.delta")
        .expect("assistant message");
    let text = message["payload"]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("first prompt"),
        "boundary keeps messages at or before it"
    );
    assert!(
        !text.contains("second prompt"),
        "boundary drops messages after it: {text}"
    );
}

#[test]
fn exec_resume_at_rejects_unknown_boundary() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "first prompt"])
        .output()
        .expect("run first orca");
    assert_eq!(first.status.code(), Some(0));
    let session_id = session_id_from_home(home.path());

    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "resume",
            &session_id,
            "--resume-at",
            "item_00000000-0000-0000-0000-000000000000",
            "mock_history_echo",
        ])
        .output()
        .expect("resume at unknown boundary");

    assert_eq!(resumed.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert!(
        stderr.contains("no saved message matches"),
        "unknown boundary must fail closed: {stderr}"
    );
}

#[test]
fn budget_exhausted_session_persists_typed_checkpoint() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--provider",
            "mock",
            "--max-budget",
            "0.000001",
            "mock_usage",
        ])
        .output()
        .expect("run budget-limited orca");
    assert_eq!(first.status.code(), Some(4));

    let records = &session_documents(home.path())[0].1;
    let checkpoint = records
        .iter()
        .find(|record| record["type"] == "session.checkpoint")
        .expect("typed checkpoint on budget exhaustion");
    assert_eq!(checkpoint["status"], "budget_exhausted");
    assert_eq!(checkpoint["reason"], "cost_budget_exhausted");
    assert_eq!(checkpoint["resumable"], true);
    assert_eq!(
        checkpoint["budget_consumed"]["input_tokens"], 120,
        "checkpoint records the consumption that exhausted the budget"
    );
    let checkpoint_index = records
        .iter()
        .position(|record| record["type"] == "session.checkpoint")
        .expect("checkpoint index");
    let completed_index = records
        .iter()
        .position(|record| record["type"] == "session.completed")
        .expect("completed index");
    assert!(
        checkpoint_index < completed_index,
        "the resume boundary (session checkpoint) lands BEFORE the terminal: the operation can only claim resumability after its boundary is durable"
    );
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
