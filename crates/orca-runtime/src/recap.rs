//! Read-only session recap contracts and evidence projection.
//!
//! Recap deliberately lives beside the runtime actor.  It can inspect the
//! typed surface projection, but it never appends a surface event or mutates
//! the active conversation.

use std::time::{Duration, Instant};

use orca_core::cancel::CancelToken;
use orca_core::config::ProviderKind;
use orca_core::provider_types::Usage;
use orca_provider::context::TokenCounter;
use sha2::{Digest, Sha256};

use crate::runtime_surface as surface;

pub const RECAP_INPUT_TOKEN_LIMIT: usize = 4_096;
pub const RECAP_OUTPUT_TOKEN_LIMIT: usize = 120;
pub const RECAP_DEADLINE: Duration = Duration::from_secs(8);
const RECAP_EVIDENCE_BYTE_LIMIT: usize = 24 * 1024;
const EVIDENCE_TRUNCATION_MARKER: &str = "[earlier session evidence omitted]";
/// A tool's target (a path, a command line) is identification, not content.
const TOOL_TARGET_CHAR_LIMIT: usize = 120;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecapRequestId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecapContentMarker {
    pub thread_id: surface::SurfaceThreadId,
    pub incarnation: surface::SurfaceIncarnation,
    pub completed_user_operations: u64,
    pub evidence_digest: surface::Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecapSourceFence {
    pub cursor: surface::SurfaceCursor,
    pub marker: RecapContentMarker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecapTrigger {
    Manual,
    Automatic,
}

/// Build the runtime-owned inflight identity for a recap request.
///
/// The request id is a delivery identity only.  Two callers asking for the
/// same content and provider configuration must share one worker even when
/// their local request ids differ -- and even when surface events that do not
/// change the recap's evidence (usage, settings) moved the cursor in between,
/// which is why the key is the content marker and not the cursor.
pub fn dedupe_key(request: &RecapRequest, provider_fingerprint: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!(
        "{:?}|{}|{}",
        request.source.marker,
        provider_fingerprint,
        match request.trigger {
            RecapTrigger::Manual => "manual",
            RecapTrigger::Automatic => "automatic",
        },
    ));
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecapEvidence {
    pub digest: String,
    pub text: String,
    pub input_tokens: usize,
    pub completed_user_operations: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RecapUsage {
    Provider(Usage),
    #[default]
    Cached,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecapSkipReason {
    Ineligible,
    Duplicate,
    EmptyEvidence,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecapRequest {
    pub request_id: RecapRequestId,
    pub source: RecapSourceFence,
    pub trigger: RecapTrigger,
    pub evidence: RecapEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafeDiagnosticText(String);

impl SafeDiagnosticText {
    pub fn new(value: impl Into<String>) -> Self {
        let mut value = value.into();
        if value.len() > surface::SAFE_DIAGNOSTIC_TEXT_BYTE_LIMIT {
            let mut end = surface::SAFE_DIAGNOSTIC_TEXT_BYTE_LIMIT;
            while end > 0 && !value.is_char_boundary(end) {
                end -= 1;
            }
            value.truncate(end);
        }
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecapResult {
    Ready {
        request_id: RecapRequestId,
        source: RecapSourceFence,
        text: String,
        usage: RecapUsage,
    },
    Failed {
        request_id: RecapRequestId,
        source: RecapSourceFence,
        error: SafeDiagnosticText,
    },
    Skipped {
        request_id: RecapRequestId,
        reason: RecapSkipReason,
    },
}

/// Return the bounded, redacted evidence sent to the display-only provider.
pub fn recap_evidence(snapshot: &surface::SurfaceSnapshot) -> RecapEvidence {
    let mut sections = Vec::new();
    let tools = snapshot
        .tools
        .iter()
        .map(|tool| (&tool.request.tool_call_id, &tool.request))
        .collect::<std::collections::HashMap<_, _>>();

    for item in &snapshot.items {
        match item {
            surface::SurfaceItem::UserMessage { input, .. } => match input {
                surface::SurfaceUserInputState::Pending { .. } => {
                    sections.push("user: [pending input]".to_string());
                }
                surface::SurfaceUserInputState::Resolved { fact } => {
                    let text = match fact {
                        surface::SurfaceResolvedInputFact::Replayable { input, .. } => {
                            input.canonical_text.as_str()
                        }
                        surface::SurfaceResolvedInputFact::NonReplayable {
                            presentation: surface::SurfaceInputPresentation::Visible { text },
                            ..
                        } => text.as_str(),
                        surface::SurfaceResolvedInputFact::NonReplayable {
                            presentation: surface::SurfaceInputPresentation::Redacted,
                            ..
                        } => "[redacted input]",
                    };
                    sections.push(format!("user: {text}"));
                }
                surface::SurfaceUserInputState::ResolutionFailed { .. } => {
                    sections.push("user: [input resolution failed]".to_string());
                }
            },
            surface::SurfaceItem::AssistantMessage { text, .. } => {
                sections.push(format!("assistant: {}", text.as_str()));
            }
            surface::SurfaceItem::AssistantPlan { text, .. } => {
                sections.push(format!("plan: {}", text.as_str()));
            }
            surface::SurfaceItem::ToolResultMessage {
                tool_call_id,
                terminal,
                ..
            } => {
                let label = tools
                    .get(tool_call_id)
                    .map(|request| {
                        request
                            .target
                            .as_ref()
                            .map(|target| {
                                format!(
                                    "{} {}",
                                    request.name.as_str(),
                                    bound_chars(target.as_str(), TOOL_TARGET_CHAR_LIMIT)
                                )
                            })
                            .unwrap_or_else(|| request.name.as_str().to_string())
                    })
                    .unwrap_or_else(|| "tool".to_string());
                sections.push(format!("tool: {label} ({:?})", terminal.kind));
            }
            surface::SurfaceItem::SystemMessage { .. }
            | surface::SurfaceItem::AssistantReasoning { .. } => {}
        }
    }

    if let Some(goal) = &snapshot.goal {
        sections.push(format!(
            "goal: {} ({:?})",
            goal.objective.as_str(),
            goal.state
        ));
    }
    if let Some(explanation) = &snapshot.plan.explanation {
        sections.push(format!("plan context: {}", explanation.as_str()));
    }
    for item in &snapshot.plan.items {
        sections.push(format!(
            "plan step: {} ({:?})",
            item.step.as_str(),
            item.status
        ));
    }

    let completed_user_operations = snapshot
        .operation_history
        .iter()
        .chain(snapshot.foreground_operation.iter())
        .filter(|operation| {
            matches!(operation.intent.kind, surface::OperationKind::UserTurn)
                && operation.terminal.is_some()
        })
        .count() as u64;
    let text = bound_evidence_sections(&sections);
    let digest_bytes = Sha256::digest(text.as_bytes());
    let digest = digest_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let input_tokens = orca_provider::context::DefaultTokenCounter.count_text(&text);

    RecapEvidence {
        digest,
        text,
        input_tokens,
        completed_user_operations,
    }
}

pub fn content_marker(
    snapshot: &surface::SurfaceSnapshot,
    evidence: &RecapEvidence,
) -> RecapContentMarker {
    RecapContentMarker {
        thread_id: snapshot.thread.thread_id.clone(),
        incarnation: snapshot.cursor.incarnation.clone(),
        completed_user_operations: evidence.completed_user_operations,
        evidence_digest: surface::Sha256Digest::digest(evidence.digest.as_bytes()),
    }
}

pub fn source_fence(
    snapshot: &surface::SurfaceSnapshot,
    evidence: &RecapEvidence,
) -> RecapSourceFence {
    RecapSourceFence {
        cursor: snapshot.cursor.clone(),
        marker: content_marker(snapshot, evidence),
    }
}

pub fn request_recap(
    provider_kind: ProviderKind,
    provider_config: &orca_provider::ProviderConfig,
    request: &RecapRequest,
    cancel: &CancelToken,
    deadline: Instant,
) -> RecapResult {
    if cancel.is_cancelled() {
        return RecapResult::Skipped {
            request_id: request.request_id,
            reason: RecapSkipReason::Cancelled,
        };
    }
    let evidence = orca_provider::DisplaySummaryEvidence {
        digest: request.evidence.digest.clone(),
        text: request.evidence.text.clone(),
        input_tokens: request.evidence.input_tokens,
    };
    match orca_provider::request_display_summary(
        provider_kind,
        provider_config,
        &evidence,
        cancel,
        deadline,
    ) {
        Ok(result) => RecapResult::Ready {
            request_id: request.request_id,
            source: request.source.clone(),
            text: result.text,
            usage: result
                .usage
                .map(RecapUsage::Provider)
                .unwrap_or(RecapUsage::Cached),
        },
        // Cancelling is how a request is withdrawn, not a failure to report.
        Err(orca_provider::DisplaySummaryError::Cancelled) => RecapResult::Skipped {
            request_id: request.request_id,
            reason: RecapSkipReason::Cancelled,
        },
        Err(error) => RecapResult::Failed {
            request_id: request.request_id,
            source: request.source.clone(),
            error: SafeDiagnosticText::new(error.to_string()),
        },
    }
}

/// Keep the newest sections that fit the byte and token limits, oldest
/// dropped first, so the latest facts and the current goal and plan survive.
/// Each section is measured once and only the kept ones are tokenized, so a
/// long session costs no more than a short one.
fn bound_evidence_sections(sections: &[String]) -> String {
    if sections.is_empty() {
        return String::new();
    }

    let counter = orca_provider::context::DefaultTokenCounter;
    let joined_fits = |text: &str| {
        text.len() <= RECAP_EVIDENCE_BYTE_LIMIT
            && counter.count_text(text) <= RECAP_INPUT_TOKEN_LIMIT
    };
    // What the omission marker and its newline cost when sections are dropped.
    let marker_bytes = EVIDENCE_TRUNCATION_MARKER.len() + 1;
    let marker_tokens = counter.count_text(EVIDENCE_TRUNCATION_MARKER) + 1;

    let mut kept = 0usize;
    let mut bytes = marker_bytes;
    let mut tokens = marker_tokens;
    for section in sections.iter().rev() {
        let separator = usize::from(kept > 0);
        let next_bytes = bytes + section.len() + separator;
        if next_bytes > RECAP_EVIDENCE_BYTE_LIMIT {
            break;
        }
        let next_tokens = tokens + counter.count_text(section) + separator;
        if next_tokens > RECAP_INPUT_TOKEN_LIMIT {
            break;
        }
        bytes = next_bytes;
        tokens = next_tokens;
        kept += 1;
    }

    if kept == sections.len() {
        let joined = sections.join("\n");
        if joined_fits(&joined) {
            return joined;
        }
    }
    // Section counts summed can differ from the joined text's count by a
    // token here and there, so the result is checked once as a whole.
    while kept > 0 {
        let bounded = format!(
            "{EVIDENCE_TRUNCATION_MARKER}\n{}",
            sections[sections.len() - kept..].join("\n")
        );
        if joined_fits(&bounded) {
            return bounded;
        }
        kept -= 1;
    }

    // The newest section alone is over the limits. Keep its tail so the
    // conclusion at the end of a long message survives the bound.
    let newest = sections.last().expect("non-empty sections");
    let tail = truncate_tail_to_limits(
        newest,
        RECAP_EVIDENCE_BYTE_LIMIT.saturating_sub(marker_bytes),
        RECAP_INPUT_TOKEN_LIMIT.saturating_sub(marker_tokens),
        &counter,
    );
    if tail.is_empty() {
        EVIDENCE_TRUNCATION_MARKER.to_string()
    } else {
        format!("{EVIDENCE_TRUNCATION_MARKER}\n{tail}")
    }
}

fn bound_chars(value: &str, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value.to_string(),
    }
}

fn truncate_tail_to_limits(
    value: &str,
    byte_limit: usize,
    token_limit: usize,
    counter: &impl orca_provider::context::TokenCounter,
) -> String {
    if value.len() <= byte_limit && counter.count_text(value) <= token_limit {
        return value.to_string();
    }
    let boundaries = value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
        .collect::<Vec<_>>();
    let mut low = 0usize;
    let mut high = boundaries.len().saturating_sub(1);
    while low < high {
        let middle = (low + high) / 2;
        let suffix = &value[boundaries[middle]..];
        if suffix.len() <= byte_limit && counter.count_text(suffix) <= token_limit {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    for start in low..boundaries.len() {
        let suffix = &value[boundaries[start]..];
        if suffix.len() <= byte_limit && counter.count_text(suffix) <= token_limit {
            return suffix.trim_start().to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_provider::context::{DefaultTokenCounter, TokenCounter};

    #[test]
    fn bounds_unicode_without_splitting_utf8() {
        let value = "鲸鱼".repeat(20_000);
        let bounded = bound_evidence_sections(&[value]);
        assert!(bounded.starts_with(EVIDENCE_TRUNCATION_MARKER));
        assert!(bounded.ends_with("鲸鱼"));
        assert!(bounded.len() <= RECAP_EVIDENCE_BYTE_LIMIT);
        assert!(DefaultTokenCounter.count_text(&bounded) <= RECAP_INPUT_TOKEN_LIMIT);
    }

    #[test]
    fn short_evidence_is_kept_whole_and_unmarked() {
        let sections = vec!["user: fix it".to_string(), "assistant: fixed".to_string()];
        assert_eq!(
            bound_evidence_sections(&sections),
            "user: fix it\nassistant: fixed"
        );
    }

    #[test]
    fn a_long_session_keeps_only_its_newest_sections_in_order() {
        let mut sections = (0..20_000)
            .map(|index| format!("assistant: historical fact {index} {}", "word ".repeat(40)))
            .collect::<Vec<_>>();
        sections.push("assistant: latest fact".to_string());
        sections.push("plan: current plan".to_string());

        let bounded = bound_evidence_sections(&sections);
        assert!(bounded.starts_with(EVIDENCE_TRUNCATION_MARKER));
        assert!(bounded.ends_with("assistant: latest fact\nplan: current plan"));
        assert!(!bounded.contains("historical fact 0 "));
        assert!(bounded.contains("historical fact 19999 "));
        assert!(bounded.len() <= RECAP_EVIDENCE_BYTE_LIMIT);
        assert!(DefaultTokenCounter.count_text(&bounded) <= RECAP_INPUT_TOKEN_LIMIT);
    }

    #[test]
    fn long_tool_targets_are_cut_on_a_char_boundary() {
        assert_eq!(bound_chars("short", 120), "short");
        assert_eq!(bound_chars("鲸鱼鲸鱼", 2), "鲸鱼…");
    }

    #[test]
    fn evidence_bound_keeps_latest_fact_and_plan_within_token_limit() {
        let mut sections = (0..80)
            .map(|index| format!("assistant: historical fact {index} {}", "x ".repeat(500)))
            .collect::<Vec<_>>();
        sections.push("assistant: latest fact".to_string());
        sections.push("plan: current plan".to_string());

        let bounded = bound_evidence_sections(&sections);
        let counter = DefaultTokenCounter;
        assert!(counter.count_text(&bounded) <= RECAP_INPUT_TOKEN_LIMIT);
        assert!(bounded.len() <= RECAP_EVIDENCE_BYTE_LIMIT);
        assert!(bounded.contains("assistant: latest fact"));
        assert!(bounded.contains("plan: current plan"));
    }
}
