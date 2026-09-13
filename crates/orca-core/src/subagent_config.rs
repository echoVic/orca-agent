use crate::config::DelegationSnapshot;
use crate::subagent_types::agent_definition::EffectiveAgentDefinition;
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_SUBAGENT_DEPTH: u32 = 2;
pub const DEFAULT_MAX_PARALLEL_SUBAGENTS: usize = 6;

/// How proactively the agent may start child agents.
///
/// This is deliberately separate from the approval mode: approval mode grants
/// permission to execute tools, it never asks for more delegation. Delegation
/// policy narrows what the model is told to do, and the runtime still enforces
/// permissions, depth, capacity, and budget on top of it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationPolicy {
    /// No new child agents may be started. Existing children stay queryable,
    /// stoppable, and resumable.
    Off,
    /// Only start a child when the user explicitly asked for delegation.
    #[default]
    Explicit,
    /// Split off independent, well-bounded branches inside the authorized task
    /// while the main agent keeps the critical path.
    Adaptive,
}

impl DelegationPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Explicit => "explicit",
            Self::Adaptive => "adaptive",
        }
    }

    /// Whether a new child agent may be started at all.
    pub fn allows_new_children(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Whether the model is told to look for delegable branches on its own.
    pub fn is_proactive(&self) -> bool {
        matches!(self, Self::Adaptive)
    }

    /// The effective policy for a child at `depth`.
    ///
    /// `max_depth` already bounds nesting; beyond it a child must not be told
    /// it can delegate, and it must not be able to start one.
    pub fn for_child(&self, child_depth: u32, max_depth: u32) -> Self {
        if child_depth >= max_depth {
            Self::Off
        } else {
            *self
        }
    }
}

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
    /// How proactively the agent may start child agents.
    #[serde(default)]
    pub delegation: DelegationPolicy,
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
            delegation: DelegationPolicy::default(),
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
            delegation: DelegationPolicy::Adaptive,
            effective_definition: None,
            inherited_tools: None,
        }
        .normalized();
        assert_eq!(config.max_depth, 3);
        assert_eq!(config.max_parallel, 1);
        assert_eq!(config.delegation, DelegationPolicy::Adaptive);
    }

    #[test]
    fn delegation_defaults_to_explicit_so_existing_sessions_do_not_change() {
        assert_eq!(DelegationPolicy::default(), DelegationPolicy::Explicit);
        assert_eq!(
            SubagentConfig::default().delegation,
            DelegationPolicy::Explicit
        );
        assert!(!DelegationPolicy::Explicit.is_proactive());
        assert!(DelegationPolicy::Explicit.allows_new_children());
        assert!(!DelegationPolicy::Off.allows_new_children());
        assert!(DelegationPolicy::Adaptive.is_proactive());
    }

    #[test]
    fn delegation_is_closed_at_the_depth_ceiling() {
        assert_eq!(
            DelegationPolicy::Adaptive.for_child(1, 2),
            DelegationPolicy::Adaptive
        );
        assert_eq!(
            DelegationPolicy::Adaptive.for_child(2, 2),
            DelegationPolicy::Off
        );
        assert_eq!(DelegationPolicy::Off.for_child(0, 2), DelegationPolicy::Off);
    }

    #[test]
    fn delegation_policy_uses_the_documented_config_spellings() {
        for (value, expected) in [
            ("\"off\"", DelegationPolicy::Off),
            ("\"explicit\"", DelegationPolicy::Explicit),
            ("\"adaptive\"", DelegationPolicy::Adaptive),
        ] {
            let decoded: DelegationPolicy = serde_json::from_str(value).unwrap();
            assert_eq!(decoded, expected, "{value}");
        }
        assert_eq!(DelegationPolicy::Adaptive.as_str(), "adaptive");
    }
}
