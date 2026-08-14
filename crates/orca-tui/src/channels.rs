use crossbeam_channel::{Receiver, Sender};

use crate::types::{TuiEvent, UserAction};

pub(crate) const TUI_EVENT_CAPACITY: usize = 256;
pub(crate) const USER_ACTION_CAPACITY: usize = 64;

pub(crate) type TuiEventSender = Sender<TuiEvent>;
pub(crate) type TuiEventReceiver = Receiver<TuiEvent>;
pub(crate) type UserActionSender = Sender<UserAction>;
pub(crate) type UserActionReceiver = Receiver<UserAction>;

pub(crate) fn tui_event_channel() -> (TuiEventSender, TuiEventReceiver) {
    crossbeam_channel::bounded(TUI_EVENT_CAPACITY)
}

pub(crate) fn user_action_channel() -> (UserActionSender, UserActionReceiver) {
    crossbeam_channel::bounded(USER_ACTION_CAPACITY)
}

#[cfg(test)]
mod tests {
    use crossbeam_channel as mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn event_and_action_mailboxes_enforce_declared_capacity() {
        let (event_tx, _event_rx) = tui_event_channel();
        for index in 0..TUI_EVENT_CAPACITY {
            event_tx
                .try_send(TuiEvent::MessageDelta(index.to_string()))
                .expect("event within capacity");
        }
        assert!(
            event_tx
                .try_send(TuiEvent::Notice("mailbox full".to_string()))
                .is_err()
        );

        let (action_tx, _action_rx) = user_action_channel();
        for _ in 0..USER_ACTION_CAPACITY {
            action_tx
                .try_send(UserAction::Interrupt)
                .expect("action within capacity");
        }
        assert!(action_tx.try_send(UserAction::Cancel).is_err());
    }

    #[test]
    fn full_event_mailbox_backpressures_then_delivers_terminal_event() {
        let (event_tx, event_rx) = tui_event_channel();
        for index in 0..TUI_EVENT_CAPACITY {
            event_tx
                .send(TuiEvent::MessageDelta(index.to_string()))
                .unwrap();
        }
        let (done_tx, done_rx) = mpsc::unbounded();
        let producer = thread::spawn(move || {
            let result = event_tx.send(TuiEvent::SessionCompleted {
                status: "success".to_string(),
            });
            done_tx.send(result).unwrap();
        });

        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        event_rx.recv().unwrap();
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        producer.join().unwrap();

        let events: Vec<_> = event_rx.try_iter().collect();
        assert!(matches!(
            events.last(),
            Some(TuiEvent::SessionCompleted { status }) if status == "success"
        ));
    }

    #[test]
    fn dropping_event_receiver_wakes_a_blocked_producer() {
        let (event_tx, event_rx) = tui_event_channel();
        for index in 0..TUI_EVENT_CAPACITY {
            event_tx
                .send(TuiEvent::MessageDelta(index.to_string()))
                .unwrap();
        }
        let (done_tx, done_rx) = mpsc::unbounded();
        let producer = thread::spawn(move || {
            done_tx
                .send(event_tx.send(TuiEvent::Notice("blocked send".to_string())))
                .unwrap();
        });

        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(event_rx);
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        producer.join().unwrap();
    }

    #[test]
    fn dropping_action_receiver_wakes_a_blocked_producer() {
        let (action_tx, action_rx) = user_action_channel();
        for _ in 0..USER_ACTION_CAPACITY {
            action_tx.send(UserAction::Interrupt).unwrap();
        }
        let (done_tx, done_rx) = mpsc::unbounded();
        let producer = thread::spawn(move || {
            done_tx.send(action_tx.send(UserAction::Cancel)).unwrap();
        });

        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(action_rx);
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        producer.join().unwrap();
    }

    #[test]
    fn slow_consumer_receives_complete_assistant_text_and_terminal_status() {
        let (event_tx, event_rx) = tui_event_channel();
        let producer = thread::spawn(move || {
            for index in 0..1024 {
                event_tx
                    .send(TuiEvent::MessageDelta(format!("{index},")))
                    .unwrap();
            }
            event_tx
                .send(TuiEvent::SessionCompleted {
                    status: "success".to_string(),
                })
                .unwrap();
        });

        let mut text = String::new();
        let mut terminal = None;
        let mut max_queued = 0;
        while terminal.is_none() {
            max_queued = max_queued.max(event_rx.len());
            match event_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
                TuiEvent::MessageDelta(delta) => text.push_str(&delta),
                TuiEvent::SessionCompleted { status } => terminal = Some(status),
                _ => {}
            }
            thread::sleep(Duration::from_micros(100));
        }
        producer.join().unwrap();

        assert_eq!(terminal.as_deref(), Some("success"));
        assert_eq!(
            text,
            (0..1024)
                .map(|index| format!("{index},"))
                .collect::<String>()
        );
        assert!(max_queued <= TUI_EVENT_CAPACITY);
    }
}
