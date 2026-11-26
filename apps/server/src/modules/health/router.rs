//! Routes for `/health`.

use axum::{Router, routing::get};

use super::handlers;
use crate::app::DaemonState;

pub(crate) fn router() -> Router<DaemonState> {
    Router::new().route("/health", get(handlers::health_check))
}
