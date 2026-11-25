//! Real-time voice session state machine (denoise + VAD + async STT orchestration).
//!
//! The loop is deliberately non-blocking: final transcription runs in a spawned task and
//! reports back through a channel, so a slow model never stops the microphone from being
//! consumed. Blocking here is what makes assistants "go deaf" right after they answer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::aec::{AecProbe, AecState};
use crate::audio::{AudioFrame, TARGET_SAMPLE_RATE, rms_level};
use crate::denoise::VoiceDenoiser;
use crate::endpoint::{EndpointPolicy, EndpointReason, EndpointReport, TimeoutKind, VoiceTimeouts};
use crate::ring::AudioRing;
use crate::stt::{DownloadProgressFn, SttEngine, Transcript};
use crate::vad::{EnergyVad, VadDecision, VadEnd, VadReport};

/// How often the loop checks endpointing deadlines.
const ENDPOINT_TICK: Duration = Duration::from_millis(100);
/// Minimum spacing between UI level events.
const LEVEL_EMIT: Duration = Duration::from_millis(50);

/// Configuration for a real-time [`VoiceSession`].
#[derive(Clone, Debug)]
pub struct VoiceConfig {
    /// Preferred BCP-47 language hint (e.g. `Some("ru")`, `Some("en")`, or `None` for auto).
    pub language: Option<String>,
    /// Endpointing policy: onset thresholds, pre-roll, commit rules.
    pub endpoint: EndpointPolicy,
    /// Deadlines that keep a stalled session from hanging.
    pub timeouts: VoiceTimeouts,
    /// Seconds of microphone audio kept in the ring buffer.
    ///
    /// Ten seconds matches Quark's `audiosender.historySeconds`; it is what makes
    /// "the assistant missed my first word" fixable after the fact.
    pub ring_seconds: f32,
    /// Ignore energy-based barge-in while echo cancellation is unavailable.
    ///
    /// Without AEC the microphone hears the assistant's own TTS, so `SpeechStarted` fires
    /// on every spoken word and interrupts the answer it just generated. Hosts with a real
    /// echo canceller can leave this off; desktop hosts without one should turn it on.
    pub barge_in_requires_aec: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            language: None,
            endpoint: EndpointPolicy::default(),
            timeouts: VoiceTimeouts::default(),
            ring_seconds: 10.0,
            barge_in_requires_aec: false,
        }
    }
}

impl VoiceConfig {
    /// TTS-only voice mod preset: fast endpointing, no partial STT, AEC-gated barge-in.
    #[must_use]
    pub fn low_latency() -> Self {
        Self {
            language: None,
            endpoint: EndpointPolicy::low_latency(),
            timeouts: VoiceTimeouts::low_latency(),
            ring_seconds: 10.0,
            barge_in_requires_aec: true,
        }
    }

    /// Natural conversational preset for long-form speech (adaptive pauses up to 60 s).
    #[must_use]
    pub fn conversational() -> Self {
        Self {
            language: None,
            endpoint: EndpointPolicy::conversational(),
            timeouts: VoiceTimeouts::default(),
            ring_seconds: 12.0,
            barge_in_requires_aec: true,
        }
    }
}

/// Commands sent from the host application (desktop, CLI, FFI) into [`VoiceSession`].
#[derive(Debug)]
pub enum VoiceCommand {
    /// Pushes a raw PCM16 frame from the microphone.
    PushAudio(AudioFrame),
    /// Forces the currently buffered speech to transcribe immediately.
    CommitTurn,
    /// Discards the current speech buffer.
    Cancel,
    /// Copies the rolling microphone history for diagnostics.
    DumpRing {
        /// Reply channel receiving the buffered audio.
        reply: tokio::sync::oneshot::Sender<Vec<f32>>,
    },
    /// Cleanly terminates the voice session.
    Close,
}

/// Events emitted by [`VoiceSession`] to the UI or agent host.
#[derive(Clone, Debug)]
pub enum VoiceEvent {
    /// Local STT model is being downloaded for the first time (`progress` in `0.0..=1.0`).
    DownloadingModel {
        /// Download progress in `0.0..=1.0`.
        progress: f32,
        /// File being downloaded.
        file: String,
    },
    /// Local STT model is loaded and ready for instant real-time transcription.
    ModelReady,
    /// Session is actively listening for speech.
    Listening,
    /// Smoothed input microphone energy level (`0.0..=1.0`).
    SpeechLevel(f32),
    /// User started speaking (host can use this for barge-in / cancelling AI response).
    SpeechStarted,
    /// A turn was committed, with what the segmenter actually delivered.
    Endpoint(EndpointReport),
    /// Utterance ended and is currently being transcribed locally.
    Transcribing,
    /// Live intermediate hypothesis while the user is still speaking.
    PartialTranscript(String),
    /// Final recognized text, with word timings when the engine provides them.
    Transcript(Transcript),
    /// Platform echo-cancellation status, reported once at startup.
    AecStatus(AecState),
    /// A deadline expired; the session keeps running.
    Timeout {
        /// Which deadline expired.
        kind: TimeoutKind,
    },
    /// Recoverable or fatal error message.
    Error(String),
}

/// Cheaply cloneable handle for pushing audio and controlling an active [`VoiceSession`].
#[derive(Clone)]
pub struct VoiceHandle {
    tx: mpsc::UnboundedSender<VoiceCommand>,
}

impl VoiceHandle {
    /// Pushes a raw capture frame into the session.
    pub fn push_audio(&self, frame: AudioFrame) {
        let _ = self.tx.send(VoiceCommand::PushAudio(frame));
    }

    /// Forces the buffered speech to be transcribed now (push-to-talk release).
    pub fn commit_turn(&self) {
        let _ = self.tx.send(VoiceCommand::CommitTurn);
    }

    /// Discards the buffered speech.
    pub fn cancel(&self) {
        let _ = self.tx.send(VoiceCommand::Cancel);
    }

    /// Copies the rolling microphone history (last `ring_seconds` of audio).
    ///
    /// This is the "what did the microphone actually hear" dump: attach it to bug reports
    /// instead of guessing why recognition failed.
    pub async fn dump_ring(&self) -> Option<Vec<f32>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.tx.send(VoiceCommand::DumpRing { reply }).ok()?;
        rx.await.ok()
    }

    /// Terminates the session.
    pub fn close(&self) {
        let _ = self.tx.send(VoiceCommand::Close);
    }
}

/// Orchestrates real-time VAD segmentation and local/remote STT transcription.
pub struct VoiceSession;

/// Result of a background transcription task.
enum SttResult {
    Partial { turn_id: u64, text: String },
    Final { transcript: Transcript },
}

impl VoiceSession {
    /// Spawns a voice session on the given Tokio runtime handle.
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        config: VoiceConfig,
        stt: Arc<dyn SttEngine>,
    ) -> (VoiceHandle, mpsc::UnboundedReceiver<VoiceEvent>) {
        Self::spawn_with_probe(runtime, config, stt, None)
    }

    /// Spawns a voice session and reports echo-cancellation status at startup.
    pub fn spawn_with_probe(
        runtime: &tokio::runtime::Handle,
        config: VoiceConfig,
        stt: Arc<dyn SttEngine>,
        aec_probe: Option<Arc<dyn AecProbe>>,
    ) -> (VoiceHandle, mpsc::UnboundedReceiver<VoiceEvent>) {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<VoiceCommand>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<VoiceEvent>();

        runtime.spawn(async move {
            let policy = config.endpoint;
            let timeouts = config.timeouts;
            let language = config.language;

            if let Some(probe) = aec_probe {
                let _ = evt_tx.send(VoiceEvent::AecStatus(probe.probe()));
            }

            let evt_progress = evt_tx.clone();
            let progress_cb: DownloadProgressFn = Box::new(move |progress, file| {
                let _ = evt_progress.send(VoiceEvent::DownloadingModel {
                    progress,
                    file: file.to_string(),
                });
            });

            if let Err(err) = stt.ensure_ready(Some(&progress_cb)).await {
                let _ = evt_tx.send(VoiceEvent::Error(err.to_string()));
                return;
            }
            let _ = evt_tx.send(VoiceEvent::ModelReady);
            let _ = evt_tx.send(VoiceEvent::Listening);

            let mut denoiser = VoiceDenoiser::new();
            let mut vad = EnergyVad::new(policy.vad_config());
            let mut ring = AudioRing::with_seconds(config.ring_seconds);

            let mut turn_id: u64 = 0;
            let mut speech_started_at: Option<Instant> = None;
            let mut first_partial_reported = false;
            let mut idle_since = Instant::now();
            let mut last_level_emit = Instant::now();
            let mut last_partial_run = Instant::now();
            let mut last_partial_samples: usize = 0;
            let mut last_partial_text = String::new();
            let mut last_text_change = Instant::now();
            let mut last_voice_activity = Instant::now();
            let mut pause_checked_samples: usize = 0;
            let partial_in_flight = Arc::new(AtomicBool::new(false));
            let (result_tx, mut result_rx) = mpsc::unbounded_channel::<SttResult>();
            let mut endpoint_tick = tokio::time::interval(ENDPOINT_TICK);
            endpoint_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    cmd_opt = cmd_rx.recv() => {
                        let Some(cmd) = cmd_opt else {
                            break;
                        };
                        match cmd {
                            VoiceCommand::PushAudio(frame) => {
                                let (samples, speech_prob) = denoiser.process_frame(&frame);
                                if samples.is_empty() {
                                    continue;
                                }
                                ring.push(&samples);

                                let rms = rms_level(&samples);
                                if last_level_emit.elapsed() >= LEVEL_EMIT {
                                    last_level_emit = Instant::now();
                                    let level = (rms * 7.5).clamp(0.0, 1.0);
                                    let _ = evt_tx.send(VoiceEvent::SpeechLevel(level));
                                }

                                match vad.push_frame(rms, speech_prob, &samples) {
                                    VadDecision::Continue { speech_started, voiced, .. } => {
                                        if voiced {
                                            last_voice_activity = Instant::now();
                                        }
                                        if speech_started {
                                            turn_id += 1;
                                            speech_started_at = Some(Instant::now());
                                            first_partial_reported = false;
                                            last_partial_text.clear();
                                            last_partial_samples = 0;
                                            pause_checked_samples = 0;
                                            last_text_change = Instant::now();
                                            last_partial_run = Instant::now()
                                                .checked_sub(Duration::from_millis(u64::from(
                                                    policy.partial_cadence_ms,
                                                )))
                                                .unwrap_or_else(Instant::now);
                                            let _ = evt_tx.send(VoiceEvent::SpeechStarted);
                                        }

                                        let buffered = vad.current_utterance().len();
                                        let continuous_partial = policy.enable_partials
                                            && vad.is_in_speech()
                                            && buffered >= ms_to_samples(policy.min_partial_ms)
                                            && buffered > last_partial_samples
                                            && last_partial_run.elapsed()
                                                >= Duration::from_millis(u64::from(policy.partial_cadence_ms));
                                        let pause_check = !policy.enable_partials
                                            && policy.silence_commit_ms >= 600
                                            && vad.is_in_speech()
                                            && vad.silence_ms() >= 450
                                            && vad.voiced_ms() >= policy.min_speech_ms
                                            && buffered > pause_checked_samples;
                                        if (continuous_partial || pause_check)
                                            && !partial_in_flight.swap(true, Ordering::AcqRel)
                                        {
                                            last_partial_run = Instant::now();
                                            last_partial_samples = buffered;
                                            if pause_check {
                                                pause_checked_samples = buffered;
                                            }
                                            let snapshot = vad.current_utterance().to_vec();
                                            spawn_partial(
                                                Arc::clone(&stt),
                                                snapshot,
                                                language.clone(),
                                                turn_id,
                                                result_tx.clone(),
                                                Arc::clone(&partial_in_flight),
                                            );
                                        }
                                    }
                                    VadDecision::UtteranceReady { audio, report } => {
                                        turn_id += 1;
                                        let reason = endpoint_reason(report.ended_by);
                                        let _ = evt_tx.send(VoiceEvent::Endpoint(endpoint_report(
                                            report, reason,
                                        )));
                                        pause_checked_samples = 0;
                                        reset_turn_state(
                                            &mut speech_started_at,
                                            &mut first_partial_reported,
                                            &mut idle_since,
                                            &mut last_partial_text,
                                            &mut last_partial_samples,
                                            &mut last_text_change,
                                        );
                                        spawn_final(
                                            Arc::clone(&stt),
                                            audio,
                                            language.clone(),
                                            evt_tx.clone(),
                                            result_tx.clone(),
                                        );
                                    }
                                }
                            }
                            VoiceCommand::CommitTurn => {
                                turn_id += 1;
                                if let Some((audio, report)) = vad.force_commit() {
                                    let _ = evt_tx.send(VoiceEvent::Endpoint(endpoint_report(
                                        report,
                                        EndpointReason::Manual,
                                    )));
                                    spawn_final(
                                        Arc::clone(&stt),
                                        audio,
                                        language.clone(),
                                        evt_tx.clone(),
                                        result_tx.clone(),
                                    );
                                }
                                pause_checked_samples = 0;
                                reset_turn_state(
                                    &mut speech_started_at,
                                    &mut first_partial_reported,
                                    &mut idle_since,
                                    &mut last_partial_text,
                                    &mut last_partial_samples,
                                    &mut last_text_change,
                                );
                            }
                            VoiceCommand::Cancel => {
                                turn_id += 1;
                                vad.reset();
                                denoiser.reset();
                                ring.clear();
                                pause_checked_samples = 0;
                                reset_turn_state(
                                    &mut speech_started_at,
                                    &mut first_partial_reported,
                                    &mut idle_since,
                                    &mut last_partial_text,
                                    &mut last_partial_samples,
                                    &mut last_text_change,
                                );
                                let _ = evt_tx.send(VoiceEvent::Listening);
                            }
                            VoiceCommand::DumpRing { reply } => {
                                let _ = reply.send(ring.snapshot());
                            }
                            VoiceCommand::Close => {
                                break;
                            }
                        }
                    }
                    Some(result) = result_rx.recv() => {
                        match result {
                            SttResult::Partial { turn_id: result_turn, text } => {
                                partial_in_flight.store(false, Ordering::Release);
                                if result_turn != turn_id {
                                    continue;
                                }
                                if !policy.enable_partials
                                    && vad.is_in_speech()
                                    && looks_incomplete_phrase(&text)
                                {
                                    vad.extend_silence(900);
                                }
                                if is_valid_speech_text(&text) && text != last_partial_text {
                                    last_partial_text = text.clone();
                                    last_text_change = Instant::now();
                                    let _ = evt_tx.send(VoiceEvent::PartialTranscript(text));
                                }
                            }
                            SttResult::Final { transcript } => {
                                let _ = evt_tx.send(VoiceEvent::Transcript(transcript));
                                let _ = evt_tx.send(VoiceEvent::Listening);
                            }
                        }
                    }
                    _ = endpoint_tick.tick() => {
                        // 1. Nothing was said at all: report a dead microphone instead of hanging.
                        if !vad.is_in_speech()
                            && speech_started_at.is_none()
                            && idle_since.elapsed()
                                >= Duration::from_millis(u64::from(timeouts.starting_silence_ms))
                        {
                            idle_since = Instant::now();
                            let _ = evt_tx.send(VoiceEvent::Timeout {
                                kind: TimeoutKind::StartingSilence,
                            });
                            continue;
                        }

                        // 2. Speech started but the recognizer produced nothing usable.
                        if policy.enable_partials
                            && let Some(started) = speech_started_at
                            && !first_partial_reported
                            && last_partial_text.is_empty()
                            && started.elapsed()
                                >= Duration::from_millis(u64::from(timeouts.first_partial_ms))
                        {
                            first_partial_reported = true;
                            let _ = evt_tx.send(VoiceEvent::Timeout {
                                kind: TimeoutKind::FirstPartial,
                            });
                        }

                        // 3. Text stopped changing: commit without waiting for silence.
                        if last_partial_text.is_empty()
                            || partial_in_flight.load(Ordering::Acquire)
                        {
                            continue;
                        }
                        let text_stable = last_text_change.elapsed();
                        let voice_silent = last_voice_activity.elapsed();
                        let trimmed_partial = last_partial_text.trim();
                        let word_count = trimmed_partial.split_whitespace().count();
                        let ends_sentence = trimmed_partial.ends_with(['.', '!', '?']);
                        let looks_incomplete = word_count < 3
                            || trimmed_partial.ends_with([',', ':', ';', '-', '—']);
                        let scale_num: u64 = if ends_sentence {
                            65
                        } else if looks_incomplete {
                            145
                        } else {
                            100
                        };
                        let dyn_partial_ms =
                            u64::from(policy.partial_stable_ms).saturating_mul(scale_num) / 100;
                        let dyn_silence_ms =
                            u64::from(policy.silence_commit_ms).saturating_mul(scale_num) / 100;
                        let dyn_only_ms =
                            u64::from(policy.text_stable_only_ms).saturating_mul(scale_num) / 100;
                        let should_commit = (text_stable >= Duration::from_millis(dyn_partial_ms)
                            && voice_silent >= Duration::from_millis(dyn_silence_ms))
                            || text_stable >= Duration::from_millis(dyn_only_ms);

                        if should_commit {
                            turn_id += 1;
                            if let Some((audio, report)) = vad.force_commit() {
                                let _ = evt_tx.send(VoiceEvent::Endpoint(endpoint_report(
                                    report,
                                    EndpointReason::TextStable,
                                )));
                                spawn_final(
                                    Arc::clone(&stt),
                                    audio,
                                    language.clone(),
                                    evt_tx.clone(),
                                    result_tx.clone(),
                                );
                            }
                            reset_turn_state(
                                &mut speech_started_at,
                                &mut first_partial_reported,
                                &mut idle_since,
                                &mut last_partial_text,
                                &mut last_partial_samples,
                                &mut last_text_change,
                            );
                        }
                    }
                }
            }
        });

        (VoiceHandle { tx: cmd_tx }, evt_rx)
    }
}

fn spawn_partial(
    stt: Arc<dyn SttEngine>,
    audio: Vec<f32>,
    language: Option<String>,
    turn_id: u64,
    result_tx: mpsc::UnboundedSender<SttResult>,
    in_flight: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let text = stt
            .transcribe(&audio, language.as_deref())
            .await
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
        let _ = result_tx.send(SttResult::Partial { turn_id, text });
        in_flight.store(false, Ordering::Release);
    });
}

fn spawn_final(
    stt: Arc<dyn SttEngine>,
    audio: Vec<f32>,
    language: Option<String>,
    evt_tx: mpsc::UnboundedSender<VoiceEvent>,
    result_tx: mpsc::UnboundedSender<SttResult>,
) {
    tokio::spawn(async move {
        let _ = evt_tx.send(VoiceEvent::Transcribing);
        match stt.transcribe_detailed(&audio, language.as_deref()).await {
            Ok(transcript) => {
                if is_valid_speech_text(&transcript.text) {
                    let _ = result_tx.send(SttResult::Final { transcript });
                } else {
                    let _ = evt_tx.send(VoiceEvent::Listening);
                }
            }
            Err(err) => {
                let _ = evt_tx.send(VoiceEvent::Error(err.to_string()));
                let _ = evt_tx.send(VoiceEvent::Listening);
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn reset_turn_state(
    speech_started_at: &mut Option<Instant>,
    first_partial_reported: &mut bool,
    idle_since: &mut Instant,
    last_partial_text: &mut String,
    last_partial_samples: &mut usize,
    last_text_change: &mut Instant,
) {
    *speech_started_at = None;
    *first_partial_reported = false;
    *idle_since = Instant::now();
    last_partial_text.clear();
    *last_partial_samples = 0;
    *last_text_change = Instant::now();
}

fn endpoint_reason(ended_by: VadEnd) -> EndpointReason {
    match ended_by {
        VadEnd::Silence => EndpointReason::Silence,
        VadEnd::MaxDuration => EndpointReason::MaxDuration,
        VadEnd::Manual => EndpointReason::Manual,
    }
}

fn endpoint_report(report: VadReport, reason: EndpointReason) -> EndpointReport {
    EndpointReport {
        reason,
        actual_before_ms: report.actual_before_ms,
        actual_after_ms: report.actual_after_ms,
        rms_ratio: report.rms_ratio,
    }
}

fn ms_to_samples(ms: u32) -> usize {
    (u64::from(ms) * u64::from(TARGET_SAMPLE_RATE) / 1000) as usize
}

/// Validates that recognized text represents meaningful human speech,
/// filtering out STT hallucinations on background noise/breathing like "да", "б", ".", single letters.
fn is_valid_speech_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Filter out common CTC hallucinations on background noise/room breathing
    let lower = trimmed.to_lowercase();
    let hallucinations = [
        "да", "а", "э", "мм", "угу", "ну", "б", "д", "и", "в", "я", "о",
    ];
    if hallucinations.contains(&lower.as_str()) {
        return false;
    }

    // Must contain at least one word character and be longer than 2 chars
    let char_count = trimmed.chars().count();
    if char_count < 3 {
        return false;
    }

    trimmed.chars().any(char::is_alphabetic)
}

/// Detects trailing conjunctions, prepositions, or open punctuation indicating the speaker is
/// likely mid-thought rather than finished.
fn looks_incomplete_phrase(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.ends_with([',', ':', ';', '-', '—']) || trimmed.ends_with("...") {
        return true;
    }
    let Some(last_raw) = trimmed.split_whitespace().last() else {
        return false;
    };
    let last_clean = last_raw
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    matches!(
        last_clean.as_str(),
        "и" | "а"
            | "но"
            | "или"
            | "либо"
            | "да"
            | "что"
            | "чтобы"
            | "чтоб"
            | "потому"
            | "поскольку"
            | "если"
            | "когда"
            | "как"
            | "где"
            | "куда"
            | "откуда"
            | "который"
            | "которая"
            | "которое"
            | "которые"
            | "в"
            | "во"
            | "на"
            | "с"
            | "со"
            | "по"
            | "к"
            | "ко"
            | "от"
            | "до"
            | "для"
            | "про"
            | "без"
            | "из"
            | "за"
            | "у"
            | "о"
            | "об"
            | "при"
            | "под"
            | "над"
            | "между"
            | "через"
            | "перед"
            | "после"
            | "не"
            | "ни"
            | "бы"
            | "же"
            | "ну"
            | "вот"
            | "типа"
            | "короче"
            | "значит"
            | "and"
            | "but"
            | "or"
            | "because"
            | "so"
            | "if"
            | "when"
            | "while"
            | "that"
            | "which"
            | "who"
            | "to"
            | "for"
            | "with"
            | "in"
            | "on"
            | "of"
            | "at"
            | "from"
            | "by"
            | "about"
            | "into"
            | "the"
            | "a"
            | "an"
    )
}

#[cfg(test)]
mod tests {
    use super::looks_incomplete_phrase;

    #[test]
    fn detects_incomplete_trailing_clauses_and_complete_sentences() {
        assert!(looks_incomplete_phrase("Сделай пожалуйста так, чтобы"));
        assert!(looks_incomplete_phrase("Открой файл в"));
        assert!(looks_incomplete_phrase("Ну смотри,"));
        assert!(looks_incomplete_phrase("Open the file and"));
        assert!(!looks_incomplete_phrase("Открой список задач."));
        assert!(!looks_incomplete_phrase("Покажи текущую погоду в Москве"));
        assert!(!looks_incomplete_phrase("Close the sidebar now"));
    }
}
