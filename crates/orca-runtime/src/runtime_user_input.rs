use std::collections::{BTreeMap, HashSet};
use std::io;

use orca_core::tool_types::{ToolRequest, ToolResult};
use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeUserInputRequest {
    pub id: String,
    pub questions: Vec<RuntimeUserInputQuestion>,
}

impl RuntimeUserInputRequest {
    pub fn single(
        id: impl Into<String>,
        question: impl Into<String>,
        choices: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            questions: vec![RuntimeUserInputQuestion {
                id: "question-1".to_string(),
                header: "Question".to_string(),
                question: question.into(),
                options: choices
                    .into_iter()
                    .map(|label| RuntimeUserInputOption {
                        label,
                        description: String::new(),
                        preview: None,
                    })
                    .collect(),
                multi_select: false,
            }],
        }
    }

    /// Recognizes only the compatibility shape emitted by `single`.
    pub(crate) fn legacy_single_question(&self) -> Option<&RuntimeUserInputQuestion> {
        let [question] = self.questions.as_slice() else {
            return None;
        };
        (question.id == "question-1"
            && question.header == "Question"
            && !question.multi_select
            && question
                .options
                .iter()
                .all(|option| option.description.is_empty() && option.preview.is_none()))
        .then_some(question)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeUserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<RuntimeUserInputOption>,
    pub multi_select: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeUserInputOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeUserInputAnswer {
    pub question_id: String,
    pub answers: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeUserInputResponse {
    Submitted {
        answers: Vec<RuntimeUserInputAnswer>,
    },
    Chat {
        message: String,
    },
}

pub trait RuntimeUserInputHandler {
    fn request_user_input(
        &self,
        request: &RuntimeUserInputRequest,
    ) -> io::Result<Option<RuntimeUserInputResponse>>;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserQuestionArgs {
    questions: Vec<AskUserQuestion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskUserQuestion {
    header: String,
    question: String,
    options: Vec<AskUserQuestionOption>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserQuestionOption {
    label: String,
    description: String,
    #[serde(default)]
    preview: Option<String>,
}

pub(crate) fn execute_user_input_tool(
    request: &ToolRequest,
    handler: &dyn RuntimeUserInputHandler,
) -> io::Result<ToolResult> {
    execute_ask_user_question_tool(request, handler)
}

pub(crate) fn execute_ask_user_question_tool(
    request: &ToolRequest,
    handler: &dyn RuntimeUserInputHandler,
) -> io::Result<ToolResult> {
    let questions = parse_ask_user_question_request(request)?;
    let input = RuntimeUserInputRequest {
        id: request.id.clone(),
        questions: questions
            .iter()
            .enumerate()
            .map(|(index, question)| RuntimeUserInputQuestion {
                id: format!("question-{}", index + 1),
                header: question.header.trim().to_string(),
                question: question.question.trim().to_string(),
                options: question
                    .options
                    .iter()
                    .map(|option| RuntimeUserInputOption {
                        label: option.label.trim().to_string(),
                        description: option.description.trim().to_string(),
                        preview: option
                            .preview
                            .as_deref()
                            .map(str::trim)
                            .filter(|preview| !preview.is_empty())
                            .map(str::to_string),
                    })
                    .collect(),
                multi_select: question.multi_select,
            })
            .collect(),
    };
    let Some(response) = handler.request_user_input(&input)? else {
        return Ok(ToolResult::cancelled(
            request,
            "user question request cancelled",
            None,
        ));
    };
    let output = match response {
        RuntimeUserInputResponse::Submitted { answers: submitted } => {
            let question_by_id = input
                .questions
                .iter()
                .map(|question| (question.id.clone(), question.question.clone()))
                .collect::<BTreeMap<_, _>>();
            let mut seen = HashSet::new();
            let mut answers = BTreeMap::new();
            for answer in submitted {
                let Some(question) = question_by_id.get(&answer.question_id) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "user-input response references unknown question id '{}'",
                            answer.question_id
                        ),
                    ));
                };
                if !seen.insert(answer.question_id.clone()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "user-input response repeats question id '{}'",
                            answer.question_id
                        ),
                    ));
                }
                let answer = answer
                    .answers
                    .into_iter()
                    .map(|answer| answer.trim().to_string())
                    .filter(|answer| !answer.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                if !answer.is_empty() {
                    answers.insert(question.clone(), answer);
                }
            }
            serde_json::json!({ "answers": answers })
        }
        RuntimeUserInputResponse::Chat { message } => {
            serde_json::json!({ "answers": {}, "chat": message })
        }
    };

    let output = serde_json::to_string(&output).map_err(|error| {
        io::Error::other(format!(
            "failed to serialize ask_user_question answers: {error}"
        ))
    })?;
    Ok(ToolResult::completed(request, output, false))
}

fn parse_ask_user_question_request(request: &ToolRequest) -> io::Result<Vec<AskUserQuestion>> {
    let raw = request.raw_arguments.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing ask_user_question arguments JSON",
        )
    })?;
    let args: AskUserQuestionArgs = serde_json::from_str(raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid ask_user_question arguments JSON: {error}"),
        )
    })?;
    if !(1..=4).contains(&args.questions.len()) {
        return Err(invalid_questionnaire(
            "ask_user_question requires between 1 and 4 questions",
        ));
    }

    let mut question_texts = HashSet::new();
    for (index, question) in args.questions.iter().enumerate() {
        let position = index + 1;
        let header = question.header.trim();
        if header.is_empty() || header.chars().count() > 12 {
            return Err(invalid_questionnaire(format!(
                "ask_user_question question {position} header must contain 1 to 12 characters"
            )));
        }
        let question_text = question.question.trim();
        if question_text.is_empty() {
            return Err(invalid_questionnaire(format!(
                "ask_user_question question {position} text must not be empty"
            )));
        }
        if !question_texts.insert(question_text.to_string()) {
            return Err(invalid_questionnaire(format!(
                "ask_user_question question {position} duplicates an earlier question"
            )));
        }
        if !(2..=4).contains(&question.options.len()) {
            return Err(invalid_questionnaire(format!(
                "ask_user_question question {position} requires between 2 and 4 options"
            )));
        }
        let mut labels = HashSet::new();
        for (option_index, option) in question.options.iter().enumerate() {
            let option_position = option_index + 1;
            let label = option.label.trim();
            if label.is_empty() {
                return Err(invalid_questionnaire(format!(
                    "ask_user_question question {position} option {option_position} label must not be empty"
                )));
            }
            if !labels.insert(label.to_string()) {
                return Err(invalid_questionnaire(format!(
                    "ask_user_question question {position} option labels must be distinct"
                )));
            }
            if option.description.trim().is_empty() {
                return Err(invalid_questionnaire(format!(
                    "ask_user_question question {position} option {option_position} description must not be empty"
                )));
            }
        }
    }

    Ok(args.questions)
}

fn invalid_questionnaire(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};

    use super::*;

    struct RecordingHandler {
        answer: Mutex<Option<RuntimeUserInputResponse>>,
        requests: Mutex<Vec<RuntimeUserInputRequest>>,
    }

    impl RecordingHandler {
        fn new(answer: Option<RuntimeUserInputResponse>) -> Self {
            Self {
                answer: Mutex::new(answer),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl RuntimeUserInputHandler for RecordingHandler {
        fn request_user_input(
            &self,
            request: &RuntimeUserInputRequest,
        ) -> io::Result<Option<RuntimeUserInputResponse>> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(self.answer.lock().unwrap().take())
        }
    }

    fn questionnaire_request(arguments: &str) -> ToolRequest {
        ToolRequest {
            id: "ask-1".to_string(),
            name: ToolName::plain("ask_user_question"),
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(arguments.to_string()),
        }
    }

    #[test]
    fn legacy_single_detection_does_not_capture_structured_questions() {
        let legacy =
            RuntimeUserInputRequest::single("legacy", "Continue?", vec!["yes".to_string()]);
        assert!(legacy.legacy_single_question().is_some());

        let structured = RuntimeUserInputRequest {
            id: "structured".to_string(),
            questions: vec![RuntimeUserInputQuestion {
                id: "question-1".to_string(),
                header: "Question".to_string(),
                question: "Continue?".to_string(),
                options: vec![RuntimeUserInputOption {
                    label: "yes".to_string(),
                    description: "Continue the operation".to_string(),
                    preview: None,
                }],
                multi_select: false,
            }],
        };
        assert!(structured.legacy_single_question().is_none());
    }

    #[test]
    fn ask_user_question_collects_ordered_answers_through_typed_handler() {
        let request = questionnaire_request(
            r#"{
                "questions": [
                    {
                        "header": "Runtime",
                        "question": "Which path?",
                        "options": [
                            {"label": "Reuse", "description": "Use the runtime broker"},
                            {"label": "New", "description": "Create another interaction path"}
                        ],
                        "multiSelect": false
                    },
                    {
                        "header": "Signals",
                        "question": "Which signals?",
                        "options": [
                            {"label": "Logs", "description": "Capture structured logs"},
                            {"label": "Metrics", "description": "Capture numeric metrics", "preview": "p95"}
                        ],
                        "multiSelect": true
                    }
                ]
            }"#,
        );
        let handler = RecordingHandler::new(Some(RuntimeUserInputResponse::Submitted {
            answers: vec![
                RuntimeUserInputAnswer {
                    question_id: "question-1".to_string(),
                    answers: vec!["Reuse".to_string()],
                },
                RuntimeUserInputAnswer {
                    question_id: "question-2".to_string(),
                    answers: vec!["Logs".to_string(), "Metrics".to_string()],
                },
            ],
        }));

        let result = execute_ask_user_question_tool(&request, &handler).expect("questionnaire");

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(result.output.as_deref().unwrap()).unwrap(),
            serde_json::json!({
                "answers": {
                    "Which path?": "Reuse",
                    "Which signals?": "Logs, Metrics"
                }
            })
        );
        let requests = handler.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].id, "ask-1");
        assert_eq!(requests[0].questions.len(), 2);
        assert_eq!(requests[0].questions[0].header, "Runtime");
        assert_eq!(requests[0].questions[0].question, "Which path?");
        assert_eq!(
            requests[0].questions[0].options[0],
            RuntimeUserInputOption {
                label: "Reuse".to_string(),
                description: "Use the runtime broker".to_string(),
                preview: None,
            }
        );
        assert_eq!(
            requests[0].questions[1].options[1].preview.as_deref(),
            Some("p95")
        );
        assert!(requests[0].questions[1].multi_select);
    }

    #[test]
    fn ask_user_question_cancels_whole_tool_when_any_question_is_dismissed() {
        let request = questionnaire_request(
            r#"{"questions":[{"header":"Runtime","question":"Which path?","options":[{"label":"Reuse","description":"Use it"},{"label":"New","description":"Replace it"}]}]}"#,
        );
        let handler = RecordingHandler::new(None);

        let result = execute_ask_user_question_tool(&request, &handler).expect("cancel result");

        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(
            result.error.as_deref(),
            Some("user question request cancelled")
        );
    }

    #[test]
    fn ask_user_question_returns_chat_response_for_follow_up_round() {
        let request = questionnaire_request(
            r#"{"questions":[{"header":"Runtime","question":"Which path?","options":[{"label":"Reuse","description":"Use it"},{"label":"New","description":"Replace it"}]}]}"#,
        );
        let handler = RecordingHandler::new(Some(RuntimeUserInputResponse::Chat {
            message: "Explain the rollback tradeoff first.".to_string(),
        }));

        let result = execute_ask_user_question_tool(&request, &handler).expect("chat response");

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(result.output.as_deref().unwrap()).unwrap(),
            serde_json::json!({
                "answers": {},
                "chat": "Explain the rollback tradeoff first."
            })
        );
    }

    #[test]
    fn ask_user_question_rejects_invalid_questionnaire_bounds_and_content() {
        let invalid_arguments = [
            r#"{"questions":[]}"#,
            r#"{"questions":[{"header":"One","question":"1?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]},{"header":"Two","question":"2?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]},{"header":"Three","question":"3?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]},{"header":"Four","question":"4?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]},{"header":"Five","question":"5?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]}]}"#,
            r#"{"questions":[{"header":"","question":"Which?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]}]}"#,
            r#"{"questions":[{"header":"This header is too long","question":"Which?","options":[{"label":"A","description":"A"},{"label":"B","description":"B"}]}]}"#,
            r#"{"questions":[{"header":"Choice","question":"Which?","options":[{"label":"Only","description":"One"}]}]}"#,
            r#"{"questions":[{"header":"Choice","question":"Which?","options":[{"label":"Same","description":"One"},{"label":"Same","description":"Two"}]}]}"#,
            r#"{"questions":[{"header":"Choice","question":"Which?","options":[{"label":"A","description":"One"},{"label":"B","description":"Two"}],"multi_select":true}]}"#,
        ];

        for arguments in invalid_arguments {
            let error = execute_ask_user_question_tool(
                &questionnaire_request(arguments),
                &RecordingHandler::new(None),
            )
            .expect_err(arguments);
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{arguments}");
        }
    }
}
