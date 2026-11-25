use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use protocol::{CompletionRequest, StreamEvent};
use provider::{
    BoxStream, ModelCatalog, ModelInfo, Provider, ProviderError, ensure_success, read_checked_body,
    send_with_retry, sse_stream,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;
use transport::HttpTransport;

use crate::auth::AntigravityAuth;

mod request;
mod stream;

pub use request::antigravity_request_body;
use stream::AntigravityDecoder;

const DEFAULT_MODELS_CACHE_TTL: Duration = Duration::from_secs(300);
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityQuota {
    #[serde(default)]
    pub remaining_fraction: Option<f64>,
    #[serde(default)]
    pub reset_time: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityAccount {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub tier_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityModel {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub supports_thinking: Option<bool>,
    #[serde(default)]
    pub thinking_level: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub recommended: Option<bool>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_provider: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub supports_images: Option<bool>,
    #[serde(default)]
    pub supports_video: Option<bool>,
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub is_internal: Option<bool>,
    #[serde(default)]
    pub quota_info: Option<AntigravityQuota>,
}

impl AntigravityModel {
    /// Whether the model can currently serve agent requests.
    pub fn is_usable(&self) -> bool {
        if self.disabled == Some(true) || self.is_internal == Some(true) {
            return false;
        }
        if self.id.starts_with("chat_") || self.id.ends_with("-disabled") {
            return false;
        }
        match &self.quota_info {
            Some(quota) => match quota.remaining_fraction {
                Some(fraction) => fraction > 0.0,
                None => quota.reset_time.is_none(),
            },
            None => true,
        }
    }
}

pub struct AntigravityProvider {
    name: &'static str,
    auth: AntigravityAuth,
    transport: Arc<dyn HttpTransport>,
    api_url: String,
    project: String,
    user_agent: String,
    models_cache_ttl: Duration,
    models_cache: RwLock<Option<(Instant, Vec<AntigravityModel>)>>,
}

impl AntigravityProvider {
    pub fn new(auth: AntigravityAuth, transport: Arc<dyn HttpTransport>) -> Self {
        let api_url = auth.api_url().to_string();
        let project = auth.project().to_string();
        Self {
            name: "antigravity",
            auth,
            transport,
            api_url,
            project,
            user_agent: crate::DEFAULT_USER_AGENT.to_string(),
            models_cache_ttl: DEFAULT_MODELS_CACHE_TTL,
            models_cache: RwLock::new(None),
        }
    }

    pub fn with_models_cache_ttl(mut self, ttl: Duration) -> Self {
        self.models_cache_ttl = ttl;
        self
    }

    pub fn name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }

    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = api_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = project.into();
        self
    }

    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    pub fn auth(&self) -> &AntigravityAuth {
        &self.auth
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub async fn list_models(&self) -> Result<Vec<AntigravityModel>, ProviderError> {
        if !self.models_cache_ttl.is_zero()
            && let Some((cached_at, models)) = self.models_cache.read().await.as_ref()
            && cached_at.elapsed() < self.models_cache_ttl
        {
            return Ok(models.clone());
        }

        let access = self.auth.access().await?;
        let url = format!(
            "{}/v1internal:fetchAvailableModels",
            self.api_url.trim_end_matches('/')
        );
        let body = Bytes::from(
            serde_json::to_vec(&json!({ "project": self.project })).map_err(|error| {
                ProviderError::InvalidResponse(format!(
                    "Failed to serialize models request: {error}"
                ))
            })?,
        );
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::post(url.as_str())
                .header(AUTHORIZATION, format!("Bearer {}", access.token))
                .header(CONTENT_TYPE, "application/json")
                .header(USER_AGENT, self.user_agent.as_str())
                .body(body.clone())
                .map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to build models request: {error}"
                    ))
                })
        })
        .await?;

        let text = read_checked_body(response).await?;

        let envelope: ModelsEnvelope = serde_json::from_str(&text).map_err(|error| {
            ProviderError::InvalidResponse(format!("Failed to parse models response: {error}"))
        })?;
        let models = match envelope.models {
            ModelsField::Map(models) => models
                .into_iter()
                .map(|(id, mut model)| {
                    model.id = id;
                    model
                })
                .collect(),
            ModelsField::List(models) => models,
            ModelsField::Empty => Vec::new(),
        };
        let models: Vec<AntigravityModel> = models
            .into_iter()
            .filter(AntigravityModel::is_usable)
            .collect();

        if !self.models_cache_ttl.is_zero() {
            *self.models_cache.write().await = Some((Instant::now(), models.clone()));
        }
        Ok(models)
    }

    /// Запрашивает email аккаунта и текущий тариф Cloud Code.
    pub async fn fetch_account(&self) -> Result<AntigravityAccount, ProviderError> {
        let access = self.auth.access().await?;
        let email = self.fetch_email(&access.token).await.ok().flatten();
        let (plan, tier_id) = self.fetch_tier(&access.token).await?;
        Ok(AntigravityAccount {
            email,
            plan,
            tier_id,
        })
    }

    async fn fetch_email(&self, token: &str) -> Result<Option<String>, ProviderError> {
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::get(USERINFO_URL)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(ACCEPT, "application/json")
                .header(USER_AGENT, self.user_agent.as_str())
                .body(Bytes::new())
                .map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to build userinfo request: {error}"
                    ))
                })
        })
        .await?;
        let text = read_checked_body(response).await?;

        #[derive(Deserialize)]
        struct UserInfo {
            #[serde(default)]
            email: Option<String>,
        }
        let parsed: UserInfo = serde_json::from_str(&text).map_err(|error| {
            ProviderError::InvalidResponse(format!("Failed to parse userinfo response: {error}"))
        })?;
        Ok(parsed.email)
    }

    async fn fetch_tier(
        &self,
        token: &str,
    ) -> Result<(Option<String>, Option<String>), ProviderError> {
        let url = format!(
            "{}/v1internal:loadCodeAssist",
            self.api_url.trim_end_matches('/')
        );
        let body = Bytes::from(
            serde_json::to_vec(&json!({ "metadata": { "ideType": "ANTIGRAVITY" } })).map_err(
                |error| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to serialize account request: {error}"
                    ))
                },
            )?,
        );
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::post(url.as_str())
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(CONTENT_TYPE, "application/json")
                .header(USER_AGENT, self.user_agent.as_str())
                .body(body.clone())
                .map_err(|error| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to build account request: {error}"
                    ))
                })
        })
        .await?;
        let text = read_checked_body(response).await?;

        #[derive(Deserialize)]
        struct Tier {
            #[serde(default)]
            id: Option<String>,
            #[serde(default)]
            name: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct LoadCodeAssist {
            #[serde(default)]
            current_tier: Option<Tier>,
            #[serde(default)]
            paid_tier: Option<Tier>,
        }
        let parsed: LoadCodeAssist = serde_json::from_str(&text).map_err(|error| {
            ProviderError::InvalidResponse(format!("Failed to parse account response: {error}"))
        })?;
        let plan = parsed
            .paid_tier
            .as_ref()
            .and_then(|tier| tier.name.clone())
            .or_else(|| {
                parsed
                    .current_tier
                    .as_ref()
                    .and_then(|tier| tier.name.clone())
            });
        let tier_id = parsed
            .paid_tier
            .as_ref()
            .and_then(|tier| tier.id.clone())
            .or_else(|| {
                parsed
                    .current_tier
                    .as_ref()
                    .and_then(|tier| tier.id.clone())
            });
        Ok((plan, tier_id))
    }

    pub async fn model_catalog(&self) -> Result<ModelCatalog, ProviderError> {
        let models = self.list_models().await?;
        Ok(ModelCatalog::from_models(
            models.iter().map(ModelInfo::from).collect(),
        ))
    }

    pub async fn stream_generate(
        &self,
        request: &CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let access = self.auth.access().await?;
        let payload = antigravity_request_body(&self.project, &request.model, request)?;
        let body = Bytes::from(serde_json::to_vec(&payload).map_err(|error| {
            ProviderError::InvalidResponse(format!("Failed to serialize request: {error}"))
        })?);
        let url = format!(
            "{}/v1internal:streamGenerateContent?alt=sse",
            self.api_url.trim_end_matches('/')
        );
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::post(url.as_str())
                .header(AUTHORIZATION, format!("Bearer {}", access.token))
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "text/event-stream")
                .header(USER_AGENT, self.user_agent.as_str())
                .body(body.clone())
                .map_err(|error| {
                    ProviderError::InvalidResponse(format!("Failed to build request: {error}"))
                })
        })
        .await?;

        let response = ensure_success(response).await?;
        Ok(sse_stream(
            response.into_body(),
            AntigravityDecoder::default(),
        ))
    }
}

#[async_trait]
impl Provider for AntigravityProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.stream_generate(&request).await
    }
}

#[derive(Deserialize)]
struct ModelsEnvelope {
    #[serde(default)]
    models: ModelsField,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum ModelsField {
    #[default]
    Empty,
    Map(HashMap<String, AntigravityModel>),
    List(Vec<AntigravityModel>),
}

impl From<&AntigravityModel> for ModelInfo {
    fn from(model: &AntigravityModel) -> Self {
        let mut info = ModelInfo::new("antigravity", model.id.clone());
        info.display_name = model.display_name.clone();
        info.max_input_tokens = model.max_tokens;
        info.max_output_tokens = model.max_output_tokens;
        if let Some(thinking) = model.supports_thinking {
            info.supports_thinking = thinking;
        }
        info.supports_vision = model.supports_images.unwrap_or(false);
        info.recommended = model.recommended.unwrap_or(false);
        info
    }
}
