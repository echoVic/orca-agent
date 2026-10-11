mod apps;
mod permissions;

use std::path::PathBuf;

use objc2::rc::autoreleasepool;
use objc2_foundation::NSProcessInfo;
use orca_platform::fs::ExclusiveFileLock;

use crate::protocol::{DriverError, ErrorKind, PROTOCOL_VERSION, Reply, Request};
use crate::serve::{DriverHandler, LineWriter};

pub struct MacDriver {
    lock_path: PathBuf,
    lock: Option<ExclusiveFileLock>,
    turn_active: bool,
    hello_complete: bool,
}

impl MacDriver {
    pub fn new(lock_path: PathBuf) -> Self {
        Self {
            lock_path,
            lock: None,
            turn_active: false,
            hello_complete: false,
        }
    }
}

pub(super) fn error(kind: ErrorKind, message: impl Into<String>) -> DriverError {
    DriverError {
        kind,
        message: message.into(),
        candidates: vec![],
    }
}

impl DriverHandler for MacDriver {
    fn handle(&mut self, request: Request, _events: &LineWriter) -> Result<Reply, DriverError> {
        if !matches!(request, Request::Hello { .. } | Request::Shutdown)
            && (!self.hello_complete || self.lock.is_none())
        {
            if self.lock.is_none() {
                // Probe contention without granting a handshake or retaining a free lock.
                drop(crate::session_lock::acquire(&self.lock_path)?);
            }
            return Err(error(ErrorKind::Failed, "send hello first"));
        }
        autoreleasepool(|_| match request {
            Request::Hello { protocol } => {
                if self.lock.is_none() {
                    self.lock = Some(crate::session_lock::acquire(&self.lock_path)?);
                }
                if protocol != PROTOCOL_VERSION {
                    self.shutdown();
                    return Err(error(
                        ErrorKind::VersionMismatch,
                        "Orca was updated; start a new session to use computer use",
                    ));
                }
                self.hello_complete = true;
                Ok(Reply::Hello {
                    protocol: PROTOCOL_VERSION,
                    macos_major: NSProcessInfo::processInfo()
                        .operatingSystemVersion()
                        .majorVersion as u32,
                    host_bundle_id: permissions::host_bundle_id(),
                    permissions: permissions::check(),
                })
            }
            Request::ListApps => Ok(Reply::Apps {
                apps: apps::list()?,
            }),
            Request::ResolveApp {
                app,
                allow_not_running,
            } => Ok(Reply::Resolved {
                app: apps::resolve(&app, allow_not_running)?,
            }),
            Request::LaunchApp { app, open } => Ok(Reply::Launched {
                app: apps::launch(&app, &open)?,
            }),
            Request::Turn { active } => {
                self.turn_active = active;
                Ok(Reply::Done)
            }
            Request::Shutdown => {
                self.shutdown();
                Ok(Reply::Done)
            }
            Request::Observe(_) | Request::Act(_) => Err(error(
                ErrorKind::Failed,
                "This computer-use request is not implemented yet",
            )),
        })
    }

    fn shutdown(&mut self) {
        self.hello_complete = false;
        self.turn_active = false;
        self.lock.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_require_a_successful_hello_even_when_the_lock_is_free() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("computer-use.lock");
        let mut driver = MacDriver::new(path.clone());
        let output = LineWriter::new(std::io::sink());
        for request in [
            Request::ListApps,
            Request::Turn { active: true },
            Request::LaunchApp {
                app: "dev.orca.not-installed".into(),
                open: vec![],
            },
        ] {
            let failure = driver.handle(request, &output).unwrap_err();
            assert_eq!(failure.kind, ErrorKind::Failed);
            assert_eq!(failure.message, "send hello first");
        }
        let mismatch = driver
            .handle(
                Request::Hello {
                    protocol: PROTOCOL_VERSION + 1,
                },
                &output,
            )
            .unwrap_err();
        assert_eq!(mismatch.kind, ErrorKind::VersionMismatch);
        assert!(
            crate::session_lock::acquire(&path).is_ok(),
            "rejected handshake releases its lock"
        );
        let failure = driver
            .handle(Request::Turn { active: true }, &output)
            .unwrap_err();
        assert_eq!(failure.message, "send hello first");
        assert_eq!(
            driver.handle(Request::Shutdown, &output).unwrap(),
            Reply::Done
        );
    }
}
