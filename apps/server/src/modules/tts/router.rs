//! Routes for `/v1/tts`.

use axum::{Router, routing::post};

use super::handlers;
use crate::app::DaemonState;

pub(crate) fn router() -> Router<DaemonState> {
    Router::new().route("/v1/tts", post(handlers::synthesize_tts))
}
