//! Routes nested under `/v1/sessions`.

use axum::{
    Router,
    routing::{delete, get},
};

use super::handlers;
use crate::app::DaemonState;

pub(crate) fn router() -> Router<DaemonState> {
    let base = Router::new()
        .route(
            "/",
            get(handlers::list_sessions).post(handlers::create_session),
        )
        .route("/{id}", delete(handlers::delete_session))
        .route("/{id}/history", get(handlers::get_history))
        .route("/{id}/metrics", get(handlers::get_metrics))
        .route("/{id}/ws", get(handlers::ws_handler));

    #[cfg(feature = "host-tools")]
    let base = base.merge(crate::modules::git::router());

    base
}
