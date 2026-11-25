//! Провайдер OpenCode — OpenAI-совместимый API (`/chat/completions`, `GET /models`).
//!
//! Обёртка над [`stream_chat_completions_with_cache`] с именем провайдера `"opencode"`
//! и дефолтным base URL OpenCode.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use protocol::{CompletionRequest, StreamEvent};
use provider::{
    BoxStream, ModelCatalog, ModelInfo, Provider, ProviderError, read_checked_body, send_with_retry,
};
use provider_openai::{
    openai_request_body_with_cache, send_openai_stream, stream_chat_completions_with_cache,
};
use serde::Deserialize;
use transport::HttpTransport;

/// Дефолтный base URL OpenCode API
pub const DEFAULT_OPENCODE_BASE_URL: &str = "https://opencode.ai/inference/openai/v1";

/// Провайдер OpenCode поверх OpenAI-совместимого `/chat/completions`
pub struct OpenCodeProvider {
    name: &'static str,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
    api_key: String,
    extra_headers: Vec<(String, String)>,
    prompt_cache: Option<bool>,
    /// Если `true` — ключ не требуется (бесплатный локальный уровень OpenCode)
    no_auth: bool,
}

impl OpenCodeProvider {
    /// Создаёт провайдер OpenCode с указанным API-ключом и транспортом.
    ///
    /// `base_url` может быть пустым — тогда используется [`DEFAULT_OPENCODE_BASE_URL`].
    pub fn new(api_key: impl Into<String>, transport: Arc<dyn HttpTransport>) -> Self {
        Self::with_base_url(DEFAULT_OPENCODE_BASE_URL, api_key, transport)
    }

    /// Создаёт провайдер OpenCode с кастомным `base_url`.
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let base_url = base_url.into();
        let base_url = if base_url.is_empty() {
            DEFAULT_OPENCODE_BASE_URL.to_string()
        } else {
            base_url
        };
        Self {
            name: "opencode",
            transport,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            extra_headers: Vec::new(),
            prompt_cache: None,
            no_auth: false,
        }
    }

    /// Создаёт провайдер для бесплатного локального уровня OpenCode без API-ключа
    pub fn free(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            name: "opencode",
            transport,
            base_url: DEFAULT_OPENCODE_BASE_URL.to_string(),
            api_key: String::new(),
            extra_headers: Vec::new(),
            prompt_cache: None,
            no_auth: true,
        }
    }

    /// Добавляет статический HTTP-заголовок ко всем запросам
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Явно включает или отключает инъекцию `cache_control`
    pub fn prompt_cache(mut self, enabled: Option<bool>) -> Self {
        self.prompt_cache = enabled;
        self
    }

    /// Запрашивает список моделей через `GET /models`
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let response = send_with_retry(self.transport.as_ref(), || {
            let mut builder = Request::get(url.as_str()).header(ACCEPT, "application/json");
            if !self.no_auth {
                builder = builder.header(AUTHORIZATION, format!("Bearer {}", self.api_key));
            }
            for (k, v) in &self.extra_headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            builder.body(Bytes::new()).map_err(|err| {
                ProviderError::InvalidResponse(format!("Failed to build models request: {err}"))
            })
        })
        .await?;
        let body = read_checked_body(response).await?;

        #[derive(Deserialize)]
        struct ModelEntry {
            id: String,
        }
        #[derive(Deserialize)]
        struct ModelsList {
            data: Vec<ModelEntry>,
        }

        let entries: Vec<ModelEntry> = if let Ok(wrapped) =
            serde_json::from_str::<ModelsList>(&body)
        {
            wrapped.data
        } else {
            serde_json::from_str::<Vec<ModelEntry>>(&body).map_err(|err| {
                ProviderError::InvalidResponse(format!("Failed to parse models response: {err}"))
            })?
        };

        Ok(entries
            .into_iter()
            .map(|entry| ModelInfo::new(self.name, entry.id))
            .collect())
    }

    /// Возвращает унифицированный [`ModelCatalog`] доступных моделей
    pub async fn model_catalog(&self) -> Result<ModelCatalog, ProviderError> {
        let models = self.list_models().await?;
        Ok(ModelCatalog::from_models(models))
    }
}

#[async_trait]
impl Provider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let headers: Vec<(&str, &str)> = self
            .extra_headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();

        if self.no_auth {
            // Без ключа не отправляем `Authorization` вовсе — пустой Bearer даёт 401.
            let endpoint = format!("{}/chat/completions", self.base_url);
            let payload = openai_request_body_with_cache(&request, self.prompt_cache)?;
            let body = serde_json::to_vec(&payload).map_err(|error| {
                ProviderError::InvalidResponse(format!("Failed to serialize request: {error}"))
            })?;
            let mut builder = Request::post(endpoint).header(CONTENT_TYPE, "application/json");
            for (name, value) in &headers {
                builder = builder.header(*name, *value);
            }
            let http_request = builder.body(Bytes::from(body)).map_err(|error| {
                ProviderError::InvalidResponse(format!("Failed to build HTTP request: {error}"))
            })?;
            return send_openai_stream(self.transport.as_ref(), http_request).await;
        }

        stream_chat_completions_with_cache(
            self.transport.as_ref(),
            &self.base_url,
            &self.api_key,
            &headers,
            &request,
            self.prompt_cache,
        )
        .await
    }
}
