use uno_voice::{
    AudioFrame, EndpointPolicy, EnergyVad, VadConfig, VadDecision, VadReport, VoiceDenoiser,
    rms_level,
};

fn test_vad_config() -> VadConfig {
    VadConfig {
        speech_threshold: 0.02,
        speech_prob: 0.5,
        min_speech_ms: 100,
        silence_commit_ms: 200,
        max_utterance_ms: 5_000,
        pre_roll_ms: 50,
        sound_after_trigger_ms: 50,
    }
}

fn feed(vad: &mut EnergyVad, samples: &[f32], prob: f32) -> Option<(Vec<f32>, VadReport)> {
    match vad.push_frame(rms_level(samples), prob, samples) {
        VadDecision::UtteranceReady { audio, report } => Some((audio, report)),
        VadDecision::Continue { .. } => None,
    }
}

#[test]
fn processes_silence_with_low_vad() {
    let mut denoiser = VoiceDenoiser::new();
    let silence = vec![0.0_f32; 1600];
    let (out, vad) = denoiser.process(&silence);
    assert_eq!(out.len(), 1600);
    assert!(vad < 0.2, "silence should have low VAD prob, got {vad}");
}

#[test]
fn native_48k_path_preserves_duration() {
    let mut denoiser = VoiceDenoiser::new();
    let frame = AudioFrame::new(vec![0i16; 480 * 2], 48_000, 2);
    let (out, _) = denoiser.process_frame(&frame);
    assert_eq!(
        out.len(),
        160,
        "10 ms at 48 kHz must become 10 ms at 16 kHz"
    );
}

#[test]
fn reset_clears_filter_state() {
    let mut denoiser = VoiceDenoiser::new();
    denoiser.process(&vec![0.3_f32; 1600]);
    denoiser.reset();

    let (out, _) = denoiser.process(&vec![0.0_f32; 1600]);
    assert!(out.iter().all(|sample| sample.abs() < 1e-6));
}

#[test]
fn policy_maps_onto_the_segmenter() {
    let policy = EndpointPolicy::default();
    let vad = policy.vad_config();

    assert_eq!(vad.speech_threshold, policy.speech_rms);
    assert_eq!(vad.pre_roll_ms, policy.sound_before_trigger_ms);
    assert_eq!(vad.max_utterance_ms, policy.recording_max_ms);
}

#[test]
fn low_latency_drops_partials_and_shortens_deadlines() {
    let normal = EndpointPolicy::default();
    let fast = EndpointPolicy::low_latency();

    assert!(!fast.enable_partials);
    assert!(normal.enable_partials);
    assert!(fast.silence_commit_ms < normal.silence_commit_ms / 2);
    assert!(fast.partial_stable_ms < normal.partial_stable_ms / 2);
    assert!(fast.text_stable_only_ms < normal.text_stable_only_ms / 2);
    assert!(fast.sound_after_trigger_ms < normal.sound_after_trigger_ms);
}

#[test]
fn endpoint_defaults_are_coherent() {
    let policy = EndpointPolicy::default();
    assert!(policy.sound_after_trigger_ms < policy.silence_commit_ms);
    assert!(policy.min_partial_ms < policy.recording_max_ms);
    assert!(policy.partial_stable_ms < policy.text_stable_only_ms);
}

#[test]
fn segments_speech_after_trailing_silence() {
    let mut vad = EnergyVad::new(test_vad_config());
    let quiet = vec![0.0_f32; 800];
    let loud = vec![0.2_f32; 1600];

    assert!(matches!(
        vad.push_frame(0.0, 0.0, &quiet),
        VadDecision::Continue {
            speech_started: false,
            ..
        }
    ));
    assert!(matches!(
        vad.push_frame(0.2, 0.9, &loud),
        VadDecision::Continue {
            speech_started: true,
            ..
        }
    ));
    assert!(matches!(
        vad.push_frame(0.2, 0.9, &loud),
        VadDecision::Continue {
            speech_started: false,
            ..
        }
    ));

    let mut committed = None;
    for _ in 0..4 {
        if let Some(result) = feed(&mut vad, &quiet, 0.0) {
            committed = Some(result);
            break;
        }
    }
    let (audio, report) = committed.expect("utterance should commit after trailing silence");
    assert!(audio.len() >= 3200);
    assert!(report.actual_before_ms >= 50);
    assert_eq!(report.actual_after_ms, 50);
    assert!(report.rms_ratio > 1.0);
}

#[test]
fn loud_noise_without_speech_probability_never_starts_a_turn() {
    let mut vad = EnergyVad::new(test_vad_config());
    let loud = vec![0.4_f32; 1600];

    for _ in 0..20 {
        let decision = vad.push_frame(0.4, 0.05, &loud);
        assert!(matches!(
            decision,
            VadDecision::Continue {
                speech_started: false,
                ..
            }
        ));
    }
    assert!(!vad.is_in_speech());
}

#[test]
fn pre_roll_is_prepended_to_the_utterance() {
    let mut vad = EnergyVad::new(test_vad_config());
    let quiet = vec![0.0_f32; 800];
    let speech = vec![0.2_f32; 1600];

    vad.push_frame(0.0, 0.0, &quiet);
    vad.push_frame(0.2, 0.9, &speech);

    assert_eq!(vad.current_utterance().len(), 800 + 1600);
}

#[test]
fn force_commit_returns_buffered_speech() {
    let mut vad = EnergyVad::new(test_vad_config());
    let speech = vec![0.2_f32; 1600];

    vad.push_frame(0.2, 0.9, &speech);
    let (audio, report) = vad.force_commit().expect("manual commit should succeed");

    assert_eq!(audio.len(), 1600);
    assert_eq!(report.actual_before_ms, 0);
    assert!(!vad.is_in_speech());
}

#[test]
fn force_commit_rejects_too_short_speech() {
    let mut vad = EnergyVad::new(test_vad_config());
    let blip = vec![0.2_f32; 320];

    vad.push_frame(0.2, 0.9, &blip);
    assert!(vad.force_commit().is_none());
}
