use std::io::{self, Write};

use serde_json::{Value, json};

use super::super::*;

pub(in crate::server::router) fn is_query_operation(op: &ClientOp) -> bool {
    matches!(
        op,
        ClientOp::ThreadRead { .. }
            | ClientOp::ThreadList { .. }
            | ClientOp::ThreadSearch { .. }
            | ClientOp::ThreadTurnsList { .. }
            | ClientOp::ThreadItemsList { .. }
            | ClientOp::ThreadMetadataUpdate { .. }
            | ClientOp::ThreadQueue { .. }
    )
}

pub(in crate::server::router) fn dispatch_query_operation<W: Write>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::ThreadRead {
            thread_id,
            include_messages,
            include_turns,
        } => {
            state.prune_finished_turns();
            run_thread_read(
                state,
                thread_id,
                *include_messages,
                *include_turns,
                id,
                writer,
            )
        }
        ClientOp::ThreadList {
            cursor,
            sort_key,
            sort_direction,
            search_term,
            limit,
            filters,
        } => run_thread_list(
            state,
            cursor.as_deref(),
            *limit,
            filters.clone(),
            *sort_key,
            *sort_direction,
            search_term.as_deref(),
            id,
            writer,
        ),
        ClientOp::ThreadSearch {
            query,
            cursor,
            sort_key,
            sort_direction,
            include_archived,
            limit,
        } => run_thread_search(
            state,
            query,
            cursor.as_deref(),
            *limit,
            *include_archived,
            *sort_key,
            *sort_direction,
            id,
            writer,
        ),
        ClientOp::ThreadTurnsList {
            thread_id,
            cursor,
            sort_direction,
            items_view,
            limit,
        } => {
            state.prune_finished_turns();
            run_thread_turns_list(
                state,
                thread_id,
                cursor.as_deref(),
                *limit,
                *sort_direction,
                *items_view,
                id,
                writer,
            )
        }
        ClientOp::ThreadItemsList {
            thread_id,
            turn_id,
            cursor,
            sort_direction,
            limit,
        } => {
            state.prune_finished_turns();
            run_thread_items_list(
                state,
                thread_id,
                turn_id.as_deref(),
                cursor.as_deref(),
                *limit,
                *sort_direction,
                id,
                writer,
            )
        }
        ClientOp::ThreadMetadataUpdate { thread_id, title } => {
            run_thread_metadata_update(state, thread_id, title.clone(), id, writer)
        }
        ClientOp::ThreadQueue { thread_id, action } => {
            match state.threads.prompt_queue(thread_id, action.clone()) {
                Ok(snapshot) => protocol::write_server_event(
                    writer,
                    &id,
                    ServerEvent::ThreadQueueSnapshot {
                        thread_id: Value::from(thread_id.clone()),
                        snapshot: serde_json::to_value(snapshot).map_err(io::Error::other)?,
                    },
                ),
                Err(error) => write_prompt_queue_error(writer, &id, thread_id, &error),
            }
        }
        _ => protocol::write_server_event(
            writer,
            &id,
            ServerEvent::error("unsupported thread operation"),
        ),
    }
}

fn write_prompt_queue_error<W: Write>(
    writer: &mut W,
    id: &Value,
    thread_id: &str,
    error: &crate::prompt_queue::PromptQueueMutationError,
) -> io::Result<()> {
    let current = match error {
        crate::prompt_queue::PromptQueueMutationError::RevisionConflict { current }
        | crate::prompt_queue::PromptQueueMutationError::NotFound { current }
        | crate::prompt_queue::PromptQueueMutationError::CapacityExceeded { current }
        | crate::prompt_queue::PromptQueueMutationError::DispatchInProgress { current }
        | crate::prompt_queue::PromptQueueMutationError::InvalidInput { current, .. } => current,
        crate::prompt_queue::PromptQueueMutationError::PersistenceFailed { .. }
        | crate::prompt_queue::PromptQueueMutationError::RuntimeUnavailable => {
            return protocol::write_server_event(writer, id, ServerEvent::error(error.to_string()));
        }
    };
    let mut bytes = serde_json::to_vec(&json!({
        "id": id,
        "event": "error",
        "message": error.to_string(),
        "threadId": thread_id,
        "snapshot": current,
    }))
    .map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_queue_mutation_error_includes_current_snapshot_bits_spec_ut() {
        let current = crate::prompt_queue::PromptQueueSnapshot {
            revision: crate::prompt_queue::QueueRevision::from_u64(7),
            paused: true,
            ..Default::default()
        };
        let error = crate::prompt_queue::PromptQueueMutationError::RevisionConflict { current };
        let mut output = Vec::new();

        write_prompt_queue_error(
            &mut output,
            &Value::from("queue-update"),
            "thread-1",
            &error,
        )
        .expect("write queue mutation error");

        let event: Value = serde_json::from_slice(&output).expect("queue mutation error event");
        assert_eq!(event["id"], "queue-update");
        assert_eq!(event["event"], "error");
        assert_eq!(event["threadId"], "thread-1");
        assert_eq!(event["snapshot"]["revision"], 7);
        assert_eq!(event["snapshot"]["paused"], true);
        assert!(
            event["message"]
                .as_str()
                .unwrap()
                .contains("revision conflict")
        );
    }
}
