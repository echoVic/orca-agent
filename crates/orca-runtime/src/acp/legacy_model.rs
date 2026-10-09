//! The session model API that ACP schema 0.11 shipped as `unstable_session_model`.
//!
//! Schema 1.x dropped it for the `model` config option. Orca still reports
//! `models` from `session/new` and `session/load` and still accepts
//! `session/set_model`, in the 0.11 wire shapes, so clients written against
//! the old API keep working.

use std::sync::Arc;

use agent_client_protocol::schema::v1::SessionId;
use serde::{Deserialize, Serialize};

type Meta = serde_json::Map<String, serde_json::Value>;

/// A unique identifier for a model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub Arc<str>);

impl std::fmt::Display for ModelId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for ModelId {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for ModelId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

/// Information about a selectable model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model_id: ModelId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

impl ModelInfo {
    pub fn new(model_id: impl Into<ModelId>, name: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            name: name.into(),
            description: None,
            meta: None,
        }
    }
}

/// The set of models and the one currently active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelState {
    pub current_model_id: ModelId,
    pub available_models: Vec<ModelInfo>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

impl SessionModelState {
    pub fn new(current_model_id: impl Into<ModelId>, available_models: Vec<ModelInfo>) -> Self {
        Self {
            current_model_id: current_model_id.into(),
            available_models,
            meta: None,
        }
    }
}

/// `session/set_model` parameters.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "session/set_model", response = SetSessionModelResponse)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelRequest {
    pub session_id: SessionId,
    pub model_id: ModelId,
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

impl SetSessionModelRequest {
    pub fn new(session_id: impl Into<SessionId>, model_id: impl Into<ModelId>) -> Self {
        Self {
            session_id: session_id.into(),
            model_id: model_id.into(),
            meta: None,
        }
    }
}

/// `session/set_model` result.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    agent_client_protocol::JsonRpcResponse,
)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelResponse {
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// A `session/new` or `session/load` result with the legacy `models` field.
#[derive(Debug, Serialize)]
pub struct WithModels<T> {
    #[serde(flatten)]
    pub response: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<SessionModelState>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_model_state_keeps_the_schema_0_11_shape() {
        let state = SessionModelState::new(
            "auto",
            vec![
                ModelInfo::new("auto", "auto"),
                ModelInfo::new("deepseek-v4-pro", "deepseek-v4-pro"),
            ],
        );
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            json!({
                "currentModelId": "auto",
                "availableModels": [
                    {"modelId": "auto", "name": "auto"},
                    {"modelId": "deepseek-v4-pro", "name": "deepseek-v4-pro"},
                ],
            })
        );
    }

    #[test]
    fn set_model_keeps_the_schema_0_11_shape() {
        let wire = json!({"sessionId": "s-1", "modelId": "deepseek-flash", "_meta": {"k": 1}});
        let request: SetSessionModelRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(request.model_id.to_string(), "deepseek-flash");
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        let missing = serde_json::from_value::<SetSessionModelRequest>(json!({"sessionId": "s-1"}))
            .unwrap_err();
        assert_eq!(missing.to_string(), "missing field `modelId`");
        assert_eq!(
            serde_json::to_value(SetSessionModelResponse::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn models_ride_beside_the_session_response() {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            session_id: &'static str,
        }
        let with = WithModels {
            response: Response { session_id: "s-1" },
            models: Some(SessionModelState::new("auto", Vec::new())),
        };
        assert_eq!(
            serde_json::to_value(&with).unwrap(),
            json!({
                "sessionId": "s-1",
                "models": {"currentModelId": "auto", "availableModels": []},
            })
        );
        let without = WithModels {
            response: Response { session_id: "s-1" },
            models: None,
        };
        assert_eq!(
            serde_json::to_value(&without).unwrap(),
            json!({"sessionId": "s-1"})
        );
    }
}
