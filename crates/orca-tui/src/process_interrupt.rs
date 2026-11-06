//! OS-level interrupt for the command the TUI is currently showing.
//!
//! Esc must be able to stop a shell that is blocked in an uninterruptible
//! syscall even when the JSONL/session persistence layer is broken. This
//! handle is deliberately independent of the transcript: it only remembers the
//! process group of the running command and signals it directly.

use std::sync::{Arc, Mutex};

pub(crate) type Pid = u32;

/// Tracks the process group of the current command.
///
/// The slot is shared with the runtime (through `orca_tools`) so a shell
/// started by the tool router is visible to the TUI's Esc handler.
#[derive(Clone, Debug)]
pub(crate) struct ProcessInterruptHandle {
    current: Arc<Mutex<Option<Pid>>>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ProcessInterruptHandle {
    /// A handle backed by the process-wide runtime registration.
    pub(crate) fn shared_with_runtime() -> Self {
        Self {
            current: orca_tools::process::global_process_interrupt_pgid(),
        }
    }

    pub(crate) fn register(&self, pgid: Pid) {
        *self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pgid);
    }

    pub(crate) fn clear(&self, pgid: Pid) {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current == Some(pgid) {
            *current = None;
        }
    }

    pub(crate) fn current(&self) -> Option<Pid> {
        *self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stops the current process group at the OS level. This never consults
    /// the session writer or any queued JSONL record, so a degraded transcript
    /// cannot swallow Esc.
    pub(crate) fn force_kill(&self) -> bool {
        let Some(pgid) = self.current() else {
            return false;
        };
        let delivered = orca_tools::process::force_kill_process_group(pgid);
        if delivered {
            self.clear(pgid);
        }
        delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_scoped_to_the_current_process_group() {
        let handle = ProcessInterruptHandle {
            current: Arc::new(Mutex::new(None)),
        };
        handle.register(41);
        assert_eq!(handle.current(), Some(41));
        handle.clear(42);
        assert_eq!(handle.current(), Some(41), "another pid cannot clear it");
        handle.clear(41);
        assert_eq!(handle.current(), None);
    }

    #[test]
    fn force_kill_without_a_registration_is_a_no_op() {
        let handle = ProcessInterruptHandle {
            current: Arc::new(Mutex::new(None)),
        };
        assert!(!handle.force_kill());
    }
}
