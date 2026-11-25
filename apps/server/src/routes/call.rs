//! Protobuf WebSocket endpoint (`GET /v1/call/ws`) hosting `uno-voice` STT + VAD + denoise.

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::{
    Router,
    extract::{
        State,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use tokio::runtime::Handle;
use uno_daemon_protocol::{decode_proto, encode_proto_bytes, pb};
use uno_voice::{AudioFrame, TimeoutKind, VoiceConfig, VoiceEvent, VoiceHandle, VoiceSession};

use crate::{
    server::DaemonState,
    tts::orchestrator::{CallTtsPipeline, spawn_session_tts_bridge},
};

pub(crate) fn router() -> Router<DaemonState> {
    Router::new().route("/v1/call/ws", get(call_ws_handler))
}

async fn call_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<DaemonState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_call_ws(socket, state))
}

fn resolve_call_voice(explicit: Option<String>) -> Option<String> {
    explicit
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("UNATROP_TTS_VOICE")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| Some("kolya".to_owned()))
}

async fn handle_call_ws(socket: WebSocket, state: DaemonState) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<pb::CallServerMsg>(128);
    let write_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let bytes = encode_proto_bytes(&msg);
            if ws_tx.send(WsMessage::Binary(bytes)).await.is_err() {
                break;
            }
        }
    });

    let pipeline =
        std::sync::Arc::new(CallTtsPipeline::new(state.tts_pool.clone(), out_tx.clone()));
    let mut voice_handle: Option<VoiceHandle> = None;
    let mut forward_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut session_bridge_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut stt_started = false;
    let mut call_voice: Option<String> = resolve_call_voice(None);
    let mut call_lang: Option<String> = None;
    let mut auto_speak = false;

    while let Some(Ok(frame)) = ws_rx.next().await {
        let WsMessage::Binary(bytes) = frame else {
            continue;
        };
        let Ok(cmd) = decode_proto::<pb::CallClientMsg>(&bytes) else {
            continue;
        };
        match cmd.msg {
            Some(pb::call_client_msg::Msg::Start(start)) => {
                if !stt_started {
                    state.active_calls.fetch_add(1, Ordering::AcqRel);
                    state.call_epoch.fetch_add(1, Ordering::AcqRel);
                    stt_started = true;
                }
                if let Some(h) = voice_handle.take() {
                    h.close();
                }
                if let Some(t) = forward_task.take() {
                    t.abort();
                }
                if let Some(t) = session_bridge_task.take() {
                    t.abort();
                }

                call_voice = resolve_call_voice(start.voice);
                call_lang = start.language.clone();
                auto_speak = start.auto_speak;

                let tts_pool = state.tts_pool.clone();
                tokio::spawn(async move {
                    tts_pool.prewarm().await;
                });

                // Conversational endpointing by default: 900 ms base pause with 60 s utterances
                // and mid-pause completeness detection.
                // Set UNATROP_VOICE_LOW_LATENCY=1 for aggressive 280 ms turns, or =0 for continuous partials.
                let config = match std::env::var("UNATROP_VOICE_LOW_LATENCY").ok().as_deref() {
                    Some("1" | "true") => VoiceConfig {
                        language: start.language,
                        ..VoiceConfig::low_latency()
                    },
                    Some("0" | "false") => VoiceConfig {
                        language: start.language,
                        ..VoiceConfig::default()
                    },
                    _ => VoiceConfig {
                        language: start.language,
                        ..VoiceConfig::conversational()
                    },
                };
                let (handle, mut events) =
                    VoiceSession::spawn(&Handle::current(), config, state.shared_stt());
                voice_handle = Some(handle);

                let tx = out_tx.clone();
                let pipe_for_fwd = pipeline.clone();
                forward_task = Some(tokio::spawn(async move {
                    while let Some(ev) = events.recv().await {
                        match &ev {
                            VoiceEvent::SpeechStarted => {
                                if auto_speak {
                                    pipe_for_fwd.interrupt();
                                }
                            }
                            VoiceEvent::Transcript(t) => {
                                let trimmed = t.text.trim();
                                if !trimmed.is_empty() && pipe_for_fwd.is_recent_echo(trimmed) {
                                    continue;
                                }
                            }
                            _ => {}
                        }
                        let mapped = map_voice_event(ev);
                        if let Some(msg) = mapped
                            && tx.send(pb::CallServerMsg { msg: Some(msg) }).await.is_err()
                        {
                            break;
                        }
                    }
                }));

                if auto_speak
                    && let Some(ref sid) = start.session_id
                    && let Ok(entry) = state.get_session(sid)
                {
                    session_bridge_task = Some(spawn_session_tts_bridge(
                        entry,
                        pipeline.clone(),
                        out_tx.clone(),
                        call_voice.clone(),
                        call_lang.clone(),
                    ));
                }
            }
            Some(pb::call_client_msg::Msg::AttachSession(sid)) => {
                if let Some(t) = session_bridge_task.take() {
                    t.abort();
                }
                if auto_speak && let Ok(entry) = state.get_session(&sid) {
                    session_bridge_task = Some(spawn_session_tts_bridge(
                        entry,
                        pipeline.clone(),
                        out_tx.clone(),
                        call_voice.clone(),
                        call_lang.clone(),
                    ));
                }
            }
            Some(pb::call_client_msg::Msg::Speak(cmd)) => {
                let voice = resolve_call_voice(cmd.voice.or_else(|| call_voice.clone()));
                let lang = cmd.lang.or_else(|| call_lang.clone());
                pipeline.speak_text(&cmd.text, voice, lang, cmd.tag);
            }
            Some(pb::call_client_msg::Msg::InterruptTts(_)) => {
                pipeline.interrupt();
            }
            Some(pb::call_client_msg::Msg::Audio(chunk)) => {
                if let Some(ref handle) = voice_handle {
                    let (pairs, _) = chunk.pcm_s16le.as_chunks::<2>();
                    let samples: Vec<i16> = pairs
                        .iter()
                        .map(|&[lo, hi]| i16::from_le_bytes([lo, hi]))
                        .collect();
                    let channels = u16::try_from(chunk.channels.max(1)).unwrap_or(1);
                    handle.push_audio(AudioFrame::new(samples, chunk.sample_rate, channels));
                }
            }
            Some(pb::call_client_msg::Msg::Commit(_)) => {
                if let Some(ref handle) = voice_handle {
                    handle.commit_turn();
                }
            }
            Some(pb::call_client_msg::Msg::Cancel(_)) => {
                if let Some(ref handle) = voice_handle {
                    handle.cancel();
                }
            }
            Some(pb::call_client_msg::Msg::Stop(_)) => break,
            None => {}
        }
    }

    if let Some(h) = voice_handle {
        h.close();
    }
    if let Some(t) = forward_task {
        t.abort();
    }
    if let Some(t) = session_bridge_task {
        t.abort();
    }
    pipeline.begin_turn(false);
    write_task.abort();

    if stt_started && state.active_calls.fetch_sub(1, Ordering::AcqRel) == 1 {
        let epoch = state.call_epoch.load(Ordering::Acquire);
        let stt_cell = state.stt.clone();
        let active_calls = state.active_calls.clone();
        let call_epoch = state.call_epoch.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if active_calls.load(Ordering::Acquire) == 0
                && call_epoch.load(Ordering::Acquire) == epoch
                && let Some(stt) = stt_cell.get()
            {
                stt.unload().await;
                tracing::info!("unloaded idle GigaAM STT model");
            }
        });
    }
}

fn map_voice_event(ev: VoiceEvent) -> Option<pb::call_server_msg::Msg> {
    match ev {
        VoiceEvent::DownloadingModel { progress, .. } => {
            Some(pb::call_server_msg::Msg::DownloadProgress(progress))
        }
        VoiceEvent::ModelReady => Some(pb::call_server_msg::Msg::ClearHypothesis(pb::Empty {})),
        VoiceEvent::Listening => Some(pb::call_server_msg::Msg::Ready(pb::Empty {})),
        VoiceEvent::SpeechStarted => Some(pb::call_server_msg::Msg::SpeechStarted(pb::Empty {})),
        VoiceEvent::PartialTranscript(text) => {
            Some(pb::call_server_msg::Msg::PartialTranscript(text))
        }
        VoiceEvent::Transcript(t) => {
            let trimmed = t.text.trim();
            (!trimmed.is_empty()).then(|| pb::call_server_msg::Msg::Transcript(trimmed.to_owned()))
        }
        VoiceEvent::Endpoint(report)
            if report.actual_before_ms == 0 && report.actual_after_ms == 0 =>
        {
            Some(pb::call_server_msg::Msg::ClearHypothesis(pb::Empty {}))
        }
        VoiceEvent::Timeout {
            kind: TimeoutKind::FirstPartial | TimeoutKind::StartingSilence,
        } => Some(pb::call_server_msg::Msg::ClearHypothesis(pb::Empty {})),
        VoiceEvent::Error(err) => Some(pb::call_server_msg::Msg::Error(err)),
        _ => None,
    }
}
