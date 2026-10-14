use std::path::Path;

use orca_platform::PlatformError;
use orca_platform::fs::ExclusiveFileLock;

use crate::protocol::{DriverError, ErrorKind};

pub fn acquire(path: &Path) -> Result<ExclusiveFileLock, DriverError> {
    ExclusiveFileLock::try_acquire(path).map_err(|error| match error {
        PlatformError::LockContended { .. } => DriverError {
            kind: ErrorKind::LockHeld,
            message: "another Orca session is using the computer".into(),
            candidates: vec![],
        },
        error => DriverError {
            kind: ErrorKind::Failed,
            message: format!("Could not acquire computer-use session lock: {error}"),
            candidates: vec![],
        },
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_held_lock_reports_another_session() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("computer-use.lock");
        let held = super::acquire(&path).unwrap();
        let error = super::acquire(&path)
            .err()
            .expect("second session must fail");
        assert_eq!(error.kind, crate::protocol::ErrorKind::LockHeld);
        assert_eq!(error.message, "another Orca session is using the computer");
        drop(held);
        assert!(
            super::acquire(&path).is_ok(),
            "released lock can be acquired"
        );
    }
}
