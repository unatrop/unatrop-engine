//! Routes for `/v1/call/ws`.

use axum::{Router, routing::get};

use super::handlers;
use crate::app::DaemonState;

pub(crate) fn router() -> Router<DaemonState> {
    Router::new().route("/v1/call/ws", get(handlers::call_ws_handler))
}
