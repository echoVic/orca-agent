//! Execution budget protocol: pure types for budget specs, usage accounting,
//! and the one typed operation terminal every surface consumes.
//!
//! The redesign contract (see `docs/superpowers/specs/2026-08-12-execution-budget-redesign.md`):
//! - Budget dimensions are independently optional; `None` means unlimited.
//! - `ModelEnded` is normal completion; budget/safety stops are typed
//!   non-success terminals (`OperationTerminal::Stopped`).
//! - `resumable` is true only after a committed conversation boundary exists.
//! - Verifier success never upgrades a budget stop.

use serde::{Deserialize, Serialize};

/// Independently optional budget dimensions for one operation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", default)]
pub struct BudgetSpec {
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_cost_usd_micros: Option<u64>,
    pub max_wall_time_ms: Option<u64>,
}

impl BudgetSpec {
    /// True when every dimension is unlimited (the default contract).
    pub fn is_unlimited(&self) -> bool {
        self.max_turns.is_none()
            && self.max_tool_calls.is_none()
            && self.max_cost_usd_micros.is_none()
            && self.max_wall_time_ms.is_none()
    }

    /// Validates that every present dimension is a positive value.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(max_turns) = self.max_turns
            && max_turns == 0
        {
            return Err("max_turns must be positive".to_string());
        }
        if let Some(max_tool_calls) = self.max_tool_calls
            && max_tool_calls == 0
        {
            return Err("max_tool_calls must be positive".to_string());
        }
        if let Some(max_cost_usd_micros) = self.max_cost_usd_micros
            && max_cost_usd_micros == 0
        {
            return Err("max_cost_usd_micros must be positive".to_string());
        }
        if let Some(max_wall_time_ms) = self.max_wall_time_ms
            && max_wall_time_ms == 0
        {
            return Err("max_wall_time_ms must be positive".to_string());
        }
        Ok(())
    }
}

/// Cumulative usage accounting for one operation. All counters saturate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", default)]
pub struct BudgetUsage {
    pub turns: u32,
    pub tool_calls: u32,
    pub cost_usd_micros: u64,
    pub wall_time_ms: u64,
}

impl BudgetUsage {
    pub fn add_turn(&mut self) {
        self.turns = self.turns.saturating_add(1);
    }

    pub fn add_tool_call(&mut self) {
        self.tool_calls = self.tool_calls.saturating_add(1);
    }

    pub fn add_cost_usd_micros(&mut self, cost_usd_micros: u64) {
        self.cost_usd_micros = self.cost_usd_micros.saturating_add(cost_usd_micros);
    }

    pub fn add_wall_time_ms(&mut self, wall_time_ms: u64) {
        self.wall_time_ms = self.wall_time_ms.saturating_add(wall_time_ms);
    }

    /// Merges another usage snapshot into this one, saturating every counter.
    pub fn merge(&mut self, other: BudgetUsage) {
        self.turns = self.turns.saturating_add(other.turns);
        self.tool_calls = self.tool_calls.saturating_add(other.tool_calls);
        self.cost_usd_micros = self.cost_usd_micros.saturating_add(other.cost_usd_micros);
        self.wall_time_ms = self.wall_time_ms.saturating_add(other.wall_time_ms);
    }
}

/// Why a budget-bounded operation stopped before natural completion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    TurnBudget { max_turns: u32 },
    ToolCallBudget { max_tool_calls: u32 },
    CostBudget { max_cost_usd_micros: u64 },
    WallTimeBudget { max_wall_time_ms: u64 },
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TurnBudget { .. } => "turn_budget",
            Self::ToolCallBudget { .. } => "tool_call_budget",
            Self::CostBudget { .. } => "cost_budget",
            Self::WallTimeBudget { .. } => "wall_time_budget",
        }
    }
}

/// Class of a failed operation, kept separate from budget stops.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Provider,
    Tool,
    Hook,
    Workflow,
    Verification,
    Runtime,
    Persistence,
    ExternalEffectAmbiguous,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Tool => "tool",
            Self::Hook => "hook",
            Self::Workflow => "workflow",
            Self::Verification => "verification",
            Self::Runtime => "runtime",
            Self::Persistence => "persistence",
            Self::ExternalEffectAmbiguous => "external_effect_ambiguous",
        }
    }
}

/// Why an operation was cancelled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    User,
    Parent,
    GoalPause,
    System,
}

impl CancelReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Parent => "parent",
            Self::GoalPause => "goal_pause",
            Self::System => "system",
        }
    }
}

/// The one typed terminal every projection (TUI, JSONL, history, Goal,
/// Harbor) consumes. Adapters must not reconstruct terminal facts from
/// constants or status strings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationTerminal {
    /// ModelEnded: natural completion.
    Completed { usage: BudgetUsage },
    /// A budget or safety stop. `resumable` is true only after a committed
    /// conversation boundary (`checkpoint.created`) exists.
    Stopped {
        reason: StopReason,
        usage: BudgetUsage,
        checkpoint_id: String,
        resumable: bool,
    },
    Failed {
        class: FailureClass,
        message: String,
    },
    Cancelled {
        reason: CancelReason,
        checkpoint_id: Option<String>,
    },
}

impl OperationTerminal {
    pub fn usage(&self) -> Option<BudgetUsage> {
        match self {
            Self::Completed { usage } | Self::Stopped { usage, .. } => Some(*usage),
            Self::Failed { .. } | Self::Cancelled { .. } => None,
        }
    }

    /// Exit-code contribution for this terminal, mirroring the legacy
    /// `RunStatus::exit_code` mapping (budget stops keep exit code 4).
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Completed { .. } => 0,
            Self::Stopped { .. } => 4,
            Self::Failed { .. } => 1,
            Self::Cancelled { .. } => 130,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed { .. } => "completed",
            Self::Stopped { .. } => "stopped",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
        }
    }
}

/// Admission rejection carrying the exhausted dimension and current usage.
/// Produced by `BudgetController::admit_*`; never mutates success state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BudgetStop {
    pub reason: StopReason,
    pub usage: BudgetUsage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spec_is_unlimited_on_every_dimension() {
        let spec = BudgetSpec::default();
        assert!(spec.is_unlimited());
        assert!(spec.max_turns.is_none());
        assert!(spec.max_tool_calls.is_none());
        assert!(spec.max_cost_usd_micros.is_none());
        assert!(spec.max_wall_time_ms.is_none());
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn any_present_dimension_disables_unlimited_shortcut() {
        assert!(
            !BudgetSpec {
                max_turns: Some(128),
                ..BudgetSpec::default()
            }
            .is_unlimited()
        );
        assert!(
            !BudgetSpec {
                max_tool_calls: Some(1),
                ..BudgetSpec::default()
            }
            .is_unlimited()
        );
        assert!(
            !BudgetSpec {
                max_cost_usd_micros: Some(1),
                ..BudgetSpec::default()
            }
            .is_unlimited()
        );
        assert!(
            !BudgetSpec {
                max_wall_time_ms: Some(1),
                ..BudgetSpec::default()
            }
            .is_unlimited()
        );
    }

    #[test]
    fn validation_rejects_zero_dimensions() {
        assert!(
            BudgetSpec {
                max_turns: Some(0),
                ..BudgetSpec::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            BudgetSpec {
                max_tool_calls: Some(0),
                ..BudgetSpec::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            BudgetSpec {
                max_cost_usd_micros: Some(0),
                ..BudgetSpec::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            BudgetSpec {
                max_wall_time_ms: Some(0),
                ..BudgetSpec::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn usage_counters_saturate_instead_of_wrapping() {
        let mut usage = BudgetUsage {
            turns: u32::MAX,
            tool_calls: u32::MAX,
            cost_usd_micros: u64::MAX,
            wall_time_ms: u64::MAX,
        };
        usage.add_turn();
        usage.add_tool_call();
        usage.add_cost_usd_micros(1);
        usage.add_wall_time_ms(1);
        assert_eq!(usage.turns, u32::MAX);
        assert_eq!(usage.tool_calls, u32::MAX);
        assert_eq!(usage.cost_usd_micros, u64::MAX);
        assert_eq!(usage.wall_time_ms, u64::MAX);
    }

    #[test]
    fn usage_merge_saturates_every_counter() {
        let mut usage = BudgetUsage {
            turns: u32::MAX - 1,
            tool_calls: 1,
            cost_usd_micros: u64::MAX - 1,
            wall_time_ms: 1,
        };
        usage.merge(BudgetUsage {
            turns: 2,
            tool_calls: 1,
            cost_usd_micros: 2,
            wall_time_ms: 1,
        });
        assert_eq!(usage.turns, u32::MAX);
        assert_eq!(usage.tool_calls, 2);
        assert_eq!(usage.cost_usd_micros, u64::MAX);
        assert_eq!(usage.wall_time_ms, 2);
    }

    #[test]
    fn budget_spec_serializes_stable_snake_case() {
        let spec = BudgetSpec {
            max_turns: Some(16),
            max_tool_calls: Some(64),
            max_cost_usd_micros: Some(1_250_000),
            max_wall_time_ms: Some(3_600_000),
        };
        let json = serde_json::to_string(&spec).expect("serialize spec");
        assert_eq!(
            json,
            r#"{"max_turns":16,"max_tool_calls":64,"max_cost_usd_micros":1250000,"max_wall_time_ms":3600000}"#
        );
        let round_trip: BudgetSpec = serde_json::from_str(&json).expect("deserialize spec");
        assert_eq!(round_trip, spec);
    }

    #[test]
    fn usage_serializes_stable_snake_case_with_defaults() {
        let usage = BudgetUsage {
            turns: 2,
            tool_calls: 3,
            cost_usd_micros: 4,
            wall_time_ms: 5,
        };
        let json = serde_json::to_string(&usage).expect("serialize usage");
        assert_eq!(
            json,
            r#"{"turns":2,"tool_calls":3,"cost_usd_micros":4,"wall_time_ms":5}"#
        );
        let round_trip: BudgetUsage = serde_json::from_str(&json).expect("deserialize usage");
        assert_eq!(round_trip, usage);

        // Missing fields default to zero.
        let sparse: BudgetUsage =
            serde_json::from_str(r#"{"turns":7}"#).expect("sparse usage deserializes");
        assert_eq!(sparse.turns, 7);
        assert_eq!(sparse.tool_calls, 0);
    }

    #[test]
    fn terminal_serializes_typed_variants_snake_case() {
        let stopped = OperationTerminal::Stopped {
            reason: StopReason::TurnBudget { max_turns: 3 },
            usage: BudgetUsage {
                turns: 3,
                tool_calls: 3,
                cost_usd_micros: 0,
                wall_time_ms: 12,
            },
            checkpoint_id: "cp-1".to_string(),
            resumable: true,
        };
        let json = serde_json::to_string(&stopped).expect("serialize stopped terminal");
        assert_eq!(
            json,
            r#"{"stopped":{"reason":{"turn_budget":{"max_turns":3}},"usage":{"turns":3,"tool_calls":3,"cost_usd_micros":0,"wall_time_ms":12},"checkpoint_id":"cp-1","resumable":true}}"#
        );
        let round_trip: OperationTerminal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_trip, stopped);

        let completed = OperationTerminal::Completed {
            usage: BudgetUsage::default(),
        };
        let json = serde_json::to_string(&completed).expect("serialize completed terminal");
        assert_eq!(
            json,
            r#"{"completed":{"usage":{"turns":0,"tool_calls":0,"cost_usd_micros":0,"wall_time_ms":0}}}"#
        );
        let round_trip: OperationTerminal = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_trip, completed);
    }

    #[test]
    fn stop_reason_and_class_strings_are_stable() {
        assert_eq!(
            StopReason::TurnBudget { max_turns: 1 }.as_str(),
            "turn_budget"
        );
        assert_eq!(
            StopReason::ToolCallBudget { max_tool_calls: 1 }.as_str(),
            "tool_call_budget"
        );
        assert_eq!(
            StopReason::CostBudget {
                max_cost_usd_micros: 1
            }
            .as_str(),
            "cost_budget"
        );
        assert_eq!(
            StopReason::WallTimeBudget {
                max_wall_time_ms: 1
            }
            .as_str(),
            "wall_time_budget"
        );
        assert_eq!(FailureClass::Verification.as_str(), "verification");
        assert_eq!(CancelReason::GoalPause.as_str(), "goal_pause");
    }

    #[test]
    fn terminal_exit_codes_preserve_legacy_mapping() {
        assert_eq!(
            OperationTerminal::Completed {
                usage: BudgetUsage::default()
            }
            .exit_code(),
            0
        );
        assert_eq!(
            OperationTerminal::Stopped {
                reason: StopReason::TurnBudget { max_turns: 1 },
                usage: BudgetUsage::default(),
                checkpoint_id: String::new(),
                resumable: false,
            }
            .exit_code(),
            4
        );
        assert_eq!(
            OperationTerminal::Failed {
                class: FailureClass::Runtime,
                message: "boom".to_string(),
            }
            .exit_code(),
            1
        );
        assert_eq!(
            OperationTerminal::Cancelled {
                reason: CancelReason::User,
                checkpoint_id: None,
            }
            .exit_code(),
            130
        );
    }
}
