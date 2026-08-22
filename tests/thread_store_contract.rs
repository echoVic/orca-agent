use orca_core::approval_rules::{PermissionRule, PermissionRules};
use orca_core::approval_types::{ActionKind, ApprovalMode, Decision};
use orca_core::config::ActivePermissionProfile;
use orca_core::conversation::{MISSING_TOOL_TERMINAL_ERROR, Message, RawToolCall};
use orca_core::thread_identity::TurnId;
use orca_core::tool_types::{
    ToolInvocationStarted, ToolName, ToolRequest, ToolResult, ToolResultKind, ToolStatus,
    ToolTerminalSource,
};
use orca_runtime::history::{
    LiveThread, SessionStore, SortDirection, ThreadListFilters, ThreadMetadataPatch,
    ThreadRelationFilter, ThreadSortKey, ThreadStore, TurnItemsView,
};
use tempfile::tempdir;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn session_store_thread_store_appends_live_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(
                home,
                "mock",
                Some("deepseek-v4-flash".to_string()),
                "thread store prompt",
            )
            .expect("create live thread");

        assert!(!thread.thread_id().is_empty());

        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "thread store prompt".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("thread store response".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append thread items");
        thread.complete("success").expect("complete thread");

        let transcript = store
            .load_session(thread.thread_id())
            .expect("thread id loads transcript");
        assert_eq!(transcript.meta.session_id, thread.thread_id());
        assert_eq!(transcript.meta.title, "thread store prompt");
        assert!(transcript.messages.iter().any(|message| {
            matches!(message, Message::User { content, .. } if content == "thread store prompt")
        }));
        assert!(transcript.messages.iter().any(|message| {
            matches!(message, Message::Assistant { content: Some(content), .. } if content == "thread store response")
        }));
    });
}

#[test]
fn session_store_persists_thread_permission_profile() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread_with_permissions(
                home,
                "mock",
                Some("deepseek-v4-flash".to_string()),
                "permission profile prompt",
                Some(ActivePermissionProfile::new(
                    "locked-down",
                    Some(":workspace"),
                )),
                ApprovalMode::Plan,
                PermissionRules {
                    rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
                },
                Vec::new(),
            )
            .expect("create live thread with permissions");
        let thread_id = thread.thread_id().to_string();
        thread.complete("success").expect("complete thread");

        let transcript = store.load_session(&thread_id).expect("load thread");
        assert_eq!(
            transcript.meta.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "locked-down",
                Some(":workspace")
            ))
        );
        assert_eq!(transcript.meta.approval_mode, Some(ApprovalMode::Plan));
        assert_eq!(transcript.meta.permission_rules.rules.len(), 1);
        assert_eq!(transcript.meta.permission_rules.rules[0].tool, "bash");

        let listed = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("list threads");
        assert_eq!(listed.data[0].approval_mode, Some(ApprovalMode::Plan));
        assert_eq!(
            listed.data[0].active_permission_profile,
            Some(ActivePermissionProfile::new(
                "locked-down",
                Some(":workspace")
            ))
        );
        assert_eq!(listed.data[0].permission_rule_count, 1);
    });
}

#[test]
fn session_store_thread_store_updates_metadata_by_thread_id() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "old title")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread.complete("success").expect("complete thread");

        store
            .update_thread_metadata(
                &thread_id,
                ThreadMetadataPatch {
                    title: Some("new title".to_string()),
                    ..ThreadMetadataPatch::default()
                },
            )
            .expect("update metadata");

        let transcript = store.load_session(&thread_id).expect("load updated thread");
        assert_eq!(transcript.meta.title, "new title");
        let summary = store
            .list_sessions(1)
            .expect("list sessions")
            .into_iter()
            .find(|summary| summary.session_id == thread_id)
            .expect("thread summary");
        assert_eq!(summary.title, "new title");
    });
}

#[test]
fn bounded_session_page_does_not_materialize_all_transcripts() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let sessions = home.join("sessions").join("bulk");
        std::fs::create_dir_all(&sessions).expect("create bulk session directory");
        for index in 0..2_000 {
            let meta = store.create_meta(
                home,
                "mock",
                None,
                &format!("metadata-only session {index}"),
            );
            let mut wire = serde_json::to_value(meta)
                .expect("serialize session metadata")
                .as_object()
                .cloned()
                .expect("session metadata is an object");
            wire.insert(
                "type".to_string(),
                serde_json::Value::String("session.meta".to_string()),
            );
            std::fs::write(
                sessions.join(format!("session-{index:04}.jsonl")),
                format!(
                    "{}\n{{this transcript body is intentionally invalid\n",
                    serde_json::Value::Object(wire)
                ),
            )
            .expect("write metadata-only session fixture");
        }

        let page = store
            .list_threads(
                None,
                25,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("bounded metadata-only session page");

        assert_eq!(page.data.len(), 25);
        assert!(page.next_cursor.is_some());
    });
}

#[test]
fn session_store_thread_store_updates_permission_metadata_by_thread_id() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread_with_permissions(
                home,
                "mock",
                None,
                "old permissions",
                None,
                ApprovalMode::Plan,
                PermissionRules {
                    rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
                },
                Vec::new(),
            )
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        thread.complete("success").expect("complete thread");

        store
            .update_thread_metadata(
                &thread_id,
                ThreadMetadataPatch {
                    active_permission_profile: Some(ActivePermissionProfile::new(
                        "workspace-plus",
                        Some(":workspace"),
                    )),
                    approval_mode: Some(ApprovalMode::AutoEdit),
                    permission_rules: Some(PermissionRules {
                        rules: vec![PermissionRule::new(
                            "bash",
                            "cargo test *",
                            Decision::Prompt,
                        )],
                    }),
                    ..ThreadMetadataPatch::default()
                },
            )
            .expect("update permission metadata");

        let transcript = store.load_session(&thread_id).expect("load updated thread");
        assert_eq!(
            transcript.meta.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "workspace-plus",
                Some(":workspace")
            ))
        );
        assert_eq!(transcript.meta.approval_mode, Some(ApprovalMode::AutoEdit));
        assert_eq!(
            transcript.meta.permission_rules.rules[0].pattern,
            "cargo test *"
        );
        assert_eq!(
            transcript.meta.permission_rules.rules[0].decision,
            Decision::Prompt
        );
        let summary = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("list threads")
            .data
            .into_iter()
            .find(|summary| summary.thread_id == thread_id)
            .expect("thread summary");
        assert_eq!(summary.approval_mode, Some(ApprovalMode::AutoEdit));
        assert_eq!(
            summary.active_permission_profile,
            Some(ActivePermissionProfile::new(
                "workspace-plus",
                Some(":workspace")
            ))
        );
        assert_eq!(summary.permission_rule_count, 1);
    });
}

#[test]
fn session_store_paginates_thread_summaries_and_search_hits() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut first = store
            .create_live_thread(home, "mock", None, "first paginated thread")
            .expect("create first thread");
        enter_test_turn(&mut first);
        first
            .append_items(&[Message::User {
                content: "shared search needle first".to_string(),
                images: Vec::new(),
                pinned: false,
            }])
            .expect("append first");
        let first_id = first.thread_id().to_string();
        first.complete("success").expect("complete first");
        std::thread::sleep(std::time::Duration::from_millis(5));

        let mut second = store
            .create_live_thread(home, "mock", None, "second paginated thread")
            .expect("create second thread");
        enter_test_turn(&mut second);
        second
            .append_items(&[Message::User {
                content: "shared search needle second".to_string(),
                images: Vec::new(),
                pinned: false,
            }])
            .expect("append second");
        let second_id = second.thread_id().to_string();
        second.complete("success").expect("complete second");

        let first_page = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("first list page");
        assert_eq!(first_page.data.len(), 1);
        let first_list_id = first_page.data[0].thread_id.clone();
        assert!(first_list_id == first_id || first_list_id == second_id);
        assert_eq!(first_page.next_cursor.as_deref(), Some("1"));

        let second_page = store
            .list_threads(
                first_page.next_cursor.as_deref(),
                1,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("second list page");
        assert_eq!(second_page.data.len(), 1);
        let second_list_id = second_page.data[0].thread_id.clone();
        assert!(second_list_id == first_id || second_list_id == second_id);
        assert_ne!(first_list_id, second_list_id);
        assert_eq!(second_page.next_cursor, None);
        assert_eq!(second_page.backwards_cursor.as_deref(), Some("1"));

        let asc_page = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("ascending list page");
        assert_eq!(asc_page.data.len(), 1);
        assert_eq!(asc_page.data[0].thread_id, first_id);

        let created_desc_page = store
            .list_threads(
                None,
                1,
                ThreadListFilters::active(),
                ThreadSortKey::CreatedAt,
                SortDirection::Desc,
                None,
            )
            .expect("created desc list page");
        assert_eq!(created_desc_page.data[0].thread_id, second_id);

        let filtered_page = store
            .list_threads(
                None,
                10,
                ThreadListFilters::active(),
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
                Some("second paginated"),
            )
            .expect("filtered list page");
        assert_eq!(filtered_page.data.len(), 1);
        assert_eq!(filtered_page.data[0].thread_id, second_id);
        assert_eq!(filtered_page.next_cursor, None);

        let search_page = store
            .search_threads(
                "shared search needle",
                None,
                1,
                false,
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
            )
            .expect("first search page");
        assert_eq!(search_page.data.len(), 1);
        let first_search_id = search_page.data[0].thread.thread_id.clone();
        assert!(first_search_id == first_id || first_search_id == second_id);
        assert_eq!(search_page.next_cursor.as_deref(), Some("1"));

        let search_page_2 = store
            .search_threads(
                "shared search needle",
                search_page.next_cursor.as_deref(),
                1,
                false,
                ThreadSortKey::UpdatedAt,
                SortDirection::Desc,
            )
            .expect("second search page");
        assert_eq!(search_page_2.data.len(), 1);
        let second_search_id = search_page_2.data[0].thread.thread_id.clone();
        assert!(second_search_id == first_id || second_search_id == second_id);
        assert_ne!(first_search_id, second_search_id);
        assert_eq!(search_page_2.next_cursor, None);
    });
}

#[test]
fn session_store_filters_thread_list_by_metadata_archival_and_relation() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let alpha_cwd = home.join("alpha");
        let beta_cwd = home.join("beta");
        std::fs::create_dir_all(&alpha_cwd).expect("alpha cwd");
        std::fs::create_dir_all(&beta_cwd).expect("beta cwd");

        let mut parent = store
            .create_live_thread(
                &alpha_cwd,
                "deepseek",
                Some("deepseek-v4-flash".to_string()),
                "parent relation thread",
            )
            .expect("create parent");
        let parent_id = parent.thread_id().to_string();
        parent.complete("success").expect("complete parent");

        let direct_child_meta = store.create_fork_meta(
            &alpha_cwd,
            "deepseek",
            Some("deepseek-reasoner".to_string()),
            "direct child relation thread",
            parent_id.clone(),
        );
        let direct_child_id = direct_child_meta.session_id.clone();
        let mut direct_child = store
            .start_writer_from_meta(direct_child_meta)
            .expect("direct child writer");
        direct_child
            .complete("success")
            .expect("complete direct child");

        let grandchild_meta = store.create_fork_meta(
            &beta_cwd,
            "openai",
            Some("gpt-5".to_string()),
            "grandchild relation thread",
            direct_child_id.clone(),
        );
        let grandchild_id = grandchild_meta.session_id.clone();
        let mut grandchild = store
            .start_writer_from_meta(grandchild_meta)
            .expect("grandchild writer");
        grandchild.complete("success").expect("complete grandchild");

        let archived_meta = store.create_meta(
            &beta_cwd,
            "deepseek",
            Some("deepseek-v4-flash".to_string()),
            "archived beta thread",
        );
        let archived_id = archived_meta.session_id.clone();
        let mut archived = store
            .start_writer_from_meta(archived_meta)
            .expect("archived writer");
        archived.complete("success").expect("complete archived");
        store
            .archive_session(&archived_id)
            .expect("archive beta thread");

        let alpha_only = store
            .list_threads(
                None,
                10,
                ThreadListFilters {
                    cwd_filters: vec![alpha_cwd.display().to_string()],
                    ..ThreadListFilters::active()
                },
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("alpha cwd list");
        assert_eq!(
            alpha_only
                .data
                .iter()
                .map(|thread| thread.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec![parent_id.as_str(), direct_child_id.as_str()]
        );

        let deepseek_flash = store
            .list_threads(
                None,
                10,
                ThreadListFilters {
                    model_providers: Some(vec!["deepseek".to_string()]),
                    model_names: Some(vec!["deepseek-v4-flash".to_string()]),
                    ..ThreadListFilters::active()
                },
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("deepseek flash list");
        assert_eq!(deepseek_flash.data.len(), 1);
        assert_eq!(deepseek_flash.data[0].thread_id, parent_id);

        let archived_only = store
            .list_threads(
                None,
                10,
                ThreadListFilters::archived(),
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("archived list");
        assert_eq!(archived_only.data.len(), 1);
        assert_eq!(archived_only.data[0].thread_id, archived_id);
        assert!(archived_only.data[0].archived);

        let direct_children = store
            .list_threads(
                None,
                10,
                ThreadListFilters {
                    relation: Some(ThreadRelationFilter::DirectChildrenOf(parent_id.clone())),
                    ..ThreadListFilters::active()
                },
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("direct children list");
        assert_eq!(direct_children.data.len(), 1);
        assert_eq!(direct_children.data[0].thread_id, direct_child_id);

        let descendants = store
            .list_threads(
                None,
                10,
                ThreadListFilters {
                    relation: Some(ThreadRelationFilter::DescendantsOf(parent_id)),
                    ..ThreadListFilters::active()
                },
                ThreadSortKey::CreatedAt,
                SortDirection::Asc,
                None,
            )
            .expect("descendants list");
        assert_eq!(
            descendants
                .data
                .iter()
                .map(|thread| thread.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec![direct_child_id.as_str(), grandchild_id.as_str()]
        );
    });
}

#[test]
fn session_store_projects_thread_turns_and_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        let turn_id = enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "turn projection user".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("turn projection assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append projection items");
        thread.complete("success").expect("complete thread");

        let turns = store
            .list_thread_turns(
                &thread_id,
                None,
                10,
                SortDirection::Asc,
                TurnItemsView::Full,
            )
            .expect("list thread turns");
        assert_eq!(turns.data.len(), 1);
        assert_eq!(turns.data[0].items_view, TurnItemsView::Full);
        assert_eq!(turns.data[0].turn_id, turn_id.as_str());
        assert_eq!(turns.data[0].index, 0);
        assert_eq!(turns.data[0].role, "user");
        assert_eq!(turns.data[0].items.len(), 2);
        assert_eq!(turns.data[0].items[0]["content"], "turn projection user");
        assert_eq!(
            turns.data[0].items[1]["content"],
            "turn projection assistant"
        );

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        assert_eq!(items.data.len(), 2);
        assert!(items.data[0].item_id.starts_with("item_"));
        assert_eq!(items.data[0].turn_id, turn_id.as_str());
        assert_eq!(items.data[0].item["content"], "turn projection user");
        assert!(items.data[1].item_id.starts_with("item_"));
        assert_ne!(items.data[0].item_id, items.data[1].item_id);
        assert_eq!(items.data[1].turn_id, turn_id.as_str());
        assert_eq!(items.data[1].item["content"], "turn projection assistant");

        let filtered = store
            .list_thread_items(
                &thread_id,
                Some(turn_id.as_str()),
                None,
                10,
                SortDirection::Asc,
            )
            .expect("list filtered thread items");
        assert_eq!(filtered.data.len(), 2);
        assert_eq!(filtered.data[1].turn_id, turn_id.as_str());
        assert_eq!(filtered.data[0].item_id, items.data[0].item_id);
        assert_eq!(filtered.data[1].item_id, items.data[1].item_id);
        assert_eq!(
            filtered.data[1].item["content"],
            "turn projection assistant"
        );
    });
}

#[test]
fn session_store_projects_mcp_tool_calls_as_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "mcp projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "call mcp search".to_string(),
                images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "mcp-call-1".to_string(),
                        function_name: "mcp__local__search".to_string(),
                        arguments: r#"{"query":"orca"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "mcp-call-1".to_string(),
                    content: r#"{"content":[{"type":"text","text":"found"}],"structuredContent":{"count":1},"_meta":{"source":"test"}}"#.to_string(),
                    terminal: None,
                    pinned: false,
                },
            ])
            .expect("append mcp projection items");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let mcp_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "mcp-call-1")
            .expect("projected mcp item");
        assert_eq!(mcp_item.item["type"], "mcpToolCall");
        assert_eq!(mcp_item.item["server"], "local");
        assert_eq!(mcp_item.item["tool"], "search");
        assert_eq!(mcp_item.item["status"], "completed");
        assert_eq!(mcp_item.item["arguments"]["query"], "orca");
        assert_eq!(mcp_item.item["result"]["content"][0]["text"], "found");
        assert_eq!(mcp_item.item["result"]["structuredContent"]["count"], 1);
        assert_eq!(mcp_item.item["result"]["_meta"]["source"], "test");
        assert!(mcp_item.item["error"].is_null());
    });
}

#[test]
fn session_store_preserves_failed_mcp_tool_metadata_in_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "failed mcp projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "search failed".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "mcp-call-1".to_string(),
                        function_name: "mcp__local__search".to_string(),
                        arguments: r#"{"query":"orca"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "mcp-call-1".to_string(),
                    content:
                        r#"{"status":"failed","error":"MCP request timed out","exit_code":124}"#
                            .to_string(),
                    terminal: None,
                    pinned: false,
                },
            ])
            .expect("append failed mcp projection items");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let mcp_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "mcp-call-1")
            .expect("projected mcp item");
        assert_eq!(mcp_item.item["type"], "mcpToolCall");
        assert_eq!(mcp_item.item["status"], "failed");
        assert!(mcp_item.item["result"].is_null());
        assert_eq!(mcp_item.item["error"]["message"], "MCP request timed out");
        assert_eq!(mcp_item.item["error"]["exitCode"], 124);
    });
}

#[test]
fn session_store_projects_error_prefixed_mcp_tool_content_as_failed_item() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "error prefixed mcp thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "slow mcp".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "mcp-call-error".to_string(),
                        function_name: "mcp__slow__wait".to_string(),
                        arguments: "{}".to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "mcp-call-error".to_string(),
                    content: "ERROR: MCP request 'tools/call' timed out after 100ms".to_string(),
                    terminal: None,
                    pinned: false,
                },
            ])
            .expect("append error-prefixed mcp projection items");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let mcp_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "mcp-call-error")
            .expect("projected mcp item");
        assert_eq!(mcp_item.item["type"], "mcpToolCall");
        assert_eq!(mcp_item.item["status"], "failed");
        assert!(mcp_item.item["result"].is_null());
        assert_eq!(
            mcp_item.item["error"]["message"],
            "MCP request 'tools/call' timed out after 100ms"
        );
    });
}

#[test]
fn session_store_projects_external_tool_calls_as_dynamic_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "external projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "deploy staging".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "external-call-1".to_string(),
                        function_name: "deploy".to_string(),
                        arguments: r#"{"env":"staging"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "external-call-1".to_string(),
                    content: "deployed staging".to_string(),
                    terminal: None,
                    pinned: false,
                },
            ])
            .expect("append external projection items");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let external_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "external-call-1")
            .expect("projected external item");
        assert_eq!(external_item.item["type"], "dynamicToolCall");
        assert!(external_item.item["namespace"].is_null());
        assert_eq!(external_item.item["tool"], "deploy");
        assert_eq!(external_item.item["status"], "completed");
        assert_eq!(external_item.item["arguments"]["env"], "staging");
        assert_eq!(external_item.item["success"], true);
        assert_eq!(external_item.item["contentItems"][0]["type"], "text");
        assert_eq!(
            external_item.item["contentItems"][0]["text"],
            "deployed staging"
        );
        assert!(external_item.item["error"].is_null());
    });
}

#[test]
fn session_store_preserves_failed_external_tool_metadata_in_dynamic_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "failed external projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "deploy staging".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "external-call-1".to_string(),
                        function_name: "deploy".to_string(),
                        arguments: r#"{"env":"staging"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "external-call-1".to_string(),
                    content: r#"{"status":"failed","error":"deploy failed","exit_code":42}"#
                        .to_string(),
                    terminal: None,
                    pinned: false,
                },
            ])
            .expect("append failed external projection items");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let external_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "external-call-1")
            .expect("projected external item");
        assert_eq!(external_item.item["type"], "dynamicToolCall");
        assert_eq!(external_item.item["status"], "failed");
        assert_eq!(external_item.item["success"], false);
        assert_eq!(external_item.item["error"]["message"], "deploy failed");
        assert_eq!(external_item.item["error"]["exitCode"], 42);
        assert!(external_item.item["contentItems"].is_null());
    });
}

#[test]
fn session_store_preserves_denied_external_tool_metadata_in_dynamic_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "denied external projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "deploy production".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "external-denied-1".to_string(),
                        function_name: "deploy".to_string(),
                        arguments: r#"{"env":"production"}"#.to_string(),
                    }],
                    pinned: false,
                },
            ])
            .expect("append denied external projection items");

        let request = ToolRequest {
            id: "external-denied-1".to_string(),
            name: ToolName::External("deploy".to_string()),
            action: ActionKind::Write,
            target: Some("production".to_string()),
            raw_arguments: Some(r#"{"env":"production"}"#.to_string()),
        };
        let result = ToolResult::denied(&request, "policy denied deploy");
        thread
            .writer_mut()
            .append_tool_result_message(&result, String::new(), false)
            .expect("append denied external tool result");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let external_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "external-denied-1")
            .expect("projected external item");
        assert_eq!(external_item.item["type"], "dynamicToolCall");
        assert_eq!(external_item.item["status"], "denied");
        assert_eq!(external_item.item["success"], false);
        assert_eq!(
            external_item.item["error"]["message"],
            "policy denied deploy"
        );
        assert!(external_item.item["contentItems"].is_null());
    });
}

#[test]
fn session_store_preserves_truncated_tool_metadata_in_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "truncated projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "run verbose command".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "bash-call-1".to_string(),
                        function_name: "bash".to_string(),
                        arguments: r#"{"command":"printf lots"}"#.to_string(),
                    }],
                    pinned: false,
                },
            ])
            .expect("append tool call");

        let request = ToolRequest {
            id: "bash-call-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf lots".to_string()),
            raw_arguments: Some(r#"{"command":"printf lots"}"#.to_string()),
        };
        let result = ToolResult::completed(&request, "truncated visible output".to_string(), true);
        thread
            .writer_mut()
            .append_tool_result_message(&result, "truncated visible output".to_string(), false)
            .expect("append truncated tool result");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let tool_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "bash-call-1")
            .expect("projected tool item");
        assert_eq!(tool_item.item["type"], "commandExecution");
        assert_eq!(tool_item.item["status"], "completed");
        assert_eq!(
            tool_item.item["aggregatedOutput"],
            "truncated visible output"
        );
        assert!(tool_item.item.get("result").is_none());
        assert_eq!(tool_item.item["truncated"], true);
    });
}

#[test]
fn session_store_preserves_failed_command_projection_without_aggregated_output() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "failed command projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "run failing command".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "bash-failed-1".to_string(),
                        function_name: "bash".to_string(),
                        arguments: r#"{"command":"exit 42"}"#.to_string(),
                    }],
                    pinned: false,
                },
            ])
            .expect("append tool call");

        let request = ToolRequest {
            id: "bash-failed-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("exit 42".to_string()),
            raw_arguments: Some(r#"{"command":"exit 42"}"#.to_string()),
        };
        let result = ToolResult::failed(&request, "command failed", Some(42));
        thread
            .writer_mut()
            .append_tool_result_message(&result, String::new(), false)
            .expect("append failed command result");
        thread.complete("failed").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let tool_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "bash-failed-1")
            .expect("projected command item");
        assert_eq!(tool_item.item["type"], "commandExecution");
        assert_eq!(tool_item.item["status"], "failed");
        assert!(tool_item.item["aggregatedOutput"].is_null());
        assert_eq!(tool_item.item["error"]["message"], "command failed");
        assert_eq!(tool_item.item["error"]["exitCode"], 42);
    });
}

#[test]
fn session_store_round_trips_tool_terminal_metadata() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "terminal metadata round trip")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);

        let cancelled_request = ToolRequest {
            id: "cancelled-call".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("sleep 10".to_string()),
            raw_arguments: Some(r#"{"command":"sleep 10"}"#.to_string()),
        };
        let indeterminate_request = ToolRequest {
            id: "indeterminate-call".to_string(),
            name: ToolName::External("deploy".to_string()),
            action: ActionKind::Write,
            target: Some("production".to_string()),
            raw_arguments: Some(r#"{"env":"production"}"#.to_string()),
        };
        let cancelled = ToolResult::cancelled(&cancelled_request, "turn interrupted", Some(130));
        let indeterminate =
            ToolResult::indeterminate(&indeterminate_request, MISSING_TOOL_TERMINAL_ERROR)
                .with_terminal_source(ToolTerminalSource::CompatibilityRepair);

        thread
            .append_items(&[
                Message::user("run tools".to_string()),
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![
                        RawToolCall {
                            id: cancelled_request.id.clone(),
                            function_name: "bash".to_string(),
                            arguments: cancelled_request.raw_arguments.clone().unwrap(),
                        },
                        RawToolCall {
                            id: indeterminate_request.id.clone(),
                            function_name: "deploy".to_string(),
                            arguments: indeterminate_request.raw_arguments.clone().unwrap(),
                        },
                    ],
                    pinned: false,
                },
            ])
            .expect("append tool calls");
        thread
            .writer_mut()
            .append_tool_result_message(&cancelled, String::new(), false)
            .expect("append cancelled terminal");
        thread
            .writer_mut()
            .append_tool_result_message(
                &indeterminate,
                format!("ERROR: {MISSING_TOOL_TERMINAL_ERROR}"),
                false,
            )
            .expect("append indeterminate terminal");

        let transcript = store.load_session(&thread_id).expect("reload transcript");
        let raw = std::fs::read_to_string(&transcript.path).expect("read raw transcript");
        let records = raw
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["type"] == "conversation.message")
            .filter(|record| record["message"]["role"] == "tool")
            .collect::<Vec<_>>();
        let cancelled_record = records
            .iter()
            .find(|record| record["message"]["tool_call_id"] == "cancelled-call")
            .expect("cancelled JSONL record");
        assert_eq!(cancelled_record["message"]["status"], "cancelled");
        assert_eq!(cancelled_record["message"]["kind"], "cancelled");
        assert_eq!(cancelled_record["message"]["invocation_started"], "yes");
        assert!(cancelled_record["message"].get("terminal_source").is_none());
        let indeterminate_record = records
            .iter()
            .find(|record| record["message"]["tool_call_id"] == "indeterminate-call")
            .expect("indeterminate JSONL record");
        assert_eq!(indeterminate_record["message"]["status"], "indeterminate");
        assert_eq!(indeterminate_record["message"]["kind"], "indeterminate");
        assert_eq!(
            indeterminate_record["message"]["terminal_source"],
            "compatibility_repair"
        );
        assert!(
            indeterminate_record["message"]
                .get("invocation_started")
                .is_none()
        );
        let terminals = transcript
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool {
                    tool_call_id,
                    terminal: Some(terminal),
                    ..
                } => Some((tool_call_id.as_str(), terminal)),
                _ => None,
            })
            .collect::<std::collections::HashMap<_, _>>();

        let cancelled_terminal = terminals["cancelled-call"];
        assert_eq!(cancelled_terminal.status, ToolStatus::Cancelled);
        assert_eq!(cancelled_terminal.kind, ToolResultKind::Cancelled);
        assert_eq!(cancelled_terminal.source, ToolTerminalSource::Observed);
        assert_eq!(cancelled_terminal.started, ToolInvocationStarted::Yes);
        assert_eq!(
            cancelled_terminal.error.as_deref(),
            Some("turn interrupted")
        );
        assert_eq!(cancelled_terminal.exit_code, Some(130));

        let indeterminate_terminal = terminals["indeterminate-call"];
        assert_eq!(indeterminate_terminal.status, ToolStatus::Indeterminate);
        assert_eq!(indeterminate_terminal.kind, ToolResultKind::Indeterminate);
        assert_eq!(
            indeterminate_terminal.source,
            ToolTerminalSource::CompatibilityRepair
        );
        assert_eq!(
            indeterminate_terminal.started,
            ToolInvocationStarted::Unknown
        );
        assert_eq!(
            indeterminate_terminal.error.as_deref(),
            Some(MISSING_TOOL_TERMINAL_ERROR)
        );
    });
}

#[test]
fn session_store_repairs_missing_tool_terminal_for_recovered_projections() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "recover missing terminal")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::user("run legacy command".to_string()),
                Message::Assistant {
                    content: None,
                    reasoning_content: Some("legacy incomplete invocation".to_string()),
                    tool_calls: vec![RawToolCall {
                        id: "legacy-missing-call".to_string(),
                        function_name: "bash".to_string(),
                        arguments: r#"{"command":"deploy production"}"#.to_string(),
                    }],
                    pinned: false,
                },
            ])
            .expect("append incomplete legacy turn");
        enter_test_turn(&mut thread);
        thread
            .append_items(&[Message::user("continue after recovery".to_string())])
            .expect("append recovery continuation turn");
        thread
            .complete("success")
            .expect("complete recovered thread");

        let transcript = store.load_session(&thread_id).expect("load raw transcript");
        let original = std::fs::read(&transcript.path).expect("read source JSONL");
        let projection = store
            .read_thread(&thread_id, true, true)
            .expect("read recovered projection");
        let items = store
            .list_thread_items(&thread_id, None, None, 20, SortDirection::Asc)
            .expect("list recovered items");

        let repaired_message = projection
            .messages
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("synthetic repaired tool message");
        assert_eq!(repaired_message["toolCallId"], "legacy-missing-call");
        assert!(
            repaired_message["content"]
                .as_str()
                .is_some_and(|content| content.contains(MISSING_TOOL_TERMINAL_ERROR))
        );

        let repaired_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "legacy-missing-call")
            .expect("repaired command item");
        assert_eq!(repaired_item.item["status"], "indeterminate");
        assert_eq!(repaired_item.item["kind"], "indeterminate");
        assert_eq!(repaired_item.item["terminalSource"], "compatibility_repair");
        assert_eq!(
            std::fs::read(&transcript.path).expect("reread source JSONL"),
            original,
            "projection repair must not rewrite the source JSONL"
        );
    });
}

#[test]
fn session_store_projects_file_edit_calls_as_file_change_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "edit projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "edit note".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "edit-call-1".to_string(),
                        function_name: "edit".to_string(),
                        arguments: r#"{"path":"note.txt","old_text":"hello","new_text":"hi"}"#
                            .to_string(),
                    }],
                    pinned: false,
                },
            ])
            .expect("append edit tool call");

        let request = ToolRequest {
            id: "edit-call-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some("note.txt".to_string()),
            raw_arguments: Some(
                r#"{"path":"note.txt","old_text":"hello","new_text":"hi"}"#.to_string(),
            ),
        };
        let result = ToolResult::completed(&request, "edited note.txt".to_string(), false);
        thread
            .writer_mut()
            .append_tool_result_message(&result, "edited note.txt".to_string(), false)
            .expect("append edit result");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let file_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "edit-call-1:file-change")
            .expect("projected file change item");
        assert_eq!(file_item.item["type"], "fileChange");
        assert_eq!(file_item.item["status"], "completed");
        assert_eq!(file_item.item["changes"][0]["path"], "note.txt");
        assert_eq!(file_item.item["changes"][0]["kind"], "edit");
        assert!(file_item.item["changes"][0]["diff"].as_str().is_some());
        assert!(file_item.item.get("tool").is_none());
        assert!(file_item.item.get("output").is_none());
        assert!(file_item.item.get("error").is_none());
    });
}

#[test]
fn session_store_projects_write_file_calls_as_file_change_thread_items() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "write projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "write note".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "write-call-1".to_string(),
                        function_name: "write_file".to_string(),
                        arguments: r#"{"path":"notes/new.txt","content":"hello"}"#.to_string(),
                    }],
                    pinned: false,
                },
            ])
            .expect("append write_file tool call");

        let request = ToolRequest {
            id: "write-call-1".to_string(),
            name: ToolName::WriteFile,
            action: ActionKind::Write,
            target: Some("notes/new.txt".to_string()),
            raw_arguments: Some(r#"{"path":"notes/new.txt","content":"hello"}"#.to_string()),
        };
        let result = ToolResult::completed(
            &request,
            "wrote 5 bytes to notes/new.txt".to_string(),
            false,
        );
        thread
            .writer_mut()
            .append_tool_result_message(
                &result,
                "wrote 5 bytes to notes/new.txt".to_string(),
                false,
            )
            .expect("append write_file result");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let file_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "write-call-1:file-change")
            .expect("projected file change item");
        assert_eq!(file_item.item["type"], "fileChange");
        assert_eq!(file_item.item["status"], "completed");
        assert_eq!(file_item.item["changes"][0]["path"], "notes/new.txt");
        assert_eq!(file_item.item["changes"][0]["kind"], "write");
        assert!(file_item.item["changes"][0]["diff"].as_str().is_some());
        assert!(file_item.item.get("tool").is_none());
        assert!(file_item.item.get("output").is_none());
        assert!(file_item.item.get("error").is_none());
    });
}

#[test]
fn session_store_projects_builtin_read_tool_as_dynamic_thread_item() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "read projected thread")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "read readme".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: vec![RawToolCall {
                        id: "read-call-1".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: r#"{"path":"README.md"}"#.to_string(),
                    }],
                    pinned: false,
                },
                Message::Tool {
                    tool_call_id: "read-call-1".to_string(),
                    content: "readme contents".to_string(),
                    terminal: None,
                    pinned: false,
                },
            ])
            .expect("append read projection items");
        thread.complete("success").expect("complete thread");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list thread items");
        let read_item = items
            .data
            .iter()
            .find(|item| item.item["id"] == "read-call-1")
            .expect("projected read item");
        assert_eq!(read_item.item["type"], "dynamicToolCall");
        assert_eq!(read_item.item["tool"], "read_file");
        assert_eq!(read_item.item["status"], "completed");
        assert_eq!(read_item.item["arguments"]["path"], "README.md");
        assert_eq!(read_item.item["success"], true);
        assert_eq!(read_item.item["contentItems"][0]["type"], "text");
        assert_eq!(read_item.item["contentItems"][0]["text"], "readme contents");
        assert!(read_item.item["error"].is_null());
    });
}

#[test]
fn session_store_projects_multiple_user_turns_with_stable_item_ids() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "multi turn projection")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        let first_turn_id = enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "first user".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("first assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append first turn items");
        let second_turn_id = enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "second user".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("second assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append second turn items");
        thread.complete("success").expect("complete thread");

        let turns = store
            .list_thread_turns(
                &thread_id,
                None,
                10,
                SortDirection::Asc,
                TurnItemsView::Full,
            )
            .expect("list thread turns");
        assert_eq!(turns.data.len(), 2);
        assert_eq!(turns.data[0].turn_id, first_turn_id.as_str());
        assert_eq!(turns.data[0].items.len(), 2);
        assert_eq!(turns.data[1].turn_id, second_turn_id.as_str());
        assert_eq!(turns.data[1].items.len(), 2);
        assert_eq!(turns.data[1].items[0]["content"], "second user");

        let items = store
            .list_thread_items(&thread_id, None, None, 10, SortDirection::Asc)
            .expect("list all items");
        assert_eq!(items.data.len(), 4);
        assert!(
            items
                .data
                .iter()
                .all(|item| item.item_id.starts_with("item_"))
        );
        assert_eq!(items.data[0].turn_id, first_turn_id.as_str());
        assert_eq!(items.data[1].turn_id, first_turn_id.as_str());
        assert_eq!(items.data[2].turn_id, second_turn_id.as_str());
        assert_eq!(items.data[3].turn_id, second_turn_id.as_str());
        let item_ids = items
            .data
            .iter()
            .map(|item| item.item_id.clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(item_ids.len(), 4);

        let second_turn_items = store
            .list_thread_items(
                &thread_id,
                Some(second_turn_id.as_str()),
                None,
                10,
                SortDirection::Asc,
            )
            .expect("list second turn items");
        assert_eq!(second_turn_items.data.len(), 2);
        assert_eq!(second_turn_items.data[0].item_id, items.data[2].item_id);
        assert_eq!(second_turn_items.data[0].item["content"], "second user");
        assert_eq!(second_turn_items.data[1].item_id, items.data[3].item_id);
        assert_eq!(
            second_turn_items.data[1].item["content"],
            "second assistant"
        );
    });
}

#[test]
fn session_store_paginates_thread_turns_and_items_with_cursors() {
    with_orca_home(|home| {
        let store = SessionStore::new();
        let mut thread = store
            .create_live_thread(home, "mock", None, "paginated projection")
            .expect("create live thread");
        let thread_id = thread.thread_id().to_string();
        let first_turn_id = enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "first user".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("first assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append first paginated turn");
        let second_turn_id = enter_test_turn(&mut thread);
        thread
            .append_items(&[
                Message::User {
                    content: "second user".to_string(),
                    images: Vec::new(),
                    pinned: false,
                },
                Message::Assistant {
                    content: Some("second assistant".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    pinned: false,
                },
            ])
            .expect("append second paginated turn");
        thread.complete("success").expect("complete thread");

        let first_turn_page = store
            .list_thread_turns(&thread_id, None, 1, SortDirection::Asc, TurnItemsView::Full)
            .expect("first turn page");
        assert_eq!(first_turn_page.data.len(), 1);
        assert_eq!(first_turn_page.data[0].turn_id, first_turn_id.as_str());
        assert_eq!(first_turn_page.next_cursor.as_deref(), Some("1"));
        assert_eq!(first_turn_page.backwards_cursor.as_deref(), Some("0"));

        let second_turn_page = store
            .list_thread_turns(
                &thread_id,
                first_turn_page.next_cursor.as_deref(),
                1,
                SortDirection::Asc,
                TurnItemsView::Full,
            )
            .expect("second turn page");
        assert_eq!(second_turn_page.data.len(), 1);
        assert_eq!(second_turn_page.data[0].turn_id, second_turn_id.as_str());
        assert_eq!(second_turn_page.next_cursor, None);
        assert_eq!(second_turn_page.backwards_cursor.as_deref(), Some("1"));

        let first_item_page = store
            .list_thread_items(&thread_id, None, None, 2, SortDirection::Asc)
            .expect("first item page");
        assert_eq!(first_item_page.data.len(), 2);
        assert!(
            first_item_page
                .data
                .iter()
                .all(|item| item.item_id.starts_with("item_"))
        );
        assert!(
            first_item_page
                .data
                .iter()
                .all(|item| item.turn_id == first_turn_id.as_str())
        );
        assert_eq!(first_item_page.next_cursor.as_deref(), Some("2"));

        let second_item_page = store
            .list_thread_items(
                &thread_id,
                None,
                first_item_page.next_cursor.as_deref(),
                2,
                SortDirection::Asc,
            )
            .expect("second item page");
        assert_eq!(second_item_page.data.len(), 2);
        assert!(
            second_item_page
                .data
                .iter()
                .all(|item| item.item_id.starts_with("item_"))
        );
        assert!(
            second_item_page
                .data
                .iter()
                .all(|item| item.turn_id == second_turn_id.as_str())
        );
        assert_eq!(second_item_page.next_cursor, None);
        assert_eq!(second_item_page.backwards_cursor.as_deref(), Some("2"));

        let latest_turn_page = store
            .list_thread_turns(
                &thread_id,
                None,
                1,
                SortDirection::Desc,
                TurnItemsView::Full,
            )
            .expect("latest turn page");
        assert_eq!(latest_turn_page.data.len(), 1);
        assert_eq!(latest_turn_page.data[0].turn_id, second_turn_id.as_str());
        assert_eq!(latest_turn_page.next_cursor.as_deref(), Some("1"));

        let unloaded_turn_page = store
            .list_thread_turns(
                &thread_id,
                None,
                10,
                SortDirection::Asc,
                TurnItemsView::NotLoaded,
            )
            .expect("unloaded turn page");
        assert_eq!(unloaded_turn_page.data.len(), 2);
        assert_eq!(
            unloaded_turn_page.data[0].items_view,
            TurnItemsView::NotLoaded
        );
        assert!(unloaded_turn_page.data[0].items.is_empty());

        let latest_item_page = store
            .list_thread_items(&thread_id, None, None, 1, SortDirection::Desc)
            .expect("latest item page");
        assert_eq!(latest_item_page.data.len(), 1);
        assert_eq!(
            latest_item_page.data[0].item_id,
            second_item_page.data[1].item_id
        );
        assert_eq!(latest_item_page.data[0].item["content"], "second assistant");
    });
}

fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let home = tempdir().expect("temp home");
    let previous = std::env::var_os("ORCA_HOME");
    unsafe {
        std::env::set_var("ORCA_HOME", home.path());
    }
    let result = f(home.path());
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("ORCA_HOME", previous);
        } else {
            std::env::remove_var("ORCA_HOME");
        }
    }
    result
}

fn enter_test_turn(thread: &mut LiveThread) -> TurnId {
    let turn_id = TurnId::new();
    thread.writer_mut().enter_turn(turn_id.clone());
    turn_id
}
