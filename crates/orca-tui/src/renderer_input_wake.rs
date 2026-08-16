use std::io;
use std::time::Duration;

use crossbeam_channel as mpsc;
use crossterm::event::Event;

use crate::input_event_actions::should_queue_input_event;
use crate::input_runtime::InputControl;
use crate::input_wake::{InputWake, receive_prioritized_input_or_control};
use crate::terminal_session::TerminalInputReceivers;

pub(crate) struct RendererInputWakeOwner {
    events: mpsc::Receiver<Event>,
    focus_events: mpsc::Receiver<Event>,
    controls: mpsc::Receiver<InputControl>,
    ordinary_limit: usize,
}

impl RendererInputWakeOwner {
    pub(crate) fn new(receivers: TerminalInputReceivers, ordinary_limit: usize) -> Self {
        let (events, focus_events, controls) = receivers.into_parts();
        Self {
            events,
            focus_events,
            controls,
            ordinary_limit,
        }
    }

    pub(crate) fn receive(
        &self,
        timeout: Duration,
        mut resume: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Vec<Event>> {
        match receive_prioritized_input_or_control(
            &self.events,
            &self.focus_events,
            &self.controls,
            timeout,
            self.ordinary_limit,
        ) {
            Ok(InputWake::Events(events)) => Ok(events
                .into_iter()
                .filter(should_queue_input_event)
                .collect()),
            Ok(InputWake::Suspend { acknowledge }) => {
                acknowledge.send(()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "terminal input runtime dropped suspend acknowledgement",
                    )
                })?;
                loop {
                    match self.controls.recv() {
                        Ok(InputControl::Resumed) => {
                            resume()?;
                            break;
                        }
                        Ok(InputControl::Suspend { acknowledge }) => {
                            let _ = acknowledge.send(());
                        }
                        Err(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "terminal input runtime disconnected while suspended",
                            ));
                        }
                    }
                }
                Ok(Vec::new())
            }
            Ok(InputWake::Resumed) | Err(mpsc::RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input runtime disconnected",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use crossbeam_channel as mpsc;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    use super::RendererInputWakeOwner;
    use crate::input_runtime::InputControl;
    use crate::terminal_session::TerminalInputReceivers;

    fn channels(
        ordinary_limit: usize,
    ) -> (
        RendererInputWakeOwner,
        mpsc::Sender<Event>,
        mpsc::Receiver<Event>,
        mpsc::Sender<Event>,
        mpsc::Sender<InputControl>,
    ) {
        let (event_tx, event_rx) = mpsc::bounded(8);
        let event_observer = event_rx.clone();
        let (focus_tx, focus_rx) = mpsc::bounded(8);
        let (control_tx, control_rx) = mpsc::bounded(8);
        (
            RendererInputWakeOwner::new(
                TerminalInputReceivers::from_parts_for_test(event_rx, focus_rx, control_rx),
                ordinary_limit,
            ),
            event_tx,
            event_observer,
            focus_tx,
            control_tx,
        )
    }

    #[test]
    fn suspend_acknowledges_repeated_controls_before_one_resume() {
        let (owner, event_tx, event_observer, _focus_tx, control_tx) = channels(64);
        event_tx
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .expect("ordinary input receiver alive");
        let (first_ack, first_acknowledged) = tokio::sync::oneshot::channel();
        let (second_ack, second_acknowledged) = tokio::sync::oneshot::channel();
        control_tx
            .send(InputControl::Suspend {
                acknowledge: first_ack,
            })
            .expect("control receiver alive");
        control_tx
            .send(InputControl::Suspend {
                acknowledge: second_ack,
            })
            .expect("control receiver alive");
        control_tx
            .send(InputControl::Resumed)
            .expect("control receiver alive");

        let mut resumes = 0;
        let events = owner
            .receive(Duration::ZERO, || {
                resumes += 1;
                Ok(())
            })
            .expect("suspend handshake");

        assert!(events.is_empty());
        assert_eq!(first_acknowledged.blocking_recv(), Ok(()));
        assert_eq!(second_acknowledged.blocking_recv(), Ok(()));
        assert_eq!(resumes, 1);
        assert_eq!(event_observer.len(), 1, "queued key waits through suspend");
    }

    #[test]
    fn dropped_first_suspend_acknowledgement_keeps_exact_error() {
        let (owner, _event_tx, _event_observer, _focus_tx, control_tx) = channels(64);
        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
        drop(acknowledged);
        control_tx
            .send(InputControl::Suspend { acknowledge })
            .expect("control receiver alive");

        let error = owner
            .receive(Duration::ZERO, || Ok(()))
            .expect_err("dropped acknowledgement must fail");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            error.to_string(),
            "terminal input runtime dropped suspend acknowledgement"
        );
    }

    #[test]
    fn suspended_control_disconnect_keeps_exact_error_after_acknowledgement() {
        let (owner, _event_tx, _event_observer, _focus_tx, control_tx) = channels(64);
        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
        control_tx
            .send(InputControl::Suspend { acknowledge })
            .expect("control receiver alive");
        drop(control_tx);

        let error = owner
            .receive(Duration::ZERO, || Ok(()))
            .expect_err("suspended disconnect must fail");
        assert_eq!(acknowledged.blocking_recv(), Ok(()));
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            error.to_string(),
            "terminal input runtime disconnected while suspended"
        );
    }

    #[test]
    fn resume_callback_error_is_not_translated() {
        let (owner, _event_tx, _event_observer, _focus_tx, control_tx) = channels(64);
        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
        control_tx
            .send(InputControl::Suspend { acknowledge })
            .expect("control receiver alive");
        control_tx
            .send(InputControl::Resumed)
            .expect("control receiver alive");

        let error = owner
            .receive(Duration::ZERO, || Err(io::Error::other("resume failed")))
            .expect_err("resume error must win");
        assert_eq!(acknowledged.blocking_recv(), Ok(()));
        assert_eq!(error.to_string(), "resume failed");
    }

    #[test]
    fn wake_filters_motion_bounds_ordinary_input_and_preserves_empty_wakes() {
        let (owner, event_tx, event_observer, _focus_tx, control_tx) = channels(2);
        assert!(
            owner
                .receive(Duration::ZERO, || Ok(()))
                .expect("timeout wake")
                .is_empty()
        );
        control_tx
            .send(InputControl::Resumed)
            .expect("control receiver alive");
        assert!(
            owner
                .receive(Duration::ZERO, || Ok(()))
                .expect("unsolicited resume wake")
                .is_empty()
        );

        event_tx
            .send(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 1,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }))
            .expect("ordinary input receiver alive");
        event_tx
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            )))
            .expect("ordinary input receiver alive");
        event_tx
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::NONE,
            )))
            .expect("ordinary input receiver alive");

        let events = owner
            .receive(Duration::ZERO, || Ok(()))
            .expect("ordinary wake");
        assert!(matches!(events.as_slice(), [Event::Key(key)] if key.code == KeyCode::Char('a')));
        assert_eq!(event_observer.len(), 1, "ordinary overflow stays queued");
    }

    #[test]
    fn focus_wake_bypasses_the_ordinary_limit_without_draining_overflow() {
        let (owner, event_tx, event_observer, focus_tx, _control_tx) = channels(1);
        for key in ['a', 'b'] {
            event_tx
                .send(Event::Key(KeyEvent::new(
                    KeyCode::Char(key),
                    KeyModifiers::NONE,
                )))
                .expect("ordinary input receiver alive");
        }
        focus_tx
            .send(Event::FocusLost)
            .expect("focus receiver alive");

        let events = owner
            .receive(Duration::ZERO, || Ok(()))
            .expect("focus wake");
        assert!(matches!(
            events.as_slice(),
            [Event::FocusLost, Event::Key(key)] if key.code == KeyCode::Char('a')
        ));
        assert_eq!(event_observer.len(), 1, "ordinary overflow stays queued");
    }

    #[test]
    fn disconnected_control_wake_keeps_exact_error() {
        let (owner, _event_tx, _event_observer, _focus_tx, control_tx) = channels(64);
        drop(control_tx);

        let error = owner
            .receive(Duration::ZERO, || Ok(()))
            .expect_err("disconnected wake must fail");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "terminal input runtime disconnected");
    }
}
