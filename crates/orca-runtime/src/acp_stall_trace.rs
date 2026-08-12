//! Test-build-only phase timeline for the ACP terminal cleanup stall
//! investigation (`docs/superpowers/specs/2026-08-13-acp-terminal-stall-
//! instrumentation.md`). Call sites are `#[cfg(test)]`-gated in the
//! production paths; this module only exists in test builds. Events are a
//! bounded ring of timestamped phases per process, flushed on the
//! test-side deadline. Silent unless `ORCA_ACP_STALL_TRACE=1`.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct TraceEvent {
    elapsed_ms: u128,
    thread: String,
    phase: &'static str,
    detail: String,
}

fn trace_state() -> &'static Mutex<Vec<TraceEvent>> {
    static STATE: OnceLock<Mutex<Vec<TraceEvent>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Vec::new()))
}

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("ORCA_ACP_STALL_TRACE").is_some())
}

const RING_CAPACITY: usize = 256;

pub(crate) fn record(phase: &'static str, detail: &str) {
    if !enabled() {
        return;
    }
    let start = *START_TIME.get_or_init(Instant::now);
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();
    let mut events = trace_state().lock().expect("stall trace lock");
    if events.len() >= RING_CAPACITY {
        events.remove(0);
    }
    events.push(TraceEvent {
        elapsed_ms: start.elapsed().as_millis(),
        thread,
        phase,
        detail: detail.to_string(),
    });
}

static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Prints the accumulated timeline (call from the panicking test thread
/// when its deadline fires).
pub(crate) fn flush_and_print() {
    if !enabled() {
        return;
    }
    let events = trace_state().lock().expect("stall trace lock");
    eprintln!("acp-stall-trace begin ({} events)", events.len());
    for event in events.iter() {
        eprintln!(
            "acp-stall-trace +{:>9}ms [{}] {} {}",
            event.elapsed_ms, event.thread, event.phase, event.detail
        );
    }
    eprintln!("acp-stall-trace end");
}
