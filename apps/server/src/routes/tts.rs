//! Protobuf TTS endpoint (`POST /v1/tts`) backed by the pooled Rust `uni.ws` client.

use axum::{Router, extract::State, routing::post};
use uno_daemon_protocol::pb;

use crate::{
    routes::Protobuf,
    server::{DaemonError, DaemonState},
};

pub(crate) fn router() -> Router<DaemonState> {
    Router::new().route("/v1/tts", post(synthesize_tts))
}

async fn synthesize_tts(
    State(state): State<DaemonState>,
    Protobuf(req): Protobuf<pb::TtsSynthesizeRequest>,
) -> Result<Protobuf<pb::TtsSynthesizeResponse>, DaemonError> {
    let audio = state
        .tts_pool
        .synthesize(
            &req.text,
            req.voice.as_deref(),
            req.lang.as_deref(),
            req.quality.as_deref(),
            req.emotion.as_deref(),
        )
        .await
        .map_err(DaemonError::bad_request)?;

    Ok(Protobuf(pb::TtsSynthesizeResponse {
        utterance_id: audio.utterance_id,
        sample_rate: audio.sample_rate,
        pcm_s16le: audio.pcm_s16le,
        duration_ms: audio.duration_ms,
    }))
}
