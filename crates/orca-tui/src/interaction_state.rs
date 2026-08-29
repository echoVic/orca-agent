//! State owned by the interaction acknowledgement protocol.

use crate::protocol::{PendingTuiInput, TuiInteractionKey, TuiMcpElicitationMode};
use crate::user_input_dialog::UserInputDialog;
use orca_runtime::mentions::MentionBindings;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingInteractionSubmission {
    pub(crate) key: TuiInteractionKey,
    pub(crate) pending_input: PendingTuiInput,
    pub(crate) mcp_mode: Option<TuiMcpElicitationMode>,
    pub(crate) visible_text: String,
    pub(crate) mention_bindings: MentionBindings,
    pub(crate) atomic_skill_tokens: MentionBindings,
    pub(crate) pending_pastes: Vec<(String, String)>,
    pub(crate) user_input_dialog: Option<UserInputDialog>,
}

#[derive(Debug, Default)]
pub(crate) struct InteractionState {
    pub(crate) pending_input: Option<PendingTuiInput>,
    pub(crate) pending_mcp_elicitation_mode: Option<TuiMcpElicitationMode>,
    pub(crate) pending_submission: Option<PendingInteractionSubmission>,
}

#[cfg(test)]
#[path = "interaction_state_tests.rs"]
mod tests;
