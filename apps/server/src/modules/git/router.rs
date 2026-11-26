//! Routes for session workspace git operations (`/{id}/diff`, `/{id}/revert`).

use axum::{
    Router,
    routing::{get, post},
};

use super::handlers;
use crate::app::DaemonState;

pub(crate) fn router() -> Router<DaemonState> {
    Router::new()
        .route("/{id}/diff", get(handlers::get_turn_diff))
        .route("/{id}/revert", post(handlers::revert_turn_changes))
}
