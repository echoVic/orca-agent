//! Consecutive computer failures per app within one user turn.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use orca_core::tool_types::ToolResult;

use crate::extension::ExtensionData;

const HINT_AT: u32 = 3;
const STOP_AT: u32 = 6;

/// Lives in the turn's extension store, so it ends with the user turn.
#[derive(Default)]
pub(crate) struct FailureBudget {
    /// Counts by app, keyed case-insensitively and by resolved bundle ID.
    failures: Mutex<HashMap<String, u32>>,
    /// Names the model used, lowercased, mapped to the bundle ID they resolved to.
    aliases: Mutex<HashMap<String, String>>,
}

impl FailureBudget {
    pub(crate) fn for_turn(turn_store: &ExtensionData) -> Arc<Self> {
        turn_store.get_or_init(Self::default)
    }

    /// Remembers that a name resolved to a bundle ID, so either spelling
    /// shares one count and a stopped app is refused before resolving again.
    pub(crate) fn alias(&self, name: &str, bundle_id: &str) {
        let (name, bundle_id) = (name.to_lowercase(), bundle_id.to_lowercase());
        if name == bundle_id {
            return;
        }
        // Failures counted under the name (validation failures before it
        // resolved) belong to the app it now names.
        let mut failures = self.failures.lock().expect("failure budget lock poisoned");
        if let Some(count) = failures.remove(&name) {
            *failures.entry(bundle_id.clone()).or_default() += count;
        }
        drop(failures);
        self.aliases
            .lock()
            .expect("failure budget lock poisoned")
            .insert(name, bundle_id);
    }

    fn key(&self, app: &str) -> String {
        let name = app.to_lowercase();
        self.aliases
            .lock()
            .expect("failure budget lock poisoned")
            .get(&name)
            .cloned()
            .unwrap_or(name)
    }

    /// The refusal for an app that already failed STOP_AT times in a row.
    pub(crate) fn stopped(&self, app: &str) -> Option<String> {
        let key = self.key(app);
        let failures = *self
            .failures
            .lock()
            .expect("failure budget lock poisoned")
            .get(&key)?;
        (failures >= STOP_AT).then(|| stop_message(app))
    }

    /// Records one finished call; returns the note a failure must carry.
    pub(crate) fn record(&self, app: &str, failed: bool) -> Option<String> {
        let key = self.key(app);
        let mut failures = self.failures.lock().expect("failure budget lock poisoned");
        if !failed {
            failures.remove(&key);
            return None;
        }
        let count = failures.entry(key).or_default();
        *count += 1;
        match *count {
            n if n >= STOP_AT => Some(stop_message(app)),
            n if n >= HINT_AT => Some(format!(
                "{app} has failed {n} times in a row. Stop trying workarounds such as shell commands, scripts or other apps; tell the user what failed and what you tried, unless one specific fix remains."
            )),
            _ => None,
        }
    }

    /// A call refused by schema validation still counts toward the app it names.
    pub(crate) fn note_invalid_call(&self, raw_arguments: Option<&str>, result: &mut ToolResult) {
        if let Some(app) = raw_app(raw_arguments)
            && let Some(note) = self.record(&app, true)
        {
            result.append_error_note(&note);
        }
    }
}

/// The `app` argument as the model wrote it, when the arguments are readable JSON.
pub(crate) fn raw_app(raw_arguments: Option<&str>) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw_arguments?)
        .ok()?
        .get("app")?
        .as_str()
        .filter(|app| !app.trim().is_empty())
        .map(str::to_owned)
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
    fn failures_under_a_name_carry_over_when_it_resolves() {
        let budget = FailureBudget::default();
        // Five calls that failed validation before the name ever resolved.
        for _ in 0..5 {
            budget.record("TextEdit", true);
        }
        budget.alias("TextEdit", "com.apple.TextEdit");
        let stop = budget.record("com.apple.TextEdit", true);
        assert!(
            stop.as_deref().is_some_and(
                |stop| stop.starts_with("Orca stopped computer use on com.apple.TextEdit")
            ),
            "{stop:?}"
        );
    }

    #[test]
    fn a_resolved_name_shares_its_apps_count_in_any_spelling() {
        let budget = FailureBudget::default();
        budget.alias("TextEdit", "com.apple.TextEdit");
        budget.record("TextEdit", true);
        budget.record("com.apple.textedit", true);
        let hint = budget.record("com.apple.TextEdit", true).unwrap();
        assert!(
            hint.starts_with("com.apple.TextEdit has failed 3 times"),
            "{hint}"
        );
        assert!(budget.record("Calculator", true).is_none());
    }

    #[test]
    fn an_invalid_call_counts_under_the_app_it_names() {
        let budget = FailureBudget::default();
        let request = orca_core::tool_types::ToolRequest {
            id: "t".into(),
            name: orca_core::tool_types::ToolName::Computer,
            action: orca_core::approval_types::ActionKind::Computer,
            target: None,
            raw_arguments: None,
        };
        let raw = r#"{"action":"click","app":"TextEdit","click_count":3}"#;
        let mut results: Vec<_> = (0..3)
            .map(|_| orca_core::tool_types::ToolResult::failed(&request, "schema", None))
            .collect();
        for result in &mut results {
            budget.note_invalid_call(Some(raw), result);
        }
        assert!(
            results[2]
                .error
                .as_deref()
                .unwrap()
                .contains("TextEdit has failed 3 times"),
            "{:?}",
            results[2].error
        );
        // Without a readable app name there is nothing to count.
        let mut broken = orca_core::tool_types::ToolResult::failed(&request, "schema", None);
        budget.note_invalid_call(Some("{not json"), &mut broken);
        assert_eq!(broken.error.as_deref(), Some("schema"));
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
