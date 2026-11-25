//! Voice-mod glue: recognized text -> responder -> TTS -> host playback.
//!
//! [`VoiceSession`] owns the microphone side (denoise, VAD, STT). [`VoicePipeline`] adds
//! the other half of an assistant turn without dragging any audio backend into the core:
//! it answers the transcript, synthesizes the answer and emits an utterance for the host.
//! A new `SpeechStarted` or an explicit barge-in cancels the pending result, so a slow
//! dialog or TTS request cannot speak over the user.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::audio::AudioFrame;
use crate::playback::SpeechUtterance;
use crate::session::{VoiceConfig, VoiceEvent, VoiceHandle, VoiceSession};
use crate::stt::{SttEngine, VoiceError};
use crate::tts::{TtsEngine, TtsOptions};

/// Answer plus the structured actions the assistant wants the host to perform.
#[derive(Clone, Debug, Default)]
pub struct AssistantReply {
    /// Text to speak. Empty means "stay silent".
    pub text: String,
    /// Tool calls (`client_action`, `server_action`, ...) as returned by the provider.
    pub actions: serde_json::Value,
}

impl AssistantReply {
    /// Creates a plain-text reply without actions.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            actions: serde_json::Value::Null,
        }
    }
}

/// Produces the text to speak for a recognized utterance.
#[async_trait]
pub trait Responder: Send + Sync {
    /// Turns recognized text into an answer. An empty answer means "stay silent".
    async fn respond(&self, text: &str) -> Result<String, VoiceError>;

    /// Same as [`Self::respond`], but also carries tool calls when available.
    async fn respond_detailed(&self, text: &str) -> Result<AssistantReply, VoiceError> {
        Ok(AssistantReply::new(self.respond(text).await?))
    }
}

/// Responder that repeats what was heard; useful for TTS-only voice mods and tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct EchoResponder;

#[async_trait]
impl Responder for EchoResponder {
    async fn respond(&self, text: &str) -> Result<String, VoiceError> {
        Ok(text.to_owned())
    }
}

/// Everything the host needs to drive a voice turn.
#[derive(Clone, Debug)]
pub enum VoicePipelineEvent {
    /// Transparently forwarded from the underlying [`VoiceSession`].
    Voice(VoiceEvent),
    /// Final recognized text.
    Heard(String),
    /// Text returned by the responder, before synthesis.
    Answer(String),
    /// Synthesis has started; the host can show a placeholder while audio is generated.
    Preparing,
    /// Structured tool calls returned alongside the answer, if any.
    Actions(serde_json::Value),
    /// Synthesized audio ready for the host player.
    Speaking(SpeechUtterance),
    /// The user started speaking (or barge-in was requested): stop current playback.
    BargeIn,
    /// Recoverable dialog/TTS failure.
    Error(String),
}

enum PipelineCommand {
    Say(String),
    BargeIn,
    Close,
}

/// Cloneable handle for feeding audio and controlling a running [`VoicePipeline`].
#[derive(Clone)]
pub struct VoicePipelineHandle {
    voice: VoiceHandle,
    tx: mpsc::UnboundedSender<PipelineCommand>,
}

impl VoicePipelineHandle {
    /// Pushes a raw capture frame into the voice session.
    pub fn push_audio(&self, frame: AudioFrame) {
        self.voice.push_audio(frame);
    }

    /// Forces the buffered speech to be transcribed now.
    pub fn commit_turn(&self) {
        self.voice.commit_turn();
    }

    /// Discards the current speech buffer.
    pub fn cancel(&self) {
        self.voice.cancel();
    }

    /// Synthesizes `text` without asking the responder (wake-up sounds, notifications).
    pub fn say(&self, text: impl Into<String>) {
        let _ = self.tx.send(PipelineCommand::Say(text.into()));
    }

    /// Cancels a pending answer and tells the host to stop playback.
    pub fn barge_in(&self) {
        let _ = self.tx.send(PipelineCommand::BargeIn);
    }

    /// Copies the rolling microphone history for diagnostics.
    pub async fn dump_ring(&self) -> Option<Vec<f32>> {
        self.voice.dump_ring().await
    }

    /// Stops the pipeline and its voice session.
    pub fn close(&self) {
        self.voice.close();
        let _ = self.tx.send(PipelineCommand::Close);
    }
}

/// Ties a voice session to a responder and a TTS engine.
pub struct VoicePipeline;

impl VoicePipeline {
    /// Spawns the pipeline on `runtime` and returns its control handle and event stream.
    pub fn spawn(
        runtime: &tokio::runtime::Handle,
        config: VoiceConfig,
        stt: Arc<dyn SttEngine>,
        responder: Arc<dyn Responder>,
        tts: Arc<dyn TtsEngine>,
    ) -> (
        VoicePipelineHandle,
        mpsc::UnboundedReceiver<VoicePipelineEvent>,
    ) {
        let runtime = runtime.clone();
        let barge_in_requires_aec = config.barge_in_requires_aec;
        let (voice, mut voice_events) = VoiceSession::spawn(&runtime, config, stt);
        let mut aec_available = false;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<PipelineCommand>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<VoicePipelineEvent>();
        let generation = Arc::new(AtomicU64::new(0));

        runtime.clone().spawn(async move {
            loop {
                tokio::select! {
                    maybe_event = voice_events.recv() => {
                        let Some(event) = maybe_event else { break };
                        match event {
                            VoiceEvent::SpeechStarted => {
                                generation.fetch_add(1, Ordering::AcqRel);
                                if !barge_in_requires_aec || aec_available {
                                    let _ = evt_tx.send(VoicePipelineEvent::BargeIn);
                                }
                                let _ = evt_tx.send(VoicePipelineEvent::Voice(event));
                            }
                            VoiceEvent::AecStatus(state) => {
                                aec_available = state.is_available();
                                let _ = evt_tx.send(VoicePipelineEvent::Voice(event));
                            }
                            VoiceEvent::Transcript(transcript) => {
                                let text = transcript.text.trim().to_owned();
                                let _ = evt_tx.send(VoicePipelineEvent::Voice(
                                    VoiceEvent::Transcript(transcript),
                                ));
                                if text.is_empty() {
                                    continue;
                                }
                                let _ = evt_tx.send(VoicePipelineEvent::Heard(text.clone()));
                                let generation_id = generation.load(Ordering::Acquire);
                                let evt_tx = evt_tx.clone();
                                let responder = Arc::clone(&responder);
                                let tts = Arc::clone(&tts);
                                let generation = Arc::clone(&generation);
                                runtime.clone().spawn(async move {
                                    match responder.respond_detailed(&text).await {
                                        Ok(reply) if !reply.text.trim().is_empty() => {
                                            if is_current(&generation, generation_id) {
                                                let _ = evt_tx.send(
                                                    VoicePipelineEvent::Answer(reply.text.clone()),
                                                );
                                                if !reply.actions.is_null() {
                                                    let _ = evt_tx.send(
                                                        VoicePipelineEvent::Actions(reply.actions),
                                                    );
                                                }
                                            }
                                            synthesize(tts, reply.text, generation_id, &generation, &evt_tx)
                                                .await;
                                        }
                                        Ok(_) => {}
                                        Err(err) => {
                                            if is_current(&generation, generation_id) {
                                                let _ = evt_tx.send(
                                                    VoicePipelineEvent::Error(err.to_string()),
                                                );
                                            }
                                        }
                                    }
                                });
                            }
                            other => {
                                let _ = evt_tx.send(VoicePipelineEvent::Voice(other));
                            }
                        }
                    }
                    maybe_cmd = cmd_rx.recv() => {
                        let Some(cmd) = maybe_cmd else { break };
                        match cmd {
                            PipelineCommand::Say(text) => {
                                let generation_id = generation.load(Ordering::Acquire);
                                let evt_tx = evt_tx.clone();
                                let tts = Arc::clone(&tts);
                                let generation = Arc::clone(&generation);
                                runtime.clone().spawn(async move {
                                    synthesize(tts, text, generation_id, &generation, &evt_tx).await;
                                });
                            }
                            PipelineCommand::BargeIn => {
                                generation.fetch_add(1, Ordering::AcqRel);
                                let _ = evt_tx.send(VoicePipelineEvent::BargeIn);
                            }
                            PipelineCommand::Close => break,
                        }
                    }
                }
            }
        });

        (VoicePipelineHandle { voice, tx: cmd_tx }, evt_rx)
    }

    /// TTS-only pipeline: speaks what was recognized, without any dialog model.
    ///
    /// This is the path for a voice mod that only needs Alice's voice. The recognized
    /// text goes straight to the TTS engine; no model is queried for an answer.
    pub fn spawn_tts(
        runtime: &tokio::runtime::Handle,
        config: VoiceConfig,
        stt: Arc<dyn SttEngine>,
        tts: Arc<dyn TtsEngine>,
    ) -> (
        VoicePipelineHandle,
        mpsc::UnboundedReceiver<VoicePipelineEvent>,
    ) {
        Self::spawn(runtime, config, stt, Arc::new(EchoResponder), tts)
    }
}

fn is_current(generation: &AtomicU64, expected: u64) -> bool {
    generation.load(Ordering::Acquire) == expected
}

async fn synthesize(
    tts: Arc<dyn TtsEngine>,
    text: String,
    generation_id: u64,
    generation: &AtomicU64,
    evt_tx: &mpsc::UnboundedSender<VoicePipelineEvent>,
) {
    if !is_current(generation, generation_id) {
        return;
    }
    let _ = evt_tx.send(VoicePipelineEvent::Preparing);

    match tts.synthesize_stream(&text, &TtsOptions::default()).await {
        Ok(mut chunks) => {
            while let Some(chunk) = chunks.recv().await {
                if !is_current(generation, generation_id) {
                    return;
                }
                match chunk {
                    Ok(utterance) => {
                        let _ = evt_tx.send(VoicePipelineEvent::Speaking(utterance));
                    }
                    Err(err) => {
                        let _ = evt_tx.send(VoicePipelineEvent::Error(err.to_string()));
                        return;
                    }
                }
            }
        }
        Err(err) => {
            if is_current(generation, generation_id) {
                let _ = evt_tx.send(VoicePipelineEvent::Error(err.to_string()));
            }
        }
    }
}
