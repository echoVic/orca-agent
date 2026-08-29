//! Interaction owner tests: pending runtime input and MCP acknowledgements.

use crossbeam_channel as mpsc;
use orca_file_search::SearchPhase;

use crate::protocol::{
    PendingTuiInput, TuiEvent, TuiInteractionKey, TuiInteractionKind, TuiMcpElicitationMode,
};
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus, SlashMenu, SlashMenuItem};

fn state() -> AppState {
    let (tx, _rx) = mpsc::unbounded();
    AppState::new(
        tx,
        "0.0.0-test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    )
}

fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
    TuiInteractionKey::new(
        orca_core::cancel::OperationIdAllocator::new().allocate(),
        id,
        kind,
    )
}

#[test]
fn user_input_requested_event_tracks_pending_runtime_interaction_id() {
    let mut state = state();
    state.slash_menu = Some(SlashMenu {
        items: vec![SlashMenuItem {
            command: "/config".to_string(),
            description: "Configure".to_string(),
        }],
        selected: 0,
        sub_menu: None,
    });
    state.mention.phase = Some(SearchPhase::Complete);
    state.update(TuiEvent::UserInputRequested {
        key: interaction_key(TuiInteractionKind::UserInput, "ask-1"),
        question: "Continue?".to_string(),
        choices: vec!["yes - Continue".to_string(), "no - Stop".to_string()],
    });

    assert_eq!(state.status, AppStatus::WaitingUserInput);
    assert!(matches!(
        state.interaction.pending_input.as_ref(),
        Some(PendingTuiInput::UserInput(key)) if key.request_id == "ask-1"
    ));
    assert!(state.interaction.pending_mcp_elicitation_mode.is_none());
    let dialog = state.user_input_dialog.as_ref().expect("choice dialog");
    assert_eq!(dialog.question(), "Continue?");
    assert_eq!(dialog.choices()[0].label(), "yes");
    assert!(state.slash_menu.is_none());
    assert!(state.mention.phase.is_none());
}

#[test]
fn mcp_elicitation_requested_event_tracks_pending_runtime_interaction_id() {
    let mut state = state();
    state.update(TuiEvent::McpElicitationRequested {
        key: interaction_key(
            TuiInteractionKind::McpElicitation,
            "mcp_elicitation:github:42",
        ),
        server_name: "github".to_string(),
        mode: TuiMcpElicitationMode::Url,
        message: "Authorize GitHub".to_string(),
        url: Some("https://github.com/login/device".to_string()),
        requested_schema_json: None,
    });

    assert_eq!(state.status, AppStatus::WaitingUserInput);
    assert!(matches!(
        state.interaction.pending_input.as_ref(),
        Some(PendingTuiInput::McpElicitation(key))
            if key.request_id == "mcp_elicitation:github:42"
    ));
    assert_eq!(
        state.interaction.pending_mcp_elicitation_mode,
        Some(TuiMcpElicitationMode::Url)
    );
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::System(message))
            if message.contains("MCP github requests input: Authorize GitHub")
                && message.contains("Mode: url")
                && message.contains("URL: https://github.com/login/device")
    ));
}

#[test]
fn session_completion_clears_pending_interaction_projection() {
    let mut input_state = state();
    input_state.update(TuiEvent::McpElicitationRequested {
        key: interaction_key(TuiInteractionKind::McpElicitation, "mcp-1"),
        server_name: "fixture".to_string(),
        mode: TuiMcpElicitationMode::Form,
        message: "Provide fields".to_string(),
        url: None,
        requested_schema_json: None,
    });
    input_state.stage_pending_interaction_submission("answer".to_string());
    assert!(input_state.interaction.pending_submission.is_some());

    input_state.update(TuiEvent::SessionCompleted {
        status: "interrupted".to_string(),
    });

    assert_eq!(input_state.status, AppStatus::Idle);
    assert!(input_state.interaction.pending_input.is_none());
    assert!(
        input_state
            .interaction
            .pending_mcp_elicitation_mode
            .is_none()
    );
    assert!(input_state.interaction.pending_submission.is_none());

    let mut approval_state = state();
    approval_state.update(TuiEvent::ApprovalNeeded {
        key: interaction_key(TuiInteractionKind::Approval, "approval-1"),
        tool: "bash".to_string(),
        target: Some("cargo test".to_string()),
        preview: None,
    });

    approval_state.update(TuiEvent::SessionCompleted {
        status: "interrupted".to_string(),
    });

    assert_eq!(approval_state.status, AppStatus::Idle);
    assert!(approval_state.approval_dialog.is_none());
}

#[test]
fn session_reset_clears_pending_mcp_mode_with_the_interaction_key() {
    let mut state = state();
    state.update(TuiEvent::McpElicitationRequested {
        key: interaction_key(TuiInteractionKind::McpElicitation, "mcp-reset"),
        server_name: "fixture".to_string(),
        mode: TuiMcpElicitationMode::Url,
        message: "Authorize".to_string(),
        url: Some("https://example.test/device".to_string()),
        requested_schema_json: None,
    });
    state.stage_pending_interaction_submission("answer".to_string());
    assert!(state.interaction.pending_submission.is_some());

    state.reset_session_projection();

    assert!(state.interaction.pending_input.is_none());
    assert!(state.interaction.pending_mcp_elicitation_mode.is_none());
    assert!(state.interaction.pending_submission.is_none());
}
