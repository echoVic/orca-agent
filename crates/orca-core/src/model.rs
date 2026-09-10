use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::subagent_types::SubagentType;

pub const FLASH_MODEL: &str = "deepseek-flash";
pub const LEGACY_FLASH_MODEL: &str = "deepseek-v4-flash";
pub const LEGACY_VISION_MODEL: &str = "deepseek-v4-flash-vision-exp";
/// Model used for image understanding. DeepSeek-V4.1-Flash is multimodal.
pub const VISION_MODEL: &str = FLASH_MODEL;
pub const PRO_MODEL: &str = "deepseek-v4-pro";
pub const AUTO_MODEL: &str = "auto";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDefinition {
    #[serde(default)]
    pub supports_images: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelection {
    value: Option<String>,
    models: BTreeMap<String, ModelDefinition>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRouteReason {
    Explicit,
    DefaultPro,
    SubagentType,
    SubagentOverride,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRouteDecision {
    /// No image-bearing conversation content needs special handling.
    None,
    /// The selected model receives the original image blocks.
    Direct,
    /// A vision model describes images before the selected text model runs.
    DescribeThenContinue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelRouteDecision {
    pub requested_model: Option<String>,
    pub actual_model: String,
    pub reason: ModelRouteReason,
    pub image_route: ImageRouteDecision,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ModelRouteContext<'a> {
    pub subagent_type: &'a SubagentType,
    pub subagent_model: Option<&'a str>,
    pub has_images: bool,
}

impl ModelSelection {
    pub fn parse(value: Option<String>) -> Result<Self, String> {
        Self::parse_with_models(value, BTreeMap::new())
    }

    pub fn parse_with_models(
        value: Option<String>,
        models: BTreeMap<String, ModelDefinition>,
    ) -> Result<Self, String> {
        if let Some(model) = value.as_deref() {
            validate_model(model)?;
        }
        let value = value.map(|model| canonical_model_name(&model).to_string());
        let mut canonical_models = BTreeMap::new();
        for (model, definition) in models {
            validate_model(&model)?;
            let canonical = canonical_model_name(&model).to_string();
            if let Some(existing) = canonical_models.get(&canonical)
                && existing != &definition
            {
                return Err(format!(
                    "conflicting model definitions for aliases of '{canonical}'"
                ));
            }
            canonical_models.insert(canonical, definition);
        }
        Ok(Self {
            value,
            models: canonical_models,
        })
    }

    pub fn from_unchecked(value: Option<String>) -> Self {
        Self {
            value: value.map(|model| canonical_model_name(&model).to_string()),
            models: BTreeMap::new(),
        }
    }

    pub fn with_value(&self, value: Option<String>) -> Result<Self, String> {
        if let Some(model) = value.as_deref() {
            validate_model(model)?;
        }
        Ok(self.with_value_unchecked(value))
    }

    pub fn with_value_unchecked(&self, value: Option<String>) -> Self {
        Self {
            value: value.map(|model| canonical_model_name(&model).to_string()),
            models: self.models.clone(),
        }
    }

    pub fn as_option(&self) -> Option<String> {
        self.value.clone()
    }

    pub fn as_deref(&self) -> Option<&str> {
        match self.value.as_deref() {
            Some(AUTO_MODEL) | None => None,
            other => other,
        }
    }

    pub fn as_history_value(&self) -> Option<String> {
        Some(self.display_name().to_string())
    }

    pub fn display_name(&self) -> &str {
        self.value.as_deref().unwrap_or(AUTO_MODEL)
    }

    pub fn with_subagent_override(&self, model: Option<String>) -> Self {
        match model.as_deref() {
            Some(AUTO_MODEL) | None => self.clone(),
            Some(_) => self.with_value_unchecked(model),
        }
    }

    pub fn supports_images(&self, model: &str) -> bool {
        let model = canonical_model_name(model);
        self.models
            .get(model)
            .and_then(|definition| definition.supports_images)
            .or_else(|| builtin_model_definition(model).supports_images)
            .unwrap_or(false)
    }

    pub fn route(&self, context: ModelRouteContext<'_>) -> ModelRouteDecision {
        let (actual_model, reason) = if let Some(override_model) = context.subagent_model {
            (
                canonical_model_name(override_model).to_string(),
                ModelRouteReason::SubagentOverride,
            )
        } else {
            match self.value.as_deref() {
                Some(AUTO_MODEL) | None => (PRO_MODEL.to_string(), ModelRouteReason::DefaultPro),
                Some(model) => (model.to_string(), ModelRouteReason::Explicit),
            }
        };
        let image_route = if !context.has_images {
            ImageRouteDecision::None
        } else if self.supports_images(&actual_model) {
            ImageRouteDecision::Direct
        } else {
            ImageRouteDecision::DescribeThenContinue
        };
        ModelRouteDecision {
            requested_model: self.value.clone(),
            actual_model,
            reason,
            image_route,
        }
    }
}

pub fn builtin_model_definition(model: &str) -> ModelDefinition {
    let supports_images = match canonical_model_name(model) {
        FLASH_MODEL => Some(true),
        AUTO_MODEL | PRO_MODEL => Some(false),
        _ => None,
    };
    ModelDefinition { supports_images }
}

/// Model used for auxiliary/background tasks (compaction, memory extraction).
/// Always returns the cheapest model to minimize cost on utility work.
pub fn auxiliary_model() -> &'static str {
    FLASH_MODEL
}

pub fn validate_model(model: &str) -> Result<(), String> {
    if model.is_empty() {
        return Err("model must not be empty".to_string());
    }
    if model.trim() != model {
        return Err("model must not have leading or trailing whitespace".to_string());
    }
    if model.chars().any(char::is_control) {
        return Err("model must not contain control characters".to_string());
    }
    Ok(())
}

/// Resolve retired first-party names to the current API model identifier.
///
/// DeepSeek still accepts the aliases at the API boundary, but normalizing
/// here keeps persisted settings, cache identities, usage accounting, and UI
/// state on one canonical model name.
pub fn canonical_model_name(model: &str) -> &str {
    match model {
        LEGACY_FLASH_MODEL | LEGACY_VISION_MODEL => FLASH_MODEL,
        _ => model,
    }
}

/// Stable first-party presets shown by interactive model selectors.
///
/// Other provider model IDs remain valid and can be supplied through config,
/// environment variables, CLI flags, or slash commands.
pub fn preset_models() -> &'static [&'static str] {
    &[AUTO_MODEL, FLASH_MODEL, PRO_MODEL]
}

pub fn max_context_tokens(_model: Option<&str>) -> usize {
    1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ModelRouteContext<'static> {
        ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: None,
            has_images: false,
        }
    }

    #[test]
    fn auto_defaults_to_pro() {
        let selection = ModelSelection::parse(None).unwrap();
        let decision = selection.route(context());
        assert_eq!(decision.actual_model, PRO_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::DefaultPro);
    }

    #[test]
    fn explicit_flash_stays_flash() {
        let selection = ModelSelection::parse(Some(FLASH_MODEL.to_string())).unwrap();
        let decision = selection.route(context());
        assert_eq!(decision.actual_model, FLASH_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::Explicit);
    }

    #[test]
    fn explicit_pro_stays_pro() {
        let selection = ModelSelection::parse(Some(PRO_MODEL.to_string())).unwrap();
        let decision = selection.route(context());
        assert_eq!(decision.actual_model, PRO_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::Explicit);
    }

    #[test]
    fn legacy_vision_alias_routes_to_canonical_flash() {
        let selection = ModelSelection::parse(Some(LEGACY_VISION_MODEL.to_string())).unwrap();
        let decision = selection.route(context());
        assert_eq!(selection.display_name(), FLASH_MODEL);
        assert_eq!(decision.requested_model.as_deref(), Some(FLASH_MODEL));
        assert_eq!(decision.actual_model, FLASH_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::Explicit);
    }

    #[test]
    fn legacy_flash_alias_routes_to_canonical_flash() {
        let selection = ModelSelection::parse(Some(LEGACY_FLASH_MODEL.to_string())).unwrap();
        let decision = selection.route(context());
        assert_eq!(selection.display_name(), FLASH_MODEL);
        assert_eq!(decision.actual_model, FLASH_MODEL);
    }

    #[test]
    fn subagent_model_override_wins() {
        let selection = ModelSelection::parse(None).unwrap();
        let ctx = ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: Some(FLASH_MODEL),
            has_images: false,
        };
        let decision = selection.route(ctx);
        assert_eq!(decision.actual_model, FLASH_MODEL);
        assert_eq!(decision.reason, ModelRouteReason::SubagentOverride);
    }

    #[test]
    fn images_use_builtin_model_capabilities_by_default() {
        let mut ctx = context();
        ctx.has_images = true;

        let auto = ModelSelection::parse(None).unwrap().route(ctx.clone());
        assert_eq!(auto.actual_model, PRO_MODEL);
        assert_eq!(auto.image_route, ImageRouteDecision::DescribeThenContinue);

        let pro = ModelSelection::parse(Some(PRO_MODEL.to_string()))
            .unwrap()
            .route(ctx.clone());
        assert_eq!(pro.image_route, ImageRouteDecision::DescribeThenContinue);

        let flash = ModelSelection::parse(Some(FLASH_MODEL.to_string()))
            .unwrap()
            .route(ctx);
        assert_eq!(flash.image_route, ImageRouteDecision::Direct);
    }

    #[test]
    fn configured_model_capabilities_control_image_routing() {
        let custom_model = "deepseek-v4.1-flash-expires-on-0910";
        let mut models = BTreeMap::new();
        models.insert(
            custom_model.to_string(),
            ModelDefinition {
                supports_images: Some(true),
            },
        );
        models.insert(
            FLASH_MODEL.to_string(),
            ModelDefinition {
                supports_images: Some(false),
            },
        );
        let mut ctx = context();
        ctx.has_images = true;

        let custom =
            ModelSelection::parse_with_models(Some(custom_model.to_string()), models.clone())
                .unwrap()
                .route(ctx.clone());
        assert_eq!(custom.image_route, ImageRouteDecision::Direct);

        let overridden_builtin =
            ModelSelection::parse_with_models(Some(LEGACY_VISION_MODEL.to_string()), models)
                .unwrap()
                .route(ctx);
        assert_eq!(
            overridden_builtin.image_route,
            ImageRouteDecision::DescribeThenContinue
        );
    }

    #[test]
    fn changing_selection_preserves_model_definitions() {
        let custom_model = "vendor/multimodal";
        let mut models = BTreeMap::new();
        models.insert(
            custom_model.to_string(),
            ModelDefinition {
                supports_images: Some(true),
            },
        );
        let selection = ModelSelection::parse_with_models(None, models)
            .unwrap()
            .with_value(Some(custom_model.to_string()))
            .unwrap();

        assert!(selection.supports_images(custom_model));
    }

    #[test]
    fn auto_subagent_override_preserves_parent_router() {
        let selection = ModelSelection::parse(None).unwrap();
        assert_eq!(
            selection
                .with_subagent_override(Some(AUTO_MODEL.to_string()))
                .display_name(),
            AUTO_MODEL
        );
    }

    #[test]
    fn auxiliary_model_returns_flash() {
        assert_eq!(auxiliary_model(), FLASH_MODEL);
    }

    #[test]
    fn preset_models_are_stable() {
        assert_eq!(preset_models(), &[AUTO_MODEL, FLASH_MODEL, PRO_MODEL]);
        assert!(!preset_models().contains(&LEGACY_FLASH_MODEL));
        assert!(!preset_models().contains(&LEGACY_VISION_MODEL));
    }

    #[test]
    fn alias_model_definitions_are_canonicalized_and_conflicts_fail_closed() {
        let mut models = BTreeMap::new();
        models.insert(
            LEGACY_VISION_MODEL.to_string(),
            ModelDefinition {
                supports_images: Some(false),
            },
        );
        let selection =
            ModelSelection::parse_with_models(Some(LEGACY_FLASH_MODEL.to_string()), models)
                .unwrap();
        assert_eq!(selection.display_name(), FLASH_MODEL);
        assert!(!selection.supports_images(FLASH_MODEL));

        let mut conflicting = BTreeMap::new();
        conflicting.insert(
            LEGACY_FLASH_MODEL.to_string(),
            ModelDefinition {
                supports_images: Some(false),
            },
        );
        conflicting.insert(
            FLASH_MODEL.to_string(),
            ModelDefinition {
                supports_images: Some(true),
            },
        );
        assert!(
            ModelSelection::parse_with_models(None, conflicting)
                .unwrap_err()
                .contains("conflicting model definitions")
        );
    }

    #[test]
    fn explicit_custom_model_is_not_rerouted() {
        let custom_model = "deepseek-v4.1-flash-expires-on-0910";
        let selection = ModelSelection::parse(Some(custom_model.to_string())).unwrap();
        let decision = selection.route(context());
        assert_eq!(decision.actual_model, custom_model);
        assert_eq!(decision.reason, ModelRouteReason::Explicit);
    }

    #[test]
    fn model_validation_accepts_provider_ids_and_rejects_malformed_values() {
        assert!(validate_model("deepseek-reasoner").is_ok());
        assert!(validate_model("vendor/private-model:2026-09").is_ok());
        assert!(validate_model("").is_err());
        assert!(validate_model(" model").is_err());
        assert!(validate_model("model ").is_err());
        assert!(validate_model("model\nname").is_err());
    }

    #[test]
    fn deepseek_models_use_one_million_token_context_window() {
        assert_eq!(max_context_tokens(Some(FLASH_MODEL)), 1_000_000);
        assert_eq!(max_context_tokens(Some(LEGACY_FLASH_MODEL)), 1_000_000);
        assert_eq!(max_context_tokens(Some(LEGACY_VISION_MODEL)), 1_000_000);
        assert_eq!(max_context_tokens(Some(PRO_MODEL)), 1_000_000);
        assert_eq!(max_context_tokens(Some(AUTO_MODEL)), 1_000_000);
        assert_eq!(max_context_tokens(Some("vendor/private-model")), 1_000_000);
        assert_eq!(max_context_tokens(None), 1_000_000);
    }
}
