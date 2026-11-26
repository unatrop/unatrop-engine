//! Axum Protobuf extractor & responder.

use axum::{
    body::Bytes,
    extract::{FromRequest, Request},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use prost::Message as ProstMessage;
use uno_daemon_protocol::CONTENT_TYPE_PROTOBUF;

/// Axum extractor & responder for `application/x-protobuf` payloads.
pub(crate) struct Protobuf<T>(pub T);

impl<S, T> FromRequest<S> for Protobuf<T>
where
    S: Send + Sync,
    T: ProstMessage + Default,
{
    type Rejection = (StatusCode, String);

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        let decoded = T::decode(bytes.as_ref())
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid protobuf: {e}")))?;
        Ok(Self(decoded))
    }
}

impl<T: ProstMessage> IntoResponse for Protobuf<T> {
    fn into_response(self) -> Response {
        (
            [(header::CONTENT_TYPE, CONTENT_TYPE_PROTOBUF)],
            self.0.encode_to_vec(),
        )
            .into_response()
    }
}
