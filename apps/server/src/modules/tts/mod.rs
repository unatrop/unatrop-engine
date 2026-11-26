//! Text-to-speech feature module (`POST /v1/tts`).

mod handlers;
mod router;
mod service;

pub(crate) use router::router;
pub(crate) use service::TtsService;
