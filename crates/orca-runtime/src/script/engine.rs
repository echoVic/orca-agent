//! Runs one script in an embedded QuickJS: created for the script, dropped
//! after it. The engine runs no tool. It runs the script until the script can
//! go no further, hands over the calls that are waiting, and resumes it with
//! their answers.
//!
//! The script runs on a thread of its own. QuickJS asks the interrupt handler
//! whether to stop only between steps of the script, and some of its native
//! functions loop for a very long time without taking one (`join` on an
//! object whose `length` is 2**53, say). The caller's thread keeps watching
//! the clock and the cancel while the engine is busy, and when the script must
//! stop it stops waiting: the engine thread is left to end on its own.

use std::cell::RefCell;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rquickjs::allocator::{Allocator, RustAllocator};
use rquickjs::context::{EvalOptions, intrinsic};
use rquickjs::promise::PromiseState;
use rquickjs::{Context, Ctx, Exception, Function, Object, Promise, Runtime, Value as JsValue};
use serde_json::Value;

const PRELUDE: &str = include_str!("prelude.js");
/// The name a script's errors give for where they happened.
const SCRIPT_FILE: &str = "script.js";
/// A script is the body of this function. It opens on the script's first
/// line, so the lines of an error are the lines of the script.
const MAIN_OPEN: &str = "(async function main() {";
const MAIN_CLOSE: &str = "\n})";

/// The stack of the thread a script runs on.
const ENGINE_THREAD_STACK_BYTES: usize = 2 * 1024 * 1024;
/// How much of that stack the script's own calls may take before QuickJS
/// raises a `RangeError`. The rest is for the native code beneath them.
const SCRIPT_STACK_BYTES: usize = 256 * 1024;
/// How often the caller looks at the cancel and the clock while the engine is
/// silent.
const WATCH_STEP: Duration = Duration::from_millis(20);
/// How long a stopped engine is given to say it ended before it is left
/// behind.
const STOP_GRACE: Duration = Duration::from_millis(200);

/// The most text one call's arguments may be. Arguments are held outside the
/// script's memory until they are handed over.
const MAX_CALL_ARGUMENTS_BYTES: usize = 1024 * 1024;
/// The most calls that may wait to be handed over at once.
const MAX_WAITING_CALLS: usize = 1000;

const FAILED_INSIDE_THE_ENGINE: &str = "run_script failed inside the script engine";
const BROKE_ITS_FUNCTIONS: &str = "The script broke the functions it runs on";
const WAITING_ON_NOTHING: &str = "the script is waiting on a promise that nothing will resolve";
const ARGUMENTS_TOO_LONG: &str = "a tool call's arguments are limited to 1 MiB";
const TOO_MANY_WAITING: &str = "too many tool calls are waiting; await some before starting more";

const RUNNING: u8 = 0;
const CANCELLED: u8 = 1;
const OUT_OF_TIME: u8 = 2;
/// The caller stopped waiting for a reason of its own: a panic while it
/// answered a batch.
const ABANDONED: u8 = 3;

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
        let watch = Arc::new(Watch {
            stopped: AtomicU8::new(RUNNING),
            stop,
            // A wall clock too long to add to now never runs out.
            deadline: Instant::now().checked_add(limits.wall_clock),
        });
        let (to_caller, from_engine) = mpsc::channel();
        let (to_engine, from_caller) = mpsc::channel();
        let engine = thread::Builder::new()
            .name("orca-script".to_string())
            .stack_size(ENGINE_THREAD_STACK_BYTES)
            .spawn({
                let script = script.to_string();
                let watch = Arc::clone(&watch);
                move || {
                    let ended = catch_unwind(AssertUnwindSafe(|| {
                        run_engine(&script, limits, &watch, &to_caller, &from_caller)
                    }))
                    .unwrap_or_else(|_| Ok(failed(FAILED_INSIDE_THE_ENGINE)));
                    // Nobody hears this if the caller has stopped waiting.
                    let _ = to_caller.send(FromEngine::Ended(ended));
                }
            })
            .map_err(|error| {
                io::Error::other(format!("the script engine could not start: {error}"))
            })?;

        Caller {
            watch: &watch,
            limits,
            engine,
            from_engine,
            to_engine,
        }
        .wait(answer)
    }
}

/// What the engine thread tells the caller.
enum FromEngine {
    /// The calls the script started together, to be answered in this order.
    Calls(Vec<ScriptToolCall>),
    /// How the script ended.
    Ended(io::Result<ScriptOutcome>),
}

type Answers = Vec<Result<String, String>>;

/// Whether the script is still to run, read by both threads.
struct Watch {
    /// Why the script was stopped, if it was. Once set it does not change,
    /// and the engine refuses to run anything more of the script.
    stopped: AtomicU8,
    stop: Arc<dyn Fn() -> bool + Send + Sync>,
    /// `None` when the wall clock is too long to be reached.
    deadline: Option<Instant>,
}

impl Watch {
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) != RUNNING
    }

    /// Records why the script must stop, if it must, and says whether it is
    /// stopped. A cancel takes precedence over the deadline.
    fn check(&self) -> bool {
        if self.is_stopped() {
            return true;
        }
        if (self.stop)() {
            self.halt(CANCELLED);
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.halt(OUT_OF_TIME);
        } else {
            return false;
        }
        true
    }

    /// Stops the script for `why`, unless it was already stopped.
    fn halt(&self, why: u8) {
        let _ = self
            .stopped
            .compare_exchange(RUNNING, why, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// How a stopped script ended, or `None` if it was not stopped.
    fn outcome(&self, limits: ScriptEngineLimits) -> Option<io::Result<ScriptOutcome>> {
        match self.stopped.load(Ordering::SeqCst) {
            CANCELLED => Some(Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "the script was cancelled",
            ))),
            OUT_OF_TIME => Some(Ok(failed(&format!(
                "the script ran longer than {} and was stopped",
                seconds(limits.wall_clock)
            )))),
            // Only the engine thread reads this, after the caller has gone.
            ABANDONED => Some(Err(io::Error::other(
                "the caller stopped waiting for the script",
            ))),
            _ => None,
        }
    }
}

/// The caller's side: answers the engine's batches on the caller's own
/// thread, and watches the cancel and the clock while the engine is silent.
struct Caller<'a> {
    watch: &'a Watch,
    limits: ScriptEngineLimits,
    engine: JoinHandle<()>,
    from_engine: Receiver<FromEngine>,
    to_engine: Sender<Answers>,
}

impl Caller<'_> {
    fn wait(
        self,
        answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Answers,
    ) -> io::Result<ScriptOutcome> {
        loop {
            match self.from_engine.recv_timeout(WATCH_STEP) {
                Ok(FromEngine::Calls(calls)) => {
                    if self.watch.check() {
                        return self.stopped();
                    }
                    let Ok(answers) = catch_unwind(AssertUnwindSafe(|| answer(calls))) else {
                        self.watch.halt(ABANDONED);
                        self.let_go();
                        return Ok(failed(FAILED_INSIDE_THE_ENGINE));
                    };
                    if self.watch.check() {
                        return self.stopped();
                    }
                    // If the engine has ended, its outcome is already on the
                    // way.
                    let _ = self.to_engine.send(answers);
                }
                Ok(FromEngine::Ended(ended)) => {
                    let _ = self.engine.join();
                    // A script that ended after its time was up, or after a
                    // cancel, was stopped.
                    self.watch.check();
                    return self.watch.outcome(self.limits).unwrap_or(ended);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.watch.check() {
                        return self.stopped();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Ok(failed(FAILED_INSIDE_THE_ENGINE));
                }
            }
        }
    }

    /// The script was stopped while the engine ran: say how, once the engine
    /// has had a moment to end.
    fn stopped(self) -> io::Result<ScriptOutcome> {
        let limits = self.limits;
        let watch = self.watch;
        self.let_go();
        watch
            .outcome(limits)
            .unwrap_or_else(|| Ok(failed(FAILED_INSIDE_THE_ENGINE)))
    }

    /// Stops waiting for the engine. An engine waiting for answers ends at
    /// once; one busy in the script is given `STOP_GRACE` to notice that it
    /// was stopped, and is otherwise left behind: it is not joined, and it
    /// can do nothing more than end, since everything it sends or waits for
    /// fails once these channels are dropped.
    fn let_go(self) {
        let Caller {
            engine,
            from_engine,
            to_engine,
            ..
        } = self;
        drop(to_engine);
        let grace_ends = Instant::now() + STOP_GRACE;
        loop {
            let left = grace_ends.saturating_duration_since(Instant::now());
            match from_engine.recv_timeout(left) {
                // Not answered: the script is stopped.
                Ok(FromEngine::Calls(_)) => {}
                Ok(FromEngine::Ended(_)) => {
                    let _ = engine.join();
                    return;
                }
                Err(_) => return,
            }
        }
    }
}

/// The engine thread's side: creates the runtime, runs the script, drops the
/// runtime.
fn run_engine(
    script: &str,
    limits: ScriptEngineLimits,
    watch: &Arc<Watch>,
    to_caller: &Sender<FromEngine>,
    from_caller: &Receiver<Answers>,
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
    // Measured from where the runtime was created: near the top of this
    // thread's stack.
    runtime.set_max_stack_size(SCRIPT_STACK_BYTES);
    runtime.set_interrupt_handler(Some(Box::new({
        let watch = Arc::clone(watch);
        move || watch.check()
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
        watch,
        refused: &refused,
        limits,
    };
    let outcome = context.with(|ctx| drive(&ctx, script, &ending, to_caller, from_caller));
    // Nothing of the script may outlive this call.
    drop(context);
    drop(runtime);
    outcome
}

/// What the script's side of the engine has told the Rust side.
#[derive(Default)]
struct Told {
    /// Calls started since the last hand-over, each with its number.
    started: Vec<(u32, ScriptToolCall)>,
    numbered: u32,
    outcome: Option<ScriptOutcome>,
}

impl Told {
    /// Records how the script ended. The prelude reports once; a second
    /// report could only come from a script that reached the prelude's own
    /// functions, and the first outcome stands.
    fn finish(&mut self, outcome: ScriptOutcome) {
        if self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
    }
}

/// The two native functions the prelude is given: `start_call(tool,
/// arguments)`, which records a call and returns its number, and
/// `finish(kind, text)`, which records how the script ended.
fn natives<'js>(
    ctx: &Ctx<'js>,
    told: &Rc<RefCell<Told>>,
) -> rquickjs::Result<(Function<'js>, Function<'js>)> {
    let start_call = Function::new(ctx.clone(), {
        let told = Rc::clone(told);
        move |ctx: Ctx<'js>, tool: String, arguments: String| -> rquickjs::Result<u32> {
            let mut told = told.borrow_mut();
            // A refused call is not kept: its promise rejects in the script.
            if told.started.len() >= MAX_WAITING_CALLS {
                return Err(Exception::throw_message(&ctx, TOO_MANY_WAITING));
            }
            if arguments.len() > MAX_CALL_ARGUMENTS_BYTES {
                return Err(Exception::throw_message(&ctx, ARGUMENTS_TOO_LONG));
            }
            told.numbered += 1;
            let number = told.numbered;
            let arguments = serde_json::from_str(&arguments).unwrap_or(Value::Null);
            told.started
                .push((number, ScriptToolCall { tool, arguments }));
            Ok(number)
        }
    })?;
    let finish = Function::new(ctx.clone(), {
        let told = Rc::clone(told);
        move |kind: String, text: String| {
            told.borrow_mut().finish(if kind == "completed" {
                ScriptOutcome::Completed { output: text }
            } else {
                ScriptOutcome::Failed { error: text }
            });
        }
    })?;
    Ok((start_call, finish))
}

fn drive<'js>(
    ctx: &Ctx<'js>,
    script: &str,
    ending: &Ending<'_>,
    to_caller: &Sender<FromEngine>,
    from_caller: &Receiver<Answers>,
) -> io::Result<ScriptOutcome> {
    let told = Rc::new(RefCell::new(Told::default()));
    let (start_call, finish) = natives(ctx, &told).map_err(engine_error)?;

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
    // A script that closes the function early runs code of its own here, and
    // may be stopped or fail while it does.
    let main = match ctx
        .eval_with_options::<JsValue, _>(format!("{MAIN_OPEN}{script}{MAIN_CLOSE}"), options)
    {
        Ok(value) => match value.into_function() {
            Some(main) => main,
            // It closed the function and left something else in its place.
            None => return ending.failed(BROKE_ITS_FUNCTIONS.to_string()),
        },
        Err(_) => {
            let thrown = ctx.catch();
            if let Some(stopped) = ending.stopped() {
                return stopped;
            }
            let error = describe
                .call::<_, String>((thrown,))
                .unwrap_or_else(|_| "the script could not be compiled".to_string());
            return ending.failed(error);
        }
    };
    let Ok(running) = run.call::<_, Promise>((main,)) else {
        return ended_early(ctx, ending);
    };

    loop {
        // The outcome is returned as soon as the script reports it: jobs the
        // script left running are not run to their end.
        loop {
            if ending.watch.is_stopped() || told.borrow().outcome.is_some() {
                break;
            }
            if !ctx.execute_pending_job() {
                break;
            }
        }
        if let Some(stopped) = ending.stopped() {
            return stopped;
        }
        if let Some(outcome) = told.borrow_mut().outcome.take() {
            return ending.reported(outcome);
        }
        let batch = std::mem::take(&mut told.borrow_mut().started);
        if batch.is_empty() {
            // If the prelude's `run` rejected, the prelude itself failed while
            // reporting; otherwise the script waits on something that will
            // never settle.
            return ending.failed(if running.state() == PromiseState::Rejected {
                BROKE_ITS_FUNCTIONS.to_string()
            } else {
                WAITING_ON_NOTHING.to_string()
            });
        }

        let (numbers, calls): (Vec<u32>, Vec<ScriptToolCall>) = batch.into_iter().unzip();
        // Either fails only when the caller has stopped waiting, and then the
        // script is already stopped.
        if to_caller.send(FromEngine::Calls(calls)).is_err() {
            return ending.abandoned();
        }
        let Ok(answers) = from_caller.recv() else {
            return ending.abandoned();
        };
        // The clock may have run out since the caller looked.
        ending.watch.check();
        if let Some(stopped) = ending.stopped() {
            return stopped;
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

/// Turns how the script went into how it ended. Whatever else happened, a
/// stopped script was stopped, and a script that failed after an allocation
/// was refused failed for want of memory: QuickJS may throw `null` or
/// anything else when it cannot allocate the error itself.
struct Ending<'a> {
    watch: &'a Watch,
    refused: &'a AtomicBool,
    limits: ScriptEngineLimits,
}

impl Ending<'_> {
    fn stopped(&self) -> Option<io::Result<ScriptOutcome>> {
        self.watch.outcome(self.limits)
    }

    fn failed(&self, error: String) -> io::Result<ScriptOutcome> {
        if let Some(stopped) = self.stopped() {
            return stopped;
        }
        if self.refused.load(Ordering::SeqCst) {
            return Ok(failed(&format!(
                "the script used more than {} MiB",
                self.limits.memory_bytes / (1024 * 1024)
            )));
        }
        Ok(ScriptOutcome::Failed { error })
    }

    /// The outcome the script reported. A script that caught a refused
    /// allocation and went on to complete has completed.
    fn reported(&self, outcome: ScriptOutcome) -> io::Result<ScriptOutcome> {
        match outcome {
            ScriptOutcome::Completed { output } => self
                .stopped()
                .unwrap_or(Ok(ScriptOutcome::Completed { output })),
            ScriptOutcome::Failed { error } => self.failed(error),
        }
    }

    /// The caller has stopped waiting; what is returned here reaches no one.
    fn abandoned(&self) -> io::Result<ScriptOutcome> {
        self.watch.halt(ABANDONED);
        self.stopped()
            .unwrap_or_else(|| Ok(failed(FAILED_INSIDE_THE_ENGINE)))
    }
}

/// A call into the engine's own functions failed. Either the script was
/// stopped while it ran, or it ran out of memory, or the script changed what
/// those functions run on.
fn ended_early(ctx: &Ctx<'_>, ending: &Ending<'_>) -> io::Result<ScriptOutcome> {
    let _ = ctx.catch();
    ending.failed(BROKE_ITS_FUNCTIONS.to_string())
}

fn failed(error: &str) -> ScriptOutcome {
    ScriptOutcome::Failed {
        error: error.to_string(),
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
    fn a_second_report_of_how_the_script_ended_is_ignored() {
        let runtime = Runtime::new().unwrap();
        let context = Context::full(&runtime).unwrap();
        let told = Rc::new(RefCell::new(Told::default()));

        context.with(|ctx| {
            let (_, finish) = natives(&ctx, &told).unwrap();
            finish.call::<_, ()>(("completed", "first")).unwrap();
            finish.call::<_, ()>(("failed", "second")).unwrap();
        });

        assert_eq!(
            told.borrow().outcome,
            Some(ScriptOutcome::Completed {
                output: "first".to_string()
            })
        );
    }

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
