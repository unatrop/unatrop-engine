use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use protocol::{CompletionRequest, StreamEvent};
use provider::{
    BoxStream, ModelCatalog, ModelInfo, Provider, ProviderError, ensure_success, read_checked_body,
    send_with_retry,
};
use serde::Deserialize;
use transport::HttpTransport;

use crate::request::build_messages_request_with_options;
use crate::stream::parse_anthropic_sse_stream;

pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Провайдер Anthropic Messages API (`/v1/messages`).
/// Работает с официальным `https://api.anthropic.com` и совместимыми эндпоинтами
/// (например `https://api.deepseek.com/anthropic`).
pub struct AnthropicProvider {
    name: &'static str,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
    api_key: String,
    anthropic_version: String,
    extra_headers: Vec<(String, String)>,
    prompt_cache: bool,
    adaptive_thinking: bool,
}

impl AnthropicProvider {
    /// Создаёт провайдер Anthropic для указанного `base_url` и API-ключа
    pub fn new(
        name: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            name,
            transport,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.to_owned(),
            extra_headers: Vec::new(),
            prompt_cache: true,
            adaptive_thinking: false,
        }
    }

    /// Включает или отключает режим adaptive thinking (`thinking.type = "adaptive"` + `output_config.effort`)
    pub fn adaptive_thinking(mut self, enabled: bool) -> Self {
        self.adaptive_thinking = enabled;
        self
    }

    /// Переопределяет заголовок `anthropic-version` (по умолчанию `2023-06-01`)
    pub fn anthropic_version(mut self, version: impl Into<String>) -> Self {
        self.anthropic_version = version.into();
        self
    }

    /// Добавляет статический HTTP-заголовок ко всем запросам
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Включает или отключает `cache_control: { type: "ephemeral" }` на `system`-промпте
    pub fn prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self
    }

    fn messages_url(&self) -> String {
        if self.base_url.ends_with("/v1") {
            format!("{}/messages", self.base_url)
        } else {
            format!("{}/v1/messages", self.base_url)
        }
    }

    fn models_url(&self) -> String {
        if self.base_url.ends_with("/v1") {
            format!("{}/models", self.base_url)
        } else {
            format!("{}/v1/models", self.base_url)
        }
    }

    fn apply_auth_headers(&self, headers: &mut http::HeaderMap) {
        if let Ok(val) = HeaderValue::from_str(&self.api_key) {
            headers.insert(HeaderName::from_static("x-api-key"), val);
        }
        if let Ok(val) = HeaderValue::from_str(&self.anthropic_version) {
            headers.insert(HeaderName::from_static("anthropic-version"), val);
        }
        for (k, v) in &self.extra_headers {
            if let (Ok(name), Ok(val)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                headers.insert(name, val);
            }
        }
    }

    /// Запрашивает список доступных моделей Anthropic через `GET /v1/models`
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = self.models_url();
        let response = send_with_retry(self.transport.as_ref(), || {
            let mut req = Request::builder()
                .method(http::Method::GET)
                .uri(&url)
                .body(Bytes::new())
                .map_err(|err| {
                    ProviderError::InvalidResponse(format!("build models request error: {err}"))
                })?;
            req.headers_mut()
                .insert(ACCEPT, HeaderValue::from_static("application/json"));
            self.apply_auth_headers(req.headers_mut());
            Ok(req)
        })
        .await?;

        let body = read_checked_body(response).await?;

        #[derive(Deserialize)]
        struct AnthropicModelEntry {
            id: String,
            #[serde(default)]
            display_name: Option<String>,
        }
        #[derive(Deserialize)]
        struct AnthropicModelsResponse {
            data: Vec<AnthropicModelEntry>,
        }

        let parsed: AnthropicModelsResponse = serde_json::from_str(&body).map_err(|err| {
            ProviderError::InvalidResponse(format!("Failed to parse Anthropic models: {err}"))
        })?;

        Ok(parsed
            .data
            .into_iter()
            .map(|entry| {
                let mut info = ModelInfo::new(self.name, entry.id);
                info.display_name = entry.display_name;
                info.supports_vision = true;
                info
            })
            .collect())
    }

    /// Возвращает унифицированный [`ModelCatalog`] доступных моделей Anthropic
    pub async fn model_catalog(&self) -> Result<ModelCatalog, ProviderError> {
        let models = self.list_models().await?;
        Ok(ModelCatalog::from_models(models))
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let body_json = build_messages_request_with_options(
            &request,
            self.prompt_cache,
            self.adaptive_thinking,
        );
        let body =
            Bytes::from(serde_json::to_vec(&body_json).map_err(|err| {
                ProviderError::InvalidResponse(format!("serialize error: {err}"))
            })?);
        let url = self.messages_url();

        let response = send_with_retry(self.transport.as_ref(), || {
            let mut http_req = Request::builder()
                .method(http::Method::POST)
                .uri(&url)
                .body(body.clone())
                .map_err(|err| {
                    ProviderError::InvalidResponse(format!("build request error: {err}"))
                })?;

            let headers = http_req.headers_mut();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
            self.apply_auth_headers(headers);
            Ok(http_req)
        })
        .await?;

        let response = ensure_success(response).await?;

        Ok(parse_anthropic_sse_stream(response.into_body()))
    }
}
