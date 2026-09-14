use crate::config::DelegationSnapshot;
use crate::subagent_types::agent_definition::EffectiveAgentDefinition;
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_SUBAGENT_DEPTH: u32 = 2;
/// Execution leases one root task tree may hold at once.
///
/// This is a capacity ceiling, not a per-task delegation target: a task is
/// never expected to fill it, and reaching it queues rather than fails.
pub const DEFAULT_MAX_RUNNING_SUBAGENTS: usize = 32;
/// Accepted-but-not-yet-started tasks one root task tree may hold.
pub const DEFAULT_MAX_QUEUED_SUBAGENTS: usize = 256;
/// Non-terminal child tasks one root task tree may hold.
///
/// Deliberately larger than `max_running + max_queued`: a child that released
/// its execution lease while waiting for its own children is neither running
/// nor waiting to start, and must still be accounted for.
pub const DEFAULT_MAX_LIVE_SUBAGENT_TASKS: usize = 512;
/// Model turns a built-in read-only child may spend gathering evidence before
/// the runtime removes its tools and requests one final report.
pub const DEFAULT_MAX_INVESTIGATION_TURNS: u32 = 6;
/// Tool calls a built-in read-only child may execute before the runtime closes
/// the current tool batch and requests one final report.
pub const DEFAULT_MAX_INVESTIGATION_TOOL_CALLS: u32 = 8;

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
    Explicit,
    /// Split off independent, well-bounded branches inside the authorized task
    /// while the main agent keeps the critical path.
    #[default]
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

/// Scope-limited capacity for one root task tree.
///
/// The three limits are independent on purpose. `max_running` bounds execution
/// leases; `max_queued` bounds accepted work that has not started; and
/// `max_live_tasks` bounds every non-terminal child, including one that
/// released its lease while waiting for its own children. Without the third
/// limit a tree could release execution capacity and still accumulate
/// unbounded processes and conversations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentLimits {
    #[serde(default = "default_max_running")]
    pub max_running: usize,
    #[serde(default = "default_max_queued")]
    pub max_queued: usize,
    #[serde(default = "default_max_live_tasks")]
    pub max_live_tasks: usize,
}

impl Default for SubagentLimits {
    fn default() -> Self {
        Self {
            max_running: DEFAULT_MAX_RUNNING_SUBAGENTS,
            max_queued: DEFAULT_MAX_QUEUED_SUBAGENTS,
            max_live_tasks: DEFAULT_MAX_LIVE_SUBAGENT_TASKS,
        }
    }
}

impl SubagentLimits {
    /// Every limit must be a positive integer. `delegation = "off"` is how a
    /// namespace turns new delegation off; a capacity of zero would silently
    /// mean the same thing while looking like a bound.
    pub fn normalized(mut self) -> Self {
        self.max_running = self.max_running.max(1);
        self.max_queued = self.max_queued.max(1);
        self.max_live_tasks = self.max_live_tasks.max(1);
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SubagentConfig {
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Scope-limited capacity; see [`SubagentLimits`].
    #[serde(default, flatten)]
    pub limits: SubagentLimits,
    #[serde(default = "default_stream_progress")]
    pub stream_progress: bool,
    /// How proactively the agent may start child agents.
    #[serde(default)]
    pub delegation: DelegationPolicy,
    /// Per-attempt convergence bound for built-in read-only roles. The final
    /// summary turn is admitted separately and receives no tools.
    #[serde(default = "default_max_investigation_turns")]
    pub max_investigation_turns: u32,
    /// Per-attempt read-only tool bound. Calls beyond this value are closed as
    /// not started, preserving a settled conversation for the summary turn.
    #[serde(default = "default_max_investigation_tool_calls")]
    pub max_investigation_tool_calls: u32,
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
            limits: SubagentLimits::default(),
            stream_progress: true,
            delegation: DelegationPolicy::default(),
            max_investigation_turns: DEFAULT_MAX_INVESTIGATION_TURNS,
            max_investigation_tool_calls: DEFAULT_MAX_INVESTIGATION_TOOL_CALLS,
            effective_definition: None,
            inherited_tools: None,
        }
    }
}

impl SubagentConfig {
    pub fn normalized(mut self) -> Self {
        self.limits = self.limits.normalized();
        self.max_investigation_turns = self.max_investigation_turns.max(1);
        self.max_investigation_tool_calls = self.max_investigation_tool_calls.max(1);
        self
    }

    /// Execution leases this scope may hold at once.
    pub fn max_running(&self) -> usize {
        self.limits.normalized().max_running
    }

    /// Accepted work waiting for an execution lease.
    pub fn max_queued(&self) -> usize {
        self.limits.normalized().max_queued
    }

    /// Every non-terminal child in this scope.
    pub fn max_live_tasks(&self) -> usize {
        self.limits.normalized().max_live_tasks
    }
}

fn default_max_depth() -> u32 {
    DEFAULT_MAX_SUBAGENT_DEPTH
}

fn default_max_running() -> usize {
    DEFAULT_MAX_RUNNING_SUBAGENTS
}

fn default_max_queued() -> usize {
    DEFAULT_MAX_QUEUED_SUBAGENTS
}

fn default_max_live_tasks() -> usize {
    DEFAULT_MAX_LIVE_SUBAGENT_TASKS
}

fn default_stream_progress() -> bool {
    true
}

fn default_max_investigation_turns() -> u32 {
    DEFAULT_MAX_INVESTIGATION_TURNS
}

fn default_max_investigation_tool_calls() -> u32 {
    DEFAULT_MAX_INVESTIGATION_TOOL_CALLS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_scope_limited_product_decision() {
        let config = SubagentConfig::default();
        assert_eq!(config.max_depth, 2);
        assert_eq!(config.max_running(), 32);
        assert_eq!(config.max_queued(), 256);
        assert_eq!(config.max_live_tasks(), 512);
        assert_eq!(config.max_investigation_turns, 6);
        assert_eq!(config.max_investigation_tool_calls, 8);
        assert!(config.stream_progress);
    }

    #[test]
    fn delegation_defaults_to_adaptive() {
        assert_eq!(DelegationPolicy::default(), DelegationPolicy::Adaptive);
        assert_eq!(
            SubagentConfig::default().delegation,
            DelegationPolicy::Adaptive
        );
        assert!(DelegationPolicy::Adaptive.is_proactive());
        assert!(DelegationPolicy::Adaptive.allows_new_children());
        assert!(!DelegationPolicy::Off.allows_new_children());
        assert!(!DelegationPolicy::Explicit.is_proactive());
    }

    #[test]
    fn limits_must_be_positive() {
        let limits = SubagentLimits {
            max_running: 0,
            max_queued: 0,
            max_live_tasks: 0,
        }
        .normalized();
        assert_eq!(limits.max_running, 1);
        assert_eq!(limits.max_queued, 1);
        assert_eq!(limits.max_live_tasks, 1);
    }

    #[test]
    fn investigation_convergence_limits_must_be_positive() {
        let config = SubagentConfig {
            max_investigation_turns: 0,
            max_investigation_tool_calls: 0,
            ..SubagentConfig::default()
        }
        .normalized();
        assert_eq!(config.max_investigation_turns, 1);
        assert_eq!(config.max_investigation_tool_calls, 1);
    }

    #[test]
    fn live_task_limit_dominates_running_plus_queued() {
        let config = SubagentConfig::default();
        assert!(
            config.max_live_tasks() >= config.max_running() + config.max_queued(),
            "a waiting child is neither running nor queued and still needs a bound"
        );
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
