//! Whose windows are whose. The windows open at a turn's first computer call
//! are the user's, except those Orca opened in an earlier turn; windows that
//! appear during a turn are Orca's for the rest of the session.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use orca_computer_use::protocol::{AppInfo, WindowInfo};

use super::ComputerUseService;
use crate::extension::ExtensionData;

/// Every app's windows, by bundle ID.
pub(crate) type Snapshot = HashMap<String, Vec<WindowInfo>>;

/// Groups a window listing by bundle ID; instances of one app share an entry.
pub(crate) fn snapshot_of(apps: Vec<AppInfo>) -> Snapshot {
    let mut snapshot = Snapshot::new();
    for app in apps {
        snapshot
            .entry(app.bundle_id)
            .or_default()
            .extend(app.windows);
    }
    snapshot
}

/// The windows in `end` that `start` did not have, by bundle ID.
pub(crate) fn new_windows(start: &Snapshot, end: &Snapshot) -> Vec<(String, u32)> {
    end.iter()
        .flat_map(|(bundle_id, windows)| {
            let before = start.get(bundle_id);
            windows
                .iter()
                .filter(move |window| {
                    !before.is_some_and(|before| before.iter().any(|old| old.id == window.id))
                })
                .map(move |window| (bundle_id.clone(), window.id))
        })
        .collect()
}

/// The turn's first window snapshot. Lives in the turn's extension store.
#[derive(Default)]
pub(crate) struct TurnWindows {
    start: Mutex<Option<Snapshot>>,
}

impl TurnWindows {
    pub(crate) fn for_turn(turn_store: &ExtensionData) -> Arc<Self> {
        turn_store.get_or_init(Self::default)
    }

    /// Takes the snapshot at the turn's first computer call, before anything
    /// acts. A failure refuses the call rather than count any window as
    /// Orca's on a guess; the next call tries again.
    pub(crate) fn ensure_start(&self, service: &ComputerUseService) -> Result<(), String> {
        let mut start = self.start.lock().expect("turn windows lock poisoned");
        if start.is_none() {
            *start = Some(service.snapshot_windows().map_err(|error| {
                format!(
                    "Orca could not list the open windows, so it cannot tell which are the user's ({error}); try again"
                )
            })?);
        }
        Ok(())
    }

    pub(crate) fn start(&self) -> Option<Snapshot> {
        self.start
            .lock()
            .expect("turn windows lock poisoned")
            .clone()
    }

    /// The app's windows that were open when the turn began and that Orca did
    /// not open; none before the turn's first snapshot.
    pub(crate) fn user_windows(
        &self,
        service: &ComputerUseService,
        bundle_id: &str,
    ) -> Vec<WindowInfo> {
        let start = self.start.lock().expect("turn windows lock poisoned");
        start
            .as_ref()
            .and_then(|start| start.get(bundle_id))
            .map(|windows| {
                windows
                    .iter()
                    .filter(|window| !service.is_orca_window(bundle_id, window.id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_that_appear_between_snapshots_are_new_and_instances_share_an_entry() {
        let app = |bundle_id: &str, ids: &[u32]| AppInfo {
            bundle_id: bundle_id.into(),
            name: bundle_id.into(),
            pid: Some(1),
            category: None,
            windows: ids
                .iter()
                .map(|&id| WindowInfo {
                    id,
                    title: format!("w{id}"),
                    minimized: false,
                })
                .collect(),
        };
        let start = snapshot_of(vec![app("a", &[1]), app("a", &[2]), app("b", &[])]);
        assert_eq!(
            start["a"]
                .iter()
                .map(|window| window.id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let end = snapshot_of(vec![app("a", &[1, 2, 3]), app("b", &[4]), app("c", &[5])]);
        let mut fresh = new_windows(&start, &end);
        fresh.sort();
        assert_eq!(
            fresh,
            vec![("a".into(), 3), ("b".into(), 4), ("c".into(), 5)]
        );
    }
}
