#![deny(deprecated)]

mod capabilities;
mod policy;
#[cfg(windows)]
mod security;
#[cfg(windows)]
mod spawn;

use std::fmt;
use std::io;

pub use capabilities::{CapabilityStore, SETUP_HELPER_VERSION, SandboxSetupReceipt};
pub use policy::{SandboxFilesystemMode, WindowsSandboxPlan, WindowsSandboxPolicyInput};
#[cfg(windows)]
pub use security::{
    AppContainerSecurity, PreparedSecurity, ensure_appcontainer_profile,
    prepare_appcontainer_security, prepare_security,
};
#[cfg(windows)]
pub use spawn::{SandboxSpawnRequest, SandboxedChild, SandboxedPty, SandboxedPtyInput};

#[derive(Debug)]
pub enum WindowsSandboxError {
    InvalidPolicy(String),
    InvalidState(String),
    Io(io::Error),
    Platform(orca_platform::PlatformError),
    Serialization(serde_json::Error),
}

impl fmt::Display for WindowsSandboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPolicy(message) => {
                write!(formatter, "invalid Windows sandbox policy: {message}")
            }
            Self::InvalidState(message) => {
                write!(formatter, "invalid Windows sandbox state: {message}")
            }
            Self::Io(error) => write!(formatter, "Windows sandbox I/O failed: {error}"),
            Self::Platform(error) => error.fmt(formatter),
            Self::Serialization(error) => {
                write!(
                    formatter,
                    "Windows sandbox state serialization failed: {error}"
                )
            }
        }
    }
}

impl std::error::Error for WindowsSandboxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Platform(error) => Some(error),
            Self::Serialization(error) => Some(error),
            Self::InvalidPolicy(_) | Self::InvalidState(_) => None,
        }
    }
}

impl From<io::Error> for WindowsSandboxError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<orca_platform::PlatformError> for WindowsSandboxError {
    fn from(error: orca_platform::PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl From<serde_json::Error> for WindowsSandboxError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}
