use clap::Parser;
use libtest_mimic::{Arguments, Conclusion, Trial};

#[derive(Parser)]
pub struct Cli {
    #[command(flatten)]
    args: Arguments,
    /// Additional test-name filters, matched with OR like standard libtest.
    filters: Vec<String>,
}

impl Cli {
    pub fn run(mut self, mut tests: Vec<Trial>) -> Conclusion {
        if let Some(first) = self.args.filter.take() {
            self.filters.insert(0, first);
        }
        if !self.filters.is_empty() {
            tests.retain(|test| {
                self.filters.iter().any(|filter| {
                    if self.args.exact {
                        test.name() == filter
                    } else {
                        test.name().contains(filter)
                    }
                })
            });
        }
        libtest_mimic::run(&self.args, tests)
    }
}

// mimic's serial path and worker pool both reuse threads. Spawn once per test
// so thread_local state and thread names have standard libtest semantics.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn trial(name: &'static str, ignored: bool, body: impl FnOnce() + Send + 'static) -> Trial {
    Trial::test(name, move || {
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(body)
            .map_err(|error| format!("could not spawn test thread: {error}"))?;
        if let Err(panic) = thread.join() {
            // mimic catches this and reports a failed test; the harness keeps
            // running and macOS still gets its suite-level GUI restoration.
            std::panic::resume_unwind(panic);
        }
        Ok(())
    })
    .with_ignored_flag(ignored)
}
