use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::approval_types::ActionKind;
use crate::tool_types::{ToolName, ToolRequest, ToolResult, ToolTerminal, ToolTerminalSource};

pub const MISSING_TOOL_TERMINAL_ERROR: &str = "Tool invocation outcome is indeterminate because its terminal result was missing from recovered history. Inspect external state before retrying.";
pub const IMAGE_ANALYSIS_MESSAGE_PREFIX: &str = "[Image analysis:";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RawToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    Low,
    #[default]
    High,
    Original,
    Auto,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
    File { file_id: String },
}

impl fmt::Debug for ImageSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Base64 { media_type, data } => formatter
                .debug_struct("Base64")
                .field("media_type", media_type)
                .field("encoded_chars", &data.len())
                .finish(),
            Self::Url { url } => formatter
                .debug_struct("Url")
                .field("url_chars", &url.len())
                .finish(),
            Self::File { file_id } => formatter
                .debug_struct("File")
                .field("file_id_chars", &file_id.len())
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImageInput {
    pub source: ImageSource,
    #[serde(default)]
    pub detail: ImageDetail,
}

#[derive(Clone, Debug)]
pub enum Message {
    System {
        content: String,
        pinned: bool,
    },
    User {
        content: String,
        images: Vec<ImageInput>,
        pinned: bool,
    },
    Assistant {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
        pinned: bool,
    },
    Tool {
        tool_call_id: String,
        content: String,
        terminal: Option<ToolTerminal>,
        pinned: bool,
    },
}

impl Message {
    pub fn system(content: String) -> Self {
        Self::System {
            content,
            pinned: false,
        }
    }

    pub fn pinned_system(content: String) -> Self {
        Self::System {
            content,
            pinned: true,
        }
    }

    pub fn user(content: String) -> Self {
        Self::User {
            content,
            images: Vec::new(),
            pinned: false,
        }
    }

    pub fn user_with_images(content: String, images: Vec<ImageInput>) -> Self {
        Self::User {
            content,
            images,
            pinned: false,
        }
    }

    pub fn pinned_user(content: String) -> Self {
        Self::User {
            content,
            images: Vec::new(),
            pinned: true,
        }
    }

    pub fn pinned_user_with_images(content: String, images: Vec<ImageInput>) -> Self {
        Self::User {
            content,
            images,
            pinned: true,
        }
    }

    pub fn is_pinned(&self) -> bool {
        match self {
            Self::System { pinned, .. }
            | Self::User { pinned, .. }
            | Self::Assistant { pinned, .. }
            | Self::Tool { pinned, .. } => *pinned,
        }
    }

    pub fn content_str(&self) -> Option<&str> {
        match self {
            Self::System { content, .. }
            | Self::User { content, .. }
            | Self::Tool { content, .. } => Some(content),
            Self::Assistant { content, .. } => content.as_deref(),
        }
    }
}

pub const RUNTIME_CONTEXT_FRAGMENT_ID: &str = "runtime";
pub const MODE_CONTEXT_FRAGMENT_ID: &str = "mode";
pub const GOAL_CONTEXT_FRAGMENT_ID: &str = "goal";
pub const PLAN_CONTEXT_FRAGMENT_ID: &str = "plan";
pub const SKILL_CONTEXT_FRAGMENT_ID: &str = "skill";
pub const MEMORY_CONTEXT_FRAGMENT_ID: &str = "memory";

pub const RUNTIME_CONTEXT_MAX_TOKENS: usize = 1_024;
pub const MODE_CONTEXT_MAX_TOKENS: usize = 2_048;
pub const GOAL_CONTEXT_MAX_TOKENS: usize = 4_096;
pub const PLAN_CONTEXT_MAX_TOKENS: usize = 4_096;
pub const SKILL_CONTEXT_MAX_TOKENS: usize = 4_096;
pub const MEMORY_CONTEXT_MAX_TOKENS: usize = 3_072;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InternalContextKind {
    Runtime,
    Memory,
    Goal,
    Plan,
    Skill,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InternalContextOrigin {
    System,
    GoalRuntime,
    Model,
    User,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InternalContextFragment {
    pub id: String,
    pub kind: InternalContextKind,
    pub origin: InternalContextOrigin,
    pub content: String,
    pub max_tokens: usize,
}

#[derive(Clone, Debug, Default)]
pub struct InternalContext {
    fragments: Vec<InternalContextFragment>,
    /// Assistant turns since the plan fragment was last successfully updated.
    plan_age_turns: u32,
}

/// Assistant turns a plan with open steps may go without an update before the
/// rendered context nudges the model to reconcile it.
pub const PLAN_REMINDER_AFTER_TURNS: u32 = 10;

const PLAN_REMINDER: &str = "[Plan reminder] The pinned plan above has open steps but has not been updated for a while. If steps were finished, call update_plan now with every step's current status. If the plan no longer matches the work, replace or clear it via update_plan. Do not announce completion while the plan still shows open steps.";

impl InternalContext {
    pub fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }

    pub fn len(&self) -> usize {
        self.fragments.len()
    }

    pub fn get(&self, id: &str) -> Option<&InternalContextFragment> {
        self.fragments.iter().find(|fragment| fragment.id == id)
    }

    pub fn fragments(&self) -> &[InternalContextFragment] {
        &self.fragments
    }

    pub fn replace(&mut self, fragment: InternalContextFragment) {
        if let Some(existing) = self
            .fragments
            .iter_mut()
            .find(|existing| existing.id == fragment.id)
        {
            *existing = fragment;
        } else {
            self.fragments.push(fragment);
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.fragments.retain(|fragment| fragment.id != id);
    }

    pub fn note_assistant_turn(&mut self) {
        if self.get(PLAN_CONTEXT_FRAGMENT_ID).is_some() {
            self.plan_age_turns = self.plan_age_turns.saturating_add(1);
        }
    }

    fn plan_reminder_due(&self) -> bool {
        self.plan_age_turns >= PLAN_REMINDER_AFTER_TURNS
            && self.get(PLAN_CONTEXT_FRAGMENT_ID).is_some_and(|plan| {
                plan.content.contains("[in_progress]") || plan.content.contains("[pending]")
            })
    }

    pub fn rendered_fragments(&self) -> Vec<InternalContextFragment> {
        let mut fragments = self.fragments.clone();
        fragments.sort_by_key(|fragment| fragment.kind);
        if self.plan_reminder_due()
            && let Some(plan) = fragments
                .iter_mut()
                .find(|fragment| fragment.id == PLAN_CONTEXT_FRAGMENT_ID)
        {
            plan.content.push_str("\n\n");
            plan.content.push_str(PLAN_REMINDER);
        }
        fragments
    }

    pub fn render(&self) -> String {
        self.rendered_fragments()
            .into_iter()
            .map(|fragment| fragment.content)
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SummaryState {
    pub baseline: Option<String>,
    pub deltas: Vec<String>,
}

impl SummaryState {
    pub fn is_empty(&self) -> bool {
        self.baseline.is_none() && self.deltas.is_empty()
    }

    pub fn latest_rolling(&self) -> Option<&str> {
        if let Some(last_delta) = self.deltas.last() {
            Some(last_delta.as_str())
        } else {
            self.baseline.as_deref()
        }
    }

    pub fn total_tokens(&self, counter: &impl TokenCountable) -> usize {
        let mut total = 0;
        if let Some(baseline) = &self.baseline {
            total += counter.count(baseline);
        }
        for delta in &self.deltas {
            total += counter.count(delta);
        }
        total
    }
}

pub trait TokenCountable {
    fn count(&self, text: &str) -> usize;
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub messages: Vec<Message>,
    pub internal_context: InternalContext,
    pub rolling_summary: Option<String>,
    pub summary: SummaryState,
}

impl Conversation {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            internal_context: InternalContext::default(),
            rolling_summary: None,
            summary: SummaryState::default(),
        }
    }

    pub fn add_system(&mut self, content: String) {
        self.messages.push(Message::system(content));
    }

    pub fn add_system_pinned(&mut self, content: String) {
        self.messages.push(Message::pinned_system(content));
    }

    pub fn replace_plan_state(&mut self, content: String) {
        self.internal_context.replace(InternalContextFragment {
            id: PLAN_CONTEXT_FRAGMENT_ID.to_string(),
            kind: InternalContextKind::Plan,
            origin: InternalContextOrigin::Model,
            content,
            max_tokens: PLAN_CONTEXT_MAX_TOKENS,
        });
        self.internal_context.plan_age_turns = 0;
    }

    pub fn replace_goal_state(&mut self, content: Option<String>) {
        self.replace_internal_context(
            GOAL_CONTEXT_FRAGMENT_ID,
            InternalContextKind::Goal,
            InternalContextOrigin::GoalRuntime,
            content
                .filter(|text| !text.trim().is_empty())
                .map(|text| format!("[Goal state]\n{text}")),
            GOAL_CONTEXT_MAX_TOKENS,
        );
    }

    pub fn replace_runtime_context(&mut self, content: Option<String>) {
        self.replace_internal_context(
            RUNTIME_CONTEXT_FRAGMENT_ID,
            InternalContextKind::Runtime,
            InternalContextOrigin::System,
            content
                .filter(|text| !text.trim().is_empty())
                .map(|text| format!("[Runtime context]\n{text}")),
            RUNTIME_CONTEXT_MAX_TOKENS,
        );
    }

    pub fn replace_mode_context(&mut self, content: Option<String>) {
        self.replace_internal_context(
            MODE_CONTEXT_FRAGMENT_ID,
            InternalContextKind::Runtime,
            InternalContextOrigin::System,
            content
                .filter(|text| !text.trim().is_empty())
                .map(|text| format!("[Mode context]\n{text}")),
            MODE_CONTEXT_MAX_TOKENS,
        );
    }

    pub fn replace_skill_context(&mut self, content: Option<String>) {
        self.replace_internal_context(
            SKILL_CONTEXT_FRAGMENT_ID,
            InternalContextKind::Skill,
            InternalContextOrigin::User,
            content
                .filter(|text| !text.trim().is_empty())
                .map(|text| format!("[Skill context]\n{text}")),
            SKILL_CONTEXT_MAX_TOKENS,
        );
    }

    pub fn replace_memory_context(&mut self, content: Option<String>) {
        self.replace_internal_context(
            MEMORY_CONTEXT_FRAGMENT_ID,
            InternalContextKind::Memory,
            InternalContextOrigin::System,
            content
                .filter(|text| !text.trim().is_empty())
                .map(|text| format!("[Relevant memory]\n{text}")),
            MEMORY_CONTEXT_MAX_TOKENS,
        );
    }

    pub fn replace_internal_context(
        &mut self,
        id: &str,
        kind: InternalContextKind,
        origin: InternalContextOrigin,
        content: Option<String>,
        max_tokens: usize,
    ) {
        let content = content.filter(|text| !text.trim().is_empty());
        match content {
            Some(content) => self.internal_context.replace(InternalContextFragment {
                id: id.to_string(),
                kind,
                origin,
                content,
                max_tokens: max_tokens.max(1),
            }),
            None => self.internal_context.remove(id),
        }
    }

    pub fn strip_legacy_pinned_volatile(&mut self) {
        self.messages.retain(|msg| {
            if let Message::System {
                content,
                pinned: true,
            } = msg
            {
                !content.starts_with("[Pinned plan state]")
                    && !content.starts_with("[Pinned goal state]")
                    && !content.starts_with("[Pinned skill context]")
            } else {
                true
            }
        });
    }

    pub fn strip_legacy_summary_messages(&mut self) {
        self.messages.retain(|msg| {
            !matches!(
                msg,
                Message::System { content, pinned: false }
                if content.starts_with("[Summary of earlier conversation]")
                || content.starts_with("[Earlier conversation history was truncated")
            )
        });
    }

    pub fn add_user(&mut self, content: String) {
        self.messages.push(Message::user(content));
    }

    pub fn add_user_with_images(&mut self, content: String, images: Vec<ImageInput>) {
        self.messages
            .push(Message::user_with_images(content, images));
    }

    pub fn add_user_pinned(&mut self, content: String) {
        self.messages.push(Message::pinned_user(content));
    }

    pub fn add_assistant(
        &mut self,
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
    ) {
        self.internal_context.note_assistant_turn();
        self.messages.push(Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            pinned: false,
        });
    }

    pub fn add_tool_result(&mut self, tool_call_id: String, content: String) {
        self.messages.push(Message::Tool {
            tool_call_id,
            content,
            terminal: None,
            pinned: false,
        });
    }

    pub fn add_tool_result_with_terminal(&mut self, result: &ToolResult, content: String) {
        self.messages.push(Message::Tool {
            tool_call_id: result.id.clone(),
            content,
            terminal: Some(result.terminal().clone()),
            pinned: false,
        });
    }

    pub fn last_user_message(&self) -> Option<&str> {
        self.messages.iter().rev().find_map(|msg| match msg {
            Message::User { content, .. } => Some(content.as_str()),
            _ => None,
        })
    }

    pub fn backtrack_last_user(&mut self) -> Option<String> {
        let index = self
            .messages
            .iter()
            .rposition(|message| matches!(message, Message::User { pinned: false, .. }))?;
        let prompt = match &self.messages[index] {
            Message::User { content, .. } => content.clone(),
            _ => unreachable!("rposition only matches user messages"),
        };
        self.messages.truncate(index);
        Some(prompt)
    }
}

pub fn assistant_message_has_payload(content: Option<&str>, tool_calls: &[RawToolCall]) -> bool {
    content.is_some_and(|text| !text.trim().is_empty()) || !tool_calls.is_empty()
}

pub fn normalize_tool_boundaries(messages: &mut Vec<Message>) {
    let mut normalized = Vec::with_capacity(messages.len());
    let mut index = 0usize;

    while index < messages.len() {
        match &messages[index] {
            Message::Tool { .. } => {
                index += 1;
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } if !assistant_message_has_payload(content.as_deref(), tool_calls) => {
                index += 1;
            }
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } if !tool_calls.is_empty() => {
                let mut seen_call_ids = HashSet::new();
                let unique_tool_calls = tool_calls
                    .iter()
                    .filter(|tool_call| seen_call_ids.insert(tool_call.id.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                let expected = unique_tool_calls
                    .iter()
                    .map(|tool_call| tool_call.id.as_str())
                    .collect::<HashSet<_>>();
                let mut collected_tools = HashMap::new();
                let mut next = index + 1;

                while next < messages.len() {
                    let Message::Tool { tool_call_id, .. } = &messages[next] else {
                        break;
                    };
                    if expected.contains(tool_call_id.as_str()) {
                        collected_tools
                            .entry(tool_call_id.clone())
                            .or_insert_with(|| messages[next].clone());
                    }
                    next += 1;
                }

                normalized.push(Message::Assistant {
                    content: content.clone(),
                    reasoning_content: reasoning_content.clone(),
                    tool_calls: unique_tool_calls.clone(),
                    pinned: *pinned,
                });
                for tool_call in &unique_tool_calls {
                    normalized.push(
                        collected_tools
                            .remove(&tool_call.id)
                            .unwrap_or_else(|| repaired_missing_tool_result(tool_call)),
                    );
                }
                index = next.max(index + 1);
            }
            message => {
                normalized.push(message.clone());
                index += 1;
            }
        }
    }

    *messages = normalized;
}

pub fn repaired_missing_tool_result(tool_call: &RawToolCall) -> Message {
    let request = ToolRequest {
        id: tool_call.id.clone(),
        name: ToolName::from_str(&tool_call.function_name)
            .unwrap_or_else(|| ToolName::plain(&tool_call.function_name)),
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(tool_call.arguments.clone()),
    };
    let result = ToolResult::indeterminate(&request, MISSING_TOOL_TERMINAL_ERROR)
        .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
    Message::Tool {
        tool_call_id: tool_call.id.clone(),
        content: format!("ERROR: {MISSING_TOOL_TERMINAL_ERROR}"),
        terminal: Some(result.terminal().clone()),
        pinned: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_source_debug_never_exposes_payload_or_remote_identifier() {
        let base64 = ImageSource::Base64 {
            media_type: "image/png".to_string(),
            data: "secret-base64-payload".to_string(),
        };
        let url = ImageSource::Url {
            url: "https://example.com/private?token=secret".to_string(),
        };
        let file = ImageSource::File {
            file_id: "file-secret".to_string(),
        };

        assert!(!format!("{base64:?}").contains("secret-base64-payload"));
        assert!(!format!("{url:?}").contains("token=secret"));
        assert!(!format!("{file:?}").contains("file-secret"));
    }

    #[test]
    fn new_conversation_is_empty() {
        let conv = Conversation::new();
        assert!(conv.messages.is_empty());
    }

    #[test]
    fn add_system_pushes_system_message() {
        let mut conv = Conversation::new();
        conv.add_system("you are helpful".to_string());
        assert!(
            matches!(&conv.messages[0], Message::System { content, .. } if content == "you are helpful")
        );
    }

    #[test]
    fn add_user_pushes_user_message() {
        let mut conv = Conversation::new();
        conv.add_user("hello".to_string());
        assert!(matches!(&conv.messages[0], Message::User { content, .. } if content == "hello"));
    }

    #[test]
    fn add_pinned_user_marks_message_pinned() {
        let mut conv = Conversation::new();
        conv.add_user_pinned("keep this constraint".to_string());

        assert!(conv.messages[0].is_pinned());
    }

    #[test]
    fn replace_goal_state_keeps_single_internal_fragment() {
        let mut conv = Conversation::new();
        conv.replace_goal_state(Some("first".to_string()));
        conv.replace_goal_state(Some("second".to_string()));

        assert!(conv.messages.is_empty());
        let fragment = conv
            .internal_context
            .get(GOAL_CONTEXT_FRAGMENT_ID)
            .expect("goal fragment");
        assert_eq!(fragment.kind, InternalContextKind::Goal);
        assert_eq!(fragment.origin, InternalContextOrigin::GoalRuntime);
        assert!(fragment.content.contains("second"));
        assert!(!fragment.content.contains("first"));
        assert_eq!(conv.internal_context.len(), 1);

        conv.replace_goal_state(None);
        assert!(
            conv.internal_context
                .get(GOAL_CONTEXT_FRAGMENT_ID)
                .is_none()
        );
        conv.replace_goal_state(Some("  ".to_string()));
        assert!(
            conv.internal_context
                .get(GOAL_CONTEXT_FRAGMENT_ID)
                .is_none()
        );
    }

    #[test]
    fn replace_skill_context_updates_internal_fragment() {
        let mut conv = Conversation::new();
        conv.replace_skill_context(Some("first".to_string()));
        conv.replace_skill_context(Some("second".to_string()));

        assert!(conv.messages.is_empty());
        assert!(
            conv.internal_context
                .get(SKILL_CONTEXT_FRAGMENT_ID)
                .unwrap()
                .content
                .contains("second")
        );

        conv.replace_skill_context(None);
        assert!(
            conv.internal_context
                .get(SKILL_CONTEXT_FRAGMENT_ID)
                .is_none()
        );
    }

    #[test]
    fn replace_memory_context_keeps_recalled_memory_out_of_transcript_messages() {
        let mut conv = Conversation::new();
        conv.add_user("prepare a release".to_string());
        conv.replace_memory_context(Some(
            "- Run the workspace verification before publishing.".to_string(),
        ));

        assert_eq!(conv.messages.len(), 1);
        let fragment = conv
            .internal_context
            .get(MEMORY_CONTEXT_FRAGMENT_ID)
            .expect("memory context");
        assert_eq!(fragment.kind, InternalContextKind::Memory);
        assert_eq!(fragment.origin, InternalContextOrigin::System);
        assert!(fragment.content.contains("workspace verification"));

        conv.replace_memory_context(None);
        assert!(
            conv.internal_context
                .get(MEMORY_CONTEXT_FRAGMENT_ID)
                .is_none()
        );
    }

    #[test]
    fn replace_mode_context_is_independent_from_runtime_context() {
        let mut conv = Conversation::new();
        conv.replace_runtime_context(Some("resume hint".to_string()));
        conv.replace_mode_context(Some("plan instructions".to_string()));

        assert!(
            conv.internal_context
                .get(RUNTIME_CONTEXT_FRAGMENT_ID)
                .unwrap()
                .content
                .contains("resume hint")
        );
        assert!(
            conv.internal_context
                .get(MODE_CONTEXT_FRAGMENT_ID)
                .unwrap()
                .content
                .contains("plan instructions")
        );

        conv.replace_mode_context(None);
        assert!(
            conv.internal_context
                .get(MODE_CONTEXT_FRAGMENT_ID)
                .is_none()
        );
        assert!(
            conv.internal_context
                .get(RUNTIME_CONTEXT_FRAGMENT_ID)
                .is_some()
        );
    }

    #[test]
    fn add_assistant_with_content_and_reasoning() {
        let mut conv = Conversation::new();
        conv.add_assistant(
            Some("answer".to_string()),
            Some("thinking".to_string()),
            vec![],
        );
        match &conv.messages[0] {
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                assert_eq!(content.as_deref(), Some("answer"));
                assert_eq!(reasoning_content.as_deref(), Some("thinking"));
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn add_assistant_with_tool_calls() {
        let mut conv = Conversation::new();
        let tc = RawToolCall {
            id: "call_1".to_string(),
            function_name: "read_file".to_string(),
            arguments: r#"{"path":"x.rs"}"#.to_string(),
        };
        conv.add_assistant(None, None, vec![tc]);
        match &conv.messages[0] {
            Message::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].function_name, "read_file");
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn add_tool_result_pushes_tool_message() {
        let mut conv = Conversation::new();
        conv.add_tool_result("call_1".to_string(), "file contents".to_string());
        match &conv.messages[0] {
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(content, "file contents");
            }
            _ => panic!("expected Tool message"),
        }
    }

    #[test]
    fn add_tool_result_with_terminal_preserves_canonical_terminal() {
        let request = ToolRequest {
            id: "call_1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let result = ToolResult::indeterminate(&request, "terminal missing")
            .with_terminal_source(ToolTerminalSource::CompatibilityRepair);
        let mut conv = Conversation::new();

        conv.add_tool_result_with_terminal(&result, "ERROR: terminal missing".to_string());

        assert!(matches!(
            &conv.messages[0],
            Message::Tool { terminal: Some(terminal), .. }
                if terminal == result.terminal()
        ));
    }

    #[test]
    fn last_user_message_returns_most_recent() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(Some("reply".to_string()), None, vec![]);
        conv.add_user("second".to_string());
        assert_eq!(conv.last_user_message(), Some("second"));
    }

    #[test]
    fn last_user_message_returns_none_when_empty() {
        let conv = Conversation::new();
        assert_eq!(conv.last_user_message(), None);
    }

    #[test]
    fn last_user_message_skips_non_user_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_assistant(Some("hi".to_string()), None, vec![]);
        assert_eq!(conv.last_user_message(), None);
    }

    #[test]
    fn backtrack_last_user_removes_user_and_later_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("first".to_string());
        conv.add_assistant(Some("reply".to_string()), None, vec![]);
        conv.add_user("second".to_string());
        conv.add_assistant(Some("later".to_string()), None, vec![]);

        assert_eq!(conv.backtrack_last_user(), Some("second".to_string()));
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.last_user_message(), Some("first"));
    }

    #[test]
    fn backtrack_last_user_skips_pinned_user_context() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("first".to_string());
        conv.add_assistant(Some("reply".to_string()), None, vec![]);
        conv.add_user_pinned("workflow notification".to_string());
        conv.add_assistant(Some("notification reply".to_string()), None, vec![]);

        assert_eq!(conv.backtrack_last_user(), Some("first".to_string()));
        assert_eq!(conv.messages.len(), 1);
    }

    /// DeepSeek prefix cache requires that a normal turn only *appends* to the
    /// conversation: the existing prefix (every earlier message) must stay
    /// byte-identical so the server-side cache keeps hitting. This locks that
    /// `add_*` never rewrites or reorders prior messages.
    #[test]
    fn turn_appends_never_mutate_the_existing_prefix() {
        let mut conv = Conversation::new();
        conv.add_system("system prompt".to_string());
        conv.add_user("first request".to_string());
        conv.add_assistant(
            Some("calling a tool".to_string()),
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x.rs"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file contents".to_string());

        let prefix_snapshot = render_prefix(&conv);

        // A second turn: append a new user message, assistant reply, and tool result.
        conv.add_user("second request".to_string());
        conv.add_assistant(Some("answer".to_string()), None, vec![]);

        // The first four messages must be byte-identical to the snapshot.
        let prefix_after = render_prefix(&conv);
        assert_eq!(&prefix_after[..prefix_snapshot.len()], &prefix_snapshot[..]);
        assert_eq!(prefix_snapshot.len(), 4);
        assert_eq!(prefix_after.len(), 6);
    }

    /// Renders each message into a stable string so prefix equality can be
    /// asserted across mutations.
    fn render_prefix(conv: &Conversation) -> Vec<String> {
        conv.messages
            .iter()
            .map(|message| match message {
                Message::System { content, pinned } => format!("sys|{pinned}|{content}"),
                Message::User {
                    content, pinned, ..
                } => format!("usr|{pinned}|{content}"),
                Message::Assistant {
                    content,
                    reasoning_content,
                    tool_calls,
                    pinned,
                } => format!(
                    "ast|{pinned}|{content:?}|{reasoning_content:?}|{}",
                    tool_calls
                        .iter()
                        .map(|tc| format!("{}:{}:{}", tc.id, tc.function_name, tc.arguments))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                Message::Tool {
                    tool_call_id,
                    content,
                    terminal,
                    pinned,
                } => format!("tool|{pinned}|{tool_call_id}|{content}|{terminal:?}"),
            })
            .collect()
    }

    #[test]
    fn internal_context_updates_never_touch_messages() {
        let mut conv = Conversation::new();
        conv.add_system("system".to_string());
        conv.add_user("hello".to_string());
        conv.add_assistant(Some("reply".to_string()), None, vec![]);

        let snapshot = render_prefix(&conv);

        conv.replace_plan_state("step 1: do X".to_string());
        conv.replace_goal_state(Some("build a widget".to_string()));
        conv.replace_skill_context(Some("rust expertise".to_string()));

        assert_eq!(render_prefix(&conv), snapshot);
        assert_eq!(conv.messages.len(), 3);
        assert!(
            conv.internal_context
                .get(PLAN_CONTEXT_FRAGMENT_ID)
                .is_some()
        );
        assert!(
            conv.internal_context
                .get(GOAL_CONTEXT_FRAGMENT_ID)
                .is_some()
        );
        assert!(
            conv.internal_context
                .get(SKILL_CONTEXT_FRAGMENT_ID)
                .is_some()
        );
    }

    #[test]
    fn multiple_plan_updates_only_change_internal_context_not_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("do work".to_string());

        let snapshot = render_prefix(&conv);

        conv.replace_plan_state("plan v1".to_string());
        conv.replace_plan_state("plan v2".to_string());
        conv.replace_plan_state("plan v3".to_string());

        assert_eq!(render_prefix(&conv), snapshot);
        assert_eq!(
            conv.internal_context
                .get(PLAN_CONTEXT_FRAGMENT_ID)
                .map(|fragment| fragment.content.as_str()),
            Some("plan v3")
        );
    }

    #[test]
    fn stale_plan_with_open_steps_gets_reminder_in_render() {
        let mut conv = Conversation::new();
        conv.replace_plan_state(
            "[Pinned plan state]\n[completed] a\n[in_progress] b\n[pending] c".to_string(),
        );

        for _ in 0..PLAN_REMINDER_AFTER_TURNS - 1 {
            conv.add_assistant(Some("working".to_string()), None, vec![]);
        }
        assert!(
            !conv.internal_context.render().contains("[Plan reminder]"),
            "reminder must not fire before the threshold"
        );

        conv.add_assistant(Some("working".to_string()), None, vec![]);
        assert!(
            conv.internal_context.render().contains("[Plan reminder]"),
            "reminder must fire once the plan has gone stale"
        );
    }

    #[test]
    fn plan_update_resets_staleness_reminder() {
        let mut conv = Conversation::new();
        conv.replace_plan_state("[Pinned plan state]\n[pending] a".to_string());
        for _ in 0..PLAN_REMINDER_AFTER_TURNS {
            conv.add_assistant(None, None, vec![]);
        }
        assert!(conv.internal_context.render().contains("[Plan reminder]"));

        conv.replace_plan_state("[Pinned plan state]\n[in_progress] a".to_string());
        assert!(
            !conv.internal_context.render().contains("[Plan reminder]"),
            "a successful update must clear the reminder"
        );
    }

    #[test]
    fn fully_completed_plan_never_reminds() {
        let mut conv = Conversation::new();
        conv.replace_plan_state("[Pinned plan state]\n[completed] a\n[completed] b".to_string());
        for _ in 0..PLAN_REMINDER_AFTER_TURNS * 2 {
            conv.add_assistant(None, None, vec![]);
        }
        assert!(!conv.internal_context.render().contains("[Plan reminder]"));
    }

    #[test]
    fn assistant_turns_without_plan_do_not_accumulate_age() {
        let mut conv = Conversation::new();
        for _ in 0..PLAN_REMINDER_AFTER_TURNS * 2 {
            conv.add_assistant(None, None, vec![]);
        }
        assert_eq!(conv.internal_context.plan_age_turns, 0);

        conv.replace_plan_state("[Pinned plan state]\n[pending] a".to_string());
        assert!(!conv.internal_context.render().contains("[Plan reminder]"));
    }

    #[test]
    fn strip_legacy_pinned_volatile_removes_old_format_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("hello".to_string());
        conv.messages.push(Message::pinned_system(
            "[Pinned plan state]\nold plan".to_string(),
        ));
        conv.messages.push(Message::pinned_system(
            "[Pinned goal state]\nold goal".to_string(),
        ));
        conv.messages.push(Message::pinned_system(
            "[Pinned skill context]\nold skill".to_string(),
        ));
        conv.add_assistant(Some("reply".to_string()), None, vec![]);

        assert_eq!(conv.messages.len(), 6);
        conv.strip_legacy_pinned_volatile();
        assert_eq!(conv.messages.len(), 3);
        assert!(
            matches!(&conv.messages[0], Message::System { content, pinned: false } if content == "sys")
        );
        assert!(matches!(&conv.messages[1], Message::User { content, .. } if content == "hello"));
        assert!(matches!(&conv.messages[2], Message::Assistant { .. }));
    }

    #[test]
    fn strip_legacy_preserves_non_volatile_pinned() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user_pinned("important constraint".to_string());
        conv.messages.push(Message::pinned_system(
            "[Pinned plan state]\nold".to_string(),
        ));
        conv.messages.push(Message::pinned_system(
            "[Hook context]\nkeep this".to_string(),
        ));

        conv.strip_legacy_pinned_volatile();
        assert_eq!(conv.messages.len(), 3);
        assert!(conv.messages[1].is_pinned());
        assert!(
            matches!(&conv.messages[2], Message::System { content, pinned: true } if content.contains("Hook context"))
        );
    }

    #[test]
    fn normalize_tool_boundaries_repairs_missing_results() {
        let mut messages = vec![
            Message::user("before".to_string()),
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RawToolCall {
                    id: "call_1".to_string(),
                    function_name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
                pinned: false,
            },
            Message::user("after".to_string()),
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[0], Message::User { content, .. } if content == "before"));
        assert!(
            matches!(&messages[1], Message::Assistant { tool_calls, .. } if tool_calls.len() == 1)
        );
        assert!(matches!(
            &messages[2],
            Message::Tool {
                tool_call_id,
                terminal: Some(terminal),
                ..
            } if tool_call_id == "call_1"
                && terminal.status == crate::tool_types::ToolStatus::Indeterminate
                && terminal.source == crate::tool_types::ToolTerminalSource::CompatibilityRepair
        ));
        assert!(matches!(&messages[3], Message::User { content, .. } if content == "after"));
    }

    #[test]
    fn normalize_tool_boundaries_drops_reasoning_only_assistant() {
        let mut messages = vec![
            Message::user("before".to_string()),
            Message::Assistant {
                content: None,
                reasoning_content: Some("private reasoning".to_string()),
                tool_calls: vec![],
                pinned: false,
            },
            Message::user("after".to_string()),
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::User { content, .. } if content == "before"));
        assert!(matches!(&messages[1], Message::User { content, .. } if content == "after"));
    }

    #[test]
    fn assistant_message_payload_requires_content_or_tool_calls() {
        assert!(!assistant_message_has_payload(None, &[]));
        assert!(!assistant_message_has_payload(Some("  \n"), &[]));
        assert!(assistant_message_has_payload(Some("answer"), &[]));
        assert!(assistant_message_has_payload(
            None,
            &[RawToolCall {
                id: "call_1".to_string(),
                function_name: "read_file".to_string(),
                arguments: "{}".to_string(),
            }],
        ));
    }

    #[test]
    fn normalize_tool_boundaries_keeps_complete_assistant_tool_call() {
        let mut messages = vec![
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RawToolCall {
                    id: "call_1".to_string(),
                    function_name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "ok".to_string(),
                terminal: None,
                pinned: false,
            },
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            Message::Assistant { tool_calls, .. } if tool_calls.len() == 1
        ));
        assert!(
            matches!(&messages[1], Message::Tool { tool_call_id, .. } if tool_call_id == "call_1")
        );
    }

    #[test]
    fn normalize_tool_boundaries_orders_results_and_discards_duplicate_orphans() {
        let mut messages = vec![
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![
                    RawToolCall {
                        id: "call_1".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: "{}".to_string(),
                    },
                    RawToolCall {
                        id: "call_2".to_string(),
                        function_name: "grep".to_string(),
                        arguments: "{}".to_string(),
                    },
                ],
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "orphan".to_string(),
                content: "discard me".to_string(),
                terminal: None,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_2".to_string(),
                content: "existing second".to_string(),
                terminal: None,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_2".to_string(),
                content: "duplicate second".to_string(),
                terminal: None,
                pinned: false,
            },
            Message::user("after".to_string()),
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 4);
        assert!(
            matches!(&messages[1], Message::Tool { tool_call_id, terminal: Some(_), .. } if tool_call_id == "call_1")
        );
        assert!(
            matches!(&messages[2], Message::Tool { tool_call_id, content, terminal: None, .. }
            if tool_call_id == "call_2" && content == "existing second")
        );
        assert!(matches!(&messages[3], Message::User { content, .. } if content == "after"));

        let once = format!("{messages:?}");
        normalize_tool_boundaries(&mut messages);
        assert_eq!(format!("{messages:?}"), once);
    }

    #[test]
    fn normalize_tool_boundaries_keeps_first_duplicate_assistant_call_id() {
        let mut messages = vec![
            Message::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![
                    RawToolCall {
                        id: "call_1".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: r#"{"path":"first"}"#.to_string(),
                    },
                    RawToolCall {
                        id: "call_1".to_string(),
                        function_name: "write_file".to_string(),
                        arguments: r#"{"path":"second"}"#.to_string(),
                    },
                ],
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "first result".to_string(),
                terminal: None,
                pinned: false,
            },
        ];

        normalize_tool_boundaries(&mut messages);

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            Message::Assistant { tool_calls, .. }
                if tool_calls.len() == 1
                    && tool_calls[0].function_name == "read_file"
                    && tool_calls[0].arguments == r#"{"path":"first"}"#
        ));
        assert!(matches!(
            &messages[1],
            Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "call_1" && content == "first result"
        ));
    }
}
