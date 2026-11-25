use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use uno_voice::{
    AssistantReply, AudioFrame, DownloadProgressFn, EchoResponder, EndpointPolicy, Responder,
    SpeechUtterance, SttEngine, TtsEngine, TtsOptions, VoiceConfig, VoiceError, VoiceEvent,
    VoicePipeline, VoicePipelineEvent,
};

struct FakeStt;

#[async_trait]
impl SttEngine for FakeStt {
    async fn ensure_ready(
        &self,
        _on_progress: Option<&DownloadProgressFn>,
    ) -> Result<(), VoiceError> {
        Ok(())
    }

    async fn transcribe(
        &self,
        _pcm_16khz: &[f32],
        _language: Option<&str>,
    ) -> Result<String, VoiceError> {
        Ok("привет мир".into())
    }
}

struct CountingResponder {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Responder for CountingResponder {
    async fn respond(&self, text: &str) -> Result<String, VoiceError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(format!("ответ: {text}"))
    }

    async fn respond_detailed(&self, text: &str) -> Result<AssistantReply, VoiceError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(AssistantReply {
            text: format!("ответ: {text}"),
            actions: serde_json::json!([{"name": "set_timer", "duration": 300}]),
        })
    }
}

struct FakeTts;

#[async_trait]
impl TtsEngine for FakeTts {
    async fn synthesize(
        &self,
        text: &str,
        _options: &TtsOptions,
    ) -> Result<SpeechUtterance, VoiceError> {
        Ok(SpeechUtterance::new(1, 16_000, vec![0.0; text.len()]))
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
}

fn config() -> VoiceConfig {
    VoiceConfig {
        endpoint: EndpointPolicy {
            speech_prob: 0.0,
            speech_rms: 0.0,
            ..EndpointPolicy::default()
        },
        ..VoiceConfig::default()
    }
}

fn push_speech(handle: &uno_voice::VoicePipelineHandle) {
    for _ in 0..2 {
        handle.push_audio(AudioFrame::new(vec![0i16; 1600], 16_000, 1));
    }
    for _ in 0..5 {
        let mut pcm = Vec::with_capacity(1600);
        for index in 0..1600 {
            let phase = index as f32 * 2.0 * std::f32::consts::PI * 300.0 / 16_000.0;
            pcm.push((phase.sin() * 26_000.0) as i16);
        }
        handle.push_audio(AudioFrame::new(pcm, 16_000, 1));
    }
    handle.commit_turn();
}

#[test]
fn transcript_is_answered_and_spoken() {
    let rt = runtime();
    let calls = Arc::new(AtomicUsize::new(0));
    let (handle, mut events) = VoicePipeline::spawn(
        rt.handle(),
        config(),
        Arc::new(FakeStt),
        Arc::new(CountingResponder {
            calls: Arc::clone(&calls),
        }),
        Arc::new(FakeTts),
    );

    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                if matches!(event, VoicePipelineEvent::Voice(VoiceEvent::Listening)) {
                    break;
                }
            }
            push_speech(&handle);

            let mut heard = false;
            let mut answer = false;
            let mut actions = false;
            let mut spoken = false;
            while let Some(event) = events.recv().await {
                match event {
                    VoicePipelineEvent::Heard(text) => {
                        assert_eq!(text, "привет мир");
                        heard = true;
                    }
                    VoicePipelineEvent::Answer(text) => {
                        assert_eq!(text, "ответ: привет мир");
                        answer = true;
                    }
                    VoicePipelineEvent::Actions(value) => {
                        assert_eq!(value[0]["name"], "set_timer");
                        actions = true;
                    }
                    VoicePipelineEvent::Speaking(utterance) => {
                        assert_eq!(utterance.sample_rate, 16_000);
                        assert_eq!(utterance.samples.len(), "ответ: привет мир".len());
                        spoken = true;
                        break;
                    }
                    VoicePipelineEvent::Error(message) => panic!("unexpected error: {message}"),
                    _ => {}
                }
            }
            assert!(heard, "final transcript must be reported");
            assert!(answer, "responder answer must be reported");
            assert!(actions, "tool calls must be reported");
            assert!(spoken, "answer must be synthesized");
        })
        .await
        .expect("pipeline should respond within 10 s");
    });
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    handle.close();
}

#[test]
fn say_bypasses_responder() {
    let rt = runtime();
    let calls = Arc::new(AtomicUsize::new(0));
    let (handle, mut events) = VoicePipeline::spawn(
        rt.handle(),
        config(),
        Arc::new(FakeStt),
        Arc::new(CountingResponder {
            calls: Arc::clone(&calls),
        }),
        Arc::new(FakeTts),
    );

    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                if matches!(event, VoicePipelineEvent::Voice(VoiceEvent::Listening)) {
                    break;
                }
            }
            handle.say("привет");
            let mut spoken = false;
            while let Some(event) = events.recv().await {
                match event {
                    VoicePipelineEvent::Speaking(utterance) => {
                        assert_eq!(utterance.samples.len(), "привет".len());
                        spoken = true;
                        break;
                    }
                    VoicePipelineEvent::Answer(_) => panic!("say must not call the responder"),
                    VoicePipelineEvent::Error(message) => panic!("unexpected error: {message}"),
                    _ => {}
                }
            }
            assert!(spoken, "say must synthesize audio");
        })
        .await
        .expect("pipeline should respond within 10 s");
    });
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    handle.close();
}

#[test]
fn tts_only_pipeline_does_not_need_a_dialog_model() {
    let rt = runtime();
    let (handle, mut events) =
        VoicePipeline::spawn_tts(rt.handle(), config(), Arc::new(FakeStt), Arc::new(FakeTts));

    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                if matches!(event, VoicePipelineEvent::Voice(VoiceEvent::Listening)) {
                    break;
                }
            }
            push_speech(&handle);

            let mut spoken = false;
            while let Some(event) = events.recv().await {
                match event {
                    VoicePipelineEvent::Answer(text) => {
                        assert_eq!(text, "привет мир", "TTS-only mode speaks the transcript");
                    }
                    VoicePipelineEvent::Speaking(utterance) => {
                        assert_eq!(utterance.samples.len(), "привет мир".len());
                        spoken = true;
                        break;
                    }
                    VoicePipelineEvent::Error(message) => panic!("unexpected error: {message}"),
                    _ => {}
                }
            }
            assert!(spoken, "recognized text must be synthesized directly");
        })
        .await
        .expect("TTS-only pipeline should respond within 10 s");
    });
    handle.close();
}

#[test]
fn echo_responder_repeats_text() {
    let rt = runtime();
    let replied = rt.block_on(async {
        EchoResponder
            .respond_detailed("привет")
            .await
            .expect("echo responder")
    });
    assert_eq!(replied.text, "привет");
    assert!(replied.actions.is_null());
}
