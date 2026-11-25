//! Route composition for `uno-daemon`.

mod call;
#[cfg(feature = "host-tools")]
mod git;
mod health;
mod sessions;
mod tts;
mod ws;

use axum::{
    Router,
    body::Bytes,
    extract::{FromRequest, Request},
    http::{Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
};
use prost::Message as ProstMessage;
use tower_http::cors::{AllowOrigin, CorsLayer};
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

use crate::server::{DaemonState, auth_middleware};

fn is_loopback_origin(origin: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(origin) else {
        return false;
    };
    let Some(rest) = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
    else {
        return false;
    };
    let check_port = |after: &str| {
        after.is_empty()
            || after
                .strip_prefix(':')
                .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    if let Some(after) = rest.strip_prefix("localhost") {
        check_port(after)
    } else if let Some(after) = rest.strip_prefix("127.0.0.1") {
        check_port(after)
    } else if let Some(after) = rest.strip_prefix("[::1]") {
        check_port(after)
    } else {
        false
    }
}

/// Builds the complete HTTP + WebSocket router for `uno-daemon`.
pub(crate) fn router(state: DaemonState) -> Router {
    let sessions_api = Router::new().merge(sessions::router()).merge(ws::router());
    #[cfg(feature = "host-tools")]
    let sessions_api = sessions_api.merge(git::router());

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            is_loopback_origin(origin.as_bytes())
        }))
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT]);

    Router::new()
        .merge(health::router())
        .merge(call::router())
        .merge(tts::router())
        .nest("/v1/sessions", sessions_api)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(cors)
        .with_state(state)
}
