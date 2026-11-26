//! HTTP handlers for `/v1/providers` and `/v1/accounts`.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};

use super::dto::{
    AccountView, OAuthPollResponse, OAuthStartResponse, ProviderPresetView, UpsertAccountRequest,
};
use crate::app::{DaemonError, DaemonState};

#[derive(Deserialize)]
pub(super) struct AddModelRequest {
    model: String,
}

#[derive(Default, Deserialize)]
pub(super) struct StartOAuthRequest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Serialize)]
pub(super) struct DeleteAccountResponse {
    deleted: bool,
}

pub(super) async fn list_providers(
    State(state): State<DaemonState>,
) -> Json<Vec<ProviderPresetView>> {
    Json(state.accounts.list_presets())
}

pub(super) async fn list_accounts(State(state): State<DaemonState>) -> Json<Vec<AccountView>> {
    Json(state.accounts.list_views().await)
}

pub(super) async fn upsert_account(
    State(state): State<DaemonState>,
    Json(req): Json<UpsertAccountRequest>,
) -> Result<Json<AccountView>, DaemonError> {
    Ok(Json(state.accounts.upsert(req).await?))
}

pub(super) async fn delete_account(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
) -> Result<Json<DeleteAccountResponse>, DaemonError> {
    let deleted = state.accounts.delete(&id).await?;
    Ok(Json(DeleteAccountResponse { deleted }))
}

pub(super) async fn add_account_model(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
    Json(req): Json<AddModelRequest>,
) -> Result<Json<AccountView>, DaemonError> {
    Ok(Json(state.accounts.add_model(&id, &req.model).await?))
}

pub(super) async fn refresh_account(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
) -> Result<Json<AccountView>, DaemonError> {
    Ok(Json(state.accounts.refresh(&id).await?))
}

pub(super) async fn start_oauth(
    State(state): State<DaemonState>,
    Path(kind): Path<String>,
    body: Option<Json<StartOAuthRequest>>,
) -> Result<Json<OAuthStartResponse>, DaemonError> {
    let req = body.map(|b| b.0).unwrap_or_default();
    Ok(Json(
        state.accounts.start_oauth(&kind, req.id, req.label).await?,
    ))
}

pub(super) async fn poll_oauth(
    State(state): State<DaemonState>,
    Path(flow_id): Path<String>,
) -> Result<Json<OAuthPollResponse>, DaemonError> {
    Ok(Json(state.accounts.poll_oauth(&flow_id).await?))
}
