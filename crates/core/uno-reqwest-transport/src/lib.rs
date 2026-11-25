//! Реализация [`HttpTransport`] на базе `reqwest`

#![deny(missing_docs)]

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{Request, Response};
use transport::{ByteStream, HttpTransport, TransportError};

mod error;

use error::map_reqwest_error;

/// HTTP-транспорт поверх `reqwest::Client`
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Создаёт транспорт с таймаутом подключения 30 с и чтения 180 с
    pub fn new() -> Result<Self, reqwest::Error> {
        Self::build(false, true)
    }

    /// Создаёт транспорт без системного HTTP-прокси (удобно для локальных адресов и тестов)
    pub fn without_proxy() -> Result<Self, reqwest::Error> {
        Self::build(true, true)
    }

    /// Создаёт транспорт без автоматических HTTP-редиректов (для ручной проверки `Location` от SSRF)
    pub fn without_redirects() -> Result<Self, reqwest::Error> {
        Self::build(false, false)
    }

    fn build(disable_proxy: bool, follow_redirects: bool) -> Result<Self, reqwest::Error> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(180))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30));
        if disable_proxy {
            builder = builder.no_proxy();
        }
        if !follow_redirects {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        let client = builder.build()?;
        Ok(Self { client })
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<ByteStream>, TransportError> {
        let (parts, body) = request.into_parts();
        let request = self
            .client
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body)
            .build()
            .map_err(map_reqwest_error)?;

        let response = self
            .client
            .execute(request)
            .await
            .map_err(map_reqwest_error)?;

        let status = response.status();
        let headers = response.headers().clone();
        let body: ByteStream = Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(map_reqwest_error)),
        );

        let mut output = Response::new(body);
        *output.status_mut() = status;
        *output.headers_mut() = headers;
        Ok(output)
    }
}
