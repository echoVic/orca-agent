use crossbeam_channel::Receiver;
use tui_textarea::TextArea;

use crate::action_dispatcher::InteractionResponseAck;
use crate::runtime_event_actions::handle_interaction_response_ack;
use crate::theme::Theme;
use crate::types::AppState;
use crate::vim::VimState;

pub(crate) struct RendererInteractionAckOwner {
    acknowledgements: Receiver<InteractionResponseAck>,
}

impl RendererInteractionAckOwner {
    pub(crate) fn new(acknowledgements: Receiver<InteractionResponseAck>) -> Self {
        Self { acknowledgements }
    }

    pub(crate) fn drain(
        &self,
        state: &mut AppState,
        textarea: &mut TextArea,
        vim_state: &mut VimState,
        theme: &Theme,
    ) -> bool {
        let mut received = false;
        for acknowledgement in self.acknowledgements.try_iter() {
            handle_interaction_response_ack(acknowledgement, state, textarea, vim_state, theme);
            received = true;
        }
        received
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use tui_textarea::TextArea;

    use orca_core::cancel::OperationIdAllocator;
    use orca_core::config::ThemeName;

    use super::RendererInteractionAckOwner;
    use crate::action_dispatcher::InteractionResponseAck;
    use crate::composer_textarea::textarea_text;
    use crate::theme::Theme;
    use crate::types::{
        AppState, AppStatus, ChatMessage, PendingTuiInput, TuiInteractionKey, TuiInteractionKind,
    };
    use crate::vim::VimState;

    struct Fixture {
        state: AppState,
        textarea: TextArea<'static>,
        vim: VimState,
        theme: Theme,
    }

    impl Fixture {
        fn new() -> Self {
            let (action_tx, _action_rx) = mpsc::unbounded();
            Self {
                state: AppState::new(
                    action_tx,
                    "test".to_string(),
                    "mock".to_string(),
                    "/tmp".to_string(),
                ),
                textarea: TextArea::default(),
                vim: VimState::new(false),
                theme: Theme::named(ThemeName::Dark),
            }
        }

        fn drain(&mut self, owner: &RendererInteractionAckOwner) -> bool {
            owner.drain(
                &mut self.state,
                &mut self.textarea,
                &mut self.vim,
                &self.theme,
            )
        }
    }

    fn interaction_key(kind: TuiInteractionKind, id: &str) -> TuiInteractionKey {
        TuiInteractionKey::new(OperationIdAllocator::new().allocate(), id, kind)
    }

    #[test]
    fn empty_and_disconnected_drains_are_inert() {
        let (ack_tx, ack_rx) = mpsc::unbounded();
        let owner = RendererInteractionAckOwner::new(ack_rx);
        let mut fixture = Fixture::new();

        assert!(!fixture.drain(&owner));
        drop(ack_tx);
        assert!(!fixture.drain(&owner));
        assert_eq!(fixture.state.status, AppStatus::Idle);
        assert!(fixture.state.messages.is_empty());
    }

    #[test]
    fn no_op_acknowledgement_still_reports_batch_activity() {
        let (ack_tx, ack_rx) = mpsc::unbounded();
        let owner = RendererInteractionAckOwner::new(ack_rx);
        let mut fixture = Fixture::new();
        let key = interaction_key(TuiInteractionKind::UserInput, "already-committed");
        ack_tx
            .send(InteractionResponseAck::Committed { key })
            .expect("ack receiver alive");

        assert!(fixture.drain(&owner));
        assert_eq!(fixture.state.status, AppStatus::Idle);
        assert!(fixture.state.messages.is_empty());
        assert_eq!(textarea_text(&fixture.textarea), "");
        assert!(!fixture.drain(&owner));
    }

    #[test]
    fn queued_acknowledgements_drain_fifo_in_one_pass() {
        let (ack_tx, ack_rx) = mpsc::unbounded();
        let owner = RendererInteractionAckOwner::new(ack_rx);
        let mut fixture = Fixture::new();
        for (id, message) in [("first", "first rejection"), ("second", "second rejection")] {
            ack_tx
                .send(InteractionResponseAck::Failed {
                    key: interaction_key(TuiInteractionKind::Approval, id),
                    message: message.to_string(),
                })
                .expect("ack receiver alive");
        }

        assert!(fixture.drain(&owner));
        let errors: Vec<&str> = fixture
            .state
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Error(message) => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(errors, ["first rejection", "second rejection"]);
        assert!(!fixture.drain(&owner));
    }

    #[test]
    fn post_construction_failed_input_ack_uses_existing_restoration() {
        let (ack_tx, ack_rx) = mpsc::unbounded();
        let owner = RendererInteractionAckOwner::new(ack_rx);
        let mut fixture = Fixture::new();
        let key = interaction_key(TuiInteractionKind::UserInput, "retry");
        fixture.state.status = AppStatus::WaitingUserInput;
        fixture.state.pending_input = Some(PendingTuiInput::UserInput(key.clone()));
        assert_eq!(
            fixture
                .state
                .stage_pending_interaction_submission("exact answer".to_string()),
            Some(key.clone())
        );
        fixture.state.pending_input = None;
        fixture.state.enter_running();

        ack_tx
            .send(InteractionResponseAck::Failed {
                key: key.clone(),
                message: "runtime unavailable".to_string(),
            })
            .expect("owner retains receiver after construction");

        assert!(fixture.drain(&owner));
        assert_eq!(fixture.state.status, AppStatus::WaitingUserInput);
        assert!(matches!(
            fixture.state.pending_input.as_ref(),
            Some(PendingTuiInput::UserInput(actual)) if actual == &key
        ));
        assert_eq!(textarea_text(&fixture.textarea), "exact answer");
        assert!(fixture.state.messages.iter().any(
            |message| matches!(message, ChatMessage::Error(text) if text == "runtime unavailable")
        ));
    }
}
