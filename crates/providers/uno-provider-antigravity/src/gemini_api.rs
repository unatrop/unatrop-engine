use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::Request;
use http::header::{ACCEPT, CONTENT_TYPE};
use protocol::{CompletionRequest, StreamEvent};
use provider::{
    BoxStream, ModelCatalog, ModelInfo, Provider, ProviderError, ensure_success, read_checked_body,
    send_with_retry, sse_stream,
};
use serde::Deserialize;
use transport::HttpTransport;

use crate::request::gemini_request_body;
use crate::stream::AntigravityDecoder;

/// Дефолтный base URL Google Gemini Developer API (AI Studio).
pub const DEFAULT_GEMINI_API_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Провайдер Google Gemini API по API-ключу (`x-goog-api-key`) с отдельной квотой от Antigravity OAuth.
pub struct GeminiApiProvider {
    name: &'static str,
    base_url: String,
    api_key: String,
    transport: Arc<dyn HttpTransport>,
}

impl GeminiApiProvider {
    /// Создаёт провайдер Gemini Developer API. Пустой `base_url` использует [`DEFAULT_GEMINI_API_BASE_URL`].
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let base_url = base_url.into();
        let trimmed = base_url.trim().trim_end_matches('/');
        let resolved_url = if trimmed.is_empty() {
            DEFAULT_GEMINI_API_BASE_URL.to_owned()
        } else {
            trimmed.to_owned()
        };
        Self {
            name: "gemini",
            base_url: resolved_url,
            api_key: api_key.into(),
            transport,
        }
    }

    /// Запрашивает список доступных моделей через `GET /models`.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let response = send_with_retry(self.transport.as_ref(), || {
            Request::get(url.as_str())
                .header(ACCEPT, "application/json")
                .header("x-goog-api-key", self.api_key.as_str())
                .body(Bytes::new())
                .map_err(|err| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to build Gemini models request: {err}"
                    ))
                })
        })
        .await?;

        let body = read_checked_body(response).await?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct GeminiModelEntry {
            name: String,
            #[serde(default)]
            display_name: Option<String>,
        }
        #[derive(Deserialize)]
        struct GeminiModelsResponse {
            #[serde(default)]
            models: Vec<GeminiModelEntry>,
        }

        let parsed: GeminiModelsResponse = serde_json::from_str(&body).map_err(|err| {
            ProviderError::InvalidResponse(format!("Failed to parse Gemini models response: {err}"))
        })?;

        Ok(parsed
            .models
            .into_iter()
            .map(|entry| {
                let id = entry
                    .name
                    .strip_prefix("models/")
                    .unwrap_or(&entry.name)
                    .to_owned();
                let mut info = ModelInfo::new(self.name, id);
                info.display_name = entry.display_name;
                info.supports_vision = true;
                info
            })
            .collect())
    }

    /// Возвращает унифицированный [`ModelCatalog`] доступных моделей Gemini API.
    pub async fn model_catalog(&self) -> Result<ModelCatalog, ProviderError> {
        Ok(ModelCatalog::from_models(self.list_models().await?))
    }
}

#[async_trait]
impl Provider for GeminiApiProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let model = request
            .model
            .strip_prefix("models/")
            .unwrap_or(&request.model);
        let endpoint = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, model
        );
        let payload = gemini_request_body(model, &request)?;
        let body = Bytes::from(serde_json::to_vec(&payload).map_err(|err| {
            ProviderError::InvalidResponse(format!("Failed to serialize Gemini request: {err}"))
        })?);

        let response = send_with_retry(self.transport.as_ref(), || {
            Request::post(endpoint.as_str())
                .header("x-goog-api-key", self.api_key.as_str())
                .header(CONTENT_TYPE, "application/json")
                .header(ACCEPT, "text/event-stream")
                .body(body.clone())
                .map_err(|err| {
                    ProviderError::InvalidResponse(format!(
                        "Failed to build Gemini stream request: {err}"
                    ))
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
