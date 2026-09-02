#![deny(deprecated)]

pub mod agent_event;
pub mod approval_rules;
pub mod approval_types;
pub mod budget;
pub mod cancel;
pub mod capability;
pub mod config;
pub mod conversation;
pub mod cost_types;
pub mod event_schema;
pub mod event_sink;
pub mod execution_broker;
pub mod external_config;
pub mod goal_runtime;
pub mod goal_types;
pub mod hook_types;
pub mod mcp_types;
pub mod model;
pub mod plan_types;
pub mod proposed_plan;
pub mod provider_types;
pub mod retained_output;
pub mod subagent_config;
pub mod subagent_types;
pub mod task_types;
pub mod thread_identity;
pub mod thread_item_projection;
pub mod tool_types;
pub mod verification;
pub mod workflow_types;
pub mod workspace_identity;

#[cfg(test)]
mod proposed_plan_tests {
    use crate::proposed_plan::{ProposedPlanSegment, ProposedPlanStreamParser};

    #[test]
    fn proposed_plan_parser_handles_split_tags_and_preserves_agent_text() {
        let mut parser = ProposedPlanStreamParser::default();

        let mut segments = parser.push("Intro\n<proposed");
        segments.extend(parser.push("_plan>\n# Plan\n- inspect\n</proposed_plan>\nOutro"));

        assert_eq!(
            segments,
            vec![
                ProposedPlanSegment::Agent("Intro\n".to_string()),
                ProposedPlanSegment::Plan("# Plan\n- inspect\n".to_string()),
                ProposedPlanSegment::Agent("\nOutro".to_string()),
            ]
        );
    }

    #[test]
    fn proposed_plan_parser_flushes_incomplete_plan_as_agent_text() {
        let mut parser = ProposedPlanStreamParser::default();

        let mut segments = parser.push("Intro\n<proposed_plan> unfinished");
        segments.extend(parser.finish());

        let agent_text: String = segments
            .iter()
            .filter_map(|segment| match segment {
                ProposedPlanSegment::Agent(text) => Some(text.as_str()),
                ProposedPlanSegment::Plan(_) => None,
            })
            .collect();

        assert_eq!(agent_text, "Intro\n<proposed_plan> unfinished");
        assert!(
            segments
                .iter()
                .all(|segment| matches!(segment, ProposedPlanSegment::Agent(_)))
        );
    }

    #[test]
    fn proposed_plan_parser_handles_non_ascii_agent_text_without_panicking() {
        let mut parser = ProposedPlanStreamParser::default();

        let segments = parser.push("第000段:内容片全芯业型环训练力栈全首片闭\n\n");

        assert_eq!(
            segments,
            vec![ProposedPlanSegment::Agent(
                "第000段:内容片全芯业型环训练力栈全首片闭\n\n".to_string()
            )]
        );
    }
}

#[cfg(test)]
mod thread_item_projection_tests {
    use crate::thread_identity::TurnId;
    use crate::thread_item_projection::{
        CompletedModelItem, CompletedModelResponse, ModelResponseIdentity,
    };

    #[test]
    fn completed_model_response_owns_stable_text_items_and_plan_reduction() {
        let identity = ModelResponseIdentity::new(TurnId::new());
        let response = CompletedModelResponse::new(
            identity.clone(),
            Some("Preface\n<proposed_plan>\n1. inspect\n</proposed_plan>\nPostscript".to_string()),
            Some("thinking".to_string()),
            Vec::new(),
        );
        let items = response.completed_items();

        assert_eq!(items.len(), 3);
        assert!(matches!(
            &items[0],
            CompletedModelItem::AgentMessage { id, text }
                if id == identity.item_ids.agent_message_item_id()
                    && text == "Preface\n\nPostscript"
        ));
        assert!(matches!(
            &items[1],
            CompletedModelItem::Plan { id, text }
                if id == &identity.item_ids.plan_item_id && text == "1. inspect\n"
        ));
        assert!(matches!(
            &items[2],
            CompletedModelItem::Reasoning { id, summary, content }
                if id == &identity.item_ids.reasoning_item_id
                    && summary == "thinking" && content.is_empty()
        ));

        let started = items[1].started_item().into_value();
        let completed = items[1].clone().into_value();
        assert_eq!(started["id"], identity.item_ids.plan_item_id.as_str());
        assert_eq!(started["type"], "plan");
        assert_eq!(started["text"], "");
        assert_eq!(completed["id"], identity.item_ids.plan_item_id.as_str());
        assert_eq!(completed["type"], "plan");
        assert_eq!(completed["text"], "1. inspect\n");
    }
}
