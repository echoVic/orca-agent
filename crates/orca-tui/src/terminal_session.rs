use std::io;

use crossbeam_channel as mpsc;
use crossterm::event::Event;
use orca_core::config::ThemeName;
use ratatui::backend::CrosstermBackend;

use crate::capability_backend::CapabilityBackend;
use crate::input_runtime::{InputControl, InputRuntime, InputRuntimeOptions};
use crate::presentation::{
    InlineTerminal, finish_terminal_presentation, initialize_terminal_presentation,
    with_terminal_presentation_cleanup,
};
use crate::renderer_input_wake::RendererInputWakeOwner;
use crate::stdio_guard::RetryWriter;
use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
use crate::theme::Theme;
use crate::types::AppStatus;

type InlineBackend = CapabilityBackend<CrosstermBackend<RetryWriter<std::io::Stdout>>>;

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

    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
        events: mpsc::Receiver<Event>,
        focus_events: mpsc::Receiver<Event>,
        controls: mpsc::Receiver<InputControl>,
    ) -> Self {
        Self {
            events,
            focus_events,
            controls,
        }
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
            terminal,
            presentation,
            input: input_runtime,
        })
    }
}

pub(crate) struct ActivatedTerminalSession<Terminal = InlineTerminal, Input = InputRuntime> {
    theme: Theme,
    input_receivers: TerminalInputReceivers,
    terminal: Terminal,
    presentation: TerminalPresentation,
    input: Input,
}

impl<Terminal, Input> ActivatedTerminalSession<Terminal, Input> {
    #[allow(clippy::too_many_arguments)]
    fn run_with<R, Context>(
        self,
        max_input_events: usize,
        mut context: Context,
        initialize: impl FnOnce(
            &mut Terminal,
            &mut TerminalPresentation,
            &Theme,
            &mut Context,
        ) -> io::Result<()>,
        body: impl FnOnce(
            &mut Terminal,
            &mut TerminalPresentation,
            &RendererInputWakeOwner,
            &Theme,
            &mut Context,
        ) -> io::Result<R>,
        reset_title: impl FnOnce(&mut Terminal, &mut TerminalPresentation) -> io::Result<()>,
        drop_terminal: impl FnOnce(Terminal),
        finish_input: impl FnOnce(&mut Input) -> io::Result<()>,
    ) -> io::Result<R> {
        let Self {
            theme,
            input_receivers,
            terminal,
            presentation,
            input,
        } = self;
        let input_wake = RendererInputWakeOwner::new(input_receivers, max_input_events);
        with_terminal_presentation_cleanup(
            (terminal, presentation, input),
            |(terminal, presentation, _input)| {
                initialize(terminal, presentation, &theme, &mut context)?;
                body(terminal, presentation, &input_wake, &theme, &mut context)
            },
            |(terminal, mut presentation, mut input)| {
                finish_terminal_presentation(
                    terminal,
                    |terminal| reset_title(terminal, &mut presentation),
                    drop_terminal,
                    || finish_input(&mut input),
                )
            },
        )
    }
}

impl ActivatedTerminalSession {
    pub(crate) fn run<R, Context>(
        self,
        max_input_events: usize,
        initial_status: AppStatus,
        context: Context,
        draw_initial: impl FnOnce(&mut InlineTerminal, &Theme, &mut Context) -> io::Result<()>,
        body: impl FnOnce(
            &mut InlineTerminal,
            &mut TerminalPresentation,
            &RendererInputWakeOwner,
            &Theme,
            &mut Context,
        ) -> io::Result<R>,
    ) -> io::Result<R> {
        self.run_with(
            max_input_events,
            context,
            |terminal, presentation, theme, context| {
                initialize_terminal_presentation(
                    terminal,
                    |terminal| {
                        let _ = presentation
                            .write_pending(terminal.backend_mut().inner_mut(), initial_status);
                        Ok(())
                    },
                    |terminal| draw_initial(terminal, theme, context),
                )
            },
            body,
            |terminal, presentation| {
                let _ = presentation.write_reset_title(terminal.backend_mut().inner_mut());
                Ok(())
            },
            drop,
            InputRuntime::finish,
        )
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
    use std::time::Duration;

    use crossbeam_channel as mpsc;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use orca_core::config::ThemeName;

    use super::{
        ActivatedTerminalSession, TerminalInputReceivers, activate_terminal_session_with,
        finish_startup_failure_with,
    };
    use crate::terminal_presentation::{TerminalPresentation, TerminalPresentationProfile};
    use crate::theme::Theme;

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

    #[test]
    fn activated_session_owns_input_wake_body_and_total_cleanup() {
        let (event_tx, event_rx) = mpsc::bounded(1);
        let (_focus_tx, focus_rx) = mpsc::bounded(1);
        let (_control_tx, control_rx) = mpsc::bounded(1);
        event_tx
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .expect("queued input");

        let session = ActivatedTerminalSession::<Vec<&str>, Vec<&str>> {
            theme: Theme::named(ThemeName::Dark),
            input_receivers: TerminalInputReceivers::from_parts_for_test(
                event_rx, focus_rx, control_rx,
            ),
            terminal: Vec::new(),
            presentation: TerminalPresentation::new(
                false,
                TerminalPresentationProfile {
                    osc9_supported: false,
                    tmux_passthrough: false,
                },
            ),
            input: Vec::new(),
        };
        let calls = Rc::new(RefCell::new(Vec::new()));
        let initialize_calls = Rc::clone(&calls);
        let body_calls = Rc::clone(&calls);
        let reset_calls = Rc::clone(&calls);
        let drop_calls = Rc::clone(&calls);
        let finish_calls = Rc::clone(&calls);

        let error = session
            .run_with(
                1,
                Vec::<&str>::new(),
                move |terminal, _presentation, _theme, context| {
                    terminal.push("initialized");
                    context.push("initialize");
                    initialize_calls.borrow_mut().push("initialize");
                    Ok(())
                },
                move |terminal, _presentation, input_wake, _theme, context| {
                    assert_eq!(terminal.as_slice(), ["initialized"]);
                    assert_eq!(context.as_slice(), ["initialize"]);
                    context.push("body");
                    let input = input_wake.receive(Duration::ZERO, || Ok(()))?;
                    assert!(matches!(
                        input.as_slice(),
                        [Event::Key(key)] if key.code == KeyCode::Char('x')
                    ));
                    terminal.push("body");
                    body_calls.borrow_mut().push("body");
                    Err::<(), _>(io::Error::other("body failed"))
                },
                move |terminal, _presentation| {
                    assert_eq!(terminal.as_slice(), ["initialized", "body"]);
                    reset_calls.borrow_mut().push("reset");
                    Ok(())
                },
                move |terminal| {
                    assert_eq!(terminal.as_slice(), ["initialized", "body"]);
                    drop_calls.borrow_mut().push("drop");
                },
                move |input| {
                    input.push("finish");
                    finish_calls.borrow_mut().push("finish");
                    Ok(())
                },
            )
            .expect_err("body error should survive total cleanup");

        assert_eq!(error.to_string(), "body failed");
        assert_eq!(
            *calls.borrow(),
            ["initialize", "body", "reset", "drop", "finish"]
        );
    }

    #[test]
    fn initialization_failure_skips_body_and_still_cleans() {
        let (_event_tx, event_rx) = mpsc::bounded(1);
        let (_focus_tx, focus_rx) = mpsc::bounded(1);
        let (_control_tx, control_rx) = mpsc::bounded(1);
        let session = ActivatedTerminalSession::<Vec<&str>, Vec<&str>> {
            theme: Theme::named(ThemeName::Dark),
            input_receivers: TerminalInputReceivers::from_parts_for_test(
                event_rx, focus_rx, control_rx,
            ),
            terminal: Vec::new(),
            presentation: TerminalPresentation::new(
                false,
                TerminalPresentationProfile {
                    osc9_supported: false,
                    tmux_passthrough: false,
                },
            ),
            input: Vec::new(),
        };
        let calls = Rc::new(RefCell::new(Vec::new()));
        let initialize_calls = Rc::clone(&calls);
        let reset_calls = Rc::clone(&calls);
        let drop_calls = Rc::clone(&calls);
        let finish_calls = Rc::clone(&calls);

        let error = session
            .run_with(
                1,
                Vec::<&str>::new(),
                move |terminal, _presentation, _theme, context| {
                    terminal.push("initialized");
                    context.push("initialize");
                    initialize_calls.borrow_mut().push("initialize");
                    Err(io::Error::other("initialize failed"))
                },
                |_terminal, _presentation, _input_wake, _theme, _context| -> io::Result<()> {
                    panic!("renderer body must not run after initialization failure")
                },
                move |terminal, _presentation| {
                    assert_eq!(terminal.as_slice(), ["initialized"]);
                    reset_calls.borrow_mut().push("reset");
                    Ok(())
                },
                move |terminal| {
                    assert_eq!(terminal.as_slice(), ["initialized"]);
                    drop_calls.borrow_mut().push("drop");
                },
                move |input| {
                    input.push("finish");
                    finish_calls.borrow_mut().push("finish");
                    Ok(())
                },
            )
            .expect_err("initialization error should survive total cleanup");

        assert_eq!(error.to_string(), "initialize failed");
        assert_eq!(*calls.borrow(), ["initialize", "reset", "drop", "finish"]);
    }
}
