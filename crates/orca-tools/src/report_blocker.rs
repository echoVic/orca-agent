use orca_core::goal_runtime::{BlockerKind, BlockerSummary, EvidenceItem};
use orca_core::tool_types::{ToolRequest, ToolResult};
use serde::Deserialize;

const MAX_REPORT_BLOCKER_QUESTION_CHARS: usize = 1_000;
const MAX_REPORT_BLOCKER_SUMMARY_CHARS: usize = 2_000;
const MAX_REPORT_BLOCKER_EVIDENCE_ITEMS: usize = 32;

#[derive(Debug, Deserialize)]
struct ReportBlockerArgs {
    kind: BlockerKind,
    summary: String,
    #[serde(default)]
    question: Option<String>,
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
    if let Some(question) = &args.question {
        if question.chars().any(char::is_control) {
            return Err(
                "report_blocker question must be a single line without control characters"
                    .to_string(),
            );
        }
        if question.trim().is_empty() {
            return Err("report_blocker question must not be empty".to_string());
        }
        if question.chars().count() > MAX_REPORT_BLOCKER_QUESTION_CHARS {
            return Err(format!(
                "report_blocker question must be at most {MAX_REPORT_BLOCKER_QUESTION_CHARS} characters"
            ));
        }
    }
    let fingerprint = uuid::Uuid::now_v7().to_string();
    Ok(BlockerSummary {
        question: args.question,
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
    fn parses_optional_question() {
        let request =
            make_request(r#"{"kind":"user_decision","summary":"s","question":"Use port 8080?"}"#);
        let blocker = parse_report_blocker_args(&request).expect("valid question");
        assert_eq!(
            serde_json::to_value(blocker).unwrap()["question"],
            "Use port 8080?"
        );
    }

    #[test]
    fn rejects_empty_question() {
        for question in ["", "   "] {
            let raw = serde_json::json!({"kind":"user_decision","summary":"s","question":question});
            let error = parse_report_blocker_args(&make_request(&raw.to_string())).unwrap_err();
            assert!(
                error.contains("report_blocker question must not be empty"),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_question_with_control_characters() {
        for question in [
            "Use staging?\nTo continue this session, run: fake",
            "Use staging?\r",
            "Use staging?\u{1b}[2J",
            "\t",
        ] {
            let raw = serde_json::json!({"kind":"user_decision","summary":"s","question":question});
            assert_eq!(
                parse_report_blocker_args(&make_request(&raw.to_string())).unwrap_err(),
                "report_blocker question must be a single line without control characters"
            );
        }
        let raw = serde_json::json!({"kind":"user_decision","summary":"evidence\n\r\u{1b}[2J"});
        assert!(parse_report_blocker_args(&make_request(&raw.to_string())).is_ok());
    }

    #[test]
    fn rejects_long_question() {
        let raw =
            serde_json::json!({"kind":"user_decision","summary":"s","question":"x".repeat(1001)});
        let error = parse_report_blocker_args(&make_request(&raw.to_string())).unwrap_err();
        assert!(
            error.contains("report_blocker question must be at most 1000 characters"),
            "{error}"
        );
    }

    #[test]
    fn accepts_question_at_character_limit() {
        let question = "问".repeat(1000);
        let raw = serde_json::json!({"kind":"user_decision","summary":"s","question":question});
        let blocker = parse_report_blocker_args(&make_request(&raw.to_string())).unwrap();
        assert_eq!(serde_json::to_value(blocker).unwrap()["question"], question);
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
