//! Local HTTP + WebSocket daemon (`unatropd`) exposing the Unatrop agent core
//! over `uno-daemon-protocol` for Desktop, Mobile, and CLI clients.

mod agent;
mod routes;
mod server;
mod session;
mod storage;
mod tts;

pub use agent::config::load_or_create_local_daemon_token;
pub use server::DaemonState;
pub use uno_daemon_protocol as protocol;

use std::{future::Future, net::SocketAddr, path::PathBuf};

use axum::Router;
use tokio::net::TcpListener;

pub fn build_router(state: DaemonState) -> Router {
    routes::router(state)
}

/// Builds the default layered system prompt for a workspace and model.
pub fn build_default_system_prompt(
    workspace: &std::path::Path,
    model: &str,
) -> Result<String, String> {
    agent::prompts::build_system_prompt(None, workspace, model, &[]).map_err(|e| format!("{e:?}"))
}

pub async fn serve(addr: SocketAddr, auth_token: Option<String>) -> Result<(), std::io::Error> {
    let has_token = auth_token.as_deref().is_some_and(|t| !t.trim().is_empty());
    if !addr.ip().is_loopback() && !has_token {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to bind non-loopback address without UNATROP_DAEMON_TOKEN",
        ));
    }
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("unatropd listening on http://{addr}");
    serve_with_shutdown(listener, DaemonState::new(auth_token), shutdown_signal()).await
}

/// Serves `uno-daemon` on a pre-bound `TcpListener` (useful for embedded mobile hosts binding `127.0.0.1:0`).
pub async fn serve_with_shutdown(
    listener: TcpListener,
    state: DaemonState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let app = build_router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

/// Convenience helper for embedded hosts (mobile / tests) that want a custom `sessions_dir`.
pub async fn serve_in_dir_with_shutdown(
    listener: TcpListener,
    auth_token: Option<String>,
    sessions_dir: PathBuf,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let state = DaemonState::with_sessions_dir(auth_token, sessions_dir);
    serve_with_shutdown(listener, state, shutdown).await
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("unatropd shutting down");
}
