#[path = "support/live_cleanup.rs"]
mod cleanup;

use cleanup::{
    CleanupAction, cleanup_pids, cleanup_with_probes, fixture_launch_conflicts,
    fixture_started_pids,
};
use std::collections::HashSet;

#[test]
fn fixture_receipts_include_all_started_pids_and_late_arrivals() {
    let initial = "{\"type\":\"started\",\"pid\":42}\n";
    let mut registered = fixture_started_pids(initial);
    assert_eq!(registered, HashSet::from([42]));
    let later = format!(
        "{initial}{}",
        r#"{"type":"input","pid":77}
{"type":"started","pid":43}
{"type":"started","pid":42}
{"type":"started","pid":0}
{"type":"started","pid":-1}
{"type":"started","pid":1}
{"type":"started","pid":2147483648}
{"type":"started","pid":"99"}
{"type":"started","pid":"#
    );
    registered.extend(fixture_started_pids(&later));
    assert_eq!(registered, HashSet::from([42, 43]));
}

#[test]
fn cleanup_candidates_exclude_preexisting_and_other_sessions() {
    let running = HashSet::from([10]);
    let mut owned = HashSet::from([10]);
    owned.extend(fixture_started_pids("{\"type\":\"started\",\"pid\":42}\n"));
    assert_eq!(cleanup_pids(&running, &owned), vec![42]);
    // PID 77 belongs to a different session's private log, so it is never added.
    assert!(!cleanup_pids(&running, &owned).contains(&77));
    assert_eq!(
        cleanup_pids(&HashSet::new(), &HashSet::from([-1, 0, 1, 42])),
        vec![42]
    );
}

#[test]
fn fixture_launch_reports_foreign_instances_but_allows_owned_instances() {
    let running = HashSet::from([42, 77, 88]);
    assert_eq!(
        fixture_launch_conflicts(&running, &HashSet::from([42]), |pid| {
            assert_ne!(pid, 42, "owned processes do not conflict");
            Ok(pid != 88) // Stale AppKit entries must not block the next launch.
        })
        .unwrap(),
        vec![77]
    );
    assert!(
        fixture_launch_conflicts(&running, &running, |_| panic!("all owned"))
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture_launch_conflicts(&HashSet::new(), &HashSet::new(), |_| panic!("no processes"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture_launch_conflicts(&running, &HashSet::from([42, 77]), |_| Err(
            "probe failed".into()
        )),
        Err("probe failed".into())
    );
}

#[test]
fn cleanup_escalates_until_the_process_probe_confirms_exit() {
    for (statuses, expected) in [
        (vec![true, false], vec![CleanupAction::Terminate]),
        (
            vec![true, true, false],
            vec![CleanupAction::Terminate, CleanupAction::ForceTerminate],
        ),
        (
            vec![true, true, true, false],
            vec![
                CleanupAction::Terminate,
                CleanupAction::ForceTerminate,
                CleanupAction::Sigkill,
            ],
        ),
    ] {
        let mut statuses = statuses.into_iter();
        let mut actions = vec![];
        let result = cleanup_with_probes(
            true,
            || Ok(statuses.next().expect("bounded process probes")),
            |action| actions.push(action),
        )
        .unwrap();
        assert_eq!(result, expected.last().copied());
        assert_eq!(actions, expected);
        assert!(
            statuses.next().is_none(),
            "exit must be confirmed after the last request"
        );
    }
}

#[test]
fn cleanup_without_an_appkit_handle_still_uses_signals() {
    let mut statuses = [true, true, false].into_iter();
    let mut actions = vec![];
    let result = cleanup_with_probes(
        false,
        || Ok(statuses.next().unwrap()),
        |action| actions.push(action),
    )
    .unwrap();
    assert_eq!(result, Some(CleanupAction::Sigkill));
    assert_eq!(actions, [CleanupAction::Sigterm, CleanupAction::Sigkill]);
    assert!(statuses.next().is_none());
}

#[test]
fn cleanup_cannot_report_success_while_the_process_is_alive() {
    for native_handle in [true, false] {
        assert!(cleanup_with_probes(native_handle, || Ok(true), |_| {}).is_err());
    }
}

#[test]
fn cleanup_does_nothing_for_a_gone_process() {
    for native_handle in [true, false] {
        assert_eq!(
            cleanup_with_probes(native_handle, || Ok(false), |_| panic!("already gone")).unwrap(),
            None
        );
    }
}

#[test]
fn cleanup_probe_failures_are_reported() {
    let failure = "could not inspect process".to_owned();
    assert_eq!(
        cleanup_with_probes(
            true,
            || Err(failure.clone()),
            |_| panic!("unknown process state")
        ),
        Err(failure.clone())
    );
    let mut statuses = [Ok(true), Err(failure.clone())].into_iter();
    assert_eq!(
        cleanup_with_probes(true, || statuses.next().unwrap(), |_| {}),
        Err(failure)
    );
}
