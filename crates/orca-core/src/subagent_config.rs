use crate::config::DelegationSnapshot;
use crate::subagent_types::agent_definition::EffectiveAgentDefinition;
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_SUBAGENT_DEPTH: u32 = 2;
pub const DEFAULT_MAX_PARALLEL_SUBAGENTS: usize = 6;

/// Persisted with the launch and continuation, never reconstructed from agent files.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FrozenAgentConfig {
    pub definition: EffectiveAgentDefinition,
    pub delegation: DelegationSnapshot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SubagentConfig {
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,
    #[serde(default = "default_stream_progress")]
    pub stream_progress: bool,
    /// Runtime-owned snapshot; project/user config files cannot inject this field.
    #[serde(skip)]
    pub effective_definition: Option<EffectiveAgentDefinition>,
    /// Admitting turn's tool policy, carried only to child admission.
    #[serde(skip)]
    pub inherited_tools: Option<Vec<String>>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            max_parallel: DEFAULT_MAX_PARALLEL_SUBAGENTS,
            stream_progress: true,
            effective_definition: None,
            inherited_tools: None,
        }
    }
}

impl SubagentConfig {
    pub fn normalized(mut self) -> Self {
        if self.max_parallel == 0 {
            self.max_parallel = 1;
        }
        self
    }
}

fn default_max_depth() -> u32 {
    DEFAULT_MAX_SUBAGENT_DEPTH
}

fn default_max_parallel() -> usize {
    DEFAULT_MAX_PARALLEL_SUBAGENTS
}

fn default_stream_progress() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allows_nested_parallel_subagents() {
        let config = SubagentConfig::default();
        assert_eq!(config.max_depth, 2);
        assert_eq!(config.max_parallel, 6);
        assert!(config.stream_progress);
    }

    #[test]
    fn normalized_keeps_parallel_at_least_one() {
        let config = SubagentConfig {
            max_depth: 3,
            max_parallel: 0,
            stream_progress: true,
            effective_definition: None,
            inherited_tools: None,
        }
        .normalized();
        assert_eq!(config.max_depth, 3);
        assert_eq!(config.max_parallel, 1);
    }
}
