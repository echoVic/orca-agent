//! Codex-style soft-landing reminders before hard budget walls.
//!
//! Thresholds are pure policy: callers own delivery state so the same
//! reminder is not restated every iteration within one outer turn.

/// Remaining-inner-turn thresholds at which a soft-landing reminder is due,
/// ordered from furthest to nearest the hard wall.
pub(crate) const INNER_TURN_REMINDER_AT_REMAINING: &[u32] = &[16, 8, 4, 2];

/// Remaining-cost fraction thresholds (0.0–1.0 of max budget remaining).
pub(crate) const COST_BUDGET_REMINDER_AT_REMAINING_FRACTION: &[f64] = &[0.25, 0.10, 0.05];

/// Remaining goal-token thresholds as fractions of the configured budget.
pub(crate) const GOAL_TOKEN_REMINDER_AT_REMAINING_FRACTION: &[f64] = &[0.25, 0.10, 0.05];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SoftLandingKind {
    InnerTurns { max_turns: u32 },
    CostBudget,
    GoalTokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SoftLandingReminder {
    pub remaining: u64,
    /// Number of configured thresholds that have been crossed (1-based count).
    pub reminder_index: u32,
    pub kind: SoftLandingKind,
}

/// Returns a reminder when more remaining-turn thresholds have been crossed
/// than `delivered_index` has already acknowledged.
pub(crate) fn pending_inner_turn_reminder(
    max_turns: u32,
    turns_started: u32,
    delivered_index: u32,
) -> Option<SoftLandingReminder> {
    if max_turns == 0 {
        return None;
    }
    let remaining = max_turns.saturating_sub(turns_started);
    if remaining == 0 {
        return None;
    }
    let reminder_index = INNER_TURN_REMINDER_AT_REMAINING
        .iter()
        .filter(|&&threshold| remaining <= threshold)
        .count() as u32;
    if reminder_index == 0 || reminder_index <= delivered_index {
        return None;
    }
    Some(SoftLandingReminder {
        remaining: remaining.into(),
        reminder_index,
        kind: SoftLandingKind::InnerTurns { max_turns },
    })
}

/// Returns a reminder when remaining cost budget crosses configured fractions.
/// Both values are in USD micros so callers never round-trip through floats.
pub(crate) fn pending_cost_budget_reminder(
    max_cost_usd_micros: u64,
    spent_usd_micros: u64,
    delivered_index: u32,
) -> Option<SoftLandingReminder> {
    if max_cost_usd_micros == 0 {
        return None;
    }
    let remaining_micros = max_cost_usd_micros.saturating_sub(spent_usd_micros);
    let remaining_fraction = remaining_micros as f64 / max_cost_usd_micros as f64;
    let reminder_index = COST_BUDGET_REMINDER_AT_REMAINING_FRACTION
        .iter()
        .filter(|&&threshold| remaining_fraction <= threshold)
        .count() as u32;
    if reminder_index == 0 || reminder_index <= delivered_index {
        return None;
    }
    Some(SoftLandingReminder {
        remaining: remaining_micros,
        reminder_index,
        kind: SoftLandingKind::CostBudget,
    })
}

/// Returns a reminder when remaining goal tokens cross configured fractions.
pub(crate) fn pending_goal_token_reminder(
    budget: i64,
    used: i64,
    delivered_index: u32,
) -> Option<SoftLandingReminder> {
    if budget <= 0 {
        return None;
    }
    let remaining = (budget - used).max(0) as u64;
    let remaining_fraction = remaining as f64 / budget as f64;
    let reminder_index = GOAL_TOKEN_REMINDER_AT_REMAINING_FRACTION
        .iter()
        .filter(|&&threshold| remaining_fraction <= threshold)
        .count() as u32;
    if reminder_index == 0 || reminder_index <= delivered_index {
        return None;
    }
    Some(SoftLandingReminder {
        remaining,
        reminder_index,
        kind: SoftLandingKind::GoalTokens,
    })
}

pub(crate) fn format_soft_landing_message(reminder: &SoftLandingReminder) -> String {
    match reminder.kind {
        SoftLandingKind::InnerTurns { max_turns } => format!(
            "[Budget soft landing]\n\
Inner-turn budget is low: {remaining} of {max_turns} model turns remain in this outer turn.\n\
Prioritize the highest-value unfinished work, verify requirements against current state, \
and avoid thrashing. Do not mark the goal complete merely because the turn budget is nearly exhausted. \
If the work cannot finish this outer turn, update the task plan, record key findings, and leave durable \
progress with a clear next action.",
            remaining = reminder.remaining,
            max_turns = max_turns,
        ),
        SoftLandingKind::CostBudget => {
            let remaining_usd = reminder.remaining as f64 / 1_000_000.0;
            format!(
                "[Budget soft landing]\n\
Cost budget is low: about ${remaining_usd:.4} remains on the configured cost ceiling.\n\
Prioritize finishing or verifying the highest-value requirements. Do not mark the goal complete \
merely because money is running out. Prefer targeted evidence over broad exploration, update the \
task plan, and record key findings before the hard wall."
            )
        }
        SoftLandingKind::GoalTokens => format!(
            "[Budget soft landing]\n\
Goal token budget is low: about {remaining} charged tokens remain.\n\
Prioritize finishing and verifying the real objective. Do not redefine success around a smaller task, \
and do not mark complete merely because the token budget is nearly exhausted. Update the task plan \
and record key findings before the hard wall.",
            remaining = reminder.remaining,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inner_turn_reminder_crosses_thresholds_once_each() {
        let max = 128;
        // Far from the wall: no reminder.
        assert!(pending_inner_turn_reminder(max, 100, 0).is_none());

        // Cross 16 remaining (turns_started = 112).
        let first = pending_inner_turn_reminder(max, 112, 0).expect("16-remaining reminder");
        assert_eq!(first.remaining, 16);
        assert_eq!(first.reminder_index, 1);
        assert!(pending_inner_turn_reminder(max, 112, 1).is_none());

        // Cross 8 remaining without restating the first threshold.
        let second = pending_inner_turn_reminder(max, 120, 1).expect("8-remaining reminder");
        assert_eq!(second.remaining, 8);
        assert_eq!(second.reminder_index, 2);

        // Jump straight to 2 remaining after only delivering index 0 → index 4.
        let jumped = pending_inner_turn_reminder(max, 126, 0).expect("jumped thresholds");
        assert_eq!(jumped.remaining, 2);
        assert_eq!(jumped.reminder_index, 4);
    }

    #[test]
    fn cost_and_goal_token_reminders_use_fraction_thresholds() {
        let cost = pending_cost_budget_reminder(1_000_000, 800_000, 0).expect("25% remaining cost");
        assert_eq!(cost.reminder_index, 1);
        assert!(pending_cost_budget_reminder(1_000_000, 800_000, 1).is_none());

        let tokens = pending_goal_token_reminder(10_000, 9_200, 0).expect("8% remaining tokens");
        assert_eq!(tokens.reminder_index, 2); // crossed 25% and 10%
        assert_eq!(tokens.remaining, 800);
    }

    #[test]
    fn soft_landing_message_mentions_remaining_budget() {
        let reminder = SoftLandingReminder {
            remaining: 4,
            reminder_index: 3,
            kind: SoftLandingKind::InnerTurns { max_turns: 128 },
        };
        let message = format_soft_landing_message(&reminder);
        assert!(message.contains("4 of 128"));
        assert!(message.contains("Do not mark the goal complete"));
    }
}
