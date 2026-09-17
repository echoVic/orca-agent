use std::collections::HashMap;

use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::thread_identity::TurnId;
use orca_core::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};

#[derive(Clone, Debug)]
pub struct RuntimeModelResponse {
    pub response: ProviderResponse,
    pub identity: ModelResponseIdentity,
}

impl RuntimeModelResponse {
    pub fn new(response: ProviderResponse, turn_id: TurnId) -> Self {
        Self {
            response,
            identity: ModelResponseIdentity::new(turn_id),
        }
    }

    pub fn from_parts(response: ProviderResponse, identity: ModelResponseIdentity) -> Self {
        Self { response, identity }
    }

    pub fn completed(&self) -> CompletedModelResponse {
        CompletedModelResponse::new(
            self.identity.clone(),
            self.response.assistant_content.clone(),
            self.response.assistant_reasoning.clone(),
            self.response.tool_calls.clone(),
        )
    }

    /// Response-local name for the tool calls of this response.
    pub fn tool_call_namespace(&self) -> String {
        self.identity
            .item_ids
            .conversation_item_id
            .as_str()
            .to_string()
    }

    /// Rename the tool calls whose ids the session already uses.
    ///
    /// A provider only owes id uniqueness *within one response*, but the runtime
    /// keys a tool by its id for the whole session: a second tool request with an
    /// id the surface already holds aborted the session with an opaque
    /// `SurfaceReducerError` (issue #67). That is reachable with any provider
    /// that derives ids deterministically (`call_0`, `call_1`, …), with a relay
    /// that mints them, and with an id reused across turns.
    ///
    /// `is_taken` answers "does the session already hold this id from a
    /// *different* response?" — a replayed response must keep its names, or the
    /// surface ingress would treat the replay as new tool calls. The assistant
    /// message and the executable request are renamed together, so the pairing
    /// the provider sees stays consistent.
    pub fn rename_repeated_tool_call_ids(
        &mut self,
        is_taken: impl Fn(&str) -> bool,
    ) -> Vec<(String, String)> {
        let namespace = self.tool_call_namespace();
        let mut renamed = Vec::new();
        let mut call_occurrences: HashMap<String, u32> = HashMap::new();
        for call in &mut self.response.tool_calls {
            if let Some(new_id) =
                repeated_tool_call_id(&call.id, &namespace, &is_taken, &mut call_occurrences)
            {
                renamed.push((call.id.clone(), new_id.clone()));
                call.id = new_id;
            }
        }
        let mut step_occurrences: HashMap<String, u32> = HashMap::new();
        for step in &mut self.response.steps {
            if let ProviderStep::ToolCall(request) = step
                && let Some(new_id) =
                    repeated_tool_call_id(&request.id, &namespace, &is_taken, &mut step_occurrences)
            {
                request.id = new_id;
            }
        }
        renamed
    }
}

/// `None` when the id can stay; otherwise the session-unique replacement.
fn repeated_tool_call_id(
    id: &str,
    namespace: &str,
    is_taken: &impl Fn(&str) -> bool,
    occurrences: &mut HashMap<String, u32>,
) -> Option<String> {
    let seen = occurrences.entry(id.to_string()).or_insert(0);
    *seen += 1;
    if *seen == 1 && !is_taken(id) {
        return None;
    }
    Some(if *seen == 1 {
        format!("{id}@{namespace}")
    } else {
        // A single response may repeat an id; number the repeats so both lists
        // still agree call for call.
        format!("{id}@{namespace}#{seen}")
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use orca_core::provider_types::ProviderStep;
    use orca_core::thread_identity::TurnId;

    use super::*;

    fn response_with_tool_calls(ids: &[&str]) -> RuntimeModelResponse {
        let mut steps = Vec::new();
        let mut tool_calls = Vec::new();
        for id in ids {
            let arguments = serde_json::json!({ "command": "true" }).to_string();
            steps.push(ProviderStep::ToolCall(orca_core::tool_types::ToolRequest {
                id: (*id).to_string(),
                name: orca_core::tool_types::ToolName::Bash,
                action: orca_core::approval_types::ActionKind::Shell,
                target: Some("true".to_string()),
                raw_arguments: Some(arguments.clone()),
            }));
            tool_calls.push(orca_core::conversation::RawToolCall {
                id: (*id).to_string(),
                function_name: "bash".to_string(),
                arguments,
            });
        }
        RuntimeModelResponse::new(
            ProviderResponse {
                steps,
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls,
                usage: None,
            },
            TurnId::new(),
        )
    }

    fn request_ids(response: &RuntimeModelResponse) -> Vec<String> {
        response
            .response
            .steps
            .iter()
            .filter_map(|step| match step {
                ProviderStep::ToolCall(request) => Some(request.id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ids_the_session_already_uses_are_renamed_with_the_response() {
        let mut response = response_with_tool_calls(&["call_0", "call_1"]);
        let namespace = response.tool_call_namespace();
        let taken: HashSet<String> = ["call_0".to_string()].into_iter().collect();

        let renamed = response.rename_repeated_tool_call_ids(|id| taken.contains(id));

        assert_eq!(
            renamed,
            vec![("call_0".to_string(), format!("call_0@{namespace}"))]
        );
        // The assistant message and the executable requests must agree, or the
        // surface rejects the batch for lacking its matching response.
        let calls: Vec<&str> = response
            .response
            .tool_calls
            .iter()
            .map(|call| call.id.as_str())
            .collect();
        let expected = vec![format!("call_0@{namespace}"), "call_1".to_string()];
        assert_eq!(calls, expected);
        assert_eq!(request_ids(&response), expected);
    }

    #[test]
    fn ids_repeated_inside_one_response_are_numbered() {
        let mut response = response_with_tool_calls(&["call_0", "call_0", "call_0"]);
        let namespace = response.tool_call_namespace();

        response.rename_repeated_tool_call_ids(|_| false);

        // The first use of an id the session has never seen stays as it is; the
        // repeats are numbered so the session-wide key stays unique.
        let expected = vec![
            "call_0".to_string(),
            format!("call_0@{namespace}#2"),
            format!("call_0@{namespace}#3"),
        ];
        let calls: Vec<String> = response
            .response
            .tool_calls
            .iter()
            .map(|call| call.id.clone())
            .collect();
        assert_eq!(calls, expected);
        assert_eq!(request_ids(&response), expected);
    }

    #[test]
    fn the_same_response_identity_produces_the_same_names() {
        let template = response_with_tool_calls(&["call_0"]);
        let identity = template.identity.clone();
        let mut first =
            RuntimeModelResponse::from_parts(template.response.clone(), identity.clone());
        let mut replay = RuntimeModelResponse::from_parts(template.response, identity);
        // The session holds `call_0` from an earlier response, so both attempts
        // rename it — and both must land on the same name, or the surface would
        // see the replayed response as a fresh tool call and run it twice.
        let taken: HashSet<String> = ["call_0".to_string()].into_iter().collect();

        first.rename_repeated_tool_call_ids(|id| taken.contains(id));
        replay.rename_repeated_tool_call_ids(|id| taken.contains(id));

        assert_eq!(
            first.response.tool_calls[0].id,
            replay.response.tool_calls[0].id
        );
        assert_eq!(
            first.response.tool_calls[0].id,
            format!("call_0@{}", first.tool_call_namespace())
        );
    }
}
