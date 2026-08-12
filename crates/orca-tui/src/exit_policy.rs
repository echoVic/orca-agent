//! Exit policy: the resume hint printed after a headless-capable exit and
//! the saved-session id resolution. Extracted from `app.rs` (TUI
//! convergence slice 6).

use orca_core::config::HistoryMode;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TuiExit {
    pub(crate) code: i32,
    pub(crate) session_id: Option<String>,
}
use orca_runtime::surface::RuntimeSurfaceHostHandle;

pub(crate) fn exit_resume_hint(session_id: Option<&str>) -> Option<String> {
    session_id.map(|session_id| format!("Resume this session with:\norca --resume {session_id}\n"))
}

pub(crate) fn exit_session_id(
    active_session_id: Option<String>,
    history_mode: &HistoryMode,
) -> Option<String> {
    if let HistoryMode::Resume(selector) = history_mode {
        RuntimeSurfaceHostHandle::load_saved_session(selector)
            .ok()
            .map(|transcript| transcript.meta.session_id)
            .or(active_session_id)
    } else {
        active_session_id
    }
}
