//! HTTP/Axum infrastructure: shared state, error mapping, middleware, and top-level router.

pub(crate) mod error;
pub(crate) mod middleware;
pub(crate) mod protobuf;
pub(crate) mod state;

use axum::{
    Router,
    http::{Method, header},
    middleware as axum_middleware,
};
use tower_http::cors::{AllowOrigin, CorsLayer};

pub(crate) use error::DaemonError;
pub(crate) use middleware::{auth_middleware, is_loopback_origin};
pub(crate) use protobuf::Protobuf;
pub use state::DaemonState;

use crate::modules;

/// Builds the complete HTTP + WebSocket router for `uno-daemon`.
pub(crate) fn build_router(state: DaemonState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            is_loopback_origin(origin.as_bytes())
        }))
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT]);

    Router::new()
        .merge(modules::health::router())
        .merge(modules::accounts::router())
        .merge(modules::call::router())
        .merge(modules::tts::router())
        .nest("/v1/sessions", modules::sessions::router())
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(cors)
        .with_state(state)
}
