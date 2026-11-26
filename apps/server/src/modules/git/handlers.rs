//! Thin HTTP handlers for git diff and revert endpoints.

use axum::extract::{Path as AxumPath, State};
use uno_daemon_protocol::pb;

use super::service;
use crate::app::{DaemonError, DaemonState, Protobuf};

pub(super) async fn get_turn_diff(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Protobuf<pb::TurnDiffResponse>, DaemonError> {
    let ws = state.sessions.workspace_of(&id)?;
    tokio::task::spawn_blocking(move || service::read_turn_diff(&ws))
        .await
        .map_err(|e| DaemonError::internal(e.to_string()))?
        .map(Protobuf)
}

pub(super) async fn revert_turn_changes(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
    Protobuf(req): Protobuf<pb::RevertRequest>,
) -> Result<Protobuf<pb::RevertResponse>, DaemonError> {
    let ws = state.sessions.workspace_of(&id)?;
    tokio::task::spawn_blocking(move || service::revert_files(&ws, req.files))
        .await
        .map_err(|e| DaemonError::internal(e.to_string()))?
        .map(Protobuf)
}
