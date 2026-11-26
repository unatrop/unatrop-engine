//! Voice call feature module (`GET /v1/call/ws`): STT + VAD + streaming TTS orchestration.

mod handlers;
mod router;
mod service;
mod tts_bridge;

pub(crate) use router::router;
pub(crate) use service::CallService;
