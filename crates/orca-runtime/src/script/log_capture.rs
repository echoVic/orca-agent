//! Bounded console output shared with the caller if the script is stopped.

use std::sync::{Arc, Mutex, PoisonError};

use orca_core::tool_types::{ToolOutputTruncation, truncate_output_with_policy};
use rquickjs::{Ctx, Function};

pub(super) struct LogCapture {
    policy: ToolOutputTruncation,
    log: Mutex<ScriptLog>,
}

struct ScriptLog {
    text: Option<String>,
    bytes_left: usize,
}

impl LogCapture {
    pub(super) fn new(policy: ToolOutputTruncation) -> Self {
        Self {
            policy,
            log: Mutex::new(ScriptLog {
                text: None,
                bytes_left: policy.retained_byte_budget(),
            }),
        }
    }

    pub(super) fn byte_budget(&self) -> usize {
        self.policy.retained_byte_budget()
    }

    pub(super) fn bridge<'js>(
        self: &Arc<Self>,
        ctx: &Ctx<'js>,
        is_stopped: impl Fn() -> bool + 'static,
    ) -> rquickjs::Result<Function<'js>> {
        let capture = Arc::clone(self);
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, line: rquickjs::String<'js>| {
                if is_stopped() {
                    return 0;
                }
                let line = match line.to_string() {
                    Ok(line) => line,
                    Err(error) => {
                        if matches!(error, rquickjs::Error::Exception) {
                            let _ = ctx.catch();
                        }
                        return 0;
                    }
                };
                let mut log = capture.log.lock().unwrap_or_else(PoisonError::into_inner);
                log.push(&line)
            },
        )?
        .with_name("__orcaLogLine")
    }

    pub(super) fn append_to_stop_error(&self, error: &mut String) {
        let text = self
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .text
            .clone();
        if let Some(text) = text {
            let (text, _) = truncate_output_with_policy(text, self.policy);
            error.push_str("\n\nLogged before it was stopped:\n");
            error.push_str(&text);
        }
    }
}

impl ScriptLog {
    /// Returns the byte budget for the next line, allowing for its separator.
    fn push(&mut self, line: &str) -> usize {
        if let Some(text) = &mut self.text {
            text.push('\n');
            self.bytes_left -= 1;
        }
        let end = line.floor_char_boundary(self.bytes_left);
        self.text
            .get_or_insert_with(String::new)
            .push_str(&line[..end]);
        self.bytes_left = if end == line.len() {
            self.bytes_left - end
        } else {
            0
        };
        self.bytes_left.saturating_sub(1)
    }
}
