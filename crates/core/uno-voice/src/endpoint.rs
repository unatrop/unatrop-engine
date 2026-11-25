//! Endpointing policy and session timeouts.
//!
//! Everything that used to be a magic number scattered across the session loop lives here:
//! how much audio to keep before the trigger, how much trailing silence to include, how long
//! to wait for the first partial, and so on.
//!
//! The shape is borrowed from Quark's `TVoiceActivityDetectorCapability`, where the server
//! tells the client how much audio before/after the trigger it wants (`requestSoundBeforeTriggerMs`,
//! `requestSoundAfterTriggerMs`) and the client reports back how much it actually delivered
//! (`actualSoundBeforeTriggerMs`, `actualSoundAfterTriggerMs`) together with `rmsRatios`.
//! Making this data instead of constants is what lets the same engine survive a different
//! microphone, room, or user without a rebuild.

use crate::vad::VadConfig;

/// Tuning for utterance segmentation and turn endpointing.
#[derive(Clone, Copy, Debug)]
pub struct EndpointPolicy {
    /// Minimum RMS energy (after denoising) to count as voiced.
    pub speech_rms: f32,
    /// Minimum RNNoise speech probability to count as voiced (`0.0..=1.0`).
    ///
    /// The neural probability rejects music and keyboard noise that still has plenty of
    /// energy, which plain RMS cannot distinguish from speech.
    pub speech_prob: f32,
    /// Voiced audio required before an utterance is eligible to commit.
    pub min_speech_ms: u32,
    /// Trailing silence that ends a turn.
    pub silence_commit_ms: u32,
    /// Hard cap on a single utterance.
    pub recording_max_ms: u32,
    /// Audio kept before speech onset so the first syllable is never clipped.
    pub sound_before_trigger_ms: u32,
    /// Trailing silence included in the committed utterance.
    pub sound_after_trigger_ms: u32,
    /// Whether to run live partial transcriptions while the user is still speaking.
    ///
    /// Turning this off removes the biggest source of STT contention: partials and the final
    /// pass share the same blocking model lock, so a partial running at 300 ms cadence can
    /// delay the final transcript by a full inference. TTS-only voice mods never need partials.
    pub enable_partials: bool,
    /// Minimum spacing between live partial transcriptions.
    pub partial_cadence_ms: u32,
    /// Minimum buffered speech before the first partial is attempted.
    pub min_partial_ms: u32,
    /// Partial text unchanged for this long, while the room is quiet, commits the turn.
    pub partial_stable_ms: u32,
    /// Partial text unchanged for this long commits the turn even if energy is still high.
    pub text_stable_only_ms: u32,
}

impl Default for EndpointPolicy {
    fn default() -> Self {
        Self {
            speech_rms: 0.007,
            speech_prob: 0.62,
            min_speech_ms: 120,
            silence_commit_ms: 1_250,
            recording_max_ms: 28_000,
            sound_before_trigger_ms: 350,
            sound_after_trigger_ms: 250,
            enable_partials: true,
            partial_cadence_ms: 300,
            min_partial_ms: 800,
            partial_stable_ms: 1_450,
            text_stable_only_ms: 2_400,
        }
    }
}

impl EndpointPolicy {
    /// TTS-only low-latency preset.
    ///
    /// Partials are disabled because they compete with the final transcription for the same
    /// model lock; the trailing-silence and stable-text deadlines are cut to a fraction of
    /// their defaults so the answer reaches TTS as soon as the user stops talking.
    #[must_use]
    pub fn low_latency() -> Self {
        Self {
            speech_rms: 0.008,
            speech_prob: 0.5,
            min_speech_ms: 80,
            silence_commit_ms: 280,
            recording_max_ms: 12_000,
            sound_before_trigger_ms: 200,
            sound_after_trigger_ms: 100,
            enable_partials: false,
            partial_cadence_ms: 150,
            min_partial_ms: 400,
            partial_stable_ms: 400,
            text_stable_only_ms: 600,
        }
    }

    /// Conversational voice preset for natural long-form phrases without mid-sentence chops.
    ///
    /// Uses a ~900 ms base pause with up to 60 s utterances and triggers a single partial
    /// completeness check during pauses to extend the wait when a clause trails off mid-thought.
    #[must_use]
    pub fn conversational() -> Self {
        Self {
            speech_rms: 0.007,
            speech_prob: 0.56,
            min_speech_ms: 140,
            silence_commit_ms: 900,
            recording_max_ms: 60_000,
            sound_before_trigger_ms: 350,
            sound_after_trigger_ms: 250,
            enable_partials: false,
            partial_cadence_ms: 600,
            min_partial_ms: 500,
            partial_stable_ms: 900,
            text_stable_only_ms: 2_200,
        }
    }

    /// Builds the low-level segmenter configuration implied by this policy.
    pub fn vad_config(&self) -> VadConfig {
        VadConfig {
            speech_threshold: self.speech_rms,
            speech_prob: self.speech_prob,
            min_speech_ms: self.min_speech_ms,
            silence_commit_ms: self.silence_commit_ms,
            max_utterance_ms: self.recording_max_ms,
            pre_roll_ms: self.sound_before_trigger_ms,
            sound_after_trigger_ms: self.sound_after_trigger_ms,
        }
    }
}

/// Why a turn was committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointReason {
    /// Trailing silence reached `silence_commit_ms`.
    Silence,
    /// The utterance hit `recording_max_ms`.
    MaxDuration,
    /// The host forced the commit (push-to-talk release).
    Manual,
    /// The user started speaking over playback.
    BargeIn,
    /// Partial text stopped changing, so the turn was committed without waiting for silence.
    TextStable,
}

/// What the segmenter actually delivered for a committed turn.
///
/// Reported alongside the transcript so latency and clipping problems are visible in
/// telemetry instead of being guessed from "it cut me off again".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EndpointReport {
    /// Why the turn ended.
    pub reason: EndpointReason,
    /// Audio actually delivered before speech onset.
    pub actual_before_ms: u32,
    /// Trailing silence actually delivered after speech ended.
    pub actual_after_ms: u32,
    /// Mean voiced RMS divided by the tracked noise floor.
    pub rms_ratio: f32,
}

impl Default for EndpointReport {
    fn default() -> Self {
        Self {
            reason: EndpointReason::Silence,
            actual_before_ms: 0,
            actual_after_ms: 0,
            rms_ratio: 0.0,
        }
    }
}

/// Deadlines that keep a stalled voice session from hanging forever.
#[derive(Clone, Copy, Debug)]
pub struct VoiceTimeouts {
    /// Time from speech onset to the first non-empty partial before reporting a stall.
    pub first_partial_ms: u32,
    /// Idle time with no speech before reporting a stalled microphone.
    pub starting_silence_ms: u32,
    /// Time from sending a TTS request to the first audio frame.
    pub tts_start_ms: u32,
    /// Upper bound on a single synthesis session.
    pub tts_session_ms: u32,
}

impl Default for VoiceTimeouts {
    fn default() -> Self {
        Self {
            first_partial_ms: 4_000,
            starting_silence_ms: 5_000,
            tts_start_ms: 5_000,
            tts_session_ms: 180_000,
        }
    }
}

impl VoiceTimeouts {
    /// Tighter deadlines for the low-latency preset.
    #[must_use]
    pub fn low_latency() -> Self {
        Self {
            first_partial_ms: 2_000,
            starting_silence_ms: 5_000,
            tts_start_ms: 3_000,
            tts_session_ms: 60_000,
        }
    }
}

/// Which deadline expired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutKind {
    /// Speech started but no partial transcript arrived in time.
    FirstPartial,
    /// No speech at all for `starting_silence_ms`.
    StartingSilence,
    /// No TTS audio arrived after the request.
    TtsStart,
    /// The synthesis session exceeded `tts_session_ms`.
    TtsSession,
}
