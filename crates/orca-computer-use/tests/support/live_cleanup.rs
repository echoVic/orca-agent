use std::collections::HashSet;

pub fn fixture_started_pids(text: &str) -> HashSet<i32> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["type"] == "started")
        .filter_map(|event| {
            event["pid"]
                .as_i64()
                .and_then(|pid| i32::try_from(pid).ok())
        })
        .filter(|pid| *pid > 1)
        .collect()
}

pub fn cleanup_pids(running: &HashSet<i32>, launched: &HashSet<i32>) -> Vec<i32> {
    let mut pids: Vec<_> = launched
        .difference(running)
        .copied()
        .filter(|pid| *pid > 1)
        .collect();
    pids.sort_unstable();
    pids
}

pub fn fixture_launch_conflicts(
    running: &HashSet<i32>,
    owned: &HashSet<i32>,
    mut is_alive: impl FnMut(i32) -> Result<bool, String>,
) -> Result<Vec<i32>, String> {
    let mut conflicts = vec![];
    for pid in cleanup_pids(owned, running) {
        if is_alive(pid)? {
            conflicts.push(pid);
        }
    }
    Ok(conflicts)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupAction {
    Terminate,
    ForceTerminate,
    Sigterm,
    Sigkill,
}

pub fn cleanup_with_probes(
    native_handle: bool,
    mut is_alive: impl FnMut() -> Result<bool, String>,
    mut stop: impl FnMut(CleanupAction),
) -> Result<Option<CleanupAction>, String> {
    if !is_alive()? {
        return Ok(None);
    }
    let actions: &[CleanupAction] = if native_handle {
        &[
            CleanupAction::Terminate,
            CleanupAction::ForceTerminate,
            CleanupAction::Sigkill,
        ]
    } else {
        &[CleanupAction::Sigterm, CleanupAction::Sigkill]
    };
    for &action in actions {
        stop(action);
        if !is_alive()? {
            return Ok(Some(action));
        }
    }
    Err("process remains alive after SIGKILL".into())
}
