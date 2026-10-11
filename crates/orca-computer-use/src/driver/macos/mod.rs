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
}

impl MacDriver {
    pub fn new(lock_path: PathBuf) -> Self {
        Self {
            lock_path,
            lock: None,
            turn_active: false,
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
        autoreleasepool(|_| match request {
            Request::Hello { protocol } => {
                if self.lock.is_none() {
                    self.lock = Some(crate::session_lock::acquire(&self.lock_path)?);
                }
                if protocol != PROTOCOL_VERSION {
                    return Err(error(
                        ErrorKind::VersionMismatch,
                        "Orca was updated; start a new session to use computer use",
                    ));
                }
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
        self.turn_active = false;
        self.lock.take();
    }
}
