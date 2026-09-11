//! ACP settings use the existing typed mutation path, bounded by daemon policy.

use std::collections::BTreeSet;

use agent_client_protocol::{
    Error, ModelInfo, SessionConfigOption, SessionConfigSelectOption, SessionMode,
    SessionModeState, SessionModelState,
};
use orca_core::approval_types::ApprovalMode;

use crate::surface::{
    AttachResult, DetachRequest, FreshAttachRequest, MutationReply, NonEmptyVec,
    RuntimeSettingsPatch, RuntimeSurfaceHandle, SurfaceApprovalMode, SurfaceAttachmentRole,
    SurfaceCapability, SurfaceReasoningEffort, SurfaceRequestId, SurfaceRuntimeSettings,
};

pub(super) fn mode_name(mode: SurfaceApprovalMode) -> &'static str {
    match mode {
        SurfaceApprovalMode::Plan => "plan",
        SurfaceApprovalMode::Suggest => "suggest",
        SurfaceApprovalMode::AutoEdit => "auto-edit",
        SurfaceApprovalMode::FullAuto => "full-auto",
    }
}

pub(super) fn allowed_modes(ceiling: ApprovalMode) -> Vec<&'static str> {
    match ceiling {
        ApprovalMode::Plan => vec!["plan"],
        ApprovalMode::Suggest => vec!["plan", "suggest"],
        ApprovalMode::AutoEdit => vec!["plan", "suggest", "auto-edit"],
        ApprovalMode::FullAuto => vec!["plan", "suggest", "auto-edit", "full-auto"],
    }
}

pub(super) fn mode_patch(mode: &str, ceiling: ApprovalMode) -> Result<RuntimeSettingsPatch, Error> {
    if !allowed_modes(ceiling).contains(&mode) {
        return Err(Error::invalid_params().data("mode exceeds daemon policy or is unknown"));
    }
    Ok(match mode {
        "plan" => RuntimeSettingsPatch::SetApprovalMode {
            mode: SurfaceApprovalMode::Plan,
        },
        "suggest" => RuntimeSettingsPatch::SetApprovalMode {
            mode: SurfaceApprovalMode::Suggest,
        },
        "auto-edit" => RuntimeSettingsPatch::SetApprovalMode {
            mode: SurfaceApprovalMode::AutoEdit,
        },
        "full-auto" => RuntimeSettingsPatch::EnableFullAccess,
        _ => unreachable!(),
    })
}

pub(super) fn read(surface: &RuntimeSurfaceHandle) -> Result<SurfaceRuntimeSettings, Error> {
    change(surface, None)
}

pub(super) fn change(
    surface: &RuntimeSurfaceHandle,
    patch: Option<RuntimeSettingsPatch>,
) -> Result<SurfaceRuntimeSettings, Error> {
    let mut capabilities = BTreeSet::from([SurfaceCapability::ReadSnapshot]);
    if patch.is_some() {
        capabilities.insert(SurfaceCapability::ManageThreadSettings);
    }
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Acp,
        requested_capabilities: capabilities,
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => return Err(Error::invalid_request().data("ACP settings attachment unavailable")),
    };
    let result = if let Some(patch) = patch {
        let result = attachment.client.update_settings(
            SurfaceRequestId::new(),
            attachment.baseline.snapshot.settings.thread_revision,
            NonEmptyVec::try_new(vec![patch]).expect("one settings patch"),
        );
        match result {
            Ok(MutationReply::Committed { .. }) => Ok(()),
            _ => Err(Error::invalid_request().data("ACP settings mutation did not commit")),
        }
    } else {
        Ok(())
    };
    let settings = attachment.baseline.snapshot.settings.effective.clone();
    let _ = surface.detach(
        &attachment.client,
        DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
    result.map(|()| settings)
}

pub(super) fn models(settings: &SurfaceRuntimeSettings) -> SessionModelState {
    let mut models = orca_core::model::preset_models()
        .iter()
        .map(|model| model.to_string())
        .collect::<Vec<_>>();
    if !models.iter().any(|model| model == settings.model.as_str()) {
        models.push(settings.model.as_str().into());
    }
    SessionModelState::new(
        settings.model.as_str().to_string(),
        models
            .into_iter()
            .map(|model| ModelInfo::new(model.clone(), model))
            .collect(),
    )
}

pub(super) fn modes(settings: &SurfaceRuntimeSettings, ceiling: ApprovalMode) -> SessionModeState {
    SessionModeState::new(
        mode_name(settings.approval_mode),
        allowed_modes(ceiling)
            .into_iter()
            .map(|mode| SessionMode::new(mode, mode))
            .collect(),
    )
}

pub(super) fn options(
    settings: &SurfaceRuntimeSettings,
    ceiling: ApprovalMode,
) -> Vec<SessionConfigOption> {
    let models = models(settings)
        .available_models
        .into_iter()
        .map(|model| SessionConfigSelectOption::new(model.model_id.to_string(), model.name))
        .collect::<Vec<_>>();
    let modes = allowed_modes(ceiling)
        .into_iter()
        .map(|mode| SessionConfigSelectOption::new(mode, mode))
        .collect::<Vec<_>>();
    let reasoning = ["low", "high", "max"]
        .into_iter()
        .map(|value| SessionConfigSelectOption::new(value, value))
        .collect::<Vec<_>>();
    vec![
        SessionConfigOption::select(
            "model",
            "Model",
            settings.model.as_str().to_string(),
            models,
        ),
        SessionConfigOption::select(
            "mode",
            "Approval mode",
            mode_name(settings.approval_mode),
            modes,
        ),
        SessionConfigOption::select(
            "reasoning",
            "Reasoning effort",
            match settings.reasoning_effort {
                SurfaceReasoningEffort::Low => "low",
                SurfaceReasoningEffort::Medium => "medium",
                SurfaceReasoningEffort::High => "high",
                SurfaceReasoningEffort::Max => "max",
            },
            reasoning,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::mode_patch;
    use crate::surface::RuntimeSettingsPatch;
    use orca_core::approval_types::ApprovalMode;

    #[test]
    fn acp_full_auto_uses_the_explicit_full_access_patch() {
        assert!(matches!(
            mode_patch("full-auto", ApprovalMode::FullAuto).expect("allowed mode"),
            RuntimeSettingsPatch::EnableFullAccess
        ));
        assert!(mode_patch("full-auto", ApprovalMode::AutoEdit).is_err());
    }
}
