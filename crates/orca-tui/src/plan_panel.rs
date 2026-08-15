use orca_core::plan_types::PlanItem;

use crate::types::AppState;

#[derive(Default)]
pub(crate) struct PlanPanelState {
    current_plan: Option<(Option<String>, Vec<PlanItem>)>,
    update_failed: bool,
}

impl PlanPanelState {
    pub(crate) fn apply_update(&mut self, explanation: Option<String>, plan: Vec<PlanItem>) {
        self.update_failed = false;
        self.current_plan = (!plan.is_empty()).then_some((explanation, plan));
    }

    pub(crate) fn restore(&mut self, plan: Option<(Option<String>, Vec<PlanItem>)>) {
        self.current_plan = plan;
    }

    pub(crate) fn reset_for_session(&mut self) {
        self.current_plan = None;
        self.update_failed = false;
    }

    pub(crate) fn mark_update_failed(&mut self) {
        self.update_failed = true;
    }

    pub(crate) fn take_for_archive(&mut self) -> Option<(Option<String>, Vec<PlanItem>)> {
        self.update_failed = false;
        self.current_plan
            .take()
            .filter(|(_, plan)| !plan.is_empty())
    }

    fn current_plan(&self) -> Option<&(Option<String>, Vec<PlanItem>)> {
        self.current_plan.as_ref()
    }

    fn update_failed(&self) -> bool {
        self.update_failed
    }
}

impl AppState {
    pub fn current_plan(&self) -> Option<&(Option<String>, Vec<PlanItem>)> {
        self.plan_panel.current_plan()
    }

    pub fn plan_update_failed(&self) -> bool {
        self.plan_panel.update_failed()
    }

    pub(crate) fn apply_plan_update(&mut self, explanation: Option<String>, plan: Vec<PlanItem>) {
        self.plan_panel.apply_update(explanation, plan);
    }

    pub(crate) fn restore_plan(&mut self, plan: Option<(Option<String>, Vec<PlanItem>)>) {
        self.plan_panel.restore(plan);
    }

    pub(crate) fn clear_plan_panel(&mut self) {
        self.plan_panel.reset_for_session();
    }

    pub(crate) fn mark_plan_update_failed(&mut self) {
        self.plan_panel.mark_update_failed();
    }

    pub(crate) fn take_plan_for_archive(&mut self) -> Option<(Option<String>, Vec<PlanItem>)> {
        self.plan_panel.take_for_archive()
    }

    #[cfg(test)]
    pub(crate) fn replace_plan_for_test(&mut self, plan: Option<(Option<String>, Vec<PlanItem>)>) {
        self.restore_plan(plan);
    }
}

#[cfg(test)]
mod tests {
    use orca_core::plan_types::{PlanItem, PlanStatus};

    use super::PlanPanelState;

    fn item(step: &str) -> PlanItem {
        PlanItem {
            step: step.to_string(),
            status: PlanStatus::Pending,
        }
    }

    #[test]
    fn plan_panel_replaces_marks_stale_and_transfers_archive_once() {
        let mut panel = PlanPanelState::default();
        panel.apply_update(Some("inspect".to_string()), vec![item("Inspect")]);
        panel.mark_update_failed();
        assert!(panel.update_failed());

        panel.apply_update(None, Vec::new());
        assert!(panel.current_plan().is_none());
        assert!(!panel.update_failed());

        panel.apply_update(None, vec![item("Patch")]);
        assert_eq!(panel.take_for_archive().unwrap().1[0].step, "Patch");
        assert!(panel.current_plan().is_none());
        assert!(panel.take_for_archive().is_none());
    }

    #[test]
    fn plan_panel_restore_preserves_stale_until_session_clear() {
        let mut panel = PlanPanelState::default();
        panel.apply_update(None, vec![item("Live")]);
        panel.mark_update_failed();

        panel.restore(Some((Some("resumed".to_string()), vec![item("Resume")])));
        assert!(panel.update_failed());
        assert_eq!(panel.current_plan().unwrap().1[0].step, "Resume");

        panel.reset_for_session();
        assert!(panel.current_plan().is_none());
        assert!(!panel.update_failed());
    }
}
