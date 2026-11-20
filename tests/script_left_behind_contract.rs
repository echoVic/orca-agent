//! What becomes of a script that was stopped inside a native call the
//! interrupt handler does not reach: it is left behind, and counted until
//! that call returns.
//!
//! The count is one the whole process shares, so this is one test in a file
//! of its own. It leaves two busy threads behind for the rest of its process.

use std::sync::Arc;
use std::time::{Duration, Instant};

use orca_runtime::script::engine::{
    ScriptEngine, ScriptEngineLimits, ScriptOutcome, scripts_left_behind,
};

/// Runs `script` with a wall clock it cannot meet and returns once it was
/// stopped.
fn run_until_stopped(script: &str) {
    let outcome = ScriptEngine::run(
        script,
        ScriptEngineLimits {
            wall_clock: Duration::from_millis(300),
            memory_bytes: 64 * 1024 * 1024,
        },
        &mut |calls| calls.iter().map(|_| Ok(String::new())).collect(),
        Arc::new(|| false),
    )
    .expect("the script engine ran");
    assert_eq!(
        outcome,
        ScriptOutcome::Failed {
            error: "the script ran longer than 300ms and was stopped".to_string()
        },
        "{script}"
    );
}

/// Waits until `count` scripts are left behind, for at most `within`.
fn left_behind_becomes(count: usize, within: Duration) -> bool {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if scripts_left_behind() == count {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    scripts_left_behind() == count
}

#[test]
fn a_script_left_behind_is_counted_until_it_ends() {
    assert_eq!(scripts_left_behind(), 0);

    // A loop the interrupt does not reach, which ends on its own after a few
    // seconds: it is left behind when its script is stopped, and is no
    // longer counted once it has ended.
    let started = Instant::now();
    run_until_stopped(r#"return new Array(2 ** 26).join("").length;"#);
    assert_eq!(scripts_left_behind(), 1, "counted while it runs");
    assert!(
        left_behind_becomes(0, Duration::from_secs(600)),
        "not counted once it has ended"
    );
    eprintln!("the loop took {:?}", started.elapsed());

    // Loops that do not end stay counted.
    run_until_stopped("return [].reverse.call({ length: 2 ** 53 - 1 });");
    assert_eq!(scripts_left_behind(), 1);
    run_until_stopped(r#"return Array.prototype.join.call({ length: 2 ** 53 - 1 }, "");"#);
    assert_eq!(scripts_left_behind(), 2);

    // A script that ends on its own was never left behind.
    let outcome = ScriptEngine::run(
        "return 1 + 1;",
        ScriptEngineLimits {
            wall_clock: Duration::from_secs(20),
            memory_bytes: 64 * 1024 * 1024,
        },
        &mut |calls| calls.iter().map(|_| Ok(String::new())).collect(),
        Arc::new(|| false),
    )
    .expect("the script engine ran");
    assert_eq!(
        outcome,
        ScriptOutcome::Completed {
            output: "2".to_string()
        }
    );
    assert_eq!(scripts_left_behind(), 2);
}
