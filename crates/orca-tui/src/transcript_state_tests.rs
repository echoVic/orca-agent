//! Transcript owner tests: search, revisions, streaming assembly, and flush boundaries.

use crossbeam_channel as mpsc;

use crate::protocol::TuiEvent;
use crate::transcript_state::{ChatMessage, TranscriptState};
use crate::transcript_view::TranscriptRenderContext;
use crate::types::AppState;

fn state() -> AppState {
    let (tx, _rx) = mpsc::unbounded();
    AppState::new(
        tx,
        "0.0.0-test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    )
}

fn prepare_transcript_cache(state: &mut AppState, width: usize) {
    let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
    let messages = &state.transcript.messages;
    let revisions = &state.transcript.message_revisions;
    state.transcript.render_cache.prepare(
        messages,
        revisions,
        TranscriptRenderContext::new(&theme, width, 0, false),
        |_, message, theme, width, tick, force_expand| {
            crate::ui::build_lines_for_messages(
                std::slice::from_ref(message),
                theme,
                width,
                tick,
                force_expand,
            )
        },
    );
}

fn assistant_projection_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::Assistant(text) | ChatMessage::AssistantChunk { text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn revisions_start_at_one_and_stream_tail_is_empty() {
    let state = TranscriptState::new();
    assert_eq!(state.next_message_revision, 1);
    assert!(state.messages.is_empty());
    assert!(state.message_revisions.is_empty());
    assert_eq!(state.finalized_count, 0);
    assert_eq!(state.flushed_count, 0);
    assert!(state.tool_call_indices.is_empty());
    assert!(state.assistant_stream_tail.is_none());
}

#[test]
fn opening_search_preserves_scroll_and_refresh_selects_viewport_match() {
    let mut state = state();
    state.push_message(ChatMessage::Assistant(
        "first hit\nsecond\nthird hit".to_string(),
    ));
    prepare_transcript_cache(&mut state, 20);
    state.viewport.scroll_offset = 1;
    state.viewport.viewport_base_row = 1;
    state.open_transcript_search();
    state.replace_transcript_search_query("hit");
    state.refresh_transcript_search();

    assert!(state.transcript.search.open);
    assert_eq!(state.viewport.scroll_offset, 1);
    assert_eq!(
        state
            .transcript
            .search
            .active_match()
            .map(|found| found.start.row),
        Some(2)
    );
}

#[test]
fn explicit_search_jump_disables_follow_and_reveals_match() {
    let mut state = state();
    for index in 0..30 {
        state.push_message(ChatMessage::System(format!("line {index} target")));
    }
    prepare_transcript_cache(&mut state, 80);
    state.viewport.visible_height = 5;
    state.viewport.scroll_offset = 20;
    state.viewport.auto_scroll = true;
    state.open_transcript_search();
    state.replace_transcript_search_query("target");
    state.refresh_transcript_search();

    state.search_next();

    assert!(!state.viewport.auto_scroll);
    let active = state.transcript.search.active_match().unwrap();
    assert!(active.start.row >= state.viewport.scroll_offset);
    assert!(active.start.row < state.viewport.scroll_offset + state.viewport.visible_height);
}

#[test]
fn clear_resets_search_but_truncate_reconciles_lazily() {
    let mut state = state();
    state.open_transcript_search();
    state.replace_transcript_search_query("x");
    state.push_message(ChatMessage::System("x".to_string()));
    prepare_transcript_cache(&mut state, 40);
    state.refresh_transcript_search();
    assert_eq!(state.transcript.search.match_count(), 1);

    state.truncate_messages(0);
    state.refresh_transcript_search();
    assert_eq!(state.transcript.search.match_count(), 0);
    assert_eq!(state.transcript.search.query(), "x");

    state.clear_messages();
    assert!(!state.transcript.search.open);
    assert_eq!(state.transcript.search.query(), "");
}

#[test]
fn append_and_retain_preserve_active_revision_identity() {
    let mut state = state();
    state.push_message(ChatMessage::System("remove".to_string()));
    state.push_message(ChatMessage::System("target".to_string()));
    prepare_transcript_cache(&mut state, 40);
    state.open_transcript_search();
    state.replace_transcript_search_query("target");
    state.refresh_transcript_search();
    let identity = state
        .transcript
        .search
        .active_match()
        .unwrap()
        .line_identity;

    state.push_message(ChatMessage::System("later target".to_string()));
    prepare_transcript_cache(&mut state, 40);
    state.refresh_transcript_search();
    assert_eq!(
        state
            .transcript
            .search
            .active_match()
            .unwrap()
            .line_identity,
        identity
    );

    state.retain_messages(
        |message| !matches!(message, ChatMessage::System(text) if text == "remove"),
    );
    prepare_transcript_cache(&mut state, 40);
    state.refresh_transcript_search();
    assert_eq!(
        state
            .transcript
            .search
            .active_match()
            .unwrap()
            .line_identity,
        identity
    );
}

#[test]
fn one_append_rebuilds_one_message_then_rescans_without_render_rebuilds() {
    let mut state = state();
    for index in 0..1_000 {
        state.push_message(ChatMessage::System(format!("item {index} needle")));
    }
    prepare_transcript_cache(&mut state, 80);
    state.open_transcript_search();
    state.replace_transcript_search_query("needle");
    state.refresh_transcript_search();
    let scans = state.transcript.search.scan_count_for_test();

    state.push_message(ChatMessage::System("last needle".to_string()));
    prepare_transcript_cache(&mut state, 80);
    assert_eq!(state.transcript.render_cache.last_prepare_visited(), 1);
    let render_generation = state.transcript.render_cache.content_generation();
    state.refresh_transcript_search();
    assert_eq!(state.transcript.search.scan_count_for_test(), scans + 1);
    assert_eq!(
        state.transcript.render_cache.content_generation(),
        render_generation
    );
    assert_eq!(state.transcript.render_cache.last_prepare_visited(), 1);
}

#[test]
fn removal_chooses_nearest_following_match_and_open_does_not_disable_follow() {
    let mut state = state();
    for text in ["target one", "middle", "target two"] {
        state.push_message(ChatMessage::System(text.to_string()));
    }
    prepare_transcript_cache(&mut state, 40);
    state.viewport.auto_scroll = true;
    state.open_transcript_search();
    assert!(state.viewport.auto_scroll);
    state.replace_transcript_search_query("target");
    state.refresh_transcript_search();
    let first_revision = state
        .transcript
        .search
        .active_match()
        .unwrap()
        .line_identity;

    state.retain_messages(
        |message| !matches!(message, ChatMessage::System(text) if text == "target one"),
    );
    prepare_transcript_cache(&mut state, 40);
    state.refresh_transcript_search();
    assert_ne!(
        state
            .transcript
            .search
            .active_match()
            .unwrap()
            .line_identity,
        first_revision
    );
    assert_eq!(state.transcript.search.match_count(), 1);
}

#[test]
fn nth_final_assistant_response_ignores_streaming_chunks() {
    let mut state = state();
    state.push_message(ChatMessage::Assistant("older".to_string()));
    state.push_message(ChatMessage::AssistantChunk {
        text: "unfinished".to_string(),
        trailing_blank: false,
    });
    state.push_message(ChatMessage::Assistant("latest".to_string()));

    assert_eq!(state.nth_final_assistant_response(1), Some("latest"));
    assert_eq!(state.nth_final_assistant_response(2), Some("older"));
    assert_eq!(state.nth_final_assistant_response(0), None);
    assert_eq!(state.nth_final_assistant_response(3), None);
}

#[test]
fn replacing_messages_resets_tracking_after_same_length_replacement() {
    let mut state = state();
    state.push_message(ChatMessage::Assistant("old session".to_string()));
    let old_revision = state.transcript.message_revisions[0];

    state.replace_messages([ChatMessage::Assistant("new session".to_string())]);

    assert_ne!(state.transcript.message_revisions[0], old_revision);
    assert_eq!(
        state.transcript.render_cache.len(),
        state.transcript.messages.len()
    );
}

#[test]
fn retaining_messages_rebases_watermarks_and_cache_entries() {
    use std::cell::RefCell;

    let mut state = state();
    state.push_message(ChatMessage::User("keep before".to_string()));
    state.push_message(ChatMessage::System("remove before".to_string()));
    state.push_message(ChatMessage::Assistant("keep after".to_string()));
    state.transcript.finalized_count = 3;
    state.transcript.flushed_count = 2;
    let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
    state.transcript.render_cache.prepare(
        &state.transcript.messages,
        &state.transcript.message_revisions,
        TranscriptRenderContext::new(&theme, 40, 0, false),
        |_, message, _, _, _, _| vec![ratatui::text::Line::from(format!("{message:?}"))],
    );
    assert_eq!(state.transcript.render_cache.populated_len(), 3);

    state.retain_messages(
        |message| !matches!(message, ChatMessage::System(text) if text == "remove before"),
    );

    assert_eq!(state.transcript.messages.len(), 2);
    assert_eq!(state.transcript.message_revisions.len(), 2);
    assert_eq!(state.transcript.finalized_count, 2);
    assert_eq!(state.transcript.flushed_count, 1);
    assert_eq!(state.transcript.render_cache.len(), 2);
    assert_eq!(state.transcript.render_cache.populated_len(), 2);

    state.touch_message(1);
    let built_indices = RefCell::new(Vec::new());
    state.transcript.render_cache.prepare(
        &state.transcript.messages,
        &state.transcript.message_revisions,
        TranscriptRenderContext::new(&theme, 40, 0, false),
        |index, message, _, _, _, _| {
            built_indices.borrow_mut().push(index);
            vec![ratatui::text::Line::from(format!("{message:?}"))]
        },
    );

    assert_eq!(*built_indices.borrow(), vec![1]);
    assert_eq!(state.transcript.render_cache.last_prepare_visited(), 1);
}

#[test]
fn complete_lines_mutate_only_the_active_assistant_tail_revision() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("first line\n".to_string()));
    let first_revision = state.transcript.message_revisions[0];
    state.update(TuiEvent::MessageDelta("second line\n".to_string()));
    assert_eq!(state.transcript.messages.len(), 1);
    assert_ne!(state.transcript.message_revisions[0], first_revision);

    let revisions = state.transcript.message_revisions.clone();
    state.update(TuiEvent::MessageDelta("hidden half".to_string()));
    assert_eq!(state.transcript.message_revisions, revisions);
}

#[test]
fn blank_boundary_freezes_tail_revision_and_new_block_uses_new_tail() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("first\n\n".to_string()));
    assert!(matches!(
        &state.transcript.messages[..],
        [ChatMessage::AssistantChunk {
            text,
            trailing_blank: true,
        }] if text == "first\n\n"
    ));
    let frozen_revision = state.transcript.message_revisions[0];

    state.update(TuiEvent::MessageDelta("second\n".to_string()));
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::Assistant(text)) if text == "second\n"
    ));
    assert_eq!(state.transcript.message_revisions[0], frozen_revision);
}

#[test]
fn reconcile_assistant_response_replaces_frozen_chunks_and_open_tail() {
    let mut state = state();
    state.push_message(ChatMessage::User("prompt".to_string()));
    state.update(TuiEvent::MessageDelta("first paragraph\n\n".to_string()));
    state.update(TuiEvent::MessageDelta("second paragraph".to_string()));
    state.update(TuiEvent::ReasoningDelta("streamed thinking".to_string()));
    assert!(matches!(
        state.transcript.messages.as_slice(),
        [
            ChatMessage::User(_),
            ChatMessage::AssistantChunk { .. },
            ChatMessage::Assistant(_),
            ChatMessage::Reasoning(_),
        ]
    ));

    state.update(TuiEvent::AssistantResponseCompleted(
        Some("full answer\n\n".to_string()),
        Some("full reasoning".to_string()),
    ));

    // Frozen chunks and the open tail are both replaced by the completed
    // response instead of being left to duplicate it.
    assert!(matches!(
        state.transcript.messages.as_slice(),
        [
            ChatMessage::User(_),
            ChatMessage::Reasoning(reasoning),
            ChatMessage::AssistantChunk {
                text,
                trailing_blank: true,
            },
        ] if reasoning == "full reasoning" && text == "full answer\n\n"
    ));
}

#[test]
fn reconcile_assistant_response_drops_pending_partial_line() {
    let mut state = state();
    state.push_message(ChatMessage::User("prompt".to_string()));
    // The stream ends mid-line: the assembler holds "stale tail" without any
    // newline, so it has not been rendered as a message yet.
    state.update(TuiEvent::MessageDelta("first paragraph\n\n".to_string()));
    state.update(TuiEvent::MessageDelta("stale tail".to_string()));
    assert!(matches!(
        state.transcript.messages.as_slice(),
        [ChatMessage::User(_), ChatMessage::AssistantChunk { .. }]
    ));

    state.update(TuiEvent::AssistantResponseCompleted(
        Some("full answer\n\n".to_string()),
        Some("full reasoning".to_string()),
    ));

    // The held partial line must not bleed into the completed response.
    assert!(matches!(
        state.transcript.messages.as_slice(),
        [
            ChatMessage::User(_),
            ChatMessage::Reasoning(reasoning),
            ChatMessage::AssistantChunk { text, .. },
        ] if reasoning == "full reasoning" && text == "full answer\n\n"
    ));
}

#[test]
fn assistant_stream_completion_flushes_partial_unicode_once() {
    let mut state = state();
    for delta in ["中", "文👍🏽e\u{301}", "\n尾", "行"] {
        state.update(TuiEvent::MessageDelta(delta.to_string()));
    }
    assert_eq!(
        assistant_projection_text(&state.transcript.messages),
        "中文👍🏽e\u{301}\n"
    );

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert_eq!(
        assistant_projection_text(&state.transcript.messages),
        "中文👍🏽e\u{301}\n尾行"
    );
    let revisions = state.transcript.message_revisions.clone();

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });
    assert_eq!(
        assistant_projection_text(&state.transcript.messages),
        "中文👍🏽e\u{301}\n尾行"
    );
    assert_eq!(state.transcript.message_revisions, revisions);
}

#[test]
fn proposed_plan_boundaries_preserve_agent_source_order() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("Intro\n<proposed".to_string()));
    state.update(TuiEvent::MessageDelta(
        "_plan>\n# Plan\n- inspect\n</proposed_plan>\nOutro".to_string(),
    ));
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    let plan_index = state
        .transcript
        .messages
        .iter()
        .position(|message| matches!(message, ChatMessage::ProposedPlan(_)))
        .expect("proposed plan message");
    assert_eq!(
        assistant_projection_text(&state.transcript.messages[..plan_index]),
        "Intro\n"
    );
    assert_eq!(
        assistant_projection_text(&state.transcript.messages[plan_index + 1..]),
        "\nOutro"
    );
    assert!(matches!(
        &state.transcript.messages[plan_index],
        ChatMessage::ProposedPlan(text) if text == "# Plan\n- inspect\n"
    ));
}

#[test]
fn tool_boundary_finishes_hidden_assistant_text_before_tool_row() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("hidden tail".to_string()));
    state.update(TuiEvent::ToolRequested {
        id: "tool-1".to_string(),
        name: "grep".to_string(),
        target: None,
    });

    assert!(matches!(
        &state.transcript.messages[..],
        [
            ChatMessage::Assistant(text),
            ChatMessage::ToolCall { id, .. }
        ] if text == "hidden tail" && id == "tool-1"
    ));
}

#[test]
fn transcript_reset_discards_hidden_assistant_text() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("discard me".to_string()));
    assert!(state.transcript.messages.is_empty());

    state.clear_messages();
    state.update(TuiEvent::MessageDelta("visible\n".to_string()));
    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    assert_eq!(
        assistant_projection_text(&state.transcript.messages),
        "visible\n"
    );
}

#[test]
fn discarded_attempt_removes_only_trailing_assistant_output() {
    let mut state = state();
    state.push_message(ChatMessage::User("prompt".to_string()));
    state.push_message(ChatMessage::ToolCall {
        id: "tool-1".to_string(),
        name: "read_file".to_string(),
        target: None,
        status: "completed".to_string(),
        output: Some("done".to_string()),
        diff: None,
        expanded: false,
        kind: None,
    });
    state.update(TuiEvent::ReasoningDelta("partial reasoning".to_string()));
    state.update(TuiEvent::MessageDelta("partial answer\n".to_string()));

    state.update(TuiEvent::AssistantAttemptDiscarded);

    assert!(matches!(state.transcript.messages[0], ChatMessage::User(_)));
    assert!(matches!(
        state.transcript.messages[1],
        ChatMessage::ToolCall { .. }
    ));
    assert_eq!(state.transcript.messages.len(), 2);
    state.update(TuiEvent::MessageDelta("recovered\n".to_string()));
    assert_eq!(
        assistant_projection_text(&state.transcript.messages),
        "recovered\n"
    );
}

#[test]
fn retaining_messages_reindexes_the_active_assistant_tail() {
    let mut state = state();
    state.push_message(ChatMessage::System("remove".to_string()));
    state.update(TuiEvent::MessageDelta("first\n".to_string()));
    state.retain_messages(
        |message| !matches!(message, ChatMessage::System(text) if text == "remove"),
    );

    state.update(TuiEvent::MessageDelta("second\n".to_string()));

    assert_eq!(state.transcript.messages.len(), 1);
    assert!(matches!(
        state.transcript.messages.last(),
        Some(ChatMessage::Assistant(text)) if text == "first\nsecond\n"
    ));
}

#[test]
fn system_notice_finishes_hidden_assistant_text_before_notice() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("hidden tail".to_string()));
    state.update(TuiEvent::Notice("notice".to_string()));

    assert!(matches!(
        &state.transcript.messages[..],
        [ChatMessage::Assistant(text), ChatMessage::System(notice)]
            if text == "hidden tail" && notice == "notice"
    ));
}

#[test]
fn session_completion_without_receiving_tools_preserves_populated_render_cache() {
    let mut state = state();
    state.push_message(ChatMessage::Assistant("stable markdown".to_string()));
    let theme = crate::theme::Theme::named(orca_core::config::ThemeName::Dark);
    state.transcript.render_cache.prepare(
        &state.transcript.messages,
        &state.transcript.message_revisions,
        TranscriptRenderContext::new(&theme, 40, 0, false),
        |_, message, _, _, _, _| match message {
            ChatMessage::Assistant(text) => vec![ratatui::text::Line::from(text.clone())],
            _ => unreachable!(),
        },
    );
    assert_eq!(state.transcript.render_cache.populated_len(), 1);

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    assert_eq!(state.transcript.render_cache.populated_len(), 1);
}

#[test]
fn flushable_prefix_stops_at_a_running_tool_call() {
    let mut state = state();
    state
        .transcript
        .messages
        .push(ChatMessage::User("hi".to_string()));
    state.update(TuiEvent::ToolRequested {
        id: "t1".to_string(),
        name: "grep".to_string(),
        target: Some("a".to_string()),
    });
    // User is settled, the running tool blocks everything after it.
    assert_eq!(state.flushable_prefix_end(false), 1);

    state.update(TuiEvent::ToolCompleted {
        id: "t1".to_string(),
        name: "grep".to_string(),
        status: "completed".to_string(),
        output: "hit".to_string(),
        diff: None,
        kind: Some("success".to_string()),
    });
    // Now the completed tool can flush too.
    assert_eq!(state.flushable_prefix_end(false), 2);
}

#[test]
fn flushable_prefix_stops_at_a_receiving_tool_call() {
    let mut state = state();
    state
        .transcript
        .messages
        .push(ChatMessage::User("hi".to_string()));
    state.update(TuiEvent::ToolCallProgress {
        id: "t1".to_string(),
        name: Some("write_file".to_string()),
        arguments_bytes: 1024,
    });

    assert_eq!(state.flushable_prefix_end(false), 1);
}

#[test]
fn flushable_prefix_excludes_hidden_partial_until_completion_flushes_it() {
    let mut state = state();
    state
        .transcript
        .messages
        .push(ChatMessage::User("hi".to_string()));
    state.update(TuiEvent::MessageDelta("partial".to_string()));

    // The partial source line is still hidden, so only the user prompt exists.
    assert_eq!(state.flushable_prefix_end(false), 1);
    assert_eq!(state.flushable_prefix_end(true), 1);

    state.update(TuiEvent::SessionCompleted {
        status: "success".to_string(),
    });

    // Completion flushes the hidden line as a finalized assistant message.
    assert_eq!(state.flushable_prefix_end(true), 2);
}

#[test]
fn flushable_prefix_releases_an_assistant_block_once_a_newer_message_follows() {
    let mut state = state();
    state.update(TuiEvent::MessageDelta("first answer".to_string()));
    // While it is the last message it is still mutable.
    assert_eq!(state.flushable_prefix_end(false), 0);

    // A following tool call means the assistant block will never grow again.
    state.update(TuiEvent::ToolRequested {
        id: "t1".to_string(),
        name: "grep".to_string(),
        target: None,
    });
    state.update(TuiEvent::ToolCompleted {
        id: "t1".to_string(),
        name: "grep".to_string(),
        status: "completed".to_string(),
        output: "out".to_string(),
        diff: None,
        kind: None,
    });
    assert_eq!(state.flushable_prefix_end(false), 2);
}

#[test]
fn flushable_prefix_is_bounded_by_already_flushed_count() {
    let mut state = state();
    state
        .transcript
        .messages
        .push(ChatMessage::User("a".to_string()));
    state
        .transcript
        .messages
        .push(ChatMessage::System("b".to_string()));
    state.transcript.flushed_count = 1;
    // Counts the contiguous settled run starting from flushed_count, not from 0.
    assert_eq!(state.flushable_prefix_end(false), 2);

    state.transcript.flushed_count = 2;
    assert_eq!(state.flushable_prefix_end(false), 2);
}
