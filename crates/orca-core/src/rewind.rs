//! Shared types for the `/rewind` file-checkpoint and conversation-rewind
//! feature.
//!
//! The durable records live in the runtime's session transcript
//! (`session.snapshot` / `session.rewind`); this module only holds the
//! transport- and UI-facing vocabulary so the CLI, the runtime surface host
//! and the TUI agree on mode names and report shapes.

use serde::{Deserialize, Serialize};

/// What a rewind restores.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindMode {
    /// Restore files and truncate the conversation to the target turn.
    Both,
    /// Restore files only; the conversation is untouched.
    Code,
    /// Truncate the conversation only; files are untouched.
    Conversation,
    /// Keep the conversation before the target and replace everything from
    /// the target onward with a summary. Files are untouched.
    SummarizeFrom,
    /// Keep the conversation after the target and replace everything before
    /// it with a summary. Files are untouched.
    SummarizeUpTo,
}

impl RewindMode {
    pub const ALL: [Self; 5] = [
        Self::Both,
        Self::Conversation,
        Self::Code,
        Self::SummarizeFrom,
        Self::SummarizeUpTo,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Code => "code",
            Self::Conversation => "conversation",
            Self::SummarizeFrom => "summarize_from",
            Self::SummarizeUpTo => "summarize_up_to",
        }
    }

    /// CLI spelling (`orca rewind --mode ...`) also accepts hyphenated forms.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "both" => Some(Self::Both),
            "code" | "files" => Some(Self::Code),
            "conversation" | "chat" => Some(Self::Conversation),
            "summarize_from" | "summarize-from" => Some(Self::SummarizeFrom),
            "summarize_up_to" | "summarize-upto" | "summarize-up-to" => Some(Self::SummarizeUpTo),
            _ => None,
        }
    }

    pub fn includes_files(self) -> bool {
        matches!(self, Self::Both | Self::Code)
    }

    pub fn includes_conversation(self) -> bool {
        !matches!(self, Self::Code)
    }

    pub fn is_summarize(self) -> bool {
        matches!(self, Self::SummarizeFrom | Self::SummarizeUpTo)
    }
}

/// One selectable rewind target, as shown by the TUI picker or returned by a
/// headless list request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RewindCheckpointView {
    /// Persisted conversation item id (`item_...`) of the user message.
    pub message_id: String,
    /// Zero-based index among the rewindable user messages of the effective
    /// (post-rewind, post-compaction) conversation.
    pub ordinal: u32,
    /// First line / truncated prompt text.
    pub prompt_preview: String,
    pub recorded_at: Option<chrono::DateTime<chrono::Utc>>,
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
    /// A `session.snapshot` record exists for this turn.
    pub has_snapshot: bool,
    /// The snapshot record exists but its blobs were evicted by retention.
    pub snapshot_evicted: bool,
    /// `false` when the conversation-scoped modes are unavailable (for
    /// example when the message is no longer in the effective context).
    pub conversation_available: bool,
    /// `false` when the file-scoped modes are unavailable (no snapshot, root
    /// mismatch, evicted, ...).
    pub files_available: bool,
    /// Human-readable reasons for unavailable modes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_reasons: Vec<String>,
}

/// Aggregate file statistics for a rewind plan or apply.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct RewindStats {
    pub insertions: u64,
    pub deletions: u64,
    pub omitted: bool,
}

/// A path that the plan or apply intentionally skipped.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RewindSkip {
    pub path: String,
    pub reason: String,
}

/// A non-fatal condition the user should see before confirming.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RewindWarning {
    /// `untracked_change`, `root_mismatch`, `blob_missing`, `pending_journal`,
    /// `snapshot_evicted`, `untracked_roots`, ...
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// File-side outcome embedded in a [`RewindReport`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RewindFilesReport {
    pub changed: u64,
    pub deleted: u64,
    pub unchanged: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<RewindSkip>,
}

/// Conversation-side outcome embedded in a [`RewindReport`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConversationRewindReport {
    pub kind: String,
    pub kept_messages: u32,
}

/// One file's unified-diff preview for the TUI confirmation page.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RewindFilePreview {
    pub path: String,
    #[serde(default)]
    pub diff: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub binary: bool,
    #[serde(default)]
    pub insertions: u64,
    #[serde(default)]
    pub deletions: u64,
}

/// Complete, JSON-serializable outcome of a rewind (dry-run or applied).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RewindReport {
    pub target_message_id: String,
    pub ordinal: u32,
    pub mode: String,
    pub dry_run: bool,
    pub files: RewindFilesReport,
    pub stats: RewindStats,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<RewindWarning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationRewindReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewind_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_id: Option<String>,
    /// Per-file unified-diff previews for the planned restore (dry-run only,
    /// capped by the runtime). Empty when files are not part of the mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previews: Vec<RewindFilePreview>,
}

impl RewindReport {
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Warnings that must block an unattended apply (the TUI can confirm
    /// them explicitly; the CLI requires `--force`).
    pub fn has_blocking_warnings(&self) -> bool {
        self.warnings
            .iter()
            .any(|warning| warning.kind == "untracked_change")
    }
}

/// Result of an applied (or dry-run) rewind, including the prompt the TUI
/// should restore into the composer after a conversation rewind.
#[derive(Clone, Debug, Default)]
pub struct RewindOutcome {
    pub report: RewindReport,
    /// Original prompt text of the target user message, when the conversation
    /// was (or would be) rewound.
    pub prompt: Option<String>,
    /// Zero-based index among the rewindable user messages, used by the TUI to
    /// truncate its rendered view without matching message text.
    pub user_ordinal: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trips_through_its_spelling() {
        for mode in RewindMode::ALL {
            assert_eq!(RewindMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(RewindMode::parse("Code"), Some(RewindMode::Code));
        assert_eq!(
            RewindMode::parse("summarize-from"),
            Some(RewindMode::SummarizeFrom)
        );
        assert_eq!(RewindMode::parse("nope"), None);
    }

    #[test]
    fn mode_scope_flags_are_consistent() {
        assert!(RewindMode::Both.includes_files() && RewindMode::Both.includes_conversation());
        assert!(RewindMode::Code.includes_files() && !RewindMode::Code.includes_conversation());
        assert!(
            !RewindMode::Conversation.includes_files()
                && RewindMode::Conversation.includes_conversation()
        );
        assert!(RewindMode::SummarizeFrom.is_summarize());
        assert!(!RewindMode::SummarizeFrom.includes_files());
        assert!(RewindMode::SummarizeUpTo.is_summarize());
    }
}
