use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use protocol::{CompletionRequest, Message, Role, StreamEvent};
use provider::{
    BoxStream, ModelCatalog, ModelInfo, Provider, ProviderError, read_checked_body, send_with_retry,
};
use provider_openai::{stream_chat_completions, stream_responses};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use transport::HttpTransport;

use crate::auth::CopilotAuth;

const DEFAULT_INTEGRATION_ID: &str = "vscode-chat";
const DEFAULT_EDITOR_VERSION: &str = crate::COPILOT_EDITOR_VERSION;
const DEFAULT_PLUGIN_VERSION: &str = crate::COPILOT_PLUGIN_VERSION;
const DEFAULT_USER_AGENT: &str = crate::COPILOT_USER_AGENT;

/// Политика доступности модели Copilot
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopilotModelPolicy {
    /// Состояние политики (`enabled`, `disabled` и т.п.)
    #[serde(default)]
    pub state: Option<String>,
}

/// Возможности модели Copilot
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopilotModelCapabilities {
    /// Тип модели (`chat`, `embeddings`, `decisions` и т.п.)
    #[serde(default, rename = "type")]
    pub model_type: Option<String>,
}

/// Описание модели, возвращаемое эндпоинтом `/models` GitHub Copilot
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopilotModel {
    /// Идентификатор модели для `CompletionRequest::model`
    pub id: String,
    /// Человекочитаемое имя модели
    #[serde(default)]
    pub name: Option<String>,
    /// Поставщик модели (например, `openai`, `anthropic`)
    #[serde(default)]
    pub vendor: Option<String>,
    /// Доступна ли модель в UI-пикере Copilot
    #[serde(default)]
    pub model_picker_enabled: bool,
    /// Политика доступности модели на текущем аккаунте
    #[serde(default)]
    pub policy: Option<CopilotModelPolicy>,
    /// Возможности модели
    #[serde(default)]
    pub capabilities: Option<CopilotModelCapabilities>,
    /// Поддерживаемые HTTP-эндпоинты модели
    #[serde(default)]
    pub supported_endpoints: Option<Vec<String>>,
}

/// HTTP-эндпоинт инференса для модели Copilot
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopilotEndpoint {
    ChatCompletions,
    Responses,
}

impl CopilotModel {
    /// Возвращает предпочтительный эндпоинт модели, если она доступна для чата
    pub fn preferred_endpoint(&self) -> Option<CopilotEndpoint> {
        if let Some(state) = self.policy.as_ref().and_then(|p| p.state.as_deref())
            && state != "enabled"
        {
            return None;
        }
        if let Some(model_type) = self
            .capabilities
            .as_ref()
            .and_then(|c| c.model_type.as_deref())
            && model_type != "chat"
        {
            return None;
        }
        match self.supported_endpoints.as_deref() {
            Some(endpoints) if !endpoints.is_empty() => {
                if endpoints.iter().any(|ep| ep == "/chat/completions") {
                    Some(CopilotEndpoint::ChatCompletions)
                } else if endpoints.iter().any(|ep| ep == "/responses") {
                    Some(CopilotEndpoint::Responses)
                } else {
                    None
                }
            }
            _ => Some(CopilotEndpoint::ChatCompletions),
        }
    }

    /// Возвращает `true`, если модель не отключена политикой и поддерживает `/chat/completions`
    pub fn is_chat_completions_available(&self) -> bool {
        self.preferred_endpoint() == Some(CopilotEndpoint::ChatCompletions)
    }
}

/// Провайдер моделей GitHub Copilot поверх [`Provider`]
pub struct CopilotProvider {
    name: &'static str,
    auth: CopilotAuth,
    transport: Arc<dyn HttpTransport>,
    editor_version: String,
    integration_id: String,
    model_endpoints: Arc<RwLock<HashMap<String, CopilotEndpoint>>>,
}

impl CopilotProvider {
    /// Создаёт провайдер Copilot с указанной авторизацией и транспортом
    pub fn new(auth: CopilotAuth, transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            name: "copilot",
            auth,
            transport,
            editor_version: DEFAULT_EDITOR_VERSION.to_string(),
            integration_id: DEFAULT_INTEGRATION_ID.to_string(),
            model_endpoints: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Переопределяет имя провайдера
    pub fn name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }

    /// Задаёт заголовок `Editor-Version`
    pub fn editor_version(mut self, value: impl Into<String>) -> Self {
        self.editor_version = value.into();
        self
    }

    /// Задаёт заголовок `Copilot-Integration-Id`
    pub fn integration_id(mut self, value: impl Into<String>) -> Self {
        self.integration_id = value.into();
        self
    }

    /// Возвращает используемый менеджер авторизации
    pub fn auth(&self) -> &CopilotAuth {
        &self.auth
    }

    /// Запрашивает список доступных моделей Copilot через `GET /models`
    pub async fn list_models(&self) -> Result<Vec<CopilotModel>, ProviderError> {
        let access = self.auth.access().await?;
        let url = format!("{}/models", access.api_url.trim_end_matches('/'));
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::get(url.as_str())
                .header(AUTHORIZATION, format!("Bearer {}", access.token))
                .header(ACCEPT, "application/json")
                .header(USER_AGENT, DEFAULT_USER_AGENT)
                .header("Editor-Version", self.editor_version.as_str())
                .header("Editor-Plugin-Version", DEFAULT_PLUGIN_VERSION)
                .header("Copilot-Integration-Id", self.integration_id.as_str())
                .header("Openai-Intent", "conversation-edits")
                .body(Bytes::new())
                .map_err(|err| {
                    ProviderError::InvalidResponse(format!("Failed to build models request: {err}"))
                })
        })
        .await?;
        let body = read_checked_body(response).await?;

        let models = if let Ok(wrapped) = serde_json::from_str::<ModelsEnvelope>(&body) {
            wrapped.data
        } else {
            serde_json::from_str::<Vec<CopilotModel>>(&body).map_err(|err| {
                ProviderError::InvalidResponse(format!(
                    "Failed to parse Copilot models response: {err}"
                ))
            })?
        };
        let available: Vec<CopilotModel> = models
            .into_iter()
            .filter(|model| model.preferred_endpoint().is_some())
            .collect();
        let mut cache = self.model_endpoints.write().await;
        cache.clear();
        for model in &available {
            if let Some(endpoint) = model.preferred_endpoint() {
                cache.insert(model.id.clone(), endpoint);
            }
        }
        Ok(available)
    }

    /// Возвращает унифицированный [`ModelCatalog`] доступных моделей Copilot
    pub async fn model_catalog(&self) -> Result<ModelCatalog, ProviderError> {
        let models = self.list_models().await?;
        Ok(ModelCatalog::from_models(
            models.iter().map(ModelInfo::from).collect(),
        ))
    }

    async fn resolve_endpoint(&self, model_id: &str) -> CopilotEndpoint {
        if let Some(&endpoint) = self.model_endpoints.read().await.get(model_id) {
            return endpoint;
        }
        if model_id.starts_with("gpt-5")
            || model_id.starts_with("gpt-6")
            || model_id.starts_with("mai-code")
        {
            CopilotEndpoint::Responses
        } else {
            CopilotEndpoint::ChatCompletions
        }
    }
}

#[derive(Deserialize)]
struct ModelsEnvelope {
    data: Vec<CopilotModel>,
}

impl From<&CopilotModel> for ModelInfo {
    fn from(model: &CopilotModel) -> Self {
        let mut info = ModelInfo::new("copilot", model.id.clone());
        info.display_name = model.name.clone();
        info.recommended = model.model_picker_enabled;
        info
    }
}

fn initiator_for(messages: &[Message]) -> &'static str {
    match messages.last().map(|msg| msg.role) {
        Some(Role::Assistant | Role::Tool) => "agent",
        _ => "user",
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let endpoint = self.resolve_endpoint(&request.model).await;
        let access = self.auth.access().await?;
        let initiator = initiator_for(&request.messages);
        let headers = [
            ("User-Agent", crate::COPILOT_USER_AGENT),
            ("Editor-Version", self.editor_version.as_str()),
            ("Editor-Plugin-Version", crate::COPILOT_PLUGIN_VERSION),
            ("Copilot-Integration-Id", self.integration_id.as_str()),
            ("X-Initiator", initiator),
            ("Openai-Intent", "conversation-panel"),
        ];

        match endpoint {
            CopilotEndpoint::ChatCompletions => {
                stream_chat_completions(
                    self.transport.as_ref(),
                    &access.api_url,
                    &access.token,
                    &headers,
                    &request,
                )
                .await
            }
            CopilotEndpoint::Responses => {
                stream_responses(
                    self.transport.as_ref(),
                    &access.api_url,
                    &access.token,
                    &headers,
                    &request,
                )
                .await
            }
        }
    }
}
