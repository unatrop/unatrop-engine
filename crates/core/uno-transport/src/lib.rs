//! Абстракция HTTP-транспорта с потоковым телом ответа

#![deny(missing_docs)]

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use http::{Request, Response};

mod error;

pub use error::TransportError;

/// Поток байтовых чанков тела ответа
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send + 'static>>;

/// Асинхронный HTTP-клиент для отправки запросов с потоковым ответом
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Отправляет HTTP-запрос и возвращает ответ с потоком тела
    async fn send(&self, request: Request<Bytes>) -> Result<Response<ByteStream>, TransportError>;
}
