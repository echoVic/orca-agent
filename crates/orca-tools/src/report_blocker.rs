use orca_core::goal_runtime::{BlockerKind, BlockerSummary, EvidenceItem};
use orca_core::tool_types::{ToolRequest, ToolResult};
use serde::Deserialize;

const MAX_REPORT_BLOCKER_SUMMARY_CHARS: usize = 2_000;
const MAX_REPORT_BLOCKER_EVIDENCE_ITEMS: usize = 32;

#[derive(Debug, Deserialize)]
struct ReportBlockerArgs {
    kind: BlockerKind,
    summary: String,
    #[serde(default)]
    evidence: Vec<EvidenceItem>,
}

/// Parse and validate report_blocker args, return a BlockerSummary.
pub fn parse_report_blocker_args(request: &ToolRequest) -> Result<BlockerSummary, String> {
    let raw = request.raw_arguments.as_deref().unwrap_or("{}");
    let args: ReportBlockerArgs = serde_json::from_str(raw)
        .map_err(|e| format!("report_blocker arguments are not valid JSON: {e}"))?;
    if args.summary.chars().count() > MAX_REPORT_BLOCKER_SUMMARY_CHARS {
        return Err(format!(
            "report_blocker summary must be at most {MAX_REPORT_BLOCKER_SUMMARY_CHARS} characters"
        ));
    }
    if args.evidence.len() > MAX_REPORT_BLOCKER_EVIDENCE_ITEMS {
        return Err(format!(
            "report_blocker evidence must contain at most {MAX_REPORT_BLOCKER_EVIDENCE_ITEMS} items"
        ));
    }
    let fingerprint = uuid::Uuid::now_v7().to_string();
    Ok(BlockerSummary {
        kind: args.kind,
        summary: args.summary,
        fingerprint,
        evidence: args.evidence,
    })
}

/// Placeholder executor — the agent loop router intercepts before this is called.
pub fn execute_report_blocker(request: &ToolRequest) -> ToolResult {
    execute_unavailable(request)
}

fn execute_unavailable(request: &ToolRequest) -> ToolResult {
    ToolResult::failed(request, "report_blocker is handled by the runtime", None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;

    fn make_request(raw: &str) -> ToolRequest {
        ToolRequest {
            id: "tc-1".to_string(),
            name: ToolName::ReportBlocker,
            action: ActionKind::Agent,
            target: None,
            raw_arguments: Some(raw.to_string()),
        }
    }

    #[test]
    fn parses_valid_args() {
        let request = make_request(
            r#"{"kind":"environment_contradiction","summary":"workspace is read-only"}"#,
        );
        let blocker = parse_report_blocker_args(&request).expect("valid args");
        assert_eq!(blocker.kind, BlockerKind::EnvironmentContradiction);
        assert_eq!(blocker.summary, "workspace is read-only");
        assert!(blocker.evidence.is_empty());
        assert!(!blocker.fingerprint.is_empty());
    }

    #[test]
    fn rejects_summary_over_limit() {
        let long = "x".repeat(2001);
        let request = make_request(&format!(r#"{{"kind":"user_decision","summary":"{long}"}}"#));
        assert!(parse_report_blocker_args(&request).is_err());
    }

    #[test]
    fn rejects_too_many_evidence_items() {
        let evidence: Vec<_> = (0..33)
            .map(|i| serde_json::json!({"kind": "observation", "summary": format!("item {i}")}))
            .collect();
        let raw = serde_json::to_string(
            &serde_json::json!({"kind":"user_decision","summary":"s","evidence":evidence}),
        )
        .unwrap();
        let request = make_request(&raw);
        assert!(parse_report_blocker_args(&request).is_err());
    }
}
