//! What is on screen, kept apart from what the controller operates on.
//!
//! The controller takes one action at a time, and a turn keeps it until the
//! turn ends. Showing a child's conversation must not wait for that: the
//! focus and the return are done here, on the thread that asks (the
//! dispatcher's), and the controller only follows between actions
//! ([`HostedView::adopt`]). Until it has followed, the controller still
//! operates on the main conversation and the prompt queue stays bound to it.

use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam_channel as mpsc;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle};

use crate::attachment_routing::AttachmentRouting;
use crate::hosted_child::{HostedChildAction, HostedChildFocus, handle_hosted_child_action};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::{SessionAttachmentId, TuiEvent};

/// How the controller's operating thread must move to match the screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Adoption {
    Unchanged,
    /// Take up the child on screen.
    Child,
    /// Go back to the main conversation.
    Main,
}

/// `on_screen` is the attachment of the child in view, `adopted` that of
/// the child the controller operates on; `None` is the main conversation.
pub(crate) fn adoption(
    on_screen: Option<SessionAttachmentId>,
    adopted: Option<SessionAttachmentId>,
) -> Adoption {
    match (on_screen, adopted) {
        (Some(on_screen), Some(adopted)) if on_screen == adopted => Adoption::Unchanged,
        (Some(_), _) => Adoption::Child,
        (None, Some(_)) => Adoption::Main,
        (None, None) => Adoption::Unchanged,
    }
}

/// The main conversation as the controller last held it.
struct MainBinding {
    thread: Option<RuntimeThreadHandle>,
    event_tx: mpsc::Sender<TuiEvent>,
    attachment: SessionAttachmentId,
}

struct Inner {
    main: MainBinding,
    side_active: bool,
    focus: Option<HostedChildFocus>,
    adopted: Option<SessionAttachmentId>,
    closed: bool,
}

#[derive(Clone)]
pub(crate) struct HostedView {
    inner: Arc<Mutex<Inner>>,
    host: RuntimeHostHandle,
    root_event_tx: mpsc::Sender<TuiEvent>,
    routing: Arc<Mutex<AttachmentRouting>>,
}

impl std::fmt::Debug for HostedView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostedView").finish_non_exhaustive()
    }
}

impl HostedView {
    pub(crate) fn new(
        host: RuntimeHostHandle,
        root_event_tx: mpsc::Sender<TuiEvent>,
        routing: Arc<Mutex<AttachmentRouting>>,
        event_tx: mpsc::Sender<TuiEvent>,
        attachment: SessionAttachmentId,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                main: MainBinding {
                    thread: None,
                    event_tx,
                    attachment,
                },
                side_active: false,
                focus: None,
                adopted: None,
                closed: false,
            })),
            host,
            root_event_tx,
            routing,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Shows a child's conversation, or the main one again, on the calling
    /// thread. It changes what is on screen and nothing the controller
    /// holds.
    pub(crate) fn handle(&self, action: HostedChildAction, control: &TuiSurfaceTaskControl) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        if inner.closed {
            return;
        }
        // Where the action starts from: the child in view, or else the main
        // conversation the controller recorded.
        let (mut thread, mut event_tx, mut attachment) = match inner.focus.as_ref() {
            Some(focus) => (
                Some(focus.child_thread.clone()),
                focus.child_event_tx.clone(),
                focus.child_attachment,
            ),
            None => (
                // The main conversation as the controller last recorded it.
                // Only a turn that started the very first thread has not let
                // the controller record it, and then the control's bound
                // thread is that one.
                inner.main.thread.clone().or_else(|| {
                    inner
                        .adopted
                        .is_none()
                        .then(|| control.runtime_thread())
                        .flatten()
                }),
                inner.main.event_tx.clone(),
                inner.main.attachment,
            ),
        };
        if matches!(action, HostedChildAction::Focus { .. }) {
            // Direct settings stay unavailable throughout the change; the
            // end of this function sets them to what is then on screen.
            control.restrict_hosted_settings(true);
            if inner.focus.is_some() {
                handle_hosted_child_action(
                    HostedChildAction::Return,
                    &mut thread,
                    &mut inner.focus,
                    inner.side_active,
                    &self.host,
                    &self.root_event_tx,
                    &mut event_tx,
                    &mut attachment,
                    &self.routing,
                    control,
                );
            }
        }
        handle_hosted_child_action(
            action,
            &mut thread,
            &mut inner.focus,
            inner.side_active,
            &self.host,
            &self.root_event_tx,
            &mut event_tx,
            &mut attachment,
            &self.routing,
            control,
        );
        // Settings are restricted exactly while a side conversation or a
        // child is on screen: a focus that was refused restricts nothing.
        control.restrict_hosted_settings(inner.side_active || inner.focus.is_some());
    }

    /// Moves the controller's operating thread to what is on screen. Called
    /// by the controller between actions only. While it operates on main it
    /// also records main here, for the next focus to start from.
    pub(crate) fn adopt(
        &self,
        thread: &mut Option<RuntimeThreadHandle>,
        event_tx: &mut mpsc::Sender<TuiEvent>,
        attachment: &mut SessionAttachmentId,
        side_active: bool,
    ) -> Adoption {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let on_screen = inner.focus.as_ref().map(|focus| focus.child_attachment);
        let change = adoption(on_screen, inner.adopted);
        match change {
            Adoption::Child => {
                if inner.adopted.is_none() {
                    // The controller is about to leave main: keep it.
                    inner.main = MainBinding {
                        thread: thread.clone(),
                        event_tx: event_tx.clone(),
                        attachment: *attachment,
                    };
                }
                let focus = inner.focus.as_ref().expect("a child is on screen");
                *thread = Some(focus.child_thread.clone());
                *event_tx = focus.child_event_tx.clone();
                *attachment = focus.child_attachment;
                inner.adopted = on_screen;
            }
            Adoption::Main => {
                *thread = inner.main.thread.clone();
                *event_tx = inner.main.event_tx.clone();
                *attachment = inner.main.attachment;
                inner.adopted = None;
            }
            Adoption::Unchanged => {
                if inner.adopted.is_none() {
                    inner.main = MainBinding {
                        thread: thread.clone(),
                        event_tx: event_tx.clone(),
                        attachment: *attachment,
                    };
                    inner.side_active = side_active;
                }
            }
        }
        change
    }

    pub(crate) fn has_focus(&self) -> bool {
        self.lock().focus.is_some()
    }

    /// A child is on screen and the controller has not taken it up: what
    /// the controller is sent now still goes to the main conversation.
    pub(crate) fn awaits_main(&self) -> bool {
        let inner = self.lock();
        let on_screen = inner.focus.as_ref().map(|focus| focus.child_attachment);
        on_screen.is_some() && on_screen != inner.adopted
    }

    /// Ends the view as the controller ends: nothing can be shown through
    /// it afterwards, and the child still on screen is handed back to be
    /// shut down.
    pub(crate) fn close(&self) -> Option<HostedChildFocus> {
        let mut inner = self.lock();
        inner.closed = true;
        inner.focus.take()
    }

    /// The controller replaced its main thread in the middle of an action
    /// (a session installed, then a turn run on it): a focus made during
    /// that turn must start from the new thread.
    pub(crate) fn note_main_thread(&self, thread: &RuntimeThreadHandle) {
        let mut inner = self.lock();
        if inner.adopted.is_none() {
            inner.main.thread = Some(thread.clone());
        }
    }

    /// Said by the controller before it starts or switches to a side
    /// conversation, so that no child is focused meanwhile. The controller
    /// records the truth again as it next follows the view.
    pub(crate) fn set_side_active(&self, active: bool) {
        self.lock().side_active = active;
    }
}

#[cfg(test)]
mod tests {
    use super::{Adoption, adoption};
    use crate::protocol::SessionAttachmentId;

    fn id(value: u64) -> SessionAttachmentId {
        SessionAttachmentId::new(value)
    }

    #[test]
    fn the_controller_follows_what_is_on_screen() {
        // Nothing focused, nothing adopted: the controller stays on main.
        assert_eq!(adoption(None, None), Adoption::Unchanged);
        // A child came on screen while the controller was on main.
        assert_eq!(adoption(Some(id(2)), None), Adoption::Child);
        // The controller already operates on the child on screen.
        assert_eq!(adoption(Some(id(2)), Some(id(2))), Adoption::Unchanged);
        // Another child replaced it while the controller was busy.
        assert_eq!(adoption(Some(id(3)), Some(id(2))), Adoption::Child);
        // The view went back to main; the controller still holds the child.
        assert_eq!(adoption(None, Some(id(2))), Adoption::Main);
    }

    fn view_for_test() -> (
        super::HostedView,
        crate::operation_controller::TuiSurfaceTaskControl,
        crossbeam_channel::Receiver<crate::protocol::TuiEvent>,
        orca_runtime::runtime_host::RuntimeHost,
    ) {
        let control = crate::operation_controller::TuiSurfaceTaskControl::isolated_for_test();
        let host = orca_runtime::runtime_host::RuntimeHost::start_with_background_capacity(1)
            .expect("runtime host");
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let attachment = id(1);
        let view = super::HostedView::new(
            host.handle(),
            event_tx.clone(),
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::attachment_routing::AttachmentRouting::new(attachment),
            )),
            event_tx,
            attachment,
        );
        (view, control, event_rx, host)
    }

    fn focus_action() -> crate::hosted_child::HostedChildAction {
        crate::hosted_child::HostedChildAction::Focus {
            task_id: "child".to_string(),
            expected_revision: 1,
        }
    }

    #[test]
    fn a_focus_is_refused_while_a_side_conversation_is_starting() {
        let (view, control, events, _host) = view_for_test();
        view.set_side_active(true);
        view.handle(focus_action(), &control);
        match events.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(crate::protocol::TuiEvent::OperationRejected(message)) => assert_eq!(
                message,
                "live child focus is unavailable in a side conversation"
            ),
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(!view.has_focus());
    }

    #[test]
    fn a_closed_view_shows_nothing() {
        let (view, control, events, _host) = view_for_test();
        view.handle(focus_action(), &control);
        match events.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(crate::protocol::TuiEvent::OperationRejected(message)) => {
                assert_eq!(
                    message,
                    "focus a main conversation before selecting a child"
                )
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(view.close().is_none());
        view.handle(focus_action(), &control);
        assert!(
            events
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "a closed view emits nothing"
        );
    }
}
