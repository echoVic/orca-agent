//! Shared bounds for foreground borrowing and subprocess shutdown.
use std::time::Duration;
pub(crate) const IDLE_WAIT_LIMIT: Duration = Duration::from_secs(3);
pub(crate) const ACTIVATION_WAIT_LIMIT: Duration = Duration::from_secs(1);
pub(crate) const RESTORE_ACTIVATION_LIMIT: Duration = Duration::from_secs(1);
pub(crate) const RESTORE_WINDOW_LIMIT: Duration = Duration::from_secs(2);
pub(crate) const FOREGROUND_ACTION_LIMIT: Duration = Duration::from_secs(30);
pub(crate) const ACTIVATION_SETTLE: Duration = Duration::from_millis(50);
#[cfg(target_os = "macos")]
pub(crate) const FOCUS_WAIT_LIMIT: Duration = Duration::from_secs(1);
// Include idle, activation, the bounded delivery, and both native restore steps.
pub(crate) const DRIVER_STOP_GRACE: Duration = IDLE_WAIT_LIMIT
    .saturating_add(ACTIVATION_WAIT_LIMIT)
    .saturating_add(FOREGROUND_ACTION_LIMIT)
    .saturating_add(RESTORE_WINDOW_LIMIT)
    .saturating_add(RESTORE_ACTIVATION_LIMIT)
    .saturating_add(ACTIVATION_SETTLE);
