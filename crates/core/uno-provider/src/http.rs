use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{Request, Response};
use transport::{ByteStream, HttpTransport};

use crate::error::ProviderError;

/// Единый лимит тела HTTP-ошибки для всех провайдеров (4 КиБ)
pub const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

/// Единый лимит тела служебного HTTP-ответа (токены, каталоги моделей — 8 МиБ)
pub const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Суффикс, добавляемый к телу ошибки при превышении [`MAX_ERROR_BODY_BYTES`]
pub const ERROR_BODY_TRUNCATED_SUFFIX: &str = "\n[error body truncated]";

/// Политика повторных попыток HTTP-запросов к провайдерам
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Задержки перед повторными попытками в миллисекундах
    pub delays_ms: Vec<u64>,
    /// Повторять ли неуспешные HTTP-ответы, если их [`crate::ProviderErrorKind::is_retryable`] равен `true`
    pub retry_http_status: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            delays_ms: vec![250, 750],
            retry_http_status: false,
        }
    }
}

impl RetryPolicy {
    /// Создаёт политику с указанными задержками (в миллисекундах)
    pub fn new(delays_ms: impl Into<Vec<u64>>) -> Self {
        Self {
            delays_ms: delays_ms.into(),
            retry_http_status: false,
        }
    }

    /// Включает или выключает автоматический повтор при HTTP-ответах с retryable-типом (`RateLimit`, `Overloaded`, `Server`)
    pub fn with_http_status_retry(mut self, enabled: bool) -> Self {
        self.retry_http_status = enabled;
        self
    }

    /// Проверяет по классификации [`ProviderError::is_retryable`], следует ли повторить запрос
    pub fn should_retry(&self, error: &ProviderError) -> bool {
        match error {
            ProviderError::Transport(_) => error.is_retryable(),
            ProviderError::HttpStatus { .. } => self.retry_http_status && error.is_retryable(),
            _ => error.is_retryable(),
        }
    }
}

/// Отправляет HTTP-запрос с политикой повтора [`RetryPolicy::default`]
pub async fn send_with_retry(
    transport: &dyn HttpTransport,
    make_request: impl FnMut() -> Result<Request<Bytes>, ProviderError>,
) -> Result<Response<ByteStream>, ProviderError> {
    send_with_retry_policy(transport, &RetryPolicy::default(), make_request).await
}

/// Отправляет HTTP-запрос с заданной [`RetryPolicy`]
pub async fn send_with_retry_policy(
    transport: &dyn HttpTransport,
    policy: &RetryPolicy,
    mut make_request: impl FnMut() -> Result<Request<Bytes>, ProviderError>,
) -> Result<Response<ByteStream>, ProviderError> {
    for &delay_ms in &policy.delays_ms {
        let request = make_request()?;
        match transport.send(request).await {
            Ok(response) if response.status().is_success() || !policy.retry_http_status => {
                return Ok(response);
            }
            Ok(response) => {
                let status = response.status().as_u16();
                let body = read_error_body(response.into_body()).await?;
                let err = ProviderError::HttpStatus { status, body };
                if policy.should_retry(&err) {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                } else {
                    return Err(err);
                }
            }
            Err(transport_err) => {
                let err = ProviderError::Transport(transport_err);
                if policy.should_retry(&err) {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                } else {
                    return Err(err);
                }
            }
        }
    }

    let request = make_request()?;
    transport.send(request).await.map_err(Into::into)
}

/// Читает тело HTTP-ошибки с ограничением [`MAX_ERROR_BODY_BYTES`] и пометкой об усечении
pub async fn read_error_body(mut body_stream: ByteStream) -> Result<String, ProviderError> {
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk?;
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        let copy = chunk.len().min(remaining);
        body.extend_from_slice(&chunk[..copy]);
        if copy < chunk.len() {
            truncated = true;
            break;
        }
    }
    let mut text = protocol::redact_secrets(&String::from_utf8_lossy(&body));
    if truncated {
        text.push_str(ERROR_BODY_TRUNCATED_SUFFIX);
    }
    Ok(text)
}

/// Читает тело служебного HTTP-ответа (JSON токена, списка моделей) с лимитом [`MAX_RESPONSE_BODY_BYTES`]
pub async fn read_response_body(mut body_stream: ByteStream) -> Result<String, ProviderError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(ProviderError::InvalidResponse(format!(
                "Response body exceeded {MAX_RESPONSE_BODY_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Проверяет статус ответа и при ошибке читает тело через [`read_error_body`]
pub async fn ensure_success(
    response: Response<ByteStream>,
) -> Result<Response<ByteStream>, ProviderError> {
    let status = response.status();
    if !status.is_success() {
        let body = read_error_body(response.into_body()).await?;
        return Err(ProviderError::HttpStatus {
            status: status.as_u16(),
            body,
        });
    }
    Ok(response)
}

/// Проверяет статус ответа: при ошибке читает тело с лимитом [`MAX_ERROR_BODY_BYTES`], при успехе — с лимитом [`MAX_RESPONSE_BODY_BYTES`]
pub async fn read_checked_body(response: Response<ByteStream>) -> Result<String, ProviderError> {
    let response = ensure_success(response).await?;
    read_response_body(response.into_body()).await
}
