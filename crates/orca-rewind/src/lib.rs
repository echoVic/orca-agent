//! File checkpoint storage for Orca's `/rewind`.
//!
//! This crate owns the disk side of the feature described in
//! `docs/design/rewind-file-checkpoints.md`:
//!
//! * [`RewindStore`] — a session-scoped content-addressed blob store
//!   (`$ORCA_HOME/rewind/sessions/<session_id>/objects/<2-hex>/<sha256>`) with
//!   zstd-or-raw encoding, deduplication and garbage collection.
//! * [`build_plan`] — the undo-log planner from §8.3. Given the ordered
//!   snapshot views for a session and a target turn it derives, for every path
//!   the agent touched at or after that turn, the state the file had when the
//!   target message was submitted, compares it with what is on disk and emits
//!   the write/delete/skip plan (plus diff statistics and warnings).
//! * [`plan_diff_previews`] — capped, line-bounded unified-diff previews of a
//!   restore plan for the TUI confirmation page.
//! * [`Applier`] — the two-phase applier from §12.1: a JSONL journal with
//!   fsync'd `intent` / `applied` / `committed` lines, per-file backups of the
//!   current content under `journals/<op>/pre/<sha256>`, atomic
//!   temp-file + fsync + rename writes, deletes after writes, and an explicit
//!   [`Applier::rollback`] for a partially applied operation.
//!
//! Errors are plain [`std::io::Error`] values wrapping a [`RewindError`]; use
//! [`rewind_error`] to recover the structured cause.
//!
//! Nothing here talks to a transcript, a TUI or a runtime: the crate is pure
//! storage + planning, and its public surface is stable for the runtime caller.

#![forbid(unsafe_code)]

mod apply;
mod diff;
mod error;
mod path;
mod plan;
mod store;

/// Hard cap for a single captured file pre-image (mirrors `MAX_EDIT_FILE_BYTES`).
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Files at or above this size that are valid UTF-8 are stored zstd-compressed.
pub const ZSTD_THRESHOLD_BYTES: u64 = 4096;

pub use apply::{Applier, ApplyReport, RollbackReport};
pub use diff::{FileDiffPreview, plan_diff_previews};
pub use error::{RewindError, rewind_error};
pub use path::{file_sha256, resolve_inside};
pub use plan::{
    LineDiffStats, PlanOptions, PlanSkip, PlanWarning, PlannedDelete, PlannedWrite, RestorePlan,
    SnapshotFileView, SnapshotView, build_plan, line_diff_stats,
};
pub use store::{BlobEncoding, BlobRef, GcReport, PreImage, RewindStore};

/// Largest pre-image the planner will run a text diff over (`similar` input cap).
pub(crate) const MAX_DIFF_INPUT_BYTES: u64 = 4 * 1024 * 1024;
