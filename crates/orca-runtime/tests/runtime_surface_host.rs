use orca_core::approval_types::ActionKind;
use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::config::{
    AdditionalWorkingDirectory, HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind,
    RunConfig, ThemeName, ToolConfig, WorkflowConfig,
};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_core::task_types::{PendingToolCallSummary, TaskStatus};
use orca_runtime::runtime_host::{
    GenerationContext, HostedTurnRequest, RuntimeHost, RuntimeThreadStartRequest,
    ThreadOperationExecutor, ThreadOperationOutcome,
};
use orca_runtime::surface::{
    AttachResult, CompactionState, DisplayText, FreshAttachRequest, MutationDisposition,
    MutationReply, NonEmptyText, OperationTerminal, PinnedContextAction, PinnedContextRevision,
    PinnedContextSourceRevision, PinnedUserRevision, Sha256Digest, SurfaceAttachmentRole,
    SurfaceCapability, SurfaceCatalogEntryId, SurfaceEvent, SurfaceInteractionKind,
    SurfacePinnedContextEntry, SurfacePinnedContextKind, SurfaceRequestId, SurfaceShutdownReason,
    SurfaceSubscriptionItem, WaitOperationTerminalResult, WorkflowCatalogRevision,
    WorkflowControlAction,
};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tempfile::tempdir;

use orca_runtime::surface::{
    FirstOperationCompletionPolicy, NonReplayableReason, OperationIngressCorrelation,
    OperationKind, OperationRequestIntent, OperationSettingsPreparation, ReplayabilityRequest,
    SurfaceInputRequest, SurfaceInputRequestBlock, SurfaceSubscriptionSealReason,
    ThreadPersistence,
};

static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());
const EPHEMERAL_CONFIG_CHILD_ENV: &str = "ORCA_RUNTIME_SURFACE_EPHEMERAL_CONFIG_CHILD";
const EPHEMERAL_CONFIG_CWD_ENV: &str = "ORCA_RUNTIME_SURFACE_EPHEMERAL_CONFIG_CWD";

struct ObserveAutoMemoryExecutor {
    observed: mpsc::SyncSender<bool>,
}

impl ThreadOperationExecutor for ObserveAutoMemoryExecutor {
    fn run_turn(
        &self,
        thread: &mut orca_runtime::thread::RuntimeThread,
        _request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn io::Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        self.observed
            .send(generation.config().auto_memory)
            .map_err(|_| io::Error::other("auto-memory observer closed"))?;
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

#[test]
fn closed_host_facade_starts_a_typed_thread_surface() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let surface_host = host.surface_handle();
    let thread = surface_host
        .start_thread(test_config(cwd.path().to_path_buf()), "facade")
        .expect("typed thread");

    assert!(!thread.thread_id().is_empty());
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::<SurfaceInteractionKind>::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };
    let thread_id = orca_runtime::surface::SurfaceThreadId::try_from_bytes(
        *uuid::Uuid::parse_str(thread.thread_id())
            .expect("thread id is UUID")
            .as_bytes(),
    )
    .expect("surface thread id");
    assert_eq!(attachment.baseline.snapshot.thread.thread_id, thread_id);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn explicit_history_disabled_one_shot_starts_an_ephemeral_typed_surface() {
    if std::env::var_os(EPHEMERAL_CONFIG_CHILD_ENV).is_none() {
        let home = tempdir().expect("temporary ORCA_HOME");
        let cwd = tempdir().expect("temp cwd");
        let output = Command::new(std::env::current_exe().expect("runtime surface test binary"))
            .args([
                "--exact",
                "explicit_history_disabled_one_shot_starts_an_ephemeral_typed_surface",
                "--nocapture",
            ])
            .env(EPHEMERAL_CONFIG_CHILD_ENV, "1")
            .env(EPHEMERAL_CONFIG_CWD_ENV, cwd.path())
            .env("ORCA_HOME", home.path())
            .output()
            .expect("run isolated ephemeral runtime test");
        assert!(
            output.status.success(),
            "isolated ephemeral runtime test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let entries = fs::read_dir(home.path())
            .expect("read ephemeral ORCA_HOME")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "agent-events.jsonl" && name != "agent-events.lock")
            .collect::<Vec<_>>();
        assert!(
            entries.is_empty(),
            "ephemeral one-shot wrote persistent ORCA_HOME state: {entries:?}"
        );
        assert!(
            !directory_tree_has_files(&cwd.path().join(".orca")),
            "ephemeral one-shot wrote persistent workspace state"
        );
        return;
    }

    let cwd =
        PathBuf::from(std::env::var_os(EPHEMERAL_CONFIG_CWD_ENV).expect("isolated ephemeral cwd"));
    let (observed_tx, observed_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(ObserveAutoMemoryExecutor {
        observed: observed_tx,
    }))
    .expect("runtime host");
    let mut config = test_config(cwd.clone());
    config.history_mode = HistoryMode::Disabled;
    config.auto_memory = true;
    let thread = host
        .surface_handle()
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(config, "stateless submit")
                .with_ephemeral_non_catalogued_one_shot(FirstOperationCompletionPolicy::Terminal),
        )
        .expect("ephemeral typed thread");

    assert!(thread.session_id().is_none());
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("ephemeral surface must be attachable"),
    };
    assert_eq!(
        attachment.baseline.snapshot.thread.persistence,
        ThreadPersistence::EphemeralNonCataloguedOneShot {
            close_after: FirstOperationCompletionPolicy::Terminal,
        }
    );
    assert!(matches!(
        attachment.baseline.snapshot.cursor.source_revision,
        orca_runtime::surface::CursorSourceRevision::Ephemeral { live_revision }
            if live_revision.get() == 1
    ));
    assert_eq!(
        attachment.baseline.snapshot.thread.thread_id.as_bytes(),
        uuid::Uuid::parse_str(thread.thread_id())
            .expect("runtime thread id is UUIDv7")
            .as_bytes()
    );
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("ephemeral subscription");
    let snapshot = attachment.baseline.snapshot;
    let reserved = match attachment
        .client
        .reserve_operation(
            SurfaceRequestId::new(),
            ephemeral_one_shot_intent(&snapshot, "finish one-shot"),
        )
        .expect("reserve ephemeral operation")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("ephemeral reservation must commit"),
    };
    let _ = attachment
        .client
        .admit_reserved(
            SurfaceRequestId::new(),
            reserved.operation_id.clone(),
            reserved.lease.lease_id,
        )
        .expect("admit ephemeral operation");
    assert_eq!(
        observed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("observe effective actor config"),
        false,
        "ephemeral actor retained caller auto-memory configuration"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut terminal_visible = false;
    let mut sealed = false;
    while Instant::now() < deadline {
        match subscription.recv_timeout(Duration::from_millis(20)) {
            Some(SurfaceSubscriptionItem::Batch { batch }) => {
                terminal_visible |= batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Operation(
                            orca_runtime::surface::OperationPatch::Terminal { record }
                        ) if record.operation_id == reserved.operation_id
                    )
                });
            }
            Some(SurfaceSubscriptionItem::Sealed {
                reason: SurfaceSubscriptionSealReason::ThreadClosed,
            }) => {
                sealed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(terminal_visible, "one-shot terminal was not published");
    assert!(sealed, "one-shot subscription was not sealed");
    while thread.is_available() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(!thread.is_available(), "one-shot actor remained available");

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn not_admitted_one_shot_closes_and_rejects_reuse() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let mut config = test_config(cwd.path().to_path_buf());
    config.history_mode = HistoryMode::Disabled;
    let thread = host
        .surface_handle()
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(config, "cancelled stateless submit")
                .with_ephemeral_non_catalogued_one_shot(
                    FirstOperationCompletionPolicy::NotAdmitted,
                ),
        )
        .expect("ephemeral typed thread");
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("ephemeral surface must be attachable"),
    };
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("ephemeral subscription");
    let snapshot = attachment.baseline.snapshot;
    let reserved = match attachment
        .client
        .reserve_operation(
            SurfaceRequestId::new(),
            ephemeral_one_shot_intent(&snapshot, "cancel before admission"),
        )
        .expect("reserve ephemeral operation")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("ephemeral reservation must commit"),
    };
    let _ = attachment
        .client
        .cancel_operation(SurfaceRequestId::new(), reserved.operation_id.clone())
        .expect("cancel reservation");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_not_admitted = false;
    let mut sealed = false;
    while Instant::now() < deadline {
        match subscription.recv_timeout(Duration::from_millis(20)) {
            Some(SurfaceSubscriptionItem::Batch { batch }) => {
                saw_not_admitted |= batch.events.as_slice().iter().any(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Operation(
                            orca_runtime::surface::OperationPatch::Terminal { record }
                        ) if record.operation_id == reserved.operation_id
                            && matches!(record.terminal, OperationTerminal::NotAdmitted { .. })
                    )
                });
            }
            Some(SurfaceSubscriptionItem::Sealed {
                reason: SurfaceSubscriptionSealReason::ThreadClosed,
            }) => {
                sealed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_not_admitted, "reservation terminal was not published");
    assert!(sealed, "cancelled one-shot subscription was not sealed");
    while thread.is_available() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        !thread.is_available(),
        "cancelled one-shot remained available"
    );
    assert!(matches!(
        attachment.client.reserve_operation(
            SurfaceRequestId::new(),
            ephemeral_one_shot_intent(&snapshot, "must not be reused"),
        ),
        Err(orca_runtime::surface::SurfaceClientCommandError::RuntimeUnavailable)
    ));
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn typed_thread_snapshot_preserves_configured_additional_directories() {
    let cwd = tempdir().expect("temp cwd");
    let extra = tempdir().expect("additional cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let mut config = test_config(cwd.path().to_path_buf());
    config.runtime_workspace_roots =
        Some(vec![cwd.path().to_path_buf(), extra.path().to_path_buf()]);
    config.additional_working_directories = vec![AdditionalWorkingDirectory::new(
        extra.path().to_path_buf(),
        "acp",
    )];
    let thread = host
        .surface_handle()
        .start_thread(config, "additional roots")
        .expect("typed thread");
    let attachment = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };

    assert_eq!(
        attachment
            .baseline
            .snapshot
            .settings
            .effective
            .workspace_roots[1]
            .as_path(),
        extra.path()
    );
    assert_eq!(
        attachment
            .baseline
            .snapshot
            .settings
            .effective
            .additional_working_directories[0]
            .path
            .as_path(),
        extra.path()
    );
    assert_eq!(
        attachment
            .baseline
            .snapshot
            .settings
            .effective
            .additional_working_directories[0]
            .source
            .as_str(),
        "acp"
    );
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn unbound_thread_facade_does_not_issue_acp_surface_authority() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(test_config(cwd.path().to_path_buf()), "ACP facade")
        .expect("typed thread");
    assert!(thread.acp_surface().is_none());
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn tui_surface_can_commit_and_publish_pinned_context() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(test_config(cwd.path().to_path_buf()), "pinned context")
        .expect("typed thread");
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManagePinnedContext,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };
    let entry = SurfacePinnedContextEntry {
        id: SurfaceCatalogEntryId::try_new("user-note-1").unwrap(),
        kind: SurfacePinnedContextKind::User,
        label: NonEmptyText::try_new("remembered note").unwrap(),
        content: DisplayText::new("remember this"),
        content_digest: Sha256Digest::new([7; 32]),
        source_revision: PinnedContextSourceRevision::User(PinnedUserRevision::try_new(1).unwrap()),
    };
    let result = attachment.client.pinned_context_mutation(
        SurfaceRequestId::new(),
        PinnedContextAction::Add {
            expected_revision: PinnedContextRevision::try_new(1).unwrap(),
            entry: entry.clone(),
            memory_receipt: None,
        },
    );
    let output = match result.expect("pinned context command") {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("unexpected pinned context result"),
    };
    assert_eq!(
        output.snapshot.revision,
        PinnedContextRevision::try_new(2).unwrap()
    );
    assert_eq!(output.snapshot.entries, vec![entry]);
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn tui_surface_manual_compaction_is_durable_before_terminal() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf()),
            "typed manual compaction",
        )
        .expect("typed thread");
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };
    let expected_context_revision = attachment.baseline.snapshot.context.revision;
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("surface subscription");
    let output = match attachment
        .client
        .manual_compact(SurfaceRequestId::new(), expected_context_revision)
        .expect("manual compact command")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("manual compaction must commit"),
    };
    let terminal = attachment
        .client
        .wait_operation_terminal(SurfaceRequestId::new(), output.operation_id.clone())
        .expect("terminal wait");
    assert!(matches!(
        terminal,
        WaitOperationTerminalResult::Terminal { value }
            if matches!(value.terminal, OperationTerminal::Succeeded { .. })
    ));

    let mut saw_running = false;
    let mut saw_completed = false;
    let mut saw_terminal = false;
    while let Some(item) = subscription.try_recv() {
        if let SurfaceSubscriptionItem::Batch { batch } = item {
            for envelope in batch.events.as_slice() {
                match &envelope.event {
                    SurfaceEvent::Context(context) => match &context.compaction {
                        CompactionState::Running { operation_id, .. }
                            if operation_id == &output.operation_id =>
                        {
                            saw_running = true;
                            assert!(!saw_completed);
                            assert!(!saw_terminal);
                        }
                        CompactionState::Completed { operation_id, .. }
                            if operation_id == &output.operation_id =>
                        {
                            saw_completed = true;
                            assert!(saw_running);
                            assert!(!saw_terminal);
                        }
                        _ => {}
                    },
                    SurfaceEvent::Operation(orca_runtime::surface::OperationPatch::Terminal {
                        record,
                    }) if record.operation_id == output.operation_id => {
                        saw_terminal = true;
                        assert!(saw_completed);
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(saw_running);
    assert!(saw_completed);
    assert!(saw_terminal);

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn typed_thread_expands_mentions_with_runtime_owned_registry() {
    let cwd = tempdir().expect("temp cwd");
    fs::write(cwd.path().join("context.txt"), "runtime-owned context").expect("context file");
    let root = cwd.path().canonicalize().expect("canonical cwd");
    let input = "read @context.txt";
    let bindings = orca_runtime::mentions::MentionBindings::from_bindings(
        input,
        vec![orca_runtime::mentions::MentionBinding {
            start: 5,
            end: input.len(),
            visible: "@context.txt".to_string(),
            target: orca_runtime::mentions::MentionTarget::File {
                root: root.clone(),
                path: "context.txt".to_string(),
                kind: orca_runtime::mentions::MentionFileKind::File,
            },
        }],
    );
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(test_config(root.clone()), "mention expansion")
        .expect("typed thread");

    let expanded = thread
        .expand_mentions(input, &bindings, &root, std::slice::from_ref(&root))
        .expect("runtime mention expansion");

    assert!(expanded.contains("runtime-owned context"));
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn typed_thread_discovers_mention_catalog_with_runtime_owned_registry() {
    let cwd = tempdir().expect("temp cwd");
    let manifest_dir = cwd.path().join(".orca/plugins/github/.codex-plugin");
    fs::create_dir_all(&manifest_dir).expect("plugin directory");
    fs::write(
        manifest_dir.join("plugin.json"),
        r#"{"name":"github","description":"GitHub workflows","interface":{"displayName":"GitHub"}}"#,
    )
    .expect("plugin manifest");
    let root = cwd.path().canonicalize().expect("canonical cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(test_config(root.clone()), "mention catalog")
        .expect("typed thread");

    let catalog = thread.discover_mention_catalog(std::slice::from_ref(&root));

    assert!(
        catalog
            .candidates()
            .iter()
            .any(|candidate| candidate.display == "GitHub")
    );
    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn closed_thread_facade_owns_task_control_and_background_approval() {
    let cwd = tempdir().expect("temp cwd");
    let host = RuntimeHost::start().expect("runtime host");
    let runtime_thread = host
        .handle()
        .start_thread(test_config(cwd.path().to_path_buf()), "task facade")
        .expect("runtime thread");
    let registry = runtime_thread.task_registry();
    let foreground = registry.create_main_session("background turn".to_string());
    registry.mark_running(&foreground.id).expect("running task");
    registry
        .mark_backgrounded(&foreground.id)
        .expect("backgrounded task");
    let approval = registry.create_main_session("approval turn".to_string());
    registry
        .approval_required_for_pending_tool(
            &approval.id,
            "approval required".to_string(),
            Some(PendingToolCallSummary {
                id: "approval-request".to_string(),
                name: "shell".to_string(),
                action: ActionKind::Shell,
                target: None,
                arguments: "{}".to_string(),
            }),
        )
        .expect("approval task");
    let surface = runtime_thread.typed_surface();

    let foregrounded = surface
        .foreground_task(&foreground.id)
        .expect("foreground through facade");
    assert!(
        foregrounded
            .iter()
            .any(|task| task.id == foreground.id && !task.is_backgrounded)
    );

    let (task_id, denied) = surface
        .resolve_background_approval("approval-request", false)
        .expect("deny approval through facade");
    assert_eq!(task_id, approval.id);
    assert!(
        denied
            .iter()
            .any(|task| task.id == approval.id && task.status == TaskStatus::Stopped)
    );

    let stopped = surface
        .stop_task(&foreground.id)
        .expect("stop through facade");
    assert!(
        stopped
            .iter()
            .any(|task| task.id == foreground.id && task.status == TaskStatus::Stopping)
    );

    host.shutdown().expect("shutdown runtime host");
}

#[test]
fn typed_workflow_launch_commits_task_workflow_and_operation_before_returning() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().expect("temporary ORCA_HOME");
    let cwd = tempdir().expect("workflow cwd");
    let workflow_dir = cwd.path().join(".orca").join("workflows");
    fs::create_dir_all(&workflow_dir).expect("workflow directory");
    fs::write(
        workflow_dir.join("typed-launch.js"),
        "export const meta = { name: 'typed-launch', description: 'typed launch', phases: ['main'] };\nexport const args = { label: { type: 'string', required: true }, count: { type: 'number', required: true }, enabled: { type: 'boolean', required: true }, payload: { type: 'json', required: true } };\nexport default await phase('main', async () => agent('inspect repo'));",
    )
    .expect("saved workflow");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trusted workflow workspace");
    let previous_home = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };

    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    let restart_config = config.clone();
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(config, "typed workflow launch")
        .expect("typed thread");
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ManageWorkflow,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };

    let launch_request_id = SurfaceRequestId::new();
    let output = match attachment
        .client
        .workflow_control(
            launch_request_id.clone(),
            WorkflowControlAction::Launch {
                catalog_entry_id: SurfaceCatalogEntryId::try_new("typed-launch").unwrap(),
                observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                args: vec![
                    (
                        NonEmptyText::try_new("label").unwrap(),
                        DisplayText::new(r#""alpha""#),
                    ),
                    (
                        NonEmptyText::try_new("count").unwrap(),
                        DisplayText::new("2"),
                    ),
                    (
                        NonEmptyText::try_new("enabled").unwrap(),
                        DisplayText::new("true"),
                    ),
                    (
                        NonEmptyText::try_new("payload").unwrap(),
                        DisplayText::new("null"),
                    ),
                ],
                parent: None,
            },
        )
        .expect("typed workflow launch")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("workflow launch must be durable before returning"),
    };
    let operation_id = output
        .operation_id
        .as_ref()
        .expect("standalone workflow owns an operation");
    let replay = attachment
        .client
        .workflow_control(
            launch_request_id.clone(),
            WorkflowControlAction::Launch {
                catalog_entry_id: SurfaceCatalogEntryId::try_new("typed-launch").unwrap(),
                observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                args: vec![
                    (
                        NonEmptyText::try_new("label").unwrap(),
                        DisplayText::new(r#""alpha""#),
                    ),
                    (
                        NonEmptyText::try_new("count").unwrap(),
                        DisplayText::new("2"),
                    ),
                    (
                        NonEmptyText::try_new("enabled").unwrap(),
                        DisplayText::new("true"),
                    ),
                    (
                        NonEmptyText::try_new("payload").unwrap(),
                        DisplayText::new("null"),
                    ),
                ],
                parent: None,
            },
        )
        .expect("replay typed workflow launch");
    match replay {
        MutationReply::Committed { mutation, value } => {
            assert_eq!(
                mutation.disposition,
                MutationDisposition::AlreadyApplied,
                "same request id must not launch a second workflow"
            );
            assert_eq!(value.operation_id.as_ref(), Some(operation_id));
            assert_eq!(
                value.workflow.workflow_run_id,
                output.workflow.workflow_run_id
            );
        }
        _ => panic!("same workflow request must replay the committed launch"),
    }
    let conflicting_replay = attachment
        .client
        .workflow_control(
            launch_request_id.clone(),
            WorkflowControlAction::Launch {
                catalog_entry_id: SurfaceCatalogEntryId::try_new("typed-launch").unwrap(),
                observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                args: vec![(
                    NonEmptyText::try_new("label").unwrap(),
                    DisplayText::new(r#""different""#),
                )],
                parent: None,
            },
        )
        .expect("conflicting replay response");
    assert!(
        matches!(conflicting_replay, MutationReply::Uncommitted { .. }),
        "same request id with different workflow arguments must be rejected"
    );
    let snapshot = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("snapshot attachment failed"),
    };
    assert!(
        snapshot
            .tasks
            .iter()
            .any(|task| task.task_id == output.workflow.task_id)
    );
    assert!(
        snapshot
            .workflows
            .iter()
            .any(|workflow| workflow == &output.workflow)
    );
    assert!(
        snapshot
            .background_operations
            .iter()
            .any(|operation| &operation.operation_id == operation_id)
    );
    assert_eq!(snapshot.cursor, output.cursor);

    let deadline = Instant::now() + Duration::from_secs(10);
    let terminal_snapshot = loop {
        let snapshot = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
            _ => panic!("terminal snapshot attachment failed"),
        };
        let workflow_terminal = snapshot.workflows.iter().any(|workflow| {
            workflow.workflow_run_id == output.workflow.workflow_run_id
                && matches!(
                    workflow.status,
                    orca_runtime::surface::SurfaceWorkflowStatus::Completed
                )
                && workflow.result.is_some()
        });
        let operation_terminal = snapshot.operation_history.iter().any(|operation| {
            &operation.operation_id == operation_id && operation.terminal.is_some()
        });
        if workflow_terminal && operation_terminal {
            break snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "workflow completion must become durable typed workflow and operation terminal facts"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        terminal_snapshot
            .background_operations
            .iter()
            .all(|operation| &operation.operation_id != operation_id)
    );
    let delayed_replay = attachment
        .client
        .workflow_control(
            launch_request_id,
            WorkflowControlAction::Launch {
                catalog_entry_id: SurfaceCatalogEntryId::try_new("typed-launch").unwrap(),
                observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                args: vec![
                    (
                        NonEmptyText::try_new("label").unwrap(),
                        DisplayText::new(r#""alpha""#),
                    ),
                    (
                        NonEmptyText::try_new("count").unwrap(),
                        DisplayText::new("2"),
                    ),
                    (
                        NonEmptyText::try_new("enabled").unwrap(),
                        DisplayText::new("true"),
                    ),
                    (
                        NonEmptyText::try_new("payload").unwrap(),
                        DisplayText::new("null"),
                    ),
                ],
                parent: None,
            },
        )
        .expect("replay completed workflow launch");
    match delayed_replay {
        MutationReply::Committed { mutation, value } => {
            assert_eq!(mutation.disposition, MutationDisposition::AlreadyApplied);
            assert_eq!(value.cursor, output.cursor);
            assert_eq!(
                value.workflow.status,
                orca_runtime::surface::SurfaceWorkflowStatus::AsyncLaunched,
                "launch replay projection must match the launch cursor"
            );
        }
        _ => panic!("completed workflow launch must remain replayable"),
    }

    let session_id = thread.thread_id().to_string();
    host.shutdown().expect("shutdown runtime host");
    let transcript =
        orca_runtime::history::load_session(&session_id).expect("saved workflow session");
    let mut restart_config = restart_config;
    restart_config.history_mode = HistoryMode::Resume(session_id.clone());
    let restarted_host = RuntimeHost::start().expect("restarted runtime host");
    let restarted = restarted_host
        .surface_handle()
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(restart_config, "restarted typed workflow")
                .with_preloaded(transcript),
        )
        .expect("restarted typed thread");
    let recovered = match restarted.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("restarted snapshot attachment failed"),
    };
    assert!(recovered.workflows.iter().any(|workflow| {
        workflow.workflow_run_id == output.workflow.workflow_run_id
            && workflow.status == orca_runtime::surface::SurfaceWorkflowStatus::Completed
            && workflow.result.is_some()
    }));
    assert!(recovered.operation_history.iter().any(|operation| {
        &operation.operation_id == operation_id && operation.terminal.is_some()
    }));
    restarted_host.shutdown().expect("shutdown restarted host");
    match previous_home {
        Some(value) => unsafe { std::env::set_var("ORCA_HOME", value) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn typed_workflow_background_cancel_commits_stop_and_terminalizes() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().expect("temporary ORCA_HOME");
    let cwd = tempdir().expect("workflow cwd");
    let workflow_dir = cwd.path().join(".orca").join("workflows");
    fs::create_dir_all(&workflow_dir).expect("workflow directory");
    fs::write(
        workflow_dir.join("typed-cancel.js"),
        "export const meta = { name: 'typed-cancel', description: 'typed cancel', phases: ['main'] };\nexport default await phase('main', async () => agent('mock_stream_delay_ms 30000'));",
    )
    .expect("saved workflow");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trusted workflow workspace");
    let previous_home = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };

    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(config, "typed workflow cancel")
        .expect("typed thread");
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::ManageWorkflow,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };
    let output = match attachment
        .client
        .workflow_control(
            SurfaceRequestId::new(),
            WorkflowControlAction::Launch {
                catalog_entry_id: SurfaceCatalogEntryId::try_new("typed-cancel").unwrap(),
                observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                args: Vec::new(),
                parent: None,
            },
        )
        .expect("typed workflow launch")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("workflow launch must commit"),
    };
    let operation_id = output.operation_id.expect("workflow operation");
    let before_cancel = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("pre-cancel snapshot attachment failed"),
    };
    assert!(
        before_cancel
            .background_operations
            .iter()
            .any(|operation| operation.operation_id == operation_id),
        "launched workflow operation must be background-owned before cancel"
    );
    let cancelled = attachment
        .client
        .cancel_operation(SurfaceRequestId::new(), operation_id.clone())
        .expect("background workflow cancel");
    match cancelled {
        MutationReply::Committed { mutation, value } => {
            assert!(matches!(
                value,
                orca_runtime::surface::CancelOperationOutput::Accepted { .. }
            ));
            assert_eq!(
                mutation.acknowledgements.as_slice().len(),
                3,
                "cancel acceptance must acknowledge operation, task, and workflow durable facts"
            );
        }
        _ => panic!("background workflow cancellation must durably commit"),
    }
    let terminal = attachment
        .client
        .wait_operation_terminal(SurfaceRequestId::new(), operation_id)
        .expect("wait cancelled workflow");
    assert!(matches!(
        terminal,
        WaitOperationTerminalResult::Terminal { value }
            if matches!(
                value.terminal,
                OperationTerminal::Cancelled { .. }
            )
    ));
    let snapshot = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("snapshot attachment failed"),
    };
    assert!(snapshot.workflows.iter().any(|workflow| {
        workflow.workflow_run_id == output.workflow.workflow_run_id
            && matches!(
                workflow.status,
                orca_runtime::surface::SurfaceWorkflowStatus::Stopped
                    | orca_runtime::surface::SurfaceWorkflowStatus::Cancelled
            )
    }));

    host.shutdown().expect("shutdown runtime host");
    match previous_home {
        Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn restarted_runtime_terminalizes_an_inflight_typed_workflow_and_its_task() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().expect("temporary ORCA_HOME");
    let cwd = tempdir().expect("workflow cwd");
    let workflow_dir = cwd.path().join(".orca").join("workflows");
    fs::create_dir_all(&workflow_dir).expect("workflow directory");
    fs::write(
        workflow_dir.join("restart-running.js"),
        "export const meta = { name: 'restart-running', description: 'restart running', phases: ['main'] };\nexport default await phase('main', async () => agent('mock_stream_delay_ms 3000'));",
    )
    .expect("saved workflow");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trusted workflow workspace");
    let previous_home = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };

    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    let host = RuntimeHost::start().expect("runtime host");
    let thread = host
        .surface_handle()
        .start_thread(config.clone(), "restart running workflow")
        .expect("typed thread");
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManageWorkflow,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("unexpected attachment result"),
    };
    let output = match attachment
        .client
        .workflow_control(
            SurfaceRequestId::new(),
            WorkflowControlAction::Launch {
                catalog_entry_id: SurfaceCatalogEntryId::try_new("restart-running").unwrap(),
                observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                args: Vec::new(),
                parent: None,
            },
        )
        .expect("typed workflow launch")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("workflow launch must commit"),
    };
    let operation_id = output.operation_id.clone().expect("workflow operation");
    let session_id = thread.thread_id().to_string();
    host.shutdown().expect("shutdown runtime host");

    let transcript =
        orca_runtime::history::load_session(&session_id).expect("saved workflow session");
    config.history_mode = HistoryMode::Resume(session_id);
    let restarted_host = RuntimeHost::start().expect("restarted runtime host");
    let restarted = restarted_host
        .surface_handle()
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(config, "recovered running workflow")
                .with_preloaded(transcript),
        )
        .expect("restarted typed thread");
    let recovered = match restarted.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("restarted snapshot attachment failed"),
    };
    assert!(recovered.operation_history.iter().any(|operation| {
        operation.operation_id == operation_id
            && matches!(
                operation.terminal.as_ref().map(|record| &record.terminal),
                Some(OperationTerminal::Shutdown {
                    reason: SurfaceShutdownReason::HostShutdown
                })
            )
    }));
    assert!(
        recovered
            .background_operations
            .iter()
            .all(|operation| operation.operation_id != operation_id)
    );
    assert!(recovered.workflows.iter().any(|workflow| {
        workflow.workflow_run_id == output.workflow.workflow_run_id
            && !matches!(
                workflow.status,
                orca_runtime::surface::SurfaceWorkflowStatus::Running
                    | orca_runtime::surface::SurfaceWorkflowStatus::AsyncLaunched
            )
    }));
    assert!(recovered.tasks.iter().any(|task| {
        task.task_id == output.workflow.task_id
            && !matches!(
                task.status,
                orca_runtime::surface::SurfaceTaskStatus::Running
                    | orca_runtime::surface::SurfaceTaskStatus::Queued
            )
    }));
    restarted_host.shutdown().expect("shutdown restarted host");
    match previous_home {
        Some(value) => unsafe { std::env::set_var("ORCA_HOME", value) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn cold_owner_takeover_reconciles_crashed_workflow_task_and_operation() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().expect("temporary ORCA_HOME");
    let cwd = tempdir().expect("workflow cwd");
    let workflow_dir = cwd.path().join(".orca").join("workflows");
    fs::create_dir_all(&workflow_dir).expect("workflow directory");
    fs::write(
        workflow_dir.join("crash-running.js"),
        "export const meta = { name: 'crash-running', description: 'crash running', phases: ['main'] };\nexport default await phase('main', async () => agent('mock_stream_delay_ms 3000'));",
    )
    .expect("saved workflow");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trusted workflow workspace");
    let fixture_output = home.path().join("workflow-crash-fixture.json");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("typed_workflow_crash_fixture")
        .arg("--nocapture")
        .env("ORCA_WORKFLOW_CRASH_FIXTURE", "1")
        .env("ORCA_WORKFLOW_CRASH_HOME", home.path())
        .env("ORCA_WORKFLOW_CRASH_CWD", cwd.path())
        .env("ORCA_WORKFLOW_CRASH_OUTPUT", &fixture_output)
        .status()
        .expect("run workflow crash fixture");
    assert!(status.success(), "workflow crash fixture failed: {status}");
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture_output).expect("workflow crash fixture output"))
            .expect("workflow crash fixture identity");
    let session_id = identity["session_id"]
        .as_str()
        .expect("fixture session id")
        .to_string();
    let operation_id = orca_runtime::surface::SurfaceOperationId::try_from_bytes(
        *uuid::Uuid::parse_str(
            identity["operation_id"]
                .as_str()
                .expect("fixture operation id"),
        )
        .expect("operation UUID")
        .as_bytes(),
    )
    .expect("surface operation id");
    let workflow_run_id = identity["workflow_run_id"]
        .as_str()
        .expect("fixture workflow run id");
    let task_id = identity["task_id"].as_str().expect("fixture task id");

    let previous_home = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };
    let transcript =
        orca_runtime::history::load_session(&session_id).expect("crashed workflow session");
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    config.history_mode = HistoryMode::Resume(session_id);
    let host = RuntimeHost::start().expect("takeover runtime host");
    let thread = host
        .surface_handle()
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(config, "take over crashed workflow")
                .with_preloaded(transcript),
        )
        .expect("take over crashed workflow thread");
    let recovered = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("takeover snapshot attachment failed"),
    };
    assert!(recovered.operation_history.iter().any(|operation| {
        operation.operation_id == operation_id
            && matches!(
                operation.terminal.as_ref().map(|record| &record.terminal),
                Some(OperationTerminal::AbortedByRuntimeRestart { .. })
            )
    }));
    assert!(recovered.workflows.iter().any(|workflow| {
        workflow.workflow_run_id.as_str() == workflow_run_id
            && workflow.status == orca_runtime::surface::SurfaceWorkflowStatus::Stopped
            && workflow.result.is_some()
    }));
    assert!(recovered.tasks.iter().any(|task| {
        task.task_id.as_str() == task_id
            && task.status == orca_runtime::surface::SurfaceTaskStatus::Stopped
    }));
    host.shutdown().expect("shutdown takeover host");
    match previous_home {
        Some(value) => unsafe { std::env::set_var("ORCA_HOME", value) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn cold_owner_takeover_settles_workflow_when_task_exists_but_run_state_is_missing() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().expect("temporary ORCA_HOME");
    let cwd = tempdir().expect("workflow cwd");
    let workflow_dir = cwd.path().join(".orca").join("workflows");
    fs::create_dir_all(&workflow_dir).expect("workflow directory");
    fs::write(
        workflow_dir.join("crash-running.js"),
        "export const meta = { name: 'crash-running', description: 'crash running', phases: ['main'] };\nexport default await phase('main', async () => agent('mock_stream_delay_ms 3000'));",
    )
    .expect("saved workflow");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trusted workflow workspace");
    let fixture_output = home.path().join("workflow-missing-state-fixture.json");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("typed_workflow_crash_fixture")
        .arg("--nocapture")
        .env("ORCA_WORKFLOW_CRASH_FIXTURE", "1")
        .env("ORCA_WORKFLOW_CRASH_HOME", home.path())
        .env("ORCA_WORKFLOW_CRASH_CWD", cwd.path())
        .env("ORCA_WORKFLOW_CRASH_OUTPUT", &fixture_output)
        .status()
        .expect("run workflow crash fixture");
    assert!(status.success(), "workflow crash fixture failed: {status}");
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture_output).expect("workflow crash fixture output"))
            .expect("workflow crash fixture identity");
    let session_id = identity["session_id"]
        .as_str()
        .expect("fixture session id")
        .to_string();
    let operation_id = orca_runtime::surface::SurfaceOperationId::try_from_bytes(
        *uuid::Uuid::parse_str(
            identity["operation_id"]
                .as_str()
                .expect("fixture operation id"),
        )
        .expect("operation UUID")
        .as_bytes(),
    )
    .expect("surface operation id");
    let workflow_run_id = identity["workflow_run_id"]
        .as_str()
        .expect("fixture workflow run id");
    let task_id = identity["task_id"].as_str().expect("fixture task id");
    let run_state_path = cwd
        .path()
        .join(".orca")
        .join("workflow-sessions")
        .join(&session_id)
        .join("workflow-runs")
        .join(workflow_run_id)
        .join("state.json");
    let worker_path = run_state_path
        .parent()
        .expect("workflow run directory")
        .join("worker.json");
    fs::remove_file(&run_state_path)
        .expect("simulate crash after TaskRegistry persistence but before run state creation");

    let previous_home = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };
    let transcript =
        orca_runtime::history::load_session(&session_id).expect("crashed workflow session");
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    config.history_mode = HistoryMode::Resume(session_id);
    let host = RuntimeHost::start().expect("takeover runtime host");
    let thread = host
        .surface_handle()
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(config, "settle missing workflow state")
                .with_preloaded(transcript),
        )
        .expect("missing workflow state must not block takeover");
    let recovered = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("takeover snapshot attachment failed"),
    };
    assert!(recovered.operation_history.iter().any(|operation| {
        operation.operation_id == operation_id
            && matches!(
                operation.terminal.as_ref().map(|record| &record.terminal),
                Some(OperationTerminal::AbortedByRuntimeRestart { .. })
            )
    }));
    assert!(recovered.workflows.iter().any(|workflow| {
        workflow.workflow_run_id.as_str() == workflow_run_id
            && workflow.status == orca_runtime::surface::SurfaceWorkflowStatus::Stopped
    }));
    assert!(recovered.tasks.iter().any(|task| {
        task.task_id.as_str() == task_id
            && task.status == orca_runtime::surface::SurfaceTaskStatus::Stopped
    }));
    let worker: serde_json::Value =
        serde_json::from_slice(&fs::read(worker_path).expect("workflow worker record"))
            .expect("workflow worker JSON");
    assert_eq!(
        worker["active"], false,
        "cold takeover must retire stale durable worker ownership"
    );
    host.shutdown().expect("shutdown takeover host");
    match previous_home {
        Some(value) => unsafe { std::env::set_var("ORCA_HOME", value) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn cold_owner_takeover_preserves_durable_workflow_success_before_projection() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().expect("temporary ORCA_HOME");
    let cwd = tempdir().expect("workflow cwd");
    let workflow_dir = cwd.path().join(".orca").join("workflows");
    fs::create_dir_all(&workflow_dir).expect("workflow directory");
    fs::write(
        workflow_dir.join("crash-running.js"),
        "export const meta = { name: 'crash-running', description: 'crash running', phases: ['main'] };\nexport default await phase('main', async () => agent('mock_stream_delay_ms 30000'));",
    )
    .expect("saved workflow");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trusted workflow workspace");
    let fixture_output = home
        .path()
        .join("workflow-completed-before-projection.json");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("typed_workflow_crash_fixture")
        .arg("--nocapture")
        .env("ORCA_WORKFLOW_CRASH_FIXTURE", "1")
        .env("ORCA_WORKFLOW_HOLD_FIXTURE", "1")
        .env("ORCA_WORKFLOW_CRASH_HOME", home.path())
        .env("ORCA_WORKFLOW_CRASH_CWD", cwd.path())
        .env("ORCA_WORKFLOW_CRASH_OUTPUT", &fixture_output)
        .spawn()
        .expect("spawn held workflow fixture");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture_output.exists() {
        assert!(
            Instant::now() < deadline,
            "held workflow fixture must publish its durable identity"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture_output).expect("held workflow fixture output"))
            .expect("held workflow fixture identity");
    let session_id = identity["session_id"]
        .as_str()
        .expect("fixture session id")
        .to_string();
    let operation_id = orca_runtime::surface::SurfaceOperationId::try_from_bytes(
        *uuid::Uuid::parse_str(
            identity["operation_id"]
                .as_str()
                .expect("fixture operation id"),
        )
        .expect("operation UUID")
        .as_bytes(),
    )
    .expect("surface operation id");
    let workflow_run_id = identity["workflow_run_id"]
        .as_str()
        .expect("fixture workflow run id");
    let task_id = identity["task_id"]
        .as_str()
        .expect("fixture task id")
        .to_string();
    let run_state_path = cwd
        .path()
        .join(".orca")
        .join("workflow-sessions")
        .join(&session_id)
        .join("workflow-runs")
        .join(workflow_run_id)
        .join("state.json");
    let mut run_state: serde_json::Value =
        serde_json::from_slice(&fs::read(&run_state_path).expect("active workflow run state"))
            .expect("workflow run state JSON");
    run_state["status"] = serde_json::Value::String("completed".to_string());
    run_state["finalSummary"] = serde_json::Value::String("durable workflow result".to_string());
    run_state["error"] = serde_json::Value::Null;
    child.kill().expect("crash held workflow fixture");
    child.wait().expect("reap held workflow fixture");
    fs::write(
        &run_state_path,
        serde_json::to_vec_pretty(&run_state).expect("serialize durable workflow outcome"),
    )
    .expect("persist workflow outcome before TaskRegistry projection");

    let previous_home = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };
    let transcript =
        orca_runtime::history::load_session(&session_id).expect("crashed workflow session");
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    config.history_mode = HistoryMode::Resume(session_id);
    let host = RuntimeHost::start().expect("takeover runtime host");
    let thread = host
        .surface_handle()
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(config, "recover durable workflow result")
                .with_preloaded(transcript),
        )
        .expect("recover durable workflow result thread");
    let recovered = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("takeover snapshot attachment failed"),
    };
    assert!(
        recovered.operation_history.iter().any(|operation| {
            operation.operation_id == operation_id
                && matches!(
                    operation.terminal.as_ref().map(|record| &record.terminal),
                    Some(OperationTerminal::Succeeded { .. })
                )
        }),
        "durable workflow success was not projected into its operation"
    );
    assert!(recovered.workflows.iter().any(|workflow| {
        workflow.workflow_run_id.as_str() == workflow_run_id
            && workflow.status == orca_runtime::surface::SurfaceWorkflowStatus::Completed
            && workflow
                .result
                .as_ref()
                .is_some_and(|result| result.content.as_str() == "durable workflow result")
    }));
    assert!(recovered.tasks.iter().any(|task| {
        task.task_id.as_str() == task_id
            && task.status == orca_runtime::surface::SurfaceTaskStatus::Completed
    }));
    let worker_path = run_state_path
        .parent()
        .expect("run directory")
        .join("worker.json");
    let worker: serde_json::Value =
        serde_json::from_slice(&fs::read(worker_path).expect("workflow worker record"))
            .expect("workflow worker JSON");
    assert_eq!(worker["active"], false);
    host.shutdown().expect("shutdown takeover host");
    match previous_home {
        Some(value) => unsafe { std::env::set_var("ORCA_HOME", value) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn workflow_launch_replay_uses_surface_identity_when_activation_store_is_missing() {
    if !orca_runtime::workflow::host::WorkflowHost::node_available() {
        return;
    }
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().expect("temporary ORCA_HOME");
    let cwd = tempdir().expect("workflow cwd");
    let workflow_dir = cwd.path().join(".orca").join("workflows");
    fs::create_dir_all(&workflow_dir).expect("workflow directory");
    fs::write(
        workflow_dir.join("crash-running.js"),
        "export const meta = { name: 'crash-running', description: 'crash running', phases: ['main'] };\nexport default await phase('main', async () => agent('mock_stream_delay_ms 30000'));",
    )
    .expect("saved workflow");
    orca_core::config::folder_trust::set_trust_with_config_dir(
        cwd.path(),
        home.path(),
        orca_core::config::folder_trust::TrustLevel::Trusted,
    )
    .expect("trusted workflow workspace");
    let fixture_output = home.path().join("workflow-launch-surface-identity.json");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("typed_workflow_crash_fixture")
        .arg("--nocapture")
        .env("ORCA_WORKFLOW_CRASH_FIXTURE", "1")
        .env("ORCA_WORKFLOW_HOLD_FIXTURE", "1")
        .env("ORCA_WORKFLOW_CRASH_HOME", home.path())
        .env("ORCA_WORKFLOW_CRASH_CWD", cwd.path())
        .env("ORCA_WORKFLOW_CRASH_OUTPUT", &fixture_output)
        .spawn()
        .expect("spawn held workflow fixture");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture_output.exists() {
        assert!(
            Instant::now() < deadline,
            "held workflow fixture must publish its durable identity"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture_output).expect("workflow fixture identity"))
            .expect("workflow fixture identity JSON");
    let session_id = identity["session_id"].as_str().unwrap().to_string();
    let task_id = identity["task_id"].as_str().unwrap();
    let request_id = SurfaceRequestId::try_from_bytes(
        *uuid::Uuid::parse_str(identity["request_id"].as_str().unwrap())
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    let operation_id = orca_runtime::surface::SurfaceOperationId::try_from_bytes(
        *uuid::Uuid::parse_str(identity["operation_id"].as_str().unwrap())
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    let tasks_path = home
        .path()
        .join("task-sessions")
        .join(&session_id)
        .join("tasks.json");
    let mut tasks: serde_json::Value =
        serde_json::from_slice(&fs::read(&tasks_path).expect("activation task store"))
            .expect("activation task store JSON");
    tasks
        .as_object_mut()
        .expect("task store object")
        .remove(task_id);
    fs::write(
        &tasks_path,
        serde_json::to_vec_pretty(&tasks).expect("serialize activation store gap"),
    )
    .expect("remove activation-only workflow input");
    child.kill().expect("crash held workflow fixture");
    child.wait().expect("reap held workflow fixture");

    let previous_home = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };
    let transcript =
        orca_runtime::history::load_session(&session_id).expect("crashed workflow session");
    let mut config = test_config(cwd.path().to_path_buf());
    config.approval_mode = ApprovalMode::FullAuto;
    config.history_mode = HistoryMode::Resume(session_id);
    let host = RuntimeHost::start().expect("takeover runtime host");
    let thread = host
        .surface_handle()
        .start_thread_with_request(
            RuntimeThreadStartRequest::new(config, "replay surface-owned workflow identity")
                .with_preloaded(transcript),
        )
        .expect("take over workflow thread");
    let attachment = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManageWorkflow,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("takeover attachment failed"),
    };
    let replay = attachment
        .client
        .workflow_control(
            request_id,
            WorkflowControlAction::Launch {
                catalog_entry_id: SurfaceCatalogEntryId::try_new("crash-running").unwrap(),
                observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                args: Vec::new(),
                parent: None,
            },
        )
        .expect("replay surface-owned launch");
    assert!(matches!(
        replay,
        MutationReply::Committed { mutation, value }
            if mutation.disposition == MutationDisposition::AlreadyApplied
                && value.operation_id == Some(operation_id)
    ));
    host.shutdown().expect("shutdown takeover host");
    match previous_home {
        Some(value) => unsafe { std::env::set_var("ORCA_HOME", value) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn typed_workflow_crash_fixture() {
    if std::env::var_os("ORCA_WORKFLOW_CRASH_FIXTURE").is_none() {
        return;
    }
    let home = std::path::PathBuf::from(
        std::env::var_os("ORCA_WORKFLOW_CRASH_HOME").expect("fixture ORCA_HOME"),
    );
    let cwd =
        std::path::PathBuf::from(std::env::var_os("ORCA_WORKFLOW_CRASH_CWD").expect("fixture cwd"));
    let output_path = std::path::PathBuf::from(
        std::env::var_os("ORCA_WORKFLOW_CRASH_OUTPUT").expect("fixture output"),
    );
    unsafe { std::env::set_var("ORCA_HOME", &home) };
    let mut config = test_config(cwd);
    config.approval_mode = ApprovalMode::FullAuto;
    let host = RuntimeHost::start().expect("fixture runtime host");
    let thread = host
        .surface_handle()
        .start_thread(config, "crash workflow fixture")
        .expect("fixture typed thread");
    let attachment = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManageWorkflow,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fixture attachment failed"),
    };
    let launch_request_id = SurfaceRequestId::new();
    let output = match attachment
        .client
        .workflow_control(
            launch_request_id.clone(),
            WorkflowControlAction::Launch {
                catalog_entry_id: SurfaceCatalogEntryId::try_new("crash-running").unwrap(),
                observed_catalog_revision: WorkflowCatalogRevision::try_new(1).unwrap(),
                args: Vec::new(),
                parent: None,
            },
        )
        .expect("fixture workflow launch")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("fixture workflow launch must commit"),
    };
    fs::write(
        output_path,
        serde_json::to_vec(&serde_json::json!({
            "session_id": thread.thread_id(),
            "request_id": uuid::Uuid::from_bytes(*launch_request_id.as_bytes()).to_string(),
            "operation_id": uuid::Uuid::from_bytes(
                *output.operation_id.expect("fixture operation").as_bytes()
            )
            .to_string(),
            "workflow_run_id": output.workflow.workflow_run_id.as_str(),
            "task_id": output.workflow.task_id.as_str(),
        }))
        .expect("serialize fixture identity"),
    )
    .expect("write fixture identity");
    if std::env::var_os("ORCA_WORKFLOW_HOLD_FIXTURE").is_some() {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    std::process::exit(0);
}

fn test_config(cwd: std::path::PathBuf) -> RunConfig {
    RunConfig {
        app_version: "test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::Suggest,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).expect("default model"),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Record,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        budget: Default::default(),
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::default(),
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: false,
    }
}

fn ephemeral_one_shot_intent(
    snapshot: &orca_runtime::surface::SurfaceSnapshot,
    text: &str,
) -> OperationRequestIntent {
    OperationRequestIntent {
        correlation: OperationIngressCorrelation::TuiUser,
        kind: OperationKind::UserTurn,
        input: Some(SurfaceInputRequest {
            blocks: orca_runtime::surface::NonEmptyVec::try_new(vec![
                SurfaceInputRequestBlock::Text {
                    text: DisplayText::new(text),
                },
            ])
            .expect("one-shot input"),
        }),
        replayability: ReplayabilityRequest::NonReplayable {
            reason: NonReplayableReason::HistoryDisabled,
        },
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: snapshot.settings.thread_revision,
            expected_policy_epoch: snapshot.settings.effective.policy_epoch,
        },
    }
}

fn directory_tree_has_files(path: &std::path::Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        path.is_file() || (path.is_dir() && directory_tree_has_files(&path))
    })
}
