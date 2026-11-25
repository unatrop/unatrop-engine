//! Контракт провайдера LLM, общий HTTP/retry-слой, классификация ошибок и каталог моделей

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use protocol::{CompletionRequest, StreamEvent};

pub mod catalog;
mod error;
pub mod http;
pub mod sse;

pub use catalog::{ContextProfile, ModelCatalog, ModelInfo};
pub use error::{ProviderError, ProviderErrorKind, classify_error_text, classify_http_error};
pub use http::{
    ERROR_BODY_TRUNCATED_SUFFIX, MAX_ERROR_BODY_BYTES, MAX_RESPONSE_BODY_BYTES, RetryPolicy,
    ensure_success, read_checked_body, read_error_body, read_response_body, send_with_retry,
    send_with_retry_policy,
};
pub use sse::{
    DEFAULT_SSE_IDLE_TIMEOUT, SseDecoder, SseFrame, SseParser, sse_stream, sse_stream_with_timeout,
};
pub use transport::HttpTransport;

/// Закреплённый потоковый итератор на куче
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

/// Бэкенд модели для потоковой генерации ответов
#[async_trait]
pub trait Provider: Send + Sync {
    /// Имя провайдера для логов и диагностики
    fn name(&self) -> &'static str;

    /// Запускает потоковую генерацию по запросу
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}
