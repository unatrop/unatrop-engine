//! Acoustic echo cancellation status.
//!
//! Quark treats AEC health as a first-class event rather than a guess:
//! `SpeechGPT.SpeechUI.AecCheck.Succeed` / `.Failed` / `.Unavailable`. That matters because
//! every other voice feature degrades silently without echo cancellation — the assistant
//! hears itself, the VAD never sees silence, and barge-in fires on its own TTS.
//!
//! The probe is implemented by the host (WASAPI/AVAudioEngine/`getUserMedia` constraints);
//! this module only carries the result into [`crate::session::VoiceEvent`].

/// Result of probing the platform for echo cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AecState {
    /// The platform provides a working echo canceller for the capture device.
    Available,
    /// No echo canceller is exposed by the platform (the common desktop case).
    Unavailable,
    /// An echo canceller exists but failed to initialize.
    Failed,
}

impl AecState {
    /// Whether echo cancellation can be relied on.
    pub fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Host-side probe for echo-cancellation support.
pub trait AecProbe: Send + Sync {
    /// Inspects the capture device once and reports its echo-cancellation status.
    fn probe(&self) -> AecState;
}

/// Probe for hosts with no echo cancellation at all.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAecProbe;

impl AecProbe for NoAecProbe {
    fn probe(&self) -> AecState {
        AecState::Unavailable
    }
}
