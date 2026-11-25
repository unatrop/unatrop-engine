use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use uno_voice::{
    AudioFrame, DownloadProgressFn, EndpointPolicy, EndpointReason, SpeechUtterance, SttEngine,
    Transcript, VoiceConfig, VoiceError, VoiceEvent, VoiceSession, Word,
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
        Ok("привет мир".to_string())
    }
}

fn utterance() -> SpeechUtterance {
    SpeechUtterance::new(7, 16_000, vec![0.0; 16_000])
        .with_words(vec![Word::new("раз", 0, 300), Word::new("два", 320, 700)])
}

#[test]
fn duration_follows_sample_count() {
    assert_eq!(utterance().duration_ms(), 1_000);
}

#[test]
fn playback_word_at_tracks_position() {
    let utterance = utterance();
    assert_eq!(utterance.word_at(0), Some(0));
    assert_eq!(utterance.word_at(500), Some(1));
}

#[test]
fn zero_rate_is_not_a_division_by_zero() {
    let empty = SpeechUtterance::new(1, 0, Vec::new());
    assert_eq!(empty.duration_ms(), 0);
    assert_eq!(empty.word_at(10), None);
}

#[test]
fn word_at_finds_the_active_word() {
    let transcript = Transcript::new("привет как дела").with_words(vec![
        Word::new("привет", 0, 400),
        Word::new("как", 420, 600),
        Word::new("дела", 620, 900),
    ]);

    assert_eq!(transcript.word_at(0), Some(0));
    assert_eq!(transcript.word_at(500), Some(1));
    assert_eq!(transcript.word_at(899), Some(2));
    assert_eq!(transcript.duration_ms(), 900);
}

#[test]
fn estimated_words_cover_the_whole_duration() {
    let transcript = Transcript::new("один два три четыре").with_estimated_words(1_000);

    assert_eq!(transcript.words.len(), 4);
    assert_eq!(transcript.words[0].start_ms, 0);
    assert_eq!(transcript.words[3].end_ms, 1_000);
}

#[test]
fn estimated_words_do_not_override_real_timings() {
    let original = vec![Word::new("раз", 0, 100)];
    let transcript = Transcript::new("раз").with_words(original.clone());

    assert_eq!(transcript.with_estimated_words(5_000).words, original);
}

#[test]
fn manual_commit_transcribes_buffered_speech() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let config = VoiceConfig {
        endpoint: EndpointPolicy {
            speech_prob: 0.0,
            speech_rms: 0.0,
            ..EndpointPolicy::default()
        },
        ..VoiceConfig::default()
    };
    let (handle, mut events) = VoiceSession::spawn(rt.handle(), config, Arc::new(FakeStt));

    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut ready = false;
            while let Some(event) = events.recv().await {
                if matches!(event, VoiceEvent::Listening) {
                    ready = true;
                    break;
                }
            }
            assert!(ready, "session should reach the listening state");

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

            let mut saw_endpoint = false;
            let mut saw_transcript = false;
            while let Some(event) = events.recv().await {
                match event {
                    VoiceEvent::Endpoint(report) => {
                        saw_endpoint = true;
                        assert_eq!(report.reason, EndpointReason::Manual);
                    }
                    VoiceEvent::Transcript(transcript) => {
                        saw_transcript = true;
                        assert_eq!(transcript.text, "привет мир");
                        break;
                    }
                    _ => {}
                }
            }
            assert!(saw_endpoint, "manual commit must emit an endpoint report");
            assert!(saw_transcript, "manual commit must transcribe the buffer");
        })
        .await
        .expect("session should respond within 10 s");
    });

    handle.close();
}
