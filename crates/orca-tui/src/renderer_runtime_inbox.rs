use crossbeam_channel::TryIter;

use crate::channels::TuiEventReceiver;
use crate::types::TuiEvent;

pub(crate) struct RendererRuntimeInboxOwner {
    events: TuiEventReceiver,
}

impl RendererRuntimeInboxOwner {
    pub(crate) fn new(events: TuiEventReceiver) -> Self {
        Self { events }
    }

    pub(crate) fn pending(&self) -> TryIter<'_, TuiEvent> {
        self.events.try_iter()
    }

    pub(crate) fn shutdown(self) {
        drop(self.events);
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use crossbeam_channel as mpsc;

    use super::RendererRuntimeInboxOwner;
    use crate::channels::{TUI_EVENT_CAPACITY, tui_event_channel};
    use crate::types::TuiEvent;

    #[test]
    fn empty_and_disconnected_pending_iterators_are_inert() {
        let (event_tx, event_rx) = tui_event_channel();
        let owner = RendererRuntimeInboxOwner::new(event_rx);

        assert_eq!(owner.pending().count(), 0);
        drop(event_tx);
        assert_eq!(owner.pending().count(), 0);
    }

    #[test]
    fn pending_events_preserve_fifo_partial_consumption_and_receiver_identity() {
        let (event_tx, event_rx) = tui_event_channel();
        let owner = RendererRuntimeInboxOwner::new(event_rx);
        event_tx
            .send(TuiEvent::Notice("first".to_string()))
            .expect("runtime inbox alive");
        event_tx
            .send(TuiEvent::Notice("second".to_string()))
            .expect("runtime inbox alive");

        let mut pending = owner.pending();
        assert!(matches!(
            pending.next(),
            Some(TuiEvent::Notice(message)) if message == "first"
        ));
        drop(pending);

        let remaining = owner.pending().collect::<Vec<_>>();
        assert!(matches!(
            remaining.as_slice(),
            [TuiEvent::Notice(message)] if message == "second"
        ));

        event_tx
            .send(TuiEvent::Notice("after construction".to_string()))
            .expect("owner retains the original receiver");
        assert!(matches!(
            owner.pending().next(),
            Some(TuiEvent::Notice(message)) if message == "after construction"
        ));
    }

    #[test]
    fn shutdown_releases_a_capacity_blocked_runtime_producer() {
        let (event_tx, event_rx) = tui_event_channel();
        let owner = RendererRuntimeInboxOwner::new(event_rx);
        for index in 0..TUI_EVENT_CAPACITY {
            event_tx
                .send(TuiEvent::MessageDelta(index.to_string()))
                .expect("event within bounded capacity");
        }
        let (done_tx, done_rx) = mpsc::unbounded();
        let producer = thread::spawn(move || {
            done_tx
                .send(event_tx.send(TuiEvent::Notice("blocked".to_string())))
                .expect("test receiver alive");
        });

        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        owner.shutdown();
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocked producer released")
                .is_err()
        );
        producer.join().expect("producer joined");
    }
}
