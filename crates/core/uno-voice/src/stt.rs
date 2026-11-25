//! Speech-to-Text contracts: batch engines, streaming engines, and timed transcripts.

use async_trait::async_trait;
use thiserror::Error;

/// Errors produced by voice capture, model download, or STT inference.
#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("failed to download speech model: {0}")]
    Download(String),
    #[error("failed to load speech model: {0}")]
    ModelLoad(String),
    #[error("speech transcription failed: {0}")]
    Inference(String),
    #[error("text-to-speech failed: {0}")]
    Tts(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Progress callback invoked while downloading local STT weights (`progress` in `0.0..=1.0`).
pub type DownloadProgressFn = Box<dyn Fn(f32, &str) + Send + Sync>;

/// A single recognized word with its position in the utterance.
///
/// Word timings are not a nicety: without them there are no subtitles, no karaoke
/// highlighting, and no way to cut a barge-in at the word the user actually heard.
/// Both Yandex (`TAsrResult_TWord`, `TTtsTimings_TUtterance_Word`) and Claude
/// (`tts_word`, `speech_end_offset`) carry them in both directions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Word {
    /// Word text as recognized.
    pub text: String,
    /// Start offset from the beginning of the utterance, in milliseconds.
    pub start_ms: u32,
    /// End offset from the beginning of the utterance, in milliseconds.
    pub end_ms: u32,
    /// Engine confidence in `0.0..=1.0`; `0.0` when the engine does not report one.
    pub confidence: f32,
}

impl Word {
    /// Creates a word with explicit timings.
    pub fn new(text: impl Into<String>, start_ms: u32, end_ms: u32) -> Self {
        Self {
            text: text.into(),
            start_ms,
            end_ms,
            confidence: 0.0,
        }
    }

    /// Sets the confidence score.
    #[must_use]
    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence;
        self
    }

    /// Duration of the word in milliseconds.
    pub fn duration_ms(&self) -> u32 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// A finished transcription: text plus whatever timing information the engine provides.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcript {
    /// Recognized text.
    pub text: String,
    /// Word-level timings; empty when the engine only produces plain text.
    pub words: Vec<Word>,
    /// Detected or requested language (BCP-47).
    pub language: Option<String>,
}

impl Transcript {
    /// Creates a transcript from plain text with no word timings.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            words: Vec::new(),
            language: None,
        }
    }

    /// Attaches word timings.
    #[must_use]
    pub fn with_words(mut self, words: Vec<Word>) -> Self {
        self.words = words;
        self
    }

    /// Attaches the language tag.
    #[must_use]
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    /// Whether the transcript carries no text.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Duration covered by the word timings, or `0` when there are none.
    pub fn duration_ms(&self) -> u32 {
        self.words.last().map_or(0, |word| word.end_ms)
    }

    /// Index of the word being spoken at `at_ms`, or `None` before the first word.
    pub fn word_at(&self, at_ms: u32) -> Option<usize> {
        self.words.iter().rposition(|word| word.start_ms <= at_ms)
    }

    /// Word timings, synthesized by distributing the duration evenly over whitespace tokens.
    ///
    /// Used as a fallback for engines that return plain text only, so downstream consumers
    /// can keep a single code path. The result is approximate and should not be trusted for
    /// sub-word alignment.
    #[must_use]
    pub fn with_estimated_words(mut self, total_ms: u32) -> Self {
        if !self.words.is_empty() {
            return self;
        }
        let tokens: Vec<&str> = self.text.split_whitespace().collect();
        if tokens.is_empty() || total_ms == 0 {
            return self;
        }
        let per_word = total_ms / tokens.len() as u32;
        let mut cursor = 0u32;
        self.words = tokens
            .into_iter()
            .map(|token| {
                let start = cursor;
                cursor = cursor.saturating_add(per_word);
                Word::new(token, start, cursor)
            })
            .collect();
        self
    }
}

/// Framework-independent batch Speech-to-Text engine.
#[async_trait]
pub trait SttEngine: Send + Sync {
    /// Ensures local model weights are downloaded and loaded into memory.
    async fn ensure_ready(
        &self,
        on_progress: Option<&DownloadProgressFn>,
    ) -> Result<(), VoiceError>;

    /// Transcribes 16 kHz mono f32 audio into text.
    async fn transcribe(
        &self,
        pcm_16khz: &[f32],
        language: Option<&str>,
    ) -> Result<String, VoiceError>;

    /// Transcribes with word timings when the engine supports them.
    ///
    /// The default implementation falls back to [`Self::transcribe`] and returns an empty
    /// word list, so existing engines keep working unchanged.
    async fn transcribe_detailed(
        &self,
        pcm_16khz: &[f32],
        language: Option<&str>,
    ) -> Result<Transcript, VoiceError> {
        let text = self.transcribe(pcm_16khz, language).await?;
        let mut transcript = Transcript::new(text);
        if let Some(language) = language {
            transcript = transcript.with_language(language);
        }
        Ok(transcript)
    }
}

/// Streaming Speech-to-Text engine that consumes audio incrementally.
///
/// The batch [`SttEngine`] re-runs the whole model over a growing buffer, which is
/// quadratic in utterance length. A streaming engine keeps decoder state and emits
/// partials in constant time per chunk, the way Quark's `AsrRecognize`/`AsrResult`/
/// `ChunkAck` exchange works.
#[async_trait]
pub trait StreamingStt: Send + Sync {
    /// Feeds a chunk of 16 kHz mono f32 audio and returns newly stabilized partial text.
    async fn push(&mut self, pcm_16khz: &[f32]) -> Result<Vec<String>, VoiceError>;

    /// Flushes the decoder and returns the final transcript.
    async fn finalize(&mut self) -> Result<Transcript, VoiceError>;

    /// Drops decoder state so the next [`Self::push`] starts a new utterance.
    fn reset(&mut self);
}
