use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const AGENT_EVENT_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Queued,
    Running,
    WaitingPermission,
    Completed,
    Failed,
    Cancelled,
    Corrupt,
}

impl AgentStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::WaitingPermission)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub cost_micro_usd: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentActivity {
    Starting,
    Thinking,
    Tool {
        name: String,
        target: Option<String>,
    },
    Checkpointing,
    WaitingPermission {
        description: String,
    },
}

impl AgentActivity {
    pub fn label(&self) -> String {
        match self {
            Self::Starting => "starting".to_string(),
            Self::Thinking => "thinking".to_string(),
            Self::Tool { name, target } => target
                .as_deref()
                .map(|target| format!("{name}: {target}"))
                .unwrap_or_else(|| name.clone()),
            Self::Checkpointing => "checkpointing".to_string(),
            Self::WaitingPermission { description } => {
                format!("permission: {description}")
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Spawned {
        batch_id: String,
        batch_size: u32,
        parent_thread_id: String,
        description: String,
    },
    Activity {
        activity: AgentActivity,
        turn: Option<u32>,
        usage: Option<AgentUsage>,
    },
    OutputDelta {
        text: String,
    },
    PermissionRequested {
        description: String,
    },
    Completed {
        result: Option<String>,
        usage: AgentUsage,
    },
    Failed {
        reason: String,
        usage: AgentUsage,
    },
    Cancelled {
        reason: String,
        usage: AgentUsage,
    },
    Corrupt {
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentEventEnvelope {
    pub schema_version: u16,
    pub event_id: String,
    pub root_thread_id: String,
    pub agent_id: String,
    pub thread_id: String,
    pub attempt_id: String,
    pub source_sequence: u64,
    pub occurred_at_ms: i64,
    pub event: AgentEvent,
    pub payload_digest: String,
}

impl AgentEventEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_id: impl Into<String>,
        root_thread_id: impl Into<String>,
        agent_id: impl Into<String>,
        thread_id: impl Into<String>,
        attempt_id: impl Into<String>,
        source_sequence: u64,
        occurred_at_ms: i64,
        event: AgentEvent,
    ) -> Self {
        let mut envelope = Self {
            schema_version: AGENT_EVENT_SCHEMA_VERSION,
            event_id: event_id.into(),
            root_thread_id: root_thread_id.into(),
            agent_id: agent_id.into(),
            thread_id: thread_id.into(),
            attempt_id: attempt_id.into(),
            source_sequence,
            occurred_at_ms,
            event,
            payload_digest: String::new(),
        };
        envelope.payload_digest = envelope.compute_digest();
        envelope
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != AGENT_EVENT_SCHEMA_VERSION {
            return Err("unsupported agent event schema");
        }
        if self.event_id.trim().is_empty()
            || self.root_thread_id.trim().is_empty()
            || self.agent_id.trim().is_empty()
            || self.thread_id.trim().is_empty()
            || self.attempt_id.trim().is_empty()
        {
            return Err("agent event identity is empty");
        }
        if self.source_sequence == 0 {
            return Err("agent event source sequence must be non-zero");
        }
        if self.payload_digest != self.compute_digest() {
            return Err("agent event payload digest does not match");
        }
        Ok(())
    }

    pub fn compute_digest(&self) -> String {
        #[derive(Serialize)]
        struct DigestInput<'a> {
            schema_version: u16,
            event_id: &'a str,
            root_thread_id: &'a str,
            agent_id: &'a str,
            thread_id: &'a str,
            attempt_id: &'a str,
            source_sequence: u64,
            occurred_at_ms: i64,
            event: &'a AgentEvent,
        }

        let input = DigestInput {
            schema_version: self.schema_version,
            event_id: &self.event_id,
            root_thread_id: &self.root_thread_id,
            agent_id: &self.agent_id,
            thread_id: &self.thread_id,
            attempt_id: &self.attempt_id,
            source_sequence: self.source_sequence,
            occurred_at_ms: self.occurred_at_ms,
            event: &self.event,
        };
        let encoded = serde_json::to_vec(&input).expect("agent event digest input is serializable");
        let digest = Sha256::digest(encoded);
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentSummary {
    pub root_thread_id: String,
    pub batch_id: String,
    pub batch_size: u32,
    pub agent_id: String,
    pub thread_id: String,
    pub parent_thread_id: String,
    pub description: String,
    pub status: AgentStatus,
    pub activity: Option<AgentActivity>,
    pub turn: Option<u32>,
    pub usage: AgentUsage,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRegistrySnapshot {
    pub revision: u64,
    pub agents: Vec<AgentSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_for_fixed_point_costs() {
        let event = AgentEventEnvelope::new(
            "event-1",
            "root",
            "agent",
            "thread",
            "attempt",
            1,
            42,
            AgentEvent::Completed {
                result: Some("done".to_string()),
                usage: AgentUsage {
                    cost_micro_usd: 19_390,
                    ..AgentUsage::default()
                },
            },
        );
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: AgentEventEnvelope = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.payload_digest, event.payload_digest);
        assert_eq!(decoded.validate(), Ok(()));
    }
}
