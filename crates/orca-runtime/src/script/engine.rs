//! Runs one script in an embedded QuickJS: created for the script, dropped
//! after it. The engine runs no tool. It runs the script until the script can
//! go no further, hands over the calls that are waiting, and resumes it with
//! their answers.

use std::cell::RefCell;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use rquickjs::context::{EvalOptions, intrinsic};
use rquickjs::{Context, Ctx, Function, Object, Runtime, Value as JsValue};
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
    let runtime = Runtime::new().map_err(engine_error)?;
    runtime.set_memory_limit(limits.memory_bytes);

    // Why the script was stopped, if it was. Once set, the engine refuses to
    // run anything more of the script.
    let stopped = Arc::new(AtomicU8::new(RUNNING));
    let deadline = Instant::now() + limits.wall_clock;
    runtime.set_interrupt_handler(Some(Box::new({
        let stopped = Arc::clone(&stopped);
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
    let outcome = context.with(|ctx| drive(&ctx, script, limits, answer, &stopped, deadline));
    // Nothing of the script may outlive this call.
    drop(context);
    drop(runtime);
    outcome
}

fn drive<'js>(
    ctx: &Ctx<'js>,
    script: &str,
    limits: ScriptEngineLimits,
    answer: &mut dyn FnMut(Vec<ScriptToolCall>) -> Vec<Result<String, String>>,
    stopped: &AtomicU8,
    deadline: Instant,
) -> io::Result<ScriptOutcome> {
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
                return Ok(ScriptOutcome::Failed { error });
            }
        };
    if run.call::<_, JsValue>((main,)).is_err() {
        return ended_early(ctx, stopped, limits);
    }

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
            return Ok(name_the_memory_limit(outcome, limits));
        }
        let batch = std::mem::take(&mut told.borrow_mut().started);
        if batch.is_empty() {
            return Ok(ScriptOutcome::Failed {
                error: "the script is waiting on a promise that nothing will resolve".to_string(),
            });
        }

        let (numbers, calls): (Vec<u32>, Vec<ScriptToolCall>) = batch.into_iter().unzip();
        let answers = answer(calls);
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
                return ended_early(ctx, stopped, limits);
            }
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
/// stopped while it ran, or the script changed what those functions run on.
fn ended_early(
    ctx: &Ctx<'_>,
    stopped: &AtomicU8,
    limits: ScriptEngineLimits,
) -> io::Result<ScriptOutcome> {
    let _ = ctx.catch();
    match stopped_outcome(stopped, limits) {
        Some(ended) => ended,
        None => Ok(ScriptOutcome::Failed {
            error: "The script broke the functions it runs on".to_string(),
        }),
    }
}

/// QuickJS reports a refused allocation as an ordinary error. Say which limit
/// it was.
fn name_the_memory_limit(outcome: ScriptOutcome, limits: ScriptEngineLimits) -> ScriptOutcome {
    match outcome {
        ScriptOutcome::Failed { error } if error.contains("out of memory") => {
            ScriptOutcome::Failed {
                error: format!(
                    "the script used more than {} MiB",
                    limits.memory_bytes / (1024 * 1024)
                ),
            }
        }
        other => other,
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
