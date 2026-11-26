//! WebSocket upgrade and binary Protobuf pump for `/v1/call/ws`.

use std::sync::Arc;

use axum::{
    extract::{
        State,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use uno_daemon_protocol::{decode_proto, encode_proto_bytes, pb};

use super::service::CallService;
use crate::app::DaemonState;

pub(super) async fn call_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<DaemonState>,
) -> impl IntoResponse {
    let call_service = Arc::clone(&state.call);
    ws.on_upgrade(move |socket| handle_call_ws(socket, call_service))
}

async fn handle_call_ws(socket: WebSocket, service: Arc<CallService>) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<pb::CallServerMsg>(128);
    let write_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let bytes = encode_proto_bytes(&msg);
            if ws_tx.send(WsMessage::Binary(bytes)).await.is_err() {
                break;
            }
        }
    });

    let mut session = service.open_session(out_tx);

    while let Some(Ok(frame)) = ws_rx.next().await {
        let WsMessage::Binary(bytes) = frame else {
            continue;
        };
        let Ok(cmd) = decode_proto::<pb::CallClientMsg>(&bytes) else {
            continue;
        };
        if !session.handle_command(cmd) {
            break;
        }
    }

    session.shutdown();
    write_task.abort();
}
