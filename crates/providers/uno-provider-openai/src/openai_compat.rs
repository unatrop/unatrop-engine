use bytes::Bytes;
use http::Request;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use protocol::{CompletionRequest, StreamEvent};
use provider::{BoxStream, ProviderError, SseDecoder, ensure_success, send_with_retry};
use serde_json::Value;
use transport::HttpTransport;

use crate::responses::{ResponsesStreamState, responses_request_body};

mod request;
mod stream;

/// Формирует JSON-тело запроса в формате OpenAI `/chat/completions`
pub fn openai_request_body(request: &CompletionRequest) -> Result<Value, ProviderError> {
    request::request_body(request)
}

/// Формирует JSON-тело запроса с явным управлением `cache_control`
pub fn openai_request_body_with_cache(
    request: &CompletionRequest,
    prompt_cache: Option<bool>,
) -> Result<Value, ProviderError> {
    request::request_body_with_cache(request, prompt_cache)
}

/// Отправляет запрос на `/chat/completions` с дополнительными заголовками и возвращает поток [`StreamEvent`]
pub async fn stream_chat_completions(
    transport: &dyn HttpTransport,
    base_url: &str,
    bearer_token: &str,
    extra_headers: &[(&str, &str)],
    request: &CompletionRequest,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
    stream_chat_completions_with_cache(
        transport,
        base_url,
        bearer_token,
        extra_headers,
        request,
        None,
    )
    .await
}

/// Отправляет запрос на `/chat/completions` с явным управлением `cache_control`
pub async fn stream_chat_completions_with_cache(
    transport: &dyn HttpTransport,
    base_url: &str,
    bearer_token: &str,
    extra_headers: &[(&str, &str)],
    request: &CompletionRequest,
    prompt_cache: Option<bool>,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
    let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let payload = openai_request_body_with_cache(request, prompt_cache)?;
    post_sse(
        transport,
        &endpoint,
        bearer_token,
        extra_headers,
        &payload,
        stream::StreamState::default(),
    )
    .await
}

/// Отправляет запрос на `/responses` с дополнительными заголовками и возвращает поток [`StreamEvent`]
pub async fn stream_responses(
    transport: &dyn HttpTransport,
    base_url: &str,
    bearer_token: &str,
    extra_headers: &[(&str, &str)],
    request: &CompletionRequest,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
    let endpoint = format!("{}/responses", base_url.trim_end_matches('/'));
    let payload = responses_request_body(request)?;
    post_sse(
        transport,
        &endpoint,
        bearer_token,
        extra_headers,
        &payload,
        ResponsesStreamState::default(),
    )
    .await
}

async fn post_sse<D: SseDecoder>(
    transport: &dyn HttpTransport,
    endpoint: &str,
    bearer_token: &str,
    extra_headers: &[(&str, &str)],
    payload: &Value,
    decoder: D,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
    let body = serde_json::to_vec(payload).map_err(|error| {
        ProviderError::InvalidResponse(format!("Failed to serialize request: {error}"))
    })?;

    let mut builder = Request::post(endpoint)
        .header(AUTHORIZATION, format!("Bearer {bearer_token}"))
        .header(CONTENT_TYPE, "application/json");
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }

    let http_request = builder.body(Bytes::from(body)).map_err(|error| {
        ProviderError::InvalidResponse(format!("Failed to build HTTP request: {error}"))
    })?;

    send_sse_with_decoder(transport, http_request, decoder).await
}

/// Отправляет готовый HTTP-запрос и преобразует SSE-ответ в события протокола
///
/// Используется провайдерами с общим форматом OpenAI, но нестандартными
/// эндпоинтами, авторизацией или заголовками (например Copilot)
pub async fn send_openai_stream(
    transport: &dyn HttpTransport,
    request: Request<Bytes>,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
    send_sse_with_decoder(transport, request, stream::StreamState::default()).await
}

async fn send_sse_with_decoder<D: SseDecoder>(
    transport: &dyn HttpTransport,
    request: Request<Bytes>,
    decoder: D,
) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
    let (parts, body) = request.into_parts();
    let response = send_with_retry(transport, || {
        Ok(Request::from_parts(parts.clone(), body.clone()))
    })
    .await?;
    let response = ensure_success(response).await?;

    Ok(provider::sse_stream(response.into_body(), decoder))
}
