use crossbeam_channel as mpsc;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tui_textarea::TextArea;

use crate::clipboard_image::ImagePasteRequest;
use crate::composer_textarea::{textarea_cursor_byte_index, textarea_text};
use crate::image_preview::{ImageViewerState, VIEWER_PAN_STEP};
use crate::protocol::UserAction;
use crate::transcript_state::ChatMessage;
use crate::types::{AppState, AppStatus, PanelMode};

pub(crate) fn handle_image_paste_shortcut(
    key: KeyEvent,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> bool {
    if !is_image_paste_key(key) {
        return false;
    }
    if state.panel_mode != PanelMode::Conversation
        || !matches!(state.status, AppStatus::Idle | AppStatus::Running)
        || state.config_dialog.is_some()
        || state.plan_approval_dialog.is_some()
        || state.user_input_dialog.is_some()
        || state.transcript.search.open
    {
        return false;
    }
    begin_image_paste(state, action_tx, ImagePasteRequest::Clipboard)
}

pub(crate) fn handle_image_viewer_key(key: KeyEvent, state: &mut AppState) -> bool {
    if state.image_viewer.is_none() {
        return false;
    }
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return true;
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        state.image_viewer = None;
        return true;
    }
    let viewer = state
        .image_viewer
        .as_mut()
        .expect("image viewer existence checked above");
    match key.code {
        KeyCode::Char('+') | KeyCode::Char('=') => viewer.zoom_in(),
        KeyCode::Char('-') | KeyCode::Char('_') => viewer.zoom_out(),
        KeyCode::Char('0') => viewer.reset_view(),
        KeyCode::Left => viewer.pan(-VIEWER_PAN_STEP, 0),
        KeyCode::Right => viewer.pan(VIEWER_PAN_STEP, 0),
        KeyCode::Up => viewer.pan(0, -VIEWER_PAN_STEP),
        KeyCode::Down => viewer.pan(0, VIEWER_PAN_STEP),
        _ => {}
    }
    true
}

pub(crate) fn handle_composer_image_preview_key(
    key: KeyEvent,
    state: &mut AppState,
    textarea: &TextArea,
) -> bool {
    if key.kind != KeyEventKind::Press
        || key.code != KeyCode::Enter
        || !key.modifiers.is_empty()
        || state.panel_mode != PanelMode::Conversation
    {
        return false;
    }
    let text = textarea_text(textarea);
    let cursor = textarea_cursor_byte_index(textarea);
    let Some(image) = state
        .composer_images
        .activatable_preview_at_cursor(&text, cursor)
    else {
        return false;
    };
    match ImageViewerState::open(image) {
        Ok(viewer) => state.image_viewer = Some(viewer),
        Err(error) => state.push_message(ChatMessage::Error(error)),
    }
    true
}

pub(crate) fn begin_image_paste(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    request: ImagePasteRequest,
) -> bool {
    let Some(request_id) = state.composer_images.begin_paste() else {
        state.push_message(ChatMessage::Error(
            "an image paste is already in progress".to_string(),
        ));
        return true;
    };
    state.slash_menu = None;
    state.mention.clear_projection();
    if action_tx
        .try_send(UserAction::PasteImages {
            request_id,
            request,
        })
        .is_err()
    {
        state.composer_images.fail_paste(request_id);
        state.push_message(ChatMessage::Error(
            "image attachment reader is unavailable".to_string(),
        ));
    }
    true
}

fn is_image_paste_key(key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press || key.code != KeyCode::Char('v') {
        return false;
    }
    if matches!(key.modifiers, KeyModifiers::CONTROL | KeyModifiers::SUPER) {
        return true;
    }
    cfg!(windows) && key.modifiers == KeyModifiers::ALT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard_image::ClipboardImagePayload;

    fn vision_state() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            orca_core::model::VISION_MODEL.to_string(),
            "/tmp".to_string(),
        );
        state.set_status(AppStatus::Idle);
        state
    }

    #[test]
    fn ctrl_and_super_v_request_images_but_plain_v_does_not() {
        let (tx, rx) = mpsc::unbounded();
        let mut state = vision_state();
        let ctrl = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert!(handle_image_paste_shortcut(ctrl, &mut state, &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::PasteImages {
                request: ImagePasteRequest::Clipboard,
                ..
            })
        ));

        let mut state = vision_state();
        let super_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER);
        assert!(handle_image_paste_shortcut(super_v, &mut state, &tx));
        assert!(!handle_image_paste_shortcut(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
            &mut state,
            &tx
        ));

        let mut state = vision_state();
        let alt_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT);
        assert_eq!(
            handle_image_paste_shortcut(alt_v, &mut state, &tx),
            cfg!(windows)
        );
    }

    #[test]
    fn non_vision_model_routes_clipboard_read_for_runtime_analysis() {
        let (tx, rx) = mpsc::unbounded();
        let mut state = vision_state();
        state.model_name = "deepseek-chat".to_string();

        assert!(handle_image_paste_shortcut(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            &mut state,
            &tx
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::PasteImages {
                request: ImagePasteRequest::Clipboard,
                ..
            })
        ));
        assert!(state.transcript.messages.is_empty());
    }

    #[test]
    fn enter_on_image_chip_opens_and_controls_the_viewer() {
        let mut state = vision_state();
        let request_id = state.composer_images.begin_paste().unwrap();
        let (insertion, _, _) = state
            .composer_images
            .complete_paste(
                request_id,
                "",
                0,
                vec![ClipboardImagePayload {
                    media_type: "image/png".to_string(),
                    data: {
                        use image::ImageEncoder as _;
                        let mut bytes = Vec::new();
                        image::codecs::png::PngEncoder::new(&mut bytes)
                            .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
                            .unwrap();
                        bytes
                    },
                    width: 1,
                    height: 1,
                    source_name: Some("pixel.png".to_string()),
                }],
            )
            .unwrap();
        let mut textarea = TextArea::from([insertion.as_str()]);
        textarea.move_cursor(tui_textarea::CursorMove::Jump(
            0,
            insertion.trim_end().chars().count() as u16,
        ));

        assert!(handle_composer_image_preview_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &textarea,
        ));
        assert!(state.image_viewer.is_some());
        assert!(handle_image_viewer_key(
            KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE),
            &mut state,
        ));
        assert!(handle_image_viewer_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
        ));
        assert!(state.image_viewer.is_none());
    }
}
