use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::TextArea;

use crate::idle_submit_actions::submit_pending_user_input_choice;
use crate::protocol::UserAction;
use crate::types::AppState;

const MULTI_SELECT_HINT: &str =
    "\nSelect one or more choices separated by commas, or type a custom answer.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UserInputChoice {
    label: String,
    description: String,
    preview: Option<String>,
}

impl UserInputChoice {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UserInputDialog {
    question: String,
    choices: Vec<UserInputChoice>,
    selected: usize,
    checked: Vec<bool>,
    multi_select: bool,
}

impl UserInputDialog {
    pub(crate) fn new(question: &str, choices: Vec<String>) -> Self {
        let (question, multi_select) = question
            .strip_suffix(MULTI_SELECT_HINT)
            .map_or((question, false), |question| (question, true));
        let choices = choices
            .into_iter()
            .map(|choice| {
                let (body, preview) = choice
                    .split_once("\nPreview:\n")
                    .map_or((choice.as_str(), None), |(body, preview)| {
                        (body, Some(preview.trim().to_string()))
                    });
                let (label, description) = body
                    .split_once(" - ")
                    .map_or((body, ""), |(label, description)| (label, description));
                UserInputChoice {
                    label: label.trim().to_string(),
                    description: description.trim().to_string(),
                    preview,
                }
            })
            .collect::<Vec<_>>();
        Self {
            question: question.to_string(),
            checked: vec![false; choices.len()],
            choices,
            selected: 0,
            multi_select,
        }
    }

    pub(crate) fn question(&self) -> &str {
        &self.question
    }

    pub(crate) fn choices(&self) -> &[UserInputChoice] {
        &self.choices
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn multi_select(&self) -> bool {
        self.multi_select
    }

    pub(crate) fn is_checked(&self, index: usize) -> bool {
        self.checked.get(index).copied().unwrap_or(false)
    }

    pub(crate) fn selected_preview(&self) -> Option<&str> {
        self.choices
            .get(self.selected)
            .and_then(|choice| choice.preview.as_deref())
    }

    fn move_previous(&mut self) {
        let count = self.choices.len() + 1;
        self.selected = self.selected.checked_sub(1).unwrap_or(count - 1);
    }

    fn move_next(&mut self) {
        self.selected = (self.selected + 1) % (self.choices.len() + 1);
    }

    fn toggle_selected(&mut self) {
        if let Some(checked) = self.checked.get_mut(self.selected) {
            *checked = !*checked;
        }
    }

    fn answer(&self) -> Option<String> {
        if self.selected == self.choices.len() {
            return None;
        }
        if self.multi_select {
            let checked = self
                .choices
                .iter()
                .zip(&self.checked)
                .filter(|(_, checked)| **checked)
                .map(|(choice, _)| choice.label.as_str())
                .collect::<Vec<_>>();
            if !checked.is_empty() {
                return Some(checked.join(", "));
            }
        }
        self.choices
            .get(self.selected)
            .map(|choice| choice.label.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserInputDialogKeyFlow {
    Handled,
    Composer,
}

pub(crate) fn handle_user_input_dialog_key(
    key: &KeyEvent,
    state: &mut AppState,
    textarea: &TextArea,
    action_tx: &mpsc::Sender<UserAction>,
) -> UserInputDialogKeyFlow {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return UserInputDialogKeyFlow::Handled;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return UserInputDialogKeyFlow::Handled;
    }

    match key.code {
        KeyCode::Up | KeyCode::BackTab => {
            state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .move_previous();
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Down | KeyCode::Tab => {
            state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .move_next();
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Char(' ')
            if state
                .user_input_dialog
                .as_ref()
                .is_some_and(|d| d.multi_select) =>
        {
            state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .toggle_selected();
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Enter => {
            let answer = state
                .user_input_dialog
                .as_ref()
                .expect("dialog checked by caller")
                .answer();
            if let Some(answer) = answer {
                submit_pending_user_input_choice(answer, textarea, state, action_tx);
            } else {
                state.user_input_dialog = None;
            }
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Char(character) if character.is_ascii_digit() && character != '0' => {
            let index = character.to_digit(10).unwrap_or(1) as usize - 1;
            if let Some(dialog) = state.user_input_dialog.as_mut()
                && index < dialog.choices.len()
            {
                dialog.selected = index;
            }
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete => {
            state.user_input_dialog = None;
            UserInputDialogKeyFlow::Composer
        }
        KeyCode::Esc => {
            state.user_input_dialog = None;
            UserInputDialogKeyFlow::Handled
        }
        _ => UserInputDialogKeyFlow::Handled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use orca_core::cancel::OperationIdAllocator;

    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::protocol::{
        PendingTuiInput, TuiInteractionKey, TuiInteractionKind, TuiInteractionResponse,
    };
    use crate::types::AppStatus;

    fn state_with_dialog(question: &str) -> (AppState, TuiInteractionKey) {
        let (tx, _rx) = mpsc::unbounded();
        let key = TuiInteractionKey::new(
            OperationIdAllocator::new().allocate(),
            "ask-1",
            TuiInteractionKind::UserInput,
        );
        let mut state = AppState::new(tx, "test".to_string(), "auto".to_string(), "/tmp".into());
        state.status = AppStatus::WaitingUserInput;
        state.interaction.pending_input = Some(PendingTuiInput::UserInput(key.clone()));
        state.user_input_dialog = Some(UserInputDialog::new(
            question,
            vec![
                "Audit - Run the existing checks".to_string(),
                "Improve - Replace placeholders".to_string(),
            ],
        ));
        (state, key)
    }

    #[test]
    fn enter_submits_selected_label_and_preserves_existing_draft() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, key) = state_with_dialog("Task: Which path?");
        let textarea = make_textarea_with_text(
            "$harness-creator: reply in Chinese",
            &crate::vim::VimState::new(false),
            &crate::theme::Theme::named(orca_core::config::ThemeName::Dark),
        );
        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );
        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RespondToInteraction {
                key: actual_key,
                response: TuiInteractionResponse::UserInput(answer),
            }) if actual_key == key && answer == "Improve"
        ));
        assert_eq!(
            textarea_text(&textarea),
            "$harness-creator: reply in Chinese"
        );
        assert_eq!(state.status, AppStatus::Running);
        assert!(state.user_input_dialog.is_none());
        assert!(state.interaction.pending_submission.is_some());
    }

    #[test]
    fn multi_select_submits_checked_labels() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, _) = state_with_dialog(&format!("Signals: Which?{MULTI_SELECT_HINT}"));
        let textarea = TextArea::default();
        for code in [
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Enter,
        ] {
            handle_user_input_dialog_key(
                &KeyEvent::new(code, KeyModifiers::NONE),
                &mut state,
                &textarea,
                &action_tx,
            );
        }
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RespondToInteraction {
                response: TuiInteractionResponse::UserInput(answer),
                ..
            }) if answer == "Audit, Improve"
        ));
    }

    #[test]
    fn typing_switches_to_custom_composer_answer() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, _) = state_with_dialog("Task: Which path?");
        let textarea = TextArea::default();
        assert_eq!(
            handle_user_input_dialog_key(
                &KeyEvent::new(KeyCode::Char('自'), KeyModifiers::NONE),
                &mut state,
                &textarea,
                &action_tx,
            ),
            UserInputDialogKeyFlow::Composer
        );
        assert!(state.user_input_dialog.is_none());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn failed_choice_submission_restores_dialog_and_preserved_draft() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let (mut state, key) = state_with_dialog("Task: Which path?");
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = crate::vim::VimState::new(false);
        let mut textarea = make_textarea_with_text("keep this draft", &vim, &theme);

        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );
        crate::runtime_event_actions::handle_interaction_response_ack(
            crate::action_dispatcher::InteractionResponseAck::Failed {
                key,
                message: "runtime unavailable".to_string(),
            },
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert_eq!(state.status, AppStatus::WaitingUserInput);
        assert!(state.user_input_dialog.is_some());
        assert_eq!(textarea_text(&textarea), "keep this draft");
    }
}
