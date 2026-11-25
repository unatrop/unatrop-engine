//! HTTP CRUD, history, and telemetry endpoints for sessions.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use axum::{
    Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    routing::{delete, get},
};
use uno_daemon_protocol::pb;

use crate::{
    routes::Protobuf,
    server::{DaemonError, DaemonState, validate_session_id},
    session::build_session_entry,
    storage::derive_session_title,
};

pub(crate) fn router() -> Router<DaemonState> {
    Router::new()
        .route("/", get(list_sessions).post(create_session))
        .route("/{id}", delete(delete_session))
        .route("/{id}/history", get(get_history))
        .route("/{id}/metrics", get(get_metrics))
}

async fn list_sessions(State(state): State<DaemonState>) -> Protobuf<pb::ListSessionsResponse> {
    let entries = state.list_entries();
    let mut seen = HashSet::with_capacity(entries.len());
    let mut out = Vec::with_capacity(entries.len());

    for entry in entries {
        seen.insert(entry.id.clone());
        let history = entry.client.history().unwrap_or_default();
        let is_running = entry
            .active_task
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false);
        out.push(pb::SessionSummary {
            session_id: entry.id.clone(),
            title: derive_session_title(&history),
            model: entry.model.clone(),
            workspace: entry.workspace.display().to_string(),
            message_count: history.len() as u64,
            is_running,
            persisted: entry.session_file.is_file() || entry.store.is_some(),
            updated_at: entry.updated_at.load(Ordering::Relaxed),
        });
    }

    if let Some(store) = &state.store {
        for meta in store.list_sessions() {
            if seen.insert(meta.id.clone()) {
                out.push(pb::SessionSummary {
                    session_id: meta.id,
                    title: meta.title,
                    model: meta.model,
                    workspace: meta.workspace,
                    message_count: meta.message_count as u64,
                    is_running: false,
                    persisted: true,
                    updated_at: meta.updated_at,
                });
            }
        }
    }

    out.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Protobuf(pb::ListSessionsResponse { sessions: out })
}

async fn create_session(
    State(state): State<DaemonState>,
    Protobuf(req): Protobuf<pb::CreateSessionRequest>,
) -> Result<Protobuf<pb::CreateSessionResponse>, DaemonError> {
    let (entry, context_profile) = build_session_entry(&state, req).await?;
    let id = entry.id.clone();
    state.insert_session(id.clone(), entry);

    Ok(Protobuf(pb::CreateSessionResponse {
        session_id: id,
        context_profile: Some(context_profile.into()),
    }))
}

async fn delete_session(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, DaemonError> {
    let id = validate_session_id(&id)?;
    let removed_mem = state.remove_session(&id);
    if let Some(ref entry) = removed_mem {
        entry.cancel_active_turn();
        let path = entry.session_file.clone();
        tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_file(path);
        });
    }
    let removed_db = state
        .store
        .as_ref()
        .is_some_and(|store| store.delete_sync(&id));
    if removed_mem.is_some() || removed_db {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
}

async fn get_history(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Protobuf<pb::HistoryResponse>, DaemonError> {
    let id = validate_session_id(&id)?;
    if let Ok(entry) = state.get_session(&id) {
        let history = entry.client.history()?;
        return Ok(Protobuf(pb::HistoryResponse {
            messages: history.iter().map(pb::Message::from).collect(),
        }));
    }
    if let Some(store) = &state.store
        && let Some(history) = store.load_history(&id)
    {
        return Ok(Protobuf(pb::HistoryResponse {
            messages: history.iter().map(pb::Message::from).collect(),
        }));
    }
    Err(DaemonError::not_found("session not found"))
}

async fn get_metrics(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Protobuf<pb::SessionMetricsResponse>, DaemonError> {
    let id = validate_session_id(&id)?;
    let entry = state.get_session(&id)?;
    let metrics = entry
        .metrics
        .lock()
        .map(|m| m.to_response())
        .map_err(|_| DaemonError::internal("session metrics poisoned"))?;
    Ok(Protobuf(metrics))
}
