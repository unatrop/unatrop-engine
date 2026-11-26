//! Voice call service managing shared GigaAM STT lifecycle, per-call sessions, and TTS bridging.

use std::{
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{runtime::Handle, sync::mpsc, task::JoinHandle};
use uno_daemon_protocol::pb;
use uno_voice::{
    AudioFrame, OnnxStt, TimeoutKind, VoiceConfig, VoiceEvent, VoiceHandle, VoiceSession,
};

use super::tts_bridge::{CallTtsPipeline, spawn_session_tts_bridge};
use crate::modules::{sessions::SessionService, tts::TtsService};

pub(crate) struct CallService {
    models_dir: PathBuf,
    stt: Arc<OnceLock<Arc<OnnxStt>>>,
    active_calls: Arc<AtomicUsize>,
    call_epoch: Arc<AtomicU64>,
    tts: Arc<TtsService>,
    sessions: Arc<SessionService>,
}

impl CallService {
    pub(crate) fn new(
        models_dir: PathBuf,
        tts: Arc<TtsService>,
        sessions: Arc<SessionService>,
    ) -> Self {
        Self {
            models_dir,
            stt: Arc::new(OnceLock::new()),
            active_calls: Arc::new(AtomicUsize::new(0)),
            call_epoch: Arc::new(AtomicU64::new(0)),
            tts,
            sessions,
        }
    }

    pub(crate) fn open_session(
        self: &Arc<Self>,
        out_tx: mpsc::Sender<pb::CallServerMsg>,
    ) -> CallSession {
        CallSession {
            pipeline: Arc::new(CallTtsPipeline::new(Arc::clone(&self.tts), out_tx.clone())),
            service: Arc::clone(self),
            out_tx,
            voice_handle: None,
            forward_task: None,
            session_bridge_task: None,
            stt_started: false,
            call_voice: resolve_call_voice(None),
            call_lang: None,
            auto_speak: false,
        }
    }

    fn shared_stt(&self) -> Arc<OnnxStt> {
        Arc::clone(
            self.stt
                .get_or_init(|| Arc::new(OnnxStt::new(self.models_dir.clone()))),
        )
    }

    fn on_call_started(&self) {
        self.active_calls.fetch_add(1, Ordering::AcqRel);
        self.call_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn on_call_ended(&self) {
        if self.active_calls.fetch_sub(1, Ordering::AcqRel) == 1 {
            let epoch = self.call_epoch.load(Ordering::Acquire);
            let stt_cell = Arc::clone(&self.stt);
            let active_calls = Arc::clone(&self.active_calls);
            let call_epoch = Arc::clone(&self.call_epoch);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if active_calls.load(Ordering::Acquire) == 0
                    && call_epoch.load(Ordering::Acquire) == epoch
                    && let Some(stt) = stt_cell.get()
                {
                    stt.unload().await;
                    tracing::info!("unloaded idle GigaAM STT model");
                }
            });
        }
    }
}

pub(crate) struct CallSession {
    service: Arc<CallService>,
    out_tx: mpsc::Sender<pb::CallServerMsg>,
    pipeline: Arc<CallTtsPipeline>,
    voice_handle: Option<VoiceHandle>,
    forward_task: Option<JoinHandle<()>>,
    session_bridge_task: Option<JoinHandle<()>>,
    stt_started: bool,
    call_voice: Option<String>,
    call_lang: Option<String>,
    auto_speak: bool,
}

impl CallSession {
    /// Dispatches a single client message; returns `false` when the client requests `Stop`.
    pub(crate) fn handle_command(&mut self, cmd: pb::CallClientMsg) -> bool {
        match cmd.msg {
            Some(pb::call_client_msg::Msg::Start(start)) => self.start_call(start),
            Some(pb::call_client_msg::Msg::AttachSession(sid)) => self.attach_session(&sid),
            Some(pb::call_client_msg::Msg::Speak(cmd)) => {
                let voice = resolve_call_voice(cmd.voice.or_else(|| self.call_voice.clone()));
                let lang = cmd.lang.or_else(|| self.call_lang.clone());
                self.pipeline.speak_text(&cmd.text, voice, lang, cmd.tag);
            }
            Some(pb::call_client_msg::Msg::InterruptTts(_)) => self.pipeline.interrupt(),
            Some(pb::call_client_msg::Msg::Audio(chunk)) => {
                if let Some(ref handle) = self.voice_handle {
                    let (pairs, _) = chunk.pcm_s16le.as_chunks::<2>();
                    let samples: Vec<i16> = pairs
                        .iter()
                        .map(|&[lo, hi]| i16::from_le_bytes([lo, hi]))
                        .collect();
                    let channels = u16::try_from(chunk.channels.max(1)).unwrap_or(1);
                    handle.push_audio(AudioFrame::new(samples, chunk.sample_rate, channels));
                }
            }
            Some(pb::call_client_msg::Msg::Commit(_)) => {
                if let Some(ref handle) = self.voice_handle {
                    handle.commit_turn();
                }
            }
            Some(pb::call_client_msg::Msg::Cancel(_)) => {
                if let Some(ref handle) = self.voice_handle {
                    handle.cancel();
                }
            }
            Some(pb::call_client_msg::Msg::Stop(_)) => return false,
            None => {}
        }
        true
    }

    pub(crate) fn shutdown(mut self) {
        self.clear_active_voice();
        self.pipeline.begin_turn(false);
        if self.stt_started {
            self.service.on_call_ended();
        }
    }

    fn clear_active_voice(&mut self) {
        if let Some(h) = self.voice_handle.take() {
            h.close();
        }
        if let Some(t) = self.forward_task.take() {
            t.abort();
        }
        if let Some(t) = self.session_bridge_task.take() {
            t.abort();
        }
    }

    fn start_call(&mut self, start: pb::StartCallCommand) {
        if !self.stt_started {
            self.service.on_call_started();
            self.stt_started = true;
        }
        self.clear_active_voice();

        self.call_voice = resolve_call_voice(start.voice);
        self.call_lang = start.language.clone();
        self.auto_speak = start.auto_speak;

        let tts = Arc::clone(&self.service.tts);
        tokio::spawn(async move {
            tts.prewarm().await;
        });

        let config = resolve_voice_config(start.language);
        let (handle, mut events) =
            VoiceSession::spawn(&Handle::current(), config, self.service.shared_stt());
        self.voice_handle = Some(handle);

        let tx = self.out_tx.clone();
        let pipe_for_fwd = Arc::clone(&self.pipeline);
        let auto_speak = self.auto_speak;
        self.forward_task = Some(tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                match &ev {
                    VoiceEvent::SpeechStarted if auto_speak => pipe_for_fwd.interrupt(),
                    VoiceEvent::Transcript(t) => {
                        let trimmed = t.text.trim();
                        if !trimmed.is_empty() && pipe_for_fwd.is_recent_echo(trimmed) {
                            continue;
                        }
                    }
                    _ => {}
                }
                if let Some(msg) = map_voice_event(ev)
                    && tx.send(pb::CallServerMsg { msg: Some(msg) }).await.is_err()
                {
                    break;
                }
            }
        }));

        if let Some(ref sid) = start.session_id {
            self.attach_session(sid);
        }
    }

    fn attach_session(&mut self, sid: &str) {
        if let Some(t) = self.session_bridge_task.take() {
            t.abort();
        }
        if self.auto_speak
            && let Ok(entry) = self.service.sessions.get(sid)
        {
            self.session_bridge_task = Some(spawn_session_tts_bridge(
                entry,
                Arc::clone(&self.pipeline),
                self.out_tx.clone(),
                self.call_voice.clone(),
                self.call_lang.clone(),
            ));
        }
    }
}

fn resolve_call_voice(explicit: Option<String>) -> Option<String> {
    explicit
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("UNATROP_TTS_VOICE")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| Some("kolya".to_owned()))
}

// Conversational endpointing by default: 900 ms base pause with 60 s utterances
// and mid-pause completeness detection.
// Set UNATROP_VOICE_LOW_LATENCY=1 for aggressive 280 ms turns, or =0 for continuous partials.
fn resolve_voice_config(language: Option<String>) -> VoiceConfig {
    match std::env::var("UNATROP_VOICE_LOW_LATENCY").ok().as_deref() {
        Some("1" | "true") => VoiceConfig {
            language,
            ..VoiceConfig::low_latency()
        },
        Some("0" | "false") => VoiceConfig {
            language,
            ..VoiceConfig::default()
        },
        _ => VoiceConfig {
            language,
            ..VoiceConfig::conversational()
        },
    }
}

fn map_voice_event(ev: VoiceEvent) -> Option<pb::call_server_msg::Msg> {
    match ev {
        VoiceEvent::DownloadingModel { progress, .. } => {
            Some(pb::call_server_msg::Msg::DownloadProgress(progress))
        }
        VoiceEvent::ModelReady => Some(pb::call_server_msg::Msg::ClearHypothesis(pb::Empty {})),
        VoiceEvent::Listening => Some(pb::call_server_msg::Msg::Ready(pb::Empty {})),
        VoiceEvent::SpeechStarted => Some(pb::call_server_msg::Msg::SpeechStarted(pb::Empty {})),
        VoiceEvent::PartialTranscript(text) => {
            Some(pb::call_server_msg::Msg::PartialTranscript(text))
        }
        VoiceEvent::Transcript(t) => {
            let trimmed = t.text.trim();
            (!trimmed.is_empty()).then(|| pb::call_server_msg::Msg::Transcript(trimmed.to_owned()))
        }
        VoiceEvent::Endpoint(report)
            if report.actual_before_ms == 0 && report.actual_after_ms == 0 =>
        {
            Some(pb::call_server_msg::Msg::ClearHypothesis(pb::Empty {}))
        }
        VoiceEvent::Timeout {
            kind: TimeoutKind::FirstPartial | TimeoutKind::StartingSilence,
        } => Some(pb::call_server_msg::Msg::ClearHypothesis(pb::Empty {})),
        VoiceEvent::Error(err) => Some(pb::call_server_msg::Msg::Error(err)),
        _ => None,
    }
}
