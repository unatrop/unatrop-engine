//! Routes for `/v1/providers`, `/v1/accounts`, and `/v1/accounts/oauth/*`.

use axum::{
    Router,
    routing::{delete, get, post},
};

use super::handlers;
use crate::app::DaemonState;

pub(crate) fn router() -> Router<DaemonState> {
    Router::new()
        .route("/v1/providers", get(handlers::list_providers))
        .route(
            "/v1/accounts",
            get(handlers::list_accounts).post(handlers::upsert_account),
        )
        .route("/v1/accounts/{id}", delete(handlers::delete_account))
        .route(
            "/v1/accounts/{id}/models",
            post(handlers::add_account_model),
        )
        .route("/v1/accounts/{id}/refresh", post(handlers::refresh_account))
        .route(
            "/v1/accounts/oauth/{kind}/start",
            post(handlers::start_oauth),
        )
        .route("/v1/accounts/oauth/{flow_id}", get(handlers::poll_oauth))
}
