//! Transcript messages, revision tracking, rendering caches, and stream parsers.
//!
//! `AppState` embeds this owner as one aggregate field; transcript-specific
//! invariants do not leak into protocol, viewport, or interaction state.

use std::collections::HashMap;

use orca_core::cost_types::UsageTotals;
use orca_core::plan_types::PlanItem;
use orca_core::proposed_plan::ProposedPlanStreamParser;

use crate::composer_images::TuiImage;
use crate::streaming_markdown::StreamingMarkdownAssembler;
use crate::transcript_search::TranscriptSearchState;
use crate::transcript_view::TranscriptRenderCache;

#[derive(Debug, Clone)]
pub enum ChatMessage {
    User(String),
    Image(TuiImage),
    Reasoning(String),
    Assistant(String),
    AssistantChunk {
        text: String,
        trailing_blank: bool,
    },
    ProposedPlan(String),
    ToolCall {
        id: String,
        name: String,
        target: Option<String>,
        status: String,
        output: Option<String>,
        diff: Option<String>,
        kind: Option<String>,
        expanded: bool,
    },
    /// A live child execution projected from the parent runtime surface.
    /// Activity history is intentionally display-only and bounded by the
    /// runtime projection; the child transcript remains owned by its thread.
    Subagent {
        id: String,
        description: String,
        status: String,
        output: Option<String>,
        error: Option<String>,
        activity: Option<String>,
        activity_tail: Vec<String>,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
        expanded: bool,
    },
    PlanUpdate {
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
    Error(String),
    System(String),
}

#[derive(Default)]
pub(crate) struct TranscriptState {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) message_revisions: Vec<u64>,
    pub(crate) tool_call_indices: HashMap<String, usize>,
    pub(crate) next_message_revision: u64,
    pub(crate) render_cache: TranscriptRenderCache,
    pub(crate) welcome_render_cache: TranscriptRenderCache,
    pub(crate) search: TranscriptSearchState,
    pub(crate) finalized_count: usize,
    pub(crate) flushed_count: usize,
    pub(crate) proposed_plan_parser: ProposedPlanStreamParser,
    pub(crate) assistant_stream: StreamingMarkdownAssembler,
    pub(crate) assistant_stream_tail: Option<usize>,
}

impl TranscriptState {
    pub(crate) fn new() -> Self {
        Self {
            next_message_revision: 1,
            ..Self::default()
        }
    }
}

#[cfg(test)]
#[path = "transcript_state_tests.rs"]
mod tests;
