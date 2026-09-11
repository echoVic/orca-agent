use std::io::{self, Write};

use serde_json::{Value, json};

use super::super::*;

pub(in crate::server::router) fn is_user_input_operation(op: &ClientOp) -> bool {
    matches!(op, ClientOp::UserInputRespond { .. })
}

pub(in crate::server::router) fn dispatch_user_input_operation<W: Write>(
    state: &mut ServerState,
    op: &ClientOp,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    match op {
        ClientOp::UserInputRespond {
            request_id,
            response,
        } => run_user_input_respond(state, request_id, response.clone(), id, writer),
        _ => unreachable!("only user input operations can reach the user input processor"),
    }
}

fn run_user_input_respond<W: Write>(
    state: &mut ServerState,
    request_id: &str,
    response: protocol::UserInputResponse,
    id: Value,
    writer: &mut W,
) -> io::Result<()> {
    let response_digest = match &response {
        protocol::UserInputResponse::Cancel => jsonl_response_digest(&json!({ "answer": null }))?,
        protocol::UserInputResponse::Answer(answer) => {
            jsonl_response_digest(&json!({ "answer": answer }))?
        }
        protocol::UserInputResponse::Submitted { answers } => {
            jsonl_response_digest(&json!({ "answers": answers }))?
        }
        protocol::UserInputResponse::Chat(message) => {
            jsonl_response_digest(&json!({ "chat": message }))?
        }
    };
    let pending = state.direct_interactions.published_route(
        request_id,
        direct_interaction_adapter::JsonlDirectInteractionKind::UserInput,
    )?;
    let pending = match pending {
        Some(direct_interaction_adapter::JsonlDirectInteractionRoute::UserInput {
            client,
            interaction_id,
        }) => Some((client, interaction_id)),
        Some(direct_interaction_adapter::JsonlDirectInteractionRoute::McpElicitation {
            ..
        })
        | None => None,
    };
    let Some(pending) = pending else {
        return match state
            .direct_interactions
            .committed_replay(request_id, response_digest)?
        {
            JsonlCommittedReplay::SameResponse => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::UserInputResolved {
                    request_id: json!(request_id),
                    answered: json!(!matches!(response, protocol::UserInputResponse::Cancel)),
                },
            ),
            JsonlCommittedReplay::ConflictingResponse => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input request already resolved with a different response: {request_id}"
                )),
            ),
            JsonlCommittedReplay::NotCommitted => protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("unknown user input request: {request_id}")),
            ),
        };
    };
    let (answered, decision) = match surface_user_input_decision(response) {
        Ok(response) => response,
        Err(error) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!("invalid structured user input response: {error}")),
            );
        }
    };
    let response_request_id = crate::surface::SurfaceRequestId::new();
    match pending.0.respond_interaction_by_id(
        response_request_id,
        pending.1,
        crate::surface::SurfaceClientInteractionAnswer::UserInput { decision },
    ) {
        Ok(crate::surface::MutationReply::Committed { .. }) => {}
        Ok(crate::surface::MutationReply::Deferred { mutation, .. }) => {
            state.direct_interactions.mark_committed_pending(
                request_id,
                &mutation,
                response_digest,
            )?;
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input response is awaiting durable reconciliation: {request_id}"
                )),
            );
        }
        Ok(crate::surface::MutationReply::Uncommitted { .. }) | Err(_) => {
            return protocol::write_server_event(
                writer,
                &id,
                ServerEvent::error(format!(
                    "user input request is no longer active: {request_id}"
                )),
            );
        }
    }
    state
        .direct_interactions
        .settle_committed(request_id, response_digest)?;
    protocol::write_server_event(
        writer,
        &id,
        ServerEvent::UserInputResolved {
            request_id: json!(request_id),
            answered: json!(answered),
        },
    )
}

fn surface_user_input_decision(
    response: protocol::UserInputResponse,
) -> Result<(bool, crate::surface::SurfaceUserInputDecision), crate::surface::SurfaceValueError> {
    match response {
        protocol::UserInputResponse::Cancel => {
            Ok((false, crate::surface::SurfaceUserInputDecision::Cancel))
        }
        protocol::UserInputResponse::Answer(answer) => Ok((
            true,
            crate::surface::SurfaceUserInputDecision::Answer(crate::surface::DisplayText::new(
                answer,
            )),
        )),
        protocol::UserInputResponse::Submitted { answers } => {
            let answers = answers
                .into_iter()
                .map(|answer| {
                    Ok(crate::surface::SurfaceUserInputQuestionAnswer {
                        question_id: crate::surface::NonEmptyText::try_new(answer.question_id)?,
                        answers: answer
                            .answers
                            .into_iter()
                            .map(crate::surface::DisplayText::new)
                            .collect(),
                    })
                })
                .collect::<Result<Vec<_>, crate::surface::SurfaceValueError>>()?;
            Ok((
                true,
                crate::surface::SurfaceUserInputDecision::Submitted(
                    crate::surface::SurfaceUserInputResponse { answers },
                ),
            ))
        }
        protocol::UserInputResponse::Chat(message) => Ok((
            true,
            crate::surface::SurfaceUserInputDecision::Chat(crate::surface::DisplayText::new(
                message,
            )),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_answers_preserve_every_question_identity() {
        let (_, decision) = surface_user_input_decision(protocol::UserInputResponse::Submitted {
            answers: vec![
                protocol::UserInputQuestionAnswer {
                    question_id: "question-1".to_string(),
                    answers: vec!["Focused".to_string()],
                },
                protocol::UserInputQuestionAnswer {
                    question_id: "question-2".to_string(),
                    answers: vec!["Rust".to_string(), "TypeScript".to_string()],
                },
            ],
        })
        .expect("structured response");

        let crate::surface::SurfaceUserInputDecision::Submitted(response) = decision else {
            panic!("structured response must remain submitted");
        };
        assert_eq!(response.answers.len(), 2);
        assert_eq!(response.answers[0].question_id.as_str(), "question-1");
        assert_eq!(response.answers[1].question_id.as_str(), "question-2");
        assert_eq!(
            response.answers[1]
                .answers
                .iter()
                .map(crate::surface::DisplayText::as_str)
                .collect::<Vec<_>>(),
            vec!["Rust", "TypeScript"]
        );
    }
}
