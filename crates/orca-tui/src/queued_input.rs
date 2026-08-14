use orca_runtime::mentions::MentionBindings;
use std::collections::VecDeque;

use crate::composer_textarea::{expand_pending_pastes_with_bindings, retain_active_pending_pastes};
use crate::types::{AppState, AppStatus, ChatMessage, UserAction};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedUserMessage {
    id: u64,
    visible_text: String,
    submission_text: String,
    composer_bindings: MentionBindings,
    submission_bindings: MentionBindings,
    pending_pastes: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedComposerState {
    pub(crate) visible_text: String,
    pub(crate) mention_bindings: MentionBindings,
    pub(crate) pending_pastes: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedPreviewSnapshot {
    pub(crate) len: usize,
    pub(crate) first: String,
    pub(crate) second: Option<String>,
    pub(crate) latest: Option<String>,
}

pub(crate) struct QueuedSubmissionView {
    pub(crate) preview: QueuedPreviewSnapshot,
    pub(crate) error: Option<String>,
}

pub(crate) struct QueuedSubmissionState {
    pending: VecDeque<QueuedUserMessage>,
    in_flight: Option<QueuedUserMessage>,
    autosend: bool,
    error: Option<String>,
    next_id: u64,
}

impl Default for QueuedSubmissionState {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            in_flight: None,
            autosend: true,
            error: None,
            next_id: 1,
        }
    }
}

impl QueuedSubmissionState {
    fn enqueue(
        &mut self,
        mut message: QueuedUserMessage,
        capacity: usize,
    ) -> Result<(), QueuedUserMessage> {
        if self.pending.len() >= capacity {
            return Err(message);
        }
        message.assign_id(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.pending.push_back(message);
        self.error = None;
        Ok(())
    }

    fn pop_latest(&mut self) -> Option<QueuedUserMessage> {
        let message = self.pending.pop_back();
        if message.is_some() {
            self.error = None;
        }
        message
    }

    fn begin_next(&mut self) -> Option<QueuedUserMessage> {
        if !self.autosend || self.in_flight.is_some() {
            return None;
        }
        let message = self.pending.pop_front()?;
        self.in_flight = Some(message.clone());
        self.error = None;
        Some(message)
    }

    fn in_flight_prompt(&self) -> Option<String> {
        self.in_flight
            .as_ref()
            .map(|message| message.submission_text().to_string())
    }

    fn rollback(&mut self) -> Option<QueuedUserMessage> {
        let message = self.in_flight.take()?;
        self.pending.push_front(message.clone());
        Some(message)
    }

    fn take_rejected(&mut self) -> Option<QueuedComposerState> {
        let message = self.in_flight.take()?;
        self.autosend = false;
        self.error = None;
        Some(message.into_composer_state())
    }

    fn suspend(&mut self) {
        self.autosend = false;
    }

    fn resume_autosend(&mut self) {
        self.autosend = true;
    }

    fn pending_or_in_flight(&self) -> bool {
        !self.pending.is_empty() || self.in_flight.is_some()
    }

    fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    fn matches_id(&self, id: u64) -> bool {
        self.in_flight
            .as_ref()
            .is_some_and(|message| message.id() == id)
    }

    fn settle_started(&mut self, id: u64) -> bool {
        if !self.matches_id(id) {
            return false;
        }
        self.in_flight = None;
        true
    }

    fn fail_dispatch(&mut self, error: String) -> Option<QueuedUserMessage> {
        let message = self.rollback()?;
        self.error = Some(error);
        Some(message)
    }

    fn report_error(&mut self, error: String) {
        self.error = Some(error);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn view(&self) -> Option<QueuedSubmissionView> {
        Some(QueuedSubmissionView {
            preview: QueuedPreviewSnapshot::from_queue(&self.pending)?,
            error: self.error.clone(),
        })
    }

    #[cfg(test)]
    fn pending_visible_text(&self) -> Vec<&str> {
        self.pending
            .iter()
            .map(QueuedUserMessage::visible_text)
            .collect()
    }

    #[cfg(test)]
    fn pending_submission_binding_count(&self, index: usize) -> Option<usize> {
        self.pending
            .get(index)
            .map(|message| message.submission_bindings().bindings().len())
    }

    #[cfg(test)]
    fn in_flight_id(&self) -> Option<u64> {
        self.in_flight.as_ref().map(QueuedUserMessage::id)
    }

    #[cfg(test)]
    fn autosend_enabled(&self) -> bool {
        self.autosend
    }

    #[cfg(test)]
    fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl QueuedPreviewSnapshot {
    pub(crate) fn from_queue(queue: &VecDeque<QueuedUserMessage>) -> Option<Self> {
        Self::from_queue_with(queue, || {})
    }

    fn from_queue_with(
        queue: &VecDeque<QueuedUserMessage>,
        mut on_read: impl FnMut(),
    ) -> Option<Self> {
        let len = queue.len();
        let first = queue.front().map(|message| {
            on_read();
            message.preview_text()
        })?;
        let (second, latest) = if len == 2 {
            let second = queue.get(1).map(|message| {
                on_read();
                message.preview_text()
            });
            (second, None)
        } else if len > 2 {
            let latest = queue.back().map(|message| {
                on_read();
                message.preview_text()
            });
            (None, latest)
        } else {
            (None, None)
        };
        Some(Self {
            len,
            first,
            second,
            latest,
        })
    }

    #[cfg(test)]
    fn from_queue_with_probe(
        queue: &VecDeque<QueuedUserMessage>,
        on_read: impl FnMut(),
    ) -> Option<Self> {
        Self::from_queue_with(queue, on_read)
    }
}

impl QueuedUserMessage {
    pub(crate) fn from_composer(
        visible_text: String,
        pending_pastes: Vec<(String, String)>,
        mut mention_bindings: MentionBindings,
    ) -> Option<Self> {
        mention_bindings.reconcile(&visible_text);

        let trimmed_visible = visible_text.trim().to_string();
        if trimmed_visible.is_empty() {
            return None;
        }

        let mut composer_bindings = mention_bindings.clone();
        composer_bindings.reconcile(&trimmed_visible);

        let mut submission_bindings = mention_bindings;
        let expanded = expand_pending_pastes_with_bindings(
            &visible_text,
            &pending_pastes,
            &mut submission_bindings,
        );
        let submission_text = expanded.trim().to_string();
        submission_bindings.reconcile(&submission_text);

        let mut pending_pastes = pending_pastes;
        retain_active_pending_pastes(&trimmed_visible, &mut pending_pastes);

        Some(Self {
            id: 0,
            visible_text: trimmed_visible,
            submission_text,
            composer_bindings,
            submission_bindings,
            pending_pastes,
        })
    }

    pub(crate) fn visible_text(&self) -> &str {
        &self.visible_text
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn assign_id(&mut self, id: u64) {
        self.id = id;
    }

    pub(crate) fn submission_text(&self) -> &str {
        &self.submission_text
    }

    #[cfg(test)]
    pub(crate) fn composer_bindings(&self) -> &MentionBindings {
        &self.composer_bindings
    }

    pub(crate) fn submission_bindings(&self) -> &MentionBindings {
        &self.submission_bindings
    }

    pub(crate) fn preview_text(&self) -> String {
        self.visible_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(crate) fn into_composer_state(self) -> QueuedComposerState {
        QueuedComposerState {
            visible_text: self.visible_text,
            mention_bindings: self.composer_bindings,
            pending_pastes: self.pending_pastes,
        }
    }
}

impl AppState {
    pub(crate) fn enqueue_user_message(
        &mut self,
        message: QueuedUserMessage,
    ) -> Result<(), QueuedUserMessage> {
        self.queued_submission
            .enqueue(message, crate::channels::USER_ACTION_CAPACITY)
    }

    pub(crate) fn pop_latest_queued_message(&mut self) -> Option<QueuedUserMessage> {
        self.queued_submission.pop_latest()
    }

    pub(crate) fn begin_next_queued_message(&mut self) -> Option<UserAction> {
        if self.status != AppStatus::Idle {
            return None;
        }
        let message = self.queued_submission.begin_next()?;
        self.push_message(ChatMessage::User(message.visible_text().to_string()));
        self.enter_running();
        self.scroll_to_bottom();
        Some(UserAction::SubmitQueued {
            id: message.id(),
            prompt: message.submission_text().to_string(),
            bindings: message.submission_bindings().clone(),
        })
    }

    pub(crate) fn commit_queued_submission_admission(&mut self) {
        let Some(prompt) = self.queued_submission.in_flight_prompt() else {
            return;
        };
        self.record_prompt(prompt);
    }

    pub(crate) fn take_rejected_queued_composer_state(&mut self) -> Option<QueuedComposerState> {
        self.queued_submission.take_rejected()
    }

    pub(crate) fn suspend_queued_follow_up_autosend(&mut self) {
        self.queued_submission.suspend();
    }

    pub(crate) fn resume_queued_follow_up_autosend(&mut self) {
        self.queued_submission.resume_autosend();
    }

    pub(crate) fn queued_follow_up_pending_or_in_flight(&self) -> bool {
        self.queued_submission.pending_or_in_flight()
    }

    pub(crate) fn queued_submission_in_flight(&self) -> bool {
        self.queued_submission.in_flight()
    }

    pub(crate) fn queued_submission_matches_id(&self, id: u64) -> bool {
        self.queued_submission.matches_id(id)
    }

    pub(crate) fn settle_queued_submission_started(&mut self, id: u64) -> bool {
        self.queued_submission.settle_started(id)
    }

    pub(crate) fn fail_queued_submission_dispatch(
        &mut self,
        error: String,
    ) -> Option<QueuedUserMessage> {
        let message = self.queued_submission.fail_dispatch(error)?;
        self.remove_after_last_user();
        self.set_status(AppStatus::Idle);
        Some(message)
    }

    pub(crate) fn report_queued_input_error(&mut self, error: String) {
        self.queued_submission.report_error(error);
    }

    pub(crate) fn queued_submission_view(&self) -> Option<QueuedSubmissionView> {
        self.queued_submission.view()
    }

    pub(crate) fn reset_queued_user_messages(&mut self) {
        self.queued_submission.reset();
    }

    #[cfg(test)]
    pub(crate) fn queued_pending_visible_text(&self) -> Vec<&str> {
        self.queued_submission.pending_visible_text()
    }

    #[cfg(test)]
    pub(crate) fn queued_pending_submission_binding_count(&self, index: usize) -> Option<usize> {
        self.queued_submission
            .pending_submission_binding_count(index)
    }

    #[cfg(test)]
    pub(crate) fn queued_in_flight_id(&self) -> Option<u64> {
        self.queued_submission.in_flight_id()
    }

    #[cfg(test)]
    pub(crate) fn queued_autosend_enabled(&self) -> bool {
        self.queued_submission.autosend_enabled()
    }

    #[cfg(test)]
    pub(crate) fn queued_input_error(&self) -> Option<&str> {
        self.queued_submission.error()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use orca_runtime::mentions::{MentionBinding, MentionBindings, MentionFileKind, MentionTarget};

    use super::*;
    use crate::types::TuiEvent;

    fn app_state() -> AppState {
        let (tx, _rx) = crossbeam_channel::unbounded();
        AppState::new(
            tx,
            "0.0.0-test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        )
    }

    fn queued(text: &str) -> QueuedUserMessage {
        QueuedUserMessage::from_composer(text.to_string(), Vec::new(), MentionBindings::default())
            .unwrap()
    }

    fn binding(text: &str, visible: &str) -> MentionBindings {
        let start = text.find(visible).expect("visible mention");
        MentionBindings::from_bindings(
            text,
            vec![MentionBinding {
                start,
                end: start + visible.len(),
                visible: visible.to_string(),
                target: MentionTarget::File {
                    root: PathBuf::from("/workspace"),
                    path: visible.trim_start_matches('@').to_string(),
                    kind: MentionFileKind::File,
                },
            }],
        )
    }

    #[test]
    fn queued_message_rejects_blank_input_and_preserves_atomic_composer_state() {
        assert!(
            QueuedUserMessage::from_composer(
                " \n ".to_string(),
                Vec::new(),
                MentionBindings::default(),
            )
            .is_none()
        );

        let visible = "review @item.rs [Pasted Content 1001 chars]";
        let pasted = "body\n".repeat(201);
        let message = QueuedUserMessage::from_composer(
            visible.to_string(),
            vec![("[Pasted Content 1001 chars]".to_string(), pasted.clone())],
            binding(visible, "@item.rs"),
        )
        .expect("queued message");

        assert_eq!(message.visible_text(), visible);
        assert_eq!(
            message.submission_text(),
            format!("review @item.rs {}", pasted.trim())
        );
        assert_eq!(message.composer_bindings().bindings().len(), 1);
        assert_eq!(message.submission_bindings().bindings().len(), 1);

        let restored = message.into_composer_state();
        assert_eq!(restored.visible_text, visible);
        assert_eq!(restored.pending_pastes.len(), 1);
        assert_eq!(restored.mention_bindings.bindings().len(), 1);
    }

    #[test]
    fn queued_preview_collapses_whitespace_and_never_expands_large_paste() {
        let visible = "alpha\n  beta [Pasted Content 1001 chars]";
        let message = QueuedUserMessage::from_composer(
            visible.to_string(),
            vec![(
                "[Pasted Content 1001 chars]".to_string(),
                "secret payload\n".repeat(100),
            )],
            MentionBindings::default(),
        )
        .unwrap();

        assert_eq!(
            message.preview_text(),
            "alpha beta [Pasted Content 1001 chars]"
        );
        assert!(!message.preview_text().contains("secret payload"));
    }

    #[test]
    fn queued_preview_snapshot_reads_at_most_head_and_tail() {
        let queue = (0..64)
            .map(|index| {
                QueuedUserMessage::from_composer(
                    format!("item {index}"),
                    Vec::new(),
                    MentionBindings::default(),
                )
                .unwrap()
            })
            .collect::<VecDeque<_>>();
        let reads = Cell::new(0);
        let snapshot = QueuedPreviewSnapshot::from_queue_with_probe(&queue, || {
            reads.set(reads.get() + 1);
        })
        .unwrap();
        assert_eq!(snapshot.len, 64);
        assert_eq!(snapshot.first, "item 0");
        assert_eq!(snapshot.second, None);
        assert_eq!(snapshot.latest.as_deref(), Some("item 63"));
        assert!(reads.get() <= 2);
    }

    #[test]
    fn queued_preview_snapshot_reads_both_items_for_length_two() {
        let queue = ["first", "second"]
            .into_iter()
            .map(|text| {
                QueuedUserMessage::from_composer(
                    text.to_string(),
                    Vec::new(),
                    MentionBindings::default(),
                )
                .unwrap()
            })
            .collect::<VecDeque<_>>();
        let snapshot = QueuedPreviewSnapshot::from_queue(&queue).unwrap();
        assert_eq!(snapshot.first, "first");
        assert_eq!(snapshot.second.as_deref(), Some("second"));
        assert_eq!(snapshot.latest, None);
    }

    #[test]
    fn queued_message_retains_only_exact_active_overlapping_paste_placeholder() {
        let base = "[Pasted Content 1001 chars]";
        let suffixed = "[Pasted Content 1001 chars] #2";
        let message = QueuedUserMessage::from_composer(
            suffixed.to_string(),
            vec![
                (base.to_string(), "base payload".to_string()),
                (suffixed.to_string(), "second payload".to_string()),
            ],
            MentionBindings::default(),
        )
        .unwrap();

        let restored = message.into_composer_state();
        assert_eq!(
            restored.pending_pastes,
            vec![(suffixed.to_string(), "second payload".to_string())]
        );
    }

    #[test]
    fn queued_message_preserves_mention_between_multiple_paste_replacements() {
        let first = "[Pasted Content 1001 chars]";
        let second = "[Pasted Content 1001 chars] #2";
        let visible = format!("{first} review @item.rs {second}");
        let mention_start = visible.find("@item.rs").unwrap();
        let message = QueuedUserMessage::from_composer(
            visible.clone(),
            vec![
                (first.to_string(), "first payload".to_string()),
                (second.to_string(), "second payload".to_string()),
            ],
            MentionBindings::from_bindings(
                &visible,
                vec![MentionBinding {
                    start: mention_start,
                    end: mention_start + "@item.rs".len(),
                    visible: "@item.rs".to_string(),
                    target: MentionTarget::File {
                        root: PathBuf::from("/workspace"),
                        path: "item.rs".to_string(),
                        kind: MentionFileKind::File,
                    },
                }],
            ),
        )
        .unwrap();

        assert_eq!(message.submission_bindings().bindings().len(), 1);
        assert_eq!(
            message.submission_bindings().bindings()[0].visible,
            "@item.rs"
        );
        assert_eq!(
            &message.submission_text()[message.submission_bindings().bindings()[0].start
                ..message.submission_bindings().bindings()[0].end],
            "@item.rs"
        );
    }

    #[test]
    fn failed_dispatch_restores_fifo_and_reports_error_atomically() {
        let mut state = QueuedSubmissionState::default();
        for text in ["first", "second"] {
            state
                .enqueue(
                    QueuedUserMessage::from_composer(
                        text.to_string(),
                        Vec::new(),
                        MentionBindings::default(),
                    )
                    .unwrap(),
                    crate::channels::USER_ACTION_CAPACITY,
                )
                .unwrap();
        }

        assert_eq!(
            state.begin_next().unwrap().visible_text(),
            "first",
            "the FIFO head must own the admission fence"
        );
        state.fail_dispatch("follow-up action queue is full".to_string());

        assert_eq!(state.pending_visible_text(), vec!["first", "second"]);
        assert!(state.in_flight_id().is_none());
        assert!(state.autosend_enabled());
        assert_eq!(state.error(), Some("follow-up action queue is full"));
    }

    #[test]
    fn queued_follow_ups_promote_fifo_restore_lifo_and_fence_admission() {
        let mut state = app_state();
        state.enqueue_user_message(queued("first")).unwrap();
        state.enqueue_user_message(queued("second")).unwrap();
        state.enqueue_user_message(queued("third")).unwrap();
        state.set_status(AppStatus::Idle);

        let action = state.begin_next_queued_message().expect("first action");
        assert!(matches!(
            action,
            UserAction::SubmitQueued { prompt, .. } if prompt == "first"
        ));
        assert_eq!(state.queued_pending_visible_text(), vec!["second", "third"]);
        assert!(state.queued_submission_in_flight());
        assert!(state.begin_next_queued_message().is_none());
        assert!(matches!(
            state.messages.last(),
            Some(ChatMessage::User(text)) if text == "first"
        ));
        assert!(!state.input_history.iter().any(|entry| entry == "first"));
        state.commit_queued_submission_admission();
        assert_eq!(
            state.input_history.last().map(String::as_str),
            Some("first")
        );

        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        assert!(state.queued_submission_in_flight());
        let id = state.queued_in_flight_id().unwrap();
        state.update(TuiEvent::QueuedSubmissionStarted { id });
        assert!(!state.queued_submission_in_flight());
        assert_eq!(
            state.pop_latest_queued_message().unwrap().visible_text(),
            "third"
        );
        assert_eq!(state.queued_pending_visible_text(), vec!["second"]);
    }

    #[test]
    fn queued_follow_up_capacity_matches_user_action_mailbox() {
        let mut state = app_state();
        for index in 0..crate::channels::USER_ACTION_CAPACITY {
            state
                .enqueue_user_message(queued(&format!("queued {index}")))
                .unwrap();
        }
        let rejected = state
            .enqueue_user_message(queued("overflow"))
            .expect_err("65th item rejected");
        assert_eq!(rejected.visible_text(), "overflow");
        assert_eq!(
            state.queued_pending_visible_text().len(),
            crate::channels::USER_ACTION_CAPACITY
        );
    }

    #[test]
    fn queued_dispatch_failure_and_rejection_preserve_distinct_paths() {
        let mut state = app_state();
        state.enqueue_user_message(queued("first")).unwrap();
        state.set_status(AppStatus::Idle);
        state.begin_next_queued_message().unwrap();

        let rolled_back = state
            .fail_queued_submission_dispatch("follow-up action queue is full".to_string())
            .unwrap();
        assert_eq!(rolled_back.visible_text(), "first");
        assert_eq!(state.queued_pending_visible_text(), vec!["first"]);
        assert_eq!(
            state.queued_input_error(),
            Some("follow-up action queue is full")
        );
        assert!(
            !state
                .messages
                .iter()
                .any(|message| matches!(message, ChatMessage::User(text) if text == "first"))
        );
        assert!(!state.input_history.iter().any(|entry| entry == "first"));

        state.begin_next_queued_message().unwrap();
        state.update(TuiEvent::SubmissionRejected {
            queued_id: state.queued_in_flight_id(),
            prompt: "first".to_string(),
            message: "rejected".to_string(),
        });
        let restored = state.take_rejected_queued_composer_state().unwrap();
        assert_eq!(restored.visible_text, "first");
        assert!(!state.queued_submission_in_flight());
        assert!(!state.queued_autosend_enabled());
        assert!(state.queued_pending_visible_text().is_empty());
    }

    #[test]
    fn unrelated_turn_start_and_rejection_do_not_consume_queued_admission_fence() {
        let mut state = app_state();
        state.enqueue_user_message(queued("queued prompt")).unwrap();
        state.set_status(AppStatus::Idle);
        state.begin_next_queued_message().unwrap();

        state.update(TuiEvent::TurnStarted {
            turn: 1,
            task: None,
        });
        let live_id = state.queued_in_flight_id().unwrap();

        state.update(TuiEvent::QueuedSubmissionStarted { id: u64::MAX });
        assert_eq!(state.queued_in_flight_id(), Some(live_id));

        state.update(TuiEvent::SubmissionRejected {
            queued_id: Some(u64::MAX),
            prompt: "other prompt".to_string(),
            message: "other rejection".to_string(),
        });
        assert!(state.queued_submission_in_flight());
    }
}
