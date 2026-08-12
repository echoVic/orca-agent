//! Input wake selection: the biased receive loop that prioritizes
//! suspend/resume controls, focus events, and ordinary input batches.
//! Extracted from `app.rs` (TUI convergence slice 3).

use crossbeam_channel as mpsc;
use std::time::Duration;

use ratatui::crossterm::event::Event;

use crate::input_runtime::InputControl;

#[cfg(test)]
pub(crate) fn receive_input_batch(
    receiver: &mpsc::Receiver<Event>,
    timeout: Duration,
    limit: usize,
) -> Result<Vec<Event>, mpsc::RecvTimeoutError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let first = receiver.recv_timeout(timeout)?;
    let mut events = Vec::with_capacity(limit.min(receiver.len().saturating_add(1)));
    events.push(first);
    while events.len() < limit {
        match receiver.try_recv() {
            Ok(event) => events.push(event),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
        }
    }
    Ok(events)
}

pub(crate) enum InputWake {
    Events(Vec<Event>),
    Suspend {
        acknowledge: tokio::sync::oneshot::Sender<()>,
    },
    Resumed,
}

#[cfg(test)]
pub(crate) fn receive_input_or_control(
    events: &mpsc::Receiver<Event>,
    controls: &mpsc::Receiver<InputControl>,
    timeout: Duration,
    limit: usize,
) -> Result<InputWake, mpsc::RecvTimeoutError> {
    let timeout_rx = mpsc::after(timeout);
    crossbeam_channel::select_biased! {
        recv(controls) -> control => {
            match control {
                Ok(InputControl::Suspend { acknowledge }) => {
                    Ok(InputWake::Suspend { acknowledge })
                }
                Ok(InputControl::Resumed) => Ok(InputWake::Resumed),
                Err(_) => Err(mpsc::RecvTimeoutError::Disconnected),
            }
        }
        recv(events) -> event => {
            let first = event.map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            let mut batch = Vec::with_capacity(limit.max(1).min(events.len().saturating_add(1)));
            if limit > 0 {
                batch.push(first);
                while batch.len() < limit {
                    match events.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }
            Ok(InputWake::Events(batch))
        }
        recv(timeout_rx) -> _ => Err(mpsc::RecvTimeoutError::Timeout),
    }
}

pub(crate) fn receive_prioritized_input_or_control(
    events: &mpsc::Receiver<Event>,
    focus_events: &mpsc::Receiver<Event>,
    controls: &mpsc::Receiver<InputControl>,
    timeout: Duration,
    ordinary_limit: usize,
) -> Result<InputWake, mpsc::RecvTimeoutError> {
    let timeout_rx = mpsc::after(timeout);
    crossbeam_channel::select_biased! {
        recv(controls) -> control => {
            match control {
                Ok(InputControl::Suspend { acknowledge }) => {
                    Ok(InputWake::Suspend { acknowledge })
                }
                Ok(InputControl::Resumed) => Ok(InputWake::Resumed),
                Err(_) => Err(mpsc::RecvTimeoutError::Disconnected),
            }
        }
        recv(focus_events) -> focus => {
            let first = focus.map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            let mut batch = Vec::with_capacity(focus_events.len().saturating_add(1));
            batch.push(first);
            batch.extend(focus_events.try_iter());
            for _ in 0..ordinary_limit {
                match events.try_recv() {
                    Ok(event) => batch.push(event),
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                }
            }
            Ok(InputWake::Events(batch))
        }
        recv(events) -> event => {
            let first = event.map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
            let mut batch = Vec::with_capacity(ordinary_limit.max(1).min(events.len().saturating_add(1)));
            if ordinary_limit > 0 {
                batch.push(first);
                while batch.len() < ordinary_limit {
                    match events.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }
            Ok(InputWake::Events(batch))
        }
        recv(timeout_rx) -> _ => Err(mpsc::RecvTimeoutError::Timeout),
    }
}
