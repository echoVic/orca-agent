use std::io;

use orca_core::conversation::{Conversation, Message};

use crate::lifecycle::RuntimeTurnContext;
use crate::thread_store::SessionWriter;

pub(crate) struct RuntimeSteerStep;

pub(crate) struct RuntimeSteerInput<'a> {
    pub(crate) turn_context: RuntimeTurnContext<'a>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
}

impl RuntimeSteerStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn apply(&mut self, mut input: RuntimeSteerInput<'_>) -> io::Result<usize> {
        let Some(steer_handle) = input.turn_context.steer_handle else {
            return Ok(0);
        };

        let mut injected = 0;
        while steer_handle.consume_next(|text| {
            let message = Message::user(text.to_owned());
            if let Some(writer) = input.history_writer.as_deref_mut() {
                writer.append_message(&message)?;
            }
            input.conversation.messages.push(message);
            input.conversation.note_provider_turn_steer();
            Ok(())
        })? {
            injected += 1;
        }
        Ok(injected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::ThreadSteerHandle;
    use orca_core::subagent_types::SubagentType;

    #[test]
    fn jsonl_steer_stays_unpublished_until_its_commit_succeeds() {
        use crate::runtime_surface::SurfaceItemId;
        let handle = ThreadSteerHandle::default();
        let input = SurfaceItemId::new();
        assert!(
            handle
                .commit_steer(input.clone(), "same text".into(), || {
                    assert!(!handle.has_pending());
                    Err(io::Error::other("steer commit failed"))
                })
                .is_err()
        );
        assert!(!handle.has_pending());
        assert!(
            handle
                .commit_steer(input, "same text".into(), || {
                    assert!(!handle.has_pending());
                    Ok(())
                })
                .unwrap()
        );
        assert_eq!(handle.drain(), ["same text"]);
    }

    #[test]
    fn a_failed_steer_history_append_keeps_both_inputs_pending() {
        let temp = tempfile::tempdir().unwrap();
        let history = temp.path().join("history.jsonl");
        let meta = crate::history::create_meta(temp.path(), "mock", None, "steer history");
        let record = crate::thread_store::SessionRecord::Meta(meta);
        std::fs::write(
            &history,
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();
        let mut writer = SessionWriter::append_to_existing(history).unwrap();
        let handle = ThreadSteerHandle::default();
        assert!(handle.push("first steer"));
        assert!(handle.push("second steer"));
        let turn = RuntimeTurnContext::new(temp.path(), "", 0, false, &SubagentType::General)
            .with_steer_handle(Some(&handle));
        let mut conversation = Conversation::new();
        let mut step = RuntimeSteerStep::new();
        let mut apply = |writer: &mut SessionWriter, conversation: &mut Conversation| {
            step.apply(RuntimeSteerInput {
                turn_context: turn.clone(),
                conversation,
                history_writer: Some(writer),
            })
        };

        assert!(apply(&mut writer, &mut conversation).is_err());
        assert!(conversation.messages.is_empty());
        assert!(handle.has_pending());

        writer.enter_turn(orca_core::thread_identity::TurnId::new());
        assert_eq!(apply(&mut writer, &mut conversation).unwrap(), 2);
        assert_eq!(writer.conversation_records().len(), 2);
        let texts = conversation
            .messages
            .iter()
            .map(|message| match message {
                orca_core::conversation::Message::User { content, .. } => content.as_str(),
                other => panic!("expected a user message, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, ["first steer", "second steer"]);
        assert!(!handle.has_pending());
    }
}
