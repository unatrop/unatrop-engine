//! Health and readiness check route.

use axum::{Json, Router, routing::get};
use serde::Serialize;

use crate::server::DaemonState;

#[derive(Serialize)]
pub(crate) struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

pub(crate) fn router() -> Router<DaemonState> {
    Router::new().route("/health", get(health_check))
}

async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}
