//! Health check HTTP handler.

use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

pub(super) async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}
