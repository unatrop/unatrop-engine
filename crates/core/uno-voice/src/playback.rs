//! Playback contract for synthesized speech, including barge-in accounting.
//!
//! When the user talks over the assistant, the only number that matters is *how much audio
//! actually reached the speaker*, not how much was queued. Quark does the same thing with
//! `TtsUpdateState` (client reports real playback position) and `TTtsInterruptionTimings`
//! (where the interruption landed), so the server can resume from the right word instead of
//! repeating the whole sentence.
//!
//! The player itself lives in the host (cpal, rodio, WebAudio); this module only defines
//! what the host must report back.

use crate::stt::Word;

/// A synthesized utterance ready to be played.
#[derive(Clone, Debug)]
pub struct SpeechUtterance {
    /// Monotonic identifier used to correlate playback events with the request.
    pub id: u64,
    /// Sample rate of `samples`.
    pub sample_rate: u32,
    /// Mono `f32` samples in `[-1.0, 1.0]`.
    pub samples: Vec<f32>,
    /// Word timings inside this utterance, if the TTS engine produced them.
    pub words: Vec<Word>,
}

impl SpeechUtterance {
    /// Creates an utterance without word timings.
    pub fn new(id: u64, sample_rate: u32, samples: Vec<f32>) -> Self {
        Self {
            id,
            sample_rate,
            samples,
            words: Vec::new(),
        }
    }

    /// Attaches word timings.
    #[must_use]
    pub fn with_words(mut self, words: Vec<Word>) -> Self {
        self.words = words;
        self
    }

    /// Total duration in milliseconds.
    pub fn duration_ms(&self) -> u32 {
        if self.sample_rate == 0 {
            return 0;
        }
        let ms = self.samples.len() as u64 * 1000 / u64::from(self.sample_rate);
        u32::try_from(ms).unwrap_or(u32::MAX)
    }

    /// Index of the word being spoken at `played_ms`.
    pub fn word_at(&self, played_ms: u32) -> Option<usize> {
        self.words
            .iter()
            .rposition(|word| word.start_ms <= played_ms)
    }
}

/// Events a host player reports while speaking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackEvent {
    /// Playback of an utterance started.
    Started {
        /// Utterance identifier.
        utterance_id: u64,
    },
    /// Playback crossed into a new word (drives captions and karaoke highlighting).
    WordBoundary {
        /// Utterance identifier.
        utterance_id: u64,
        /// Word index inside [`SpeechUtterance::words`].
        word_idx: u32,
        /// Playback position in milliseconds.
        at_ms: u32,
    },
    /// Playback was cut short by the user.
    Interrupted {
        /// Utterance identifier.
        utterance_id: u64,
        /// Audio actually played before the interruption.
        played_ms: u32,
        /// Word index that was being spoken, or `0` when there are no timings.
        word_idx: u32,
    },
    /// Playback finished normally.
    Finished {
        /// Utterance identifier.
        utterance_id: u64,
    },
}

/// Result of stopping playback, to be forwarded to the assistant as "what was heard".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupted {
    /// Utterance that was playing.
    pub utterance_id: u64,
    /// Audio actually played before the interruption.
    pub played_ms: u32,
    /// Word index that was being spoken, or `0` when there are no timings.
    pub word_idx: u32,
}

/// Host-side player contract.
pub trait TtsPlayer: Send {
    /// Queues an utterance for playback.
    fn enqueue(&mut self, utterance: SpeechUtterance);

    /// Stops playback immediately and reports where it stopped.
    fn interrupt(&mut self) -> Option<Interrupted>;

    /// Whether audio is currently being played.
    fn is_playing(&self) -> bool;

    /// Number of utterances waiting behind the current one.
    fn queued(&self) -> usize;
}
