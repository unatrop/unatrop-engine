//! Thin HTTP and WebSocket handlers for `/v1/sessions`.

use std::sync::Arc;

use axum::{
    extract::{
        Path as AxumPath, State,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use uno_daemon_protocol::{decode_proto, encode_proto_bytes, pb};
use uno_protocol::Message;

use super::runtime::entry::SessionEntry;
use crate::app::{DaemonError, DaemonState, Protobuf};

pub(super) async fn list_sessions(
    State(state): State<DaemonState>,
) -> Protobuf<pb::ListSessionsResponse> {
    Protobuf(state.sessions.list())
}

pub(super) async fn create_session(
    State(state): State<DaemonState>,
    Protobuf(req): Protobuf<pb::CreateSessionRequest>,
) -> Result<Protobuf<pb::CreateSessionResponse>, DaemonError> {
    Ok(Protobuf(state.sessions.create(req).await?))
}

pub(super) async fn delete_session(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, DaemonError> {
    let deleted = state.sessions.delete(&id)?;
    Ok(if deleted {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

pub(super) async fn get_history(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Protobuf<pb::HistoryResponse>, DaemonError> {
    Ok(Protobuf(state.sessions.history(&id)?))
}

pub(super) async fn get_metrics(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Protobuf<pb::SessionMetricsResponse>, DaemonError> {
    Ok(Protobuf(state.sessions.metrics(&id)?))
}

pub(super) async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, DaemonError> {
    let entry = state.sessions.get_validated(&id)?;
    Ok(ws.on_upgrade(move |socket| handle_ws(socket, entry)))
}

async fn handle_ws(socket: WebSocket, entry: Arc<SessionEntry>) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut rx = entry.events.subscribe();

    let send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            let bytes = encode_proto_bytes(&msg);
            if ws_tx.send(WsMessage::Binary(bytes)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(frame)) = ws_rx.next().await {
        let WsMessage::Binary(bytes) = frame else {
            continue;
        };
        let Ok(cmd) = decode_proto::<pb::ClientMsg>(&bytes) else {
            continue;
        };
        dispatch_client_message(&entry, cmd);
    }

    send_task.abort();
}

fn dispatch_client_message(entry: &Arc<SessionEntry>, cmd: pb::ClientMsg) {
    match cmd.msg {
        Some(pb::client_msg::Msg::Ping(_)) => {
            let _ = entry.events.send(pb::ServerMsg {
                msg: Some(pb::server_msg::Msg::Pong(pb::Empty {})),
            });
        }
        Some(pb::client_msg::Msg::Approve(c)) => {
            let decision =
                pb::ApprovalDecision::try_from(c.decision).unwrap_or(pb::ApprovalDecision::Deny);
            entry.approval.resolve(&c.request_id, decision);
        }
        Some(pb::client_msg::Msg::Cancel(_)) => entry.cancel_active_turn(),
        Some(pb::client_msg::Msg::TruncateLastUserTurn(_)) => entry.truncate_last_user_turn(),
        Some(pb::client_msg::Msg::SetHistory(c)) => {
            entry.set_history(c.messages.into_iter().map(Message::from).collect());
        }
        Some(pb::client_msg::Msg::ResetSession(_)) => entry.reset_session(),
        Some(pb::client_msg::Msg::Send(c)) => {
            entry.spawn_turn_with_options(c.text, c.voice_mode, c.voice_locale);
        }
        None => {}
    }
}
