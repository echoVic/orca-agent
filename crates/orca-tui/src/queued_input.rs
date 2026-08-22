use orca_runtime::mentions::MentionBindings;
use std::collections::{HashMap, VecDeque};

use crate::composer_images::{ComposerImageAttachment, ComposerImageState};
use crate::composer_textarea::{expand_pending_pastes_with_bindings, retain_active_pending_pastes};
use crate::types::{AppState, UserAction};
#[cfg(test)]
use crate::types::{AppStatus, ChatMessage};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedUserMessage {
    id: u64,
    visible_text: String,
    submission_text: String,
    composer_bindings: MentionBindings,
    submission_bindings: MentionBindings,
    pending_pastes: Vec<(String, String)>,
    images: Vec<ComposerImageAttachment>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueuedComposerState {
    pub(crate) visible_text: String,
    pub(crate) mention_bindings: MentionBindings,
    pub(crate) pending_pastes: Vec<(String, String)>,
    pub(crate) images: Vec<ComposerImageAttachment>,
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
    projection: orca_runtime::prompt_queue::PromptQueueSnapshot,
    pending_composers: VecDeque<PendingQueuedComposer>,
    composers_by_id: HashMap<orca_runtime::prompt_queue::QueuedSubmissionId, QueuedComposerState>,
    pending_edit: Option<PendingQueuedEdit>,
    ready_edit: Option<QueuedComposerState>,
    error: Option<String>,
}

struct PendingQueuedComposer {
    submission_text: String,
    submission_bindings: MentionBindings,
    composer: QueuedComposerState,
}

struct PendingQueuedEdit {
    id: orca_runtime::prompt_queue::QueuedSubmissionId,
    composer: QueuedComposerState,
}

impl Default for QueuedSubmissionState {
    fn default() -> Self {
        Self {
            projection: orca_runtime::prompt_queue::PromptQueueSnapshot::default(),
            pending_composers: VecDeque::new(),
            composers_by_id: HashMap::new(),
            pending_edit: None,
            ready_edit: None,
            error: None,
        }
    }
}

impl QueuedSubmissionState {
    fn replace_runtime_projection(
        &mut self,
        snapshot: orca_runtime::prompt_queue::PromptQueueSnapshot,
        confirmed_delete: Option<&orca_runtime::prompt_queue::QueuedSubmissionId>,
    ) {
        for item in &snapshot.items {
            let is_new = !self
                .projection
                .items
                .iter()
                .any(|previous| previous.id == item.id);
            if !is_new || self.composers_by_id.contains_key(&item.id) {
                continue;
            }
            let Some(index) = self.pending_composers.iter().position(|pending| {
                pending.submission_text == item.input.text
                    && pending.submission_bindings == item.input.mention_bindings
                    && ComposerImageState::image_inputs(&pending.composer.images)
                        == item.input.images
            }) else {
                continue;
            };
            let pending = self
                .pending_composers
                .remove(index)
                .expect("matched pending queue composer");
            self.composers_by_id
                .insert(item.id.clone(), pending.composer);
        }
        if let Some(pending) = self.pending_edit.take() {
            if confirmed_delete == Some(&pending.id)
                && !snapshot.items.iter().any(|item| item.id == pending.id)
            {
                self.ready_edit = Some(pending.composer);
            } else {
                self.pending_edit = Some(pending);
            }
        }
        self.composers_by_id
            .retain(|id, _| snapshot.items.iter().any(|item| item.id == *id));
        self.projection = snapshot;
        self.error = None;
    }

    fn remember_composer(&mut self, message: QueuedUserMessage) {
        self.pending_composers.push_back(PendingQueuedComposer {
            submission_text: message.submission_text,
            submission_bindings: message.submission_bindings,
            composer: QueuedComposerState {
                visible_text: message.visible_text,
                mention_bindings: message.composer_bindings,
                pending_pastes: message.pending_pastes,
                images: message.images,
            },
        });
    }

    #[cfg(test)]
    fn enqueue(
        &mut self,
        message: QueuedUserMessage,
        capacity: usize,
    ) -> Result<(), QueuedUserMessage> {
        if self.projection.items.len() >= capacity {
            return Err(message);
        }
        let input = message.submission_text().to_string();
        let mut state =
            orca_runtime::prompt_queue::PromptQueueState::from_snapshot(self.projection.clone());
        self.projection = state
            .apply(
                orca_runtime::prompt_queue::PromptQueueAction::Add {
                    input: input.into(),
                },
                chrono::Utc::now().timestamp_millis(),
            )
            .expect("test queue input is valid");
        self.error = None;
        Ok(())
    }

    fn begin_latest_edit(&mut self) -> Option<orca_runtime::prompt_queue::PromptQueueAction> {
        if self.pending_edit.is_some() {
            return None;
        }
        let item = self.projection.items.last()?;
        self.pending_edit = Some(PendingQueuedEdit {
            id: item.id.clone(),
            composer: self
                .composers_by_id
                .get(&item.id)
                .cloned()
                .unwrap_or_else(|| queued_composer_from_runtime(item)),
        });
        Some(orca_runtime::prompt_queue::PromptQueueAction::Delete {
            expected_revision: self.projection.revision,
            id: item.id.clone(),
        })
    }

    fn cancel_latest_edit(&mut self) {
        self.pending_edit = None;
    }

    fn take_ready_edit(&mut self) -> Option<QueuedComposerState> {
        self.ready_edit.take()
    }

    #[cfg(test)]
    fn pop_latest(&mut self) -> Option<QueuedUserMessage> {
        let item = self.projection.items.pop()?;
        let (visible_text, images) =
            ComposerImageState::restore_from_inputs(&item.input.text, item.input.images.clone());
        Some(QueuedUserMessage {
            id: 0,
            visible_text,
            submission_text: item.input.text,
            composer_bindings: item.input.mention_bindings.clone(),
            submission_bindings: item.input.mention_bindings,
            pending_pastes: Vec::new(),
            images,
        })
    }

    #[cfg(test)]
    fn begin_next(&mut self) -> Option<QueuedUserMessage> {
        None
    }

    #[cfg(test)]
    fn in_flight_prompt(&self) -> Option<String> {
        None
    }

    #[cfg(test)]
    fn take_rejected(&mut self) -> Option<QueuedComposerState> {
        None
    }

    fn suspend(&mut self) {
        self.projection.paused = true;
    }

    fn resume_autosend(&mut self) {
        self.projection.paused = false;
    }

    fn pending_or_in_flight(&self) -> bool {
        !self.projection.items.is_empty() || self.projection.dispatch.is_some()
    }

    #[cfg(test)]
    fn in_flight(&self) -> bool {
        self.projection.dispatch.is_some()
    }

    #[cfg(test)]
    fn fail_dispatch(&mut self, error: String) -> Option<QueuedUserMessage> {
        self.error = Some(error);
        None
    }

    fn report_error(&mut self, error: String) {
        self.error = Some(error);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn view(&self) -> Option<QueuedSubmissionView> {
        Some(QueuedSubmissionView {
            preview: QueuedPreviewSnapshot::from_projection(&self.projection)?,
            error: self.error.clone(),
        })
    }

    #[cfg(test)]
    fn pending_visible_text(&self) -> Vec<&str> {
        self.projection
            .items
            .iter()
            .map(|item| item.input.text.as_str())
            .collect()
    }

    #[cfg(test)]
    fn pending_submission_binding_count(&self, index: usize) -> Option<usize> {
        self.projection.items.get(index).map(|_| 0)
    }

    #[cfg(test)]
    fn in_flight_id(&self) -> Option<u64> {
        None
    }

    #[cfg(test)]
    fn autosend_enabled(&self) -> bool {
        !self.projection.paused
    }

    #[cfg(test)]
    fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl QueuedPreviewSnapshot {
    fn from_projection(
        projection: &orca_runtime::prompt_queue::PromptQueueSnapshot,
    ) -> Option<Self> {
        let len = projection.items.len();
        let preview = |index: usize| {
            projection
                .items
                .get(index)
                .map(|item| compact_preview(&runtime_queue_preview_text(&item.input)))
        };
        Some(Self {
            len,
            first: preview(0)?,
            second: (len == 2).then(|| preview(1)).flatten(),
            latest: (len > 2).then(|| preview(len - 1)).flatten(),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_queue(queue: &VecDeque<QueuedUserMessage>) -> Option<Self> {
        Self::from_queue_with(queue, || {})
    }

    #[cfg(test)]
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

fn runtime_queue_preview_text(input: &orca_runtime::prompt_queue::PromptQueueInput) -> String {
    let mut text = input.text.trim().to_string();
    for number in 1..=input.images.len() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&format!("[Image #{number}]"));
    }
    text
}

fn compact_preview(text: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 256;
    const MAX_SCANNED_CHARS: usize = MAX_PREVIEW_CHARS * 4;

    let mut output = String::with_capacity(MAX_PREVIEW_CHARS);
    let mut output_chars = 0;
    let mut pending_space = false;
    for (scanned, ch) in text.chars().enumerate() {
        if scanned >= MAX_SCANNED_CHARS {
            append_preview_ellipsis(&mut output, &mut output_chars, MAX_PREVIEW_CHARS);
            return output;
        }
        if ch.is_whitespace() {
            pending_space |= !output.is_empty();
            continue;
        }
        if pending_space {
            if output_chars + 1 >= MAX_PREVIEW_CHARS {
                append_preview_ellipsis(&mut output, &mut output_chars, MAX_PREVIEW_CHARS);
                return output;
            }
            output.push(' ');
            output_chars += 1;
            pending_space = false;
        }
        if output_chars + 1 >= MAX_PREVIEW_CHARS {
            append_preview_ellipsis(&mut output, &mut output_chars, MAX_PREVIEW_CHARS);
            return output;
        }
        output.push(ch);
        output_chars += 1;
    }
    output
}

fn append_preview_ellipsis(output: &mut String, output_chars: &mut usize, limit: usize) {
    if *output_chars >= limit {
        output.pop();
        *output_chars = output_chars.saturating_sub(1);
    }
    output.push('…');
    *output_chars += 1;
}

fn queued_composer_from_runtime(
    item: &orca_runtime::prompt_queue::QueuedSubmission,
) -> QueuedComposerState {
    let (visible_text, images) =
        ComposerImageState::restore_from_inputs(&item.input.text, item.input.images.clone());
    QueuedComposerState {
        visible_text,
        mention_bindings: item.input.mention_bindings.clone(),
        pending_pastes: Vec::new(),
        images,
    }
}

impl QueuedUserMessage {
    #[cfg(test)]
    pub(crate) fn from_composer(
        visible_text: String,
        pending_pastes: Vec<(String, String)>,
        mention_bindings: MentionBindings,
    ) -> Option<Self> {
        Self::from_composer_with_images(visible_text, pending_pastes, mention_bindings, Vec::new())
    }

    pub(crate) fn from_composer_with_images(
        visible_text: String,
        pending_pastes: Vec<(String, String)>,
        mut mention_bindings: MentionBindings,
        images: Vec<ComposerImageAttachment>,
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
        let (submission_text, submission_bindings) =
            ComposerImageState::submission_text_and_bindings(
                &expanded,
                &images,
                &submission_bindings,
            );

        let mut pending_pastes = pending_pastes;
        retain_active_pending_pastes(&trimmed_visible, &mut pending_pastes);

        Some(Self {
            id: 0,
            visible_text: trimmed_visible,
            submission_text,
            composer_bindings,
            submission_bindings,
            pending_pastes,
            images,
        })
    }

    #[cfg(test)]
    pub(crate) fn visible_text(&self) -> &str {
        &self.visible_text
    }

    #[cfg(test)]
    pub(crate) fn id(&self) -> u64 {
        self.id
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

    pub(crate) fn images(&self) -> &[ComposerImageAttachment] {
        &self.images
    }

    #[cfg(test)]
    pub(crate) fn preview_text(&self) -> String {
        self.visible_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(test)]
    pub(crate) fn into_composer_state(self) -> QueuedComposerState {
        QueuedComposerState {
            visible_text: self.visible_text,
            mention_bindings: self.composer_bindings,
            pending_pastes: self.pending_pastes,
            images: self.images,
        }
    }
}

impl AppState {
    pub(crate) fn runtime_queue_revision(&self) -> orca_runtime::prompt_queue::QueueRevision {
        self.queued_submission.projection.revision
    }
    pub(crate) fn replace_runtime_queue_projection(
        &mut self,
        snapshot: orca_runtime::prompt_queue::PromptQueueSnapshot,
    ) {
        self.queued_submission
            .replace_runtime_projection(snapshot, None);
    }

    pub(crate) fn remember_runtime_queued_message(&mut self, message: QueuedUserMessage) {
        self.queued_submission.remember_composer(message);
    }

    pub(crate) fn replace_runtime_queue_control_projection(
        &mut self,
        snapshot: orca_runtime::prompt_queue::PromptQueueSnapshot,
        deleted_id: Option<&orca_runtime::prompt_queue::QueuedSubmissionId>,
    ) {
        self.queued_submission
            .replace_runtime_projection(snapshot, deleted_id);
    }
    #[cfg(test)]
    pub(crate) fn enqueue_user_message(
        &mut self,
        message: QueuedUserMessage,
    ) -> Result<(), QueuedUserMessage> {
        self.queued_submission
            .enqueue(message, crate::channels::USER_ACTION_CAPACITY)
    }

    #[cfg(test)]
    pub(crate) fn pop_latest_queued_message(&mut self) -> Option<QueuedUserMessage> {
        self.queued_submission.pop_latest()
    }

    pub(crate) fn begin_latest_queued_edit(
        &mut self,
    ) -> Option<orca_runtime::prompt_queue::PromptQueueAction> {
        self.queued_submission.begin_latest_edit()
    }

    pub(crate) fn cancel_latest_queued_edit(&mut self) {
        self.queued_submission.cancel_latest_edit();
    }

    pub(crate) fn take_ready_queued_composer_state(&mut self) -> Option<QueuedComposerState> {
        self.queued_submission.take_ready_edit()
    }

    #[cfg(test)]
    pub(crate) fn begin_next_queued_message(&mut self) -> Option<UserAction> {
        if self.status != AppStatus::Idle {
            return None;
        }
        let message = self.queued_submission.begin_next()?;
        self.push_user_message_with_images(message.visible_text().to_string(), message.images());
        self.enter_running();
        self.scroll_to_bottom();
        Some(UserAction::SubmitQueued {
            id: message.id(),
            prompt: message.submission_text().to_string(),
            bindings: message.submission_bindings().clone(),
            images: message.images().to_vec(),
        })
    }

    #[cfg(test)]
    pub(crate) fn commit_queued_submission_admission(&mut self) {
        let Some(prompt) = self.queued_submission.in_flight_prompt() else {
            return;
        };
        self.record_prompt(prompt);
    }

    #[cfg(test)]
    pub(crate) fn take_rejected_queued_composer_state(&mut self) -> Option<QueuedComposerState> {
        self.queued_submission.take_rejected()
    }

    pub(crate) fn suspend_queued_follow_up_autosend(&mut self) {
        self.queued_submission.suspend();
    }

    pub(crate) fn resume_queued_follow_up_autosend(&mut self) {
        self.queued_submission.resume_autosend();
    }

    pub(crate) fn request_runtime_queue_pause(&self) {
        if self.queued_submission.pending_or_in_flight()
            && !self.queued_submission.projection.paused
        {
            let _ = self.event_tx.send(UserAction::PromptQueueControl(
                orca_runtime::prompt_queue::PromptQueueAction::Pause {
                    expected_revision: self.runtime_queue_revision(),
                },
            ));
        }
    }

    pub(crate) fn request_runtime_queue_start(&self) {
        if self.queued_submission.pending_or_in_flight() && self.queued_submission.projection.paused
        {
            let _ = self.event_tx.send(UserAction::PromptQueueControl(
                orca_runtime::prompt_queue::PromptQueueAction::Start {
                    expected_revision: self.runtime_queue_revision(),
                },
            ));
        }
    }

    pub(crate) fn queued_follow_up_pending_or_in_flight(&self) -> bool {
        self.queued_submission.pending_or_in_flight()
    }

    #[cfg(test)]
    pub(crate) fn queued_submission_in_flight(&self) -> bool {
        self.queued_submission.in_flight()
    }

    #[cfg(test)]
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

    fn image_message() -> QueuedUserMessage {
        let payload = crate::clipboard_image::ClipboardImagePayload {
            media_type: "image/png".to_string(),
            data: b"\x89PNG\r\n\x1a\nfixture".to_vec(),
            width: 2,
            height: 1,
            source_name: None,
        };
        let mut images = ComposerImageState::default();
        let request = images.begin_paste().unwrap();
        let (label, _, _) = images
            .complete_paste(request, "inspect", 7, vec![payload])
            .unwrap();
        let visible = format!("inspect{label}");
        let attachments = images.attachments_for_text(&visible);
        QueuedUserMessage::from_composer_with_images(
            visible,
            Vec::new(),
            MentionBindings::default(),
            attachments,
        )
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
    fn queued_message_preserves_image_attachments_for_edit_restore() {
        let message = image_message();
        assert_eq!(message.images().len(), 1);
        assert_eq!(message.submission_text(), "inspect");
        let restored = message.into_composer_state();
        assert_eq!(restored.visible_text, "inspect [Image #1]");
        assert_eq!(restored.images.len(), 1);
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
    fn runtime_queue_preview_bounds_a_one_mib_whitespace_free_token() {
        let preview = compact_preview(&"x".repeat(1024 * 1024));

        assert_eq!(preview.chars().count(), 256);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn runtime_queue_preview_labels_image_only_input() {
        let input = orca_runtime::prompt_queue::PromptQueueInput {
            text: String::new(),
            mention_bindings: MentionBindings::default(),
            images: vec![orca_core::conversation::ImageInput {
                source: orca_core::conversation::ImageSource::Url {
                    url: "https://example.com/image.png".to_string(),
                },
                detail: orca_core::conversation::ImageDetail::High,
            }],
        };

        assert_eq!(runtime_queue_preview_text(&input), "[Image #1]");
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
    fn pause_and_start_are_routed_through_runtime_queue_control() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut state = AppState::new(
            tx,
            "0.0.0-test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut runtime = orca_runtime::prompt_queue::PromptQueueState::from_snapshot(
            orca_runtime::prompt_queue::PromptQueueSnapshot::default(),
        );
        let snapshot = runtime
            .apply(
                orca_runtime::prompt_queue::PromptQueueAction::Add {
                    input: "queued".into(),
                },
                1,
            )
            .unwrap();
        state.update(TuiEvent::PromptQueueUpdated(snapshot));

        let pause_revision = state.runtime_queue_revision();
        state.request_runtime_queue_pause();
        state.suspend_queued_follow_up_autosend();
        let pause = rx.try_recv().expect("runtime pause action");
        assert!(matches!(
            &pause,
            UserAction::PromptQueueControl(
                orca_runtime::prompt_queue::PromptQueueAction::Pause { expected_revision }
            ) if *expected_revision == pause_revision
        ));
        let snapshot = runtime
            .apply(
                match pause {
                    UserAction::PromptQueueControl(action) => action,
                    other => panic!("unexpected pause action: {other:?}"),
                },
                2,
            )
            .unwrap();
        state.update(TuiEvent::PromptQueueControlUpdated {
            deleted_id: None,
            snapshot,
        });
        assert!(!state.queued_autosend_enabled());

        let start_revision = state.runtime_queue_revision();
        state.request_runtime_queue_start();
        state.resume_queued_follow_up_autosend();
        let start = rx.try_recv().expect("runtime start action");
        assert!(matches!(
            &start,
            UserAction::PromptQueueControl(
                orca_runtime::prompt_queue::PromptQueueAction::Start { expected_revision }
            ) if *expected_revision == start_revision
        ));
        let snapshot = runtime
            .apply(
                match start {
                    UserAction::PromptQueueControl(action) => action,
                    other => panic!("unexpected start action: {other:?}"),
                },
                3,
            )
            .unwrap();
        state.update(TuiEvent::PromptQueueControlUpdated {
            deleted_id: None,
            snapshot,
        });
        assert!(state.queued_autosend_enabled());
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
    #[ignore = "legacy local queue dispatch shim removed"]
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
    #[ignore = "legacy local queue dispatch shim removed"]
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
    #[ignore = "legacy local queue dispatch shim removed"]
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
            bindings: MentionBindings::default(),
            images: Vec::new(),
            message: "rejected".to_string(),
        });
        let restored = state.take_rejected_queued_composer_state().unwrap();
        assert_eq!(restored.visible_text, "first");
        assert!(!state.queued_submission_in_flight());
        assert!(!state.queued_autosend_enabled());
        assert!(state.queued_pending_visible_text().is_empty());
    }

    #[test]
    #[ignore = "legacy local queue dispatch shim removed"]
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
            bindings: MentionBindings::default(),
            images: Vec::new(),
            message: "other rejection".to_string(),
        });
        assert!(state.queued_submission_in_flight());
    }
}
