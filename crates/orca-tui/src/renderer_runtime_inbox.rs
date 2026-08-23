use crossbeam_channel::TryIter;
use std::sync::Mutex;

use crate::channels::TuiEventReceiver;
use crate::types::TuiEvent;

const MAX_COALESCED_DELTA_BYTES: usize = 64 * 1024;

pub(crate) struct RendererRuntimeInboxOwner {
    events: TuiEventReceiver,
    buffered: Mutex<Option<TuiEvent>>,
}

impl RendererRuntimeInboxOwner {
    pub(crate) fn new(events: TuiEventReceiver) -> Self {
        Self {
            events,
            buffered: Mutex::new(None),
        }
    }

    pub(crate) fn pending(&self) -> CoalescedRuntimeEvents<'_> {
        let buffered = self
            .buffered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        CoalescedRuntimeEvents {
            events: self.events.try_iter(),
            owner_buffer: &self.buffered,
            buffered,
        }
    }

    pub(crate) fn shutdown(self) {
        drop(self.events);
    }
}

pub(crate) struct CoalescedRuntimeEvents<'a> {
    events: TryIter<'a, TuiEvent>,
    owner_buffer: &'a Mutex<Option<TuiEvent>>,
    buffered: Option<TuiEvent>,
}

impl Iterator for CoalescedRuntimeEvents<'_> {
    type Item = TuiEvent;

    fn next(&mut self) -> Option<Self::Item> {
        let event = self.buffered.take().or_else(|| self.events.next())?;
        match event {
            TuiEvent::MessageDelta(text) => {
                Some(TuiEvent::MessageDelta(self.coalesce_message(text)))
            }
            TuiEvent::ReasoningDelta(text) => {
                Some(TuiEvent::ReasoningDelta(self.coalesce_reasoning(text)))
            }
            event => Some(event),
        }
    }
}

impl CoalescedRuntimeEvents<'_> {
    fn coalesce_message(&mut self, mut text: String) -> String {
        loop {
            let Some(next) = self.events.next() else {
                return text;
            };
            match next {
                TuiEvent::MessageDelta(delta)
                    if text.len().saturating_add(delta.len()) <= MAX_COALESCED_DELTA_BYTES =>
                {
                    text.push_str(&delta);
                }
                event => {
                    self.buffered = Some(event);
                    return text;
                }
            }
        }
    }

    fn coalesce_reasoning(&mut self, mut text: String) -> String {
        loop {
            let Some(next) = self.events.next() else {
                return text;
            };
            match next {
                TuiEvent::ReasoningDelta(delta)
                    if text.len().saturating_add(delta.len()) <= MAX_COALESCED_DELTA_BYTES =>
                {
                    text.push_str(&delta);
                }
                event => {
                    self.buffered = Some(event);
                    return text;
                }
            }
        }
    }
}

impl Drop for CoalescedRuntimeEvents<'_> {
    fn drop(&mut self) {
        let Some(buffered) = self.buffered.take() else {
            return;
        };
        let mut owner_buffer = self
            .owner_buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(owner_buffer.is_none());
        *owner_buffer = Some(buffered);
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
    fn pending_events_coalesce_adjacent_deltas_before_terminal_barriers() {
        let (event_tx, event_rx) = tui_event_channel();
        let owner = RendererRuntimeInboxOwner::new(event_rx);
        for event in [
            TuiEvent::MessageDelta("hel".to_string()),
            TuiEvent::MessageDelta("lo".to_string()),
            TuiEvent::ReasoningDelta("thi".to_string()),
            TuiEvent::ReasoningDelta("nk".to_string()),
            TuiEvent::SessionCompleted {
                status: "success".to_string(),
            },
        ] {
            event_tx.send(event).unwrap();
        }

        let events = owner.pending().collect::<Vec<_>>();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            TuiEvent::MessageDelta(text) if text == "hello"
        ));
        assert!(matches!(
            &events[1],
            TuiEvent::ReasoningDelta(text) if text == "think"
        ));
        assert!(matches!(
            &events[2],
            TuiEvent::SessionCompleted { status } if status == "success"
        ));
    }

    #[test]
    fn coalesced_delta_lookahead_survives_partial_iterator_drop() {
        let (event_tx, event_rx) = tui_event_channel();
        let owner = RendererRuntimeInboxOwner::new(event_rx);
        event_tx
            .send(TuiEvent::MessageDelta("first".to_string()))
            .unwrap();
        event_tx
            .send(TuiEvent::Notice("barrier".to_string()))
            .unwrap();
        event_tx
            .send(TuiEvent::MessageDelta("second".to_string()))
            .unwrap();

        let mut first = owner.pending();
        assert!(matches!(
            first.next(),
            Some(TuiEvent::MessageDelta(text)) if text == "first"
        ));
        drop(first);

        let remaining = owner.pending().collect::<Vec<_>>();
        assert!(matches!(
            remaining.as_slice(),
            [TuiEvent::Notice(message), TuiEvent::MessageDelta(text)]
                if message == "barrier" && text == "second"
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
