use std::io;

use crossbeam_channel as mpsc;
use crossterm::event::Event;
use orca_core::config::ThemeName;
use ratatui::backend::CrosstermBackend;

use crate::capability_backend::CapabilityBackend;
use crate::input_runtime::{InputControl, InputRuntime, InputRuntimeOptions};
use crate::presentation::InlineTerminal;
use crate::stdio_guard::RetryWriter;
use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
use crate::theme::Theme;

type InlineBackend = CapabilityBackend<CrosstermBackend<RetryWriter<std::io::Stdout>>>;
type TerminalPresentationResources = (InlineTerminal, TerminalPresentation, InputRuntime);

pub(crate) struct TerminalInputReceivers {
    events: mpsc::Receiver<Event>,
    focus_events: mpsc::Receiver<Event>,
    controls: mpsc::Receiver<InputControl>,
}

impl TerminalInputReceivers {
    pub(crate) fn into_parts(
        self,
    ) -> (
        mpsc::Receiver<Event>,
        mpsc::Receiver<Event>,
        mpsc::Receiver<InputControl>,
    ) {
        (self.events, self.focus_events, self.controls)
    }
}

pub(crate) struct PendingTerminalSession {
    theme: Theme,
    input_receivers: TerminalInputReceivers,
    presentation: TerminalPresentation,
    backend: InlineBackend,
    input_runtime: InputRuntime,
}

impl PendingTerminalSession {
    pub(crate) fn start(theme: ThemeName, terminal_notifications: bool) -> io::Result<Self> {
        let input_runtime = InputRuntime::start(InputRuntimeOptions {
            theme,
            focus_events: terminal_notifications,
        })?;
        let theme = Theme::resolve(theme, input_runtime.profile());
        let input_receivers = TerminalInputReceivers {
            events: input_runtime.events().clone(),
            focus_events: input_runtime.focus_events().clone(),
            controls: input_runtime.controls().clone(),
        };
        let presentation_profile = TerminalPresentationProfile::from_identity(
            &qwertty::caps::identity_from_env(None, qwertty::caps::std_env_source),
        );
        let presentation = TerminalPresentation::new(terminal_notifications, presentation_profile);

        // Retry transient stdout backpressure from resize redraw storms. The
        // CLI's stdio guard remains the primary nonblocking defense.
        let backend = CapabilityBackend::new(
            CrosstermBackend::new(RetryWriter::new(io::stdout())),
            theme.color_level,
        );

        Ok(Self {
            theme,
            input_receivers,
            presentation,
            backend,
            input_runtime,
        })
    }

    pub(crate) fn theme(&self) -> &Theme {
        &self.theme
    }

    pub(crate) fn fail_after_agent_startup<T>(mut self, error: io::Error) -> io::Result<T> {
        finish_startup_failure_with(&mut self.input_runtime, error, InputRuntime::finish)
    }

    pub(crate) fn activate(self) -> io::Result<ActivatedTerminalSession> {
        let Self {
            theme,
            input_receivers,
            presentation,
            backend,
            input_runtime,
        } = self;
        let (terminal, presentation, input_runtime) = activate_terminal_session_with(
            backend,
            presentation,
            input_runtime,
            InlineTerminal::new,
            InlineTerminal::clear,
        )?;
        Ok(ActivatedTerminalSession {
            theme,
            input_receivers,
            resources: (terminal, presentation, input_runtime),
        })
    }
}

pub(crate) struct ActivatedTerminalSession {
    theme: Theme,
    input_receivers: TerminalInputReceivers,
    resources: TerminalPresentationResources,
}

impl ActivatedTerminalSession {
    pub(crate) fn into_parts(
        self,
    ) -> (Theme, TerminalInputReceivers, TerminalPresentationResources) {
        (self.theme, self.input_receivers, self.resources)
    }
}

fn finish_startup_failure_with<T, Input>(
    input: &mut Input,
    error: io::Error,
    finish: impl FnOnce(&mut Input) -> io::Result<()>,
) -> io::Result<T> {
    finish(input)?;
    Err(error)
}

fn activate_terminal_session_with<Backend, Terminal, Presentation, Input, Error>(
    backend: Backend,
    presentation: Presentation,
    input: Input,
    create: impl FnOnce(Backend) -> Result<Terminal, Error>,
    clear: impl FnOnce(&mut Terminal) -> Result<(), Error>,
) -> Result<(Terminal, Presentation, Input), Error> {
    let mut terminal = create(backend)?;
    clear(&mut terminal)?;
    Ok((terminal, presentation, input))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    use super::{activate_terminal_session_with, finish_startup_failure_with};

    #[test]
    fn activation_creates_then_clears_and_preserves_owned_resources() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let create_calls = Rc::clone(&calls);
        let clear_calls = Rc::clone(&calls);

        let (terminal, presentation, input) = activate_terminal_session_with(
            "backend",
            "presentation",
            "input",
            move |backend| {
                create_calls.borrow_mut().push(format!("create:{backend}"));
                Ok::<_, io::Error>(Vec::<&str>::new())
            },
            move |terminal| {
                clear_calls.borrow_mut().push("clear".to_string());
                terminal.push("cleared");
                Ok(())
            },
        )
        .expect("terminal activation");

        assert_eq!(*calls.borrow(), ["create:backend", "clear"]);
        assert_eq!(terminal, ["cleared"]);
        assert_eq!(presentation, "presentation");
        assert_eq!(input, "input");
    }

    #[test]
    fn agent_startup_failure_keeps_existing_finish_error_precedence() {
        let mut input = Vec::new();
        let error = finish_startup_failure_with::<(), _>(
            &mut input,
            io::Error::other("agent failed"),
            |input| {
                input.push("finish");
                Ok(())
            },
        )
        .expect_err("agent error should win after successful finish");
        assert_eq!(input, ["finish"]);
        assert_eq!(error.to_string(), "agent failed");

        let mut input = Vec::new();
        let error = finish_startup_failure_with::<(), _>(
            &mut input,
            io::Error::other("agent failed"),
            |input| {
                input.push("finish");
                Err(io::Error::other("finish failed"))
            },
        )
        .expect_err("finish error should preserve existing question-mark precedence");
        assert_eq!(input, ["finish"]);
        assert_eq!(error.to_string(), "finish failed");
    }
}
