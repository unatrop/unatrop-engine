//! Unified HTTP error mapping for `uno-daemon` routes.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug)]
pub(crate) enum DaemonError {
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl DaemonError {
    pub(crate) fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    pub(crate) fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    pub(crate) fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl IntoResponse for DaemonError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, uno_protocol::redact_secrets(&msg)),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, uno_protocol::redact_secrets(&msg)),
            Self::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                uno_protocol::redact_secrets(&msg),
            ),
        };
        (status, Json(ErrorBody { error: &message })).into_response()
    }
}

impl From<uno_session::SessionError> for DaemonError {
    fn from(err: uno_session::SessionError) -> Self {
        Self::Internal(err.to_string())
    }
}
