use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation::{Message, RawToolCall};
use crate::proposed_plan::{ProposedPlanSegment, ProposedPlanStreamParser};
use crate::thread_identity::{ConversationItemId, TurnId};
use crate::tool_types::{ToolInvocationStarted, ToolResultKind, ToolTerminal, ToolTerminalSource};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectedToolTerminalMetadata {
    #[serde(rename = "kind")]
    pub result_kind: ToolResultKind,
    #[serde(rename = "terminalSource", skip_serializing_if = "Option::is_none")]
    pub terminal_source: Option<ToolTerminalSource>,
    #[serde(rename = "invocationStarted", skip_serializing_if = "Option::is_none")]
    pub invocation_started: Option<ToolInvocationStarted>,
}

impl From<&ToolTerminal> for ProjectedToolTerminalMetadata {
    fn from(terminal: &ToolTerminal) -> Self {
        Self {
            result_kind: terminal.kind,
            terminal_source: (terminal.source != ToolTerminalSource::Observed)
                .then_some(terminal.source),
            invocation_started: (terminal.started != ToolInvocationStarted::Unknown)
                .then_some(terminal.started),
        }
    }
}

impl ProjectedToolTerminalMetadata {
    pub fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected tool terminal metadata serializes")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ProjectedUserMessageThreadItem {
    #[serde(rename = "user_message")]
    Started { role: &'static str, content: String },
}

impl ProjectedUserMessageThreadItem {
    pub fn new(content: impl Into<String>) -> Self {
        Self::Started {
            role: "user",
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ProjectedPersistedMessageThreadItem {
    System {
        role: &'static str,
        content: String,
    },
    User {
        role: &'static str,
        content: String,
    },
    Assistant {
        role: &'static str,
        content: Option<String>,
        #[serde(rename = "reasoningContent")]
        reasoning_content: Option<String>,
        #[serde(rename = "toolCalls")]
        tool_calls: Vec<Value>,
    },
    Tool {
        role: &'static str,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: String,
    },
}

impl ProjectedPersistedMessageThreadItem {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            role: "system",
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            role: "user",
            content: content.into(),
        }
    }

    pub fn assistant(
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<Value>,
    ) -> Self {
        Self::Assistant {
            role: "assistant",
            content,
            reasoning_content,
            tool_calls,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            role: "tool",
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum CompletedModelItem {
    #[serde(rename = "agent_message")]
    AgentMessage {
        id: ConversationItemId,
        text: String,
    },
    #[serde(rename = "plan")]
    Plan {
        id: ConversationItemId,
        text: String,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: ConversationItemId,
        summary: String,
        content: String,
    },
}

impl CompletedModelItem {
    pub fn agent_message(id: ConversationItemId, text: impl Into<String>) -> Self {
        Self::AgentMessage {
            id,
            text: text.into(),
        }
    }

    pub fn plan(id: ConversationItemId, text: impl Into<String>) -> Self {
        Self::Plan {
            id,
            text: text.into(),
        }
    }

    pub fn reasoning(id: ConversationItemId, summary: impl Into<String>) -> Self {
        Self::Reasoning {
            id,
            summary: summary.into(),
            content: String::new(),
        }
    }

    pub fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected text thread item serializes")
    }

    pub fn id(&self) -> &ConversationItemId {
        match self {
            Self::AgentMessage { id, .. } | Self::Plan { id, .. } | Self::Reasoning { id, .. } => {
                id
            }
        }
    }

    pub fn started_item(&self) -> Self {
        match self {
            Self::AgentMessage { id, .. } => Self::agent_message(id.clone(), ""),
            Self::Plan { id, .. } => Self::plan(id.clone(), ""),
            Self::Reasoning { id, .. } => Self::reasoning(id.clone(), ""),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelResponseItemIds {
    pub conversation_item_id: ConversationItemId,
    pub plan_item_id: ConversationItemId,
    pub reasoning_item_id: ConversationItemId,
}

impl ModelResponseItemIds {
    pub fn new() -> Self {
        Self {
            conversation_item_id: ConversationItemId::new(),
            plan_item_id: ConversationItemId::new(),
            reasoning_item_id: ConversationItemId::new(),
        }
    }

    pub fn agent_message_item_id(&self) -> &ConversationItemId {
        &self.conversation_item_id
    }
}

impl Default for ModelResponseItemIds {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelResponseIdentity {
    pub turn_id: TurnId,
    pub item_ids: ModelResponseItemIds,
}

impl ModelResponseIdentity {
    pub fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            item_ids: ModelResponseItemIds::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletedModelResponse {
    pub identity: ModelResponseIdentity,
    pub assistant_content: Option<String>,
    pub assistant_reasoning: Option<String>,
    pub tool_calls: Vec<RawToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_short: Option<crate::conversation::CutShort>,
}

impl CompletedModelResponse {
    pub fn new(
        identity: ModelResponseIdentity,
        assistant_content: Option<String>,
        assistant_reasoning: Option<String>,
        tool_calls: Vec<RawToolCall>,
    ) -> Self {
        Self {
            identity,
            assistant_content,
            assistant_reasoning,
            tool_calls,
            cut_short: None,
        }
    }

    pub fn conversation_item_id(&self) -> &ConversationItemId {
        &self.identity.item_ids.conversation_item_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.identity.turn_id
    }

    pub fn assistant_message(&self) -> Message {
        Message::Assistant {
            content: self.assistant_content.clone(),
            reasoning_content: self.assistant_reasoning.clone(),
            tool_calls: self.tool_calls.clone(),
            cut_short: self.cut_short,
            pinned: false,
        }
    }

    pub fn completed_items(&self) -> Vec<CompletedModelItem> {
        let mut agent_text = String::new();
        let mut plan_text = String::new();
        if let Some(content) = self.assistant_content.as_deref() {
            let mut parser = ProposedPlanStreamParser::default();
            let mut segments = parser.push(content);
            segments.extend(parser.finish());
            for segment in segments {
                match segment {
                    ProposedPlanSegment::Agent(text) => agent_text.push_str(&text),
                    ProposedPlanSegment::Plan(text) => plan_text.push_str(&text),
                }
            }
        }

        let mut items = Vec::new();
        if !agent_text.is_empty() {
            items.push(CompletedModelItem::agent_message(
                self.identity.item_ids.agent_message_item_id().clone(),
                agent_text,
            ));
        }
        if !plan_text.is_empty() {
            items.push(CompletedModelItem::plan(
                self.identity.item_ids.plan_item_id.clone(),
                plan_text,
            ));
        }
        if let Some(reasoning) = self
            .assistant_reasoning
            .as_deref()
            .filter(|reasoning| !reasoning.is_empty())
        {
            items.push(CompletedModelItem::reasoning(
                self.identity.item_ids.reasoning_item_id.clone(),
                reasoning,
            ));
        }
        items
    }
}
