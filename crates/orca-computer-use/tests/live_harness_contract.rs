#[path = "support/live_harness.rs"]
mod harness;

use clap::Parser;
use harness::{Cli, trial};

#[test]
fn serial_tests_get_fresh_named_threads_and_panic_does_not_stop_the_suite() {
    thread_local! {
        static USED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    let tests = vec![
        trial("first", false, || {
            assert_eq!(std::thread::current().name(), Some("first"));
            assert!(!USED.replace(true));
            panic!("intentional contract-test failure");
        }),
        trial("second", false, || {
            assert_eq!(std::thread::current().name(), Some("second"));
            assert!(!USED.replace(true), "test thread_local state leaked");
        }),
        trial("ignored", true, || panic!("ignored test ran by default")),
    ];
    let result = Cli::parse_from(["test", "--test-threads=1"]).run(tests);
    assert_eq!(result.num_passed, 1);
    assert_eq!(result.num_failed, 1);
    assert_eq!(result.num_ignored, 1);
    assert_ne!(result.exit_code(), std::process::ExitCode::SUCCESS);
}

#[test]
fn multiple_exact_filters_and_ignored_mode_select_only_requested_tests() {
    let tests = vec![
        trial("one", true, || {}),
        trial("two", true, || {}),
        trial("one_suffix", true, || panic!("exact filter was ignored")),
        trial("ordinary", false, || panic!("ignored mode was ignored")),
    ];
    let result = Cli::parse_from([
        "test",
        "--ignored",
        "--exact",
        "one",
        "two",
        "--test-threads=1",
        "--nocapture",
    ])
    .run(tests);
    assert_eq!(result.num_passed, 2);
    assert_eq!(result.num_failed, 0);
    assert_eq!(result.num_ignored, 0);
}

#[test]
fn listing_never_executes_test_bodies() {
    let result =
        Cli::parse_from(["test", "--list", "--ignored"]).run(vec![trial("live", true, || {
            panic!("listing executed a test")
        })]);
    assert_eq!(result.num_passed, 0);
    assert_eq!(result.num_failed, 0);
}
