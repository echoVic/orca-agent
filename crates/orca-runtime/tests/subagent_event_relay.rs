use orca_runtime::subagent_event_relay::{
    AppendResult, RelayLease, RelayRecord, RelayTaskType, SubagentEventRelay,
};
use orca_runtime::surface::SurfaceCommitId;

fn commit_id(seed: u8) -> SurfaceCommitId {
    let mut bytes = [seed; 16];
    bytes[6] = 0x70 | (seed & 0x0f);
    bytes[8] = 0x80 | (seed & 0x3f);
    SurfaceCommitId::try_from_bytes(bytes).expect("valid UUIDv7 commit id")
}

#[test]
fn relay_reopens_in_order_and_deduplicates_stable_commit() {
    let temp = tempfile::tempdir().expect("temporary relay root");
    let lease = RelayLease::new("task-1", RelayTaskType::Subagent, "owner-a", 1, "attempt-1")
        .expect("valid relay lease");
    let relay = SubagentEventRelay::open(temp.path(), lease.clone()).expect("open relay");

    let first = RelayRecord::new(&lease, 1, commit_id(1), b"started".to_vec());
    let second = RelayRecord::new(&lease, 2, commit_id(2), b"progress".to_vec());
    assert!(matches!(
        relay.append(first.clone()).expect("append first"),
        AppendResult::Appended {
            source_sequence: 1,
            ..
        }
    ));
    assert!(matches!(
        relay.append(first).expect("duplicate first"),
        AppendResult::AlreadyApplied {
            source_sequence: 1,
            ..
        }
    ));
    assert!(matches!(
        relay.append(second).expect("append second"),
        AppendResult::Appended {
            source_sequence: 2,
            ..
        }
    ));

    drop(relay);
    let reopened = SubagentEventRelay::open(temp.path(), lease).expect("reopen relay");
    let page = reopened.read_page(0).expect("read relay page");
    assert_eq!(
        page.records
            .iter()
            .map(|record| record.source_sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(page.records[0].task_type, RelayTaskType::Subagent);
}
