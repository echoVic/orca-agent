//! Consecutive computer failures per app within one user turn.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::extension::ExtensionData;

const HINT_AT: u32 = 3;
const STOP_AT: u32 = 6;

/// Lives in the turn's extension store, so it ends with the user turn.
#[derive(Default)]
pub(crate) struct FailureBudget {
    failures: Mutex<HashMap<String, u32>>,
}

impl FailureBudget {
    pub(crate) fn for_turn(turn_store: &ExtensionData) -> Arc<Self> {
        turn_store.get_or_init(Self::default)
    }

    /// The refusal for an app that already failed STOP_AT times in a row.
    pub(crate) fn stopped(&self, app: &str) -> Option<String> {
        let failures = *self
            .failures
            .lock()
            .expect("failure budget lock poisoned")
            .get(app)?;
        (failures >= STOP_AT).then(|| stop_message(app))
    }

    /// Records one finished call; returns the note a failure must carry.
    pub(crate) fn record(&self, app: &str, failed: bool) -> Option<String> {
        let mut failures = self.failures.lock().expect("failure budget lock poisoned");
        if !failed {
            failures.remove(app);
            return None;
        }
        let count = failures.entry(app.to_owned()).or_default();
        *count += 1;
        match *count {
            n if n >= STOP_AT => Some(stop_message(app)),
            n if n >= HINT_AT => Some(format!(
                "{app} has failed {n} times in a row. Stop trying workarounds such as shell commands, scripts or other apps; tell the user what failed and what you tried, unless one specific fix remains."
            )),
            _ => None,
        }
    }
}

fn stop_message(app: &str) -> String {
    format!(
        "Orca stopped computer use on {app} for this turn after {STOP_AT} failures in a row. Tell the user what failed and what you tried."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_third_failure_hints_and_the_sixth_stops() {
        let budget = FailureBudget::default();
        assert_eq!(budget.record("com.apple.systempreferences", true), None);
        assert_eq!(budget.record("com.apple.systempreferences", true), None);
        let hint = budget.record("com.apple.systempreferences", true).unwrap();
        assert_eq!(
            hint,
            "com.apple.systempreferences has failed 3 times in a row. Stop trying workarounds such as shell commands, scripts or other apps; tell the user what failed and what you tried, unless one specific fix remains."
        );
        assert!(
            budget
                .record("com.apple.systempreferences", true)
                .unwrap()
                .contains("4 times")
        );
        assert!(
            budget
                .record("com.apple.systempreferences", true)
                .unwrap()
                .contains("5 times")
        );
        assert_eq!(budget.stopped("com.apple.systempreferences"), None);
        let stop = budget.record("com.apple.systempreferences", true).unwrap();
        assert_eq!(
            stop,
            "Orca stopped computer use on com.apple.systempreferences for this turn after 6 failures in a row. Tell the user what failed and what you tried."
        );
        assert_eq!(budget.stopped("com.apple.systempreferences"), Some(stop));
    }

    #[test]
    fn budget_counts_each_app_separately_and_a_success_resets_it() {
        let budget = FailureBudget::default();
        for _ in 0..2 {
            budget.record("com.apple.calculator", true);
            budget.record("com.apple.TextEdit", true);
        }
        assert_eq!(budget.record("com.apple.calculator", false), None);
        assert_eq!(budget.record("com.apple.calculator", true), None);
        assert!(
            budget
                .record("com.apple.TextEdit", true)
                .unwrap()
                .contains("3 times")
        );
    }
}
