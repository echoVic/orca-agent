use std::fmt;

use orca_core::goal_runtime::{
    GoalContinuationReason, GoalGap, GoalId, GoalNextAction, GoalOuterTurnId,
    GoalOuterTurnSnapshot, GoalPauseReason, GoalRecord, GoalRejectCode, GoalRequestedState,
    GoalRunId, GoalState, GoalTurnOrigin, GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent,
    GoalUsage, GoalVerificationResult,
};

use crate::lifecycle::TurnEndReason;

pub(crate) const SAME_GAP_STREAK_LIMIT: u32 = 3;
pub(crate) const NO_SUBSTANTIVE_PROGRESS_GAP_FINGERPRINT: &str =
    "outer_turn:no_substantive_progress";
/// Independent safety net beside the same-gap streak: auto-continuing after
/// Repeated budget stops without completing forever would otherwise spin a goal that
/// keeps producing activity but never finishes.
pub(crate) const MAX_CONSECUTIVE_INNER_TURN_CONTINUATIONS: u32 = 8;

/// Recomputes the same-gap streak from recent turn fingerprints, most recent
/// first. Kept pure and separate from storage so the streak survives restarts
/// without persisting derived state that could drift from the turn history.
fn streak_from_history(fingerprints: &[Option<String>]) -> (Option<String>, u32) {
    let Some(Some(most_recent)) = fingerprints.first() else {
        return (None, 0);
    };
    let streak = fingerprints
        .iter()
        .take_while(|fingerprint| fingerprint.as_ref() == Some(most_recent))
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    (Some(most_recent.clone()), streak)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTurnResult {
    pub status: GoalTurnStatus,
    pub end_reason: TurnEndReason,
    /// Typed operation terminal; the authoritative fact for budget stops.
    pub terminal: Option<orca_core::budget::OperationTerminal>,
    pub usage: GoalUsage,
    pub gaps: Vec<GoalGap>,
    pub evidence_count: usize,
}

impl GoalTurnResult {
    pub fn successful() -> Self {
        Self {
            status: GoalTurnStatus::Success,
            end_reason: TurnEndReason::Unclassified,
            terminal: None,
            usage: GoalUsage::default(),
            gaps: Vec::new(),
            evidence_count: 0,
        }
    }

    pub fn successful_with_gaps(gaps: Vec<GoalGap>) -> Self {
        Self {
            gaps,
            ..Self::successful()
        }
    }

    pub fn budget_exhausted(end_reason: TurnEndReason) -> Self {
        Self {
            status: GoalTurnStatus::BudgetExhausted,
            end_reason,
            terminal: None,
            usage: GoalUsage::default(),
            gaps: Vec::new(),
            evidence_count: 0,
        }
    }

    /// A typed budget stop that created a resumable checkpoint.
    fn is_resumable_budget_stop(&self) -> bool {
        matches!(
            self.terminal,
            Some(orca_core::budget::OperationTerminal::Stopped {
                resumable: true,
                ..
            })
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalTrackerError {
    GoalInactive,
    OuterTurnAlreadyActive,
    NoActiveOuterTurn,
}

impl fmt::Display for GoalTrackerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::GoalInactive => "goal is not active",
            Self::OuterTurnAlreadyActive => "a goal outer turn is already active",
            Self::NoActiveOuterTurn => "no goal outer turn is active",
        })
    }
}

impl std::error::Error for GoalTrackerError {}

#[derive(Clone, Debug)]
pub struct GoalTracker {
    goal_id: GoalId,
    run_id: GoalRunId,
    state: GoalState,
    token_budget: Option<i64>,
    usage: GoalUsage,
    current_outer_turn: Option<GoalOuterTurnSnapshot>,
    last_outer_turn: Option<GoalOuterTurnSnapshot>,
    outer_turn_count: u32,
    pending_intent: Option<GoalUpdateIntent>,
    last_gap_fingerprint: Option<String>,
    same_gap_streak: u32,
    consecutive_inner_turn_budget_exhaustions: u32,
}

impl GoalTracker {
    pub fn new(goal_id: GoalId, token_budget: Option<i64>) -> Self {
        Self {
            goal_id,
            run_id: GoalRunId::new(),
            state: GoalState::Active,
            token_budget,
            usage: GoalUsage::default(),
            current_outer_turn: None,
            last_outer_turn: None,
            outer_turn_count: 0,
            pending_intent: None,
            last_gap_fingerprint: None,
            same_gap_streak: 0,
            consecutive_inner_turn_budget_exhaustions: 0,
        }
    }

    /// Remaining cumulative budget for the next continuation. `None` when the
    /// goal is unbounded; a continuation may only obtain a lease up to this
    /// remaining amount, and zero remaining disables continuation.
    pub fn remaining_budget(&self) -> Option<i64> {
        self.token_budget
            .map(|budget| budget.saturating_sub(self.usage.charged_tokens()))
    }

    /// True when the Goal's cumulative budget is exhausted and automatic
    /// continuation must be disabled.
    pub fn budget_exhausted(&self) -> bool {
        self.remaining_budget() == Some(0)
    }

    pub fn from_record(record: &GoalRecord) -> Self {
        Self {
            goal_id: record.goal_id.clone(),
            run_id: record
                .current_run
                .as_ref()
                .map(|run| run.goal_run_id.clone())
                .unwrap_or_default(),
            state: record.state.clone(),
            token_budget: record.token_budget,
            usage: record.usage.clone(),
            current_outer_turn: None,
            last_outer_turn: None,
            outer_turn_count: record
                .current_run
                .as_ref()
                .map(|run| run.continuation_count)
                .unwrap_or_default(),
            pending_intent: None,
            last_gap_fingerprint: None,
            same_gap_streak: 0,
            // In-memory only for now; gap streak is the durable watchdog.
            consecutive_inner_turn_budget_exhaustions: 0,
        }
    }

    /// Like [`Self::from_record`], but restores the same-gap streak from
    /// persisted turn history so a restart does not silently reset the
    /// no-progress watchdog.
    pub fn from_record_with_history(record: &GoalRecord, history: &[Option<String>]) -> Self {
        let mut tracker = Self::from_record(record);
        let (fingerprint, streak) = streak_from_history(history);
        tracker.last_gap_fingerprint = fingerprint;
        tracker.same_gap_streak = streak;
        tracker
    }

    pub fn goal_id(&self) -> &GoalId {
        &self.goal_id
    }

    pub fn run_id(&self) -> &GoalRunId {
        &self.run_id
    }

    pub fn state(&self) -> &GoalState {
        &self.state
    }

    pub fn usage(&self) -> &GoalUsage {
        &self.usage
    }

    pub fn outer_turn_count(&self) -> u32 {
        self.outer_turn_count
    }

    pub fn current_outer_turn(&self) -> Option<&GoalOuterTurnSnapshot> {
        self.current_outer_turn.as_ref()
    }

    pub fn last_outer_turn(&self) -> Option<&GoalOuterTurnSnapshot> {
        self.last_outer_turn.as_ref()
    }

    pub fn begin_outer_turn(
        &mut self,
        origin: GoalTurnOrigin,
    ) -> Result<GoalOuterTurnId, GoalTrackerError> {
        if !self.state.should_continue() {
            return Err(GoalTrackerError::GoalInactive);
        }
        if self.current_outer_turn.is_some() {
            return Err(GoalTrackerError::OuterTurnAlreadyActive);
        }
        let outer_turn_id = GoalOuterTurnId::new();
        self.current_outer_turn = Some(GoalOuterTurnSnapshot {
            outer_turn_id: outer_turn_id.clone(),
            goal_run_id: self.run_id.clone(),
            origin,
            model_response_count: 0,
            tool_attempt_count: 0,
            tool_names: Vec::new(),
            status: None,
        });
        self.outer_turn_count = self.outer_turn_count.saturating_add(1);
        Ok(outer_turn_id)
    }

    pub fn bind_persisted_outer_turn(
        &mut self,
        outer_turn_id: GoalOuterTurnId,
        origin: GoalTurnOrigin,
    ) -> Result<(), GoalTrackerError> {
        if !self.state.should_continue() {
            return Err(GoalTrackerError::GoalInactive);
        }
        if self.current_outer_turn.is_some() {
            return Err(GoalTrackerError::OuterTurnAlreadyActive);
        }
        self.current_outer_turn = Some(GoalOuterTurnSnapshot {
            outer_turn_id,
            goal_run_id: self.run_id.clone(),
            origin,
            model_response_count: 0,
            tool_attempt_count: 0,
            tool_names: Vec::new(),
            status: None,
        });
        Ok(())
    }

    pub fn record_model_response(&mut self) {
        if let Some(turn) = self.current_outer_turn.as_mut() {
            turn.model_response_count = turn.model_response_count.saturating_add(1);
        }
    }

    pub fn record_tool_attempt(&mut self, tool_name: impl Into<String>) {
        if let Some(turn) = self.current_outer_turn.as_mut() {
            turn.tool_attempt_count = turn.tool_attempt_count.saturating_add(1);
            turn.tool_names.push(tool_name.into());
        }
    }

    pub fn submit_terminal_intent(&mut self, intent: GoalUpdateIntent) -> GoalUpdateAck {
        if !self.state.should_continue() {
            return GoalUpdateAck::BlockedAgainstInactive {
                state: self.state.clone(),
            };
        }
        if self.current_outer_turn.is_none() {
            return GoalUpdateAck::Rejected {
                code: GoalRejectCode::NoActiveOuterTurn,
                message: "terminal goal intent requires an active outer turn".to_string(),
            };
        }
        if let Some(pending) = self.pending_intent.as_ref() {
            return if pending.intent_id == intent.intent_id {
                GoalUpdateAck::AlreadyPending {
                    intent_id: pending.intent_id.clone(),
                }
            } else {
                GoalUpdateAck::Rejected {
                    code: GoalRejectCode::TerminalIntentPending,
                    message: "another terminal goal intent is already pending".to_string(),
                }
            };
        }
        if intent.evidence.is_empty() {
            return GoalUpdateAck::Rejected {
                code: GoalRejectCode::MissingEvidence,
                message: "terminal goal intent requires structured evidence".to_string(),
            };
        }
        if intent.requested_state == GoalRequestedState::Blocked && intent.blocker.is_none() {
            return GoalUpdateAck::Rejected {
                code: GoalRejectCode::MissingBlocker,
                message: "blocked goal intent requires a typed blocker".to_string(),
            };
        }
        let intent_id = intent.intent_id.clone();
        self.pending_intent = Some(intent);
        GoalUpdateAck::DeferredToTurnEnd {
            intent_id,
            pending_depth: 1,
        }
    }

    pub fn finish_outer_turn(
        &mut self,
        result: GoalTurnResult,
    ) -> Result<GoalNextAction, GoalTrackerError> {
        let mut turn = self
            .current_outer_turn
            .take()
            .ok_or(GoalTrackerError::NoActiveOuterTurn)?;
        turn.status = Some(result.status);
        self.usage.saturating_add_assign(&result.usage);
        self.last_outer_turn = Some(turn);

        match result.status {
            GoalTurnStatus::Success => {
                self.consecutive_inner_turn_budget_exhaustions = 0;
            }
            // A typed budget stop with a committed checkpoint is resumable and
            // counts toward the consecutive-interruption safety net.
            GoalTurnStatus::BudgetExhausted if result.is_resumable_budget_stop() => {
                self.consecutive_inner_turn_budget_exhaustions = self
                    .consecutive_inner_turn_budget_exhaustions
                    .saturating_add(1);
                if self.consecutive_inner_turn_budget_exhaustions
                    >= MAX_CONSECUTIVE_INNER_TURN_CONTINUATIONS
                {
                    self.pending_intent = None;
                    return Ok(self.pause(
                        GoalPauseReason::NoProgress,
                        format!(
                            "budget stopped for {} consecutive outer turns without completion",
                            self.consecutive_inner_turn_budget_exhaustions
                        ),
                    ));
                }
                // Resumable: fall through into progress/gap accounting.
            }
            // Non-resumable budget stops (no checkpoint or a cost wall) pause
            // as a usage limit.
            GoalTurnStatus::BudgetExhausted => {
                self.pending_intent = None;
                return Ok(self.pause(
                    GoalPauseReason::UsageLimit,
                    "goal outer turn ended because the operation budget was exhausted".to_string(),
                ));
            }
            other => {
                self.pending_intent = None;
                return Ok(self.pause(
                    GoalPauseReason::Infrastructure,
                    format!("goal outer turn ended with {other:?}"),
                ));
            }
        }

        if self
            .token_budget
            .is_some_and(|budget| self.usage.charged_tokens() >= budget)
        {
            self.pending_intent = None;
            self.state = GoalState::BudgetLimited;
            return Ok(GoalNextAction::BudgetLimited);
        }

        if let Some(intent) = self.pending_intent.take() {
            return Ok(GoalNextAction::Verify { intent });
        }

        if result.evidence_count > 0 && result.gaps.is_empty() {
            self.reset_gap_streak();
            return Ok(GoalNextAction::Continue {
                reason: GoalContinuationReason::Progress,
            });
        }

        Ok(self.apply_gaps(result.gaps))
    }

    pub fn apply_verification(&mut self, result: GoalVerificationResult) -> GoalNextAction {
        match result {
            GoalVerificationResult::Achieved { evidence } => {
                self.reset_gap_streak();
                self.state = GoalState::Complete {
                    evidence: evidence.clone(),
                };
                GoalNextAction::Complete { evidence }
            }
            GoalVerificationResult::Blocked { blocker } => {
                self.reset_gap_streak();
                self.state = GoalState::Blocked {
                    blocker: blocker.clone(),
                };
                GoalNextAction::Blocked { blocker }
            }
            GoalVerificationResult::NotAchieved { gaps } => self.apply_gaps(gaps),
            GoalVerificationResult::Indeterminate { message } => {
                self.pause(GoalPauseReason::Infrastructure, message)
            }
        }
    }

    pub fn pause(&mut self, reason: GoalPauseReason, message: impl Into<String>) -> GoalNextAction {
        let message = message.into();
        self.state = GoalState::Paused {
            reason,
            message: message.clone(),
        };
        GoalNextAction::Pause { reason, message }
    }

    pub fn resume(&mut self, origin: GoalTurnOrigin) -> GoalNextAction {
        if let GoalState::Complete { evidence } = &self.state {
            return GoalNextAction::Complete {
                evidence: evidence.clone(),
            };
        }
        self.run_id = GoalRunId::new();
        self.state = GoalState::Active;
        self.current_outer_turn = None;
        self.pending_intent = None;
        self.reset_gap_streak();
        self.consecutive_inner_turn_budget_exhaustions = 0;
        GoalNextAction::Continue {
            reason: match origin {
                GoalTurnOrigin::Resume => GoalContinuationReason::Resume,
                GoalTurnOrigin::WorkflowNotification => {
                    GoalContinuationReason::WorkflowNotification
                }
                GoalTurnOrigin::User => GoalContinuationReason::Initial,
                GoalTurnOrigin::Continuation => GoalContinuationReason::Progress,
            },
        }
    }

    fn apply_gaps(&mut self, gaps: Vec<GoalGap>) -> GoalNextAction {
        let Some(gap) = gaps.into_iter().find(|gap| gap.model_fixable) else {
            self.reset_gap_streak();
            return GoalNextAction::Continue {
                reason: GoalContinuationReason::Progress,
            };
        };

        if self.last_gap_fingerprint.as_deref() == Some(gap.fingerprint.as_str()) {
            self.same_gap_streak = self.same_gap_streak.saturating_add(1);
        } else {
            self.last_gap_fingerprint = Some(gap.fingerprint.clone());
            self.same_gap_streak = 1;
        }

        if self.same_gap_streak >= SAME_GAP_STREAK_LIMIT {
            return self.pause(
                GoalPauseReason::NoProgress,
                format!(
                    "same model-fixable gap repeated for {} outer turns: {}",
                    self.same_gap_streak, gap.summary
                ),
            );
        }

        GoalNextAction::Continue {
            reason: GoalContinuationReason::GapFeedback,
        }
    }

    fn reset_gap_streak(&mut self) {
        self.last_gap_fingerprint = None;
        self.same_gap_streak = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::goal_runtime::{
        BlockerKind, BlockerSummary, EvidenceItem, GoalId, GoalNextAction, GoalPauseReason,
        GoalRequestedState, GoalState, GoalTurnOrigin, GoalUpdateAck, GoalUpdateIntent,
        GoalVerificationResult, IntentId,
    };

    fn model_fixable_gap() -> GoalGap {
        GoalGap {
            summary: "pick the next roadmap slice".to_string(),
            fingerprint: "roadmap:next-slice".to_string(),
            model_fixable: true,
        }
    }

    #[test]
    fn inner_iterations_never_increment_goal_outer_turns() {
        let mut tracker = GoalTracker::new(GoalId::new(), None);
        tracker.begin_outer_turn(GoalTurnOrigin::User).unwrap();

        for index in 0..128 {
            tracker.record_model_response();
            tracker.record_tool_attempt(format!("tool-{index}"));
        }

        assert_eq!(tracker.outer_turn_count(), 1);
        let current = tracker.current_outer_turn().unwrap();
        assert_eq!(current.model_response_count, 128);
        assert_eq!(current.tool_attempt_count, 128);
    }

    #[test]
    fn three_closed_outer_turns_with_same_fixable_gap_pause_as_no_progress() {
        let mut tracker = GoalTracker::new(GoalId::new(), None);

        for attempt in 1..=3 {
            tracker
                .begin_outer_turn(GoalTurnOrigin::Continuation)
                .unwrap();
            let action = tracker
                .finish_outer_turn(GoalTurnResult::successful_with_gaps(vec![
                    model_fixable_gap(),
                ]))
                .unwrap();

            if attempt < 3 {
                assert!(matches!(action, GoalNextAction::Continue { .. }));
            } else {
                assert!(matches!(
                    action,
                    GoalNextAction::Pause {
                        reason: GoalPauseReason::NoProgress,
                        ..
                    }
                ));
            }
        }

        assert!(matches!(
            tracker.state(),
            GoalState::Paused {
                reason: GoalPauseReason::NoProgress,
                ..
            }
        ));
    }

    #[test]
    fn resumable_budget_stop_can_continue() {
        let mut tracker = GoalTracker::new(GoalId::new(), None);
        tracker.begin_outer_turn(GoalTurnOrigin::User).unwrap();
        let action = tracker
            .finish_outer_turn(GoalTurnResult {
                status: GoalTurnStatus::BudgetExhausted,
                end_reason: TurnEndReason::Unclassified,
                terminal: Some(orca_core::budget::OperationTerminal::Stopped {
                    reason: orca_core::budget::StopReason::TurnBudget { max_turns: 3 },
                    usage: orca_core::budget::BudgetUsage::default(),
                    checkpoint_id: "cp-1".to_string(),
                    resumable: true,
                }),
                usage: GoalUsage::default(),
                gaps: Vec::new(),
                evidence_count: 3,
            })
            .unwrap();
        assert!(matches!(
            action,
            GoalNextAction::Continue {
                reason: GoalContinuationReason::Progress
            }
        ));
        assert_eq!(tracker.state(), &GoalState::Active);
    }

    #[test]
    fn cost_budget_exhaustion_pauses_as_usage_limit() {
        let mut tracker = GoalTracker::new(GoalId::new(), None);
        tracker.begin_outer_turn(GoalTurnOrigin::User).unwrap();
        let action = tracker
            .finish_outer_turn(GoalTurnResult {
                status: GoalTurnStatus::BudgetExhausted,
                end_reason: TurnEndReason::CostBudgetExhausted,
                terminal: Some(orca_core::budget::OperationTerminal::Stopped {
                    reason: orca_core::budget::StopReason::CostBudget {
                        max_cost_usd_micros: 1_000_000,
                    },
                    usage: orca_core::budget::BudgetUsage::default(),
                    checkpoint_id: String::new(),
                    resumable: false,
                }),
                usage: GoalUsage::default(),
                gaps: Vec::new(),
                evidence_count: 0,
            })
            .unwrap();
        assert!(matches!(
            action,
            GoalNextAction::Pause {
                reason: GoalPauseReason::UsageLimit,
                ..
            }
        ));
        assert!(matches!(
            tracker.state(),
            GoalState::Paused {
                reason: GoalPauseReason::UsageLimit,
                ..
            }
        ));
    }

    #[test]
    fn eight_consecutive_budget_stops_pause_as_no_progress() {
        let mut tracker = GoalTracker::new(GoalId::new(), None);
        for attempt in 1..=MAX_CONSECUTIVE_INNER_TURN_CONTINUATIONS {
            tracker
                .begin_outer_turn(GoalTurnOrigin::Continuation)
                .unwrap();
            let action = tracker
                .finish_outer_turn(GoalTurnResult {
                    status: GoalTurnStatus::BudgetExhausted,
                    end_reason: TurnEndReason::Unclassified,
                    terminal: Some(orca_core::budget::OperationTerminal::Stopped {
                        reason: orca_core::budget::StopReason::TurnBudget { max_turns: 3 },
                        usage: orca_core::budget::BudgetUsage::default(),
                        checkpoint_id: format!("cp-{attempt}"),
                        resumable: true,
                    }),
                    usage: GoalUsage::default(),
                    gaps: Vec::new(),
                    evidence_count: 2,
                })
                .unwrap();
            if attempt < MAX_CONSECUTIVE_INNER_TURN_CONTINUATIONS {
                assert!(
                    matches!(action, GoalNextAction::Continue { .. }),
                    "attempt {attempt} should still continue"
                );
            } else {
                assert!(matches!(
                    action,
                    GoalNextAction::Pause {
                        reason: GoalPauseReason::NoProgress,
                        ..
                    }
                ));
            }
        }
    }

    #[test]
    fn terminal_intent_is_deferred_until_turn_end_verification() {
        let mut tracker = GoalTracker::new(GoalId::new(), None);
        tracker.begin_outer_turn(GoalTurnOrigin::User).unwrap();
        let intent = GoalUpdateIntent {
            intent_id: IntentId::new(),
            requested_state: GoalRequestedState::Complete,
            reason: "all requirements verified".to_string(),
            evidence: vec![EvidenceItem::observation("full gate passed")],
            blocker: None,
        };

        let ack = tracker.submit_terminal_intent(intent.clone());
        assert!(matches!(ack, GoalUpdateAck::DeferredToTurnEnd { .. }));
        assert_eq!(tracker.state(), &GoalState::Active);

        let action = tracker
            .finish_outer_turn(GoalTurnResult::successful())
            .unwrap();
        assert!(matches!(
            action,
            GoalNextAction::Verify { intent: pending } if pending == intent
        ));
        assert_eq!(tracker.state(), &GoalState::Active);

        let action = tracker.apply_verification(GoalVerificationResult::Achieved {
            evidence: intent.evidence,
        });
        assert!(matches!(action, GoalNextAction::Complete { .. }));
        assert!(matches!(tracker.state(), GoalState::Complete { .. }));
    }

    #[test]
    fn model_fixable_verifier_gap_never_becomes_blocked() {
        let mut tracker = GoalTracker::new(GoalId::new(), None);
        tracker.begin_outer_turn(GoalTurnOrigin::User).unwrap();
        tracker
            .finish_outer_turn(GoalTurnResult::successful())
            .unwrap();

        let action = tracker.apply_verification(GoalVerificationResult::NotAchieved {
            gaps: vec![model_fixable_gap()],
        });

        assert!(matches!(action, GoalNextAction::Continue { .. }));
        assert_eq!(tracker.state(), &GoalState::Active);
    }

    #[test]
    fn streak_is_rebuilt_from_recent_fingerprints() {
        // Ordered most-recent-first, as returned by the history query.
        let repeated = vec![
            Some("gap:alpha".to_string()),
            Some("gap:alpha".to_string()),
            Some("gap:beta".to_string()),
        ];
        assert_eq!(
            streak_from_history(&repeated),
            (Some("gap:alpha".to_string()), 2)
        );

        let fresh = vec![Some("gap:beta".to_string()), Some("gap:alpha".to_string())];
        assert_eq!(
            streak_from_history(&fresh),
            (Some("gap:beta".to_string()), 1)
        );

        assert_eq!(streak_from_history(&[]), (None, 0));

        let all_same = vec![
            Some("gap:alpha".to_string()),
            Some("gap:alpha".to_string()),
            Some("gap:alpha".to_string()),
        ];
        assert_eq!(
            streak_from_history(&all_same),
            (Some("gap:alpha".to_string()), 3)
        );

        let progress_barrier = vec![
            Some("gap:alpha".to_string()),
            None,
            Some("gap:alpha".to_string()),
        ];
        assert_eq!(
            streak_from_history(&progress_barrier),
            (Some("gap:alpha".to_string()), 1)
        );
        assert_eq!(
            streak_from_history(&[None, Some("gap:alpha".to_string())]),
            (None, 0)
        );
    }

    #[test]
    fn verifier_can_block_only_with_a_non_model_fixable_blocker() {
        let mut tracker = GoalTracker::new(GoalId::new(), None);
        let blocker = BlockerSummary {
            kind: BlockerKind::UserDecision,
            summary: "the user must choose the public API".to_string(),
            fingerprint: "user-decision:public-api".to_string(),
            evidence: Vec::new(),
        };

        let action = tracker.apply_verification(GoalVerificationResult::Blocked {
            blocker: blocker.clone(),
        });

        assert!(matches!(action, GoalNextAction::Blocked { .. }));
        assert_eq!(tracker.state(), &GoalState::Blocked { blocker });
    }
}
