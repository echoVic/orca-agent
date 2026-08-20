use std::path::Path;

use orca_core::approval_types::ApprovalMode;
use orca_core::conversation::Conversation;
use orca_core::subagent_types::SubagentType;

use crate::child_agent_types::ChildAgentCheckpointSink;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;
use crate::session::bootstrap_agent_conversation_for_loop;
use crate::thread_store::SessionWriter;

pub(crate) struct RuntimeConversationBootstrapStep;

pub(crate) enum AgentConversationContext<'a> {
    Owned {
        checkpoint_observer: Option<&'a dyn ChildAgentCheckpointSink>,
    },
    Borrowed {
        conversation: &'a mut Conversation,
        history_writer: Option<&'a mut SessionWriter>,
        checkpoint_observer: Option<&'a dyn ChildAgentCheckpointSink>,
    },
}

pub(crate) struct RuntimePreparedConversation<'a> {
    conversation: RuntimePreparedConversationStorage<'a>,
    history_writer: Option<&'a mut SessionWriter>,
    checkpoint_observer: Option<&'a dyn ChildAgentCheckpointSink>,
}

enum RuntimePreparedConversationStorage<'a> {
    Borrowed(&'a mut Conversation),
    Owned(Conversation),
}

impl<'a> AgentConversationContext<'a> {
    pub(crate) fn owned() -> Self {
        Self::Owned {
            checkpoint_observer: None,
        }
    }

    pub(crate) fn borrowed(
        conversation: &'a mut Conversation,
        history_writer: Option<&'a mut SessionWriter>,
    ) -> Self {
        Self::Borrowed {
            conversation,
            history_writer,
            checkpoint_observer: None,
        }
    }

    pub(crate) fn borrowed_with_checkpoint_observer(
        conversation: &'a mut Conversation,
        checkpoint_observer: &'a dyn ChildAgentCheckpointSink,
    ) -> Self {
        Self::Borrowed {
            conversation,
            history_writer: None,
            checkpoint_observer: Some(checkpoint_observer),
        }
    }
}

impl RuntimeConversationBootstrapStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn prepare<'a>(
        &mut self,
        conversation_context: AgentConversationContext<'a>,
        cwd: &Path,
        prompt: &str,
        subagent_depth: u32,
        subagent_type: &SubagentType,
        instructions: &ProjectInstructions,
        approval_mode: ApprovalMode,
        memory: &MemoryBlock,
    ) -> RuntimePreparedConversation<'a> {
        let prepared = match conversation_context {
            AgentConversationContext::Owned {
                checkpoint_observer,
            } => RuntimePreparedConversation {
                conversation: RuntimePreparedConversationStorage::Owned(
                    bootstrap_agent_conversation_for_loop(
                        cwd,
                        prompt,
                        subagent_depth,
                        subagent_type,
                        instructions,
                        approval_mode,
                        memory,
                    ),
                ),
                history_writer: None,
                checkpoint_observer,
            },
            AgentConversationContext::Borrowed {
                conversation,
                history_writer,
                checkpoint_observer,
            } => RuntimePreparedConversation {
                conversation: RuntimePreparedConversationStorage::Borrowed(conversation),
                history_writer,
                checkpoint_observer,
            },
        };

        prepared
    }
}

impl RuntimePreparedConversation<'_> {
    #[cfg(test)]
    pub(crate) fn conversation_mut(&mut self) -> &mut Conversation {
        match &mut self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        }
    }

    #[cfg(test)]
    pub(crate) fn history_writer_mut(&mut self) -> Option<&mut SessionWriter> {
        self.history_writer.as_deref_mut()
    }

    pub(crate) fn parts_mut(&mut self) -> (&mut Conversation, Option<&mut SessionWriter>) {
        let conversation = match &mut self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        };
        (conversation, self.history_writer.as_deref_mut())
    }

    pub(crate) fn parts_mut_with_checkpoint_observer(
        &mut self,
    ) -> (
        &mut Conversation,
        Option<&mut SessionWriter>,
        Option<&dyn ChildAgentCheckpointSink>,
    ) {
        let checkpoint_observer = self.checkpoint_observer;
        let conversation = match &mut self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        };
        (
            conversation,
            self.history_writer.as_deref_mut(),
            checkpoint_observer,
        )
    }

    pub(crate) fn checkpoint_parts(
        &self,
    ) -> (&Conversation, Option<&dyn ChildAgentCheckpointSink>) {
        let conversation = match &self.conversation {
            RuntimePreparedConversationStorage::Borrowed(conversation) => conversation,
            RuntimePreparedConversationStorage::Owned(conversation) => conversation,
        };
        (conversation, self.checkpoint_observer)
    }
}
