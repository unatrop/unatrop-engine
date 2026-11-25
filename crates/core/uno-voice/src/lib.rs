//! Core real-time voice engine for Unatrop (OS-agnostic audio pipeline, VAD, local STT).
//!
//! The crate is deliberately host-agnostic: capture and playback live in the host (cpal,
//! WASAPI, WebAudio), while this crate owns the parts that must behave identically
//! everywhere — denoising, endpointing, the rolling audio ring, transcription contracts,
//! and the barge-in accounting that ties them together.

pub mod aec;
pub mod audio;
pub mod denoise;
pub mod endpoint;
pub mod pipeline;
pub mod playback;
pub mod ring;
pub mod session;
pub mod stt;
pub mod tts;
pub mod tts_text;
pub mod vad;
pub mod wake;

#[cfg(feature = "transcribe")]
pub mod onnx_stt;

pub use aec::{AecProbe, AecState, NoAecProbe};
pub use audio::{AudioFrame, TARGET_SAMPLE_RATE, rms_level, to_mono_f32, to_mono_f32_16khz};
pub use denoise::VoiceDenoiser;
pub use endpoint::{EndpointPolicy, EndpointReason, EndpointReport, TimeoutKind, VoiceTimeouts};
pub use pipeline::{
    AssistantReply, EchoResponder, Responder, VoicePipeline, VoicePipelineEvent,
    VoicePipelineHandle,
};
pub use playback::{Interrupted, PlaybackEvent, SpeechUtterance, TtsPlayer};
pub use ring::AudioRing;
pub use session::{VoiceCommand, VoiceConfig, VoiceEvent, VoiceHandle, VoiceSession};
pub use stt::{DownloadProgressFn, StreamingStt, SttEngine, Transcript, VoiceError, Word};
pub use tts::{TtsChunkStream, TtsEngine, TtsOptions, decode_wav_mono};
pub use tts_text::{
    TtsHints, extract_ready_sentences, extract_ready_sentences_with_hints,
    finalize_tts_tail_with_hints, is_echo_of_spoken, normalize_echo_text, sanitize_for_tts,
    tts_tool_announcement, voice_mode_instruction,
};
pub use vad::{EnergyVad, VadConfig, VadDecision, VadEnd, VadReport};
pub use wake::{DisabledWakeWord, WakeHit, WakeKind, WakeWordDetector};

#[cfg(feature = "transcribe")]
pub use onnx_stt::OnnxStt;

#[cfg(feature = "alice-tts")]
pub use tts::{ALICE_DEFAULT_VOICE, AliceTts, DEFAULT_ALICE_TTS_URL};
