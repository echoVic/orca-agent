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
    events: LineWriter,
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
                // Broken stdout is handled by serve; never unwind through the tap.
                let _ = context.events.send(&DriverLine::Event(EventLine {
                    event: DriverEvent::Esc,
                }));
            }
            return std::ptr::null_mut();
        }
    }
    event.as_ptr()
}

pub(super) struct EscTap {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl EscTap {
    pub(super) fn start(events: LineWriter, aborted: Arc<AtomicBool>) -> Result<Self, DriverError> {
        let (ready, startup) = mpsc::sync_channel(1);
        let (stop, stopping) = mpsc::channel();
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
            thread: Some(thread),
        })
    }
}

impl Drop for EscTap {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
