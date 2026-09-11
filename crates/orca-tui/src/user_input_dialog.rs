use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::TextArea;

use crate::idle_submit_actions::submit_pending_user_input_response;
use crate::protocol::{
    TuiUserInputAnswer, TuiUserInputQuestionnaire, TuiUserInputResponse, UserAction,
};
use crate::types::AppState;

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
struct UserInputQuestionState {
    id: String,
    header: String,
    question: String,
    choices: Vec<UserInputChoice>,
    selected: usize,
    checked: Vec<bool>,
    multi_select: bool,
    answer: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UserInputDialogMode {
    Choices,
    CustomAnswer { value: String },
    Chat { value: String },
    ConfirmUnanswered { selected: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UserInputDialog {
    questions: Vec<UserInputQuestionState>,
    active: usize,
    mode: UserInputDialogMode,
}

impl UserInputDialog {
    pub(crate) fn new(questionnaire: TuiUserInputQuestionnaire) -> Self {
        let questions = questionnaire
            .questions
            .into_iter()
            .map(|question| {
                let choices = question
                    .options
                    .into_iter()
                    .map(|option| UserInputChoice {
                        label: option.label,
                        description: option.description,
                        preview: option.preview,
                    })
                    .collect::<Vec<_>>();
                UserInputQuestionState {
                    id: question.id,
                    header: question.header,
                    question: question.question,
                    checked: vec![false; choices.len()],
                    choices,
                    selected: 0,
                    multi_select: question.multi_select,
                    answer: None,
                }
            })
            .collect::<Vec<_>>();
        Self {
            questions,
            active: 0,
            mode: UserInputDialogMode::Choices,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_legacy(question: &str, choices: Vec<String>) -> Self {
        Self::new(TuiUserInputQuestionnaire {
            questions: vec![crate::protocol::TuiUserInputQuestion {
                id: "question-1".to_string(),
                header: "Question".to_string(),
                question: question.to_string(),
                options: choices
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
                        crate::protocol::TuiUserInputOption {
                            label: label.trim().to_string(),
                            description: description.trim().to_string(),
                            preview,
                        }
                    })
                    .collect(),
                multi_select: false,
            }],
        })
    }

    fn active_question(&self) -> &UserInputQuestionState {
        &self.questions[self.active]
    }

    fn active_question_mut(&mut self) -> &mut UserInputQuestionState {
        &mut self.questions[self.active]
    }

    pub(crate) fn question(&self) -> &str {
        &self.active_question().question
    }

    pub(crate) fn header(&self) -> &str {
        &self.active_question().header
    }

    pub(crate) fn question_count(&self) -> usize {
        self.questions.len()
    }

    pub(crate) fn active_index(&self) -> usize {
        self.active
    }

    pub(crate) fn question_tabs(&self) -> impl Iterator<Item = (&str, bool)> {
        self.questions
            .iter()
            .map(|question| (question.header.as_str(), question.is_answered()))
    }

    pub(crate) fn all_answered(&self) -> bool {
        self.questions
            .iter()
            .all(UserInputQuestionState::is_answered)
    }

    pub(crate) fn choices(&self) -> &[UserInputChoice] {
        &self.active_question().choices
    }

    pub(crate) fn selected(&self) -> usize {
        self.active_question().selected
    }

    pub(crate) fn multi_select(&self) -> bool {
        self.active_question().multi_select
    }

    pub(crate) fn is_checked(&self, index: usize) -> bool {
        self.active_question()
            .checked
            .get(index)
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn selected_preview(&self) -> Option<&str> {
        self.active_question()
            .choices
            .get(self.selected())
            .and_then(|choice| choice.preview.as_deref())
    }

    pub(crate) fn mode(&self) -> &UserInputDialogMode {
        &self.mode
    }

    pub(crate) fn confirmation_selected(&self) -> Option<usize> {
        match self.mode {
            UserInputDialogMode::ConfirmUnanswered { selected } => Some(selected),
            _ => None,
        }
    }

    fn move_previous(&mut self) {
        let question = self.active_question_mut();
        let count = question.choices.len() + 1;
        question.selected = question.selected.checked_sub(1).unwrap_or(count - 1);
    }

    fn move_next(&mut self) {
        let question = self.active_question_mut();
        question.selected = (question.selected + 1) % (question.choices.len() + 1);
    }

    fn toggle_selected(&mut self) {
        let question = self.active_question_mut();
        if question.selected < question.checked.len() {
            question.checked[question.selected] = !question.checked[question.selected];
            let answers = question
                .choices
                .iter()
                .zip(&question.checked)
                .filter(|(_, checked)| **checked)
                .map(|(choice, _)| choice.label.clone())
                .collect::<Vec<_>>();
            question.answer = (!answers.is_empty()).then_some(answers);
        }
    }

    fn select_number(&mut self, index: usize) {
        let question = self.active_question_mut();
        if index < question.choices.len() {
            question.selected = index;
        }
    }

    fn confirm_active(&mut self) -> UserInputDialogAction {
        let question = self.active_question_mut();
        if question.selected == question.choices.len() {
            self.mode = UserInputDialogMode::CustomAnswer {
                value: String::new(),
            };
            return UserInputDialogAction::Handled;
        }
        if question.multi_select {
            let answers = question
                .choices
                .iter()
                .zip(&question.checked)
                .filter(|(_, checked)| **checked)
                .map(|(choice, _)| choice.label.clone())
                .collect::<Vec<_>>();
            if answers.is_empty() {
                question.checked[question.selected] = true;
                question.answer = Some(vec![question.choices[question.selected].label.clone()]);
                return UserInputDialogAction::Handled;
            }
            question.answer = Some(answers);
        } else {
            question.answer = Some(vec![question.choices[question.selected].label.clone()]);
        }
        self.advance_or_submit()
    }

    fn advance_or_submit(&mut self) -> UserInputDialogAction {
        if self.active + 1 < self.questions.len() {
            self.active += 1;
            self.mode = UserInputDialogMode::Choices;
            UserInputDialogAction::Handled
        } else {
            self.begin_submit()
        }
    }

    fn begin_submit(&mut self) -> UserInputDialogAction {
        if let Some(first_unanswered) = self
            .questions
            .iter()
            .position(|question| !question.is_answered())
        {
            self.active = first_unanswered;
            self.mode = UserInputDialogMode::ConfirmUnanswered { selected: 0 };
            UserInputDialogAction::Handled
        } else {
            UserInputDialogAction::Submit(self.submitted_response())
        }
    }

    fn submitted_response(&self) -> TuiUserInputResponse {
        TuiUserInputResponse::Submitted {
            answers: self
                .questions
                .iter()
                .filter_map(|question| {
                    question.answer.as_ref().map(|answers| TuiUserInputAnswer {
                        question_id: question.id.clone(),
                        answers: answers.clone(),
                    })
                })
                .collect(),
        }
    }

    pub(crate) fn response_summary(&self, response: &TuiUserInputResponse) -> String {
        match response {
            TuiUserInputResponse::Submitted { answers } => {
                let mut lines = vec!["✓ Your answers".to_string()];
                for answer in answers {
                    let header = self
                        .questions
                        .iter()
                        .find(|question| question.id == answer.question_id)
                        .map_or(answer.question_id.as_str(), |question| {
                            question.header.as_str()
                        });
                    lines.push(format!("  · {header} → {}", answer.answers.join(", ")));
                }
                lines.join("\n")
            }
            TuiUserInputResponse::Chat { message } => {
                format!("✓ Chat response\n  {message}")
            }
            TuiUserInputResponse::Cancelled => "Questionnaire cancelled".to_string(),
        }
    }

    fn previous_question(&mut self) {
        if self.active > 0 {
            self.active -= 1;
        }
        self.mode = UserInputDialogMode::Choices;
    }

    fn next_question(&mut self) -> UserInputDialogAction {
        if self.active + 1 < self.questions.len() {
            self.active += 1;
            self.mode = UserInputDialogMode::Choices;
            UserInputDialogAction::Handled
        } else {
            self.begin_submit()
        }
    }

    fn begin_custom(&mut self, first: Option<char>) {
        self.mode = UserInputDialogMode::CustomAnswer {
            value: first.map(String::from).unwrap_or_default(),
        };
    }

    fn begin_chat(&mut self) {
        let value = match &self.mode {
            UserInputDialogMode::CustomAnswer { value } | UserInputDialogMode::Chat { value } => {
                value.clone()
            }
            _ => String::new(),
        };
        self.mode = UserInputDialogMode::Chat { value };
    }

    fn append_text(&mut self, text: &str) -> bool {
        match &mut self.mode {
            UserInputDialogMode::CustomAnswer { value } | UserInputDialogMode::Chat { value } => {
                value.push_str(text);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn insert_paste(&mut self, text: &str) -> bool {
        if !matches!(
            self.mode,
            UserInputDialogMode::CustomAnswer { .. } | UserInputDialogMode::Chat { .. }
        ) {
            self.begin_custom(None);
        }
        self.append_text(text)
    }

    fn backspace(&mut self) {
        if let UserInputDialogMode::CustomAnswer { value } | UserInputDialogMode::Chat { value } =
            &mut self.mode
        {
            value.pop();
        }
    }

    fn submit_text(&mut self) -> UserInputDialogAction {
        match self.mode.clone() {
            UserInputDialogMode::CustomAnswer { value } if !value.trim().is_empty() => {
                self.active_question_mut().answer = Some(vec![value.trim().to_string()]);
                self.advance_or_submit()
            }
            UserInputDialogMode::Chat { value } if !value.trim().is_empty() => {
                UserInputDialogAction::Submit(TuiUserInputResponse::Chat {
                    message: value.trim().to_string(),
                })
            }
            _ => UserInputDialogAction::Handled,
        }
    }

    fn confirm_unanswered(&mut self) -> UserInputDialogAction {
        let UserInputDialogMode::ConfirmUnanswered { selected } = self.mode else {
            return UserInputDialogAction::Handled;
        };
        if selected == 0 {
            self.mode = UserInputDialogMode::Choices;
            UserInputDialogAction::Handled
        } else {
            UserInputDialogAction::Submit(self.submitted_response())
        }
    }
}

impl UserInputQuestionState {
    fn is_answered(&self) -> bool {
        self.answer
            .as_ref()
            .is_some_and(|answers| !answers.is_empty())
    }
}

enum UserInputDialogAction {
    Handled,
    Submit(TuiUserInputResponse),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserInputDialogKeyFlow {
    Handled,
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
    if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
        state
            .user_input_dialog
            .as_mut()
            .expect("dialog checked by caller")
            .begin_chat();
        return UserInputDialogKeyFlow::Handled;
    }
    match (key.code, key.modifiers) {
        (KeyCode::PageUp, KeyModifiers::NONE) => {
            state.scroll_up(state.viewport.visible_height.saturating_sub(2));
            return UserInputDialogKeyFlow::Handled;
        }
        (KeyCode::PageDown, KeyModifiers::NONE) => {
            state.scroll_down(state.viewport.visible_height.saturating_sub(2));
            return UserInputDialogKeyFlow::Handled;
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            state.scroll_up(state.viewport.visible_height / 2);
            return UserInputDialogKeyFlow::Handled;
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            state.scroll_down(state.viewport.visible_height / 2);
            return UserInputDialogKeyFlow::Handled;
        }
        _ => {}
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return UserInputDialogKeyFlow::Handled;
    }

    let mode = state
        .user_input_dialog
        .as_ref()
        .expect("dialog checked by caller")
        .mode()
        .clone();
    if matches!(
        mode,
        UserInputDialogMode::CustomAnswer { .. } | UserInputDialogMode::Chat { .. }
    ) {
        return match key.code {
            KeyCode::Enter => {
                let action = state
                    .user_input_dialog
                    .as_mut()
                    .expect("dialog checked by caller")
                    .submit_text();
                submit_dialog_action(action, state, textarea, action_tx);
                UserInputDialogKeyFlow::Handled
            }
            KeyCode::Esc => {
                state
                    .user_input_dialog
                    .as_mut()
                    .expect("dialog checked by caller")
                    .mode = UserInputDialogMode::Choices;
                UserInputDialogKeyFlow::Handled
            }
            KeyCode::Backspace | KeyCode::Delete => {
                state
                    .user_input_dialog
                    .as_mut()
                    .expect("dialog checked by caller")
                    .backspace();
                UserInputDialogKeyFlow::Handled
            }
            KeyCode::Char(character) => {
                state
                    .user_input_dialog
                    .as_mut()
                    .expect("dialog checked by caller")
                    .append_text(&character.to_string());
                UserInputDialogKeyFlow::Handled
            }
            _ => UserInputDialogKeyFlow::Handled,
        };
    }
    if let UserInputDialogMode::ConfirmUnanswered { .. } = mode {
        return match key.code {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Char('j')
            | KeyCode::Char('k')
            | KeyCode::Tab
            | KeyCode::BackTab => {
                if let Some(UserInputDialogMode::ConfirmUnanswered { selected }) = state
                    .user_input_dialog
                    .as_mut()
                    .map(|dialog| &mut dialog.mode)
                {
                    *selected = 1 - *selected;
                }
                UserInputDialogKeyFlow::Handled
            }
            KeyCode::Enter => {
                let action = state
                    .user_input_dialog
                    .as_mut()
                    .expect("dialog checked by caller")
                    .confirm_unanswered();
                submit_dialog_action(action, state, textarea, action_tx);
                UserInputDialogKeyFlow::Handled
            }
            KeyCode::Esc => {
                state
                    .user_input_dialog
                    .as_mut()
                    .expect("dialog checked by caller")
                    .mode = UserInputDialogMode::Choices;
                UserInputDialogKeyFlow::Handled
            }
            _ => UserInputDialogKeyFlow::Handled,
        };
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .move_previous();
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Down | KeyCode::Char('j') => {
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
                .is_some_and(UserInputDialog::multi_select) =>
        {
            state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .toggle_selected();
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Enter => {
            let action = state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .confirm_active();
            submit_dialog_action(action, state, textarea, action_tx);
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Tab | KeyCode::Right => {
            let action = state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .next_question();
            submit_dialog_action(action, state, textarea, action_tx);
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Left | KeyCode::BackTab => {
            state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .previous_question();
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Char(character) if character.is_ascii_digit() && character != '0' => {
            let index = character.to_digit(10).unwrap_or(1) as usize - 1;
            if let Some(dialog) = state.user_input_dialog.as_mut() {
                dialog.select_number(index);
            }
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Char('z') => {
            state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .begin_custom(None);
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Char(character) => {
            state
                .user_input_dialog
                .as_mut()
                .expect("dialog checked by caller")
                .begin_custom(Some(character));
            UserInputDialogKeyFlow::Handled
        }
        KeyCode::Esc => {
            submit_pending_user_input_response(
                TuiUserInputResponse::Cancelled,
                textarea,
                state,
                action_tx,
            );
            UserInputDialogKeyFlow::Handled
        }
        _ => UserInputDialogKeyFlow::Handled,
    }
}

fn submit_dialog_action(
    action: UserInputDialogAction,
    state: &mut AppState,
    textarea: &TextArea,
    action_tx: &mpsc::Sender<UserAction>,
) {
    if let UserInputDialogAction::Submit(response) = action {
        submit_pending_user_input_response(response, textarea, state, action_tx);
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
        state.user_input_dialog = Some(UserInputDialog::from_legacy(
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

        let action = action_rx.try_recv();
        assert!(
            matches!(
                action,
                Ok(UserAction::RespondToInteraction {
                    key: ref actual_key,
                    response: TuiInteractionResponse::UserQuestionnaire(
                        TuiUserInputResponse::Submitted { ref answers },
                    ),
                }) if actual_key == &key
                    && *answers == vec![TuiUserInputAnswer {
                        question_id: "question-1".to_string(),
                        answers: vec!["Improve".to_string()],
                    }]
            ),
            "unexpected action: {action:?}"
        );
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
        let (mut state, _) = state_with_dialog("Signals: Which?");
        state
            .user_input_dialog
            .as_mut()
            .expect("dialog")
            .active_question_mut()
            .multi_select = true;
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
                response: TuiInteractionResponse::UserQuestionnaire(answer),
                ..
            }) if answer == TuiUserInputResponse::Submitted {
                answers: vec![TuiUserInputAnswer {
                    question_id: "question-1".to_string(),
                    answers: vec!["Audit".to_string(), "Improve".to_string()],
                }],
            }
        ));
    }

    #[test]
    fn typing_starts_inline_custom_answer_without_discarding_questionnaire() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, _) = state_with_dialog("Task: Which path?");
        let textarea = TextArea::default();
        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Char('自'), KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );
        assert!(matches!(
            state
                .user_input_dialog
                .as_ref()
                .map(UserInputDialog::mode),
            Some(UserInputDialogMode::CustomAnswer { value }) if value == "自"
        ));
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn vim_navigation_and_transcript_scrolling_remain_available() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, _) = state_with_dialog("Task: Which path?");
        state.viewport.total_lines = 100;
        state.viewport.visible_height = 20;
        state.viewport.scroll_offset = 80;
        let textarea = TextArea::default();

        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );
        assert_eq!(
            state.user_input_dialog.as_ref().expect("dialog").selected(),
            1
        );
        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );
        assert_eq!(
            state.user_input_dialog.as_ref().expect("dialog").selected(),
            0
        );

        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );
        assert_eq!(state.viewport.scroll_offset, 62);
        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &mut state,
            &textarea,
            &action_tx,
        );
        assert_eq!(state.viewport.scroll_offset, 72);
        assert!(matches!(
            state.user_input_dialog.as_ref().map(UserInputDialog::mode),
            Some(UserInputDialogMode::Choices)
        ));
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn escape_cancels_questionnaire_and_wakes_runtime() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, key) = state_with_dialog("Task: Which path?");
        let textarea = TextArea::default();

        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RespondToInteraction {
                key: actual_key,
                response: TuiInteractionResponse::UserQuestionnaire(
                    TuiUserInputResponse::Cancelled,
                ),
            }) if actual_key == key
        ));
        assert!(state.user_input_dialog.is_none());
        assert_eq!(state.status, AppStatus::Running);
    }

    #[test]
    fn ctrl_t_submits_chat_response_for_follow_up() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, key) = state_with_dialog("Task: Which path?");
        let textarea = TextArea::default();

        for key in [
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            handle_user_input_dialog_key(&key, &mut state, &textarea, &action_tx);
        }

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RespondToInteraction {
                key: actual_key,
                response: TuiInteractionResponse::UserQuestionnaire(
                    TuiUserInputResponse::Chat { message },
                ),
            }) if actual_key == key && message == "Why?"
        ));
    }

    #[test]
    fn ctrl_t_preserves_existing_custom_and_chat_text() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, key) = state_with_dialog("Task: Which path?");
        let textarea = TextArea::default();

        for key in [
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            handle_user_input_dialog_key(&key, &mut state, &textarea, &action_tx);
        }

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RespondToInteraction {
                key: actual_key,
                response: TuiInteractionResponse::UserQuestionnaire(
                    TuiUserInputResponse::Chat { message },
                ),
            }) if actual_key == key && message == "no"
        ));
    }

    #[test]
    fn unanswered_confirmation_can_submit_only_answered_questions() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, key) = state_with_dialog("First?");
        let first = state.user_input_dialog.as_ref().expect("dialog").questions[0].clone();
        state.user_input_dialog.as_mut().expect("dialog").questions = vec![
            first,
            UserInputQuestionState {
                id: "question-2".to_string(),
                header: "Second".to_string(),
                question: "Second?".to_string(),
                choices: vec![
                    UserInputChoice {
                        label: "Keep".to_string(),
                        description: "Keep it".to_string(),
                        preview: None,
                    },
                    UserInputChoice {
                        label: "Replace".to_string(),
                        description: "Replace it".to_string(),
                        preview: None,
                    },
                ],
                selected: 0,
                checked: vec![false, false],
                multi_select: false,
                answer: None,
            },
        ];
        let textarea = TextArea::default();

        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
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
            state.user_input_dialog.as_ref().map(UserInputDialog::mode),
            Some(UserInputDialogMode::ConfirmUnanswered { selected: 0 })
        ));
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
                response: TuiInteractionResponse::UserQuestionnaire(
                    TuiUserInputResponse::Submitted { answers },
                ),
            }) if actual_key == key
                && answers == vec![TuiUserInputAnswer {
                    question_id: "question-2".to_string(),
                    answers: vec!["Keep".to_string()],
                }]
        ));
    }

    #[test]
    fn multi_question_navigation_preserves_answers_and_submits_once() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let (mut state, key) = state_with_dialog("First?");
        let second = crate::protocol::TuiUserInputQuestion {
            id: "question-2".to_string(),
            header: "Second".to_string(),
            question: "Second?".to_string(),
            options: vec![
                crate::protocol::TuiUserInputOption {
                    label: "Keep".to_string(),
                    description: "Keep it".to_string(),
                    preview: None,
                },
                crate::protocol::TuiUserInputOption {
                    label: "Replace".to_string(),
                    description: "Replace it".to_string(),
                    preview: None,
                },
            ],
            multi_select: false,
        };
        state
            .user_input_dialog
            .as_mut()
            .expect("dialog")
            .questions
            .push(UserInputQuestionState {
                id: second.id,
                header: second.header,
                question: second.question,
                checked: vec![false; second.options.len()],
                choices: second
                    .options
                    .into_iter()
                    .map(|option| UserInputChoice {
                        label: option.label,
                        description: option.description,
                        preview: option.preview,
                    })
                    .collect(),
                selected: 0,
                multi_select: second.multi_select,
                answer: None,
            });
        let textarea = TextArea::default();

        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );
        assert_eq!(
            state
                .user_input_dialog
                .as_ref()
                .expect("dialog")
                .active_index(),
            1
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
                response: TuiInteractionResponse::UserQuestionnaire(
                    TuiUserInputResponse::Submitted { answers },
                ),
            }) if actual_key == key
                && answers.len() == 2
                && answers[0].answers == ["Audit"]
                && answers[1].answers == ["Replace"]
        ));
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

    #[test]
    fn committed_choice_submission_appends_answer_summary() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let (mut state, key) = state_with_dialog("Task: Which path?");
        let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
        let mut vim = crate::vim::VimState::new(false);
        let mut textarea = TextArea::default();

        handle_user_input_dialog_key(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &textarea,
            &action_tx,
        );
        crate::runtime_event_actions::handle_interaction_response_ack(
            crate::action_dispatcher::InteractionResponseAck::Committed { key },
            &mut state,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(state.interaction.pending_submission.is_none());
        assert!(state.transcript.messages.iter().any(|message| matches!(
            message,
            crate::transcript_state::ChatMessage::System(summary)
                if summary.contains("Your answers")
                    && summary.contains("Audit")
        )));
    }
}
