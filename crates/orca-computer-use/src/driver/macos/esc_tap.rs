//! A session keyboard tap owned, serviced and torn down on its own thread.
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use objc2_core_foundation::{CFMachPort, CFRunLoop, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};

use super::{error, events::HID_MARKER};
use crate::protocol::{DriverError, DriverEvent, DriverLine, ErrorKind, EventLine};
use crate::serve::LineWriter;

struct Context {
    aborted: Arc<AtomicBool>,
    events: crossbeam_channel::Sender<()>,
    tap: Option<objc2_core_foundation::CFRetained<CFMachPort>>,
}

unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    context: *mut c_void,
) -> *mut CGEvent {
    // The thread keeps this Box and its tap alive until the source is removed.
    let context = unsafe { &*context.cast::<Context>() };
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        if let Some(tap) = &context.tap {
            CGEvent::tap_enable(tap, true);
        }
    } else if matches!(kind, CGEventType::KeyDown | CGEventType::KeyUp) {
        let event = unsafe { event.as_ref() };
        if CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode) == 53
            && CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData)
                != HID_MARKER
        {
            if kind == CGEventType::KeyDown {
                context.aborted.store(true, Ordering::SeqCst);
                // The callback never takes the stdout lock or waits for capacity.
                // A full/disconnected queue can drop the notice, but not the abort.
                let _ = context.events.try_send(());
            }
            return std::ptr::null_mut();
        }
    }
    event.as_ptr()
}

const ESC_QUEUE_CAPACITY: usize = 8;

struct EscWriter {
    sender: crossbeam_channel::Sender<()>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EscWriter {
    fn start(events: LineWriter) -> Result<Self, DriverError> {
        let (sender, queued) = crossbeam_channel::bounded(ESC_QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::Builder::new()
            .name("computer-use-esc-writer".into())
            .spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    match queued.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok(()) if !stopping.load(Ordering::SeqCst) => {
                            if events
                                .send(&DriverLine::Event(EventLine {
                                    event: DriverEvent::Esc,
                                }))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            })
            .map_err(|e| {
                error(
                    ErrorKind::Failed,
                    format!("Could not start the Esc writer: {e}"),
                )
            })?;
        Ok(Self {
            sender,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for EscWriter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // A generic Write cannot be interrupted. Detach an in-flight write;
        // it exits once stdout unblocks, without holding the tap/run loop.
        if let Some(thread) = self.thread.take()
            && thread.is_finished()
        {
            let _ = thread.join();
        }
    }
}

pub(super) struct EscTap {
    stop: Sender<()>,
    writer: EscWriter,
    thread: Option<JoinHandle<()>>,
    #[cfg(test)]
    trigger: Sender<()>,
}

impl EscTap {
    pub(super) fn start(events: LineWriter, aborted: Arc<AtomicBool>) -> Result<Self, DriverError> {
        let writer = EscWriter::start(events)?;
        let events = writer.sender.clone();
        let (ready, startup) = mpsc::sync_channel(1);
        let (stop, stopping) = mpsc::channel();
        #[cfg(test)]
        let (trigger, triggering) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("computer-use-esc".into())
            .spawn(move || {
                let mut context = Box::new(Context {
                    aborted,
                    events,
                    tap: None,
                });
                let tap = unsafe {
                    CGEvent::tap_create(
                        CGEventTapLocation::SessionEventTap,
                        CGEventTapPlacement::HeadInsertEventTap,
                        CGEventTapOptions::Default,
                        (1 << CGEventType::KeyDown.0) | (1 << CGEventType::KeyUp.0),
                        Some(callback),
                        (&mut *context as *mut Context).cast(),
                    )
                };
                let Some(tap) = tap else {
                    let _ = ready.send(Err(error(
                        ErrorKind::PermissionMissing,
                        "Could not install the Esc listener",
                    )));
                    return;
                };
                let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
                    tap.invalidate();
                    let _ = ready.send(Err(error(
                        ErrorKind::Failed,
                        "Could not create the Esc run loop source",
                    )));
                    return;
                };
                let Some(run_loop) = CFRunLoop::current() else {
                    tap.invalidate();
                    let _ = ready.send(Err(error(
                        ErrorKind::Failed,
                        "Could not get the Esc run loop",
                    )));
                    return;
                };
                context.tap = Some(tap.clone());
                let mode = unsafe { kCFRunLoopDefaultMode };
                run_loop.add_source(Some(&source), mode);
                CGEvent::tap_enable(&tap, true);
                let _ = ready.send(Ok(()));
                // Bounded modes let teardown run on the owning thread, without
                // sending non-Send Core Foundation objects across threads.
                while matches!(stopping.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                    #[cfg(test)]
                    if triggering.try_recv().is_ok() {
                        let event = CGEvent::new_keyboard_event(None, 53, true).unwrap();
                        unsafe {
                            callback(
                                std::ptr::null_mut(),
                                CGEventType::KeyDown,
                                NonNull::from(&*event),
                                (&mut *context as *mut Context).cast(),
                            )
                        };
                    }
                    CFRunLoop::run_in_mode(mode, 0.05, true);
                }
                CGEvent::tap_enable(&tap, false);
                run_loop.stop();
                run_loop.remove_source(Some(&source), mode);
                tap.invalidate();
            })
            .map_err(|e| {
                error(
                    ErrorKind::Failed,
                    format!("Could not start the Esc listener: {e}"),
                )
            })?;
        let started = startup.recv().unwrap_or_else(|_| {
            Err(error(
                ErrorKind::Failed,
                "The Esc listener stopped during startup",
            ))
        });
        if let Err(error) = started {
            let _ = thread.join();
            return Err(error);
        }
        Ok(Self {
            stop,
            writer,
            thread: Some(thread),
            #[cfg(test)]
            trigger,
        })
    }
}

impl Drop for EscTap {
    fn drop(&mut self) {
        self.writer.stop.store(true, Ordering::SeqCst);
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    struct BlockedWriter {
        entered: mpsc::SyncSender<()>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }
    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let _ = self.entered.try_send(());
            let (lock, wake) = &*self.gate;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = wake.wait(open).unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stopping_the_listener_does_not_wait_for_blocked_stdout() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (entered, writing) = mpsc::sync_channel(1);
        let aborted = Arc::new(AtomicBool::new(false));
        let writer = LineWriter::new(BlockedWriter {
            entered,
            gate: gate.clone(),
        });
        let tap = match EscTap::start(writer, aborted.clone()) {
            Ok(tap) => tap,
            Err(e) if e.kind == ErrorKind::PermissionMissing => {
                eprintln!("skipped: missing Input Monitoring permission");
                return;
            }
            Err(e) => panic!("{e:?}"),
        };
        tap.trigger.send(()).unwrap();
        let entered_write = writing.recv_timeout(Duration::from_secs(2)).is_ok();
        let (stopped, stopping) = mpsc::channel();
        let worker = thread::spawn(move || {
            drop(tap);
            stopped.send(()).unwrap();
        });
        let prompt = stopping.recv_timeout(Duration::from_millis(300)).is_ok();
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        worker.join().unwrap();
        assert!(entered_write, "positive control: stdout really blocked");
        assert!(
            aborted.load(Ordering::SeqCst),
            "abort is set even with blocked stdout"
        );
        assert!(prompt, "stopping the listener waited for stdout");
    }
}

#[cfg(test)]
#[test]
fn a_full_esc_notice_queue_still_aborts_without_waiting() {
    let (events, queued) = crossbeam_channel::bounded(ESC_QUEUE_CAPACITY);
    let aborted = Arc::new(AtomicBool::new(false));
    let mut context = Context {
        aborted: aborted.clone(),
        events,
        tap: None,
    };
    let event = CGEvent::new_keyboard_event(None, 53, true).unwrap();
    for _ in 0..100 {
        let result = unsafe {
            callback(
                std::ptr::null_mut(),
                CGEventType::KeyDown,
                NonNull::from(&*event),
                (&mut context as *mut Context).cast(),
            )
        };
        assert!(result.is_null());
    }
    assert!(aborted.load(Ordering::SeqCst));
    assert_eq!(queued.len(), ESC_QUEUE_CAPACITY);
}
