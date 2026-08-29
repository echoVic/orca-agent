#![deny(deprecated)]

mod browse;
mod discovery;
mod eligibility;
mod freshness;
mod session;
mod types;

pub use freshness::RootFingerprint;
pub use session::{SearchSession, SearchSessionOptions};
pub use types::{
    MatchKind, RESULT_LIMIT, SearchMatch, SearchMode, SearchPhase, SearchProgress, SearchSnapshot,
    SessionGeneration,
};
