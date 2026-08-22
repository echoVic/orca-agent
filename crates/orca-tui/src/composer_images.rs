use base64::Engine as _;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use orca_core::conversation::{ImageDetail, ImageInput, ImageSource};
use orca_runtime::mentions::MentionBindings;
use std::fmt;
use std::io::Cursor;
use std::sync::Arc;

use crate::clipboard_image::{
    ClipboardImagePayload, MAX_COMPOSER_IMAGE_BYTES, MAX_COMPOSER_IMAGE_COUNT,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeferredImageSubmit {
    Submit,
    Queue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerImageAttachment {
    label: String,
    input: ImageInput,
    encoded_bytes: usize,
    preview: TuiImage,
}

#[derive(Clone, Eq, PartialEq)]
pub struct TuiImage {
    pub(crate) label: String,
    pub(crate) media_type: String,
    pub(crate) encoded: Option<Arc<[u8]>>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source_name: Option<String>,
}

impl fmt::Debug for TuiImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TuiImage")
            .field("label", &self.label)
            .field("media_type", &self.media_type)
            .field(
                "encoded_bytes",
                &self.encoded.as_ref().map_or(0, |bytes| bytes.len()),
            )
            .field("width", &self.width)
            .field("height", &self.height)
            .field("source_name", &self.source_name)
            .finish()
    }
}

impl ComposerImageAttachment {
    fn from_payload(label: String, payload: ClipboardImagePayload) -> Self {
        let encoded_bytes = payload.data.len();
        let preview = TuiImage {
            label: label.clone(),
            media_type: payload.media_type.clone(),
            encoded: Some(Arc::from(payload.data.clone())),
            width: payload.width,
            height: payload.height,
            source_name: payload.source_name.clone(),
        };
        Self {
            label,
            input: ImageInput {
                source: ImageSource::Base64 {
                    media_type: payload.media_type,
                    data: base64::engine::general_purpose::STANDARD.encode(payload.data),
                },
                detail: ImageDetail::High,
            },
            encoded_bytes,
            preview,
        }
    }

    #[cfg(test)]
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    #[cfg(test)]
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.preview.width, self.preview.height)
    }

    #[cfg(test)]
    pub(crate) fn source_name(&self) -> Option<&str> {
        self.preview.source_name.as_deref()
    }

    pub(crate) fn preview(&self) -> TuiImage {
        self.preview.clone()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ComposerImageState {
    attachments: Vec<ComposerImageAttachment>,
    next_number: usize,
    next_request_id: u64,
    in_flight_request: Option<u64>,
    deferred_submit: Option<DeferredImageSubmit>,
}

impl ComposerImageState {
    pub(crate) fn begin_paste(&mut self) -> Option<u64> {
        if self.in_flight_request.is_some() {
            return None;
        }
        self.next_request_id = self.next_request_id.saturating_add(1).max(1);
        self.in_flight_request = Some(self.next_request_id);
        Some(self.next_request_id)
    }

    pub(crate) fn is_paste_in_flight(&self) -> bool {
        self.in_flight_request.is_some()
    }

    pub(crate) fn is_current_request(&self, request_id: u64) -> bool {
        self.in_flight_request == Some(request_id)
    }

    pub(crate) fn defer_submit(&mut self, submit: DeferredImageSubmit) {
        if self.in_flight_request.is_some() {
            self.deferred_submit = Some(submit);
        }
    }

    pub(crate) fn fail_paste(&mut self, request_id: u64) -> bool {
        if self.in_flight_request != Some(request_id) {
            return false;
        }
        self.in_flight_request = None;
        self.deferred_submit = None;
        true
    }

    pub(crate) fn complete_paste(
        &mut self,
        request_id: u64,
        visible_text: &str,
        cursor: usize,
        payloads: Vec<ClipboardImagePayload>,
    ) -> Result<(String, usize, Option<DeferredImageSubmit>), String> {
        if self.in_flight_request != Some(request_id) {
            return Err("stale clipboard image paste completion".to_string());
        }
        self.in_flight_request = None;
        self.reconcile(visible_text);

        let next_count = self.attachments.len().saturating_add(payloads.len());
        if next_count > MAX_COMPOSER_IMAGE_COUNT {
            self.deferred_submit = None;
            return Err(format!(
                "image attachment count exceeds Orca's {MAX_COMPOSER_IMAGE_COUNT}-image limit"
            ));
        }
        let incoming_bytes = payloads
            .iter()
            .try_fold(0usize, |total, payload| {
                total.checked_add(payload.data.len())
            })
            .ok_or_else(|| "image attachment size overflow".to_string())?;
        let total_bytes = self
            .attachments
            .iter()
            .try_fold(incoming_bytes, |total, attachment| {
                total.checked_add(attachment.encoded_bytes)
            })
            .ok_or_else(|| "image attachment size overflow".to_string())?;
        if total_bytes > MAX_COMPOSER_IMAGE_BYTES {
            self.deferred_submit = None;
            return Err(format!(
                "attached images exceed Orca's {} MiB inline limit",
                MAX_COMPOSER_IMAGE_BYTES / (1024 * 1024)
            ));
        }

        let mut insertion = String::new();
        let before_cursor = visible_text.get(..cursor).unwrap_or(visible_text);
        if !before_cursor.is_empty() && !before_cursor.ends_with(char::is_whitespace) {
            insertion.push(' ');
        }
        for payload in payloads {
            self.next_number = self.next_number.saturating_add(1).max(1);
            let label = format!("[Image #{}]", self.next_number);
            if !insertion.is_empty() && !insertion.ends_with(char::is_whitespace) {
                insertion.push(' ');
            }
            insertion.push_str(&label);
            insertion.push(' ');
            self.attachments
                .push(ComposerImageAttachment::from_payload(label, payload));
        }
        let deferred = self.deferred_submit.take();
        Ok((insertion, self.attachments.len(), deferred))
    }

    pub(crate) fn attachments_for_text(
        &mut self,
        visible_text: &str,
    ) -> Vec<ComposerImageAttachment> {
        self.reconcile(visible_text);
        self.attachments.clone()
    }

    pub(crate) fn reconcile(&mut self, visible_text: &str) {
        self.attachments
            .retain(|attachment| visible_text.contains(&attachment.label));
    }

    pub(crate) fn clear_attachments(&mut self) {
        self.attachments = Vec::new();
        self.in_flight_request = None;
        self.deferred_submit = None;
    }

    pub(crate) fn reset_for_new_session(&mut self) {
        self.clear_attachments();
        self.next_number = 0;
    }

    pub(crate) fn restore(&mut self, attachments: Vec<ComposerImageAttachment>) {
        self.next_number = self.next_number.max(
            attachments
                .iter()
                .filter_map(|attachment| image_number(&attachment.label))
                .max()
                .unwrap_or(0),
        );
        self.attachments = attachments;
        self.in_flight_request = None;
        self.deferred_submit = None;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.attachments.is_empty()
    }

    pub(crate) fn preview_at_cursor(&self, visible_text: &str, cursor: usize) -> Option<TuiImage> {
        self.preview_at_cursor_with_separator(visible_text, cursor, true)
    }

    pub(crate) fn activatable_preview_at_cursor(
        &self,
        visible_text: &str,
        cursor: usize,
    ) -> Option<TuiImage> {
        self.preview_at_cursor_with_separator(visible_text, cursor, false)
    }

    fn preview_at_cursor_with_separator(
        &self,
        visible_text: &str,
        cursor: usize,
        include_trailing_separator: bool,
    ) -> Option<TuiImage> {
        self.attachments.iter().find_map(|attachment| {
            visible_text
                .match_indices(&attachment.label)
                .find_map(|(start, label)| {
                    let end = start + label.len();
                    let trailing_separator_len = if include_trailing_separator {
                        visible_text
                            .get(end..)
                            .and_then(|suffix| suffix.chars().next())
                            .filter(|character| character.is_whitespace())
                            .map(char::len_utf8)
                            .unwrap_or(0)
                    } else {
                        0
                    };
                    (start <= cursor && cursor <= end + trailing_separator_len)
                        .then(|| attachment.preview())
                })
        })
    }

    pub(crate) fn remove_for_key(
        &mut self,
        key: &KeyEvent,
        visible_text: &str,
        cursor: usize,
    ) -> Option<(String, usize)> {
        if key.kind != KeyEventKind::Press
            || !matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        self.reconcile(visible_text);
        let (attachment_index, mut start, mut end) =
            self.attachments
                .iter()
                .enumerate()
                .find_map(|(attachment_index, attachment)| {
                    visible_text
                        .match_indices(&attachment.label)
                        .find_map(|(start, label)| {
                            let end = start + label.len();
                            let trailing_separator_len = visible_text
                                .get(end..)
                                .and_then(|suffix| suffix.chars().next())
                                .filter(|character| character.is_whitespace())
                                .map(char::len_utf8)
                                .unwrap_or(0);
                            let targets_attachment = match key.code {
                                KeyCode::Backspace => {
                                    start < cursor && cursor <= end + trailing_separator_len
                                }
                                KeyCode::Delete => start <= cursor && cursor < end,
                                _ => false,
                            };
                            targets_attachment.then_some((attachment_index, start, end))
                        })
                })?;
        if visible_text
            .get(end..)
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(char::is_whitespace)
        {
            end += visible_text[end..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(0);
        } else if visible_text
            .get(..start)
            .and_then(|prefix| prefix.chars().next_back())
            .is_some_and(char::is_whitespace)
        {
            start -= visible_text[..start]
                .chars()
                .next_back()
                .map(char::len_utf8)
                .unwrap_or(0);
        }
        let mut next = visible_text.to_string();
        next.replace_range(start..end, "");
        self.attachments.remove(attachment_index);
        Some((next, start))
    }

    pub(crate) fn image_inputs(attachments: &[ComposerImageAttachment]) -> Vec<ImageInput> {
        attachments
            .iter()
            .map(|attachment| attachment.input.clone())
            .collect()
    }

    pub(crate) fn text_without_labels(
        visible_text: &str,
        attachments: &[ComposerImageAttachment],
    ) -> String {
        if attachments.is_empty() {
            return visible_text.to_string();
        }
        let mut text = visible_text.to_string();
        for attachment in attachments {
            text = text.replace(&attachment.label, "");
        }
        text.trim().to_string()
    }

    pub(crate) fn submission_text_and_bindings(
        visible_text: &str,
        attachments: &[ComposerImageAttachment],
        bindings: &MentionBindings,
    ) -> (String, MentionBindings) {
        let mut text = visible_text.to_string();
        let mut bindings = bindings.clone();
        bindings.reconcile(&text);
        let mut ranges = attachments
            .iter()
            .filter_map(|attachment| {
                text.find(&attachment.label)
                    .map(|start| (start, start + attachment.label.len()))
            })
            .collect::<Vec<_>>();
        ranges.sort_unstable_by_key(|(start, _)| std::cmp::Reverse(*start));
        for (start, end) in ranges {
            text.replace_range(start..end, "");
            bindings.reconcile(&text);
        }
        let text = text.trim().to_string();
        bindings.reconcile(&text);
        (text, bindings)
    }

    pub(crate) fn visible_text_with_attachments(
        visible_text: &str,
        attachments: &[ComposerImageAttachment],
    ) -> String {
        let mut text = visible_text.to_string();
        for attachment in attachments {
            if text.contains(&attachment.label) {
                continue;
            }
            if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                text.push(' ');
            }
            text.push_str(&attachment.label);
            text.push(' ');
        }
        text
    }

    pub(crate) fn restore_from_inputs(
        visible_text: &str,
        images: Vec<ImageInput>,
    ) -> (String, Vec<ComposerImageAttachment>) {
        let mut text = visible_text.to_string();
        let mut labels = image_labels_in_text(visible_text);
        let mut next_number = labels
            .iter()
            .filter_map(|label| image_number(label))
            .max()
            .unwrap_or(0);
        while labels.len() < images.len() {
            next_number = next_number.saturating_add(1).max(1);
            let label = format!("[Image #{next_number}]");
            if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                text.push(' ');
            }
            text.push_str(&label);
            text.push(' ');
            labels.push(label);
        }
        let attachments = images
            .into_iter()
            .zip(labels)
            .map(|(input, label)| {
                let (media_type, encoded): (String, Option<Arc<[u8]>>) = match &input.source {
                    ImageSource::Base64 { media_type, data } => (
                        media_type.clone(),
                        base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .ok()
                            .map(Arc::from),
                    ),
                    ImageSource::Url { .. } => ("image/url".to_string(), None),
                    ImageSource::File { .. } => ("image/file".to_string(), None),
                };
                let encoded_bytes = encoded.as_ref().map_or(0, |bytes| bytes.len());
                let (width, height) = encoded
                    .as_deref()
                    .and_then(|bytes| {
                        image::ImageReader::new(Cursor::new(bytes))
                            .with_guessed_format()
                            .ok()?
                            .into_dimensions()
                            .ok()
                    })
                    .filter(|(width, height)| {
                        u64::from(*width).saturating_mul(u64::from(*height))
                            <= crate::clipboard_image::MAX_COMPOSER_IMAGE_PIXELS
                    })
                    .unwrap_or((0, 0));
                ComposerImageAttachment {
                    label: label.clone(),
                    input,
                    encoded_bytes,
                    preview: TuiImage {
                        label,
                        media_type,
                        encoded,
                        width,
                        height,
                        source_name: None,
                    },
                }
            })
            .collect();
        (text, attachments)
    }
}

fn image_number(label: &str) -> Option<usize> {
    label
        .strip_prefix("[Image #")
        .and_then(|number| number.strip_suffix(']'))
        .and_then(|number| number.parse().ok())
}

fn image_labels_in_text(text: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[Image #") {
        rest = &rest[start..];
        let Some(end) = rest.find(']') else {
            break;
        };
        let label = &rest[..=end];
        if image_number(label).is_some() {
            labels.push(label.to_string());
        }
        rest = &rest[end + 1..];
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(seed: u8) -> ClipboardImagePayload {
        ClipboardImagePayload {
            media_type: "image/png".to_string(),
            data: vec![seed; 8],
            width: 2,
            height: 1,
            source_name: Some(format!("{seed}.png")),
        }
    }

    #[test]
    fn paste_assigns_stable_labels_and_preserves_metadata() {
        let mut state = ComposerImageState::default();
        let first = state.begin_paste().unwrap();
        let (inserted, count, deferred) = state
            .complete_paste(
                first,
                "inspect",
                "inspect".len(),
                vec![payload(1), payload(2)],
            )
            .unwrap();
        assert_eq!(inserted, " [Image #1] [Image #2] ");
        assert_eq!(count, 2);
        assert_eq!(deferred, None);
        assert_eq!(state.attachments[0].dimensions(), (2, 1));
        assert_eq!(state.attachments[1].source_name(), Some("2.png"));

        let visible = format!("inspect{inserted}");
        state.reconcile(&visible.replace("[Image #1] ", ""));
        assert_eq!(state.attachments.len(), 1);
        assert_eq!(state.attachments[0].label(), "[Image #2]");

        let next = state.begin_paste().unwrap();
        let (inserted, _, _) = state
            .complete_paste(next, &visible, visible.len(), vec![payload(3)])
            .unwrap();
        assert_eq!(inserted, "[Image #3] ");

        state.clear_attachments();
        let after_clear = state.begin_paste().unwrap();
        let (inserted, _, _) = state
            .complete_paste(after_clear, "", 0, vec![payload(4)])
            .unwrap();
        assert_eq!(inserted, "[Image #4] ");

        state.reset_for_new_session();
        let after_reset = state.begin_paste().unwrap();
        let (inserted, _, _) = state
            .complete_paste(after_reset, "", 0, vec![payload(5)])
            .unwrap();
        assert_eq!(inserted, "[Image #1] ");
    }

    #[test]
    fn deleting_inside_placeholder_removes_the_whole_attachment() {
        let mut state = ComposerImageState::default();
        let request = state.begin_paste().unwrap();
        let (inserted, _, _) = state
            .complete_paste(request, "", 0, vec![payload(1)])
            .unwrap();
        let visible = format!("{inserted}caption");
        let cursor = visible.find("#1").unwrap() + 1;
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let (next, next_cursor) = state.remove_for_key(&key, &visible, cursor).unwrap();
        assert_eq!(next, "caption");
        assert_eq!(next_cursor, 0);
        assert!(state.is_empty());
    }

    #[test]
    fn backspace_after_the_inserted_separator_removes_the_attachment_once() {
        let mut state = ComposerImageState::default();
        let request = state.begin_paste().unwrap();
        let (visible, _, _) = state
            .complete_paste(request, "", 0, vec![payload(1)])
            .unwrap();
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);

        let (next, next_cursor) = state
            .remove_for_key(&key, &visible, visible.len())
            .expect("attachment boundary");

        assert_eq!(next, "");
        assert_eq!(next_cursor, 0);
        assert!(state.is_empty());
    }

    #[test]
    fn deleting_a_duplicate_label_targets_the_occurrence_at_the_cursor() {
        let mut state = ComposerImageState::default();
        let request = state.begin_paste().unwrap();
        let _ = state
            .complete_paste(request, "", 0, vec![payload(1)])
            .unwrap();
        let visible = "literal [Image #1] then attachment [Image #1] ";
        let cursor = visible.rfind("#1").unwrap() + 1;
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);

        let (next, _) = state.remove_for_key(&key, visible, cursor).unwrap();

        assert_eq!(next, "literal [Image #1] then attachment ");
        assert!(state.is_empty());
    }

    #[test]
    fn stale_completion_cannot_mutate_newer_composer_state() {
        let mut state = ComposerImageState::default();
        let request = state.begin_paste().unwrap();
        assert!(state.fail_paste(request));
        let next = state.begin_paste().unwrap();
        assert!(next > request);
        assert!(
            state
                .complete_paste(request, "", 0, vec![payload(1)])
                .is_err()
        );
        assert!(state.is_empty());
        assert!(state.is_paste_in_flight());
    }

    #[test]
    fn history_text_preserves_user_whitespace_and_removes_only_attachment_labels() {
        let multiline = "first line\n  second line";
        assert_eq!(
            ComposerImageState::text_without_labels(multiline, &[]),
            multiline
        );

        let mut state = ComposerImageState::default();
        let request = state.begin_paste().unwrap();
        let _ = state
            .complete_paste(request, "", 0, vec![payload(1)])
            .unwrap();
        assert_eq!(
            ComposerImageState::text_without_labels(
                "first line\n[Image #1]  second line",
                &state.attachments,
            ),
            "first line\n  second line"
        );
    }

    #[test]
    fn submission_text_removes_labels_and_rebases_later_mentions() {
        use std::path::PathBuf;

        use orca_runtime::mentions::{MentionBinding, MentionFileKind, MentionTarget};

        let visible = "[Image #1] inspect @later.rs";
        let mention_start = visible.find("@later.rs").unwrap();
        let bindings = MentionBindings::from_bindings(
            visible,
            vec![MentionBinding {
                start: mention_start,
                end: mention_start + "@later.rs".len(),
                visible: "@later.rs".to_string(),
                target: MentionTarget::File {
                    root: PathBuf::from("/workspace"),
                    path: "later.rs".to_string(),
                    kind: MentionFileKind::File,
                },
            }],
        );
        let mut state = ComposerImageState::default();
        let request = state.begin_paste().unwrap();
        let _ = state
            .complete_paste(request, "", 0, vec![payload(1)])
            .unwrap();

        let (text, bindings) = ComposerImageState::submission_text_and_bindings(
            visible,
            &state.attachments,
            &bindings,
        );

        assert_eq!(text, "inspect @later.rs");
        assert_eq!(bindings.bindings().len(), 1);
        assert_eq!(bindings.bindings()[0].start, "inspect ".len());
    }

    #[test]
    fn rejection_text_restores_missing_labels_without_duplicating_existing_labels() {
        let mut state = ComposerImageState::default();
        let request = state.begin_paste().unwrap();
        let _ = state
            .complete_paste(request, "", 0, vec![payload(1)])
            .unwrap();

        assert_eq!(
            ComposerImageState::visible_text_with_attachments("inspect", &state.attachments),
            "inspect [Image #1] "
        );
        assert_eq!(
            ComposerImageState::visible_text_with_attachments(
                "inspect [Image #1] ",
                &state.attachments,
            ),
            "inspect [Image #1] "
        );
    }

    #[test]
    fn attachment_limits_are_checked_before_mutating_the_composer() {
        let mut state = ComposerImageState::default();
        let request = state.begin_paste().unwrap();
        let too_many = vec![payload(1); MAX_COMPOSER_IMAGE_COUNT + 1];
        assert!(state.complete_paste(request, "", 0, too_many).is_err());
        assert!(state.is_empty());
        assert!(!state.is_paste_in_flight());

        let request = state.begin_paste().unwrap();
        let oversized = ClipboardImagePayload {
            data: vec![0; MAX_COMPOSER_IMAGE_BYTES + 1],
            ..payload(2)
        };
        assert!(
            state
                .complete_paste(request, "", 0, vec![oversized])
                .is_err()
        );
        assert!(state.is_empty());
    }
}
