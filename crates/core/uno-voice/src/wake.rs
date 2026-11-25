//! Wake-word detector seam.
//!
//! Quark runs four separate spotters — activation, interruption, intonation, and
//! intonation-under-interruption — each a small model with its own lifecycle
//! (`activationSpotterSettings.resetAfterStop`, `SpotterController`, `SpotterAudioGate`).
//! For a desktop assistant two of them matter: hearing the wake word, and noticing that the
//! user started talking over playback.
//!
//! No model ships here. This is the seam so a TFLite/ONNX spotter can be dropped in without
//! touching the session loop, plus a disabled implementation for builds without one.

/// Which detector fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeKind {
    /// The assistant's name was spoken.
    Activation,
    /// The user started speaking while the assistant was talking.
    Interruption,
}

/// A wake-word detection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WakeHit {
    /// Which detector fired.
    pub kind: WakeKind,
    /// Detector confidence in `0.0..=1.0`.
    pub confidence: f32,
}

impl WakeHit {
    /// Creates a hit with the given confidence.
    pub fn new(kind: WakeKind, confidence: f32) -> Self {
        Self { kind, confidence }
    }
}

/// Streaming wake-word detector operating on 16 kHz mono `f32` audio.
///
/// Implementations keep their own model state and must be cheap enough to run on every
/// captured frame, because the detector sees the microphone before the VAD does.
pub trait WakeWordDetector: Send {
    /// Feeds a chunk and returns a hit when the wake word just completed.
    fn push(&mut self, pcm_16khz: &[f32]) -> Option<WakeHit>;

    /// Drops buffered audio and model state (after a detection or a cancel).
    fn reset(&mut self);
}

/// Detector that never fires, for builds without a wake-word model.
#[derive(Clone, Copy, Debug, Default)]
pub struct DisabledWakeWord;

impl WakeWordDetector for DisabledWakeWord {
    fn push(&mut self, _pcm_16khz: &[f32]) -> Option<WakeHit> {
        None
    }

    fn reset(&mut self) {}
}
