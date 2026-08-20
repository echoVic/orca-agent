mod live_thread;
mod local;
mod pagination;
mod projection;
#[cfg_attr(test, allow(dead_code))]
mod session_index;
mod types;
mod writer;

#[cfg(not(test))]
pub(crate) const ORCA_HOME_ENV: &str = "ORCA_HOME";

use orca_core::conversation::Conversation;

pub use live_thread::LiveThread;
#[cfg(test)]
pub(crate) use local::find_session_path;
pub(crate) use local::orca_home;
pub(crate) use local::sessions_dir;
pub use local::{
    JsonlThreadStore, SearchHit, SessionStore, archive_session, compress_session, delete_session,
    list_session_page, list_sessions, list_sessions_with_archived, load_session, rename_session,
    search_sessions,
};
pub(crate) use pagination::{page_thread_items, page_thread_turns};
pub(crate) use projection::{
    conversation_records_to_thread_items, conversation_records_to_thread_turns,
    message_to_thread_json, messages_to_thread_items, messages_to_thread_turns,
};
pub use session_index::SessionSummaryPage;
pub(crate) use types::{ManualCompactionDurableSnapshot, StoredConversationRecord};
pub use types::{
    SessionCheckpointRecord, SessionMeta, SessionSummary, SessionTranscript, SortDirection,
    StoredThreadItem, StoredThreadItemPage, StoredThreadProjection, StoredThreadSearchHit,
    StoredThreadSearchPage, StoredThreadSummary, StoredThreadSummaryPage, StoredThreadTurn,
    StoredThreadTurnPage, ThreadListFilters, ThreadMetadataPatch, ThreadRelationFilter,
    ThreadSortKey, ThreadStore, TurnItemsView,
};
pub use writer::SessionWriter;
pub(crate) use writer::{
    read_latest_context_tokens, read_manual_compaction_snapshot, read_prompt_queue_snapshot,
    redact_sensitive_text, truncate_transcript_at_boundary, write_prompt_queue_snapshot,
};

pub(crate) fn resume_conversation(
    transcript: &SessionTranscript,
    system_prompt: String,
) -> Conversation {
    crate::history::resume_conversation(transcript, system_prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history;
    use orca_core::approval_types::ActionKind;
    use orca_core::conversation::{MISSING_TOOL_TERMINAL_ERROR, Message, RawToolCall};
    use orca_core::thread_identity::{ConversationItemId, TurnId};
    use orca_core::tool_types::{ToolName, ToolRequest, ToolResult, ToolTerminalSource};

    #[test]
    fn jsonl_thread_store_is_the_named_storage_backend() {
        fn assert_thread_store<T: ThreadStore>(store: &T) {
            let _ = store;
        }

        let store = JsonlThreadStore::new();
        assert_thread_store(&store);

        let legacy: SessionStore = store;
        assert_thread_store(&legacy);
    }

    #[test]
    fn session_store_boundary_creates_loadable_jsonl_thread() {
        let cwd = tempfile::tempdir().unwrap();
        // Synchronous store work only; the redirect is a thread-local home
        // override (never the environment) and restores on panic. The old
        // remove_var here poisoned ORCA_HOME for every later test.
        history::with_redirected_orca_home("thread-store-boundary", |_home| {
            let store = SessionStore::new();
            let thread = store
                .create_live_thread(cwd.path(), "deepseek", Some("model-a".to_string()), "hello")
                .unwrap();
            let thread_id = thread.thread_id().to_string();
            drop(thread);

            let loaded = store.load_session(&thread_id).unwrap();
            assert_eq!(loaded.meta.session_id, thread_id);
            assert_eq!(loaded.meta.provider, "deepseek");
            assert_eq!(loaded.meta.model.as_deref(), Some("model-a"));
        });
    }

    #[test]
    fn thread_store_projects_conversation_turns() {
        let messages = vec![
            Message::System {
                content: "system".to_string(),
                pinned: false,
            },
            Message::User {
                content: "hello".to_string(),
                pinned: false,
            },
            Message::Assistant {
                content: Some("hi".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                pinned: false,
            },
        ];

        let turns =
            messages_to_thread_turns("thread-a", &messages, usize::MAX, TurnItemsView::Full);

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].thread_id, "thread-a");
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[0].items_view, TurnItemsView::Full);
        assert_eq!(turns[0].items.len(), 2);
    }

    #[test]
    fn thread_store_projects_messages_to_json_items() {
        let message = Message::User {
            content: "hello".to_string(),
            pinned: false,
        };

        let item = message_to_thread_json(&message);

        assert_eq!(item["role"], "user");
        assert_eq!(item["content"], "hello");
    }

    #[test]
    fn hybrid_projection_keeps_legacy_ids_and_uses_persisted_ids_for_new_records() {
        let typed_turn = TurnId::new();
        let typed_user = ConversationItemId::new();
        let typed_assistant = ConversationItemId::new();
        let mut records = vec![
            types::StoredConversationRecord::legacy(types::StoredMessage::from(&Message::user(
                "legacy user".to_string(),
            ))),
            types::StoredConversationRecord::legacy(types::StoredMessage::from(
                &Message::Assistant {
                    content: Some("legacy assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            )),
            types::StoredConversationRecord::identified(
                typed_user.clone(),
                typed_turn.clone(),
                types::StoredMessage::from(&Message::user("typed user".to_string())),
            ),
            types::StoredConversationRecord::identified(
                typed_assistant.clone(),
                typed_turn.clone(),
                types::StoredMessage::from(&Message::Assistant {
                    content: Some("typed assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                }),
            ),
        ];

        let initial_turns = projection::conversation_records_to_thread_turns(
            "thread-a",
            &records,
            usize::MAX,
            TurnItemsView::Full,
        )
        .expect("project hybrid turns");
        let initial_items = projection::conversation_records_to_thread_items(
            "thread-a",
            &records,
            None,
            usize::MAX,
        )
        .expect("project hybrid items");

        assert_eq!(initial_turns[0].turn_id, "turn-1");
        assert_eq!(initial_turns[1].turn_id, typed_turn.as_str());
        assert_eq!(initial_items[0].item_id, "item-1");
        assert_eq!(initial_items[1].item_id, "item-2");
        assert_eq!(initial_items[2].item_id, typed_user.as_str());
        assert_eq!(initial_items[3].item_id, typed_assistant.as_str());

        let later_turn = TurnId::new();
        records.push(types::StoredConversationRecord::identified(
            ConversationItemId::new(),
            later_turn,
            types::StoredMessage::from(&Message::user("later user".to_string())),
        ));
        let later_items = projection::conversation_records_to_thread_items(
            "thread-a",
            &records,
            None,
            usize::MAX,
        )
        .expect("project appended hybrid items");
        assert_eq!(
            later_items
                .iter()
                .take(initial_items.len())
                .map(|item| item.item_id.as_str())
                .collect::<Vec<_>>(),
            initial_items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn identified_projection_groups_steer_messages_and_rejects_reopened_turns() {
        let first_turn = TurnId::new();
        let second_turn = TurnId::new();
        let identified = |turn_id: &TurnId, content: &str| {
            types::StoredConversationRecord::identified(
                ConversationItemId::new(),
                turn_id.clone(),
                types::StoredMessage::from(&Message::user(content.to_string())),
            )
        };
        let records = vec![
            identified(&first_turn, "prompt"),
            identified(&first_turn, "steer"),
            identified(&second_turn, "next"),
        ];

        let turns = projection::conversation_records_to_thread_turns(
            "thread-a",
            &records,
            usize::MAX,
            TurnItemsView::Full,
        )
        .expect("project typed turns");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].items.len(), 2);

        let reopened = vec![
            identified(&first_turn, "first"),
            identified(&second_turn, "second"),
            identified(&first_turn, "reopened"),
        ];
        let error = projection::conversation_records_to_thread_turns(
            "thread-a",
            &reopened,
            usize::MAX,
            TurnItemsView::Full,
        )
        .expect_err("non-contiguous typed turn must fail closed");
        assert!(error.to_string().contains("is not contiguous"));
    }

    #[test]
    fn identified_tool_result_completes_the_domain_item_without_minting_a_wrapper() {
        let turn_id = TurnId::new();
        let request = ToolRequest {
            id: "call-stable".to_string(),
            name: ToolName::External("deploy".to_string()),
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(r#"{"env":"staging"}"#.to_string()),
        };
        let result = ToolResult::completed(&request, "deployed".to_string(), false);
        let records = vec![
            types::StoredConversationRecord::identified(
                ConversationItemId::new(),
                turn_id.clone(),
                types::StoredMessage::from(&Message::user("deploy".to_string())),
            ),
            types::StoredConversationRecord::identified(
                ConversationItemId::new(),
                turn_id.clone(),
                types::StoredMessage::from(&Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: request.id.clone(),
                        function_name: "deploy".to_string(),
                        arguments: request.raw_arguments.clone().unwrap(),
                    }],
                    pinned: false,
                }),
            ),
            types::StoredConversationRecord::identified(
                ConversationItemId::new(),
                turn_id,
                types::StoredMessage::from(&Message::Tool {
                    tool_call_id: request.id.clone(),
                    content: "deployed".to_string(),
                    terminal: Some(result.terminal().clone()),
                    pinned: false,
                }),
            ),
        ];

        let items = projection::conversation_records_to_thread_items(
            "thread-a",
            &records,
            None,
            usize::MAX,
        )
        .expect("project completed tool");

        assert_eq!(items.len(), 2);
        assert_eq!(items[1].item_id, request.id);
        assert_eq!(items[1].item["status"], "completed");
        assert_eq!(items[1].item["contentItems"][0]["text"], "deployed");
    }

    #[test]
    fn identified_tool_wrappers_keep_their_domain_item_ids() {
        for (tool_name, arguments, expected_type, expected_id_suffix) in [
            (
                "bash",
                serde_json::json!({ "command": "deploy" }),
                "commandExecution",
                "",
            ),
            (
                "mcp__ops__deploy",
                serde_json::json!({ "env": "production" }),
                "mcpToolCall",
                "",
            ),
            (
                "deploy",
                serde_json::json!({ "env": "production" }),
                "dynamicToolCall",
                "",
            ),
            (
                "write_file",
                serde_json::json!({ "path": "release.txt", "content": "ready" }),
                "fileChange",
                ":file-change",
            ),
        ] {
            let turn_id = TurnId::new();
            let request = ToolRequest {
                id: format!("{tool_name}-call"),
                name: ToolName::External(tool_name.to_string()),
                action: ActionKind::Write,
                target: None,
                raw_arguments: Some(arguments.to_string()),
            };
            let result = ToolResult::completed(&request, "done".to_string(), false);
            let records = vec![
                types::StoredConversationRecord::identified(
                    ConversationItemId::new(),
                    turn_id.clone(),
                    types::StoredMessage::from(&Message::user("run tool".to_string())),
                ),
                types::StoredConversationRecord::identified(
                    ConversationItemId::new(),
                    turn_id.clone(),
                    types::StoredMessage::from(&Message::Assistant {
                        content: None,
                        reasoning_content: None,
                        tool_calls: vec![RawToolCall {
                            id: request.id.clone(),
                            function_name: tool_name.to_string(),
                            arguments: arguments.to_string(),
                        }],
                        pinned: false,
                    }),
                ),
                types::StoredConversationRecord::identified(
                    ConversationItemId::new(),
                    turn_id,
                    types::StoredMessage::from(&Message::Tool {
                        tool_call_id: request.id.clone(),
                        content: "done".to_string(),
                        terminal: Some(result.terminal().clone()),
                        pinned: false,
                    }),
                ),
            ];

            let items = projection::conversation_records_to_thread_items(
                "thread-a",
                &records,
                None,
                usize::MAX,
            )
            .expect("project identified tool records");
            let expected_item_id = format!("{}{expected_id_suffix}", request.id);
            let item = items
                .iter()
                .find(|item| item.item["type"] == expected_type)
                .expect("projected domain tool item");

            assert_eq!(item.item_id, expected_item_id, "tool {tool_name}");
            assert_eq!(item.item["id"], expected_item_id, "tool {tool_name}");
        }
    }

    #[test]
    fn tool_terminal_metadata_projects_live_and_stored_items_identically() {
        let request = ToolRequest {
            id: "indeterminate-call".to_string(),
            name: ToolName::External("deploy".to_string()),
            action: ActionKind::Write,
            target: Some("production".to_string()),
            raw_arguments: Some(r#"{"env":"production"}"#.to_string()),
        };
        let result = ToolResult::indeterminate(&request, MISSING_TOOL_TERMINAL_ERROR)
            .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
        let messages = vec![
            Message::user("deploy production".to_string()),
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RawToolCall {
                    id: request.id.clone(),
                    function_name: "deploy".to_string(),
                    arguments: request.raw_arguments.clone().unwrap(),
                }],
                pinned: false,
            },
            Message::Tool {
                tool_call_id: request.id.clone(),
                content: format!("ERROR: {MISSING_TOOL_TERMINAL_ERROR}"),
                terminal: Some(result.terminal().clone()),
                pinned: false,
            },
        ];
        let stored_records = messages
            .iter()
            .map(types::StoredMessage::from)
            .map(types::StoredConversationRecord::legacy)
            .collect::<Vec<_>>();

        let live_items = messages_to_thread_items("thread-a", &messages, None, usize::MAX);
        let stored_items = projection::conversation_records_to_thread_items(
            "thread-a",
            &stored_records,
            None,
            usize::MAX,
        )
        .expect("project stored records");

        assert_eq!(live_items, stored_items);
        let item = &stored_items
            .iter()
            .find(|item| item.item["id"] == request.id)
            .expect("projected tool item")
            .item;
        assert_eq!(item["status"], "indeterminate");
        assert_eq!(item["terminalSource"], "compatibility_repair");
        assert!(item.get("invocationStarted").is_none());
        assert_eq!(item["kind"], "indeterminate");
        assert_eq!(item["error"]["message"], MISSING_TOOL_TERMINAL_ERROR);
    }

    #[test]
    fn tool_terminal_metadata_survives_every_persisted_tool_item_shape() {
        for (tool_name, arguments, expected_type) in [
            (
                "bash",
                serde_json::json!({ "command": "deploy" }),
                "commandExecution",
            ),
            (
                "mcp__ops__deploy",
                serde_json::json!({ "env": "production" }),
                "mcpToolCall",
            ),
            (
                "deploy",
                serde_json::json!({ "env": "production" }),
                "dynamicToolCall",
            ),
            (
                "write_file",
                serde_json::json!({ "path": "release.txt", "content": "ready" }),
                "fileChange",
            ),
        ] {
            let request = ToolRequest {
                id: format!("{tool_name}-call"),
                name: ToolName::External(tool_name.to_string()),
                action: ActionKind::Write,
                target: None,
                raw_arguments: Some(arguments.to_string()),
            };
            let result = ToolResult::indeterminate(&request, MISSING_TOOL_TERMINAL_ERROR)
                .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
            let messages = vec![
                Message::user("run tool".to_string()),
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: request.id.clone(),
                        function_name: tool_name.to_string(),
                        arguments: arguments.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: request.id.clone(),
                    content: format!("ERROR: {MISSING_TOOL_TERMINAL_ERROR}"),
                    terminal: Some(result.terminal().clone()),
                    pinned: false,
                },
            ];
            let stored_records = messages
                .iter()
                .map(types::StoredMessage::from)
                .map(types::StoredConversationRecord::legacy)
                .collect::<Vec<_>>();
            let live_items = messages_to_thread_items("thread-a", &messages, None, usize::MAX);
            let stored_items = projection::conversation_records_to_thread_items(
                "thread-a",
                &stored_records,
                None,
                usize::MAX,
            )
            .expect("project stored records");
            assert_eq!(live_items, stored_items, "tool {tool_name}");

            let item = stored_items
                .into_iter()
                .find(|item| {
                    item.item["id"] == request.id
                        || item.item["id"] == format!("{}:file-change", request.id)
                })
                .expect("projected tool item")
                .item;
            assert_eq!(item["type"], expected_type, "tool {tool_name}");
            assert_eq!(item["status"], "indeterminate", "tool {tool_name}");
            assert_eq!(
                item["terminalSource"], "compatibility_repair",
                "tool {tool_name}"
            );
            assert_eq!(item["kind"], "indeterminate", "tool {tool_name}");
            assert_eq!(
                item["error"]["message"], MISSING_TOOL_TERMINAL_ERROR,
                "tool {tool_name}"
            );
        }
    }

    #[test]
    fn stored_tool_terminal_rejects_conflicting_status_and_kind() {
        let conflicting = serde_json::json!({
            "role": "tool",
            "tool_call_id": "call-1",
            "content": "cancelled",
            "status": "cancelled",
            "kind": "runtime_error"
        });

        assert!(serde_json::from_value::<types::StoredMessage>(conflicting).is_err());
    }
}
