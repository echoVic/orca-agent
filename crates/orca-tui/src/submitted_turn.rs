use orca_runtime::mentions;

use crate::composer_images::{ComposerImageAttachment, ComposerImageState};
use crate::surface_actions::TuiSurfaceActions;
use crate::types::PendingWorkflowNotification;

enum SubmittedTurnKind {
    User {
        prompt: String,
        bindings: mentions::MentionBindings,
        attachments: Vec<ComposerImageAttachment>,
        resolved_images: Vec<orca_core::conversation::ImageInput>,
    },
    WorkflowNotification(PendingWorkflowNotification),
}

struct SubmittedTurnPresentation {
    task_label: Option<String>,
    backtrack_target: bool,
}

impl SubmittedTurnPresentation {
    fn user() -> Self {
        Self {
            task_label: None,
            backtrack_target: true,
        }
    }

    fn workflow_notification(id: &str) -> Self {
        Self {
            task_label: Some(workflow_notification_task_label(id)),
            backtrack_target: false,
        }
    }
}

pub(crate) struct SubmittedTurn {
    kind: SubmittedTurnKind,
    presentation: SubmittedTurnPresentation,
    queued_id: Option<u64>,
}

impl SubmittedTurn {
    pub(crate) fn user(prompt: String) -> Self {
        Self {
            kind: SubmittedTurnKind::User {
                bindings: mentions::MentionBindings::new(&prompt),
                prompt,
                attachments: Vec::new(),
                resolved_images: Vec::new(),
            },
            presentation: SubmittedTurnPresentation::user(),
            queued_id: None,
        }
    }

    pub(crate) fn user_with_mentions(
        prompt: String,
        bindings: mentions::MentionBindings,
        attachments: Vec<ComposerImageAttachment>,
    ) -> Self {
        Self {
            kind: SubmittedTurnKind::User {
                prompt,
                bindings,
                attachments,
                resolved_images: Vec::new(),
            },
            presentation: SubmittedTurnPresentation::user(),
            queued_id: None,
        }
    }

    pub(crate) fn queued_user_with_mentions(
        id: u64,
        prompt: String,
        bindings: mentions::MentionBindings,
        attachments: Vec<ComposerImageAttachment>,
    ) -> Self {
        Self {
            kind: SubmittedTurnKind::User {
                prompt,
                bindings,
                attachments,
                resolved_images: Vec::new(),
            },
            presentation: SubmittedTurnPresentation::user(),
            queued_id: Some(id),
        }
    }

    pub(crate) fn workflow_notification(notification: PendingWorkflowNotification) -> Self {
        let id = notification.id.clone();
        Self {
            kind: SubmittedTurnKind::WorkflowNotification(notification),
            presentation: SubmittedTurnPresentation::workflow_notification(&id),
            queued_id: None,
        }
    }

    pub(crate) fn queued_id(&self) -> Option<u64> {
        self.queued_id
    }

    pub(crate) fn prompt(&self) -> &str {
        match &self.kind {
            SubmittedTurnKind::User { prompt, .. } => prompt,
            SubmittedTurnKind::WorkflowNotification(notification) => &notification.prompt,
        }
    }

    pub(crate) fn rejection_prompt(&self) -> Option<&str> {
        match &self.kind {
            SubmittedTurnKind::User { prompt, .. } => Some(prompt),
            SubmittedTurnKind::WorkflowNotification(_) => None,
        }
    }

    pub(crate) fn task_label(&self) -> Option<&str> {
        self.presentation.task_label.as_deref()
    }

    pub(crate) fn images(&self) -> &[orca_core::conversation::ImageInput] {
        match &self.kind {
            SubmittedTurnKind::User {
                resolved_images, ..
            } => resolved_images,
            SubmittedTurnKind::WorkflowNotification(_) => &[],
        }
    }

    pub(crate) fn rejection_images(&self) -> Vec<ComposerImageAttachment> {
        match &self.kind {
            SubmittedTurnKind::User { attachments, .. } => attachments.clone(),
            SubmittedTurnKind::WorkflowNotification(_) => Vec::new(),
        }
    }

    pub(crate) fn rejection_bindings(&self) -> mentions::MentionBindings {
        match &self.kind {
            SubmittedTurnKind::User { bindings, .. } => bindings.clone(),
            SubmittedTurnKind::WorkflowNotification(_) => mentions::MentionBindings::default(),
        }
    }

    pub(crate) fn is_backtrack_target(&self) -> bool {
        self.presentation.backtrack_target
    }

    pub(crate) fn prompt_for_model(
        &self,
        actions: &TuiSurfaceActions,
        cwd: &std::path::Path,
        workspace_roots: &[std::path::PathBuf],
    ) -> Result<mentions::ExpandedPrompt, String> {
        match &self.kind {
            SubmittedTurnKind::User {
                prompt,
                bindings,
                attachments,
                ..
            } => {
                let (model_prompt, model_bindings) =
                    ComposerImageState::submission_text_and_bindings(prompt, attachments, bindings);
                let mut input = actions.expand_mentions(
                    &model_prompt,
                    &model_bindings,
                    cwd,
                    workspace_roots,
                )?;
                input
                    .images
                    .extend(ComposerImageState::image_inputs(attachments));
                Ok(input)
            }
            SubmittedTurnKind::WorkflowNotification(notification) => Ok(mentions::ExpandedPrompt {
                text: notification.prompt.clone(),
                images: Vec::new(),
            }),
        }
    }

    pub(crate) fn title_seed(&self, model_prompt: &str) -> String {
        match &self.kind {
            SubmittedTurnKind::User { .. } => model_prompt.to_string(),
            SubmittedTurnKind::WorkflowNotification(_) => self
                .presentation
                .task_label
                .clone()
                .unwrap_or_else(|| model_prompt.to_string()),
        }
    }

    pub(crate) fn with_model_input(mut self, input: mentions::ExpandedPrompt) -> Self {
        self.kind = match self.kind {
            SubmittedTurnKind::User { attachments, .. } => SubmittedTurnKind::User {
                bindings: mentions::MentionBindings::new(&input.text),
                prompt: input.text,
                attachments,
                resolved_images: input.images,
            },
            SubmittedTurnKind::WorkflowNotification(mut notification) => {
                notification.prompt = input.text;
                SubmittedTurnKind::WorkflowNotification(notification)
            }
        };
        self
    }
}

fn workflow_notification_task_label(id: &str) -> String {
    format!("Workflow notification {id}")
}
