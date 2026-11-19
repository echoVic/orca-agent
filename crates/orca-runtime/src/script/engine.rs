//! Runs one script in an embedded QuickJS: created for the script, dropped
//! after it. The engine runs no tool. It runs the script until the script can
//! go no further, hands over the calls that are waiting, and resumes it with
//! their answers.

use std::cell::RefCell;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use rquickjs::allocator::{Allocator, RustAllocator};
use rquickjs::context::{EvalOptions, intrinsic};
use rquickjs::promise::PromiseState;
use rquickjs::{Context, Ctx, Function, Object, Promise, Runtime, Value as JsValue};
use serde_json::Value;

const PRELUDE: &str = include_str!("prelude.js");
/// The name a script's errors give for where they happened.
const SCRIPT_FILE: &str = "script.js";
/// A script is the body of this function. It opens on the script's first
/// line, so the lines of an error are the lines of the script.
const MAIN_OPEN: &str = "(async function main() {";
const MAIN_CLOSE: &str = "\n})";

const RUNNING: u8 = 0;
const CANCELLED: u8 = 1;
const OUT_OF_TIME: u8 = 2;

/// What stops a script that the script itself does not end.
#[derive(Clone, Copy, Debug)]
pub struct ScriptEngineLimits {
    /// How long the script may run, tool calls included.
    pub wall_clock: Duration,
    /// How much memory the script may hold.
    pub memory_bytes: usize,
}

/// One tool call a script made.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptToolCall {
    pub tool: String,
    pub arguments: Value,
}

/// How a script ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptOutcome {
    Completed { output: String },
    Failed { error: String },
}

/// What the script's side of the engine has told the Rust side.
#[derive(Default)]
struct Told {
    /// Calls started since the last hand-over, each with its number.
    started: Vec<(u32, ScriptToolCall)>,
    numbered: u32,
    outcome: Option<ScriptOutcome>,
}

pub struct ScriptEngine;

impl ScriptEngine {
    /// Runs `script` to its end. `answer` is given the calls the script
    /// started together and returns one answer for each, in the same order:
    /// `Ok(text)` is what a call resolves to, `Err(text)` what it rejects
    /// with. A script stopped because `stop` said so is an `Interrupted`
    /// error, not an outcome.
    pub fn run(
        script: &str,
        limits: ScriptEngineLimits,
        answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>,
        stop: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> io::Result<ScriptOutcome> {
        match catch_unwind(AssertUnwindSafe(|| run(script, limits, answer, stop))) {
            Ok(result) => result,
            Err(_) => Ok(ScriptOutcome::Failed {
                error: "run_script failed inside the script engine".to_string(),
            }),
        }
    }
}

fn run(
    script: &str,
    limits: ScriptEngineLimits,
    answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
) -> io::Result<ScriptOutcome> {
    let refused = Arc::new(AtomicBool::new(false));
    // QuickJS's own limit stays off: the allocator enforces the limit, so
    // that every refusal is one it saw.
    let runtime = Runtime::new_with_alloc(LimitedAllocator {
        limit: limits.memory_bytes,
        held: 0,
        refused: Arc::clone(&refused),
    })
    .map_err(engine_error)?;

    // Why the script was stopped, if it was. Once set, the engine refuses to
    // run anything more of the script.
    let stopped = Arc::new(AtomicU8::new(RUNNING));
    let deadline = Instant::now() + limits.wall_clock;
    runtime.set_interrupt_handler(Some(Box::new({
        let stopped = Arc::clone(&stopped);
        let stop = Arc::clone(&stop);
        move || {
            if stopped.load(Ordering::Relaxed) != RUNNING {
                return true;
            }
            if stop() {
                stopped.store(CANCELLED, Ordering::Relaxed);
            } else if Instant::now() >= deadline {
                stopped.store(OUT_OF_TIME, Ordering::Relaxed);
            } else {
                return false;
            }
            true
        }
    })));

    let context = Context::custom::<(
        intrinsic::Date,
        intrinsic::Eval,
        intrinsic::RegExpCompiler,
        intrinsic::RegExp,
        intrinsic::Json,
        intrinsic::Proxy,
        intrinsic::MapSet,
        intrinsic::TypedArrays,
        intrinsic::Promise,
        intrinsic::WeakRef,
    )>(&runtime)
    .map_err(engine_error)?;
    let ending = Ending {
        stopped: &stopped,
        refused: &refused,
        limits,
    };
    let outcome = context.with(|ctx| drive(&ctx, script, &ending, answer, deadline, &*stop));
    // Nothing of the script may outlive this call.
    drop(context);
    drop(runtime);
    outcome
}

fn drive<'js>(
    ctx: &Ctx<'js>,
    script: &str,
    ending: &Ending<'_>,
    answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>,
    deadline: Instant,
    stop: &dyn Fn() -> bool,
) -> io::Result<ScriptOutcome> {
    let Ending {
        stopped, limits, ..
    } = *ending;
    let told = Rc::new(RefCell::new(Told::default()));

    let start_call = Function::new(ctx.clone(), {
        let told = Rc::clone(&told);
        move |tool: String, arguments: String| -> u32 {
            let mut told = told.borrow_mut();
            told.numbered += 1;
            let number = told.numbered;
            let arguments = serde_json::from_str(&arguments).unwrap_or(Value::Null);
            told.started
                .push((number, ScriptToolCall { tool, arguments }));
            number
        }
    })
    .map_err(engine_error)?;
    let finish = Function::new(ctx.clone(), {
        let told = Rc::clone(&told);
        move |kind: String, text: String| {
            told.borrow_mut().outcome = Some(if kind == "completed" {
                ScriptOutcome::Completed { output: text }
            } else {
                ScriptOutcome::Failed { error: text }
            });
        }
    })
    .map_err(engine_error)?;

    let install: Function = ctx.eval(PRELUDE).map_err(engine_error)?;
    let api: Object = install
        .call((start_call, finish, MAIN_OPEN.len()))
        .map_err(engine_error)?;
    let run: Function = api.get("run").map_err(engine_error)?;
    let deliver: Function = api.get("deliver").map_err(engine_error)?;
    let describe: Function = api.get("describe").map_err(engine_error)?;

    let mut options = EvalOptions::default();
    options.strict = false;
    options.filename = Some(SCRIPT_FILE.to_string());
    let main: Function =
        match ctx.eval_with_options(format!("{MAIN_OPEN}{script}{MAIN_CLOSE}"), options) {
            Ok(main) => main,
            // The script does not parse. Nothing of it ran.
            Err(_) => {
                let error = describe
                    .call::<_, String>((ctx.catch(),))
                    .unwrap_or_else(|_| "the script could not be compiled".to_string());
                return Ok(ending.failed(error));
            }
        };
    let Ok(running) = run.call::<_, Promise>((main,)) else {
        return ended_early(ctx, ending);
    };

    loop {
        while ctx.execute_pending_job() {
            if stopped.load(Ordering::Relaxed) != RUNNING {
                break;
            }
        }
        if let Some(ended) = stopped_outcome(stopped, limits) {
            return ended;
        }
        if let Some(outcome) = told.borrow_mut().outcome.take() {
            return Ok(ending.reported(outcome));
        }
        let batch = std::mem::take(&mut told.borrow_mut().started);
        if batch.is_empty() {
            // If the prelude's `run` rejected, the prelude itself failed while
            // reporting; otherwise the script waits on something that will
            // never settle.
            return Ok(ending.failed(if running.state() == PromiseState::Rejected {
                "The script broke the functions it runs on".to_string()
            } else {
                "the script is waiting on a promise that nothing will resolve".to_string()
            }));
        }

        let (numbers, calls): (Vec<u32>, Vec<ScriptToolCall>) = batch.into_iter().unzip();
        let answers = answer(calls);
        // A cancel takes precedence over the deadline, as in the interrupt
        // handler.
        if stop() {
            stopped
                .compare_exchange(RUNNING, CANCELLED, Ordering::Relaxed, Ordering::Relaxed)
                .ok();
        }
        if Instant::now() >= deadline {
            stopped
                .compare_exchange(RUNNING, OUT_OF_TIME, Ordering::Relaxed, Ordering::Relaxed)
                .ok();
        }
        if let Some(ended) = stopped_outcome(stopped, limits) {
            return ended;
        }
        for (number, answer) in numbers.into_iter().zip(answers) {
            let (ok, text) = match answer {
                Ok(text) => (true, text),
                Err(text) => (false, text),
            };
            if deliver.call::<_, JsValue>((number, ok, text)).is_err() {
                return ended_early(ctx, ending);
            }
        }
    }
}

/// Turns how the script went into how it ended. A script that failed after an
/// allocation was refused failed for want of memory, whatever it threw:
/// QuickJS may throw `null` when it cannot allocate the error itself.
#[derive(Clone, Copy)]
struct Ending<'a> {
    stopped: &'a AtomicU8,
    refused: &'a AtomicBool,
    limits: ScriptEngineLimits,
}

impl Ending<'_> {
    fn failed(&self, error: String) -> ScriptOutcome {
        if self.refused.load(Ordering::SeqCst) {
            return ScriptOutcome::Failed {
                error: format!(
                    "the script used more than {} MiB",
                    self.limits.memory_bytes / (1024 * 1024)
                ),
            };
        }
        ScriptOutcome::Failed { error }
    }

    /// The outcome the script reported. A script that caught a refused
    /// allocation and went on to complete has completed.
    fn reported(&self, outcome: ScriptOutcome) -> ScriptOutcome {
        match outcome {
            ScriptOutcome::Failed { error } => self.failed(error),
            completed => completed,
        }
    }
}

/// The script was stopped: say why.
fn stopped_outcome(
    stopped: &AtomicU8,
    limits: ScriptEngineLimits,
) -> Option<io::Result<ScriptOutcome>> {
    match stopped.load(Ordering::Relaxed) {
        CANCELLED => Some(Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "the script was cancelled",
        ))),
        OUT_OF_TIME => Some(Ok(ScriptOutcome::Failed {
            error: format!(
                "the script ran longer than {} and was stopped",
                seconds(limits.wall_clock)
            ),
        })),
        _ => None,
    }
}

/// A call into the engine's own functions failed. Either the script was
/// stopped while it ran, or it ran out of memory, or the script changed what
/// those functions run on.
fn ended_early(ctx: &Ctx<'_>, ending: &Ending<'_>) -> io::Result<ScriptOutcome> {
    let _ = ctx.catch();
    match stopped_outcome(ending.stopped, ending.limits) {
        Some(ended) => ended,
        None => Ok(ending.failed("The script broke the functions it runs on".to_string())),
    }
}

fn engine_error(error: rquickjs::Error) -> io::Error {
    io::Error::other(format!("the script engine could not start: {error}"))
}

fn seconds(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

/// QuickJS's allocator for one script: the binding's own, counting what the
/// script holds and refusing what would take it past its limit. A refusal is
/// recorded, so the engine knows the script ran out of memory whatever
/// QuickJS then throws.
struct LimitedAllocator {
    limit: usize,
    /// What the blocks handed out and not yet freed take, each with its
    /// overhead.
    held: usize,
    refused: Arc<AtomicBool>,
}

/// What one block costs beyond what it holds: the binding's header and the
/// system allocator's own.
const BLOCK_OVERHEAD: usize = 16;

impl LimitedAllocator {
    /// Whether `more` bytes fit beside what is held; if not, the refusal is
    /// recorded. `None` is a size too large to compute.
    fn fits(&mut self, more: Option<usize>) -> bool {
        let fits = more
            .and_then(|more| more.checked_add(self.held))
            .is_some_and(|held| held <= self.limit);
        if !fits {
            self.refused.store(true, Ordering::SeqCst);
        }
        fits
    }

    /// What a block of `size` bytes would take: the binding rounds each
    /// block up to 8 bytes.
    fn wanted(size: usize) -> Option<usize> {
        size.checked_next_multiple_of(8)?
            .checked_add(BLOCK_OVERHEAD)
    }

    /// What a block the binding handed out takes.
    ///
    /// # Safety
    ///
    /// `ptr` is a block `RustAllocator` handed out that is not yet freed.
    unsafe fn cost(ptr: *mut u8) -> usize {
        // SAFETY: as the caller promises.
        unsafe { RustAllocator::usable_size(ptr) }.saturating_add(BLOCK_OVERHEAD)
    }

    /// Counts a block just handed out, if one was.
    fn counted(&mut self, ptr: *mut u8) -> *mut u8 {
        if !ptr.is_null() {
            // SAFETY: `ptr` was just handed out by `RustAllocator`.
            let cost = unsafe { Self::cost(ptr) };
            self.held = self.held.saturating_add(cost);
        }
        ptr
    }
}

// SAFETY: every block is `RustAllocator`'s, which meets the trait's contract.
// This wrapper only adds refusals, which return null as the trait allows, and
// passes a block back to `RustAllocator` only as the trait's callers pass it
// here: a block this allocator handed out and has not freed.
unsafe impl Allocator for LimitedAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.fits(Self::wanted(size)) {
            return std::ptr::null_mut();
        }
        let ptr = RustAllocator.alloc(size);
        self.counted(ptr)
    }

    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        // `RustAllocator` answers an empty request with null; that is not a
        // refusal.
        if count == 0 || size == 0 {
            return std::ptr::null_mut();
        }
        if !self.fits(count.checked_mul(size).and_then(Self::wanted)) {
            return std::ptr::null_mut();
        }
        let ptr = RustAllocator.calloc(count, size);
        self.counted(ptr)
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        // SAFETY: `ptr` is a block this allocator handed out and has not
        // freed (the trait's contract for callers of `dealloc`).
        let cost = unsafe { Self::cost(ptr) };
        self.held = self.held.saturating_sub(cost);
        // SAFETY: as above.
        unsafe { RustAllocator.dealloc(ptr) }
    }

    unsafe fn realloc(&mut self, ptr: *mut u8, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(new_size);
        }
        // SAFETY: `ptr` is a block this allocator handed out and has not
        // freed (the trait's contract for callers of `realloc`).
        let old = unsafe { Self::cost(ptr) };
        // A block that does not grow is never refused.
        let grows_by = Self::wanted(new_size).map(|new| new.saturating_sub(old));
        if grows_by != Some(0) && !self.fits(grows_by) {
            // The old block stays as it was, as `realloc` leaves it on
            // failure.
            return std::ptr::null_mut();
        }
        // SAFETY: as above.
        let moved = unsafe { RustAllocator.realloc(ptr, new_size) };
        if !moved.is_null() {
            self.held = self.held.saturating_sub(old);
        }
        self.counted(moved)
    }

    unsafe fn usable_size(ptr: *mut u8) -> usize {
        // SAFETY: `ptr` is a block this allocator handed out and has not
        // freed (the trait's contract for callers of `usable_size`).
        unsafe { RustAllocator::usable_size(ptr) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allocator_refuses_past_its_limit_and_records_it() {
        let refused = Arc::new(AtomicBool::new(false));
        let mut allocator = LimitedAllocator {
            limit: 4096,
            held: 0,
            refused: Arc::clone(&refused),
        };

        let small = allocator.alloc(1000);
        assert!(!small.is_null());
        assert!(!refused.load(Ordering::SeqCst));
        // SAFETY: `small` came from this allocator and is not freed.
        let grown = unsafe { allocator.realloc(small, 2000) };
        assert!(!grown.is_null());
        assert!(allocator.alloc(4096).is_null(), "past the limit");
        assert!(refused.load(Ordering::SeqCst));
        // SAFETY: `grown` came from this allocator and is not freed.
        let refused_growth = unsafe { allocator.realloc(grown, 8192) };
        assert!(refused_growth.is_null(), "the old block stays");
        assert!(allocator.calloc(usize::MAX, 2).is_null(), "an overflow");
        // SAFETY: `grown` came from this allocator and is not freed.
        unsafe { allocator.dealloc(grown) };
        assert_eq!(allocator.held, 0);
        let again = allocator.alloc(3000);
        assert!(!again.is_null(), "what was freed is free again");
        // SAFETY: `again` came from this allocator and is not freed.
        unsafe { allocator.dealloc(again) };
    }
}
